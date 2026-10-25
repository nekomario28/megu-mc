use super::*;
use crate::json::{jparse, J};

fn stack(value: &J) -> Stack {
    if matches!(value, J::Null) {
        return (0, 0, vec![]);
    }
    let item = name_to_item(value.get("item").unwrap().str()).unwrap();
    let mut comps = vec![];
    if let Some(cap) = value.get("cap") {
        comp_set_varint(&mut comps, 1, cap.num() as i64);
    }
    (item, value.get("count").unwrap().num() as u8, comps)
}
fn slot_map(value: &J) -> HashMap<i16, Stack> {
    let J::Obj(fields) = value else {
        panic!("slot map required")
    };
    fields
        .iter()
        .map(|(key, value)| (key.parse().unwrap(), stack(value)))
        .collect()
}

#[test]
fn container_transfers_match_real_1_21_1_classes() {
    let fixture = jparse(include_bytes!(
        "../evidence/gameplay/containers/vanilla.json"
    ))
    .unwrap();
    assert_eq!(fixture.get("minecraft").unwrap().str(), "1.21.1");
    assert_eq!(
        fixture.get("server_sha1").unwrap().str(),
        "59353fb40c36d304f2035d51e7d6e6baa98dc05c"
    );
    let mut inserted = 0;
    let mut moved = 0;
    let mut ids = HashSet::new();
    for row in fixture.get("cases").unwrap().as_arr().unwrap() {
        let id = row.get("id").unwrap().str();
        assert!(ids.insert(id), "duplicate oracle case {id}");
        let expected = row.get("expected").unwrap();
        match row.get("op").unwrap().str() {
            "insert" => {
                let mut source = stack(row.get("source").unwrap());
                let target = stack(row.get("target").unwrap());
                let mut slots = HashMap::new();
                if target.1 > 0 {
                    slots.insert(0, target);
                }
                insert_into_slot(
                    &mut slots,
                    0,
                    &mut source,
                    row.get("amount").unwrap().num() as u8,
                );
                assert_eq!(
                    source,
                    stack(expected.get("source").unwrap()),
                    "{id} source"
                );
                assert_eq!(
                    slots.get(&0).cloned().unwrap_or((0, 0, vec![])),
                    stack(expected.get("target").unwrap()),
                    "{id} target"
                );
                inserted += 1;
            }
            "quick_move" => {
                let rows = row.get("rows").unwrap().num() as i16 * 9;
                assert!(matches!(rows, 27 | 54));
                let cp = (0, 70, 0);
                let cp2 = (rows == 54).then_some((1, 70, 0));
                let mut all = HashMap::new();
                let mut inv = HashMap::new();
                for (slot, st) in slot_map(row.get("slots").unwrap()) {
                    if slot < rows {
                        let (pos, key) = if slot < 27 {
                            (cp, slot)
                        } else {
                            (cp2.unwrap(), slot - 27)
                        };
                        all.entry(pos).or_insert_with(HashMap::new).insert(key, st);
                    } else {
                        inv.insert(slot - rows + 9, st);
                    }
                }
                quick_move_chest(
                    &mut all,
                    &mut inv,
                    cp,
                    cp2,
                    row.get("clicked").unwrap().num() as i16,
                );
                let mut actual = HashMap::new();
                for (pos, slots) in all {
                    for (key, st) in slots {
                        actual.insert(if pos == cp { key } else { key + 27 }, st);
                    }
                }
                for (key, st) in inv {
                    actual.insert(key + rows - 9, st);
                }
                assert_eq!(actual, slot_map(expected.get("slots").unwrap()), "{id}");
                moved += 1;
            }
            other => panic!("unhandled oracle operation {other}"),
        }
    }
    assert!(
        inserted > 0 && moved > 0,
        "empty/missing reference coverage"
    );
}

#[test]
fn container_chest_item_reaches_the_placed_default_state() {
    let item = name_to_item("chest").unwrap();
    assert_eq!(item_to_state(item), Some(name_to_state("chest").unwrap()));
}

#[test]
fn container_pickup_fills_space_instead_of_swapping_a_partial_stack() {
    // Same-item overflow used to swap the entire slot and cursor in every plain menu.
    let item = name_to_item("stone").unwrap();
    let mut slots = HashMap::from([(0, (item, 60, vec![]))]);
    let mut carried = (item, 10, vec![]);
    click_slot(&mut slots, &mut carried, 0, 0);
    assert_eq!(slots[&0], (item, 64, vec![]));
    assert_eq!(carried, (item, 6, vec![]));
    // A full matching stack is a no-op, not another opportunity to swap.
    click_slot(&mut slots, &mut carried, 0, 0);
    assert_eq!(slots[&0], (item, 64, vec![]));
    assert_eq!(carried, (item, 6, vec![]));
}

#[test]
fn container_right_pickup_swaps_unlike_stacks_and_rounds_up_a_half() {
    let stone = name_to_item("stone").unwrap();
    let dirt = name_to_item("dirt").unwrap();
    let mut slots = HashMap::from([(0, (stone, 5, vec![]))]);
    let mut carried = (dirt, 3, vec![]);
    click_slot(&mut slots, &mut carried, 0, 1);
    assert_eq!(slots[&0], (dirt, 3, vec![]));
    assert_eq!(carried, (stone, 5, vec![]));
    carried = (0, 0, vec![]);
    click_slot(&mut slots, &mut carried, 0, 1);
    assert_eq!(slots[&0], (dirt, 1, vec![]));
    assert_eq!(carried, (dirt, 2, vec![]));
}

#[test]
fn container_pickup_respects_component_limits_on_empty_slots_and_swaps() {
    let stone = name_to_item("stone").unwrap();
    let dirt = name_to_item("dirt").unwrap();
    let mut comps = vec![];
    comp_set_varint(&mut comps, 1, 16);
    let mut carried = (stone, 20, comps.clone());
    let mut slots = HashMap::from([(0, (dirt, 3, vec![]))]);
    let before = (slots.clone(), carried.clone());
    click_slot(&mut slots, &mut carried, 0, 1);
    assert_eq!(
        (slots.clone(), carried.clone()),
        before,
        "oversized stack cannot swap"
    );
    click_slot(&mut slots, &mut carried, 1, 0);
    assert_eq!(slots[&1], (stone, 16, comps.clone()));
    assert_eq!(carried, (stone, 4, comps));
}
