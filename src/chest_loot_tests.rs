use super::*;
use std::collections::BTreeMap;

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
