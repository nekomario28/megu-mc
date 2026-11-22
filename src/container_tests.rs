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
                let cp = ContainerKey::ow((0, 70, 0));
                let cp2 = (rows == 54).then_some(ContainerKey::ow((1, 70, 0)));
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
    let _guard = crate::world_guard();
    use crate::generation_payload::{ContainerKind, GenerationPayload};
    let redstone = name_to_item("minecraft:redstone").unwrap();
    let table = "spinetest:one_item";
    loot_tables().lock().unwrap_or_else(|e| e.into_inner()).insert(
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
        assert!(
            !chests().lock().unwrap_or_else(|e| e.into_inner()).contains_key(&ContainerKey::ow(pos)),
            "{kind:?} LootTable stays unmaterialized before unpack"
        );
        assert!(
            pending_block_loot().lock().unwrap_or_else(|e| e.into_inner()).contains_key(&ow_block(pos)),
            "{kind:?} LootTable staged in pending_block_loot"
        );
        assert!(unpack_block_loot_if_needed(ow_block(pos)));
        let slots = chests()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&ContainerKey::ow(pos))
            .cloned()
            .expect("a container entry is created on unpack");
        assert!(!slots.is_empty(), "{kind:?} wrote its roll");
        for slot in slots.keys() {
            assert!(
                *slot <= highest,
                "{kind:?} cannot hold slot {slot} (highest is {highest})"
            );
        }
        chests().lock().unwrap_or_else(|e| e.into_inner()).remove(&ContainerKey::ow(pos));
        pending_block_loot().lock().unwrap_or_else(|e| e.into_inner()).remove(&ow_block(pos));
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
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&ContainerKey::ow(furnace_pos)),
        "a furnace's loot belongs in furnaces(), not chests()"
    );
    assert!(furnaces()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(&ow_block(furnace_pos)));
    furnaces().lock().unwrap_or_else(|e| e.into_inner()).remove(&ow_block(furnace_pos));
    loot_tables().lock().unwrap_or_else(|e| e.into_inner()).remove(table);
}

#[test]
fn container_json_loot_table_unpacks_nested_entries_and_fills_slots() {
    let table = r#"{
        "pools": [
            {
                "rolls": {"type": "minecraft:uniform", "min": 1, "max": 2},
                "entries": [
                    {"type": "item", "name": "minecraft:iron_ingot",
                     "functions": [{"function": "minecraft:set_count",
                                    "count": {"type": "minecraft:uniform", "min": 2, "max": 5}}]},
                    {"type": "alternatives", "children": [
                        {"type": "item", "name": "minecraft:gold_ingot"},
                        {"type": "group", "children": [
                            {"type": "sequence", "children": [
                                {"type": "item", "name": "minecraft:bread"},
                                {"type": "empty"}
                            ]}
                        ]}
                    ]},
                    {"type": "tag", "name": "minecraft:planks"},
                    {"type": "empty", "weight": 3}
                ]
            },
            {
                "rolls": 1,
                "conditions": [{"condition": "minecraft:random_chance", "chance": 0.0}],
                "entries": [{"type": "item", "name": "minecraft:diamond",
                             "functions": [{"function": "minecraft:set_count", "count": 3}]}]
            },
            {
                "rolls": 1,
                "entries": [{"type": "item", "name": "minecraft:iron_ingot"}]
            }
        ]
    }"#;
    let planks = [
        "oak_planks",
        "spruce_planks",
        "birch_planks",
        "jungle_planks",
        "acacia_planks",
        "dark_oak_planks",
        "mangrove_planks",
        "cherry_planks",
        "crimson_planks",
        "warped_planks",
    ];
    let allowed: HashSet<&str> = ["iron_ingot", "gold_ingot", "bread"]
        .into_iter()
        .chain(planks)
        .collect();
    let slots = unpack_loot_table_json(table, 0x5eed, 27);
    assert!(!slots.is_empty(), "the unconditional pool always drops");
    assert_eq!(unpack_loot_table_json(table, 0x5eed, 27), slots);
    for (slot, stack) in &slots {
        assert!((0..27).contains(slot), "slot {slot} inside the container");
        let name = item_to_name(stack.0).expect("loot resolves to a known item");
        assert!(allowed.contains(name), "unexpected loot item {name}");
        assert!((1..=5).contains(&stack.1), "{name} count {}", stack.1);
    }
    let cooked = unpack_loot_table_json(
        r#"{"pools":[{"rolls":1,"entries":[{"type":"item","name":"minecraft:beef",
            "functions":[{"function":"minecraft:furnace_smelt"}]}]}]}"#,
        7,
        27,
    );
    assert!(cooked
        .values()
        .any(|stack| item_to_name(stack.0) == Some("cooked_beef")));
    let split = unpack_loot_table_json(
        r#"{"pools":[{"rolls":1,"entries":[{"type":"item","name":"minecraft:arrow",
            "functions":[{"function":"minecraft:set_count","count":20}]}]}]}"#,
        42,
        3,
    );
    assert!(split.len() >= 2 && split.len() <= 3, "{split:?}");
    assert!(split
        .values()
        .all(|stack| item_to_name(stack.0) == Some("arrow")));
    assert_eq!(
        split.values().map(|stack| u32::from(stack.1)).sum::<u32>(),
        20,
        "splitting conserves the rolled count"
    );
    assert!(unpack_loot_table_json("not json", 1, 27).is_empty());
    assert!(unpack_loot_table_json(r#"{"pools":[]}"#, 1, 27).is_empty());
}

#[test]
fn mining_efficiency_and_durability_use_shared_mechanics() {
    let stone = name_to_state("stone").unwrap() as i64;
    let pick = name_to_item("iron_pickaxe").unwrap();
    let plain = (pick, 1, vec![]);
    let enchanted = (pick, 1, vec![(COMP_ENCHANTMENTS, enchantments_data(&[(8, 2)], true))]);
    assert_eq!(tool_dig_speed(&plain, stone), 6.0);
    assert_eq!(tool_dig_speed(&enchanted, stone), 11.0);
    assert!(dig_seconds(stone, &enchanted).unwrap() < dig_seconds(stone, &plain).unwrap());
    assert_eq!(tool_max_damage(name_to_item("crossbow").unwrap()), Some(465));
    let mut worn = plain;
    comp_set_varint(&mut worn.2, COMP_DAMAGE, 249);
    assert!(damage_stack(&mut worn, 0.0));
    let mut protected = enchanted;
    comp_set(&mut protected.2, COMP_UNBREAKABLE, vec![1]);
    assert!(!damage_stack(&mut protected, 0.0));
    assert_eq!(comp_varint(&protected.2, COMP_DAMAGE), None);
}

mod block_entity_helpers {
    use super::*;
    use crate::nbt::{read_named, write_named, NbtString, Tag};

    fn compound(fields: impl IntoIterator<Item = (&'static str, Tag)>) -> Tag {
        Tag::Compound(
            fields
                .into_iter()
                .map(|(k, v)| (NbtString::text(k), v))
                .collect(),
        )
    }

    fn string(value: &str) -> Tag {
        Tag::String(NbtString::text(value))
    }

    fn item_nbt(id: &str, count: i32) -> Tag {
        compound([("id", string(id)), ("count", Tag::Int(count))])
    }

    fn item(id: &str, count: u8) -> BlockEntityItem {
        BlockEntityItem::new(id, count).unwrap()
    }

    fn saved(tag: &Tag) -> Tag {
        let name = NbtString::text("");
        let bytes = write_named(&name, tag).unwrap();
        let (decoded_name, decoded) = read_named(&bytes).unwrap();
        assert_eq!(decoded_name, name);
        assert_eq!(&decoded, tag);
        decoded
    }

    #[test]
    fn beacon_scans_complete_layers_and_refreshes_effects_every_80_ticks() {
        let fixture = compound([
            ("Levels", Tag::Int(4)),
            ("primary_effect", string("minecraft:speed")),
            ("secondary_effect", string("minecraft:regeneration")),
            ("Lock", string("key")),
        ]);
        let mut beacon = Beacon::from_nbt(&fixture);
        assert_eq!(saved(&beacon.to_nbt()), fixture);
        assert!(beacon
            .tick(79, true, |_, _, _| panic!("early scan"))
            .is_none());
        assert!(beacon
            .tick(80, false, |_, _, _| panic!("beam blocked"))
            .is_none());
        let pulse = beacon
            .tick(80, true, |x, y, z| (x, y, z) != (3, -3, 3))
            .unwrap();
        assert_eq!(beacon.levels(), 2);
        assert_eq!(
            (
                pulse.range,
                pulse.duration,
                pulse.primary,
                pulse.amplifier,
                pulse.secondary
            ),
            (30, 260, BeaconEffect::Speed, 0, None)
        );

        let mut invalid = Some(item("coal", 1));
        assert!(!beacon.confirm(Some(BeaconEffect::Haste), None, &mut invalid));
        assert_eq!(invalid, Some(item("coal", 1)));
        let mut payment = Some(item("iron_ingot", 2));
        assert!(beacon.confirm(
            Some(BeaconEffect::Haste),
            Some(BeaconEffect::Haste),
            &mut payment
        ));
        assert_eq!(payment, Some(item("iron_ingot", 1)));
        let pulse = beacon.tick(160, true, |_, _, _| true).unwrap();
        assert_eq!(
            (
                pulse.range,
                pulse.duration,
                pulse.primary,
                pulse.amplifier,
                pulse.secondary
            ),
            (50, 340, BeaconEffect::Haste, 1, None)
        );
        beacon.confirm(
            Some(BeaconEffect::Strength),
            Some(BeaconEffect::Regeneration),
            &mut payment,
        );
        assert_eq!(payment, None);
        assert_eq!(
            beacon.tick(240, true, |_, _, _| true).unwrap().secondary,
            Some(BeaconEffect::Regeneration)
        );
        assert!(beacon.tick(320, true, |_, _, _| false).is_none());
        assert_eq!(beacon.levels(), 0);
    }

    fn slot_nbt(slot: i8, id: &str, count: i32) -> Tag {
        compound([
            ("Slot", Tag::Byte(slot)),
            ("id", string(id)),
            ("count", Tag::Int(count)),
        ])
    }

    #[test]
    fn campfire_resumes_four_slots_and_cools_twice_as_fast_as_it_cooks() {
        let fixture = compound([
            (
                "Items",
                Tag::List(
                    10,
                    vec![
                        slot_nbt(0, "minecraft:beef", 1),
                        slot_nbt(1, "minecraft:potato", 1),
                        slot_nbt(2, "minecraft:cod", 1),
                        slot_nbt(3, "minecraft:porkchop", 1),
                    ],
                ),
            ),
            ("CookingTimes", Tag::IntArray(vec![3, 3, 0, 4])),
            ("CookingTotalTimes", Tag::IntArray(vec![3, 4, 2, 5])),
        ]);
        let mut fire = Campfire::from_nbt(&fixture);
        assert_eq!(saved(&fire.to_nbt()), fixture);

        let mut held = Some(item("salmon", 2));
        assert_eq!(
            fire.place(&mut held, |_| panic!("all slots occupied")),
            None
        );
        assert!(fire.tick(false, |_| panic!("unlit"), |_| true).is_empty());
        assert_eq!(fire.cooking_times, [1, 1, 0, 2]);
        assert_eq!(Campfire::from_nbt(&saved(&fire.to_nbt())), fire);

        let cook = |ingredient: &BlockEntityItem| {
            (ingredient.id.path() == "beef").then(|| item("cooked_beef", 1))
        };
        assert!(fire.tick(true, cook, |_| true).is_empty());
        assert_eq!(
            fire.tick(true, cook, |_| true),
            [item("cooked_beef", 1), item("cod", 1)]
        );
        assert_eq!(
            fire.slots,
            [
                None,
                Some(item("potato", 1)),
                None,
                Some(item("porkchop", 1))
            ]
        );
        assert_eq!(fire.place(&mut held, |_| None), None);
        assert_eq!(fire.place(&mut held, |_| Some(7)), Some(0));
        assert_eq!(held.as_ref().map(BlockEntityItem::count), Some(1));
        assert_eq!(fire.cooking_times[0], 0);
        assert_eq!(fire.cooking_total_times[0], 7);
        assert_eq!(
            fire.tick(true, cook, |output| output.id.path() != "porkchop"),
            [item("potato", 1)]
        );
        assert_eq!(fire.slots[3], Some(item("porkchop", 1)));
        assert_eq!(fire.tick(true, cook, |_| true), [item("porkchop", 1)]);
    }

    #[test]
    fn lectern_preserves_book_components_and_signals_only_real_page_turns() {
        let book = compound([
            ("id", string("minecraft:written_book")),
            ("count", Tag::Int(1)),
            (
                "components",
                compound([(
                    "minecraft:written_book_content",
                    compound([
                        ("title", compound([("raw", string("レシピ"))])),
                        ("author", string("Alex")),
                        ("resolved", Tag::Byte(1)),
                        (
                            "pages",
                            Tag::List(
                                10,
                                vec![
                                    compound([("raw", string("First"))]),
                                    compound([
                                        ("raw", string("Second")),
                                        ("filtered", string("Filtered")),
                                    ]),
                                    compound([("raw", string("Third"))]),
                                ],
                            ),
                        ),
                    ]),
                )]),
            ),
        ]);
        let fixture = compound([("Book", book.clone()), ("Page", Tag::Int(1))]);
        let mut lectern = Lectern::from_nbt(&fixture);
        assert_eq!(saved(&lectern.to_nbt()), fixture);
        assert_eq!(lectern.page_count(), 3);
        assert_eq!(lectern.page(), 1);
        assert_eq!(lectern.comparator_signal(), 8);
        assert!(!lectern.set_page(1));
        assert!(lectern.set_page(i32::MAX));
        assert_eq!((lectern.page(), lectern.comparator_signal()), (2, 15));
        assert!(!lectern.set_page(3));
        assert!(lectern.set_page(-99));
        assert_eq!((lectern.page(), lectern.comparator_signal()), (0, 1));

        let mut held = Some(item("writable_book", 2));
        assert!(!lectern.place(&mut held));
        assert_eq!(lectern.take_book().unwrap().to_nbt(), book);
        assert_eq!(lectern.comparator_signal(), 0);
        assert_eq!(lectern.to_nbt(), compound([]));
        assert!(lectern.place(&mut held));
        assert_eq!(held, Some(item("writable_book", 1)));
        assert_eq!(lectern.page_count(), 0);
        assert_eq!((lectern.page(), lectern.comparator_signal()), (0, 15));
        assert!(lectern.set_page(1));
        assert_eq!(lectern.page(), -1);
        assert_eq!(Lectern::from_nbt(&saved(&lectern.to_nbt())).page(), -1);
        lectern.take_book();
        assert!(!lectern.place(&mut Some(item("book", 1))));
    }

    #[test]
    fn chiseled_bookshelf_tracks_the_last_successful_insert_or_removal() {
        let fixture = compound([
            (
                "Items",
                Tag::List(10, vec![slot_nbt(5, "minecraft:enchanted_book", 1)]),
            ),
            ("last_interacted_slot", Tag::Int(5)),
        ]);
        assert_eq!(ChiseledBookshelf::default().comparator_signal(), 0);
        let mut shelf = ChiseledBookshelf::from_nbt(&fixture);
        assert_eq!(saved(&shelf.to_nbt()), fixture);
        assert_eq!(shelf.comparator_signal(), 6);

        let mut held = Some(item("book", 2));
        assert!(!shelf.insert_book(6, &mut held));
        assert!(!shelf.insert_book(5, &mut held));
        assert_eq!(held, Some(item("book", 2)));
        assert!(!shelf.insert_book(0, &mut Some(item("paper", 1))));
        assert_eq!(shelf.remove_book(0), None);
        assert_eq!(shelf.comparator_signal(), 6);
        assert!(shelf.insert_book(1, &mut held));
        assert_eq!(shelf.comparator_signal(), 2);
        assert_eq!(held, Some(item("book", 1)));
        assert_eq!(shelf.remove_book(5), Some(item("enchanted_book", 1)));
        assert_eq!(shelf.comparator_signal(), 6);
        assert_eq!(shelf.remove_book(1), Some(item("book", 1)));
        assert!(shelf.slots.iter().all(Option::is_none));
        assert_eq!(shelf.comparator_signal(), 2);
        assert_eq!(ChiseledBookshelf::from_nbt(&saved(&shelf.to_nbt())), shelf);
    }

    #[test]
    fn decorated_pot_preserves_side_order_and_merges_only_matching_stacks() {
        let fixture = compound([
            (
                "sherds",
                Tag::List(
                    8,
                    vec![
                        string("minecraft:angler_pottery_sherd"),
                        string("minecraft:brick"),
                        string("minecraft:archer_pottery_sherd"),
                        string("minecraft:arms_up_pottery_sherd"),
                    ],
                ),
            ),
            ("item", item_nbt("minecraft:diamond", 31)),
        ]);
        let mut pot = DecoratedPot::from_nbt(&fixture);
        assert_eq!(saved(&pot.to_nbt()), fixture);
        assert_eq!(
            pot.sherds[0].as_ref().unwrap().path(),
            "angler_pottery_sherd"
        );
        assert!(pot.sherds[1].is_none());
        assert_eq!(
            pot.sherds[3].as_ref().unwrap().path(),
            "arms_up_pottery_sherd"
        );
        assert_eq!(pot.comparator_signal(64), 7);
        let mut held = Some(item("diamond", 2));
        assert!(pot.insert(&mut held, 64));
        assert_eq!(held, Some(item("diamond", 1)));
        assert_eq!(pot.item, Some(item("diamond", 32)));
        assert_eq!(pot.comparator_signal(64), 8);
        assert!(!pot.insert(&mut held, 32));
        assert!(!pot.insert(&mut Some(item("emerald", 1)), 64));

        let mut renamed = item("diamond", 1);
        renamed
            .components
            .insert(NbtString::text("minecraft:custom_name"), string("Gem"));
        assert!(!pot.insert(&mut Some(renamed), 64));
        assert_eq!(DecoratedPot::from_nbt(&saved(&pot.to_nbt())), pot);
        assert_eq!(DecoratedPot::default().to_nbt(), compound([]));

        let contingent = compound([
            (
                "LootTable",
                string("minecraft:chests/trial_chambers/corridor"),
            ),
            ("LootTableSeed", Tag::Long(-5)),
        ]);
        let mut pot = DecoratedPot::from_nbt(&contingent);
        assert_eq!(saved(&pot.to_nbt()), contingent);
        assert!(!pot.insert(&mut held, 64));
        assert_eq!(held, Some(item("diamond", 1)));
    }

    #[test]
    fn crafter_prioritizes_empty_slots_and_consumes_only_on_a_successful_recipe() {
        let fixture = compound([
            (
                "Items",
                Tag::List(
                    10,
                    vec![
                        slot_nbt(0, "minecraft:milk_bucket", 2),
                        slot_nbt(2, "minecraft:egg", 2),
                        slot_nbt(8, "minecraft:wheat", 1),
                    ],
                ),
            ),
            ("disabled_slots", Tag::IntArray(vec![1, 3, 4, 5, 6])),
            ("crafting_ticks_remaining", Tag::Int(2)),
            ("triggered", Tag::Int(1)),
        ]);
        let mut crafter = Crafter::from_nbt(&fixture);
        assert_eq!(saved(&crafter.to_nbt()), fixture);
        assert_eq!(crafter.comparator_signal(), 8);
        assert!(!crafter.set_slot_disabled(0, true));
        assert!(!crafter.set_slot_disabled(9, true));
        assert!(!crafter.tick());
        assert!(crafter.tick());
        assert!(!crafter.tick());
        assert_eq!(crafter.set_powered(true), None);
        assert_eq!(crafter.set_powered(false), None);
        assert_eq!(crafter.set_powered(true), Some(4));

        let mut held = Some(item("egg", 2));
        assert_eq!(crafter.insert(&mut held, 16), Some(7));
        assert_eq!(crafter.insert(&mut held, 16), Some(7));
        assert_eq!(held, None);
        assert_eq!(crafter.comparator_signal(), 9);

        let before = crafter.to_nbt();
        assert_eq!(crafter.craft(|_| None), None);
        assert_eq!(crafter.to_nbt(), before);
        let result = crafter.craft(|slots| {
            assert_eq!(slots[0], Some(item("milk_bucket", 2)));
            assert_eq!(slots[7], Some(item("egg", 2)));
            Some((item("cake", 1), vec![item("bucket", 1)]))
        });
        assert_eq!(result, Some((item("cake", 1), vec![item("bucket", 1)])));
        assert_eq!(crafter.slots()[0], Some(item("milk_bucket", 1)));
        assert_eq!(crafter.slots()[2], Some(item("egg", 1)));
        assert_eq!(crafter.slots()[7], Some(item("egg", 1)));
        assert_eq!(crafter.slots()[8], None);
        assert_eq!(crafter.crafting_ticks_remaining, 6);
        assert_eq!(Crafter::from_nbt(&saved(&crafter.to_nbt())), crafter);
        for _ in 0..5 {
            assert!(!crafter.tick());
        }
        assert!(crafter.tick());
        assert!(crafter.set_slot(1, Some(item("sugar", 1))));
        assert_eq!(crafter.slots()[1], Some(item("sugar", 1)));
        assert!(crafter.set_slot(1, None));
        assert_eq!(crafter.comparator_signal(), 7);
    }

    #[test]
    fn crafter_can_use_the_existing_recipe_matcher_after_reload() {
        let mut crafter = Crafter::default();
        assert!(crafter.set_slot(1, Some(item("coal", 2))));
        assert!(crafter.set_slot(4, Some(item("stick", 1))));
        let mut crafter = Crafter::from_nbt(&saved(&crafter.to_nbt()));
        let output = crafter.craft(|slots| {
            let grid = slots
                .iter()
                .enumerate()
                .filter_map(|(index, item)| {
                    let item = item.as_ref()?;
                    let id = name_to_item(&item.id.to_string())?;
                    Some((index as i16 + 1, (id, item.count(), vec![])))
                })
                .collect();
            let (id, count, _) = craft3_result(&grid)?;
            Some((item(item_to_name(id)?, count), vec![]))
        });
        assert_eq!(output, Some((item("torch", 4), vec![])));
        assert_eq!(crafter.slots()[1], Some(item("coal", 1)));
        assert_eq!(crafter.slots()[4], None);
    }

    #[test]
    fn jukebox_resumes_at_saved_tick_and_keeps_disc_after_song_ends() {
        let fixture = compound([
            ("RecordItem", item_nbt("minecraft:music_disc_13", 1)),
            ("ticks_since_song_started", Tag::Long(3_579)),
        ]);
        let resolve = |disc: &BlockEntityItem| {
            (disc.id.path() == "music_disc_13").then_some(JukeboxSong {
                length_in_ticks: 3_560,
                comparator_output: 1,
            })
        };
        let mut jukebox = Jukebox::from_nbt(&fixture, resolve);
        assert_eq!(saved(&jukebox.to_nbt()), fixture);
        assert_eq!(jukebox.playing_ticks(), Some(3_579));
        assert_eq!(jukebox.tick(), None);
        assert_eq!(jukebox.tick(), Some(JukeboxTick::Stopped));
        assert_eq!(jukebox.tick(), None);
        assert_eq!(jukebox.playing_ticks(), None);
        assert_eq!(jukebox.comparator_signal(resolve), 1);
        assert_eq!(
            saved(&jukebox.to_nbt()),
            compound([("RecordItem", item_nbt("minecraft:music_disc_13", 1))])
        );

        let mut disc = Some(item("music_disc_13", 2));
        assert!(!jukebox.insert(&mut disc, resolve));
        assert_eq!(jukebox.eject(), Some(item("music_disc_13", 1)));
        assert!(!jukebox.insert(&mut Some(item("stone", 1)), resolve));
        assert!(jukebox.insert(&mut disc, resolve));
        assert_eq!(disc, Some(item("music_disc_13", 1)));
        assert_eq!(jukebox.playing_ticks(), Some(0));
        assert_eq!(jukebox.tick(), Some(JukeboxTick::MusicNote));
        for _ in 1..20 {
            assert_eq!(jukebox.tick(), None);
        }
        assert_eq!(jukebox.tick(), Some(JukeboxTick::MusicNote));
        assert_eq!(jukebox.eject(), Some(item("music_disc_13", 1)));
        assert_eq!(jukebox.comparator_signal(resolve), 0);

        let mut custom = item("stone", 1);
        custom.components.insert(
            NbtString::text("minecraft:jukebox_playable"),
            compound([("song", string("example:unregistered"))]),
        );
        let mut held = Some(custom.clone());
        assert!(jukebox.insert(&mut held, resolve));
        assert_eq!(held, None);
        assert_eq!(jukebox.playing_ticks(), None);
        assert_eq!(
            Jukebox::from_nbt(&saved(&jukebox.to_nbt()), resolve).eject(),
            Some(custom)
        );
    }

    #[test]
    fn banner_saves_registry_patterns_and_washes_the_last_layer_first() {
        let cross = compound([
            ("pattern", string("minecraft:cross")),
            ("color", string("red")),
        ]);
        let fixture = compound([
            (
                "patterns",
                Tag::List(
                    10,
                    vec![
                        cross.clone(),
                        compound([
                            ("pattern", string("example:sun")),
                            ("color", string("light_blue")),
                        ]),
                    ],
                ),
            ),
            ("CustomName", string("\"Harbor\"")),
        ]);
        let mut banner = Banner::from_nbt(&fixture);
        assert_eq!(saved(&banner.to_nbt()), fixture);
        let removed = banner.remove_last_pattern().unwrap();
        assert_eq!(removed.pattern.to_string(), "example:sun");
        assert_eq!(removed.color, BannerColor::LightBlue);
        assert_eq!(
            banner.to_nbt().get("patterns"),
            Some(&Tag::List(10, vec![cross]))
        );
        assert_eq!(
            banner.remove_last_pattern().unwrap().color,
            BannerColor::Red
        );
        assert_eq!(banner.remove_last_pattern(), None);
        assert_eq!(
            banner.to_nbt(),
            compound([("CustomName", string("\"Harbor\""))])
        );
    }

    #[test]
    fn skull_preserves_sound_and_signed_profile_properties() {
        let fixture = compound([
            ("note_block_sound", string("example:head_chime")),
            ("custom_name", string("\"A head\"")),
            (
                "profile",
                compound([
                    ("name", string("Alex")),
                    ("id", Tag::IntArray(vec![1, -2, 3, -4])),
                    (
                        "properties",
                        Tag::List(
                            10,
                            vec![compound([
                                ("name", string("textures")),
                                ("value", string("texture")),
                                ("signature", string("signed")),
                            ])],
                        ),
                    ),
                ]),
            ),
        ]);
        let mut skull = Skull::from_nbt(&fixture);
        assert_eq!(saved(&skull.to_nbt()), fixture);
        let profile = skull.profile.as_ref().unwrap();
        assert!(!profile.needs_resolution());
        assert_eq!(
            profile.id,
            Some(0x0000_0001_ffff_fffe_0000_0003_ffff_fffc_u128.to_be_bytes())
        );
        assert_eq!(
            skull.note_block_sound.take().unwrap().to_string(),
            "example:head_chime"
        );
        assert!(skull.to_nbt().get("note_block_sound").is_none());

        let named = Skull::from_nbt(&compound([("profile", string("Alex"))]));
        assert!(named.profile.as_ref().unwrap().needs_resolution());
        assert_eq!(
            named.to_nbt(),
            compound([("profile", compound([("name", string("Alex"))]))])
        );
        let unsigned = Skull::from_nbt(&compound([(
            "profile",
            compound([
                ("name", string("Alex")),
                (
                    "properties",
                    compound([("textures", Tag::List(8, vec![string("texture")]))]),
                ),
            ]),
        )]));
        let profile = unsigned.profile.as_ref().unwrap();
        assert!(!profile.needs_resolution());
        assert_eq!(
            profile.properties,
            [ProfileProperty {
                name: "textures".into(),
                value: "texture".into(),
                signature: None
            }]
        );
        assert_eq!(Skull::from_nbt(&saved(&unsigned.to_nbt())), unsigned);
    }

    #[test]
    fn brushing_rolls_once_and_completes_after_ten_rate_limited_strokes() {
        let fixture = compound([
            ("LootTable", string("minecraft:archaeology/desert_well")),
            ("LootTableSeed", Tag::Long(-73)),
        ]);
        let mut brushable = Brushable::from_nbt(&fixture);
        assert_eq!(saved(&brushable.to_nbt()), fixture);
        let artifact = item("arms_up_pottery_sherd", 1);
        assert_eq!(
            brushable.brush(100, Face::West, |table| {
                assert_eq!(table.id.to_string(), "minecraft:archaeology/desert_well");
                assert_eq!(table.seed, -73);
                Some(artifact.clone())
            }),
            BrushResult::Progress { dusted: 1 }
        );
        let saved_item = compound([("item", artifact.to_nbt())]);
        assert_eq!(saved(&brushable.to_nbt()), saved_item);
        assert_eq!(
            Brushable::from_nbt(&saved_item).item,
            Some(artifact.clone())
        );
        assert_eq!(
            brushable.update_nbt().get("hit_direction"),
            Some(&Tag::Int(4))
        );
        assert_eq!(
            Brushable::from_nbt(&brushable.update_nbt()).hit_direction,
            Some(Face::West)
        );
        let no_roll = |_: &BlockLootTable| panic!("Loot table may roll only once");
        assert_eq!(
            brushable.brush(109, Face::East, no_roll),
            BrushResult::CoolingDown
        );
        for stroke in 2..=9 {
            let result = brushable.brush(90 + i64::from(stroke) * 10, Face::East, no_roll);
            let dusted = if stroke == 2 {
                1
            } else if stroke < 6 {
                2
            } else {
                3
            };
            assert_eq!(result, BrushResult::Progress { dusted });
        }
        assert_eq!(
            brushable.brush(190, Face::East, no_roll),
            BrushResult::Completed {
                loot: Some(artifact),
                direction: Face::West
            }
        );
        assert_eq!(brushable.to_nbt(), compound([]));
        assert_eq!(
            brushable.brush(200, Face::East, no_roll),
            BrushResult::CoolingDown
        );
    }

    #[test]
    fn brushable_decay_refreshes_on_contact_and_clears_the_original_hit_face() {
        let mut brushable = Brushable::default();
        for now in [0, 10, 20] {
            brushable.brush(now, Face::North, |_| None);
        }
        assert_eq!(brushable.dusted(), 2);
        assert_eq!(
            brushable.brush(29, Face::East, |_| None),
            BrushResult::CoolingDown
        );
        assert_eq!(brushable.tick(68), None);
        assert_eq!(brushable.tick(69), Some(1));
        assert_eq!(brushable.tick(72), None);
        assert_eq!(brushable.tick(73), Some(0));
        assert_eq!(brushable.hit_direction, None);
        assert_eq!(
            brushable.brush(74, Face::South, |_| None),
            BrushResult::Progress { dusted: 1 }
        );
        assert_eq!(brushable.hit_direction, Some(Face::South));
    }

    fn uuid_list(values: &[[i32; 4]]) -> Tag {
        Tag::List(
            11,
            values.iter().map(|id| Tag::IntArray(id.to_vec())).collect(),
        )
    }

    #[test]
    fn trial_spawner_preserves_encounter_nbt_and_advances_spawn_and_reward_counters() {
        let player = 0x0000_0001_0000_0002_0000_0003_0000_0004_u128.to_be_bytes();
        let first = 0x0000_0004_0000_0003_0000_0002_0000_0001_u128.to_be_bytes();
        let second = 5_u128.to_be_bytes();
        let fixture = compound([
            ("normal_config", compound([("spawn_range", Tag::Int(5))])),
            (
                "ominous_config",
                compound([("ticks_between_spawn", Tag::Int(10))]),
            ),
            (
                "spawn_data",
                compound([("entity", compound([("id", string("minecraft:breeze"))]))]),
            ),
            (
                "ejecting_loot_table",
                string("minecraft:spawners/trial_chamber/key"),
            ),
            ("registered_players", uuid_list(&[[1, 2, 3, 4]])),
            ("current_mobs", uuid_list(&[[4, 3, 2, 1]])),
            ("cooldown_ends_at", Tag::Long(60)),
            ("next_mob_spawns_at", Tag::Long(125)),
            ("total_mobs_spawned", Tag::Int(1)),
            ("target_cooldown_length", Tag::Int(150)),
            ("required_player_range", Tag::Int(9)),
        ]);
        let mut spawner = TrialSpawner::from_nbt(&fixture);
        assert_eq!(saved(&spawner.to_nbt()), fixture);
        assert_eq!(spawner.required_player_range, 9);
        assert!(!spawner.record_spawn(124, 30, second));
        assert!(!spawner.record_spawn(125, 30, first));
        assert!(spawner.record_spawn(125, 30, second));
        assert_eq!(spawner.current_mobs, [first, second]);
        assert_eq!(spawner.next_mob_spawns_at, 155);
        assert_eq!(spawner.total_mobs_spawned, 2);
        assert!(!spawner.begin_rewards(135));
        assert!(spawner.remove_mob(135, 30, first));
        assert!(!spawner.remove_mob(136, 30, first));
        assert_eq!(spawner.next_mob_spawns_at, 165);
        assert!(spawner.remove_mob(140, 30, second));
        assert!(spawner.begin_rewards(140));
        assert_eq!(spawner.cooldown_ends_at, 290);
        assert_eq!(spawner.next_mob_spawns_at, 0);
        assert_eq!(spawner.total_mobs_spawned, 0);
        assert_eq!(spawner.registered_players, [player]);
        assert_eq!(TrialSpawner::from_nbt(&saved(&spawner.to_nbt())), spawner);
        assert_eq!(
            TrialSpawner::from_nbt(&compound([])),
            TrialSpawner::default()
        );
    }

    #[test]
    fn vault_charges_a_matching_key_once_and_resumes_reward_ejection_after_save() {
        let _guard = crate::world_guard();
        let player = 0x0000_0001_0000_0002_0000_0003_0000_0004_u128.to_be_bytes();
        let previous = 0x0000_0004_0000_0003_0000_0002_0000_0001_u128.to_be_bytes();
        let nearby = 5_u128.to_be_bytes();
        let fixture = compound([
            (
                "config",
                compound([
                    ("loot_table", string("example:treasure")),
                    ("activation_range", Tag::Double(5.0_f64.to_bits())),
                    ("deactivation_range", Tag::Double(6.0_f64.to_bits())),
                    ("key_item", item_nbt("minecraft:ominous_trial_key", 2)),
                    ("override_loot_table_to_display", string("example:preview")),
                ]),
            ),
            (
                "shared_data",
                compound([
                    ("display_item", item_nbt("minecraft:emerald", 5)),
                    ("connected_players", uuid_list(&[[1, 2, 3, 4]])),
                    ("connected_particles_range", Tag::Double(5.0_f64.to_bits())),
                ]),
            ),
            (
                "server_data",
                compound([
                    ("rewarded_players", uuid_list(&[[4, 3, 2, 1]])),
                    ("state_updating_resumes_at", Tag::Long(100)),
                ]),
            ),
        ]);
        let mut vault = Vault::from_nbt(&fixture, VaultPhase::Active);
        assert_eq!(saved(&vault.to_nbt()), fixture);
        let mut key = Some(item("ominous_trial_key", 3));
        let mut too_few = Some(item("ominous_trial_key", 1));
        assert!(!vault.try_unlock(90, player, &mut too_few, |_| panic!("need two keys")));
        assert!(!vault.try_unlock(90, previous, &mut key, |_| panic!("already rewarded")));
        assert!(!vault.try_unlock(90, player, &mut key, |_| vec![]));
        assert_eq!(key, Some(item("ominous_trial_key", 3)));
        assert!(vault.try_unlock(90, player, &mut key, |table| {
            assert_eq!(table.to_string(), "example:treasure");
            vec![item("diamond_block", 1), item("emerald", 4)]
        }));
        assert_eq!(key, Some(item("ominous_trial_key", 1)));
        assert_eq!(vault.phase, VaultPhase::Unlocking);
        assert_eq!(vault.total_ejections_needed, 2);
        assert_eq!(vault.display_item, Some(item("emerald", 4)));
        let mut vault = Vault::from_nbt(&saved(&vault.to_nbt()), VaultPhase::Unlocking);
        assert_eq!(vault.rewarded_players, [previous, player]);
        assert_eq!(
            vault.items_to_eject,
            [item("diamond_block", 1), item("emerald", 4)]
        );
        assert_eq!(vault.state_updating_resumes_at, 104);
        assert_eq!(vault.total_ejections_needed, 2);
        assert_eq!(vault.tick(103, |_| panic!("unlocking")), None);
        assert_eq!(vault.tick(104, |_| panic!("starts ejection")), None);
        assert_eq!(vault.phase, VaultPhase::Ejecting);
        assert_eq!(vault.tick(123, |_| panic!("ejection interval")), None);
        assert_eq!(
            vault.tick(124, |_| panic!("eject first")),
            Some(item("emerald", 4))
        );
        assert_eq!(vault.display_item, Some(item("diamond_block", 1)));
        assert_eq!(
            vault.tick(144, |_| panic!("eject second")),
            Some(item("diamond_block", 1))
        );
        assert_eq!(vault.display_item, None);
        assert_eq!(
            vault.tick(164, |range| {
                assert_eq!(range, 6.0);
                vec![player, previous]
            }),
            None
        );
        assert_eq!(vault.phase, VaultPhase::Inactive);
        assert_eq!(vault.total_ejections_needed, 0);
        assert_eq!(
            vault.tick(184, |range| {
                assert_eq!(range, 5.0);
                vec![nearby, player, nearby]
            }),
            None
        );
        assert_eq!(vault.connected_players, [nearby]);
        assert_eq!(vault.phase, VaultPhase::Active);
        assert_eq!(vault.connected_particles_range, 5.0);
        assert_eq!(
            Vault::from_nbt(&saved(&vault.to_nbt()), VaultPhase::Active),
            vault
        );
    }
    struct LiveWorld {
        store: LiveBlockEntityStore,
        time: u64,
        players: Vec<Arc<Client>>,
        sessions: HashMap<i32, PlayerMechanismSession>,
        drops: Vec<Drop>,
        rng: u64,
        cells: Vec<(DimBlockPos, u32)>,
        map_states: HashMap<i64, MapState>,
    }

    impl LiveWorld {
        fn new() -> Self {
            Self {
                store: std::mem::take(&mut *live_block_entities().lock().unwrap_or_else(|e| e.into_inner())),
                time: TICK_IDX.swap(0, Ordering::Relaxed),
                players: std::mem::take(&mut *clients().lock().unwrap_or_else(|e| e.into_inner())),
                sessions: std::mem::take(&mut *player_mechanism_sessions().lock().unwrap_or_else(|e| e.into_inner())),
                drops: std::mem::take(&mut *drops().lock().unwrap_or_else(|e| e.into_inner())),
                rng: world_rng::saved_state(),
                cells: Vec::new(),
                map_states: std::mem::take(&mut *live_map_states().lock().unwrap_or_else(|e| e.into_inner())),
            }
        }

        fn place(&mut self, key @ (dim, x, y, z): DimBlockPos, block: &str) -> u32 {
            let state = item_to_state(name_to_item(block).unwrap()).unwrap();
            let old = block_at_dim(dim, x, y, z);
            self.cells.push((key, old));
            place_block_dim(dim, x, y, z, state);
            assert_eq!(block_at_dim(dim, x, y, z), state, "{block}");
            state
        }

        fn tick_at(&self, age: u64) {
            TICK_IDX.store(age / u64::from(GAME_TICKS_PER_TICK), Ordering::Relaxed);
            live_menu_tick();
        }
    }

    impl std::ops::Drop for LiveWorld {
        fn drop(&mut self) {
            for &((dim, x, y, z), state) in self.cells.iter().rev() {
                set_block_dim(dim, x, y, z, state);
            }
            world_rng::restore(self.rng);
            TICK_IDX.store(self.time, Ordering::Relaxed);
            *live_block_entities().lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.store);
            *clients().lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.players);
            *player_mechanism_sessions().lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.sessions);
            *drops().lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.drops);
            *live_map_states().lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.map_states);
        }
    }

    fn reload_at((dim, x, y, z): DimBlockPos) -> Tag {
        let key = (dim, x, y, z);
        let (snapshot_state, tag) = live_block_entity_snapshot(key).unwrap();
        let decoded = saved(&tag);
        assert!(live_block_entities()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entries
            .remove(&key)
            .is_some());
        assert!(restore_live_block_entity(key, &decoded));
        assert_eq!(
            live_block_entity_snapshot(key),
            Some((snapshot_state, decoded.clone()))
        );
        decoded
    }

    fn attach_live_menu(c: &Client) {
        c.enter_play_wire(None, Default::default());
        player_mechanism_sessions().lock().unwrap_or_else(|e| e.into_inner()).insert(
            c.eid,
            PlayerMechanismSession {
                xp_bar: (0, 0.0),
                food_timer: 0,
                menus: Arc::new(LiveMenuConnection {
                    active: Mutex::new(None),
                    next_id: std::sync::atomic::AtomicU8::new(4),
                    enchantment_seed: Mutex::new(0),
                }),
            },
        );
    }

    fn put_hand(c: &Client, item: &str, count: u8) {
        c.inv
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(36, (name_to_item(item).unwrap(), count, vec![]));
    }

    fn click_block(c: &Client, (dim, x, y, z): DimBlockPos) -> bool {
        open_live_block_menu(c, (dim, x, y, z), block_at_dim(dim, x, y, z))
    }

    #[test]
    fn live_block_entity_placement_discovers_all_families_and_restores_nbt() {
        let _guard = world_guard();
        let mut world = LiveWorld::new();
        let cases = [
            ("beacon", "beacon"),
            ("soul_campfire", "campfire"),
            ("lectern", "lectern"),
            ("chiseled_bookshelf", "chiseled_bookshelf"),
            ("decorated_pot", "decorated_pot"),
            ("crafter", "crafter"),
            ("jukebox", "jukebox"),
            ("red_banner", "banner"),
            ("player_head", "skull"),
            ("suspicious_gravel", "brushable_block"),
            ("trial_spawner", "trial_spawner"),
            ("vault", "vault"),
        ];
        for (i, (block, _)) in cases.iter().enumerate() {
            world.place((DimId::Nether, 91_000 + i as i32, 220, 91_000), block);
        }
        world.tick_at(0);
        for (i, (_, id)) in cases.iter().enumerate() {
            let key = (DimId::Nether, 91_000 + i as i32, 220, 91_000);
            // Discovery must already have installed the entity before any use or snapshot.
            assert!(
                live_block_entities()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .entries
                    .contains_key(&key),
                "{id}"
            );
            let tag = reload_at(key);
            assert_eq!(
                block_entity_str_field(&tag, "id"),
                Some(format!("minecraft:{id}"))
            );
            assert_eq!(block_entity_int(&tag, "x"), key.1);
            assert_eq!(block_entity_int(&tag, "y"), key.2);
            assert_eq!(block_entity_int(&tag, "z"), key.3);
        }
        let key @ (dim, x, y, z) = (DimId::Nether, 91_007, 220, 91_000);
        let banner = compound([
            ("id", string("minecraft:banner")),
            (
                "patterns",
                Tag::List(
                    10,
                    vec![compound([
                        ("pattern", string("minecraft:stripe_center")),
                        ("color", string("blue")),
                    ])],
                ),
            ),
        ]);
        assert!(restore_live_block_entity(key, &banner));
        let reg = breg().unwrap();
        set_block_dim(
            dim,
            x,
            y,
            z,
            reg.with_prop(block_at_dim(dim, x, y, z), "rotation", "3"),
        );
        world.tick_at(20);
        assert_eq!(reload_at(key).get("patterns"), banner.get("patterns"));
        let head_key = (dim, x + 1, y, z);
        assert!(!restore_live_block_entity(head_key, &banner));
        let skull = compound([
            ("id", string("minecraft:skull")),
            ("profile", compound([("name", string("Alex"))])),
            (
                "note_block_sound",
                string("minecraft:block.note_block.harp"),
            ),
        ]);
        assert!(restore_live_block_entity(head_key, &skull));
        assert_eq!(reload_at(head_key).get("profile"), skull.get("profile"));
        assert!(live_block_entity_snapshot((DimId::Overworld, x, y, z)).is_none());
        set_block_dim(dim, x, y, z, 0);
        world.tick_at(40);
        assert!(!live_block_entities()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entries
            .contains_key(&key));
        world.place(key, "red_banner");
        assert!(reload_at(key).get("patterns").is_none());
    }

    #[test]
    fn live_block_entity_use_consumes_items_and_updates_world_state() {
        let _guard = world_guard();
        let mut world = LiveWorld::new();
        let (client, _receiver) = effect_tests::client_pair();
        let reg = breg().unwrap();
        let (dim, x, y, z) = (DimId::Overworld, 92_000, 312, 92_000);
        *client.pos.lock().unwrap_or_else(|e| e.into_inner()) = (
            x as f64 + 0.5,
            y as f64 - 0.7,
            z as f64 - 1.0,
            0.0,
            0.0,
            true,
        );
        attach_live_menu(&client);

        let shelf = (dim, x, y, z);
        let shelf_state = world.place(shelf, "chiseled_bookshelf");
        set_block_dim(dim, x, y, z, reg.with_prop(shelf_state, "facing", "north"));
        put_hand(&client, "book", 2);
        assert!(click_block(&client, shelf));
        assert_eq!(client.inv.lock().unwrap_or_else(|e| e.into_inner())[&36].1, 1);
        assert_eq!(
            reg.get_prop(block_at_dim(dim, x, y, z), "slot_1_occupied")
                .as_deref(),
            Some("true")
        );
        let shelf_tag = reload_at(shelf);
        assert_eq!(block_entity_int(&shelf_tag, "last_interacted_slot"), 1);
        assert!(click_block(&client, shelf));
        assert_eq!(client.inv.lock().unwrap_or_else(|e| e.into_inner())[&36].1, 2);

        let pot = (dim, x + 1, y, z);
        world.place(pot, "decorated_pot");
        put_hand(&client, "brick", 3);
        assert!(click_block(&client, pot));
        put_hand(&client, "emerald", 1);
        assert!(!click_block(&client, pot));
        assert_eq!(client.inv.lock().unwrap_or_else(|e| e.into_inner())[&36].1, 1);
        assert_eq!(
            reload_at(pot).get("item"),
            Some(&item_nbt("minecraft:brick", 1))
        );

        let lectern = (dim, x + 2, y, z);
        world.place(lectern, "lectern");
        put_hand(&client, "writable_book", 1);
        assert!(click_block(&client, lectern));
        assert!(!client.inv.lock().unwrap_or_else(|e| e.into_inner()).contains_key(&36));
        let (_, inserted) = live_block_entity_snapshot(lectern).unwrap();
        assert_eq!(
            inserted.get("Book"),
            Some(&item_nbt("minecraft:writable_book", 1))
        );
        assert_eq!(block_entity_int(&inserted, "Page"), 0);
        assert!(click_block(&client, lectern));
        let window = live_menu_id(&client).unwrap();
        // The native clamp returns -1 when the selected book has no pages.
        with_save_guard(|| live_enchant_button(&client, i32::from(window), 2));
        assert_eq!(block_entity_int(&reload_at(lectern), "Page"), -1);
        with_save_guard(|| live_enchant_button(&client, i32::from(window), 3));
        assert_eq!(
            reg.get_prop(block_at_dim(dim, x + 2, y, z), "has_book")
                .as_deref(),
            Some("false")
        );
        assert!(reload_at(lectern).get("Book").is_none());

        let box_key = (dim, x + 3, y, z);
        world.place(box_key, "jukebox");
        put_hand(&client, "music_disc_13", 1);
        assert!(click_block(&client, box_key));
        world.tick_at(0);
        let jukebox = reload_at(box_key);
        assert_eq!(
            jukebox.get("RecordItem"),
            Some(&item_nbt("minecraft:music_disc_13", 1))
        );
        assert_eq!(
            block_entity_number(&jukebox, "ticks_since_song_started"),
            Some(i64::from(GAME_TICKS_PER_TICK))
        );
        assert!(click_block(&client, box_key));
        assert!(reload_at(box_key).get("RecordItem").is_none());
        assert!(drops()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|drop| item_to_name(drop.item) == Some("music_disc_13")));
    }

    #[test]
    fn live_block_entity_ticks_resume_saved_progress_and_run_world_transitions() {
        let _guard = world_guard();
        let mut world = LiveWorld::new();
        let (client, _receiver) = effect_tests::client_pair();
        let client = Arc::new(client);
        clients().lock().unwrap_or_else(|e| e.into_inner()).push(client.clone());
        let reg = breg().unwrap();
        let (dim, x, y, z) = (DimId::Overworld, 93_000, 318, 93_000);
        *client.pos.lock().unwrap_or_else(|e| e.into_inner()) = (
            x as f64 + 0.5,
            y as f64 - 0.7,
            z as f64 - 1.0,
            0.0,
            0.0,
            true,
        );
        attach_live_menu(&client);

        let beacon = (dim, x, y, z);
        world.place(beacon, "beacon");
        for dx in -1..=1 {
            for dz in -1..=1 {
                let key = (dim, x + dx, y - 1, z + dz);
                world
                    .cells
                    .push((key, block_at_dim(dim, key.1, key.2, key.3)));
                set_block_dim(
                    dim,
                    key.1,
                    key.2,
                    key.3,
                    reg.state_of("minecraft:iron_block", None),
                );
            }
        }
        assert!(restore_live_block_entity(
            beacon,
            &compound([
                ("id", string("minecraft:beacon")),
                ("primary_effect", string("minecraft:speed")),
            ])
        ));

        let campfire = (dim, x + 1, y, z);
        world.place(campfire, "campfire");
        put_hand(&client, "beef", 2);
        assert!(click_block(&client, campfire));
        assert_eq!(client.inv.lock().unwrap_or_else(|e| e.into_inner())[&36].1, 1);
        for now in (0..600).step_by(GAME_TICKS_PER_TICK as usize) {
            world.tick_at(now);
            if now == 200 {
                assert_eq!(
                    block_entity_int_array(&reload_at(campfire), "CookingTimes")[0],
                    200 + GAME_TICKS_PER_TICK as i32
                );
            }
        }
        assert!(block_entity_entries(&reload_at(campfire), "Items").is_empty());
        assert!(drops()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|drop| item_to_name(drop.item) == Some("cooked_beef")));
        assert_eq!(block_entity_int(&reload_at(beacon), "Levels"), 1);
        assert!(client
            .effs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|effect| effect.id == 0));

        let crafter = (dim, x + 2, y, z);
        world.place(crafter, "crafter");
        assert!(click_block(&client, crafter));
        let window = live_menu_id(&client).unwrap();
        *client.cursor.lock().unwrap_or_else(|e| e.into_inner()) = (name_to_item("oak_log").unwrap(), 2, vec![]);
        with_save_guard(|| live_menu_click(&client, window, 0, 0, 0));
        assert_eq!(block_entity_entries(&reload_at(crafter), "Items").len(), 1);
        let power = (dim, x + 3, y, z);
        world
            .cells
            .push((power, block_at_dim(dim, power.1, power.2, power.3)));
        set_block_dim(
            dim,
            power.1,
            power.2,
            power.3,
            reg.state_of("minecraft:redstone_block", None),
        );
        for now in (600..620).step_by(GAME_TICKS_PER_TICK as usize) {
            world.tick_at(now);
        }
        assert_eq!(
            block_entity_number(
                &block_entity_entries(&reload_at(crafter), "Items")[0],
                "count"
            ),
            Some(1)
        );
        assert!(drops()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|drop| item_to_name(drop.item) == Some("oak_planks") && drop.cnt == 4));
        assert_eq!(
            reg.get_prop(block_at_dim(dim, x + 2, y, z), "crafting")
                .as_deref(),
            Some("false")
        );

        let brushable = (dim, x, y, z + 2);
        world.place(brushable, "suspicious_sand");
        put_hand(&client, "brush", 1);
        assert!(click_block(&client, brushable));
        assert_eq!(
            reg.get_prop(block_at_dim(dim, x, y, z + 2), "dusted")
                .as_deref(),
            Some("1")
        );
        world.tick_at(680);
        assert_eq!(
            reg.get_prop(block_at_dim(dim, x, y, z + 2), "dusted")
                .as_deref(),
            Some("0")
        );

        let spawner = (dim, x, y, z + 1);
        world.place(spawner, "trial_spawner");
        assert!(restore_live_block_entity(
            spawner,
            &compound([
                ("id", string("minecraft:trial_spawner")),
                (
                    "spawn_data",
                    compound([("entity", compound([("id", string("minecraft:zombie"))]))])
                ),
                ("next_mob_spawns_at", Tag::Long(i64::MAX)),
            ])
        ));
        let vault = (dim, x + 1, y, z + 1);
        world.place(vault, "vault");
        world.tick_at(700);
        assert_eq!(
            reg.get_prop(block_at_dim(dim, x, y, z + 1), "trial_spawner_state")
                .as_deref(),
            Some("active")
        );
        assert_eq!(
            block_entity_entries(&reload_at(spawner), "registered_players").len(),
            1
        );
        assert_eq!(
            reg.get_prop(block_at_dim(dim, x + 1, y, z + 1), "vault_state")
                .as_deref(),
            Some("active")
        );
        assert_eq!(
            block_entity_entries(
                reload_at(vault).get("shared_data").unwrap(),
                "connected_players"
            )
            .len(),
            1
        );
        let table = "example:live_vault_test";
        let mut vault_tag = reload_at(vault);
        let config = compound([
            ("loot_table", string(table)),
            ("key_item", item_nbt("minecraft:trial_key", 1)),
        ]);
        let Tag::Compound(fields) = &mut vault_tag else {
            unreachable!()
        };
        block_entity_put(fields, "config", config);
        assert!(restore_live_block_entity(vault, &vault_tag));
        let old_table = loot_tables().lock().unwrap_or_else(|e| e.into_inner()).insert(
            table.into(),
            vec![LootPool {
                rolls: 1,
                chance: 1.0,
                entries: vec![LootEntry {
                    item: name_to_item("emerald").unwrap(),
                    min: 2,
                    max: 2,
                    funcs: Vec::new(),
                }],
                funcs: Vec::new(),
            }],
        );
        put_hand(&client, "trial_key", 2);
        let unlocked = click_block(&client, vault);
        let mut tables = loot_tables().lock().unwrap_or_else(|e| e.into_inner());
        tables.remove(table);
        if let Some(old) = old_table {
            tables.insert(table.into(), old);
        }
        drop(tables);
        assert!(unlocked);
        assert_eq!(client.inv.lock().unwrap_or_else(|e| e.into_inner())[&36].1, 1);
        assert!(!click_block(&client, vault));
        let pending = reload_at(vault);
        assert_eq!(
            block_entity_entries(pending.get("server_data").unwrap(), "items_to_eject").len(),
            1
        );
        world.tick_at(720);
        world.tick_at(740);
        assert!(drops()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|drop| item_to_name(drop.item) == Some("emerald") && drop.cnt == 2));
    }

    #[test]
    fn live_block_entity_same_type_rewrite_reloads_payload() {
        let _guard = world_guard();
        let mut world = LiveWorld::new();
        let (client, _receiver) = effect_tests::client_pair();
        let (dim, x, y, z) = (DimId::Overworld, 94_000, 322, 94_000);
        *client.pos.lock().unwrap_or_else(|e| e.into_inner()) = (
            x as f64 + 0.5,
            y as f64 - 0.7,
            z as f64 - 1.0,
            0.0,
            0.0,
            true,
        );
        attach_live_menu(&client);
        let key = (dim, x, y, z);
        world.place(key, "jukebox");
        put_hand(&client, "music_disc_13", 1);
        assert!(click_block(&client, key));
        world.tick_at(0);
        assert!(reload_at(key).get("RecordItem").is_some());
        // Break and re-place the same block before the next scan: vanilla
        // creates a fresh block entity, so the old disc must not survive.
        set_block_dim(dim, x, y, z, 0);
        world.place(key, "jukebox");
        let (_, early) = live_block_entity_snapshot(key).unwrap();
        assert!(
            early.get("RecordItem").is_none(),
            "a same-type rewrite clears the payload before the next tick scan"
        );
        world.tick_at(u64::from(GAME_TICKS_PER_TICK));
        assert!(reload_at(key).get("RecordItem").is_none());
    }

    #[test]
    fn live_menu_stonecutter_opens_selects_button_and_crafts() {
        let _guard = world_guard();
        let mut world = LiveWorld::new();
        let (client, _receiver) = effect_tests::client_pair();
        let (dim, x, y, z) = (DimId::Overworld, 94_000, 320, 94_000);
        *client.pos.lock().unwrap_or_else(|e| e.into_inner()) = (
            x as f64 + 0.5,
            y as f64 + 0.5,
            z as f64 - 1.5,
            0.0,
            0.0,
            true,
        );
        attach_live_menu(&client);

        let key = (dim, x, y, z);
        world.place(key, "stonecutter");
        assert!(click_block(&client, key));
        let window = live_menu_id(&client).unwrap();
        {
            let connection = live_menu_connection(&client).unwrap();
            let active = connection.active.lock().unwrap_or_else(|e| e.into_inner());
            let session = active.as_ref().unwrap();
            assert!(matches!(session.menu, LiveMenu::Stonecutter(_)));
            assert_eq!(session.menu.screen(), (24, "Stonecutter"));
        }

        // Four stone into input slot 0 -> the deterministic recipe list appears.
        *client.cursor.lock().unwrap_or_else(|e| e.into_inner()) = (name_to_item("stone").unwrap(), 4, vec![]);
        with_save_guard(|| live_menu_click(&client, window, 0, 0, 0));
        let recipes = {
            let connection = live_menu_connection(&client).unwrap();
            let active = connection.active.lock().unwrap_or_else(|e| e.into_inner());
            let LiveMenu::Stonecutter(menu) = &active.as_ref().unwrap().menu else {
                panic!("stonecutter session lost")
            };
            assert_eq!(menu.input.1, 4);
            menu.available_recipes()
        };
        assert_eq!(recipes.len(), 7);

        // ContainerButtonClick selects recipe index 0; the result slot fills.
        with_save_guard(|| live_enchant_button(&client, i32::from(window), 0));
        with_save_guard(|| live_menu_click(&client, window, 1, 0, 0));
        let taken = client.cursor.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert_eq!(taken, (recipes[0].0, recipes[0].1, vec![]));
        {
            let connection = live_menu_connection(&client).unwrap();
            let active = connection.active.lock().unwrap_or_else(|e| e.into_inner());
            let LiveMenu::Stonecutter(menu) = &active.as_ref().unwrap().menu else {
                panic!("stonecutter session lost")
            };
            // One input consumed; the selection survives while input remains.
            assert_eq!(menu.input.1, 3);
            assert_eq!(menu.selected_recipe, Some(0));
        }

        // Emptying the input clears the selection; the menu stays live.
        *client.cursor.lock().unwrap_or_else(|e| e.into_inner()) = Stack::default();
        with_save_guard(|| live_menu_click(&client, window, 0, 0, 0));
        assert_eq!(client.cursor.lock().unwrap_or_else(|e| e.into_inner()).1, 3);
        {
            let connection = live_menu_connection(&client).unwrap();
            let active = connection.active.lock().unwrap_or_else(|e| e.into_inner());
            let LiveMenu::Stonecutter(menu) = &active.as_ref().unwrap().menu else {
                panic!("stonecutter session lost")
            };
            assert_eq!(menu.selected_recipe, None);
        }
    }

    #[test]
    fn live_menu_grindstone_disenchants_repairs_and_awards_xp() {
        let _guard = world_guard();
        let mut world = LiveWorld::new();
        let (client, _receiver) = effect_tests::client_pair();
        let (dim, x, y, z) = (DimId::Overworld, 95_000, 320, 95_000);
        *client.pos.lock().unwrap_or_else(|e| e.into_inner()) = (
            x as f64 + 0.5,
            y as f64 + 0.5,
            z as f64 - 1.5,
            0.0,
            0.0,
            true,
        );
        attach_live_menu(&client);

        let key = (dim, x, y, z);
        world.place(key, "grindstone");
        assert!(click_block(&client, key));
        let window = live_menu_id(&client).unwrap();
        {
            let connection = live_menu_connection(&client).unwrap();
            let active = connection.active.lock().unwrap_or_else(|e| e.into_inner());
            let session = active.as_ref().unwrap();
            assert!(matches!(session.menu, LiveMenu::Grindstone(_)));
            assert_eq!(session.menu.screen(), (15, "Repair & Disenchant"));
        }

        // Disenchant: one sword carrying sharpness II + binding_curse keeps only
        // the curse and refunds XP in vanilla's [j, 2j-1] band.
        let sword = name_to_item("diamond_sword").unwrap();
        let mut comps = Vec::new();
        comp_set(
            &mut comps,
            COMP_ENCHANTMENTS,
            enchantments_data(&[(32, 2), (2, 1)], true),
        );
        comp_set_varint(&mut comps, COMP_MAX_DAMAGE, 1561);
        comp_set_varint(&mut comps, COMP_DAMAGE, 40);
        *client.cursor.lock().unwrap_or_else(|e| e.into_inner()) = (sword, 1, comps);
        with_save_guard(|| live_menu_click(&client, window, 0, 0, 0));
        let xp_before = *client.xp.lock().unwrap_or_else(|e| e.into_inner());
        with_save_guard(|| live_menu_click(&client, window, 2, 0, 0));
        let taken = client.cursor.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert_eq!(taken.0, sword);
        assert_eq!(taken.1, 1);
        assert_eq!(enchantments_get(&taken.2), vec![(2, 1)]);
        assert_eq!(comp_varint(&taken.2, COMP_DAMAGE), Some(40));
        // xp_base = min_cost(sharpness II) = 11 -> j = 6, xp in [6, 11].
        let gained = *client.xp.lock().unwrap_or_else(|e| e.into_inner()) - xp_before;
        assert!((6..=11).contains(&gained), "grindstone xp {gained}");
        {
            let connection = live_menu_connection(&client).unwrap();
            let active = connection.active.lock().unwrap_or_else(|e| e.into_inner());
            let LiveMenu::Grindstone(menu) = &active.as_ref().unwrap().menu else {
                panic!("grindstone session lost")
            };
            assert_eq!(menu.top.1, 0);
            assert_eq!(menu.bottom.1, 0);
        }

        // Repair: two damaged swords merge durability with the vanilla +5%
        // max-damage bonus and strip every enchantment (no curses to keep).
        let mut damaged = Vec::new();
        comp_set(&mut damaged, COMP_ENCHANTMENTS, enchantments_data(&[(32, 2)], true));
        comp_set_varint(&mut damaged, COMP_MAX_DAMAGE, 1561);
        comp_set_varint(&mut damaged, COMP_DAMAGE, 1200);
        let mut damaged_b = Vec::new();
        comp_set(&mut damaged_b, COMP_ENCHANTMENTS, enchantments_data(&[(34, 3)], true));
        comp_set_varint(&mut damaged_b, COMP_MAX_DAMAGE, 1561);
        comp_set_varint(&mut damaged_b, COMP_DAMAGE, 1200);
        *client.cursor.lock().unwrap_or_else(|e| e.into_inner()) = (sword, 1, damaged);
        with_save_guard(|| live_menu_click(&client, window, 0, 0, 0));
        *client.cursor.lock().unwrap_or_else(|e| e.into_inner()) = (sword, 1, damaged_b);
        with_save_guard(|| live_menu_click(&client, window, 1, 0, 0));
        let xp_before = *client.xp.lock().unwrap_or_else(|e| e.into_inner());
        with_save_guard(|| live_menu_click(&client, window, 2, 0, 0));
        let repaired = client.cursor.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert_eq!(repaired.0, sword);
        assert_eq!(repaired.1, 1);
        assert!(enchantments_get(&repaired.2).is_empty());
        // 361 + 361 + 1561*5% = 800 remaining -> 1561 - 800 = 761 damage.
        assert_eq!(comp_varint(&repaired.2, COMP_DAMAGE), Some(761));
        // xp_base = 11 + 21 = 32 -> j = 16, xp in [16, 31].
        let gained = *client.xp.lock().unwrap_or_else(|e| e.into_inner()) - xp_before;
        assert!((16..=31).contains(&gained), "grindstone xp {gained}");
    }

    #[test]
    fn live_menu_loom_and_cartography_roundtrip_and_return_inputs_on_close() {
        let _guard = world_guard();
        let mut world = LiveWorld::new();
        let (client, _receiver) = effect_tests::client_pair();
        let (dim, x, y, z) = (DimId::Overworld, 96_000, 320, 96_000);
        *client.pos.lock().unwrap_or_else(|e| e.into_inner()) = (
            x as f64 + 0.5,
            y as f64 + 0.5,
            z as f64 - 1.5,
            0.0,
            0.0,
            true,
        );
        attach_live_menu(&client);

        // ---- loom ----
        let loom = (dim, x, y, z);
        world.place(loom, "loom");
        assert!(click_block(&client, loom));
        let window = live_menu_id(&client).unwrap();
        {
            let connection = live_menu_connection(&client).unwrap();
            let active = connection.active.lock().unwrap_or_else(|e| e.into_inner());
            assert_eq!(active.as_ref().unwrap().menu.screen(), (18, "Loom"));
        }
        let banner = name_to_item("white_banner").unwrap();
        *client.cursor.lock().unwrap_or_else(|e| e.into_inner()) = (banner, 1, vec![]);
        with_save_guard(|| live_menu_click(&client, window, 0, 0, 0));
        *client.cursor.lock().unwrap_or_else(|e| e.into_inner()) = (name_to_item("red_dye").unwrap(), 3, vec![]);
        with_save_guard(|| live_menu_click(&client, window, 1, 0, 0));
        // Button 13 is "cross" in the no-item-required pattern list.
        with_save_guard(|| live_enchant_button(&client, i32::from(window), 13));
        with_save_guard(|| live_menu_click(&client, window, 3, 0, 0));
        let taken = client.cursor.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert_eq!(taken.0, banner);
        assert_eq!(
            live_banner_layers(&taken),
            vec![BannerLayer {
                pattern: "cross".to_string(),
                color: 14
            }]
        );
        {
            let connection = live_menu_connection(&client).unwrap();
            let active = connection.active.lock().unwrap_or_else(|e| e.into_inner());
            let LiveMenu::Loom(menu) = &active.as_ref().unwrap().menu else {
                panic!("loom session lost")
            };
            assert_eq!(menu.banner.1, 0);
            assert_eq!(menu.dye.1, 2);
        }

        // A banner-pattern item narrows the selectable list and survives takes.
        client
            .inv
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(10, std::mem::take(&mut *client.cursor.lock().unwrap_or_else(|e| e.into_inner())));
        *client.cursor.lock().unwrap_or_else(|e| e.into_inner()) = (banner, 1, vec![]);
        with_save_guard(|| live_menu_click(&client, window, 0, 0, 0));
        *client.cursor.lock().unwrap_or_else(|e| e.into_inner()) =
            (name_to_item("creeper_banner_pattern").unwrap(), 1, vec![]);
        with_save_guard(|| live_menu_click(&client, window, 2, 0, 0));
        with_save_guard(|| live_enchant_button(&client, i32::from(window), 0));
        with_save_guard(|| live_menu_click(&client, window, 3, 0, 0));
        let taken = client.cursor.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert_eq!(
            live_banner_layers(&taken),
            vec![BannerLayer {
                pattern: "creeper".to_string(),
                color: 14
            }]
        );
        {
            let connection = live_menu_connection(&client).unwrap();
            let active = connection.active.lock().unwrap_or_else(|e| e.into_inner());
            let LiveMenu::Loom(menu) = &active.as_ref().unwrap().menu else {
                panic!("loom session lost")
            };
            assert_eq!(menu.dye.1, 1);
            assert_eq!(menu.pattern_item.1, 1);
        }
        // Closing returns the remaining dye and the (preserved) pattern item.
        with_save_guard(|| close_live_menu(&client, true, true));
        {
            let inventory = client.inv.lock().unwrap_or_else(|e| e.into_inner());
            assert!(inventory
                .values()
                .any(|s| item_to_name(s.0) == Some("red_dye") && s.1 == 1));
            assert!(inventory
                .values()
                .any(|s| item_to_name(s.0) == Some("creeper_banner_pattern") && s.1 == 1));
        }

        // ---- cartography table ----
        let carto = (dim, x + 1, y, z);
        world.place(carto, "cartography_table");
        live_map_states().lock().unwrap_or_else(|e| e.into_inner()).insert(
            7,
            MapState {
                map_id: 7,
                scale: 0,
                locked: false,
            },
        );
        assert!(click_block(&client, carto));
        let window = live_menu_id(&client).unwrap();
        {
            let connection = live_menu_connection(&client).unwrap();
            let active = connection.active.lock().unwrap_or_else(|e| e.into_inner());
            assert_eq!(
                active.as_ref().unwrap().menu.screen(),
                (23, "Cartography Table")
            );
        }
        let filled = name_to_item("filled_map").unwrap();
        let mut map_comps = Vec::new();
        comp_set_varint(&mut map_comps, WM_COMP_MAP_ID, 7);

        // Zoom out: map + paper -> fresh map id at scale 1.
        *client.cursor.lock().unwrap_or_else(|e| e.into_inner()) = (filled, 1, map_comps.clone());
        with_save_guard(|| live_menu_click(&client, window, 0, 0, 0));
        *client.cursor.lock().unwrap_or_else(|e| e.into_inner()) = (name_to_item("paper").unwrap(), 1, vec![]);
        with_save_guard(|| live_menu_click(&client, window, 1, 0, 0));
        with_save_guard(|| live_menu_click(&client, window, 2, 0, 0));
        let zoomed = client.cursor.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert_eq!((zoomed.0, zoomed.1), (filled, 1));
        let zoomed_id = comp_varint(&zoomed.2, WM_COMP_MAP_ID).unwrap();
        assert_ne!(zoomed_id, 7);
        assert_eq!(
            live_map_states().lock().unwrap_or_else(|e| e.into_inner()).get(&zoomed_id).copied(),
            Some(MapState {
                map_id: zoomed_id,
                scale: 1,
                locked: false,
            })
        );

        // Clone: the zoomed map (already on the cursor) + an empty map yields
        // two copies sharing the same id.
        with_save_guard(|| live_menu_click(&client, window, 0, 0, 0));
        *client.cursor.lock().unwrap_or_else(|e| e.into_inner()) = (name_to_item("map").unwrap(), 1, vec![]);
        with_save_guard(|| live_menu_click(&client, window, 1, 0, 0));
        with_save_guard(|| live_menu_click(&client, window, 2, 0, 0));
        let cloned = client.cursor.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert_eq!((cloned.0, cloned.1), (filled, 2));
        assert_eq!(comp_varint(&cloned.2, WM_COMP_MAP_ID), Some(zoomed_id));

        // Lock: map + glass pane -> same id, locked flag recorded server-side.
        client
            .inv
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(11, std::mem::take(&mut *client.cursor.lock().unwrap_or_else(|e| e.into_inner())));
        *client.cursor.lock().unwrap_or_else(|e| e.into_inner()) = (filled, 1, map_comps);
        with_save_guard(|| live_menu_click(&client, window, 0, 0, 0));
        *client.cursor.lock().unwrap_or_else(|e| e.into_inner()) = (name_to_item("glass_pane").unwrap(), 2, vec![]);
        with_save_guard(|| live_menu_click(&client, window, 1, 0, 0));
        with_save_guard(|| live_menu_click(&client, window, 2, 0, 0));
        let locked = client.cursor.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert_eq!((locked.0, locked.1), (filled, 1));
        assert_eq!(comp_varint(&locked.2, WM_COMP_MAP_ID), Some(7));
        assert_eq!(
            live_map_states().lock().unwrap_or_else(|e| e.into_inner()).get(&7).copied(),
            Some(MapState {
                map_id: 7,
                scale: 0,
                locked: true,
            })
        );

        // One glass pane remains in the addition slot; closing returns it.
        with_save_guard(|| close_live_menu(&client, true, true));
        let inventory = client.inv.lock().unwrap_or_else(|e| e.into_inner());
        assert!(inventory
            .values()
            .any(|s| item_to_name(s.0) == Some("glass_pane") && s.1 == 1));
        assert!(inventory.values().any(|s| {
            item_to_name(s.0) == Some("filled_map")
                && comp_varint(&s.2, WM_COMP_MAP_ID) == Some(7)
        }));
    }
}
