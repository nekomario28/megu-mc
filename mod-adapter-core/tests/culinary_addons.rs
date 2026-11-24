// SPDX-License-Identifier: LGPL-3.0-or-later

//! Multi-addon end-to-end tests for [`mod_adapter_core::culinary_adapter`].
//!
//! Three realistic JARs are built entirely in memory — a base
//! `farmersdelight` JAR, an `oceansdelight`-shaped addon, and a
//! deliberately unnamed `unknown_custom_delight` addon that no Rust source
//! file references — proving the engine is 100 % generic. Both 1.21+
//! `recipe/` and legacy `recipes/` directories, both stored and deflated
//! members, both `result.id` and `result.item` spellings, nested `#tag`
//! references, `farmersdelight:item_ability` tool actions, and
//! `*:add_item` / `*:add_table` loot modifiers are exercised.

use mod_adapter_core::culinary_adapter::{
    is_heated, rich_soil_growth_multiplier, tag_contains, CookResult, CookingPotState,
    CulinaryError, CulinaryRegistry,
};

// ---------------------------------------------------------------------------
// In-memory ZIP writer (stored + raw-deflate members, real central directory)
// ---------------------------------------------------------------------------

use megu_core::hash::crc32_ieee as crc32;

/// RFC 1951 raw-deflate stream made of uncompressed blocks (BTYPE=00):
/// a genuine deflate encoding any compliant inflater decodes, letting the
/// fixtures use real method-8 ZIP members with zero external tooling.
fn deflate_stored(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + data.len() / 65_535 * 5 + 5);
    let mut chunks = data.chunks(65_535).peekable();
    let mut wrote = false;
    while let Some(chunk) = chunks.next() {
        wrote = true;
        let last = chunks.peek().is_none();
        out.push(if last { 0x01 } else { 0x00 });
        let len = chunk.len() as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(chunk);
    }
    if !wrote {
        out.extend_from_slice(&[0x01, 0x00, 0x00, 0xff, 0xff]);
    }
    out
}

struct ZipEntry {
    name: &'static str,
    method: u16,
    payload: Vec<u8>,
    uncomp_size: u32,
    crc32: u32,
}

fn stored_entry(name: &'static str, payload: &[u8]) -> ZipEntry {
    ZipEntry {
        name,
        method: 0,
        payload: payload.to_vec(),
        uncomp_size: payload.len() as u32,
        crc32: crc32(payload),
    }
}

fn deflated_entry(name: &'static str, payload: &[u8]) -> ZipEntry {
    ZipEntry {
        name,
        method: 8,
        payload: deflate_stored(payload),
        uncomp_size: payload.len() as u32,
        crc32: crc32(payload),
    }
}

fn build_zip(entries: &[ZipEntry]) -> Vec<u8> {
    fn u16le(out: &mut Vec<u8>, v: u16) {
        out.extend_from_slice(&v.to_le_bytes());
    }
    fn u32le(out: &mut Vec<u8>, v: u32) {
        out.extend_from_slice(&v.to_le_bytes());
    }

    let mut out = Vec::new();
    let mut cd = Vec::new();
    for e in entries {
        let offset = out.len() as u32;
        u32le(&mut out, 0x0403_4b50);
        u16le(&mut out, 20);
        u16le(&mut out, 0);
        u16le(&mut out, e.method);
        u16le(&mut out, 0);
        u16le(&mut out, 0);
        u32le(&mut out, e.crc32);
        u32le(&mut out, e.payload.len() as u32);
        u32le(&mut out, e.uncomp_size);
        u16le(&mut out, e.name.len() as u16);
        u16le(&mut out, 0);
        out.extend_from_slice(e.name.as_bytes());
        out.extend_from_slice(&e.payload);

        u32le(&mut cd, 0x0201_4b50);
        u16le(&mut cd, 20);
        u16le(&mut cd, 20);
        u16le(&mut cd, 0);
        u16le(&mut cd, e.method);
        u16le(&mut cd, 0);
        u16le(&mut cd, 0);
        u32le(&mut cd, e.crc32);
        u32le(&mut cd, e.payload.len() as u32);
        u32le(&mut cd, e.uncomp_size);
        u16le(&mut cd, e.name.len() as u16);
        u16le(&mut cd, 0);
        u16le(&mut cd, 0);
        u16le(&mut cd, 0);
        u16le(&mut cd, 0);
        u32le(&mut cd, 0);
        u32le(&mut cd, offset);
        cd.extend_from_slice(e.name.as_bytes());
    }
    let cd_offset = out.len() as u32;
    out.extend_from_slice(&cd);
    u32le(&mut out, 0x0605_4b50);
    u16le(&mut out, 0);
    u16le(&mut out, 0);
    u16le(&mut out, entries.len() as u16);
    u16le(&mut out, entries.len() as u16);
    u32le(&mut out, cd.len() as u32);
    u32le(&mut out, cd_offset);
    u16le(&mut out, 0);
    out
}

// ---------------------------------------------------------------------------
// Fixture JARs
// ---------------------------------------------------------------------------

/// Base `farmersdelight` JAR: heat/rich-soil block tags, the shared
/// `#c:tools/knife` + `#c:crops/tomato` item tags, a 1.21-`recipe/`-style
/// cooking recipe (deflated member), a 50 %-chance cutting recipe, an
/// `item_ability` cutting recipe under legacy `recipes/`, a `match_tool`
/// loot modifier, a food spec, and one unrelated vanilla recipe that must
/// be skipped cleanly.
fn farmersdelight_jar() -> Vec<u8> {
    let entries = vec![
        stored_entry(
            "data/farmersdelight/tags/block/heat_sources.json",
            br#"{"replace":false,"values":["minecraft:campfire","minecraft:soul_campfire","minecraft:fire","minecraft:soul_fire","minecraft:magma_block","minecraft:lava"]}"#,
        ),
        stored_entry(
            "data/farmersdelight/tags/block/heat_conductors.json",
            br#"{"replace":false,"values":["minecraft:iron_block","farmersdelight:skillet"]}"#,
        ),
        stored_entry(
            "data/farmersdelight/tags/block/rich_soil.json",
            br#"{"replace":false,"values":["farmersdelight:rich_soil","farmersdelight:rich_soil_farmland"]}"#,
        ),
        stored_entry(
            "data/farmersdelight/tags/block/unaffected_by_rich_soil.json",
            br#"{"replace":false,"values":["minecraft:nether_wart"]}"#,
        ),
        stored_entry(
            "data/c/tags/item/tools/knife.json",
            br#"{"replace":false,"values":["farmersdelight:flint_knife","farmersdelight:iron_knife","farmersdelight:golden_knife"]}"#,
        ),
        stored_entry(
            "data/c/tags/item/crops/tomato.json",
            br#"{"replace":false,"values":["minecraft:tomato","farmersdelight:tomato"]}"#,
        ),
        // 1.21+ singular `recipe/` dir inside a *deflated* member.
        deflated_entry(
            "data/farmersdelight/recipe/cooking/beef_stew.json",
            br#"{"type":"farmersdelight:cooking","ingredients":[{"item":"minecraft:beef"},{"item":"minecraft:carrot"},{"tag":"c:crops/tomato"}],"result":{"id":"farmersdelight:beef_stew","count":1},"container":"minecraft:bowl","cookingtime":200,"experience":1.0}"#,
        ),
        // Overlapping tag+item ingredient set for the backtracking proof.
        stored_entry(
            "data/farmersdelight/recipe/cooking/tomato_sauce.json",
            br#"{"type":"farmersdelight:cooking","ingredients":[{"tag":"c:crops/tomato"},{"item":"minecraft:tomato"}],"result":{"id":"farmersdelight:tomato_sauce","count":1},"container":"minecraft:bowl","cookingtime":100,"experience":0.35}"#,
        ),
        stored_entry(
            "data/farmersdelight/recipe/cutting/straw_from_wheat.json",
            br#"{"type":"farmersdelight:cutting","ingredients":[{"item":"minecraft:wheat"}],"tool":{"tag":"c:tools/knife"},"result":[{"item":{"id":"farmersdelight:straw"},"chance":1.0},{"item":{"id":"minecraft:wheat_seeds"},"chance":0.5}],"sound":"farmersdelight:block.cutting_board.knife"}"#,
        ),
        // Legacy `recipes/` dir + `farmersdelight:item_ability` tool action.
        stored_entry(
            "data/farmersdelight/recipes/cutting/bark_from_oak.json",
            br#"{"type":"farmersdelight:cutting","ingredients":[{"item":"minecraft:oak_log"}],"tool":{"type":"farmersdelight:item_ability","action":"axe_strip"},"result":[{"item":"minecraft:stripped_oak_log","count":1},{"item":"farmersdelight:tree_bark","count":1}]}"#,
        ),
        stored_entry(
            "data/farmersdelight/loot_modifiers/straw_from_grass.json",
            br##"{"type":"farmersdelight:add_item","conditions":[{"condition":"neoforge:loot_table_id","loot_table_id":"minecraft:blocks/short_grass"},{"condition":"minecraft:match_tool","predicate":{"items":"#c:tools/knife"}}],"item":"farmersdelight:straw","chance":1.0}"##,
        ),
        stored_entry(
            "data/farmersdelight/food/beef_stew.json",
            br#"{"nutrition":10,"saturation":0.6,"using_converts_to":{"id":"minecraft:bowl"},"effects":[{"id":"farmersdelight:nourishment","duration":3000,"amplifier":0,"probability":1.0}]}"#,
        ),
        // Unrelated vanilla recipe — must be skipped, not an error.
        stored_entry(
            "data/minecraft/recipe/cake.json",
            br#"{"type":"minecraft:crafting_shaped","pattern":["AAA","BEB","CCC"],"key":{"A":{"item":"minecraft:milk_bucket"},"B":{"item":"minecraft:sugar"},"C":{"item":"minecraft:wheat"},"E":{"item":"minecraft:egg"}},"result":{"id":"minecraft:cake","count":1}}"#,
        ),
        // Class file noise real JARs carry — ignored entirely.
        stored_entry("vectorwing/farmersdelight/FarmersDelight.class", &[0xca, 0xfe, 0xba, 0xbe]),
        stored_entry("META-INF/MANIFEST.MF", b"Manifest-Version: 1.0\n"),
    ];
    build_zip(&entries)
}

/// `oceansdelight`-shaped addon JAR: the shared `#c:foods/raw_fish` tag
/// (with a nested sub-tag), a legacy `recipes/` cooking recipe using the
/// base mod's bowl container and legacy `result.item` spelling, and an
/// addon cutting recipe.
fn oceansdelight_jar() -> Vec<u8> {
    let entries = vec![
        stored_entry(
            "data/c/tags/item/foods/raw_fish.json",
            br##"{"replace":false,"values":["minecraft:cod","minecraft:salmon","oceansdelight:raw_guardian","#c:foods/raw_fish_extra"]}"##,
        ),
        stored_entry(
            "data/c/tags/item/foods/raw_fish_extra.json",
            br#"{"replace":false,"values":["oceansdelight:raw_tentacle","oceansdelight:raw_eel"]}"#,
        ),
        stored_entry(
            "data/oceansdelight/recipes/cooking/guardian_soup.json",
            br#"{"type":"oceansdelight:cooking","ingredients":[{"tag":"c:foods/raw_fish"},{"item":"minecraft:dried_kelp"},{"item":"oceansdelight:guardian_tail"}],"result":{"item":"oceansdelight:guardian_soup","count":1},"container":{"item":"minecraft:bowl"},"cookingtime":160,"experience":2.0}"#,
        ),
        stored_entry(
            "data/oceansdelight/recipe/cutting/fugu_slice.json",
            br#"{"type":"oceansdelight:cutting","ingredients":[{"item":"oceansdelight:pufferfish"}],"tool":{"tag":"c:tools/knife"},"result":[{"item":{"id":"oceansdelight:fugu_slice","count":2}}],"sound":"oceansdelight:item.knife.slice"}"#,
        ),
    ];
    build_zip(&entries)
}

/// An addon never named anywhere in Rust code. It contributes a
/// `*:fermenting` recipe (a sibling culinary type), merges a knife into
/// the shared `c:tools/knife` tag across JAR boundaries, and injects a
/// `neoforge:add_table` loot rule — all picked up purely generically.
fn unknown_custom_delight_jar() -> Vec<u8> {
    let entries = vec![
        stored_entry(
            "data/unknown_custom_delight/recipes/fermenting/cider.json",
            br#"{"type":"unknown_custom_delight:fermenting","ingredients":[{"item":"minecraft:apple"},{"item":"minecraft:sugar"}],"result":{"id":"unknown_custom_delight:cider"},"container":"minecraft:glass_bottle","cookingtime":400,"experience":0.5}"#,
        ),
        stored_entry(
            "data/c/tags/item/tools/knife.json",
            br#"{"replace":false,"values":["unknown_custom_delight:obsidian_knife"]}"#,
        ),
        stored_entry(
            "data/unknown_custom_delight/loot_modifiers/cellar_bonus.json",
            br#"{"type":"neoforge:add_table","conditions":[{"condition":"neoforge:loot_table_id","loot_table_id":"minecraft:chests/*"}],"table":"unknown_custom_delight:injected/cellar_loot","chance":1.0}"#,
        ),
    ];
    build_zip(&entries)
}

fn full_registry() -> CulinaryRegistry {
    let mut reg = CulinaryRegistry::new();
    reg.load_from_jar_bytes(&farmersdelight_jar()).unwrap();
    reg.load_from_jar_bytes(&oceansdelight_jar()).unwrap();
    reg.load_from_jar_bytes(&unknown_custom_delight_jar()).unwrap();
    reg
}

fn pot_with(inputs: &[(&str, u16)], container: Option<(&str, u16)>) -> CookingPotState {
    let mut pot = CookingPotState::new();
    for &(item, count) in inputs {
        assert!(pot.insert_input(item, count), "pot slot overflow for {item}");
    }
    pot.container = container.map(|(id, n)| (id.to_string(), n));
    pot
}

fn recipe_index(reg: &CulinaryRegistry, id: &str) -> usize {
    reg.cooking_recipes
        .iter()
        .position(|r| r.id == id)
        .unwrap_or_else(|| panic!("recipe {id} not loaded"))
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

#[test]
fn three_addon_jars_load_into_one_registry() {
    let mut reg = CulinaryRegistry::new();

    let base = reg.load_from_jar_bytes(&farmersdelight_jar()).unwrap();
    assert_eq!(base.cooking_recipes, 2, "beef_stew + tomato_sauce");
    assert_eq!(base.cutting_recipes, 2, "straw_from_wheat + bark_from_oak");
    assert_eq!(base.loot_modifiers, 1);
    assert_eq!(base.food_specs, 1);
    assert!(base.tag_files >= 6, "block + item tags loaded");
    assert_eq!(base.skipped_recipes, 1, "the vanilla cake recipe is skipped");

    let ocean = reg.load_from_jar_bytes(&oceansdelight_jar()).unwrap();
    assert_eq!(ocean.cooking_recipes, 1);
    assert_eq!(ocean.cutting_recipes, 1);

    let unknown = reg.load_from_jar_bytes(&unknown_custom_delight_jar()).unwrap();
    assert_eq!(unknown.cooking_recipes, 1, "generic *:fermenting accepted");
    assert_eq!(unknown.loot_modifiers, 1, "generic neoforge:add_table accepted");

    // Totals across all three JARs.
    assert_eq!(reg.cooking_recipes.len(), 4);
    assert_eq!(reg.cutting_recipes.len(), 3);
    assert_eq!(reg.loot_rules.len(), 2);
    assert_eq!(reg.food_specs.len(), 1);

    // Cross-JAR tag merge: the addon's knife joins the base mod's tag.
    let knife_tag = &reg.tags["c:tools/knife"];
    assert!(knife_tag.contains("farmersdelight:flint_knife"));
    assert!(knife_tag.contains("farmersdelight:iron_knife"));
    assert!(knife_tag.contains("unknown_custom_delight:obsidian_knife"));

    // Nested tag refs resolve (#c:foods/raw_fish → #c:foods/raw_fish_extra).
    assert!(tag_contains(&reg.tags, "c:foods/raw_fish", "minecraft:cod"));
    assert!(tag_contains(&reg.tags, "c:foods/raw_fish", "oceansdelight:raw_eel"));
    assert!(!tag_contains(&reg.tags, "c:foods/raw_fish", "minecraft:beef"));

    // Recipe types and defaults survive the round trip.
    let stew = &reg.cooking_recipes[recipe_index(&reg, "farmersdelight:cooking/beef_stew")];
    assert_eq!(stew.recipe_type, "farmersdelight:cooking");
    assert_eq!(stew.cooking_time, 200);
    assert_eq!(stew.container.as_deref(), Some("minecraft:bowl"));
    let soup = &reg.cooking_recipes[recipe_index(&reg, "oceansdelight:cooking/guardian_soup")];
    assert_eq!(soup.recipe_type, "oceansdelight:cooking");
    assert_eq!(soup.result_item, "oceansdelight:guardian_soup");
    let cider = &reg.cooking_recipes[recipe_index(
        &reg,
        "unknown_custom_delight:fermenting/cider",
    )];
    assert_eq!(cider.recipe_type, "unknown_custom_delight:fermenting");
    assert_eq!(cider.container.as_deref(), Some("minecraft:glass_bottle"));
}

#[test]
fn corrupt_archive_and_malformed_json_fail_closed() {
    let mut reg = CulinaryRegistry::new();
    assert!(matches!(
        reg.load_from_jar_bytes(b"not a jar"),
        Err(CulinaryError::Archive(_))
    ));

    let mut reg = CulinaryRegistry::new();
    let bad = build_zip(&[stored_entry(
        "data/farmersdelight/recipe/broken.json",
        b"{ this is not json",
    )]);
    assert!(matches!(
        reg.load_from_jar_bytes(&bad),
        Err(CulinaryError::Json(_))
    ));
}

#[test]
fn food_spec_data_driven_component() {
    let reg = full_registry();
    let spec = reg.food_spec("farmersdelight:beef_stew").unwrap();
    assert_eq!(spec.nutrition, 10);
    assert!((spec.saturation - 0.6).abs() < 1e-6);
    assert_eq!(spec.using_converts_to.as_deref(), Some("minecraft:bowl"));
    assert_eq!(
        spec.effects,
        vec![("farmersdelight:nourishment".to_string(), 3000u32, 0u8, 1.0f32)]
    );
}

// ---------------------------------------------------------------------------
// Sleeping cooking pot
// ---------------------------------------------------------------------------

#[test]
fn sleeping_pot_consumes_zero_ticks_and_jumps_o1() {
    let reg = full_registry();
    let stew_idx = recipe_index(&reg, "farmersdelight:cooking/beef_stew");

    let mut pot = pot_with(
        &[
            ("minecraft:beef", 2),
            ("minecraft:carrot", 2),
            ("farmersdelight:tomato", 2),
        ],
        Some(("minecraft:bowl", 2)),
    );
    pot.refresh(&reg, 0);

    // Unheated: sleeping, and jumping 10 000 ticks does nothing.
    assert!(pot.is_sleeping());
    assert_eq!(pot.next_completion_tick, None);
    assert_eq!(pot.advance_to(10_000, &reg), None);
    assert_eq!(pot.progress_ticks, 0);

    // Ignite at tick 10 000 — arms at +200 ticks with no per-tick polling.
    pot.heated = true;
    pot.refresh(&reg, 10_000);
    assert!(!pot.is_sleeping());
    assert_eq!(pot.active_recipe, Some(stew_idx));
    assert_eq!(pot.next_completion_tick, Some(10_200));

    // Mid-flight: still no completion, progress bookkeeping only.
    assert_eq!(pot.advance_to(10_199, &reg), None);
    assert_eq!(pot.progress_ticks, 199);

    // One O(1) jump to completion: consume 1× each slot + 1 bowl, emit stew.
    let done = pot.advance_to(10_200, &reg).unwrap();
    assert_eq!(
        done,
        CookResult {
            recipe_id: "farmersdelight:cooking/beef_stew".to_string(),
            result_item: "farmersdelight:beef_stew".to_string(),
            result_count: 1,
            experience: 1.0,
            completed_tick: 10_200,
        }
    );
    assert_eq!(pot.output, Some(("farmersdelight:beef_stew".to_string(), 1)));

    // A second full batch remains → instantly re-armed at 10_400.
    assert_eq!(pot.next_completion_tick, Some(10_400));
    assert!(!pot.is_sleeping());
    let done2 = pot.advance_to(10_400, &reg).unwrap();
    assert_eq!(done2.completed_tick, 10_400);
    assert_eq!(pot.output, Some(("farmersdelight:beef_stew".to_string(), 2)));

    // Everything consumed → back to sleep; container count reached 0.
    assert!(pot.is_sleeping());
    assert_eq!(pot.container, None);
    assert!(pot.inputs.iter().all(|s| s.is_none()));
}

#[test]
fn unheated_pause_banks_progress_and_missing_container_sleeps() {
    let reg = full_registry();
    let stew_idx = recipe_index(&reg, "farmersdelight:cooking/beef_stew");

    // No bowl: the recipe can never arm even fully stocked and heated.
    let mut pot = pot_with(
        &[
            ("minecraft:beef", 1),
            ("minecraft:carrot", 1),
            ("farmersdelight:tomato", 1),
        ],
        None,
    );
    pot.heated = true;
    pot.refresh(&reg, 0);
    assert!(pot.is_sleeping());
    assert_eq!(pot.next_completion_tick, None);

    // Bowl arrives → arms; 100 ticks of progress get banked.
    pot.container = Some(("minecraft:bowl".to_string(), 1));
    pot.refresh(&reg, 5);
    assert_eq!(pot.next_completion_tick, Some(205));
    assert_eq!(pot.advance_to(105, &reg), None);
    assert_eq!(pot.progress_ticks, 100);

    // Heat lost at tick 105 → sleeps; re-ignition at 200 resumes at +100.
    pot.heated = false;
    pot.refresh(&reg, 105);
    assert!(pot.is_sleeping());
    assert_eq!(pot.active_recipe, Some(stew_idx), "recipe kept for resume");
    assert_eq!(pot.advance_to(1_000_000, &reg), None);

    pot.heated = true;
    pot.refresh(&reg, 200);
    assert_eq!(pot.next_completion_tick, Some(300));
    assert!(pot.advance_to(300, &reg).is_some());
    assert_eq!(pot.output, Some(("farmersdelight:beef_stew".to_string(), 1)));
}

#[test]
fn blocked_output_keeps_pot_sleeping() {
    let reg = full_registry();
    let mut pot = pot_with(
        &[
            ("minecraft:beef", 1),
            ("minecraft:carrot", 1),
            ("farmersdelight:tomato", 1),
        ],
        Some(("minecraft:bowl", 1)),
    );
    pot.heated = true;
    // Output slot jammed with a foreign item.
    pot.output = Some(("minecraft:stone".to_string(), 7));
    pot.refresh(&reg, 0);
    assert!(pot.is_sleeping());
    assert_eq!(pot.next_completion_tick, None);
    assert_eq!(pot.advance_to(999_999, &reg), None);
    assert_eq!(pot.output, Some(("minecraft:stone".to_string(), 7)));
}

#[test]
fn generic_addon_fermenting_recipe_runs_in_pot() {
    let reg = full_registry();
    let mut pot = pot_with(
        &[("minecraft:apple", 1), ("minecraft:sugar", 1)],
        Some(("minecraft:glass_bottle", 1)),
    );
    pot.heated = true;
    pot.refresh(&reg, 0);
    let cider_idx = recipe_index(&reg, "unknown_custom_delight:fermenting/cider");
    assert_eq!(pot.active_recipe, Some(cider_idx));
    assert_eq!(pot.next_completion_tick, Some(400));
    let done = pot.advance_to(400, &reg).unwrap();
    assert_eq!(done.result_item, "unknown_custom_delight:cider");
    assert!((done.experience - 0.5).abs() < 1e-6);
    assert_eq!(pot.container, None, "glass bottle consumed");
}

// ---------------------------------------------------------------------------
// Exact-unordered matching (bipartite backtracking)
// ---------------------------------------------------------------------------

#[test]
fn overlapping_tag_and_item_ingredients_match_via_backtracking() {
    let reg = full_registry();
    let sauce_idx = recipe_index(&reg, "farmersdelight:cooking/tomato_sauce");

    // Slot 0 (minecraft:tomato) also lives in #c:crops/tomato: a greedy
    // left-to-right match would burn it on the tag and starve the exact
    // item ingredient — only backtracking finds slot assignment
    // tag→farmersdelight:tomato, item→minecraft:tomato.
    let mut pot = pot_with(
        &[("minecraft:tomato", 1), ("farmersdelight:tomato", 1)],
        Some(("minecraft:bowl", 1)),
    );
    pot.heated = true;
    pot.refresh(&reg, 0);
    assert_eq!(pot.active_recipe, Some(sauce_idx));
    assert_eq!(pot.next_completion_tick, Some(100));

    let done = pot.advance_to(100, &reg).unwrap();
    assert_eq!(done.result_item, "farmersdelight:tomato_sauce");
    assert!(pot.inputs.iter().all(|s| s.is_none()));

    // A single tomato cannot satisfy the two-slot recipe.
    let mut lone = pot_with(&[("minecraft:tomato", 5)], Some(("minecraft:bowl", 1)));
    lone.heated = true;
    lone.refresh(&reg, 0);
    assert!(lone.is_sleeping(), "stack size never counts as extra slots");
}

// ---------------------------------------------------------------------------
// Cutting board
// ---------------------------------------------------------------------------

#[test]
fn cutting_board_deterministic_chance_outputs() {
    let reg = full_registry();

    // Wrong tool → no recipe.
    assert!(reg
        .execute_cutting("minecraft:wheat", "minecraft:stick", 0)
        .is_none());

    // The knife tag resolves through the merged index — including the
    // knife contributed by the unnamed third JAR.
    for knife in [
        "farmersdelight:iron_knife",
        "unknown_custom_delight:obsidian_knife",
    ] {
        let out = reg.execute_cutting("minecraft:wheat", knife, 7).unwrap();
        assert_eq!(out.recipe_id, "farmersdelight:cutting/straw_from_wheat");
        assert!(out
            .outputs
            .contains(&("farmersdelight:straw".to_string(), 1)));
        assert_eq!(
            out.sound.as_deref(),
            Some("farmersdelight:block.cutting_board.knife")
        );
    }

    // The 0.5-chance wheat_seeds roll is deterministic per seed: identical
    // seeds give identical drops, and across seeds both outcomes occur.
    let mut with_seeds = 0usize;
    let mut without_seeds = 0usize;
    for seed in 0..64u64 {
        let a = reg
            .execute_cutting("minecraft:wheat", "farmersdelight:iron_knife", seed)
            .unwrap();
        let b = reg
            .execute_cutting("minecraft:wheat", "farmersdelight:iron_knife", seed)
            .unwrap();
        assert_eq!(a, b, "same seed must replay identically");
        if a.outputs
            .contains(&("minecraft:wheat_seeds".to_string(), 1))
        {
            with_seeds += 1;
        } else {
            without_seeds += 1;
        }
    }
    assert!(with_seeds > 0, "chance roll must sometimes hit");
    assert!(without_seeds > 0, "chance roll must sometimes miss");

    // The addon's own cutting recipe resolves through the same engine.
    let fugu = reg
        .execute_cutting(
            "oceansdelight:pufferfish",
            "farmersdelight:golden_knife",
            42,
        )
        .unwrap();
    assert_eq!(
        fugu.outputs,
        vec![("oceansdelight:fugu_slice".to_string(), 2)]
    );
}

#[test]
fn item_ability_tool_action_matches_tool_family() {
    let reg = full_registry();
    // "axe_strip" → family "axe" → iron axe qualifies; a knife does not.
    let out = reg
        .execute_cutting("minecraft:oak_log", "minecraft:iron_axe", 0)
        .unwrap();
    assert_eq!(
        out.outputs,
        vec![
            ("minecraft:stripped_oak_log".to_string(), 1),
            ("farmersdelight:tree_bark".to_string(), 1),
        ]
    );
    assert!(reg
        .execute_cutting("minecraft:oak_log", "farmersdelight:iron_knife", 0)
        .is_none());
}

// ---------------------------------------------------------------------------
// Loot modifiers, heat, rich soil
// ---------------------------------------------------------------------------

#[test]
fn loot_modifiers_fire_only_with_knife_in_hand() {
    let reg = full_registry();

    // No tool / wrong tool → nothing drops.
    assert!(reg
        .evaluate_loot_modifiers("minecraft:blocks/short_grass", None, 0)
        .is_empty());
    assert!(reg
        .evaluate_loot_modifiers(
            "minecraft:blocks/short_grass",
            Some("minecraft:stone_sword"),
            0,
        )
        .is_empty());
    // Wrong loot table → nothing drops.
    assert!(reg
        .evaluate_loot_modifiers(
            "minecraft:blocks/tall_grass",
            Some("farmersdelight:iron_knife"),
            0,
        )
        .is_empty());

    // Knife in hand on the target table → straw fires.
    let drops = reg.evaluate_loot_modifiers(
        "minecraft:blocks/short_grass",
        Some("farmersdelight:iron_knife"),
        0,
    );
    assert_eq!(drops.len(), 1);
    assert_eq!(drops[0].item.as_deref(), Some("farmersdelight:straw"));
    assert_eq!(drops[0].rule_id, "farmersdelight:straw_from_grass");

    // Wildcard add_table rule (no tool gate) hits any chest table.
    let chest = reg.evaluate_loot_modifiers(
        "minecraft:chests/simple_dungeon",
        None,
        0,
    );
    assert_eq!(chest.len(), 1);
    assert_eq!(
        chest[0].table.as_deref(),
        Some("unknown_custom_delight:injected/cellar_loot")
    );
    // But not a non-chest table.
    assert!(reg
        .evaluate_loot_modifiers("minecraft:entities/zombie", None, 0)
        .is_empty());
}

#[test]
fn heat_sources_conductors_and_stove_states() {
    let reg = full_registry();
    assert!(is_heated("minecraft:campfire", None, &reg.tags));
    assert!(is_heated("minecraft:magma_block", None, &reg.tags));
    assert!(is_heated("farmersdelight:stove[lit=true]", None, &reg.tags));
    assert!(!is_heated("farmersdelight:stove[lit=false]", None, &reg.tags));
    assert!(!is_heated("minecraft:campfire[lit=false]", None, &reg.tags));
    assert!(!is_heated("minecraft:stone", None, &reg.tags));

    // Conductor relays heat from a source one block lower.
    assert!(is_heated(
        "minecraft:iron_block",
        Some("minecraft:campfire"),
        &reg.tags
    ));
    assert!(!is_heated(
        "minecraft:iron_block",
        Some("minecraft:stone"),
        &reg.tags
    ));
    // A non-conductor does not.
    assert!(!is_heated(
        "minecraft:glass",
        Some("minecraft:campfire"),
        &reg.tags
    ));
}

#[test]
fn rich_soil_boosts_growth_unless_unaffected() {
    let reg = full_registry();
    assert_eq!(
        rich_soil_growth_multiplier(
            "farmersdelight:rich_soil",
            "minecraft:wheat[age=3]",
            &reg.tags,
        ),
        2
    );
    assert_eq!(
        rich_soil_growth_multiplier(
            "farmersdelight:rich_soil_farmland",
            "minecraft:nether_wart",
            &reg.tags,
        ),
        1,
        "unaffected crop keeps vanilla speed"
    );
    assert_eq!(
        rich_soil_growth_multiplier("minecraft:dirt", "minecraft:wheat", &reg.tags),
        1
    );
}
