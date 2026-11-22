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
