// SPDX-License-Identifier: LGPL-3.0-or-later
//! Tests for `megu_client_ui::recipe_search`: the kana/romaji
//! normalizer, the inverted `RecipeSearchIndex` (`@mod` / `#tag` /
//! plain text, `R`/`U` lookups, Polymorph-style conflict groups) and
//! the `NearbyChestCraftPlanner` 1-click-craft transfer plans —
//! including a 5,000+ recipe index that must answer queries in under a
//! millisecond.

use std::time::Instant;

use megu_client_ui::recipe_search::{
    kana_to_romaji, normalize_kana, query_variants, CraftAvailability, IngredientReq, NearbyChest,
    NearbyChestCraftPlanner, PullSource, RecipeSearchIndex, SearchItem, SearchRecipe, SlotStack,
};

// ---------------------------------------------------------------------------
// Japanese normalization
// ---------------------------------------------------------------------------

#[test]
fn normalizes_katakana_and_halfwidth() {
    assert_eq!(normalize_kana("ダイヤモンド"), "だいやもんど");
    assert_eq!(normalize_kana("だいやもんど"), "だいやもんど");
    // Half-width katakana, including dakuten composition (ﾀﾞ → だ).
    assert_eq!(normalize_kana("ﾀﾞｲﾔﾓﾝﾄﾞ"), "だいやもんど");
    assert_eq!(normalize_kana("ｶﾀｶﾅ"), "かたかな");
    assert_eq!(normalize_kana("ﾊﾟ"), "ぱ");
    assert_eq!(normalize_kana("ｶﾞｸｾｲ"), "がくせ"); // せい → せ (long-え)
}

#[test]
fn normalizes_fullwidth_ascii() {
    assert_eq!(normalize_kana("ＤＩＡＭＯＮＤ"), "diamond");
    assert_eq!(normalize_kana("ｄｉａｍｏｎｄ"), "diamond");
    assert_eq!(normalize_kana("　"), " ");
}

#[test]
fn drops_prolonged_sound_mark() {
    assert_eq!(normalize_kana("ラーメン"), "らめん");
    assert_eq!(normalize_kana("コーヒー"), "こひ");
    assert_eq!(normalize_kana("ケーキ"), "けき");
    assert_eq!(normalize_kana("らーめん"), "らめん");
}

#[test]
fn collapses_long_vowel_spellings() {
    assert_eq!(normalize_kana("とうきょう"), "ときょ");
    assert_eq!(normalize_kana("おおきい"), "おき");
    assert_eq!(normalize_kana("がくせい"), "がくせ");
    assert_eq!(normalize_kana("せんせい"), "せんせ");
    // ん・っ break runs and are preserved.
    assert_eq!(normalize_kana("がっこう"), "がっこ");
    assert_eq!(normalize_kana("こんにちは"), "こんにちは");
}

#[test]
fn romanizes_kana() {
    assert_eq!(kana_to_romaji("だいやもんど"), "daiyamondo");
    assert_eq!(kana_to_romaji("しゃしん"), "shashin");
    assert_eq!(kana_to_romaji("きょうしつ"), "kyoushitsu");
    assert_eq!(kana_to_romaji("しんかんせん"), "shinkansen");
    assert_eq!(kana_to_romaji("りょ"), "ryo");
    assert_eq!(kana_to_romaji("ちゃ"), "cha");
    assert_eq!(kana_to_romaji("ふぁ"), "fa");
    assert_eq!(kana_to_romaji("うぉ"), "wo");
    assert_eq!(kana_to_romaji("ゔぇ"), "ve");
    assert_eq!(kana_to_romaji("つぉ"), "tso");
    // Sokuon geminates the next consonant.
    assert_eq!(kana_to_romaji("ちょっと"), "chotto");
    assert_eq!(kana_to_romaji("がっこ"), "gakko");
    assert_eq!(kana_to_romaji("けっか"), "kekka");
    // Non-kana passes through.
    assert_eq!(kana_to_romaji("歯車"), "歯車");
    assert_eq!(kana_to_romaji("歯車mk2"), "歯車mk2");
}

#[test]
fn explodes_query_variants() {
    let v = query_variants("daia");
    assert!(v.contains(&"daia".to_string()), "{v:?}");
    assert!(v.contains(&"だいあ".to_string()), "{v:?}");

    let v = query_variants("ダイア");
    assert!(v.contains(&"だいあ".to_string()), "{v:?}");
    assert!(v.contains(&"daia".to_string()), "{v:?}");

    let v = query_variants("diamond");
    assert_eq!(v[0], "diamond");

    // Kanji stays kanji.
    assert_eq!(query_variants("歯車"), vec!["歯車".to_string()]);
    assert!(query_variants("").is_empty());
}

// ---------------------------------------------------------------------------
// RecipeSearchIndex
// ---------------------------------------------------------------------------

fn fixture_index() -> RecipeSearchIndex {
    let mut idx = RecipeSearchIndex::new();
    let mut diamond = SearchItem::new("minecraft:diamond", "Diamond", "ダイヤモンド");
    diamond.tags = vec!["minecraft:beacon_payment_items".into()];
    idx.add_item(diamond);
    let mut oak = SearchItem::new("minecraft:oak_planks", "Oak Planks", "オークの板材");
    oak.tags = vec!["minecraft:planks".into()];
    idx.add_item(oak);
    let mut birch = SearchItem::new("minecraft:birch_planks", "Birch Planks", "シラカバの板材");
    birch.tags = vec!["minecraft:planks".into()];
    idx.add_item(birch);
    let mut cog = SearchItem::new("create:cogwheel", "Cogwheel", "歯車");
    cog.tags = vec!["create:cog".into()];
    idx.add_item(cog);
    idx.add_item(SearchItem::new(
        "create:large_cogwheel",
        "Large Cogwheel",
        "大きな歯車",
    ));
    idx.add_item(SearchItem::new("minecraft:stick", "Stick", "棒"));
    idx.add_item(SearchItem::new(
        "minecraft:iron_ingot",
        "Iron Ingot",
        "鉄インゴット",
    ));
    idx.add_item(SearchItem::new(
        "mekanism:steel_ingot",
        "Steel Ingot",
        "スチールインゴット",
    ));

    let mut sticks = SearchRecipe::new("minecraft:stick", "minecraft:stick", 4);
    sticks.ingredients = vec![IngredientReq::tag("minecraft:planks", 2)];
    idx.add_recipe(sticks);

    let mut chest_oak = SearchRecipe::new("minecraft:chest_from_oak", "minecraft:chest", 1);
    chest_oak.ingredients = vec![IngredientReq::item("minecraft:oak_planks", 8)];
    idx.add_recipe(chest_oak);

    let mut chest_birch = SearchRecipe::new("minecraft:chest_from_birch", "minecraft:chest", 1);
    chest_birch.ingredients = vec![IngredientReq::item("minecraft:birch_planks", 8)];
    idx.add_recipe(chest_birch);

    let mut cog = SearchRecipe::new("create:cogwheel", "create:cogwheel", 8);
    cog.ingredients = vec![
        IngredientReq::tag("minecraft:planks", 2),
        IngredientReq::item("create:shaft", 1),
    ];
    idx.add_recipe(cog);
    idx
}

fn ids<'a>(items: &[&'a SearchItem]) -> Vec<&'a str> {
    items.iter().map(|i| i.id.as_str()).collect()
}

#[test]
fn searches_english_text() {
    let idx = fixture_index();
    assert_eq!(ids(&idx.search("diamond")), vec!["minecraft:diamond"]);
    let hits = ids(&idx.search("ingot"));
    assert!(hits.contains(&"minecraft:iron_ingot"));
    assert!(hits.contains(&"mekanism:steel_ingot"));
}

#[test]
fn searches_romaji_kana_equivalents() {
    let idx = fixture_index();
    for q in ["daia", "だいあ", "ダイア", "ダイヤ", "だいや", "daiya"] {
        let hits = ids(&idx.search(q));
        assert!(
            hits.contains(&"minecraft:diamond"),
            "query {q:?} should hit diamond, got {hits:?}"
        );
    }
    // Steel Ingot / スチールインゴット reachable through romaji forms.
    for q in ["suchiru", "スチル", "すちる", "steel"] {
        let hits = ids(&idx.search(q));
        assert!(
            hits.contains(&"mekanism:steel_ingot"),
            "query {q:?} should hit steel, got {hits:?}"
        );
    }
}

#[test]
fn searches_mod_namespace_filter() {
    let idx = fixture_index();
    let hits = ids(&idx.search("@create"));
    assert_eq!(hits.len(), 2);
    assert!(hits.contains(&"create:cogwheel"));
    assert!(hits.contains(&"create:large_cogwheel"));

    assert_eq!(
        ids(&idx.search("@create 歯車")),
        vec!["create:cogwheel", "create:large_cogwheel"]
    );
    // "cogwheel" is also a substring of the "large_cogwheel" id.
    assert_eq!(
        ids(&idx.search("@create cogwheel")),
        vec!["create:cogwheel", "create:large_cogwheel"]
    );
    assert_eq!(ids(&idx.search("@mekanism")), vec!["mekanism:steel_ingot"]);
    assert!(ids(&idx.search("@no_such_mod")).is_empty());
}

#[test]
fn searches_tag_filter() {
    let idx = fixture_index();
    let mut hits = ids(&idx.search("#planks"));
    hits.sort();
    assert_eq!(hits, vec!["minecraft:birch_planks", "minecraft:oak_planks"]);

    assert_eq!(
        ids(&idx.search("#planks oak")),
        vec!["minecraft:oak_planks"]
    );
    assert_eq!(
        ids(&idx.search("#planks シラカバ")),
        vec!["minecraft:birch_planks"]
    );
    assert_eq!(ids(&idx.search("#planks @create")), Vec::<&str>::new());
}

#[test]
fn combined_tokens_are_and_ed() {
    let idx = fixture_index();
    assert_eq!(
        ids(&idx.search("@minecraft planks oak")),
        vec!["minecraft:oak_planks"]
    );
    // Bare prefix tokens are ignored rather than nuking the result set.
    assert!(!idx.search("@ # diamond").is_empty());
    // Empty query returns the whole catalog in order.
    assert_eq!(idx.search("").len(), idx.items().len());
    assert_eq!(idx.search("   ").len(), idx.items().len());
}

#[test]
fn search_is_deterministic() {
    let idx = fixture_index();
    let a = ids(&idx.search("in"));
    let b = ids(&idx.search("in"));
    assert_eq!(a, b);
}

#[test]
fn recipes_producing_and_using_are_inverted() {
    let idx = fixture_index();
    let producing: Vec<&str> = idx
        .recipes_producing("minecraft:chest")
        .iter()
        .map(|r| r.id.as_str())
        .collect();
    assert_eq!(
        producing,
        vec!["minecraft:chest_from_oak", "minecraft:chest_from_birch"]
    );

    let using: Vec<&str> = idx
        .recipes_using("minecraft:oak_planks")
        .iter()
        .map(|r| r.id.as_str())
        .collect();
    // The planks-tag stick recipe and the cogwheel recipe resolve oak
    // planks through the tag; the oak chest recipe is a direct hit.
    assert!(using.contains(&"minecraft:stick"));
    assert!(using.contains(&"minecraft:chest_from_oak"));
    assert!(using.contains(&"create:cogwheel"));
    assert!(!using.contains(&"minecraft:chest_from_birch"));

    assert!(idx.recipes_producing("minecraft:nothing").is_empty());
    assert!(idx.recipes_using("minecraft:nothing").is_empty());
}

#[test]
fn groups_conflicting_outputs_polymorph_style() {
    let idx = fixture_index();
    let group = idx.conflict_group("minecraft:chest").unwrap();
    assert_eq!(group.output_item, "minecraft:chest");
    assert_eq!(
        group.recipe_ids,
        vec!["minecraft:chest_from_birch", "minecraft:chest_from_oak"]
    );
    assert!(idx.conflict_group("minecraft:stick").is_none());
    assert!(idx.conflict_group("minecraft:nothing").is_none());
    assert_eq!(idx.conflict_groups().len(), 1);
}

#[test]
fn rebuild_restores_derived_indexes() {
    // Recipes loaded before items resolve tags only after `rebuild`.
    let mut idx = RecipeSearchIndex::new();
    let mut sticks = SearchRecipe::new("minecraft:stick", "minecraft:stick", 4);
    sticks.ingredients = vec![IngredientReq::tag("minecraft:planks", 2)];
    idx.add_recipe(sticks);
    assert!(idx.recipes_using("minecraft:oak_planks").is_empty());

    let mut oak = SearchItem::new("minecraft:oak_planks", "Oak Planks", "オークの板材");
    oak.tags = vec!["minecraft:planks".into()];
    idx.add_item(oak);
    idx.rebuild();
    assert_eq!(idx.recipes_using("minecraft:oak_planks").len(), 1);
}

// ---------------------------------------------------------------------------
// NearbyChestCraftPlanner
// ---------------------------------------------------------------------------

fn slots(stacks: &[(&str, u32)]) -> Vec<Option<SlotStack>> {
    stacks
        .iter()
        .map(|(id, n)| Some(SlotStack::new(id, *n)))
        .collect()
}

#[test]
fn plans_craft_from_inventory_only() {
    let idx = fixture_index();
    let planner = NearbyChestCraftPlanner::new();
    let ingredients = vec![
        IngredientReq::item("minecraft:oak_planks", 2),
        IngredientReq::item("minecraft:stick", 1),
    ];
    let inv = slots(&[("minecraft:oak_planks", 8), ("minecraft:stick", 3)]);
    let result = planner.plan(&idx, &ingredients, &inv, &[], "minecraft:ladder", 3, 1);
    assert_eq!(
        result.availability,
        CraftAvailability::CraftableFromInventory
    );
    let plan = result.plan.unwrap();
    assert_eq!(plan.craft_count, 1);
    assert_eq!(plan.output_item, "minecraft:ladder");
    assert_eq!(plan.output_count, 3);
    assert_eq!(plan.total_output(), 3);
    assert_eq!(plan.pulls.len(), 2);
    assert!(plan
        .pulls
        .iter()
        .all(|p| p.source == PullSource::PlayerInventory));
    assert_eq!(plan.pulled("minecraft:oak_planks"), 2);
    assert_eq!(plan.pulled("minecraft:stick"), 1);
}

#[test]
fn plans_craft_pulling_from_nearest_chests() {
    let idx = fixture_index();
    let planner = NearbyChestCraftPlanner::new();
    let ingredients = vec![IngredientReq::item("minecraft:oak_planks", 8)];
    // Inventory has 5 of 8; the rest comes from chests. The input is
    // deliberately unsorted — the planner must order by distance and
    // keep reporting original indices.
    let inv = slots(&[("minecraft:oak_planks", 5)]);
    let far = NearbyChest::new(5.5, slots(&[("minecraft:oak_planks", 10)]));
    let near = NearbyChest::new(1.0, slots(&[("minecraft:oak_planks", 2)]));
    let chests = vec![far, near];

    let result = planner.plan(&idx, &ingredients, &inv, &chests, "minecraft:chest", 1, 1);
    assert_eq!(result.availability, CraftAvailability::CraftableWithChests);
    let plan = result.plan.unwrap();
    assert_eq!(
        plan.pulls,
        vec![
            megu_client_ui::recipe_search::SlotPull {
                source: PullSource::PlayerInventory,
                slot: 0,
                item_id: "minecraft:oak_planks".into(),
                count: 5,
            },
            megu_client_ui::recipe_search::SlotPull {
                source: PullSource::Chest(1),
                slot: 0,
                item_id: "minecraft:oak_planks".into(),
                count: 2,
            },
            megu_client_ui::recipe_search::SlotPull {
                source: PullSource::Chest(0),
                slot: 0,
                item_id: "minecraft:oak_planks".into(),
                count: 1,
            },
        ]
    );
}

#[test]
fn reports_missing_ingredients() {
    let idx = fixture_index();
    let planner = NearbyChestCraftPlanner::new();
    let ingredients = vec![
        IngredientReq::item("minecraft:oak_planks", 8),
        IngredientReq::item("minecraft:diamond", 2),
    ];
    let inv = slots(&[("minecraft:oak_planks", 5)]);
    let chests = vec![NearbyChest::new(1.0, slots(&[("minecraft:oak_planks", 1)]))];
    let result = planner.plan(&idx, &ingredients, &inv, &chests, "x:y", 1, 1);
    match result.availability {
        CraftAvailability::MissingIngredients(missing) => {
            assert_eq!(
                missing,
                vec![
                    ("minecraft:oak_planks".to_string(), 2),
                    ("minecraft:diamond".to_string(), 2),
                ]
            );
        }
        other => panic!("expected MissingIngredients, got {other:?}"),
    }
    assert!(result.plan.is_none());
}

#[test]
fn ignores_out_of_reach_chests() {
    let idx = fixture_index();
    let planner = NearbyChestCraftPlanner::new();
    let ingredients = vec![IngredientReq::item("minecraft:oak_planks", 8)];
    let inv = slots(&[("minecraft:oak_planks", 5)]);
    let chests = vec![
        NearbyChest::new(6.1, slots(&[("minecraft:oak_planks", 64)])),
        NearbyChest::new(f32::NAN, slots(&[("minecraft:oak_planks", 64)])),
    ];
    let result = planner.plan(&idx, &ingredients, &inv, &chests, "x:y", 1, 1);
    assert!(matches!(
        result.availability,
        CraftAvailability::MissingIngredients(_)
    ));
    assert!(result.plan.is_none());

    // Extending the reach makes the same plan craftable.
    let planner = NearbyChestCraftPlanner::with_reach(7.0);
    let result = planner.plan(&idx, &ingredients, &inv, &chests, "x:y", 1, 1);
    assert_eq!(result.availability, CraftAvailability::CraftableWithChests);
}

#[test]
fn never_pulls_more_than_a_slot_holds() {
    let idx = fixture_index();
    let planner = NearbyChestCraftPlanner::new();
    let ingredients = vec![IngredientReq::item("minecraft:oak_planks", 4)];
    let inv = slots(&[("minecraft:oak_planks", 3), ("minecraft:oak_planks", 3)]);
    let result = planner.plan(&idx, &ingredients, &inv, &[], "x:y", 1, 1);
    let plan = result.plan.unwrap();
    assert_eq!(plan.pulls[0].count, 3);
    assert_eq!(plan.pulls[1].count, 1);
    assert_eq!(plan.pulled("minecraft:oak_planks"), 4);
}

#[test]
fn resolves_tag_ingredients_to_best_stocked_member() {
    let idx = fixture_index();
    let planner = NearbyChestCraftPlanner::new();
    let ingredients = vec![IngredientReq::tag("minecraft:planks", 4)];
    // Oak is lexicographically smaller but birch has more on hand.
    let inv = slots(&[("minecraft:oak_planks", 2), ("minecraft:birch_planks", 64)]);
    let result = planner.plan(&idx, &ingredients, &inv, &[], "x:y", 1, 1);
    let plan = result.plan.unwrap();
    assert_eq!(plan.pulls.len(), 1);
    assert_eq!(plan.pulls[0].item_id, "minecraft:birch_planks");
    assert_eq!(plan.pulls[0].count, 4);

    // Tie on availability → the lexicographically smaller member wins.
    let inv = slots(&[("minecraft:oak_planks", 4), ("minecraft:birch_planks", 4)]);
    let result = planner.plan(&idx, &ingredients, &inv, &[], "x:y", 1, 1);
    let plan = result.plan.unwrap();
    assert_eq!(plan.pulls[0].item_id, "minecraft:birch_planks");

    // Unknown tag → reported as a #tag shortfall.
    let ingredients = vec![IngredientReq::tag("fake:no_such_tag", 2)];
    let result = planner.plan(&idx, &ingredients, &inv, &[], "x:y", 1, 1);
    match result.availability {
        CraftAvailability::MissingIngredients(missing) => {
            assert_eq!(missing, vec![("#fake:no_such_tag".to_string(), 2)]);
        }
        other => panic!("expected MissingIngredients, got {other:?}"),
    }
}

#[test]
fn plans_max_shift_craft_capped_by_stack() {
    let idx = fixture_index();
    let planner = NearbyChestCraftPlanner::new();
    let ingredients = vec![IngredientReq::item("minecraft:oak_planks", 2)];
    let inv = slots(&[("minecraft:oak_planks", 20)]);

    // 10 crafts of material, but 4-per-craft into a stack of 16 caps at 4.
    let result = planner.plan_max(&idx, &ingredients, &inv, &[], "x:y", 4, 16);
    let plan = result.plan.unwrap();
    assert_eq!(plan.craft_count, 4);
    assert_eq!(plan.pulled("minecraft:oak_planks"), 8);
    assert_eq!(plan.total_output(), 16);

    // Bigger stacks: materials become the limit (10 crafts).
    let result = planner.plan_max(&idx, &ingredients, &inv, &[], "x:y", 4, 64);
    assert_eq!(result.plan.unwrap().craft_count, 10);

    // Chest material joins the pool.
    let inv = slots(&[("minecraft:oak_planks", 4)]);
    let chests = vec![NearbyChest::new(
        2.0,
        slots(&[("minecraft:oak_planks", 10)]),
    )];
    let result = planner.plan_max(&idx, &ingredients, &inv, &chests, "x:y", 4, 64);
    assert_eq!(result.availability, CraftAvailability::CraftableWithChests);
    assert_eq!(result.plan.unwrap().craft_count, 7);

    // Nothing available at all → missing for a single craft.
    let result = planner.plan_max(&idx, &ingredients, &[], &[], "x:y", 4, 64);
    assert!(matches!(
        result.availability,
        CraftAvailability::MissingIngredients(_)
    ));
    assert!(result.plan.is_none());
}

#[test]
fn aggregates_duplicate_ingredients() {
    let idx = fixture_index();
    let planner = NearbyChestCraftPlanner::new();
    // Two slots wanting the same item sum into one requirement.
    let ingredients = vec![
        IngredientReq::item("minecraft:oak_planks", 2),
        IngredientReq::item("minecraft:oak_planks", 3),
    ];
    let inv = slots(&[("minecraft:oak_planks", 4)]);
    let result = planner.plan(&idx, &ingredients, &inv, &[], "x:y", 1, 1);
    match result.availability {
        CraftAvailability::MissingIngredients(missing) => {
            assert_eq!(missing, vec![("minecraft:oak_planks".to_string(), 1)]);
        }
        other => panic!("expected MissingIngredients, got {other:?}"),
    }
    let inv = slots(&[("minecraft:oak_planks", 5)]);
    let result = planner.plan(&idx, &ingredients, &inv, &[], "x:y", 1, 1);
    assert_eq!(
        result.availability,
        CraftAvailability::CraftableFromInventory
    );
}

#[test]
fn empty_recipe_is_always_craftable() {
    let idx = fixture_index();
    let planner = NearbyChestCraftPlanner::new();
    let result = planner.plan(&idx, &[], &[], &[], "minecraft:air", 1, 1);
    assert_eq!(
        result.availability,
        CraftAvailability::CraftableFromInventory
    );
    let plan = result.plan.unwrap();
    assert!(plan.pulls.is_empty());
    assert_eq!(plan.craft_count, 1);
}

#[test]
fn plans_are_deterministic() {
    let idx = fixture_index();
    let planner = NearbyChestCraftPlanner::new();
    let ingredients = vec![
        IngredientReq::item("minecraft:oak_planks", 6),
        IngredientReq::item("minecraft:stick", 2),
    ];
    let inv = slots(&[("minecraft:oak_planks", 3), ("minecraft:stick", 2)]);
    let chests = vec![
        NearbyChest::new(4.0, slots(&[("minecraft:oak_planks", 1)])),
        NearbyChest::new(1.0, slots(&[("minecraft:oak_planks", 2)])),
    ];
    let a = planner.plan(&idx, &ingredients, &inv, &chests, "x:y", 1, 1);
    let b = planner.plan(&idx, &ingredients, &inv, &chests, "x:y", 1, 1);
    assert_eq!(a, b);
}

// ---------------------------------------------------------------------------
// 5,000+ recipe latency budget
// ---------------------------------------------------------------------------

fn big_index(items: usize, recipes: usize) -> RecipeSearchIndex {
    let mods = ["minecraft", "create", "mekanism", "thermal", "botania"];
    let tags = [
        "minecraft:planks",
        "minecraft:logs",
        "c:ingots",
        "c:gems",
        "create:cog",
        "forge:dusts",
    ];
    // JA display names rotate across families so kana/romaji queries
    // hit large-but-not-total subsets, like a real modded catalog.
    let ja_names = [
        "かがくアイテム",
        "ダイヤブロック",
        "てっこうせきパーツ",
        "こうぎょうマシン",
        "ねこみみチャーム",
    ];
    let mut idx = RecipeSearchIndex::new();
    for i in 0..items {
        let m = mods[i % mods.len()];
        let mut item = SearchItem::new(
            &format!("{m}:item_{i}"),
            &format!("Widget {i} Sprocket"),
            &format!("{}{i}", ja_names[i % ja_names.len()]),
        );
        item.tags = vec![tags[i % tags.len()].to_string()];
        idx.add_item(item);
    }
    for i in 0..recipes {
        let out = format!("minecraft:item_{}", i % items);
        let mut r = SearchRecipe::new(&format!("recipe:r{i}"), &out, 1);
        r.ingredients = vec![
            IngredientReq::item(&format!("minecraft:item_{}", (i + 1) % items), 1),
            IngredientReq::tag(tags[i % tags.len()], 1),
        ];
        idx.add_recipe(r);
    }
    idx
}

#[test]
fn indexes_five_thousand_recipes_under_a_millisecond_per_query() {
    let idx = big_index(6_000, 5_000);
    assert_eq!(idx.recipes().len(), 5_000);
    assert_eq!(idx.items().len(), 6_000);

    let queries: &[&str] = &[
        "widget",
        "ダイヤ",
        "かがく",
        "@create",
        "#planks",
        "@create widget",
        "#cog かがく",
        "@thermal #ingots sprocket",
        "kagaku",
        "だいや",
    ];

    // Warm-up pass, then take the median of 5 runs per query so single
    // OS-scheduler preemptions on shared CI runners do not cause false
    // positives, and use an unoptimized debug ceiling when `debug_assertions`
    // is enabled (release builds keep the strict 1ms budget).
    for q in queries {
        let _ = idx.search(q);
    }
    let mut worst_median = std::time::Duration::ZERO;
    for q in queries {
        let mut samples = [std::time::Duration::ZERO; 5];
        for slot in &mut samples {
            let t = Instant::now();
            let hits = idx.search(q);
            *slot = t.elapsed();
            assert!(!hits.is_empty(), "query {q:?} returned nothing");
        }
        samples.sort_unstable();
        worst_median = worst_median.max(samples[2]);
    }
    // O(1) lookups measured too.
    let t = Instant::now();
    for i in 0..1_000usize {
        let _ = idx.recipes_producing(&format!("minecraft:item_{}", i % 6_000));
        let _ = idx.recipes_using(&format!("minecraft:item_{}", i % 6_000));
    }
    let lookup_total = t.elapsed();
    let query_budget = if cfg!(debug_assertions) {
        std::time::Duration::from_millis(15)
    } else {
        std::time::Duration::from_millis(1)
    };
    let lookup_budget = if cfg!(debug_assertions) {
        std::time::Duration::from_millis(200)
    } else {
        std::time::Duration::from_millis(50)
    };
    eprintln!("worst median query: {worst_median:?} (budget {query_budget:?}), 2000 R/U lookups: {lookup_total:?}");
    assert!(
        worst_median < query_budget,
        "search exceeded {query_budget:?} budget: {worst_median:?}"
    );
    assert!(
        lookup_total < lookup_budget,
        "R/U lookups too slow: {lookup_total:?} (budget {lookup_budget:?})"
    );
}
