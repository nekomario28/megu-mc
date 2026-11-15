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
    if let Some(damage) = value.get("damage") {
        comp_set_varint(&mut comps, COMP_DAMAGE, damage.num() as i64);
    }
    if crate::json::jbool(value, "unbreakable", false) {
        comp_set(&mut comps, COMP_UNBREAKABLE, vec![1]);
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
                let cp = ContainerKey::Block((0, 70, 0));
                let cp2 = (rows == 54).then_some(ContainerKey::Block((1, 70, 0)));
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

fn inventory_menu_slot(native: i16) -> i16 {
    match native {
        0..=8 => native + 36,
        9..=35 => native,
        36..=39 => 44 - native,
        40 => 45,
        other => panic!("not a 1.21.1 inventory slot: {other}"),
    }
}
fn inventory_slots(value: &J) -> HashMap<i16, Stack> {
    slot_map(value)
        .into_iter()
        .filter(|(_, st)| st.1 > 0)
        .map(|(slot, st)| (inventory_menu_slot(slot), st))
        .collect()
}
#[test]
fn pickup_inventory_matches_real_1_21_1_classes() {
    let fixture = jparse(include_bytes!(
        "../evidence/gameplay/inventory-pickup/vanilla.json"
    ))
    .unwrap();
    assert_eq!(fixture.get("minecraft").unwrap().str(), "1.21.1");
    assert_eq!(
        fixture.get("server_sha1").unwrap().str(),
        "59353fb40c36d304f2035d51e7d6e6baa98dc05c"
    );
    let mut ids = HashSet::new();
    let (mut completed, mut partial, mut refused) = (0, 0, 0);
    for row in fixture.get("cases").unwrap().as_arr().unwrap() {
        let id = row.get("id").unwrap().str();
        assert!(ids.insert(id), "duplicate case {id}");
        assert_eq!(row.get("op").unwrap().str(), "inventory_add");
        let mut source = stack(row.get("source").unwrap());
        let original = source.1;
        let mut inv = inventory_slots(row.get("slots").unwrap());
        let selected = row.get("selected").unwrap().num() as i16;
        let expected = row.get("expected").unwrap();
        let space = expected.get("first_space").unwrap().num() as i16;
        assert_eq!(
            pickup_slot_with_space(&inv, selected, &source),
            (space >= 0).then(|| inventory_menu_slot(space)),
            "{id} first merge"
        );
        let empty = expected.get("first_empty").unwrap().num() as i16;
        assert_eq!(
            pickup_inventory_order().find(|key| !inv.contains_key(key)),
            (empty >= 0).then(|| inventory_menu_slot(empty)),
            "{id} first empty"
        );
        let accepted = pickup_into_inventory(
            &mut inv,
            selected,
            crate::json::jbool(row, "creative", false),
            &mut source,
        );
        assert_eq!(
            accepted,
            crate::json::jbool(expected, "accepted", false),
            "{id} return"
        );
        assert_eq!(
            source,
            stack(expected.get("source").unwrap()),
            "{id} remainder"
        );
        assert_eq!(
            inv,
            inventory_slots(expected.get("slots").unwrap()),
            "{id} slots"
        );
        if original > 0 && source.1 == 0 {
            completed += 1;
        } else if source.1 < original {
            partial += 1;
        } else {
            refused += 1;
        }
    }
    assert!(
        completed > 0 && partial > 0 && refused > 0,
        "missing reference outcomes"
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

/// `LootTable.fill` fills `container.getContainerSize()` slots, so a rolled table must stay
/// inside the slots the block actually has: a decorated pot holds one, a hopper five, a
/// dispenser nine, a chest twenty-seven. Before `ContainerKind::BlockContainer` every
/// non-furnace kind was a 27-slot chest, so a small container's roll could land on a slot the
/// block cannot hold and the item would be unreachable.
#[test]
fn container_loot_roll_stays_inside_the_block_its_table_came_from() {
    use crate::generation_payload::{ContainerKind, GenerationPayload};
    let redstone = name_to_item("minecraft:redstone").unwrap() as i64;
    let table = "spinetest:one_item";
    loot_tables().lock().unwrap().insert(
        table.to_owned(),
        vec![LootPool {
            rolls: 1,
            chance: 1.0,
            entries: vec![LootEntry {
                item: redstone,
                min: 1,
                max: 1,
                funcs: Vec::new(),
            }],
            funcs: Vec::new(),
        }],
    );
    for (pos, kind, highest) in [
        ((1, 70, 1), ContainerKind::BlockContainer(1), 0i16),
        ((2, 70, 2), ContainerKind::BlockContainer(5), 4),
        ((3, 70, 3), ContainerKind::BlockContainer(9), 8),
        ((4, 70, 4), ContainerKind::BlockContainer(27), 26),
        ((5, 70, 5), ContainerKind::Chest, 26),
    ] {
        materialize_generation_payloads(vec![(
            pos,
            GenerationPayload::LootContainer {
                kind,
                loot_table: table.to_owned(),
                loot_seed: Some(1234),
            },
        )]);
        let slots = chests()
            .lock()
            .unwrap()
            .get(&ContainerKey::Block(pos))
            .cloned()
            .expect("a container entry is created");
        assert!(!slots.is_empty(), "{kind:?} wrote its roll");
        for slot in slots.keys() {
            assert!(
                *slot <= highest,
                "{kind:?} cannot hold slot {slot} (highest is {highest})"
            );
        }
        chests().lock().unwrap().remove(&ContainerKey::Block(pos));
    }
    // A furnace's table names the furnace store, never the chest store.
    let furnace_pos = (6, 70, 6);
    materialize_generation_payloads(vec![(
        furnace_pos,
        GenerationPayload::LootContainer {
            kind: ContainerKind::Furnace,
            loot_table: table.to_owned(),
            loot_seed: Some(1234),
        },
    )]);
    assert!(
        !chests()
            .lock()
            .unwrap()
            .contains_key(&ContainerKey::Block(furnace_pos)),
        "a furnace's loot belongs in furnaces(), not chests()"
    );
    assert!(furnaces()
        .lock()
        .unwrap()
        .contains_key(&furnace_pos));
    furnaces().lock().unwrap().remove(&furnace_pos);
    loot_tables().lock().unwrap().remove(table);
}
