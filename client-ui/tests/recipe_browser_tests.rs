// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! End-to-end tests for the unified recipe & resource browser: JEI/REI
//! `@mod`/`#tag`/`$category` search, `R`/`U` recipes-vs-uses navigation,
//! the bookmarks panel, EMI recipe-tree recursion with cycle safety and
//! raw-material math (including Create sequenced-assembly loops), JER
//! mob/ore/trade/loot/enchant tabs, datapack JSON ingestion, the Create
//! 3D Ponder renderer, keyboard/gamepad routing, and a layout that must
//! never overlap hit regions or rasterize blank at `640×360` through
//! `3840×2160` in English and Japanese.

use std::collections::BTreeMap;

use megu_client_ui::draw::{rasterize_draw_list, UiCommand, UiRect};
use megu_client_ui::recipe_browser::{
    AnimatedPart, BrowserTab, DetailMode, DungeonLootEntry, EnchantmentCompat, FuelEntry,
    HeatRequirement, ItemEntry, JerTab, MobDropEntry, OreDistribution, PonderAnim, PonderAxis,
    PonderScene, PonderStep, Recipe, RecipeBrowserAction, RecipeBrowserState, RecipeCategory,
    RecipeHitRegion, RecipeIndex, RecipeIngredient, RecipeWidgetId, VillagerTradeEntry,
};
use megu_client_ui::state::{GamepadButton, UiInputEvent, UiKey, UiLang};

const SIZES: [(u32, u32); 4] = [(640, 360), (1280, 720), (1920, 1080), (3840, 2160)];

fn item(id: &str, name: &str) -> ItemEntry {
    ItemEntry::new(id, name)
}

/// A catalog + recipe + JER fixture covering every browser surface.
///
/// Recipe load order matters for default tree expansion:
/// `nuggets_from_ingot` and `ingot_from_block` sit before `smelt_iron`,
/// so an unselected `iron_ingot` node takes the decompression recipe —
/// which is exactly what exercises the cycle detector.
fn fixture_index() -> RecipeIndex {
    let mut idx = RecipeIndex::new();
    let mut iron = item("minecraft:iron_ingot", "Iron Ingot");
    iron.tags = vec!["c:ingots/iron".into(), "minecraft:beacon_payment_items".into()];
    iron.tooltip_lines = vec!["A sturdy metal bar.".into()];
    let mut block = item("minecraft:iron_block", "Iron Block");
    block.tags = vec!["c:storage_blocks/iron".into()];
    let mut nugget = item("minecraft:iron_nugget", "Iron Nugget");
    nugget.tags = vec!["c:nuggets/iron".into()];
    let mut ore = item("minecraft:iron_ore", "Iron Ore");
    ore.tags = vec!["minecraft:iron_ores".into()];
    let mut planks = item("minecraft:oak_planks", "Oak Planks");
    planks.tags = vec!["minecraft:planks".into()];
    let mut log = item("minecraft:oak_log", "Oak Log");
    log.tags = vec!["minecraft:logs".into()];
    let mut cobble = item("minecraft:cobblestone", "Cobblestone");
    cobble.tags = vec!["c:cobblestones".into()];
    let mut cog = item("create:cogwheel", "Cogwheel");
    cog.tags = vec!["create:cogwheels".into()];
    cog.tooltip_lines = vec!["Transfers rotation.".into()];
    let sheet = item("create:iron_sheet", "Iron Sheet");
    let mechanism = item("create:precision_mechanism", "Precision Mechanism");
    let incomplete = item("create:incomplete_precision_mechanism", "Incomplete Mechanism");
    let crushed = item("create:crushed_iron_ore", "Crushed Iron Ore");
    let gearbox = item("create_addon:gearbox", "Turbo Gearbox");
    let emerald = item("minecraft:emerald", "Emerald");
    let book = item("minecraft:book", "Book");
    let e_book = item("minecraft:enchanted_book", "Enchanted Book");
    let sword = item("minecraft:diamond_sword", "Diamond Sword");
    let andesite = item("minecraft:andesite", "Andesite");
    let stairs = item("minecraft:cobblestone_stairs", "Cobblestone Stairs");
    idx.items = vec![
        iron, block, nugget, ore, planks, log, cobble, cog, sheet, mechanism, incomplete,
        crushed, gearbox, emerald, book, e_book, sword, andesite, stairs,
    ];

    // 9x ingot -> block (shaped grid).
    let mut to_block = Recipe::new("minecraft:iron_block", RecipeCategory::CraftingShaped);
    to_block.inputs = vec![RecipeIngredient::item("minecraft:iron_ingot", 9)];
    to_block.outputs = vec![megu_client_ui::recipe_browser::ItemStack {
        item: "minecraft:iron_block".into(),
        count: 1,
    }];
    idx.add_recipe(to_block);
    // ingot -> 9 nuggets (ingot compression, another loop arm).
    let mut to_nuggets = Recipe::new("minecraft:iron_nugget", RecipeCategory::CraftingShapeless);
    to_nuggets.inputs = vec![RecipeIngredient::item("minecraft:iron_ingot", 1)];
    to_nuggets.outputs = vec![megu_client_ui::recipe_browser::ItemStack {
        item: "minecraft:iron_nugget".into(),
        count: 9,
    }];
    idx.add_recipe(to_nuggets);
    // block -> 9 ingots (completes the block<->ingot cycle).
    let mut to_ingots = Recipe::new("minecraft:iron_ingot_from_block", RecipeCategory::CraftingShapeless);
    to_ingots.inputs = vec![RecipeIngredient::item("minecraft:iron_block", 1)];
    to_ingots.outputs = vec![megu_client_ui::recipe_browser::ItemStack {
        item: "minecraft:iron_ingot".into(),
        count: 9,
    }];
    idx.add_recipe(to_ingots);
    // ore -> ingot smelting.
    let mut smelt = Recipe::new("minecraft:smelt_iron", RecipeCategory::Smelting);
    smelt.inputs = vec![RecipeIngredient::item("minecraft:iron_ore", 1)];
    smelt.outputs = vec![megu_client_ui::recipe_browser::ItemStack {
        item: "minecraft:iron_ingot".into(),
        count: 1,
    }];
    smelt.processing_ticks = 200;
    idx.add_recipe(smelt);
    // log -> 4 planks (tag input via oak_log? use concrete id).
    let mut to_planks = Recipe::new("minecraft:oak_planks", RecipeCategory::CraftingShapeless);
    to_planks.inputs = vec![RecipeIngredient::item("minecraft:oak_log", 1)];
    to_planks.outputs = vec![megu_client_ui::recipe_browser::ItemStack {
        item: "minecraft:oak_planks".into(),
        count: 4,
    }];
    idx.add_recipe(to_planks);
    // stonecutting: cobble -> 4 stairs.
    let mut cut = Recipe::new("minecraft:cobblestone_stairs_cut", RecipeCategory::Stonecutting);
    cut.inputs = vec![RecipeIngredient::item("minecraft:cobblestone", 1)];
    cut.outputs = vec![megu_client_ui::recipe_browser::ItemStack {
        item: "minecraft:cobblestone_stairs".into(),
        count: 4,
    }];
    idx.add_recipe(cut);
    // Create pressing: ingot -> sheet.
    let mut press = Recipe::new("create:pressing/iron_sheet", RecipeCategory::CreatePressing);
    press.inputs = vec![RecipeIngredient::item("minecraft:iron_ingot", 1)];
    press.outputs = vec![megu_client_ui::recipe_browser::ItemStack {
        item: "create:iron_sheet".into(),
        count: 1,
    }];
    press.rpm_min = Some(32);
    idx.add_recipe(press);

    // JER rows.
    idx.fuels.push(FuelEntry {
        item: "minecraft:coal".into(),
        burn_ticks: 1600,
    });
    idx.mob_drops.push(MobDropEntry {
        mob_id: "minecraft:zombie".into(),
        item_id: "minecraft:iron_ingot".into(),
        min_count: 0,
        max_count: 2,
        chance: 0.12,
        looting_bonus: 1,
        condition: "rare drop".into(),
    });
    idx.ore_distributions.push(OreDistribution {
        block_id: "minecraft:iron_ore".into(),
        dimension: "minecraft:overworld".into(),
        min_y: -64,
        max_y: 72,
        peak_y: 15,
        vein_size: 9,
        biome: String::new(),
    });
    idx.villager_trades.push(VillagerTradeEntry {
        profession: "minecraft:librarian".into(),
        level: 4,
        buy_a: ("minecraft:emerald".into(), 5),
        buy_b: Some(("minecraft:book".into(), 1)),
        sell: ("minecraft:enchanted_book".into(), 1),
        max_uses: 12,
    });
    idx.dungeon_loot.push(DungeonLootEntry {
        chest: "minecraft:chests/simple_dungeon".into(),
        item_id: "minecraft:iron_ingot".into(),
        min_count: 1,
        max_count: 4,
        weight: 20,
        chance: 0.35,
    });
    idx.enchant_compat.push(EnchantmentCompat {
        item_id: "minecraft:diamond_sword".into(),
        enchantments: vec![
            ("minecraft:sharpness".into(), 5),
            ("minecraft:unbreaking".into(), 3),
        ],
    });

    // Datapack/mod JSONs ingested at fixture build time.
    idx.load_recipe_json(
        "create:mixing/andesite",
        r#"{
            "type": "create:mixing",
            "ingredients": [
                {"item": "minecraft:cobblestone"},
                {"fluid": "minecraft:water", "amount": 500}
            ],
            "results": [{"item": "minecraft:andesite", "count": 1}],
            "heatRequirement": "heated"
        }"#,
    )
    .expect("mixing JSON loads");
    idx.load_recipe_json(
        "create:crushing/iron_ore",
        r#"{
            "type": "create:crushing",
            "ingredients": [{"item": "minecraft:iron_ore"}],
            "results": [
                {"item": "create:crushed_iron_ore", "count": 1},
                {"item": "create:crushed_iron_ore", "count": 1, "chance": 0.75}
            ],
            "processingTime": 250
        }"#,
    )
    .expect("crushing JSON loads");
    idx.load_recipe_json(
        "create:sequenced_assembly/precision_mechanism",
        r#"{
            "type": "create:sequenced_assembly",
            "ingredient": {"item": "create:iron_sheet"},
            "transitionalItem": {"item": "create:incomplete_precision_mechanism"},
            "sequence": [
                {
                    "type": "create:deploying",
                    "ingredients": [
                        {"item": "create:incomplete_precision_mechanism"},
                        {"item": "create:cogwheel"}
                    ],
                    "results": [{"item": "create:incomplete_precision_mechanism"}]
                },
                {
                    "type": "create:pressing",
                    "ingredients": [{"item": "create:incomplete_precision_mechanism"}],
                    "results": [{"item": "create:incomplete_precision_mechanism"}]
                }
            ],
            "loops": 3,
            "results": [{"item": "create:precision_mechanism", "count": 1, "chance": 1.0}]
        }"#,
    )
    .expect("sequenced JSON loads");
    // Declared custom addon category.
    idx.register_custom_category("create_addon:turbo_spin", "Addon: Turbo Spin", "アドオン: ターボ回転");
    idx.load_recipe_json(
        "create_addon:gearbox",
        r#"{
            "type": "create_addon:turbo_spin",
            "ingredients": [{"item": "create:cogwheel", "count": 2}],
            "results": [{"item": "create_addon:gearbox", "count": 1}]
        }"#,
    )
    .expect("custom category JSON loads");
    idx
}

fn fixture() -> RecipeBrowserState {
    RecipeBrowserState::new(fixture_index())
}

fn key(state: &mut RecipeBrowserState, w: u32, h: u32, k: UiKey) -> Vec<RecipeBrowserAction> {
    state.handle_input(UiInputEvent::KeyPressed(k), w, h)
}

fn typed(state: &mut RecipeBrowserState, w: u32, h: u32, c: char) -> Vec<RecipeBrowserAction> {
    state.handle_input(UiInputEvent::CharTyped(c), w, h)
}

fn type_str(state: &mut RecipeBrowserState, w: u32, h: u32, text: &str) {
    for c in text.chars() {
        state.handle_input(UiInputEvent::CharTyped(c), w, h);
    }
}

fn find(hits: &[RecipeHitRegion], id: RecipeWidgetId) -> Option<UiRect> {
    hits.iter().find(|r| r.id == id).map(|r| r.rect)
}

fn click(state: &mut RecipeBrowserState, w: u32, h: u32, id: RecipeWidgetId) -> Vec<RecipeBrowserAction> {
    let (_, hits) = state.render(w, h);
    let rect = find(&hits, id).unwrap_or_else(|| panic!("no hit region for {id:?}"));
    let (x, y) = (rect.x + rect.w as i32 / 2, rect.y + rect.h as i32 / 2);
    state.handle_input(UiInputEvent::Click { x, y }, w, h)
}

fn assert_no_overlaps(regions: &[RecipeHitRegion]) {
    for (i, a) in regions.iter().enumerate() {
        for (j, b) in regions.iter().enumerate() {
            if i >= j {
                continue;
            }
            assert!(
                !a.rect.intersects(b.rect),
                "hit regions overlap: {a:?} vs {b:?}"
            );
        }
    }
}

fn assert_non_blank(rgba: &[u8]) {
    assert!(rgba.len().is_multiple_of(4));
    let (pixels, _) = rgba.as_chunks::<4>();
    let non_transparent = pixels.iter().filter(|p| p[3] != 0).count();
    assert!(non_transparent > 100, "frame is blank");
    let first = pixels[0];
    let different = pixels.iter().filter(|&&p| p != first).count();
    assert!(different > 100, "frame is a flat fill");
}

/// Item ids in the currently visible (filtered, scored) grid order.
fn visible_ids(state: &RecipeBrowserState) -> Vec<String> {
    state
        .visible_items()
        .iter()
        .map(|&i| state.index.items[i].id.clone())
        .collect()
}

// ---------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------

#[test]
fn search_text_mod_tag_and_category_tokens() {
    let (w, h) = (1280, 720);
    let mut s = fixture();

    // Plain text: case-insensitive on names and ids.
    type_str(&mut s, w, h, "IRON");
    let ids = visible_ids(&s);
    assert!(ids.contains(&"minecraft:iron_ingot".to_string()));
    assert!(ids.contains(&"minecraft:iron_block".to_string()));
    assert!(ids.contains(&"create:iron_sheet".to_string()));

    // @mod restricts to a mod namespace (substring: create + create_addon).
    s.search = "@create".into();
    for id in visible_ids(&s) {
        assert!(id.starts_with("create") || id.starts_with("create_addon"), "{id}");
    }
    s.search = "@create_addon".into();
    assert_eq!(visible_ids(&s), ["create_addon:gearbox"]);
    s.search = "@minecraft".into();
    assert!(visible_ids(&s).iter().all(|id| id.starts_with("minecraft:")));

    // #tag matches inside the tag string.
    s.search = "#c:ingots".into();
    assert_eq!(visible_ids(&s), ["minecraft:iron_ingot"]);
    s.search = "#minecraft:planks".into();
    assert_eq!(visible_ids(&s), ["minecraft:oak_planks"]);
    s.search = "#create:cogwheels".into();
    assert_eq!(visible_ids(&s), ["create:cogwheel"]);

    // $category matches recipes producing or consuming the item.
    s.search = "$pressing".into();
    let ids = visible_ids(&s);
    assert!(ids.contains(&"create:iron_sheet".to_string())); // produced
    assert!(ids.contains(&"minecraft:iron_ingot".to_string())); // consumed
    assert!(!ids.contains(&"minecraft:oak_log".to_string()));
    s.search = "$sequenced".into();
    let ids = visible_ids(&s);
    assert!(ids.contains(&"create:precision_mechanism".to_string()));
    assert!(ids.contains(&"create:iron_sheet".to_string())); // base ingredient
    // Custom addon category is searchable too.
    s.search = "$turbo".into();
    let ids = visible_ids(&s);
    assert!(ids.contains(&"create_addon:gearbox".to_string()));

    // Multi-token AND: text + @mod.
    s.search = "iron @create".into();
    let ids = visible_ids(&s);
    assert!(ids.contains(&"create:iron_sheet".to_string()));
    assert!(ids.contains(&"create:crushed_iron_ore".to_string()));
    assert!(!ids.contains(&"minecraft:iron_ingot".to_string()));

    // Text also searches tags and tooltip lines.
    s.search = "rotation".into();
    assert_eq!(visible_ids(&s), ["create:cogwheel"]);
}

// ---------------------------------------------------------------------------
// R / U navigation
// ---------------------------------------------------------------------------

#[test]
fn r_and_u_open_recipes_and_uses_for_selection() {
    let (w, h) = (1280, 720);
    let mut s = fixture();
    // Search focused by default: 'r' types into the search, not a shortcut.
    typed(&mut s, w, h, 'r');
    assert_eq!(s.search, "r");
    assert!(s.detail.is_none());
    s.search.clear();

    // Unfocus the search, select iron_ingot, press R -> Recipes detail.
    key(&mut s, w, h, UiKey::Tab);
    s.search = "iron_ingot".into();
    assert_eq!(s.selected_item().as_deref(), Some("minecraft:iron_ingot"));
    typed(&mut s, w, h, 'r');
    assert_eq!(
        s.detail,
        Some(("minecraft:iron_ingot".to_string(), DetailMode::Recipes))
    );
    // Recipes producing iron_ingot: from-block + smelting (JSON or manual).
    let texts: Vec<String> = s
        .render(w, h)
        .0
        .commands
        .iter()
        .filter_map(|c| match c {
            UiCommand::Text { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert!(texts.iter().any(|t| t.contains("Recipes for")));
    assert!(texts.iter().any(|t| t.contains("smelt_iron")));

    // Escape backs out to the grid, Escape again closes.
    assert!(key(&mut s, w, h, UiKey::Escape).is_empty());
    assert!(s.detail.is_none());
    let actions = key(&mut s, w, h, UiKey::Escape);
    assert_eq!(actions, [RecipeBrowserAction::Close]);

    // U opens uses (ingot is consumed by block, nuggets, pressing, mixing? no).
    let mut s2 = fixture();
    key(&mut s2, w, h, UiKey::Tab);
    s2.search = "iron_ingot".into();
    typed(&mut s2, w, h, 'u');
    assert_eq!(
        s2.detail,
        Some(("minecraft:iron_ingot".to_string(), DetailMode::Uses))
    );
    let texts: Vec<String> = s2
        .render(w, h)
        .0
        .commands
        .iter()
        .filter_map(|c| match c {
            UiCommand::Text { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert!(texts.iter().any(|t| t.contains("Uses of")));
    assert!(texts.iter().any(|t| t.contains("pressing/iron_sheet")));
}

// ---------------------------------------------------------------------------
// Bookmarks
// ---------------------------------------------------------------------------

#[test]
fn bookmarks_add_remove_and_reorder() {
    let (w, h) = (1280, 720);
    let mut s = fixture();
    key(&mut s, w, h, UiKey::Tab); // unfocus search

    // 'a' toggles a bookmark on the grid selection.
    s.search = "iron".into();
    let first = s.selected_item().unwrap();
    typed(&mut s, w, h, 'a');
    assert!(s.is_bookmarked(&first));
    // Second item too.
    key(&mut s, w, h, UiKey::Down);
    let second = s.selected_item().unwrap();
    assert_ne!(first, second);
    typed(&mut s, w, h, 'a');
    assert_eq!(s.bookmarks.len(), 2);
    // Toggle off the first.
    key(&mut s, w, h, UiKey::Up);
    typed(&mut s, w, h, 'a');
    assert!(!s.is_bookmarked(&first));
    assert_eq!(s.bookmarks, [second.as_str()]);

    // Add a third and exercise reorder via widget activation.
    s.search = "cogwheel".into();
    let third = s.selected_item().unwrap();
    typed(&mut s, w, h, 'a');
    s.bookmark_selected = Some(0);
    s.activate(RecipeWidgetId::BookmarkDown); // swap(0,1) -> [third, second]
    assert_eq!(s.bookmarks, [third.clone(), second.clone()]);
    s.activate(RecipeWidgetId::BookmarkUp); // swap(1,0) -> [second, third]
    assert_eq!(s.bookmarks, [second.clone(), third.clone()]);

    // Remove the selected bookmark via the panel button.
    let (_, hits) = s.render(w, h);
    let remove = find(&hits, RecipeWidgetId::BookmarkRemove).expect("remove button");
    let (x, y) = (remove.x + 8, remove.y + 7);
    s.handle_input(UiInputEvent::Click { x, y }, w, h);
    assert_eq!(s.bookmarks.len(), 1);

    // Clicking a bookmark row opens its recipes.
    let (_, hits) = s.render(w, h);
    let row = find(&hits, RecipeWidgetId::BookmarkRow(0)).expect("bookmark row");
    let (x, y) = (row.x + 8, row.y + 7);
    s.handle_input(UiInputEvent::Click { x, y }, w, h);
    assert!(s.detail.is_some());
}

// ---------------------------------------------------------------------------
// Recipe tree + raw material math
// ---------------------------------------------------------------------------

#[test]
fn recipe_tree_raw_materials_and_cycle_safety() {
    let idx = fixture_index();
    let none: BTreeMap<String, String> = BTreeMap::new();

    // Direct tree for 2x iron_block: ingot expands via from_block (first
    // producing recipe in index order) -> iron_block again -> cycle leaf.
    let tree = idx.recipe_tree("minecraft:iron_block", 2, &none);
    let root = tree.root.as_ref().expect("root node");
    assert_eq!(root.crafts, 2);
    assert_eq!(root.children.len(), 1);
    let ingot = &root.children[0];
    assert_eq!(ingot.item, "minecraft:iron_ingot");
    assert_eq!(ingot.needed, 18);
    // The ingot's child is iron_block again, flagged as a cycle.
    let cycle = &ingot.children[0];
    assert_eq!(cycle.item, "minecraft:iron_block");
    assert!(cycle.is_cycle);
    // Terminated: the looped amount lands in raw materials.
    assert_eq!(tree.raw_materials.get("minecraft:iron_block"), Some(&2));
    assert!(!tree.truncated);

    // Selection override: smelt the ingots -> raw ore.
    let sel: BTreeMap<String, String> =
        [("minecraft:iron_ingot".to_string(), "minecraft:smelt_iron".to_string())].into();
    let tree = idx.recipe_tree("minecraft:iron_block", 2, &sel);
    assert_eq!(tree.raw_materials.get("minecraft:iron_ore"), Some(&18));
    assert!(!tree.raw_materials.contains_key("minecraft:iron_block"));

    // Planks: log -> 4 planks; 10 planks need 3 logs (ceil 10/4).
    let tree = idx.recipe_tree("minecraft:oak_planks", 10, &none);
    assert_eq!(tree.root.as_ref().unwrap().crafts, 3);
    assert_eq!(tree.raw_materials.get("minecraft:oak_log"), Some(&3));

    // Remaining-needed against an inventory.
    let mut inv = BTreeMap::new();
    inv.insert("minecraft:oak_log".to_string(), 2u64);
    let rem = tree.remaining_needed(&inv);
    assert_eq!(rem.get("minecraft:oak_log"), Some(&1));
    inv.insert("minecraft:oak_log".to_string(), 99u64);
    assert!(tree.remaining_needed(&inv).is_empty());
}

#[test]
fn sequenced_assembly_loops_feed_the_material_calculator() {
    let idx = fixture_index();
    let none: BTreeMap<String, String> = BTreeMap::new();
    // Precision mechanism: 1 iron_sheet + (cogwheel x1) * 3 loops; the
    // transitional item never counts as a material.
    let tree = idx.recipe_tree("create:precision_mechanism", 1, &none);
    let root = tree.root.as_ref().unwrap();
    assert_eq!(
        root.recipe_id.as_deref(),
        Some("create:sequenced_assembly/precision_mechanism")
    );
    // Children: iron_sheet (expands via pressing) + cogwheel x3 (raw).
    let items: Vec<&str> = root.children.iter().map(|c| c.item.as_str()).collect();
    assert!(items.contains(&"create:iron_sheet"));
    assert!(items.contains(&"create:cogwheel"));
    let cog = root.children.iter().find(|c| c.item == "create:cogwheel").unwrap();
    assert_eq!(cog.needed, 3); // 1 per sequence step * 3 loops
    // No transitional item anywhere in the tree.
    for (_, node) in root.flatten() {
        assert_ne!(node.item, "create:incomplete_precision_mechanism");
    }
    // iron_sheet -> pressing -> iron_ingot -> (first recipe: from_block -> cycle)
    let raw = &tree.raw_materials;
    assert_eq!(raw.get("create:cogwheel"), Some(&3));
    // With a smelting override the chain resolves to iron_ore.
    let sel: BTreeMap<String, String> =
        [("minecraft:iron_ingot".to_string(), "minecraft:smelt_iron".to_string())].into();
    let tree2 = idx.recipe_tree("create:precision_mechanism", 2, &sel);
    assert_eq!(tree2.raw_materials.get("minecraft:iron_ore"), Some(&2));
    assert_eq!(tree2.raw_materials.get("create:cogwheel"), Some(&6));
}

#[test]
fn tree_node_budget_and_depth_cap_never_hang() {
    let mut idx = RecipeIndex::new();
    idx.add_item(item("test:root", "Root"));
    idx.add_item(item("test:a", "A"));
    idx.add_item(item("test:b", "B"));
    // A branching recipe: root needs a and b; a needs a+b... unbounded fan-out
    // would explode without the node budget.
    let mut r = Recipe::new("test:branch", RecipeCategory::CraftingShapeless);
    r.inputs = vec![RecipeIngredient::item("test:a", 1), RecipeIngredient::item("test:b", 1)];
    r.outputs = vec![megu_client_ui::recipe_browser::ItemStack {
        item: "test:root".into(),
        count: 1,
    }];
    idx.add_recipe(r);
    for (id, inp) in [("test:a", "test:b"), ("test:b", "test:a")] {
        let mut rr = Recipe::new(&format!("test:{id}"), RecipeCategory::CraftingShapeless);
        rr.inputs = vec![RecipeIngredient::item(inp, 1)];
        rr.outputs = vec![megu_client_ui::recipe_browser::ItemStack {
            item: id.into(),
            count: 1,
        }];
        idx.add_recipe(rr);
    }
    let tree = idx.recipe_tree("test:root", 1, &BTreeMap::new());
    // a<->b is a cycle pair: expansion terminates quickly.
    assert!(tree.root.is_some());
    let flat = tree.root.as_ref().unwrap().flatten();
    assert!(flat.iter().any(|(_, n)| n.is_cycle));
}

// ---------------------------------------------------------------------------
// JSON datapack ingestion
// ---------------------------------------------------------------------------

#[test]
fn json_loader_parses_vanilla_and_create_recipes() {
    let idx = fixture_index();
    let mixing = idx.recipe("create:mixing/andesite").unwrap();
    assert_eq!(mixing.category, RecipeCategory::CreateMixing);
    assert_eq!(mixing.heat, HeatRequirement::Heated);
    assert_eq!(mixing.fluid_inputs.len(), 1);
    assert_eq!(mixing.fluid_inputs[0].fluid, "minecraft:water");
    assert_eq!(mixing.fluid_inputs[0].amount_mb, 500);
    assert_eq!(mixing.outputs[0].item, "minecraft:andesite");

    let crush = idx.recipe("create:crushing/iron_ore").unwrap();
    assert_eq!(crush.category, RecipeCategory::CreateCrushing);
    assert_eq!(crush.outputs.len(), 1);
    assert_eq!(crush.chanced_outputs.len(), 1);
    assert_eq!(crush.chanced_outputs[0].1, 1);
    assert!((crush.chanced_outputs[0].2 - 0.75).abs() < 0.001);
    assert_eq!(crush.processing_ticks, 250);

    let seq = idx
        .recipe("create:sequenced_assembly/precision_mechanism")
        .unwrap();
    assert_eq!(seq.category, RecipeCategory::CreateSequencedAssembly);
    assert_eq!(seq.loops, 3);
    assert_eq!(seq.sequence_steps.len(), 2);
    assert_eq!(seq.sequence_steps[0].category, RecipeCategory::CreateDeploying);
    assert_eq!(seq.sequence_steps[1].category, RecipeCategory::CreatePressing);
    assert_eq!(seq.transitional_item, "create:incomplete_precision_mechanism");

    let custom = idx.recipe("create_addon:gearbox").unwrap();
    match &custom.category {
        RecipeCategory::CustomModCategory { id, .. } => {
            assert_eq!(id, "create_addon:turbo_spin")
        }
        other => panic!("expected custom category, got {other:?}"),
    }
    // Custom category participates in $ search and use-indexing.
    assert!(custom.category.matches_query("turbo"));
    assert!(idx
        .recipes_consuming("create:cogwheel")
        .iter()
        .any(|r| r.id == "create_addon:gearbox"));

    // Vanilla shaped JSON with pattern/key -> grid populated.
    let mut idx2 = RecipeIndex::new();
    idx2.load_recipe_json(
        "minecraft:stick",
        r##"{
            "type": "minecraft:crafting_shaped",
            "pattern": ["#", "#"],
            "key": {"#": {"item": "minecraft:oak_planks"}},
            "result": {"item": "minecraft:stick", "count": 4}
        }"##,
    )
    .expect("shaped JSON loads");
    let stick = idx2.recipe("minecraft:stick").unwrap();
    assert_eq!(stick.outputs[0].count, 4);
    let grid = stick.grid.as_ref().expect("shaped grid");
    assert_eq!(grid.len(), 2); // 1-wide pattern, 2 rows
    assert_eq!(grid[0], Some(0));
    assert_eq!(grid[1], Some(0));
}

#[test]
fn json_loader_fails_closed_on_malformed_input() {
    let mut idx = RecipeIndex::new();
    let before = idx.recipes.len();
    let deep = "{".repeat(500) + &"}".repeat(500);
    let malformed = [
        "",
        "not json",
        "{",
        "{\"type\": }",
        "{\"type\": \"unknown_mod:thing\"}", // unregistered type
        "[]",                               // non-object root
        "{\"type\": \"create:mixing\"}",    // missing ingredients/results
        "{\"type\": \"minecraft:smelting\", \"ingredient\": {\"item\": \"a:b\"}}", // no result
        &deep, // absurd nesting
        "{\"type\": \"create:mixing\", \"ingredients\": [{\"item\": \"a:b\"}], \"results\": [{\"item\": \"a:c\", \"chance\": 9.0}]}",
        "\u{0}\u{1}binary garbage",
    ];
    for bad in malformed {
        assert_eq!(idx.load_recipe_json("bad:one", bad), None, "input: {bad:.60}");
    }
    assert_eq!(idx.recipes.len(), before);
}

// ---------------------------------------------------------------------------
// JER tabs
// ---------------------------------------------------------------------------

#[test]
fn ore_distribution_histogram_is_deterministic_and_peaks() {
    let idx = fixture_index();
    let ore = &idx.ore_distributions[0];
    let hist = ore.histogram();
    assert_eq!(hist.len(), (ore.max_y - ore.min_y + 1) as usize);
    // Zero outside the range.
    assert_eq!(ore.weight_at(ore.min_y - 1), 0);
    assert_eq!(ore.weight_at(ore.max_y + 1), 0);
    // Peak is the maximum weight.
    let max = hist.iter().copied().max().unwrap();
    assert_eq!(ore.weight_at(ore.peak_y), max);
    // Monotone rise to the peak.
    let peak_i = (ore.peak_y - ore.min_y) as usize;
    for i in 1..=peak_i {
        assert!(hist[i] >= hist[i - 1], "hist[{i}] < hist[{}]", i - 1);
    }
    for i in (peak_i + 1)..hist.len() {
        assert!(hist[i] <= hist[i - 1]);
    }
}

#[test]
fn jer_tabs_render_all_sub_lists() {
    let (w, h) = (1280, 720);
    let mut s = fixture();
    s.tab = BrowserTab::Jer;
    for tab in [
        JerTab::MobDrops,
        JerTab::OreGen,
        JerTab::Trades,
        JerTab::Loot,
        JerTab::Enchants,
        JerTab::Fuels,
    ] {
        s.jer_tab = tab;
        let (list, hits) = s.render(w, h);
        let rgba = rasterize_draw_list(w, h, &list);
        assert_non_blank(&rgba);
        let texts: Vec<String> = list
            .commands
            .iter()
            .filter_map(|c| match c {
                UiCommand::Text { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        match tab {
            JerTab::MobDrops => assert!(texts.iter().any(|t| t.contains("zombie"))),
            JerTab::OreGen => assert!(texts.iter().any(|t| t.contains("peak"))),
            JerTab::Trades => assert!(texts.iter().any(|t| t.contains("librarian"))),
            JerTab::Loot => assert!(texts.iter().any(|t| t.contains("simple_dungeon"))),
            JerTab::Enchants => assert!(texts.iter().any(|t| t.contains("sharpness"))),
            JerTab::Fuels => {
                // Burn-time column — a fuel row must print its ticks.
                assert!(
                    texts
                        .iter()
                        .any(|t| t.contains("burn") || t.chars().any(|c| c.is_ascii_digit())),
                    "fuels tab shows burn ticks"
                );
            }
        }
        assert_no_overlaps(&hits);
    }
    // Ore histogram draws many vertical bars (FillRects) inside the chart.
    s.jer_tab = JerTab::OreGen;
    let (list, _) = s.render(w, h);
    let fills = list
        .commands
        .iter()
        .filter(|c| matches!(c, UiCommand::FillRect { .. }))
        .count();
    assert!(fills > 40, "histogram should paint many bars, got {fills}");
}

// ---------------------------------------------------------------------------
// Ponder 3D viewer
// ---------------------------------------------------------------------------

fn fixture_ponder() -> PonderScene {
    use megu_client_ui::draw::UiColor;
    let mut scene = PonderScene::new("Crushing Wheels");
    // 3x3 stone floor.
    for x in 0..3 {
        for z in 0..3 {
            scene.add_block(x, 0, z, UiColor::rgb(120, 120, 128));
        }
    }
    // Machine blocks.
    scene.add_block(1, 1, 1, UiColor::rgb(180, 140, 60));
    scene.add_block(1, 2, 1, UiColor::rgb(200, 60, 60));
    let mut step0 = PonderStep::new("The crushing wheel spins", "粉砕ホイールが回転", 20);
    step0.animated_parts.push(AnimatedPart {
        pos: (1, 2, 1),
        anim: PonderAnim::Spin {
            axis: PonderAxis::Y,
            rpm: 64.0,
        },
    });
    step0.highlight = Some((1, 1, 1));
    let mut step1 = PonderStep::new("A deployer places items", "デプロイヤーが設置", 20);
    step1.animated_parts.push(AnimatedPart {
        pos: (0, 2, 1),
        anim: PonderAnim::Oscillate {
            axis: PonderAxis::Y,
            amplitude: 0.4,
            period_ticks: 10,
        },
    });
    scene.steps = vec![step0, step1];
    scene
}

#[test]
fn ponder_scene_renders_depth_sorted_faces_and_highlights() {
    let (w, h) = (1280, 720);
    let mut s = fixture();
    s.ponder = Some(fixture_ponder());
    s.tab = BrowserTab::Ponder;
    let (list, hits) = s.render(w, h);
    let fills = list
        .commands
        .iter()
        .filter(|c| matches!(c, UiCommand::FillRect { .. }))
        .count();
    assert!(fills > 200, "iso voxel raster should emit many scanlines");
    let rgba = rasterize_draw_list(w, h, &list);
    assert_non_blank(&rgba);
    assert_no_overlaps(&hits);
    // Step caption visible.
    let texts: Vec<String> = list
        .commands
        .iter()
        .filter_map(|c| match c {
            UiCommand::Text { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert!(texts.iter().any(|t| t.contains("crushing wheel")));

    // Rotating the camera changes the rasterized frame (faces move).
    let (before, _) = s.render(w, h);
    s.ponder.as_mut().unwrap().rotate(90.0);
    let (after, _) = s.render(w, h);
    assert_ne!(before, after, "rotation must change the projection");
}

#[test]
fn ponder_steps_play_pause_and_wrap() {
    let mut s = fixture();
    s.ponder = Some(fixture_ponder());
    let p = s.ponder.as_mut().unwrap();
    assert_eq!(p.step_index, 0);
    // Paused: tick frozen.
    p.playing = false;
    p.advance_tick();
    assert_eq!(p.tick, 0);
    p.playing = true;
    // 20-tick step: advancing through it moves to step 1 then wraps to 0.
    for _ in 0..20 {
        p.advance_tick();
    }
    assert_eq!(p.step_index, 1);
    for _ in 0..20 {
        p.advance_tick();
    }
    assert_eq!(p.step_index, 0);
    // Manual stepping wraps both directions.
    p.prev_step();
    assert_eq!(p.step_index, 1);
    p.prev_step();
    assert_eq!(p.step_index, 0);
    p.next_step();
    assert_eq!(p.step_index, 1);
}

#[test]
fn ponder_controls_via_widgets_and_keys() {
    let (w, h) = (1280, 720);
    let mut s = fixture();
    s.ponder = Some(fixture_ponder());
    s.tab = BrowserTab::Ponder;
    // Widgets: rotate right, next step, pause.
    click(&mut s, w, h, RecipeWidgetId::PonderRotateRight);
    assert!((s.ponder.as_ref().unwrap().yaw_deg - 60.0).abs() < 0.01);
    click(&mut s, w, h, RecipeWidgetId::PonderNext);
    assert_eq!(s.ponder.as_ref().unwrap().step_index, 1);
    click(&mut s, w, h, RecipeWidgetId::PonderPlay);
    assert!(!s.ponder.as_ref().unwrap().playing);
    click(&mut s, w, h, RecipeWidgetId::PonderPrev);
    assert_eq!(s.ponder.as_ref().unwrap().step_index, 0);
    // Keyboard: left/right rotate, up/down step, space toggles play.
    key(&mut s, w, h, UiKey::Left);
    assert!((s.ponder.as_ref().unwrap().yaw_deg - 45.0).abs() < 0.01);
    key(&mut s, w, h, UiKey::Space);
    assert!(s.ponder.as_ref().unwrap().playing);
}

// ---------------------------------------------------------------------------
// Transfer / crafting-grid fill
// ---------------------------------------------------------------------------

#[test]
fn transfer_button_emits_max_craft_from_inventory() {
    let (w, h) = (1280, 720);
    let mut s = fixture();
    // 27 ingots -> 3 iron_block crafts (9 each).
    s.inventory.insert("minecraft:iron_ingot".to_string(), 27);
    key(&mut s, w, h, UiKey::Tab);
    s.search = "iron_block".into();
    typed(&mut s, w, h, 'r');
    assert!(s.detail.is_some());
    let actions = click(&mut s, w, h, RecipeWidgetId::TransferRecipe(0));
    assert_eq!(
        actions,
        [RecipeBrowserAction::TransferToCraftingGrid {
            recipe_id: "minecraft:iron_block".to_string(),
            max_craft: 3,
        }]
    );
    // Without an inventory the '+' reports a full stack (host default).
    let mut s2 = fixture();
    key(&mut s2, w, h, UiKey::Tab);
    s2.search = "iron_block".into();
    typed(&mut s2, w, h, 'r');
    let actions = click(&mut s2, w, h, RecipeWidgetId::TransferRecipe(0));
    assert!(matches!(
        actions[0],
        RecipeBrowserAction::TransferToCraftingGrid { max_craft: 64, .. }
    ));
}

// ---------------------------------------------------------------------------
// Keyboard & gamepad navigation
// ---------------------------------------------------------------------------

#[test]
fn keyboard_and_gamepad_navigation() {
    let (w, h) = (1280, 720);
    let mut s = fixture();
    // Tab toggles search focus.
    assert!(s.search_focused());
    key(&mut s, w, h, UiKey::Tab);
    assert!(!s.search_focused());
    key(&mut s, w, h, UiKey::Tab);
    assert!(s.search_focused());
    key(&mut s, w, h, UiKey::Tab);

    // Arrows move the grid selection (right = next slot).
    let first = s.selected_item();
    key(&mut s, w, h, UiKey::Right);
    assert_ne!(s.selected_item(), first);
    key(&mut s, w, h, UiKey::Left);
    assert_eq!(s.selected_item(), first);
    // Down moves a whole grid row.
    let idx0 = s.selected_index();
    key(&mut s, w, h, UiKey::Down);
    assert!(s.selected_index() > idx0);

    // Enter opens recipes for the selection; gamepad South mirrors Enter.
    let mut g = fixture();
    key(&mut g, w, h, UiKey::Tab);
    g.handle_input(UiInputEvent::GamepadPressed(GamepadButton::South), w, h);
    assert!(g.detail.is_some());
    // Gamepad East backs out, then closes.
    let acts = g.handle_input(UiInputEvent::GamepadPressed(GamepadButton::East), w, h);
    assert!(acts.is_empty() && g.detail.is_none());
    let acts = g.handle_input(UiInputEvent::GamepadPressed(GamepadButton::East), w, h);
    assert_eq!(acts, [RecipeBrowserAction::Close]);

    // Backspace edits and refocuses the search.
    s.search = "plank".into();
    key(&mut s, w, h, UiKey::Backspace);
    assert!(s.search_focused());
    assert_eq!(s.search, "plan");
}

// ---------------------------------------------------------------------------
// Layout across sizes and languages
// ---------------------------------------------------------------------------

#[test]
fn layout_has_zero_overlaps_and_non_blank_frames() {
    for &(w, h) in &SIZES {
        for lang in [UiLang::En, UiLang::Ja] {
            let mut s = fixture();
            s.lang = lang;
            s.ponder = Some(fixture_ponder());
            for tab in BrowserTab::ALL {
                s.tab = tab;
                let (list, hits) = s.render(w, h);
                assert!(!list.is_empty(), "{w}x{h} {lang:?} {tab:?} blank list");
                assert_no_overlaps(&hits);
                let rgba = rasterize_draw_list(w, h, &list);
                assert_non_blank(&rgba);
            }
            // Detail view + a built tree also stay clean.
            s.tab = BrowserTab::Items;
            s.open_detail("minecraft:iron_ingot", DetailMode::Recipes);
            let (list, hits) = s.render(w, h);
            assert_no_overlaps(&hits);
            assert_non_blank(&rasterize_draw_list(w, h, &list));
            s.open_tree("minecraft:iron_block", 8);
            let (list, hits) = s.render(w, h);
            assert_no_overlaps(&hits);
            assert_non_blank(&rasterize_draw_list(w, h, &list));
        }
    }
}

#[test]
fn layout_stays_consistent_with_large_catalog() {
    let (w, h) = (1280, 720);
    let mut idx = fixture_index();
    for i in 0..200 {
        idx.add_item(item(&format!("test:item_{i:03}"), &format!("Test Item {i:03}")));
    }
    let mut s = RecipeBrowserState::new(idx);
    // Grid + scroll buttons must not overlap at page 1 or scrolled deep.
    let (_, hits) = s.render(w, h);
    assert_no_overlaps(&hits);
    for _ in 0..5 {
        s.activate(RecipeWidgetId::GridScrollDown);
    }
    let (list, hits) = s.render(w, h);
    assert_no_overlaps(&hits);
    assert_non_blank(&rasterize_draw_list(w, h, &list));
    // Pinned trees + bookmarks crowd the panel: still no overlaps.
    s.bookmarks = (0..20).map(|i| format!("test:item_{i:03}")).collect();
    s.pinned_trees = vec![
        ("minecraft:iron_block".to_string(), 4),
        ("minecraft:oak_planks".to_string(), 16),
    ];
    s.bookmark_selected = Some(3);
    let (_, hits) = s.render(w, h);
    assert_no_overlaps(&hits);
}

#[test]
fn tiny_and_huge_frames_never_panic() {
    let s = fixture();
    for &(w, h) in &[(1, 1), (100, 80), (219, 150), (640, 360), (8192, 4096)] {
        let (list, _) = s.render(w, h);
        assert!(!list.is_empty() || w < 220, "{w}x{h}");
        let rgba = rasterize_draw_list(w, h, &list);
        assert_eq!(rgba.len(), (w * h * 4) as usize);
    }
}

#[test]
fn mouse_hover_and_click_on_grid_slot() {
    let (w, h) = (1280, 720);
    let mut s = fixture();
    // Hover updates `hover` state.
    let (_, hits) = s.render(w, h);
    let slot = find(&hits, RecipeWidgetId::ItemSlot(0)).expect("slot 0");
    let (x, y) = (slot.x + 9, slot.y + 9);
    s.handle_input(UiInputEvent::MouseMove { x, y }, w, h);
    assert_eq!(s.hover, Some(RecipeWidgetId::ItemSlot(0)));
    // Click selects the slot.
    s.handle_input(UiInputEvent::Click { x, y }, w, h);
    assert_eq!(s.selected_index(), 0);
    // Enter on a selection opens its recipes.
    let mut s2 = fixture();
    key(&mut s2, w, h, UiKey::Tab);
    s2.handle_input(UiInputEvent::Click { x, y }, w, h);
    key(&mut s2, w, h, UiKey::Enter);
    assert!(s2.detail.is_some());
}

#[test]
fn search_clear_button_and_field_focus() {
    let (w, h) = (1280, 720);
    let mut s = fixture();
    s.search = "iron".into();
    let (_, hits) = s.render(w, h);
    assert!(find(&hits, RecipeWidgetId::SearchClear).is_some());
    click(&mut s, w, h, RecipeWidgetId::SearchClear);
    assert!(s.search.is_empty());
    // Clicking the field focuses; clicking a slot unfocuses.
    click(&mut s, w, h, RecipeWidgetId::SearchField);
    assert!(s.search_focused());
    click(&mut s, w, h, RecipeWidgetId::ItemSlot(0));
    assert!(!s.search_focused());
}

// ---------------------------------------------------------------------------
// EMI query syntax: `|` OR clauses, `-` negation, `%tooltip`, craftable-only
// ---------------------------------------------------------------------------

#[test]
fn emi_or_clauses_negation_and_tooltip_tokens() {
    let mut s = fixture();

    // `|` unions two independent clauses.
    s.search = "cogwheel | gearbox".into();
    let ids = visible_ids(&s);
    assert!(ids.contains(&"create:cogwheel".to_string()));
    assert!(ids.contains(&"create_addon:gearbox".to_string()));
    assert!(!ids.contains(&"minecraft:iron_ingot".to_string()));

    // `-` negates within an AND clause — plain text and @mod forms.
    s.search = "iron -@create".into();
    let ids = visible_ids(&s);
    assert!(ids.contains(&"minecraft:iron_ingot".to_string()));
    assert!(!ids.contains(&"create:iron_sheet".to_string()));
    assert!(!ids.contains(&"create:crushed_iron_ore".to_string()));

    // Negated #tag removes tag matches.
    s.search = "iron -#c:ingots".into();
    let ids = visible_ids(&s);
    assert!(!ids.contains(&"minecraft:iron_ingot".to_string()));
    assert!(ids.contains(&"minecraft:iron_block".to_string()));

    // %tooltip restricts to tooltip-line matches.
    s.search = "%rotation".into();
    assert_eq!(visible_ids(&s), ["create:cogwheel"]);

    // Mixed clause: `(iron AND NOT create) OR cogwheel`.
    s.search = "iron -@create | cogwheel".into();
    let ids = visible_ids(&s);
    assert!(ids.contains(&"create:cogwheel".to_string()));
    assert!(ids.contains(&"minecraft:iron_ingot".to_string()));
    assert!(!ids.contains(&"create:iron_sheet".to_string()));
}

#[test]
fn craftable_only_filters_to_live_inventory() {
    let mut s = fixture();
    s.craftable_only = true;
    // With only 4 oak logs, only `oak_log -> 4 planks` is fully craftable.
    s.inventory.insert("minecraft:oak_log".to_owned(), 4);
    assert_eq!(visible_ids(&s), ["minecraft:oak_planks"]);
    // Stocking iron ore opens the smelting chain's output.
    s.inventory.insert("minecraft:iron_ore".to_owned(), 2);
    let ids = visible_ids(&s);
    assert!(ids.contains(&"minecraft:iron_ingot".to_string()));
    assert!(ids.contains(&"minecraft:oak_planks".to_string()));
    assert!(!ids.contains(&"minecraft:iron_block".to_string())); // needs 9 ingots
}

// ---------------------------------------------------------------------------
// Backspace viewing-history
// ---------------------------------------------------------------------------

#[test]
fn backspace_walks_detail_history_then_closes() {
    let mut s = fixture();
    s.open_detail("minecraft:iron_ingot", DetailMode::Recipes);
    s.open_detail("minecraft:iron_block", DetailMode::Uses);
    assert_eq!(
        s.detail,
        Some(("minecraft:iron_block".to_owned(), DetailMode::Uses))
    );
    s.back_or_close();
    assert_eq!(
        s.detail,
        Some(("minecraft:iron_ingot".to_owned(), DetailMode::Recipes))
    );
    s.back_or_close();
    assert!(s.detail.is_none());
    // Fully unwound: Backspace closes the browser surface.
    let acts = s.back_or_close();
    assert!(acts.iter().any(|a| matches!(a, RecipeBrowserAction::Close)));
}

// ---------------------------------------------------------------------------
// Visual recipe diagrams: 3x3 shaped grid keeps row/col positions
// ---------------------------------------------------------------------------

#[test]
fn shaped_recipe_grid_preserves_row_and_column_positions() {
    let mut idx = RecipeIndex::new();
    idx.items = vec![
        item("test:in_a", "Input A"),
        item("test:in_b", "Input B"),
        item("test:out", "Output"),
    ];
    let mut r = Recipe::new("test:r", RecipeCategory::CraftingShaped);
    r.inputs = vec![
        RecipeIngredient::item("test:in_a", 1),
        RecipeIngredient::item("test:in_b", 1),
    ];
    r.outputs = vec![megu_client_ui::recipe_browser::ItemStack {
        item: "test:out".into(),
        count: 1,
    }];
    // Top-left cell (0) and bottom-right cell (8) — a diagonal that MUST
    // render two rows/two columns apart, never packed side-by-side.
    r.grid = Some(vec![
        Some(0),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(1),
    ]);
    idx.add_recipe(r);
    let mut s = RecipeBrowserState::new(idx);
    s.open_detail("test:out", DetailMode::Recipes);
    let (list, hits) = s.render(1280, 720);

    let slot = |n: usize| {
        hits.iter()
            .find(|h| h.id == RecipeWidgetId::DetailSlot(n))
            .unwrap_or_else(|| panic!("no DetailSlot({n}) region"))
            .rect
    };
    let (a, b) = (slot(0), slot(1));
    // Hit regions are physical pixels: logical 18px slot x ui scale.
    let px = s.effective_scale(720);
    assert_eq!(a.w, 18 * px, "18px ingredient slot");
    // col 0 -> col 2 is two 20px columns; row 0 -> row 2 is two 19px rows.
    assert_eq!(b.x - a.x, 40 * px as i32, "column 0 to column 2");
    assert_eq!(b.y - a.y, 38 * px as i32, "row 0 to row 2");
    // Real ItemIcon commands (not flat color strips) fill the slots.
    let icons: Vec<&UiCommand> = list
        .commands
        .iter()
        .filter(|c| matches!(c, UiCommand::ItemIcon { .. }))
        .collect();
    assert!(
        icons.len() >= 3,
        "two inputs + output slots render icons, got {}",
        icons.len()
    );
}

// ---------------------------------------------------------------------------
// Side overlay: zero overlap with the container panel, None when too narrow
// ---------------------------------------------------------------------------

#[test]
fn side_overlay_never_overlaps_container_panel() {
    let mut s = fixture();
    s.bookmarks = vec!["minecraft:iron_ingot".into()];

    for &(w, h) in &SIZES {
        let scale = s.effective_scale(h) as i32;
        // A chest-panel-sized centered rect, in physical pixels.
        let (pw, ph) = (176 * scale, 166 * scale);
        let panel = UiRect::new(
            (w as i32 - pw) / 2,
            (h as i32 - ph) / 2,
            pw as u32,
            ph as u32,
        );
        let Some((list, hits)) = s.render_overlay(w, h, panel) else {
            // Too narrow is a legal answer — the overlay hides entirely
            // rather than overlap.
            assert!(w <= 640, "{w}x{h} must fit the side panels");
            continue;
        };
        // Hit regions and `panel` are both physical-pixel rects.
        for hreg in &hits {
            assert!(
                !hreg.rect.intersects(panel),
                "{w}x{h}: overlay region {hreg:?} overlaps the container panel"
            );
        }
        assert_no_overlaps(&hits);
        assert_non_blank(&rasterize_draw_list(w, h, &list));
    }

    // A near-fullscreen panel always rejects the overlay outright.
    let big = UiRect::new(2, 2, 1276, 716);
    assert!(s.render_overlay(1280, 720, big).is_none());
    // Minimum-size guard.
    assert!(s
        .render_overlay(200, 100, UiRect::new(0, 0, 8, 8))
        .is_none());
}

// ---------------------------------------------------------------------------
// JER Fuels tab
// ---------------------------------------------------------------------------

#[test]
fn jer_fuels_tab_lists_burn_ticks_and_smelted_counts() {
    let mut s = fixture();
    s.tab = BrowserTab::Jer;
    s.jer_tab = JerTab::Fuels;
    let (list, hits) = s.render(1280, 720);
    let texts: Vec<&str> = list
        .commands
        .iter()
        .filter_map(|c| match c {
            UiCommand::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    // The coal fixture: 1600 ticks = 8 items smelted.
    assert!(
        texts.iter().any(|t| t.contains("1600t") && t.contains("8")),
        "fuel row missing burn/smelted readout: {texts:?}"
    );
    // The Fuels sub-tab button is a live hit region.
    assert!(hits.iter().any(|h| h.id == RecipeWidgetId::JerTabFuels));
}

// ---------------------------------------------------------------------------
// Overlay defaults OFF (`recipe_overlay_enabled` + QoL widget)
// ---------------------------------------------------------------------------

#[test]
fn recipe_overlay_defaults_off_and_qol_widget_toggles() {
    let settings = megu_client_ui::state::QolSettings::default();
    assert!(!settings.recipe_overlay_enabled, "overlay must default OFF");

    // The Settings -> QoL widget flips the same single flag.
    use megu_client_ui::state::{Screen, SettingsTab, UiState, WidgetId};
    let mut ui = UiState::default();
    ui.navigate(Screen::Settings(SettingsTab::Qol));
    assert!(!ui.settings.qol.recipe_overlay_enabled);
    for expected in [true, false] {
        ui.render(1280, 720);
        let rect = ui
            .hit_regions
            .iter()
            .find(|r| r.id == WidgetId::QolRecipeOverlay)
            .expect("QoL tab exposes the recipe overlay toggle")
            .rect;
        ui.handle_input(UiInputEvent::Click {
            x: rect.x + rect.w as i32 / 2,
            y: rect.y + rect.h as i32 / 2,
        });
        assert_eq!(ui.settings.qol.recipe_overlay_enabled, expected);
    }
}
