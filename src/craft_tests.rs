use super::*;

// The vanilla datapack snapshot in this checkout does not include recipe JSON,
// so these small helpers exercise the same JSON shape with recipe bodies supplied
// by tests. They stay local to the crafting tests until recipes have a runtime
// consumer outside this module.
#[derive(Clone)]
enum SpecialIngredient {
    Item(i64),
    Tag(String),
}

#[derive(Clone)]
enum SpecialRecipe {
    Stonecutting {
        ingredient: SpecialIngredient,
        result: i64,
        count: u8,
    },
    SmithingTransform {
        template: SpecialIngredient,
        base: SpecialIngredient,
        addition: SpecialIngredient,
        result: i64,
    },
    SmithingTrim {
        template: SpecialIngredient,
        base: SpecialIngredient,
        addition: SpecialIngredient,
    },
    DecoratedPot,
}

#[derive(Debug, PartialEq, Eq)]
struct PotCraftResult {
    item: i64,
    count: u8,
    /// Vanilla's PotDecorations order: north, east, south, west.
    pot_decorations: [i64; 4],
}

fn special_ingredient(json: &str) -> Option<SpecialIngredient> {
    if let Some(item) = jstr(json, "item").and_then(name_to_item) {
        Some(SpecialIngredient::Item(item))
    } else {
        jstr(json, "tag").map(|tag| SpecialIngredient::Tag(tag.to_owned()))
    }
}

fn parse_special_recipe(json: &str) -> Option<SpecialRecipe> {
    let result_name = || {
        jblock(json, "result")
            .and_then(|result| jstr(result, "id"))
            .or_else(|| jstr(json, "result"))
            .and_then(name_to_item)
    };
    match jstr(json, "type")?.trim_start_matches("minecraft:") {
        "stonecutting" => Some(SpecialRecipe::Stonecutting {
            ingredient: special_ingredient(jblock(json, "ingredient")?)?,
            result: result_name()?,
            count: jblock(json, "result")
                .and_then(|result| jnum(result, "count"))
                .or_else(|| jnum(json, "count"))
                .unwrap_or(1)
                .clamp(1, 64) as u8,
        }),
        "smithing_transform" => Some(SpecialRecipe::SmithingTransform {
            template: special_ingredient(jblock(json, "template")?)?,
            base: special_ingredient(jblock(json, "base")?)?,
            addition: special_ingredient(jblock(json, "addition")?)?,
            result: result_name()?,
        }),
        "smithing_trim" => Some(SpecialRecipe::SmithingTrim {
            template: special_ingredient(jblock(json, "template")?)?,
            base: special_ingredient(jblock(json, "base")?)?,
            addition: special_ingredient(jblock(json, "addition")?)?,
        }),
        "crafting_decorated_pot" => Some(SpecialRecipe::DecoratedPot),
        _ => None,
    }
}

fn special_ingredient_matches(ingredient: &SpecialIngredient, item: i64) -> bool {
    match ingredient {
        SpecialIngredient::Item(expected) => *expected == item,
        SpecialIngredient::Tag(tag) => {
            if ingr_match(&Ingr::Tag(tag.clone()), item, 0) {
                return true;
            }
            let Some(name) = item_to_name(item) else {
                return false;
            };
            match tag.as_str() {
                // The captured vanilla datapack omits recipes and item tags.
                // These are the tag memberships used by the two smithing
                // recipes below; ordinary tags still resolve through ingr_match.
                "minecraft:diamond_tool_materials" => {
                    tool_parts(name).is_some_and(|(tier, _)| tier == "diamond")
                }
                "minecraft:trimmable_armor" => armor_parts(name).is_some(),
                "minecraft:trim_materials" => [
                    "amethyst_shard",
                    "copper_ingot",
                    "diamond",
                    "emerald",
                    "gold_ingot",
                    "iron_ingot",
                    "lapis_lazuli",
                    "netherite_ingot",
                    "quartz",
                    "redstone",
                ]
                .contains(&name),
                _ => false,
            }
        }
    }
}

fn eval_special_recipe(recipe: &SpecialRecipe, inputs: &[Stack]) -> Option<Stack> {
    match recipe {
        SpecialRecipe::Stonecutting {
            ingredient,
            result,
            count,
        } if inputs.len() == 1
            && inputs[0].1 > 0
            && special_ingredient_matches(ingredient, inputs[0].0) =>
        {
            Some((*result, *count, Vec::new()))
        }
        SpecialRecipe::SmithingTransform {
            template,
            base,
            addition,
            result,
        } if inputs.len() == 3
            && [template, base, addition]
                .iter()
                .zip(inputs)
                .all(|(ingredient, stack)| {
                    stack.1 > 0 && special_ingredient_matches(ingredient, stack.0)
                }) =>
        {
            let mut output = smithing_result(&inputs[0], &inputs[1], &inputs[2])?;
            (output.0 == *result).then_some({
                output.0 = *result;
                output
            })
        }
        SpecialRecipe::SmithingTrim {
            template,
            base,
            addition,
        } if inputs.len() == 3
            && [template, base, addition]
                .iter()
                .zip(inputs)
                .all(|(ingredient, stack)| {
                    stack.1 > 0 && special_ingredient_matches(ingredient, stack.0)
                }) => smithing_result(&inputs[0], &inputs[1], &inputs[2]),
        _ => None,
    }
}

fn eval_decorated_pot(recipe: &SpecialRecipe, ingredients: &[i64; 4]) -> Option<PotCraftResult> {
    if !matches!(recipe, SpecialRecipe::DecoratedPot) {
        return None;
    }
    if !ingredients.iter().all(|item| {
        item_to_name(*item).is_some_and(|name| name == "brick" || name.ends_with("_pottery_sherd"))
    }) {
        return None;
    }
    Some(PotCraftResult {
        item: name_to_item("decorated_pot")?,
        count: 1,
        pot_decorations: *ingredients,
    })
}

// The fixture pack `datapacks/rectest` ships `recipe/gemforge.json`: the shaped
// pattern ["##"] keyed on diamond_block, yielding 4 emeralds. Its bounding box
// is 1x2, so inside a 3x3 grid the pattern can sit at x=0 or x=1 - and vanilla
// slides a shaped pattern over the whole grid (`RecipeMatcher.findMatches`),
// so both placements have to craft. The left-aligned case alone is not enough
// to tell an offset search apart from a search that only ever tries x=0.

fn row_of(slots: &[(i16, i64)]) -> HashMap<i16, Stack> {
    slots
        .iter()
        .map(|(s, it)| (*s, (*it, 1, Comps::new())))
        .collect()
}

/// `load_datapacks` republishes the fixture packs' advancement table process-wide;
/// tests elsewhere pin the unloaded baseline, so keep whatever was registered before
/// the load and restore it on drop.
struct AdvancementRegistrySnapshot(HashMap<String, Advancement>);

impl AdvancementRegistrySnapshot {
    fn take() -> Self {
        Self(std::mem::take(
            &mut *advancements().lock().unwrap_or_else(|e| e.into_inner()),
        ))
    }
}

impl std::ops::Drop for AdvancementRegistrySnapshot {
    fn drop(&mut self) {
        *advancements().lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.0);
    }
}

#[test]
fn a_shaped_recipe_matches_at_a_non_zero_grid_offset() {
    let _guard = crate::world_guard();
    let _advs = AdvancementRegistrySnapshot::take();
    load_datapacks();
    let diamond_block = name_to_item("minecraft:diamond_block").expect("diamond_block");
    let emerald = name_to_item("minecraft:emerald").expect("emerald");
    // y = 0 of a 3x3 grid: the slot for column x is x + 1.
    let top_row = |xs: &[i16]| {
        row_of(
            &xs.iter()
                .map(|x| (x + 1, diamond_block))
                .collect::<Vec<_>>(),
        )
    };

    assert_eq!(
        json_craft_match(&top_row(&[0, 1]), 3).map(|s| (s.0, s.1)),
        Some((emerald, 4)),
        "the left-aligned pattern must craft"
    );
    assert_eq!(
        json_craft_match(&top_row(&[1, 2]), 3).map(|s| (s.0, s.1)),
        Some((emerald, 4)),
        "the same pattern shifted one column right must craft"
    );
}

#[test]
fn vanilla_log_crafting_keeps_each_species_and_four_plank_output() {
    let _guard = crate::world_guard();
    for (log, planks) in [
        ("oak_log", "oak_planks"),
        ("spruce_log", "spruce_planks"),
        ("birch_log", "birch_planks"),
        ("jungle_log", "jungle_planks"),
        ("acacia_log", "acacia_planks"),
        ("dark_oak_log", "dark_oak_planks"),
        ("mangrove_log", "mangrove_planks"),
        ("cherry_log", "cherry_planks"),
        ("crimson_stem", "crimson_planks"),
        ("warped_stem", "warped_planks"),
    ] {
        let grid = HashMap::from([(1, (name_to_item(log).unwrap(), 1, Vec::new()))]);
        assert_eq!(
            craft_result(&grid),
            Some((name_to_item(planks).unwrap(), 4, Vec::new())),
            "one {log} crafts four matching {planks}"
        );
    }
}

#[test]
fn crafting_consumption_returns_bucket_remainders_and_preserves_stacked_inputs() {
    let _guard = crate::world_guard();
    let milk_bucket = name_to_item("milk_bucket").unwrap();
    let bucket = name_to_item("bucket").unwrap();
    let mut single = HashMap::from([(1, (milk_bucket, 1, Vec::new()))]);

    let overflow = consume_crafting_grid(&mut single, 1..=4);

    assert!(overflow.is_empty());
    assert_eq!(single.get(&1), Some(&(bucket, 1, Vec::new())));

    let mut stacked = HashMap::from([(1, (milk_bucket, 2, Vec::new()))]);
    let overflow = consume_crafting_grid(&mut stacked, 1..=4);

    assert_eq!(stacked.get(&1), Some(&(milk_bucket, 1, Vec::new())));
    assert_eq!(overflow, vec![(bucket, 1, Vec::new())]);
}

#[test]
fn crafting_shift_click_moves_each_available_result_and_consumes_inputs() {
    let _guard = crate::world_guard();
    let log = name_to_item("oak_log").unwrap();
    let planks = name_to_item("oak_planks").unwrap();
    let mut grid = HashMap::from([(1, (log, 2, Vec::new()))]);
    let mut inv = HashMap::new();

    let overflow = quick_move_crafting_result(&mut inv, &mut grid, 1..=4, craft_result);

    assert!(overflow.is_empty());
    assert_eq!(inv.get(&9), Some(&(planks, 8, Vec::new())));
    assert!(grid.is_empty());
}

#[test]
fn inventory_menu_button_40_swaps_with_offhand() {
    assert_eq!(inventory_swap_slot(0), Some(36));
    assert_eq!(inventory_swap_slot(8), Some(44));
    assert_eq!(inventory_swap_slot(-1), None);
    let offhand = inventory_swap_slot(40).unwrap();
    assert_eq!(offhand, 45);

    let stone = (name_to_item("stone").unwrap(), 4, Vec::new());
    let shield = (name_to_item("shield").unwrap(), 1, Vec::new());
    let mut inv = HashMap::from([(36, stone.clone()), (45, shield.clone())]);
    swap_inventory_slots(&mut inv, 36, offhand);
    assert_eq!(inv.get(&36), Some(&shield));
    assert_eq!(inv.get(&45), Some(&stone));

    swap_inventory_slots(&mut inv, 45, offhand);
    assert_eq!(inv.get(&45), Some(&stone));
    swap_inventory_slots(&mut inv, 9, offhand);
    assert_eq!(inv.get(&9), Some(&stone));
    assert!(inv.get(&45).is_none());
}

#[test]
fn furnace_output_number_key_preserves_occupied_hotbar() {
    let _guard = world_guard();
    let (me, _peer) = crate::effect_tests::client_pair();
    let me = Arc::new(me);
    let pos = ow_block((1000, 70, 1000));
    *me.open_furnace.lock().unwrap_or_else(|e| e.into_inner()) = Some(pos);
    let held = (name_to_item("stone").unwrap(), 4, Vec::new());
    let ingot = (name_to_item("iron_ingot").unwrap(), 1, Vec::new());
    me.inv.lock().unwrap_or_else(|e| e.into_inner()).insert(36, held.clone());

    play_c2s_window_click_mode2(&me, 2, 0);
    assert_eq!(me.inv.lock().unwrap_or_else(|e| e.into_inner()).get(&36), Some(&held));
    furnaces().lock().unwrap_or_else(|e| e.into_inner()).get_mut(&pos).unwrap().output = ingot.clone();
    play_c2s_window_click_mode2(&me, 2, 0);
    assert_eq!(me.inv.lock().unwrap_or_else(|e| e.into_inner()).get(&36), Some(&held));
    assert_eq!(furnace_slot_snapshot_dim(pos.0, (pos.1, pos.2, pos.3))[2], ingot);

    play_c2s_window_click_mode2(&me, 2, 1);
    assert_eq!(me.inv.lock().unwrap_or_else(|e| e.into_inner()).get(&37), Some(&ingot));
    assert_eq!(furnace_slot_snapshot_dim(pos.0, (pos.1, pos.2, pos.3))[2], Stack::default());
    furnaces().lock().unwrap_or_else(|e| e.into_inner()).remove(&pos);
    remove_furnace_at(pos.0, pos.1, pos.2, pos.3);
}

#[test]
fn crafting_table_clicks_address_table_grid_and_player_slots() {
    let _guard = world_guard();
    let (me, _peer) = crate::effect_tests::client_pair();
    let me = Arc::new(me);
    *me.open_table.lock().unwrap_or_else(|e| e.into_inner()) = true;
    let stone = (name_to_item("stone").unwrap(), 2, Vec::new());
    let dirt = (name_to_item("dirt").unwrap(), 3, Vec::new());
    me.inv.lock().unwrap_or_else(|e| e.into_inner()).insert(44, stone.clone());
    me.inv.lock().unwrap_or_else(|e| e.into_inner()).insert(45, dirt.clone());

    play_c2s_crafting_table_click(&me, 45, 40, 2);
    assert_eq!(me.inv.lock().unwrap_or_else(|e| e.into_inner()).get(&44), Some(&dirt));
    assert_eq!(me.inv.lock().unwrap_or_else(|e| e.into_inner()).get(&45), Some(&stone));
    play_c2s_crafting_table_click(&me, 8, 40, 2);
    assert_eq!(me.craft9.lock().unwrap_or_else(|e| e.into_inner()).get(&8), Some(&stone));
    assert!(me.craft.lock().unwrap_or_else(|e| e.into_inner()).is_empty());

    play_c2s_crafting_table_click(&me, 8, 0, 1);
    assert!(me.craft9.lock().unwrap_or_else(|e| e.into_inner()).is_empty());
    let before = drops().lock().unwrap_or_else(|e| e.into_inner()).len();
    play_c2s_crafting_table_click(&me, 10, 0, 4);
    assert_eq!(me.inv.lock().unwrap_or_else(|e| e.into_inner()).get(&9).map(|s| s.1), Some(1));
    let mut drops = drops().lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(drops.len(), before + 1);
    assert_eq!(drops.pop().unwrap().item, stone.0);
    drop(drops);

    let mut carried = stone.clone();
    paint_slot(&me, 9, 1, &mut carried);
    assert_eq!(me.craft9.lock().unwrap_or_else(|e| e.into_inner()).get(&9).map(|s| s.1), Some(1));
    assert_eq!(me.inv.lock().unwrap_or_else(|e| e.into_inner()).get(&9).map(|s| s.1), Some(1));
}

#[test]
fn container_close_overflow_preserves_items_when_inventory_is_full() {
    let stone = name_to_item("stone").unwrap();
    let dirt = name_to_item("dirt").unwrap();
    let mut inv = (9..=44)
        .map(|slot| (slot, (stone, 64, vec![])))
        .collect::<HashMap<_, _>>();
    let mut grid = HashMap::from([(1, (stone, 3, Vec::new())), (2, (dirt, 2, Vec::new()))]);

    let overflow = return_crafting_grid_to_inventory(&mut inv, &mut grid);

    assert!(grid.is_empty());
    assert_eq!(inv.len(), 36);
    assert_eq!(
        inv.values()
            .filter(|stack| stack.0 == stone)
            .map(|stack| stack.1 as u16)
            .sum::<u16>(),
        36 * 64
    );
    assert_eq!(
        overflow
            .iter()
            .filter(|stack| stack.0 == stone)
            .map(|stack| stack.1 as u16)
            .sum::<u16>(),
        3
    );
    assert_eq!(
        overflow
            .iter()
            .filter(|stack| stack.0 == dirt)
            .map(|stack| stack.1 as u16)
            .sum::<u16>(),
        2
    );
}

#[test]
fn vanilla_stonecutting_json_parses_ingredient_and_result_count() {
    let _guard = crate::world_guard();
    let _advs = AdvancementRegistrySnapshot::take();
    load_datapacks();
    let recipe = parse_special_recipe(
        r#"{"type":"minecraft:stonecutting","ingredient":{"item":"minecraft:andesite"},"result":{"id":"minecraft:polished_andesite","count":2}}"#,
    )
    .unwrap();
    assert_eq!(
        eval_special_recipe(&recipe, &[(name_to_item("andesite").unwrap(), 1, Vec::new())]),
        Some((name_to_item("polished_andesite").unwrap(), 2, Vec::new()))
    );
    assert_eq!(
        eval_special_recipe(&recipe, &[(name_to_item("diorite").unwrap(), 1, Vec::new())]),
        None
    );
}

#[test]
fn vanilla_smithing_transform_json_preserves_base_components() {
    let _guard = crate::world_guard();
    let _advs = AdvancementRegistrySnapshot::take();
    load_datapacks();
    let recipe = parse_special_recipe(
        r##"{"type":"minecraft:smithing_transform","template":{"item":"minecraft:netherite_upgrade_smithing_template"},"base":{"tag":"minecraft:diamond_tool_materials"},"addition":{"item":"minecraft:netherite_ingot"},"result":{"id":"minecraft:netherite_sword"}}"##,
    )
    .unwrap();
    let mut base = (name_to_item("diamond_sword").unwrap(), 1, Vec::new());
    comp_set_varint(&mut base.2, COMP_DAMAGE, 37);
    comp_set_varint(&mut base.2, COMP_MAX_DAMAGE, 1561);
    assert_eq!(
        eval_special_recipe(
            &recipe,
            &[
                (name_to_item("netherite_upgrade_smithing_template").unwrap(), 1, Vec::new()),
                base.clone(),
                (name_to_item("netherite_ingot").unwrap(), 1, Vec::new()),
            ]
        ),
        Some((name_to_item("netherite_sword").unwrap(), 1, base.2))
    );
}

#[test]
fn vanilla_smithing_trim_json_applies_the_trim_component_to_armor() {
    let _guard = crate::world_guard();
    let _advs = AdvancementRegistrySnapshot::take();
    load_datapacks();
    let recipe = parse_special_recipe(
        r##"{"type":"minecraft:smithing_trim","template":{"item":"minecraft:bolt_armor_trim_smithing_template"},"base":{"tag":"minecraft:trimmable_armor"},"addition":{"tag":"minecraft:trim_materials"}}"##,
    )
    .unwrap();
    let armor = (name_to_item("iron_chestplate").unwrap(), 1, Vec::new());
    let result = eval_special_recipe(
        &recipe,
        &[
            (name_to_item("bolt_armor_trim_smithing_template").unwrap(), 1, Vec::new()),
            armor.clone(),
            (name_to_item("amethyst_shard").unwrap(), 1, Vec::new()),
        ],
    )
    .unwrap();
    assert_eq!(result.0, armor.0);
    assert_eq!(comp_get(&result.2, COMP_ARMOR_TRIM), Some([1, 1, 1].as_slice()));
}

#[test]
fn vanilla_decorated_pot_keeps_north_east_south_west_decoration_order() {
    let _guard = crate::world_guard();
    let _advs = AdvancementRegistrySnapshot::take();
    load_datapacks();
    let recipe = parse_special_recipe(
        r#"{"type":"minecraft:crafting_decorated_pot","category":"misc"}"#,
    )
    .unwrap();
    let north_east_south_west = [
        name_to_item("angler_pottery_sherd").unwrap(),
        name_to_item("archer_pottery_sherd").unwrap(),
        name_to_item("arms_up_pottery_sherd").unwrap(),
        name_to_item("blade_pottery_sherd").unwrap(),
    ];
    assert_eq!(
        eval_decorated_pot(&recipe, &north_east_south_west),
        Some(PotCraftResult {
            item: name_to_item("decorated_pot").unwrap(),
            count: 1,
            pot_decorations: north_east_south_west,
        })
    );
    let invalid = [
        north_east_south_west[0],
        north_east_south_west[1],
        name_to_item("stone").unwrap(),
        north_east_south_west[3],
    ];
    assert_eq!(eval_decorated_pot(&recipe, &invalid), None);
}
