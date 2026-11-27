use super::*;
use std::collections::BTreeMap;

#[cfg(megu_vanilla_data)]
#[test]
fn native_abandoned_mineshaft_loot_matches_all_19_seeds() {
    let tsv = include_str!("../tests/data/mineshaft_loot_1211.tsv");
    let mut expected: BTreeMap<i64, Vec<LootEntry>> = BTreeMap::new();
    for line in tsv.lines().skip(1).filter(|l| !l.trim().is_empty()) {
        let f: Vec<_> = line.split('\t').collect();
        assert_eq!(f.len(), 6, "bad line: {line:?}");
        let seed: i64 = f[0].parse().unwrap();
        let slot: i16 = f[1].parse().unwrap();
        let item: &'static str = Box::leak(f[2].to_string().into_boxed_str());
        let count: u8 = f[3].parse().unwrap();
        let enchantment = if f[4] == "-" {
            None
        } else {
            let name: &'static str = Box::leak(f[4].to_string().into_boxed_str());
            let level: i32 = f[5].parse().unwrap();
            Some((name, level))
        };
        expected.entry(seed).or_default().push(LootEntry {
            slot,
            item,
            count,
            enchantment,
        });
    }
    assert_eq!(expected.len(), 19);
    for (seed, want) in expected {
        let got = abandoned_mineshaft(seed);
        assert_eq!(got, want, "mismatch for seed {seed}");
    }
}

// ---------------------------------------------------------------------------
// Generic 1.21.1 loot evaluation fixtures. The callbacks stand in for the
// runtime registries so every test below is pure and seed-deterministic.
// ---------------------------------------------------------------------------

fn test_tags(name: &str) -> Vec<&'static str> {
    match name {
        "minecraft:test_tag" => vec![
            "minecraft:stone",
            "minecraft:dirt",
            "minecraft:gravel",
        ],
        _ => vec![],
    }
}

fn test_smelt(item: &str) -> Option<&'static str> {
    match item {
        "minecraft:raw_iron" => Some("minecraft:iron_ingot"),
        "minecraft:raw_beef" => Some("minecraft:cooked_beef"),
        _ => None,
    }
}

fn test_max_damage(item: &str) -> i32 {
    match item {
        "minecraft:diamond_sword" => 1561,
        _ => 0,
    }
}

fn resolvers() -> LootResolvers<'static> {
    LootResolvers {
        tags: &test_tags,
        smelt: &test_smelt,
        max_damage: &test_max_damage,
    }
}

fn item(name: &str, count: i32) -> LootItem {
    LootItem {
        item: name.to_string(),
        count,
        enchantment: None,
        damage: 0,
        potion: None,
        exploration_map: None,
        components: BTreeMap::new(),
    }
}

fn eval_table(json: &str, ctx: &LootContext, seed: u64) -> Vec<LootItem> {
    let table = jparse(json.as_bytes()).expect("valid table JSON");
    let mut rng = Lcg::new(seed);
    let res = resolvers();
    evaluate_loot_table(&table, ctx, &mut rng, &res)
}

fn eval_entry(json: &str, ctx: &LootContext, seed: u64) -> Vec<LootItem> {
    let entry = jparse(json.as_bytes()).expect("valid entry JSON");
    let mut rng = Lcg::new(seed);
    let res = resolvers();
    let mut out = Vec::new();
    expand_entry(&entry, ctx, &mut rng, &res, &mut out);
    out
}

fn eval_condition(json: &str, ctx: &LootContext, seed: u64) -> bool {
    let condition = jparse(json.as_bytes()).expect("valid condition JSON");
    let mut rng = Lcg::new(seed);
    let res = resolvers();
    condition_holds(&condition, ctx, &mut rng, &res)
}

fn apply_function(json: &str, stack: &mut LootItem, ctx: &LootContext, seed: u64) {
    let function = jparse(json.as_bytes()).expect("valid function JSON");
    let mut rng = Lcg::new(seed);
    let res = resolvers();
    apply_one_function(stack, &function, ctx, &mut rng, &res);
}

fn ctx_with_enchantment(id: &str, level: i32) -> LootContext {
    let mut ctx = LootContext::default();
    ctx.enchantments.insert(id.to_string(), level);
    ctx
}

// --- Nested composite entries ------------------------------------------------

#[test]
fn alternatives_picks_first_child_whose_conditions_pass() {
    let json = r#"{
        "type": "minecraft:alternatives",
        "children": [
            {"type":"minecraft:item","name":"minecraft:apple",
             "conditions":[{"condition":"minecraft:killed_by_player"}]},
            {"type":"minecraft:item","name":"minecraft:golden_apple"}
        ]
    }"#;
    assert_eq!(
        eval_entry(json, &LootContext::default(), 1),
        vec![item("minecraft:golden_apple", 1)]
    );
    let ctx = LootContext {
        killed_by_player: true,
        ..Default::default()
    };
    assert_eq!(
        eval_entry(json, &ctx, 1),
        vec![item("minecraft:apple", 1)]
    );
}

#[test]
fn alternatives_stops_even_when_the_winning_child_produces_no_stack() {
    let json = r#"{
        "type": "minecraft:alternatives",
        "children": [
            {"type":"minecraft:item","name":"minecraft:apple",
             "functions":[{"function":"minecraft:set_count","count":0}]},
            {"type":"minecraft:item","name":"minecraft:golden_apple"}
        ]
    }"#;
    assert!(eval_entry(json, &LootContext::default(), 1).is_empty());
}

#[test]
fn group_expands_every_child_that_passes() {
    let json = r#"{
        "type": "minecraft:group",
        "children": [
            {"type":"minecraft:item","name":"minecraft:stone",
             "functions":[{"function":"minecraft:set_count","count":3}]},
            {"type":"minecraft:item","name":"minecraft:dirt",
             "conditions":[{"condition":"minecraft:killed_by_player"}]},
            {"type":"minecraft:item","name":"minecraft:gravel"}
        ]
    }"#;
    assert_eq!(
        eval_entry(json, &LootContext::default(), 1),
        vec![item("minecraft:stone", 3), item("minecraft:gravel", 1)]
    );
    let ctx = LootContext {
        killed_by_player: true,
        ..Default::default()
    };
    assert_eq!(
        eval_entry(json, &ctx, 1),
        vec![
            item("minecraft:stone", 3),
            item("minecraft:dirt", 1),
            item("minecraft:gravel", 1)
        ]
    );
}

#[test]
fn sequence_stops_at_the_first_failing_child() {
    let json = r#"{
        "type": "minecraft:sequence",
        "children": [
            {"type":"minecraft:item","name":"minecraft:stone"},
            {"type":"minecraft:item","name":"minecraft:dirt",
             "conditions":[{"condition":"minecraft:killed_by_player"}]},
            {"type":"minecraft:item","name":"minecraft:gravel"}
        ]
    }"#;
    assert_eq!(
        eval_entry(json, &LootContext::default(), 1),
        vec![item("minecraft:stone", 1)]
    );
    let ctx = LootContext {
        killed_by_player: true,
        ..Default::default()
    };
    assert_eq!(
        eval_entry(json, &ctx, 1),
        vec![
            item("minecraft:stone", 1),
            item("minecraft:dirt", 1),
            item("minecraft:gravel", 1)
        ]
    );
}

#[test]
fn nested_alternatives_and_sequence_compose() {
    let json = r#"{
        "type": "minecraft:alternatives",
        "children": [
            {"type":"minecraft:sequence","children":[
                {"type":"minecraft:item","name":"minecraft:stone",
                 "conditions":[{"condition":"minecraft:killed_by_player"}]},
                {"type":"minecraft:item","name":"minecraft:dirt"}
            ]},
            {"type":"minecraft:item","name":"minecraft:gravel"}
        ]
    }"#;
    assert_eq!(
        eval_entry(json, &LootContext::default(), 1),
        vec![item("minecraft:gravel", 1)]
    );
    let ctx = LootContext {
        killed_by_player: true,
        ..Default::default()
    };
    assert_eq!(
        eval_entry(json, &ctx, 1),
        vec![item("minecraft:stone", 1), item("minecraft:dirt", 1)]
    );
}

// --- Tag entries -------------------------------------------------------------

#[test]
fn tag_entry_expand_true_emits_every_item_with_functions() {
    let json = r#"{
        "type": "minecraft:tag",
        "name": "minecraft:test_tag",
        "expand": true,
        "functions": [{"function":"minecraft:set_count","count":2}]
    }"#;
    assert_eq!(
        eval_entry(json, &LootContext::default(), 1),
        vec![
            item("minecraft:stone", 2),
            item("minecraft:dirt", 2),
            item("minecraft:gravel", 2)
        ]
    );
}

#[test]
fn tag_entry_expand_false_picks_one_random_item() {
    let json = r#"{
        "type": "minecraft:tag",
        "name": "minecraft:test_tag",
        "expand": false
    }"#;
    let names = [
        "minecraft:stone",
        "minecraft:dirt",
        "minecraft:gravel",
    ];
    let mut seen = BTreeMap::new();
    for seed in 0..32 {
        let got = eval_entry(json, &LootContext::default(), seed);
        assert_eq!(got.len(), 1);
        let mut replay = Lcg::new(seed);
        let expected = names[replay.next_bound(3) as usize];
        assert_eq!(got[0].item, expected, "wrong pick for seed {seed}");
        seen.insert(got[0].item.clone(), true);
    }
    assert_eq!(seen.len(), 3, "all tag members should be reachable");
}

#[test]
fn tag_entry_conditions_gate_the_whole_entry() {
    let json = r#"{
        "type": "minecraft:tag",
        "name": "minecraft:test_tag",
        "expand": true,
        "conditions": [{"condition":"minecraft:killed_by_player"}]
    }"#;
    assert!(eval_entry(json, &LootContext::default(), 1).is_empty());
    let ctx = LootContext {
        killed_by_player: true,
        ..Default::default()
    };
    assert_eq!(eval_entry(json, &ctx, 1).len(), 3);
}

#[test]
fn tag_entry_with_unknown_tag_produces_nothing() {
    let json = r#"{"type":"minecraft:tag","name":"minecraft:missing","expand":true}"#;
    assert!(eval_entry(json, &LootContext::default(), 1).is_empty());
}

// --- Conditions --------------------------------------------------------------

#[test]
fn inverted_condition_negates_its_term() {
    let json = r#"{
        "condition":"minecraft:inverted",
        "term":{"condition":"minecraft:killed_by_player"}
    }"#;
    assert!(eval_condition(json, &LootContext::default(), 1));
    let ctx = LootContext {
        killed_by_player: true,
        ..Default::default()
    };
    assert!(!eval_condition(json, &ctx, 1));
}

#[test]
fn any_of_and_all_of_combine_terms() {
    let any = r#"{"condition":"minecraft:any_of","terms":[
        {"condition":"minecraft:killed_by_player"},
        {"condition":"minecraft:entity_properties","entity":"this",
         "predicate":{"flags":{"is_on_fire":true}}}
    ]}"#;
    let all = r#"{"condition":"minecraft:all_of","terms":[
        {"condition":"minecraft:killed_by_player"},
        {"condition":"minecraft:entity_properties","entity":"this",
         "predicate":{"flags":{"is_on_fire":true}}}
    ]}"#;
    let base = LootContext::default();
    let fire = LootContext {
        this_on_fire: true,
        ..Default::default()
    };
    let mut both = fire.clone();
    both.killed_by_player = true;

    assert!(!eval_condition(any, &base, 1));
    assert!(eval_condition(any, &fire, 1));
    assert!(!eval_condition(all, &fire, 1));
    assert!(eval_condition(all, &both, 1));
    assert!(eval_condition(r#"{"condition":"minecraft:all_of","terms":[]}"#, &base, 1));
    assert!(!eval_condition(r#"{"condition":"minecraft:any_of","terms":[]}"#, &base, 1));
}

#[test]
fn random_chance_with_enchanted_bonus_uses_flat_or_linear_chance() {
    let json = r#"{
        "condition":"minecraft:random_chance_with_enchanted_bonus",
        "enchantment":"minecraft:looting",
        "unenchanted_chance":0.0,
        "enchanted_chance":{"type":"minecraft:linear","base":0.25,
                            "per_level_above_first":0.25}
    }"#;
    // Level 0 -> flat 0.0, always false; one RNG draw per evaluation.
    assert!(!eval_condition(json, &LootContext::default(), 7));
    // Level 3 -> 0.25 + 0.25 * 2 = 0.75; compare against a replayed draw.
    let ctx = ctx_with_enchantment("minecraft:looting", 3);
    let mut replay = Lcg::new(7);
    let expected = replay.next_f32() < 0.75;
    assert_eq!(eval_condition(json, &ctx, 7), expected);
    // The bare id form resolves the same enchantment.
    let bare = ctx_with_enchantment("looting", 3);
    assert_eq!(eval_condition(json, &bare, 7), expected);
}

#[test]
fn random_chance_with_enchanted_bonus_enchanted_zero_means_no_drop() {
    let json = r#"{
        "condition":"minecraft:random_chance_with_enchanted_bonus",
        "enchantment":"minecraft:looting",
        "unenchanted_chance":1.0,
        "enchanted_chance":{"type":"minecraft:linear","base":0.0,
                            "per_level_above_first":0.0}
    }"#;
    assert!(eval_condition(json, &LootContext::default(), 3));
    assert!(!eval_condition(json, &ctx_with_enchantment("minecraft:looting", 1), 3));
}

#[test]
fn killed_by_player_condition_reads_context() {
    let json = r#"{"condition":"minecraft:killed_by_player"}"#;
    assert!(!eval_condition(json, &LootContext::default(), 1));
    let ctx = LootContext {
        killed_by_player: true,
        ..Default::default()
    };
    assert!(eval_condition(json, &ctx, 1));
}

#[test]
fn entity_properties_checks_on_fire_and_baby_for_this_and_attacker() {
    let this_fire = r#"{"condition":"minecraft:entity_properties","entity":"this",
        "predicate":{"flags":{"is_on_fire":true}}}"#;
    let this_baby = r#"{"condition":"minecraft:entity_properties","entity":"this",
        "predicate":{"flags":{"is_baby":true}}}"#;
    let attacker_fire = r#"{"condition":"minecraft:entity_properties","entity":"attacker",
        "predicate":{"flags":{"is_on_fire":true}}}"#;
    let not_fire = r#"{"condition":"minecraft:entity_properties","entity":"this",
        "predicate":{"flags":{"is_on_fire":false}}}"#;

    let base = LootContext::default();
    assert!(!eval_condition(this_fire, &base, 1));
    assert!(eval_condition(not_fire, &base, 1));

    let mut ctx = LootContext {
        this_on_fire: true,
        this_is_baby: true,
        ..Default::default()
    };
    assert!(eval_condition(this_fire, &ctx, 1));
    assert!(eval_condition(this_baby, &ctx, 1));
    assert!(!eval_condition(attacker_fire, &ctx, 1));

    ctx.attacker_on_fire = true;
    ctx.attacker_is_baby = true;
    assert!(eval_condition(attacker_fire, &ctx, 1));
}

#[test]
fn survives_explosion_passes_outside_an_explosion() {
    let json = r#"{"condition":"minecraft:survives_explosion"}"#;
    assert!(eval_condition(json, &LootContext::default(), 9));
    // Inside a blast the drop survives at 1/radius odds, one draw.
    let ctx = LootContext {
        explosion_radius: Some(3.0),
        ..Default::default()
    };
    let mut replay = Lcg::new(9);
    let expected = replay.next_f32() <= 1.0 / 3.0;
    assert_eq!(eval_condition(json, &ctx, 9), expected);
    // radius <= 1 puts the threshold at or above the draw range.
    let ctx = LootContext {
        explosion_radius: Some(1.0),
        ..Default::default()
    };
    assert!(eval_condition(json, &ctx, 9));
}

#[test]
fn block_state_property_checks_block_and_properties() {
    let json = r#"{"condition":"minecraft:block_state_property","block":"minecraft:wheat",
        "properties":{"age":"7"}}"#;
    let mut ctx = LootContext {
        block_id: Some("minecraft:wheat".to_string()),
        ..Default::default()
    };
    ctx.block_properties.insert("age".to_string(), "7".to_string());
    assert!(eval_condition(json, &ctx, 1));

    ctx.block_properties.insert("age".to_string(), "6".to_string());
    assert!(!eval_condition(json, &ctx, 1), "a different age fails");

    // A different block, or no block in context, fails closed.
    let mut other = LootContext {
        block_id: Some("minecraft:carrots".to_string()),
        ..Default::default()
    };
    other
        .block_properties
        .insert("age".to_string(), "7".to_string());
    assert!(!eval_condition(json, &other, 1));
    assert!(!eval_condition(json, &LootContext::default(), 1));

    // A value list accepts any listed property value.
    let listed = r#"{"condition":"minecraft:block_state_property","block":"minecraft:oak_slab",
        "properties":{"type":["double","top"]}}"#;
    let mut slab = LootContext {
        block_id: Some("minecraft:oak_slab".to_string()),
        ..Default::default()
    };
    slab.block_properties
        .insert("type".to_string(), "top".to_string());
    assert!(eval_condition(listed, &slab, 1));
    slab.block_properties
        .insert("type".to_string(), "bottom".to_string());
    assert!(!eval_condition(listed, &slab, 1));

    // A property the state does not carry fails closed instead of defaulting.
    let missing = r#"{"condition":"minecraft:block_state_property","block":"minecraft:wheat",
        "properties":{"moisture":"0"}}"#;
    assert!(!eval_condition(missing, &ctx, 1));
}

#[test]
fn match_tool_matches_the_tool_item_and_tags() {
    let shears = r#"{"condition":"minecraft:match_tool","predicate":{"items":"minecraft:shears"}}"#;
    let ctx = LootContext {
        tool_item: Some("minecraft:shears".to_string()),
        ..Default::default()
    };
    assert!(eval_condition(shears, &ctx, 1));

    let bare = LootContext {
        tool_item: Some("shears".to_string()),
        ..Default::default()
    };
    assert!(
        !eval_condition(shears, &bare, 1),
        "an id predicate compares the namespaced id"
    );
    assert!(
        !eval_condition(shears, &LootContext::default(), 1),
        "no tool in context fails closed"
    );

    // `#tag` predicates expand through the caller's tag resolver.
    let tag = r##"{"condition":"minecraft:match_tool","predicate":{"items":"#minecraft:test_tag"}}"##;
    let stone = LootContext {
        tool_item: Some("minecraft:stone".to_string()),
        ..Default::default()
    };
    assert!(eval_condition(tag, &stone, 1));
    let sword = LootContext {
        tool_item: Some("minecraft:iron_sword".to_string()),
        ..Default::default()
    };
    assert!(!eval_condition(tag, &sword, 1));

    // A list is the union of its members, and `count` sees a single-stack tool.
    let listed = r#"{"condition":"minecraft:match_tool","predicate":{"items":
        ["minecraft:iron_sword","minecraft:shears"]}}"#;
    assert!(eval_condition(listed, &sword, 1));
    let counted = r#"{"condition":"minecraft:match_tool","predicate":
        {"items":"minecraft:shears","count":{"min":2}}}"#;
    assert!(!eval_condition(counted, &ctx, 1));
}

#[test]
fn match_tool_reads_the_silk_touch_enchantment_predicate() {
    let json = r#"{"condition":"minecraft:match_tool","predicate":{"predicates":{
        "minecraft:enchantments":[{"enchantments":"minecraft:silk_touch","levels":{"min":1}}]}}}"#;
    assert!(
        !eval_condition(json, &LootContext::default(), 1),
        "an unenchanted tool fails the silk-touch predicate"
    );
    assert!(eval_condition(
        json,
        &ctx_with_enchantment("minecraft:silk_touch", 1),
        1
    ));
    // The bare id spelling of the enchantment resolves the same way.
    assert!(eval_condition(json, &ctx_with_enchantment("silk_touch", 1), 1));

    // Level bounds are honored on both sides.
    let bounded = r#"{"condition":"minecraft:match_tool","predicate":{"predicates":{
        "minecraft:enchantments":[{"enchantments":"minecraft:fortune","levels":{"min":2,"max":2}}]}}}"#;
    assert!(!eval_condition(
        bounded,
        &ctx_with_enchantment("minecraft:fortune", 1),
        1
    ));
    assert!(eval_condition(
        bounded,
        &ctx_with_enchantment("minecraft:fortune", 2),
        1
    ));
    assert!(!eval_condition(
        bounded,
        &ctx_with_enchantment("minecraft:fortune", 3),
        1
    ));
    assert!(!eval_condition(
        bounded,
        &LootContext::default(),
        1
    ));

    // A sub-predicate this module cannot evaluate fails closed.
    let potion = r#"{"condition":"minecraft:match_tool","predicate":{"predicates":{
        "minecraft:potion":[]}}}"#;
    assert!(!eval_condition(potion, &LootContext::default(), 1));
}

#[test]
fn table_bonus_indexes_chances_by_the_fortune_level() {
    let json = r#"{"condition":"minecraft:table_bonus","enchantment":"minecraft:fortune",
        "chances":[0.05,0.0625,0.083333336,0.1]}"#;
    let chances = [0.05f32, 0.0625, 0.083333336, 0.1];
    for level in [0, 1, 2, 3, 4, 9] {
        let ctx = if level == 0 {
            LootContext::default()
        } else {
            ctx_with_enchantment("minecraft:fortune", level)
        };
        let chance = chances[(level as usize).min(chances.len() - 1)];
        let mut replay = Lcg::new(17);
        let expected = replay.next_f32() < chance;
        assert_eq!(
            eval_condition(json, &ctx, 17),
            expected,
            "wrong pass for fortune {level}"
        );
    }

    // A chance of 1.0 still draws once, as the bytecode does.
    let always = r#"{"condition":"minecraft:table_bonus","enchantment":"minecraft:fortune",
        "chances":[1.0]}"#;
    let mut replay = Lcg::new(23);
    let draw = replay.next_f32();
    assert_eq!(eval_condition(always, &LootContext::default(), 23), draw < 1.0);

    // An empty or absent `chances` list fails closed.
    assert!(!eval_condition(
        r#"{"condition":"minecraft:table_bonus","chances":[]}"#,
        &LootContext::default(),
        1
    ));
}

#[test]
fn alternative_is_the_legacy_alias_of_any_of() {
    let json = r#"{"condition":"minecraft:alternative","terms":[
        {"condition":"minecraft:killed_by_player"},
        {"condition":"minecraft:entity_properties","entity":"this",
         "predicate":{"flags":{"is_on_fire":true}}}
    ]}"#;
    assert!(!eval_condition(json, &LootContext::default(), 1));
    let ctx = LootContext {
        this_on_fire: true,
        ..Default::default()
    };
    assert!(eval_condition(json, &ctx, 1));
}

// --- Functions ---------------------------------------------------------------

#[test]
fn set_count_sets_or_adds_a_constant_or_uniform_count() {
    let set = r#"{"function":"minecraft:set_count","count":5}"#;
    let add = r#"{"function":"minecraft:set_count","count":2,"add":true}"#;
    let uniform = r#"{"function":"minecraft:set_count",
        "count":{"type":"minecraft:uniform","min":1,"max":4}}"#;

    let mut stack = item("minecraft:stone", 3);
    apply_function(set, &mut stack, &LootContext::default(), 1);
    assert_eq!(stack.count, 5);
    apply_function(add, &mut stack, &LootContext::default(), 1);
    assert_eq!(stack.count, 7);

    let mut replay = Lcg::new(9);
    let expected = replay.next_bound(4) + 1;
    let mut stack = item("minecraft:stone", 1);
    apply_function(uniform, &mut stack, &LootContext::default(), 9);
    assert_eq!(stack.count, expected);
}

#[test]
fn enchanted_count_increase_requires_an_enchantment_level() {
    let json = r#"{"function":"minecraft:enchanted_count_increase",
        "enchantment":"minecraft:looting",
        "count":{"type":"minecraft:uniform","min":0.0,"max":1.0}}"#;
    let mut stack = item("minecraft:rotten_flesh", 1);
    apply_function(json, &mut stack, &LootContext::default(), 11);
    assert_eq!(stack.count, 1, "level 0 must not draw RNG or grow the stack");

    let ctx = ctx_with_enchantment("minecraft:looting", 2);
    let mut replay = Lcg::new(11);
    let expected = 1 + (2.0 * replay.next_f32()).round() as i32;
    let mut stack = item("minecraft:rotten_flesh", 1);
    apply_function(json, &mut stack, &ctx, 11);
    assert_eq!(stack.count, expected);
}

#[test]
fn enchanted_count_increase_respects_its_limit() {
    let json = r#"{"function":"minecraft:enchanted_count_increase",
        "enchantment":"minecraft:looting","count":10.0,"limit":4}"#;
    let ctx = ctx_with_enchantment("minecraft:looting", 3);
    let mut stack = item("minecraft:rotten_flesh", 1);
    apply_function(json, &mut stack, &ctx, 1);
    assert_eq!(stack.count, 4);
}

#[test]
fn furnace_smelt_uses_the_resolver_and_keeps_the_count() {
    let json = r#"{"function":"minecraft:furnace_smelt"}"#;
    let mut stack = item("minecraft:raw_iron", 3);
    apply_function(json, &mut stack, &LootContext::default(), 1);
    assert_eq!(stack.item, "minecraft:iron_ingot");
    assert_eq!(stack.count, 3);

    let mut stack = item("minecraft:stone", 2);
    apply_function(json, &mut stack, &LootContext::default(), 1);
    assert_eq!(stack.item, "minecraft:stone");
    assert_eq!(stack.count, 2);
}

#[test]
fn set_damage_uses_the_fraction_remaining_semantics() {
    let json = |damage: f64| {
        format!(
            r#"{{"function":"minecraft:set_damage","damage":{damage}}}"#
        )
    };
    let mut stack = item("minecraft:diamond_sword", 1);
    apply_function(&json(1.0), &mut stack, &LootContext::default(), 1);
    assert_eq!(stack.damage, 0, "damage=1.0 means pristine");
    apply_function(&json(0.5), &mut stack, &LootContext::default(), 1);
    assert_eq!(stack.damage, 780, "floor(0.5 * 1561)");
    apply_function(&json(0.0), &mut stack, &LootContext::default(), 1);
    assert_eq!(stack.damage, 1561, "damage=0.0 means fully damaged");

    let add = r#"{"function":"minecraft:set_damage","damage":0.0,"add":true}"#;
    let mut stack = item("minecraft:diamond_sword", 1);
    stack.damage = 0;
    apply_function(add, &mut stack, &LootContext::default(), 1);
    assert_eq!(stack.damage, 0, "adding zero keeps an undamaged stack");

    let mut unknown = item("minecraft:stone", 1);
    apply_function(add, &mut unknown, &LootContext::default(), 1);
    assert_eq!(unknown.damage, 0);
}

#[test]
fn apply_bonus_ore_drops_scales_by_the_enchantment_level() {
    let json = r#"{"function":"minecraft:apply_bonus",
        "enchantment":"minecraft:fortune","formula":"minecraft:ore_drops"}"#;
    let mut stack = item("minecraft:coal", 2);
    apply_function(json, &mut stack, &LootContext::default(), 5);
    assert_eq!(stack.count, 2, "level 0 leaves the count unchanged");

    let ctx = ctx_with_enchantment("minecraft:fortune", 2);
    let mut replay = Lcg::new(5);
    let roll = replay.next_bound(4) - 1;
    let expected = 2 * (roll.max(0) + 1);
    let mut stack = item("minecraft:coal", 2);
    apply_function(json, &mut stack, &ctx, 5);
    assert_eq!(stack.count, expected);
}

#[test]
fn apply_bonus_binomial_with_bonus_count_draws_once_per_bonus_roll() {
    let json = r#"{"function":"minecraft:apply_bonus",
        "enchantment":"minecraft:fortune",
        "formula":"minecraft:binomial_with_bonus_count",
        "parameters":{"extra":1,"probability":0.5}}"#;
    let ctx = ctx_with_enchantment("minecraft:fortune", 2);
    let mut replay = Lcg::new(13);
    let mut expected = 4;
    for _ in 0..3 {
        if replay.next_f32() < 0.5 {
            expected += 1;
        }
    }
    let mut stack = item("minecraft:coal", 4);
    apply_function(json, &mut stack, &ctx, 13);
    assert_eq!(stack.count, expected);
}

#[test]
fn apply_bonus_uniform_bonus_count_adds_one_bounded_draw() {
    let json = r#"{"function":"minecraft:apply_bonus",
        "enchantment":"minecraft:fortune",
        "formula":"minecraft:uniform_bonus_count",
        "parameters":{"bonusMultiplier":2}}"#;
    let ctx = ctx_with_enchantment("minecraft:fortune", 2);
    let mut replay = Lcg::new(17);
    let expected = 1 + replay.next_bound(5);
    let mut stack = item("minecraft:coal", 1);
    apply_function(json, &mut stack, &ctx, 17);
    assert_eq!(stack.count, expected);
}

#[test]
fn function_level_conditions_gate_only_that_function() {
    let functions = r#"[{
        "function":"minecraft:set_count","count":9,
        "conditions":[{"condition":"minecraft:killed_by_player"}]
    }]"#;
    let entry = format!(
        r#"{{"type":"minecraft:item","name":"minecraft:stone","functions":{functions}}}"#
    );
    assert_eq!(
        eval_entry(&entry, &LootContext::default(), 1),
        vec![item("minecraft:stone", 1)]
    );
    let ctx = LootContext {
        killed_by_player: true,
        ..Default::default()
    };
    assert_eq!(
        eval_entry(&entry, &ctx, 1),
        vec![item("minecraft:stone", 9)]
    );
}

#[test]
fn set_name_writes_the_item_name_component() {
    let json = r#"{"function":"minecraft:set_name","target":"item_name",
        "name":{"translate":"filled_map.buried_treasure"}}"#;
    let mut stack = item("minecraft:filled_map", 1);
    apply_function(json, &mut stack, &LootContext::default(), 1);
    assert_eq!(
        stack
            .components
            .get("minecraft:item_name")
            .map(String::as_str),
        Some(r#"{"translate":"filled_map.buried_treasure"}"#)
    );
    assert!(!stack.components.contains_key("minecraft:custom_name"));

    // The vanilla default target is `custom_name`, and a literal name stays a
    // JSON string.
    let default_target = r#"{"function":"minecraft:set_name","name":"Buried Treasure"}"#;
    let mut stack = item("minecraft:filled_map", 1);
    apply_function(default_target, &mut stack, &LootContext::default(), 1);
    assert_eq!(
        stack
            .components
            .get("minecraft:custom_name")
            .map(String::as_str),
        Some(r#""Buried Treasure""#)
    );
}

#[test]
fn set_instrument_picks_a_member_of_the_goat_horn_tag() {
    let json =
        r##"{"function":"minecraft:set_instrument","options":"#minecraft:regular_goat_horns"}"##;
    let mut replay = Lcg::new(5);
    let expected = REGULAR_GOAT_HORNS[replay.next_bound(REGULAR_GOAT_HORNS.len() as i32) as usize];
    let mut stack = item("minecraft:goat_horn", 1);
    apply_function(json, &mut stack, &LootContext::default(), 5);
    assert_eq!(
        stack.components.get("minecraft:instrument").map(String::as_str),
        Some(expected)
    );

    // A direct id resolves without a draw, and an unknown tag resolves to
    // nothing rather than a bogus instrument.
    let direct = r#"{"function":"minecraft:set_instrument","options":"minecraft:ponder_goat_horn"}"#;
    let mut stack = item("minecraft:goat_horn", 1);
    apply_function(direct, &mut stack, &LootContext::default(), 5);
    assert_eq!(
        stack.components.get("minecraft:instrument").map(String::as_str),
        Some("minecraft:ponder_goat_horn")
    );

    let unknown =
        r##"{"function":"minecraft:set_instrument","options":"#minecraft:not_a_horn_tag"}"##;
    let mut stack = item("minecraft:goat_horn", 1);
    apply_function(unknown, &mut stack, &LootContext::default(), 5);
    assert!(stack.components.is_empty());
}

#[test]
fn copy_components_copies_included_block_entity_components() {
    let mut ctx = LootContext::default();
    ctx.source_components
        .insert("minecraft:custom_name".to_string(), r#""Treasury""#.to_string());
    ctx.source_components
        .insert("minecraft:lock".to_string(), r#"{"components":{}}"#.to_string());
    ctx.source_components
        .insert("minecraft:bees".to_string(), "[]".to_string());

    let include = r#"{"function":"minecraft:copy_components","source":"block_entity",
        "include":["minecraft:custom_name","minecraft:lock"]}"#;
    let mut stack = item("minecraft:barrel", 1);
    apply_function(include, &mut stack, &ctx, 1);
    assert_eq!(stack.components.len(), 2);
    assert_eq!(
        stack
            .components
            .get("minecraft:custom_name")
            .map(String::as_str),
        Some(r#""Treasury""#)
    );
    assert!(!stack.components.contains_key("minecraft:bees"));

    // Without `include` everything is copied, minus `exclude`.
    let exclude = r#"{"function":"minecraft:copy_components","exclude":["minecraft:bees"]}"#;
    let mut stack = item("minecraft:barrel", 1);
    apply_function(exclude, &mut stack, &ctx, 1);
    assert_eq!(stack.components.len(), 2);
    assert!(!stack.components.contains_key("minecraft:bees"));

    // A source this evaluator cannot read copies nothing.
    let storage = r#"{"function":"minecraft:copy_components","source":"storage"}"#;
    let mut stack = item("minecraft:barrel", 1);
    apply_function(storage, &mut stack, &ctx, 1);
    assert!(stack.components.is_empty());
}

#[test]
fn explosion_decay_keeps_each_item_of_the_stack_independently() {
    let json = r#"{"function":"minecraft:explosion_decay"}"#;
    // No radius: the stack is untouched and no draw happens.
    let mut stack = item("minecraft:coal", 8);
    apply_function(json, &mut stack, &LootContext::default(), 21);
    assert_eq!(stack.count, 8);

    // radius 2 keeps each item at 1/2 odds, one draw per item.
    let ctx = LootContext {
        explosion_radius: Some(2.0),
        ..Default::default()
    };
    let mut replay = Lcg::new(21);
    let expected = (0..8).filter(|_| replay.next_f32() <= 0.5).count() as i32;
    let mut stack = item("minecraft:coal", 8);
    apply_function(json, &mut stack, &ctx, 21);
    assert_eq!(stack.count, expected);

    // radius 1 puts the threshold at 1.0, so every item survives.
    let ctx = LootContext {
        explosion_radius: Some(1.0),
        ..Default::default()
    };
    let mut stack = item("minecraft:coal", 5);
    apply_function(json, &mut stack, &ctx, 33);
    assert_eq!(stack.count, 5);
}

#[test]
fn set_components_applies_the_component_patch_map() {
    let json = r#"{"function":"minecraft:set_components","components":{
        "minecraft:trim":{"material":"minecraft:copper","pattern":"minecraft:bolt"},
        "minecraft:max_stack_size":16}}"#;
    let mut stack = item("minecraft:diamond_chestplate", 1);
    apply_function(json, &mut stack, &LootContext::default(), 1);
    assert_eq!(
        stack.components.get("minecraft:trim").map(String::as_str),
        Some(r#"{"material":"minecraft:copper","pattern":"minecraft:bolt"}"#)
    );
    assert_eq!(
        stack
            .components
            .get("minecraft:max_stack_size")
            .map(String::as_str),
        Some("16")
    );
}

#[test]
fn limit_count_clamps_into_the_provider_bounds() {
    let both = r#"{"function":"minecraft:limit_count","limit":{"min":1.0,"max":4.0}}"#;
    for (start, want) in [(9, 4), (4, 4), (1, 1), (0, 1)] {
        let mut stack = item("minecraft:glowstone", start);
        apply_function(both, &mut stack, &LootContext::default(), 1);
        assert_eq!(stack.count, want, "start {start}");
    }

    // A missing side leaves that end unbounded.
    let max_only = r#"{"function":"minecraft:limit_count","limit":{"max":9.0}}"#;
    let mut stack = item("minecraft:glowstone", 20);
    apply_function(max_only, &mut stack, &LootContext::default(), 1);
    assert_eq!(stack.count, 9);

    let min_only = r#"{"function":"minecraft:limit_count","limit":{"min":0.0}}"#;
    let mut stack = item("minecraft:glowstone", 3);
    apply_function(min_only, &mut stack, &LootContext::default(), 1);
    assert_eq!(stack.count, 3);

    // The bounds are number providers, not literals.
    let provider = r#"{"function":"minecraft:limit_count","limit":
        {"max":{"type":"minecraft:constant","value":6.0}}}"#;
    let mut stack = item("minecraft:glowstone", 20);
    apply_function(provider, &mut stack, &LootContext::default(), 1);
    assert_eq!(stack.count, 6);
}

#[test]
fn copy_state_carries_the_named_block_properties() {
    let json = r#"{"function":"minecraft:copy_state","block":"minecraft:bee_nest",
        "properties":["honey_level"]}"#;
    let mut ctx = LootContext {
        block_id: Some("minecraft:bee_nest".to_string()),
        ..Default::default()
    };
    ctx.block_properties
        .insert("honey_level".to_string(), "4".to_string());
    ctx.block_properties
        .insert("facing".to_string(), "north".to_string());
    let mut stack = item("minecraft:bee_nest", 1);
    apply_function(json, &mut stack, &ctx, 1);
    assert_eq!(
        stack
            .components
            .get("minecraft:block_state")
            .map(String::as_str),
        Some(r#"{"honey_level":"4"}"#)
    );

    // A different block copies nothing, and neither does a missing block.
    let mut other = ctx.clone();
    other.block_id = Some("minecraft:beehive".to_string());
    let mut stack = item("minecraft:bee_nest", 1);
    apply_function(json, &mut stack, &other, 1);
    assert!(stack.components.is_empty());

    let mut stack = item("minecraft:bee_nest", 1);
    apply_function(json, &mut stack, &LootContext::default(), 1);
    assert!(stack.components.is_empty());

    // A property the state does not carry is skipped, not defaulted.
    let partial = r#"{"function":"minecraft:copy_state","block":"minecraft:bee_nest",
        "properties":["honey_level","missing_property"]}"#;
    let mut stack = item("minecraft:bee_nest", 1);
    apply_function(partial, &mut stack, &ctx, 1);
    assert_eq!(
        stack
            .components
            .get("minecraft:block_state")
            .map(String::as_str),
        Some(r#"{"honey_level":"4"}"#)
    );
}

#[test]
fn max_stack_size_component_caps_the_emitted_count() {
    let mut stack = item("minecraft:diamond", 40);
    stack
        .components
        .insert("minecraft:max_stack_size".to_string(), "16".to_string());
    let mut rng = Lcg::new(4);
    let entries = loot_items_to_entries(vec![stack], &mut rng);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].count, 16);
}

// --- Full-table integration --------------------------------------------------

#[test]
fn table_rolls_weighted_entries_and_applies_pool_functions() {
    let table = r#"{"pools":[{
        "rolls":2,
        "bonus_rolls":1.0,
        "entries":[
            {"type":"minecraft:item","name":"minecraft:stone","weight":3},
            {"type":"minecraft:item","name":"minecraft:dirt","weight":1}
        ],
        "functions":[{"function":"minecraft:set_count","count":5}]
    }]}"#;
    // 2 rolls + floor(1.0 * 2) = 4 draws.
    let ctx = LootContext {
        luck: 2.0,
        ..Default::default()
    };
    let got = eval_table(table, &ctx, 23);
    assert_eq!(got.len(), 4);

    let mut replay = Lcg::new(23);
    let mut expected = Vec::new();
    for _ in 0..4 {
        let pick = replay.next_bound(4);
        let name = if pick < 3 {
            "minecraft:stone"
        } else {
            "minecraft:dirt"
        };
        expected.push(item(name, 5));
    }
    expected.sort_by(|a, b| a.item.cmp(&b.item));
    let mut got = got;
    got.sort_by(|a, b| a.item.cmp(&b.item));
    assert_eq!(got, expected);
}

#[test]
fn table_skips_pools_whose_conditions_fail() {
    let table = r#"{"pools":[
        {"rolls":2,"conditions":[{"condition":"minecraft:killed_by_player"}],
         "entries":[{"type":"minecraft:item","name":"minecraft:apple"}]},
        {"rolls":1,"entries":[{"type":"minecraft:item","name":"minecraft:stone"}]}
    ]}"#;
    assert_eq!(
        eval_table(table, &LootContext::default(), 3),
        vec![item("minecraft:stone", 1)]
    );
    let ctx = LootContext {
        killed_by_player: true,
        ..Default::default()
    };
    assert_eq!(
        eval_table(table, &ctx, 3),
        vec![
            item("minecraft:apple", 1),
            item("minecraft:apple", 1),
            item("minecraft:stone", 1)
        ]
    );
}

#[test]
fn table_quality_scales_entry_weight_with_luck() {
    let table = r#"{"pools":[{
        "rolls":1,
        "entries":[
            {"type":"minecraft:item","name":"minecraft:stone","weight":1,"quality":1},
            {"type":"minecraft:item","name":"minecraft:dirt","weight":1}
        ]
    }]}"#;
    // stone weight = 1 + floor(1 * 3) = 4, dirt = 1.
    let ctx = LootContext {
        luck: 3.0,
        ..Default::default()
    };
    // Every seed that picks below 4 must be stone; verify across many seeds.
    for seed in 0..32 {
        let got = eval_table(table, &ctx, seed);
        assert_eq!(got.len(), 1);
        let mut replay = Lcg::new(seed);
        let expected = if replay.next_bound(5) < 4 {
            "minecraft:stone"
        } else {
            "minecraft:dirt"
        };
        assert_eq!(got[0].item, expected, "wrong pick for seed {seed}");
    }
}

// ---------------------------------------------------------------------------
// Built-in vanilla 1.21.1 tables. The table documents are Mojang data, so this
// tree ships none: every test below resolves them at runtime from the mounted
// vanilla pack or the local 1.21.1 JAR and skips when this checkout has
// neither. Nothing here asserts a table body, only that the evaluator runs over
// whatever the install provides.
// ---------------------------------------------------------------------------

/// Whether this checkout can see vanilla table data at all. `MEGU_VANILLA_JAR`
/// points the loader at a specific JAR; without vanilla data the built-in
/// table tests skip instead of failing on an absent install.
fn vanilla_data_available() -> bool {
    vanilla_loot_table_json("minecraft:chests/simple_dungeon").is_some()
}

fn catalogue_roll(id: &str, seed: i64) -> Vec<LootEntry> {
    vanilla_loot_roll_with_context(id, seed, &LootContext::default())
}

fn assert_valid_entries(entries: &[LootEntry]) {
    for entry in entries {
        assert!(
            (0..27).contains(&entry.slot),
            "slot {} out of range",
            entry.slot
        );
        assert!(entry.item.starts_with("minecraft:"), "bad item {}", entry.item);
        assert!(entry.count > 0, "empty stack for {}", entry.item);
    }
}

#[test]
fn builtin_tables_resolve_from_the_mounted_vanilla_data() {
    if !vanilla_data_available() {
        return;
    }
    for required in [
        "minecraft:chests/ancient_city",
        "minecraft:chests/ancient_city_ice_box",
        "minecraft:chests/bastion_bridge",
        "minecraft:chests/bastion_hoglin_stable",
        "minecraft:chests/bastion_other",
        "minecraft:chests/bastion_treasure",
        "minecraft:chests/trial_chambers/reward",
        "minecraft:chests/trial_chambers/reward_ominous_unique",
        "minecraft:archaeology/desert_pyramid",
        "minecraft:archaeology/desert_well",
        "minecraft:archaeology/ocean_ruin_cold",
        "minecraft:archaeology/ocean_ruin_warm",
        "minecraft:archaeology/trail_ruins_common",
        "minecraft:archaeology/trail_ruins_rare",
        "minecraft:gameplay/cat_morning_gift",
        "minecraft:gameplay/fishing",
        "minecraft:gameplay/fishing/fish",
        "minecraft:gameplay/fishing/junk",
        "minecraft:gameplay/fishing/treasure",
        "minecraft:gameplay/piglin_bartering",
        "minecraft:shearing/bogged",
        "minecraft:entities/sheep/red",
    ] {
        let body = vanilla_loot_table_json(required)
            .unwrap_or_else(|| panic!("{required} did not resolve from the mounted vanilla data"));
        assert!(
            jparse(body.as_bytes()).is_some(),
            "{required} did not parse as JSON"
        );
    }

    // Arbitrary ids and paths that escape the data directory resolve to
    // nothing rather than reaching a filesystem read.
    assert!(vanilla_loot_table_json("minecraft:chests/not_a_real_table").is_none());
    assert!(vanilla_loot_table_json("minecraft:chests/../../secret").is_none());
    assert!(vanilla_loot_table_json("minecraft:/etc/passwd").is_none());
    assert!(vanilla_loot_table_json("example:chests/simple_dungeon").is_none());
}

#[test]
fn datapack_override_shadows_the_builtin_table_body() {
    let _guard = crate::world_guard();
    let key = "minecraft:chests/buried_treasure";
    let body = r#"{"pools":[{"rolls":1,"entries":[{"type":"minecraft:item","name":"minecraft:diamond"}]}]}"#;
    let previous = crate::raw_loot_tables()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key.to_string(), body.to_string());
    assert_eq!(
        loot_table_body(key).as_deref(),
        Some(body),
        "a datapack override must win over the built-in body"
    );
    let entries = catalogue_roll(key, 3);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].item, "minecraft:diamond");
    match previous {
        Some(previous) => {
            crate::raw_loot_tables()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(key.to_string(), previous);
        }
        None => {
            crate::raw_loot_tables()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(key);
        }
    }
}

#[test]
fn v1211_adv_loot_ancient_city_rolls_are_seed_deterministic() {
    if !vanilla_data_available() {
        return;
    }
    for seed in [0i64, 1, 7, 42, 1_000_003] {
        let first = catalogue_roll("minecraft:chests/ancient_city", seed);
        let second = catalogue_roll("minecraft:chests/ancient_city", seed);
        assert_eq!(first, second, "seed {seed} must be deterministic");
        assert!(!first.is_empty(), "ancient_city seed {seed} produced nothing");
        assert_valid_entries(&first);
    }
    // The sculk-table pool is famous for echo shards and enchanted books.
    let mut saw_shard = false;
    let mut saw_book = false;
    for seed in 0..64i64 {
        for entry in catalogue_roll("minecraft:chests/ancient_city", seed) {
            saw_shard |= entry.item == "minecraft:echo_shard";
            saw_book |= entry.item == "minecraft:enchanted_book";
        }
    }
    assert!(saw_shard, "echo_shard never rolled in 64 seeds");
    assert!(saw_book, "enchanted_book never rolled in 64 seeds");
}

#[test]
fn v1211_adv_loot_bastion_tables_roll_deterministic_treasure() {
    if !vanilla_data_available() {
        return;
    }
    for id in [
        "minecraft:chests/bastion_bridge",
        "minecraft:chests/bastion_hoglin_stable",
        "minecraft:chests/bastion_other",
        "minecraft:chests/bastion_treasure",
    ] {
        for seed in [3i64, 99, 4_242] {
            let first = catalogue_roll(id, seed);
            assert_eq!(first, catalogue_roll(id, seed), "{id} seed {seed}");
            assert!(!first.is_empty(), "{id} seed {seed} empty");
            assert_valid_entries(&first);
        }
        // Bastion treasure is the only table carrying netherite scraps/ingots
        // and ancient debris-adjacent rares; sweep seeds for coverage.
        let mut saw_rare = false;
        for seed in 0..96i64 {
            saw_rare |= catalogue_roll(id, seed).iter().any(|e| {
                matches!(
                    e.item,
                    "minecraft:netherite_scrap"
                        | "minecraft:netherite_ingot"
                        | "minecraft:netherite_upgrade_smithing_template"
                )
            });
        }
        if id == "minecraft:chests/bastion_treasure" {
            assert!(saw_rare, "bastion_treasure never rolled a rare across 96 seeds");
        }
    }
}

#[test]
fn v1211_adv_loot_trial_chamber_tables_cover_reward_spectrums() {
    if !vanilla_data_available() {
        return;
    }
    for id in [
        "minecraft:chests/trial_chambers/corridor",
        "minecraft:chests/trial_chambers/entrance",
        "minecraft:chests/trial_chambers/intersection",
        "minecraft:chests/trial_chambers/reward",
        "minecraft:chests/trial_chambers/reward_common",
        "minecraft:chests/trial_chambers/reward_rare",
        "minecraft:chests/trial_chambers/reward_unique",
        "minecraft:chests/trial_chambers/reward_ominous_common",
        "minecraft:chests/trial_chambers/reward_ominous_rare",
        "minecraft:chests/trial_chambers/reward_ominous_unique",
        "minecraft:chests/trial_chambers/supply",
        "minecraft:pots/trial_chambers/corridor",
        "minecraft:dispensers/trial_chambers/corridor",
        "minecraft:spawners/trial_chamber/key",
        "minecraft:spawners/trial_chamber/consumables",
    ] {
        for seed in [11i64, 777] {
            let first = catalogue_roll(id, seed);
            assert_eq!(first, catalogue_roll(id, seed), "{id} seed {seed}");
            assert!(!first.is_empty(), "{id} seed {seed} produced nothing");
            assert_valid_entries(&first);
        }
    }
    // Equipment tables gate every armor piece behind independent 50% pools, so
    // a single roll may legitimately be empty; across seeds they must produce
    // real armor.
    for id in [
        "minecraft:equipment/trial_chamber",
        "minecraft:equipment/trial_chamber_melee",
        "minecraft:equipment/trial_chamber_ranged",
    ] {
        for seed in [11i64, 777] {
            let first = catalogue_roll(id, seed);
            assert_eq!(first, catalogue_roll(id, seed), "{id} seed {seed}");
            assert_valid_entries(&first);
        }
        let mut saw_armor = false;
        for seed in 0..64i64 {
            saw_armor |= catalogue_roll(id, seed).iter().any(|e| {
                e.item.ends_with("_helmet")
                    || e.item.ends_with("_chestplate")
                    || e.item.ends_with("_leggings")
                    || e.item.ends_with("_boots")
            });
        }
        assert!(saw_armor, "{id} never produced armor across 64 seeds");
    }
    // reward_ominous_unique is the heavy hitter: breeze rod / heavy core.
    let mut saw_unique = false;
    for seed in 0..128i64 {
        saw_unique |= catalogue_roll("minecraft:chests/trial_chambers/reward_ominous_unique", seed)
            .iter()
            .any(|e| {
                matches!(
                    e.item,
                    "minecraft:breeze_rod" | "minecraft:heavy_core" | "minecraft:wind_charge"
                )
            });
    }
    assert!(
        saw_unique,
        "ominous_unique never produced its signature drops"
    );
}

#[test]
fn v1211_adv_loot_archaeology_tables_roll_sherds_and_pottery() {
    if !vanilla_data_available() {
        return;
    }
    for id in [
        "minecraft:archaeology/desert_pyramid",
        "minecraft:archaeology/desert_well",
        "minecraft:archaeology/ocean_ruin_cold",
        "minecraft:archaeology/ocean_ruin_warm",
        "minecraft:archaeology/trail_ruins_common",
        "minecraft:archaeology/trail_ruins_rare",
    ] {
        for seed in [5i64, 31337] {
            let first = catalogue_roll(id, seed);
            assert_eq!(first, catalogue_roll(id, seed), "{id} seed {seed}");
            assert!(!first.is_empty(), "{id} seed {seed} produced nothing");
            assert_valid_entries(&first);
        }
    }
    // Desert pyramid brushing yields pottery sherds or precious items.
    let mut saw_sherd = false;
    for seed in 0..64i64 {
        saw_sherd |= catalogue_roll("minecraft:archaeology/desert_pyramid", seed)
            .iter()
            .any(|e| e.item.contains("sherd"));
    }
    assert!(saw_sherd, "desert_pyramid never produced sherds");
}

#[test]
fn v1211_adv_loot_nested_entity_tables_resolve_through_the_loader() {
    if !vanilla_data_available() {
        return;
    }
    // entities/sheep/<color> rolls its own wool plus the shared mutton table,
    // which only resolves when a nested `loot_table` entry reaches the loader.
    for color in ["red", "blue", "white"] {
        let entries = catalogue_roll(&format!("minecraft:entities/sheep/{color}"), 21);
        assert_eq!(
            entries,
            catalogue_roll(&format!("minecraft:entities/sheep/{color}"), 21)
        );
        assert!(
            entries
                .iter()
                .any(|e| e.item == format!("minecraft:{color}_wool")),
            "{color} sheep missing {color}_wool: {entries:?}"
        );
        assert!(
            entries.iter().any(|e| e.item == "minecraft:mutton"),
            "{color} sheep missing nested mutton drop: {entries:?}"
        );
    }
    // Creeper drops gunpowder unconditionally; the skeleton-only music-disc
    // pool stays closed without attacker context.
    let creeper = catalogue_roll("minecraft:entities/creeper", 9);
    assert!(creeper.iter().any(|e| e.item == "minecraft:gunpowder"));
    assert!(!creeper
        .iter()
        .any(|e| e.item.starts_with("minecraft:music_disc_")));
}

#[test]
fn v1211_adv_loot_gameplay_and_shearing_tables_are_reachable() {
    if !vanilla_data_available() {
        return;
    }
    for id in [
        "minecraft:gameplay/cat_morning_gift",
        "minecraft:gameplay/piglin_bartering",
        "minecraft:gameplay/sniffer_digging",
        "minecraft:gameplay/fishing/fish",
        "minecraft:gameplay/fishing/junk",
        "minecraft:gameplay/fishing/treasure",
        "minecraft:shearing/bogged",
        "minecraft:gameplay/hero_of_the_village/armorer_gift",
        "minecraft:gameplay/hero_of_the_village/weaponsmith_gift",
    ] {
        let first = catalogue_roll(id, 17);
        assert_eq!(first, catalogue_roll(id, 17), "{id} not deterministic");
        assert!(!first.is_empty(), "{id} produced nothing");
        assert_valid_entries(&first);
    }
    // panda_sneeze is a gift table whose slime_ball sits at weight 1 against a
    // weight-699 empty entry, so a single roll is empty almost every time; over
    // enough deterministic seeds the rare drop must still surface.
    let mut saw_slime = false;
    for seed in 0..8192i64 {
        saw_slime |= catalogue_roll("minecraft:gameplay/panda_sneeze", seed)
            .iter()
            .any(|e| e.item == "minecraft:slime_ball");
        if saw_slime {
            break;
        }
    }
    assert!(saw_slime, "panda_sneeze never produced slime_ball");
    // Piglin bartering is the enchanted-gear-and-rare-mats table.
    let mut saw_barter_good = false;
    for seed in 0..64i64 {
        saw_barter_good |= catalogue_roll("minecraft:gameplay/piglin_bartering", seed)
            .iter()
            .any(|e| {
                matches!(
                    e.item,
                    "minecraft:obsidian"
                        | "minecraft:fire_charge"
                        | "minecraft:blackstone"
                        | "minecraft:spectral_arrow"
                        | "minecraft:ender_pearl"
                )
            });
    }
    assert!(
        saw_barter_good,
        "piglin_bartering never produced its rare drops across 64 seeds"
    );
}
