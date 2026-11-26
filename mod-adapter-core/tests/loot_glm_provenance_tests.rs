// SPDX-License-Identifier: LGPL-3.0-or-later

//! Integration tests for the universal loot overlay, global loot modifier
//! engine, bytecode loot-hook detector, and provenance diff ledger.

use std::collections::{BTreeMap, BTreeSet};

use mod_adapter_core::data_loader::parse_json;
use mod_adapter_core::loot_modifier_engine::{
    detect_bytecode_loot_hooks, GlobalLootModifierEngine, LootDataEntry, LootDataLayer,
    LootDropItem, LootEngineError, LootLayerKind, LootModifierContext, ModLootOverlayIndex,
    MAX_LOOT_JSON_BYTES,
};
use mod_adapter_core::provenance_diff::{
    DiffDomain, ProvenanceAction, ProvenanceDiffLedger, ProvenanceRecord,
};

fn entry(path: &str, json: &str) -> LootDataEntry {
    LootDataEntry::new(path, json.as_bytes().to_vec())
}

fn layer(name: &str, kind: LootLayerKind, entries: Vec<LootDataEntry>) -> LootDataLayer {
    LootDataLayer::new(name, kind, entries)
}

fn add_item_modifier(item: &str) -> String {
    format!(
        r##"{{"type":"neoforge:add_item","item":"{item}","count":1,
        "conditions":[{{"condition":"neoforge:loot_table_id","loot_table_id":"minecraft:chests/*"}}]}}"##
    )
}

fn modifier_file(namespace: &str, name: &str, body: &str) -> LootDataEntry {
    entry(
        &format!("data/{namespace}/loot_modifiers/{name}.json"),
        body,
    )
}

fn neoforge_global(replace: bool, entries: &[&str]) -> LootDataEntry {
    let entries = entries
        .iter()
        .map(|id| format!("\"{id}\""))
        .collect::<Vec<_>>()
        .join(",");
    entry(
        "data/neoforge/loot_modifiers/global_loot_modifiers.json",
        &format!(r##"{{"replace":{replace},"entries":[{entries}]}}"##),
    )
}

fn resolver_empty(_table: &str) -> Vec<LootDropItem> {
    Vec::new()
}

#[test]
fn multi_mod_overlay_priority_and_provenance_conflict_chain() {
    let vanilla = layer(
        "vanilla",
        LootLayerKind::Vanilla,
        vec![
            entry(
                "data/minecraft/loot_tables/chests/simple_dungeon.json",
                r##"{"pools":[{"entries":[{"name":"minecraft:bone"}]}]}"##,
            ),
            entry(
                "data/minecraft/loot_table/entities/zombie.json",
                r##"{"pools":[]}"##,
            ),
        ],
    );
    let mod_a = layer(
        "mod_a",
        LootLayerKind::Mod,
        vec![entry(
            "data/minecraft/loot_tables/chests/simple_dungeon.json",
            r##"{"pools":[{"entries":[{"name":"mod_a:token"}]}]}"##,
        )],
    );
    let datapack = layer(
        "datapack",
        LootLayerKind::Datapack,
        vec![entry(
            "data/minecraft/loot_tables/chests/datapack_only.json",
            r##"{"pools":[{"entries":[{"name":"minecraft:emerald"}]}]}"##,
        )],
    );
    let mod_b = layer(
        "mod_b",
        LootLayerKind::Mod,
        vec![entry(
            "data/minecraft/loot_table/chests/simple_dungeon.json",
            r##"{"pools":[{"entries":[{"name":"mod_b:token"}]}]}"##,
        )],
    );

    // Deliberately shuffled across priority bands. The vanilla base must be
    // ingested first, the two mods must retain their input relative load
    // order (A then B), and the datapack must land last.
    let index = ModLootOverlayIndex::ingest_layers(&[mod_a, datapack, vanilla, mod_b])
        .expect("overlay ingest succeeds");

    let winning = index
        .table("minecraft:chests/simple_dungeon")
        .expect("overlaid table exists");
    assert_eq!(winning.source_layer, "mod_b");
    assert_eq!(winning.previous_owner.as_deref(), Some("mod_a"));
    assert_eq!(
        index
            .table("minecraft:entities/zombie")
            .unwrap()
            .source_layer,
        "vanilla"
    );
    assert_eq!(
        index
            .table("minecraft:chests/datapack_only")
            .unwrap()
            .source_layer,
        "datapack"
    );

    let chain = index
        .provenance()
        .chain(DiffDomain::LootTable, "minecraft:chests/simple_dungeon");
    assert_eq!(
        chain
            .iter()
            .map(|record| (record.source_layer.as_str(), record.action))
            .collect::<Vec<_>>(),
        vec![
            ("vanilla", ProvenanceAction::Added),
            ("mod_a", ProvenanceAction::Overridden),
            ("mod_b", ProvenanceAction::Overridden),
        ]
    );

    let conflicts = index.provenance().conflicts();
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].resource_id, "minecraft:chests/simple_dungeon");
    assert_eq!(conflicts[0].layers, vec!["mod_a", "mod_b"]);

    let badge = index
        .provenance()
        .badge_for(DiffDomain::LootTable, "minecraft:chests/simple_dungeon")
        .expect("badge exists");
    assert_eq!(badge.text, "Overridden: vanilla -> mod_b");
}

#[test]
fn overlay_rejects_oversized_table_json_fail_closed() {
    let oversized = vec![b' '; MAX_LOOT_JSON_BYTES + 1];
    let mod_layer = layer(
        "huge",
        LootLayerKind::Mod,
        vec![LootDataEntry::new(
            "data/huge/loot_tables/too_big.json",
            oversized,
        )],
    );
    assert!(matches!(
        ModLootOverlayIndex::ingest_layers(&[mod_layer]),
        Err(LootEngineError::TooLarge { .. })
    ));
}

#[test]
fn global_modifier_replace_true_resets_and_false_appends_deduplicated() {
    let base = layer(
        "base_mod",
        LootLayerKind::Mod,
        vec![
            neoforge_global(false, &["base:first", "base:second"]),
            modifier_file("base", "first", &add_item_modifier("base:first_item")),
            modifier_file("base", "second", &add_item_modifier("base:second_item")),
        ],
    );
    let appending = layer(
        "append_mod",
        LootLayerKind::Mod,
        vec![
            neoforge_global(false, &["base:first", "append:extra"]),
            modifier_file("append", "extra", &add_item_modifier("append:extra_item")),
        ],
    );
    let append_engine = GlobalLootModifierEngine::ingest_layers(&[base.clone(), appending])
        .expect("append engine loads");
    assert_eq!(
        append_engine.active_order(),
        &["base:first", "base:second", "append:extra"]
    );

    let resetting = layer(
        "reset_mod",
        LootLayerKind::Mod,
        vec![
            neoforge_global(true, &["reset:only"]),
            modifier_file("reset", "only", &add_item_modifier("reset:only_item")),
        ],
    );
    let reset_engine =
        GlobalLootModifierEngine::ingest_layers(&[base, resetting]).expect("replace engine loads");
    assert_eq!(reset_engine.active_order(), &["reset:only"]);
}

#[test]
fn fallback_discovery_registers_after_explicit_entries_with_diagnostic() {
    let fallback_mod = layer(
        "fallback_mod",
        LootLayerKind::Mod,
        vec![modifier_file(
            "fallback",
            "discovered",
            &add_item_modifier("fallback:item"),
        )],
    );
    let explicit_mod = layer(
        "explicit_mod",
        LootLayerKind::Mod,
        vec![
            neoforge_global(false, &["explicit:listed"]),
            modifier_file("explicit", "listed", &add_item_modifier("explicit:item")),
        ],
    );

    let engine = GlobalLootModifierEngine::ingest_layers(&[fallback_mod, explicit_mod])
        .expect("fallback discovery loads");
    assert_eq!(
        engine.active_order(),
        &["explicit:listed", "fallback:discovered"]
    );
    assert_eq!(engine.fallback_order(), &["fallback:discovered"]);
    assert!(engine
        .diagnostics()
        .iter()
        .any(|note| note.contains("fallback discovery") && note.contains("fallback:discovered")));
}

#[test]
fn add_item_count_range_is_deterministic_and_chance_gates() {
    let body = r##"{
        "type":"neoforge:add_item",
        "item":"test:range_item",
        "count":{"min":2,"max":4},
        "chance":1.0,
        "conditions":[{"condition":"neoforge:loot_table_id","loot_table_id":"minecraft:chests/simple_dungeon"}]
    }"##;
    let mod_layer = layer(
        "test",
        LootLayerKind::Mod,
        vec![
            neoforge_global(false, &["test:range"]),
            modifier_file("test", "range", body),
        ],
    );
    let mut engine = GlobalLootModifierEngine::ingest_layers(&[mod_layer]).unwrap();
    let context = LootModifierContext::default();

    let mut first = Vec::new();
    let report_first = engine.apply_loot_modifiers(
        "minecraft:chests/simple_dungeon",
        &context,
        &mut first,
        resolver_empty,
        42,
    );
    let mut second = Vec::new();
    let report_second = engine.apply_loot_modifiers(
        "minecraft:chests/simple_dungeon",
        &context,
        &mut second,
        resolver_empty,
        42,
    );
    assert_eq!(first, second);
    assert_eq!(report_first.fired_modifiers, report_second.fired_modifiers);
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].item, "test:range_item");
    assert!((2..=4).contains(&first[0].count));

    let chance_body = r##"{
        "type":"neoforge:add_item","item":"test:never","count":1,"chance":0.0,
        "conditions":[{"condition":"neoforge:loot_table_id","loot_table_id":"minecraft:chests/*"}]
    }"##;
    let chance_layer = layer(
        "test",
        LootLayerKind::Mod,
        vec![
            neoforge_global(false, &["test:never"]),
            modifier_file("test", "never", chance_body),
        ],
    );
    let mut chance_engine = GlobalLootModifierEngine::ingest_layers(&[chance_layer]).unwrap();
    let mut drops = Vec::new();
    let report = chance_engine.apply_loot_modifiers(
        "minecraft:chests/simple_dungeon",
        &context,
        &mut drops,
        resolver_empty,
        42,
    );
    assert!(drops.is_empty());
    assert_eq!(report.chance_failed_modifiers, vec!["test:never"]);
}

#[test]
fn replace_and_remove_item_mutate_existing_drops() {
    let replace_body = r##"{
        "type":"neoforge:replace_item",
        "target_item":"minecraft:iron_ingot",
        "item":"minecraft:gold_ingot",
        "chance":1.0,
        "conditions":[{"condition":"neoforge:loot_table_id","loot_table_id":"minecraft:chests/*"}]
    }"##;
    let remove_body = r##"{
        "type":"neoforge:remove_item",
        "items":["minecraft:coal","#test:garbage"],
        "conditions":[{"condition":"neoforge:loot_table_id","loot_table_id":"minecraft:chests/*"}]
    }"##;
    let mod_layer = layer(
        "test",
        LootLayerKind::Mod,
        vec![
            neoforge_global(false, &["test:replace", "test:remove"]),
            modifier_file("test", "replace", replace_body),
            modifier_file("test", "remove", remove_body),
        ],
    );
    let mut engine = GlobalLootModifierEngine::ingest_layers(&[mod_layer]).unwrap();
    let mut tags = BTreeMap::new();
    tags.insert(
        "test:garbage".to_string(),
        BTreeSet::from(["minecraft:rotten_flesh".to_string()]),
    );
    let context = LootModifierContext {
        item_tags: tags,
        ..LootModifierContext::default()
    };
    let mut drops = vec![
        LootDropItem::new("minecraft:iron_ingot", 2),
        LootDropItem::new("minecraft:coal", 1),
        LootDropItem::new("minecraft:rotten_flesh", 3),
        LootDropItem::new("minecraft:emerald", 1),
    ];

    let report = engine.apply_loot_modifiers(
        "minecraft:chests/simple_dungeon",
        &context,
        &mut drops,
        resolver_empty,
        7,
    );
    assert_eq!(report.replaced_drops, 1);
    assert_eq!(report.removed_drops, 2);
    assert_eq!(
        drops,
        vec![
            LootDropItem::new("minecraft:gold_ingot", 2),
            LootDropItem::new("minecraft:emerald", 1),
        ]
    );
}

#[test]
fn add_table_recursively_applies_nested_modifiers_and_cycle_guard_fails_closed() {
    let root_add_table = r##"{
        "type":"neoforge:add_table","table":"test:injected",
        "conditions":[{"condition":"neoforge:loot_table_id","loot_table_id":"minecraft:chests/simple_dungeon"}]
    }"##;
    let nested_add_item = r##"{
        "type":"neoforge:add_item","item":"test:berry","count":1,
        "conditions":[{"condition":"neoforge:loot_table_id","loot_table_id":"test:injected"}]
    }"##;
    let mod_layer = layer(
        "test",
        LootLayerKind::Mod,
        vec![
            neoforge_global(false, &["test:root_table", "test:nested_item"]),
            modifier_file("test", "root_table", root_add_table),
            modifier_file("test", "nested_item", nested_add_item),
        ],
    );
    let mut engine = GlobalLootModifierEngine::ingest_layers(&[mod_layer]).unwrap();
    let resolver = |table: &str| {
        if table == "test:injected" {
            vec![LootDropItem::new("minecraft:apple", 1)]
        } else {
            Vec::new()
        }
    };
    let mut drops = Vec::new();
    let report = engine.apply_loot_modifiers(
        "minecraft:chests/simple_dungeon",
        &LootModifierContext::default(),
        &mut drops,
        resolver,
        11,
    );
    let mut resolved = drops
        .iter()
        .map(|drop| (drop.item.as_str(), drop.count))
        .collect::<Vec<_>>();
    resolved.sort();
    assert_eq!(resolved, vec![("minecraft:apple", 1), ("test:berry", 1)]);
    assert!(report
        .fired_modifiers
        .contains(&"test:root_table".to_string()));
    assert!(report
        .fired_modifiers
        .contains(&"test:nested_item".to_string()));

    let cyclic = r##"{
        "type":"neoforge:add_table","table":"minecraft:chests/simple_dungeon",
        "conditions":[{"condition":"neoforge:loot_table_id","loot_table_id":"minecraft:chests/simple_dungeon"}]
    }"##;
    let cycle_layer = layer(
        "cycle",
        LootLayerKind::Mod,
        vec![
            neoforge_global(false, &["cycle:self"]),
            modifier_file("cycle", "self", cyclic),
        ],
    );
    let mut cycle_engine = GlobalLootModifierEngine::ingest_layers(&[cycle_layer]).unwrap();
    let mut cycle_drops = Vec::new();
    let cycle_report = cycle_engine.apply_loot_modifiers(
        "minecraft:chests/simple_dungeon",
        &LootModifierContext::default(),
        &mut cycle_drops,
        resolver_empty,
        11,
    );
    assert!(cycle_drops.is_empty());
    assert_eq!(cycle_report.fail_closed_modifiers, vec!["cycle:self"]);
    assert!(cycle_report
        .diagnostics
        .iter()
        .any(|note| note.contains("depth/cycle guard")));
}

#[test]
fn nested_logical_tool_wildcard_player_and_enchanted_bonus_conditions() {
    let body = r##"{
        "type":"neoforge:add_item","item":"test:conditioned","count":1,
        "conditions":[{"condition":"minecraft:all_of","terms":[
            {"condition":"neoforge:loot_table_id","loot_table_id":"minecraft:chests/*"},
            {"condition":"minecraft:any_of","terms":[
                {"condition":"minecraft:match_tool","predicate":{"items":"#test:tools/shears"}},
                {"condition":"minecraft:killed_by_player"}
            ]},
            {"condition":"minecraft:inverted","term":{"condition":"minecraft:random_chance","chance":0.0}}
        ]}]
    }"##;
    let mod_layer = layer(
        "test",
        LootLayerKind::Mod,
        vec![
            neoforge_global(false, &["test:conditioned"]),
            modifier_file("test", "conditioned", body),
        ],
    );
    let mut engine = GlobalLootModifierEngine::ingest_layers(&[mod_layer]).unwrap();

    let mut tags = BTreeMap::new();
    tags.insert(
        "test:tools/shears".to_string(),
        BTreeSet::from(["minecraft:shears".to_string()]),
    );
    let tool_context = LootModifierContext {
        held_tool: Some("minecraft:shears".to_string()),
        item_tags: tags,
        killed_by_player: false,
        enchanted_bonus_level: 0,
    };
    let mut drops = Vec::new();
    engine.apply_loot_modifiers(
        "minecraft:chests/simple_dungeon",
        &tool_context,
        &mut drops,
        resolver_empty,
        3,
    );
    assert_eq!(drops, vec![LootDropItem::new("test:conditioned", 1)]);

    let player_context = LootModifierContext {
        killed_by_player: true,
        ..LootModifierContext::default()
    };
    let mut player_drops = Vec::new();
    engine.apply_loot_modifiers(
        "minecraft:chests/simple_dungeon",
        &player_context,
        &mut player_drops,
        resolver_empty,
        3,
    );
    assert_eq!(player_drops.len(), 1);

    let mut wrong_table_drops = Vec::new();
    let wrong_table_report = engine.apply_loot_modifiers(
        "minecraft:entities/zombie",
        &player_context,
        &mut wrong_table_drops,
        resolver_empty,
        3,
    );
    assert!(wrong_table_drops.is_empty());
    assert_eq!(
        wrong_table_report.condition_failed_modifiers,
        vec!["test:conditioned"]
    );

    let bonus_body = r##"{
        "type":"neoforge:add_item","item":"test:enchanted","count":1,
        "conditions":[
            {"condition":"neoforge:loot_table_id","loot_table_id":"minecraft:chests/*"},
            {"condition":"minecraft:random_chance_with_enchanted_bonus","unenchanted_chance":0.0,"enchanted_bonus":0.5}
        ]
    }"##;
    let bonus_layer = layer(
        "bonus",
        LootLayerKind::Mod,
        vec![
            neoforge_global(false, &["bonus:enchanted"]),
            modifier_file("bonus", "enchanted", bonus_body),
        ],
    );
    let mut bonus_engine = GlobalLootModifierEngine::ingest_layers(&[bonus_layer]).unwrap();
    let no_bonus = LootModifierContext::default();
    let mut no_bonus_drops = Vec::new();
    bonus_engine.apply_loot_modifiers(
        "minecraft:chests/simple_dungeon",
        &no_bonus,
        &mut no_bonus_drops,
        resolver_empty,
        99,
    );
    assert!(no_bonus_drops.is_empty());

    let full_bonus = LootModifierContext {
        enchanted_bonus_level: 2,
        ..LootModifierContext::default()
    };
    let mut full_bonus_drops = Vec::new();
    bonus_engine.apply_loot_modifiers(
        "minecraft:chests/simple_dungeon",
        &full_bonus,
        &mut full_bonus_drops,
        resolver_empty,
        99,
    );
    assert_eq!(
        full_bonus_drops,
        vec![LootDropItem::new("test:enchanted", 1)]
    );
}

#[test]
fn unknown_conditions_and_actions_fail_closed_with_diagnostics() {
    let unknown_condition = r##"{
        "type":"neoforge:add_item","item":"test:never_condition","count":1,
        "conditions":[{"condition":"minecraft:entity_scores","entity":"this","scores":{}}]
    }"##;
    let unknown_action = r##"{
        "type":"neoforge:smite_all_items",
        "conditions":[{"condition":"neoforge:loot_table_id","loot_table_id":"minecraft:chests/*"}]
    }"##;
    let mod_layer = layer(
        "test",
        LootLayerKind::Mod,
        vec![
            neoforge_global(false, &["test:unknown_condition", "test:unknown_action"]),
            modifier_file("test", "unknown_condition", unknown_condition),
            modifier_file("test", "unknown_action", unknown_action),
        ],
    );
    let mut engine = GlobalLootModifierEngine::ingest_layers(&[mod_layer]).unwrap();
    assert!(engine
        .diagnostics()
        .iter()
        .any(|note| note.contains("unknown or malformed condition")));
    assert!(engine
        .diagnostics()
        .iter()
        .any(|note| note.contains("unsupported type")));

    let mut drops = vec![LootDropItem::new("minecraft:emerald", 1)];
    let report = engine.apply_loot_modifiers(
        "minecraft:chests/simple_dungeon",
        &LootModifierContext::default(),
        &mut drops,
        resolver_empty,
        8,
    );
    assert_eq!(drops, vec![LootDropItem::new("minecraft:emerald", 1)]);
    assert!(report
        .condition_failed_modifiers
        .contains(&"test:unknown_condition".to_string()));
    assert!(report
        .fail_closed_modifiers
        .contains(&"test:unknown_action".to_string()));
}

#[test]
fn bytecode_loot_hook_detector_flags_fabric_and_forge_java_hooks() {
    let fabric_v2 = b"\xca\xfe\xba\xbe net/fabricmc/fabric/api/loot/v2/LootTableEvents".to_vec();
    let fabric_v3 = b"\xca\xfe\xba\xbe net/fabricmc/fabric/api/loot/v3/LootTableEvents".to_vec();
    let forge = b"\xca\xfe\xba\xbe net/neoforged/neoforge/event/loot/LootTableLoadEvent".to_vec();
    let entries: Vec<(&str, &[u8])> = vec![
        ("com/example/FabricV2.class", &fabric_v2),
        ("com/example/FabricV3.class", &fabric_v3),
        ("com/example/Forge.class", &forge),
        ("data/example/not_class.txt", &fabric_v2),
    ];
    let report = detect_bytecode_loot_hooks("hooked_mod", &entries);
    assert_eq!(report.mod_id, "hooked_mod");
    assert!(report.requires_jvm_offload_for_loot);
    assert_eq!(report.hooks_detected.len(), 3);
    assert!(report
        .hooks_detected
        .iter()
        .any(|hook| hook.loader_family == "fabric_loot_table_events_v2"));
    assert!(report
        .hooks_detected
        .iter()
        .any(|hook| hook.loader_family == "fabric_loot_table_events_v3"));
    assert!(report
        .hooks_detected
        .iter()
        .any(|hook| hook.loader_family == "forge_loot_table_load_event"));

    let clean_entries: Vec<(&str, &[u8])> =
        vec![("com/example/Clean.class", b"\xca\xfe\xba\xbe no loot hooks")];
    let clean = detect_bytecode_loot_hooks("clean_mod", &clean_entries);
    assert!(clean.hooks_detected.is_empty());
    assert!(!clean.requires_jvm_offload_for_loot);
}

#[test]
fn reload_diff_summary_counts_exact_new_resources_and_conflicts() {
    let mut previous = ProvenanceDiffLedger::new();
    previous.record(ProvenanceRecord::new(
        DiffDomain::Recipe,
        "minecraft:stonecutting/copper",
        "vanilla",
        ProvenanceAction::Added,
        "vanilla recipe",
        None,
    ));
    previous.record(ProvenanceRecord::new(
        DiffDomain::Tag,
        "c:ores",
        "vanilla",
        ProvenanceAction::Added,
        "vanilla tag",
        None,
    ));

    let mut current = previous.clone();
    current.record(ProvenanceRecord::new(
        DiffDomain::Recipe,
        "farmersdelight:cooking/rice",
        "farmersdelight",
        ProvenanceAction::Added,
        "new cooking recipe",
        None,
    ));
    current.record(ProvenanceRecord::new(
        DiffDomain::Tag,
        "c:tools/knife",
        "farmersdelight",
        ProvenanceAction::Added,
        "new tag",
        None,
    ));
    current.record(ProvenanceRecord::new(
        DiffDomain::Translation,
        "item.farmersdelight.rice",
        "farmersdelight",
        ProvenanceAction::Added,
        "new translation",
        None,
    ));
    current.record(ProvenanceRecord::new(
        DiffDomain::LootTable,
        "minecraft:chests/simple_dungeon",
        "vanilla",
        ProvenanceAction::Added,
        "vanilla chest",
        None,
    ));
    current.record(ProvenanceRecord::new(
        DiffDomain::LootTable,
        "minecraft:chests/simple_dungeon",
        "mod_a",
        ProvenanceAction::Overridden,
        "mod A override",
        Some("vanilla".to_string()),
    ));
    current.record(ProvenanceRecord::new(
        DiffDomain::LootTable,
        "minecraft:chests/simple_dungeon",
        "mod_b",
        ProvenanceAction::Overridden,
        "mod B override",
        Some("mod_a".to_string()),
    ));
    current.record(ProvenanceRecord::new(
        DiffDomain::LootModifier,
        "farmersdelight:add_rice",
        "farmersdelight",
        ProvenanceAction::Added,
        "active GLM rule",
        None,
    ));

    let summary = current.diff_summary_against(&previous);
    assert_eq!(summary.added_recipes, 1);
    assert_eq!(summary.overridden_loot_tables, 1);
    assert_eq!(summary.active_glm_rules, 1);
    assert_eq!(summary.added_tags, 1);
    assert_eq!(summary.added_translations, 1);
    assert_eq!(summary.conflict_count, 1);
    assert!(!summary.is_empty());

    let unchanged = current.diff_summary_against(&current);
    assert!(unchanged.is_empty());

    let json = current.to_json();
    let parsed = parse_json(json.as_bytes()).expect("provenance JSON is valid");
    let object = parsed.as_object().unwrap();
    assert_eq!(
        object.get("schema").and_then(|v| v.as_str()),
        Some("megu-provenance-diff/1")
    );
    assert_eq!(
        object
            .get("conflicts")
            .and_then(|v| v.as_array())
            .map(|conflicts| conflicts.len()),
        Some(1)
    );
}

#[test]
fn glm_application_records_modified_by_glm_provenance_badges() {
    let first = r##"{
        "type":"neoforge:add_item","item":"test:first","count":1,
        "conditions":[{"condition":"neoforge:loot_table_id","loot_table_id":"minecraft:chests/*"}]
    }"##;
    let second = r##"{
        "type":"neoforge:add_item","item":"test:second","count":1,
        "conditions":[{"condition":"neoforge:loot_table_id","loot_table_id":"minecraft:chests/*"}]
    }"##;
    let mod_layer = layer(
        "farmersdelight",
        LootLayerKind::Mod,
        vec![
            neoforge_global(false, &["test:first", "test:second"]),
            modifier_file("test", "first", first),
            modifier_file("test", "second", second),
        ],
    );
    let mut engine = GlobalLootModifierEngine::ingest_layers(&[mod_layer]).unwrap();
    let mut drops = Vec::new();
    engine.apply_loot_modifiers(
        "minecraft:chests/simple_dungeon",
        &LootModifierContext::default(),
        &mut drops,
        resolver_empty,
        5,
    );

    let badge = engine
        .provenance()
        .badge_for(DiffDomain::LootTable, "minecraft:chests/simple_dungeon")
        .expect("GLM badge exists");
    assert_eq!(badge.text, "Modified by farmersdelight (+2 GLM)");
}

#[test]
fn overlay_enforces_per_mod_table_count_bound() {
    let mut entries = Vec::new();
    for index in 0..=4096usize {
        entries.push(entry(
            &format!("data/many/loot_tables/table_{index}.json"),
            r##"{"pools":[]}"##,
        ));
    }
    let layer = layer("many", LootLayerKind::Mod, entries);
    assert!(matches!(
        ModLootOverlayIndex::ingest_layers(&[layer]),
        Err(LootEngineError::TooManyTables { count: 4097, .. })
    ));
}

#[test]
fn add_table_depth_eight_is_fail_closed() {
    let ids = (0..10usize)
        .map(|index| format!("depth:step_{index}"))
        .collect::<Vec<_>>();
    let id_refs = ids.iter().map(String::as_str).collect::<Vec<_>>();
    let mut entries = vec![neoforge_global(false, &id_refs)];
    for index in 0..10usize {
        let body = format!(
            r##"{{"type":"neoforge:add_table","table":"depth:table_{}",
            "conditions":[{{"condition":"neoforge:loot_table_id","loot_table_id":"depth:table_{}"}}]}}"##,
            index + 1,
            index
        );
        entries.push(modifier_file("depth", &format!("step_{index}"), &body));
    }
    let layer = layer("depth", LootLayerKind::Mod, entries);
    let mut engine = GlobalLootModifierEngine::ingest_layers(&[layer]).unwrap();
    let resolver = |table: &str| {
        vec![LootDropItem::new(
            format!("depth:base_{}", table.rsplit(':').next().unwrap_or(table)),
            1,
        )]
    };
    let mut drops = Vec::new();
    let report = engine.apply_loot_modifiers(
        "depth:table_0",
        &LootModifierContext::default(),
        &mut drops,
        resolver,
        17,
    );
    assert_eq!(drops.len(), 8);
    assert!(drops.iter().any(|drop| drop.item == "depth:base_table_8"));
    assert!(!drops.iter().any(|drop| drop.item == "depth:base_table_9"));
    assert!(report
        .fail_closed_modifiers
        .contains(&"depth:step_8".to_string()));
}

#[test]
fn vanilla_linear_enchanted_chance_and_tag_key_match_tool_are_supported() {
    let body = r##"{
        "type":"neoforge:add_item","item":"test:vanilla_bonus","count":1,
        "conditions":[
            {"condition":"neoforge:loot_table_id","loot_table_id":"minecraft:chests/*"},
            {"condition":"minecraft:match_tool","predicate":{"tag":"test:tools/knife"}},
            {"condition":"minecraft:random_chance_with_enchanted_bonus",
             "enchantment":"minecraft:looting",
             "unenchanted_chance":0.0,
             "enchanted_chance":{"type":"minecraft:linear","base":0.0,"per_level_above_first":1.0}}
        ]
    }"##;
    let layer = layer(
        "test",
        LootLayerKind::Mod,
        vec![
            neoforge_global(false, &["test:vanilla_bonus"]),
            modifier_file("test", "vanilla_bonus", body),
        ],
    );
    let mut engine = GlobalLootModifierEngine::ingest_layers(&[layer]).unwrap();
    let mut tags = BTreeMap::new();
    tags.insert(
        "test:tools/knife".to_string(),
        BTreeSet::from(["test:knife".to_string()]),
    );
    let context = |level| LootModifierContext {
        held_tool: Some("test:knife".to_string()),
        item_tags: tags.clone(),
        killed_by_player: false,
        enchanted_bonus_level: level,
    };

    for level in [0, 1] {
        let mut drops = Vec::new();
        engine.apply_loot_modifiers(
            "minecraft:chests/simple_dungeon",
            &context(level),
            &mut drops,
            resolver_empty,
            123,
        );
        assert!(drops.is_empty(), "level {level} must not reach chance 1");
    }
    let mut drops = Vec::new();
    engine.apply_loot_modifiers(
        "minecraft:chests/simple_dungeon",
        &context(2),
        &mut drops,
        resolver_empty,
        123,
    );
    assert_eq!(drops, vec![LootDropItem::new("test:vanilla_bonus", 1)]);
}

#[test]
fn forge_global_list_singular_modifier_directory_and_namespace_wildcard() {
    let entries = r##"{"replace":false,"entries":["forge_mod:singular"]}"##;
    let body = r##"{
        "type":"forge:add_item","item":"forge_mod:added","count":1,
        "conditions":[{"condition":"forge:loot_table_id","loot_table_id":"minecraft:chests/*"}]
    }"##;
    let layer = layer(
        "forge_mod",
        LootLayerKind::Mod,
        vec![
            entry(
                "data/forge/loot_modifiers/global_loot_modifiers.json",
                entries,
            ),
            entry("data/forge_mod/loot_modifier/singular.json", body),
        ],
    );
    let mut engine = GlobalLootModifierEngine::ingest_layers(&[layer]).unwrap();
    assert_eq!(engine.active_order(), &["forge_mod:singular"]);

    let mut drops = Vec::new();
    engine.apply_loot_modifiers(
        "minecraft:chests/simple_dungeon",
        &LootModifierContext::default(),
        &mut drops,
        resolver_empty,
        19,
    );
    assert_eq!(drops, vec![LootDropItem::new("forge_mod:added", 1)]);

    let mut foreign_namespace_drops = Vec::new();
    let report = engine.apply_loot_modifiers(
        "forge_mod:chests/simple_dungeon",
        &LootModifierContext::default(),
        &mut foreign_namespace_drops,
        resolver_empty,
        19,
    );
    assert!(foreign_namespace_drops.is_empty());
    assert_eq!(
        report.condition_failed_modifiers,
        vec!["forge_mod:singular"]
    );
}

#[test]
fn malformed_layer_ingest_is_transactional() {
    let mut index = ModLootOverlayIndex::ingest_layers(&[layer(
        "base",
        LootLayerKind::Mod,
        vec![entry(
            "data/base/loot_tables/kept.json",
            r##"{"pools":[]}"##,
        )],
    )])
    .unwrap();
    let bad_layer = layer(
        "bad",
        LootLayerKind::Mod,
        vec![
            entry("data/bad/loot_tables/new.json", r##"{"pools":[]}"##),
            entry("data/bad/loot_tables/broken.json", "{not json"),
        ],
    );
    assert!(index.ingest_layer(&bad_layer).is_err());
    assert!(index.table("base:kept").is_some());
    assert!(index.table("bad:new").is_none());

    let valid_layer = layer(
        "valid",
        LootLayerKind::Mod,
        vec![
            neoforge_global(false, &["valid:rule"]),
            modifier_file("valid", "rule", &add_item_modifier("valid:item")),
        ],
    );
    let mut engine = GlobalLootModifierEngine::ingest_layers(&[valid_layer]).unwrap();
    let before = engine.active_order().to_vec();
    let malformed_global = layer(
        "malformed",
        LootLayerKind::Mod,
        vec![entry(
            "data/neoforge/loot_modifiers/global_loot_modifiers.json",
            r##"{"replace":"yes","entries":[]}"##,
        )],
    );
    assert!(engine.ingest_ordered_layers(&[malformed_global]).is_err());
    assert_eq!(engine.active_order(), before.as_slice());
}
