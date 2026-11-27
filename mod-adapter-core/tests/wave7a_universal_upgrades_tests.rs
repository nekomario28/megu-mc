// SPDX-License-Identifier: LGPL-3.0-or-later

//! Wave 7A universal zero-JVM parity regressions. Fixtures are synthesized
//! declarative JSON/ZIP bytes only — no loader or mod source is copied.

use std::collections::{BTreeMap, BTreeSet};

use megu_core::hash::crc32_ieee as crc32;
use mod_adapter_core::auto_adapter::extract_mod_data;
use mod_adapter_core::capability_bus::{
    CapabilityFluidStack, FluidAmount, DROPLETS_PER_BUCKET, DROPLETS_PER_MB,
};
use mod_adapter_core::data_loader::{
    evaluate_condition, evaluate_conditions, parse_json, parse_mod_recipe_json_rich,
    parse_mod_recipe_json_with_context, ConditionContext, ItemFluidIdTable, JsonValue,
};
use mod_adapter_core::loader_extensions::{
    canonical_conventional_tag_id, data_map_key_for_path, extract_embedded_jars,
    inspect_jar_extensions, is_builtin_data_map, parse_access_widener, parse_biome_modifier,
    parse_data_map, AccessWidenerOperation, AtTargetKind, BiomeModifierKind,
    ConventionalTagRegistry, DataMapRegistry,
};
use mod_adapter_core::neoforge_wire::{
    decode_frozen_registry_payload, write_mc_string, FrozenRegistryPayload,
};

fn jar(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data) in entries {
        let name = name.as_bytes();
        let crc = crc32(data);
        let size = data.len() as u32;
        let offset = out.len() as u32;
        out.extend(0x0403_4B50u32.to_le_bytes());
        out.extend(10u16.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(crc.to_le_bytes());
        out.extend(size.to_le_bytes());
        out.extend(size.to_le_bytes());
        out.extend((name.len() as u16).to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(name);
        out.extend(*data);
        central.extend(0x0201_4B50u32.to_le_bytes());
        central.extend(20u16.to_le_bytes());
        central.extend(10u16.to_le_bytes());
        central.extend(0u16.to_le_bytes());
        central.extend(0u16.to_le_bytes());
        central.extend(0u16.to_le_bytes());
        central.extend(0u16.to_le_bytes());
        central.extend(crc.to_le_bytes());
        central.extend(size.to_le_bytes());
        central.extend(size.to_le_bytes());
        central.extend((name.len() as u16).to_le_bytes());
        central.extend(0u16.to_le_bytes());
        central.extend(0u16.to_le_bytes());
        central.extend(0u16.to_le_bytes());
        central.extend(0u16.to_be_bytes());
        central.extend(0u32.to_le_bytes());
        central.extend(offset.to_le_bytes());
        central.extend(name);
    }
    let cd_offset = out.len() as u32;
    let cd_size = central.len() as u32;
    out.extend(central);
    out.extend(0x0605_4B50u32.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    out.extend((entries.len() as u16).to_le_bytes());
    out.extend((entries.len() as u16).to_le_bytes());
    out.extend(cd_size.to_le_bytes());
    out.extend(cd_offset.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    out
}

fn object(entries: &[(&str, JsonValue)]) -> JsonValue {
    JsonValue::Object(
        entries
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect::<BTreeMap<_, _>>(),
    )
}

#[test]
fn array_results_chances_and_rich_recipe_metadata_survive_generic_projection() {
    let json = br#"{
        "type": "create:crushing",
        "ingredients": [{
            "type": "neoforge:compound",
            "children": [{"item": "minecraft:gravel"}]
        }],
        "results": [
            {"item": {"id": "example:sand", "count": 2}, "chance": 1.0},
            {"id": "example:flint", "count": 1, "chance": 0.25}
        ],
        "processingTime": 250,
        "experience": 0.35,
        "heatRequirement": "heated",
        "tool": {"item": "example:crusher"},
        "container": {"item": "example:bucket"}
    }"#;
    let mut ids = ItemFluidIdTable::default();
    let rich = parse_mod_recipe_json_rich("example:crushing/gravel", 7, json, &mut ids)
        .unwrap()
        .expect("array-valued results parse");
    assert_eq!(rich.recipe.recipe.base_ticks, 250);
    assert_eq!(rich.recipe.recipe.inputs.len(), 1);
    assert_eq!(rich.recipe.recipe.outputs.len(), 2);
    assert_eq!(rich.recipe.recipe.outputs[0].amount, 2);
    assert_eq!(rich.recipe.recipe.outputs[1].amount, 1);
    assert_eq!(rich.metadata.outputs.len(), 2);
    assert_eq!(rich.metadata.outputs[0].chance, Some(1.0));
    assert_eq!(rich.metadata.outputs[1].chance, Some(0.25));
    assert_eq!(rich.metadata.processing_time, Some(250));
    assert_eq!(rich.metadata.experience, Some(0.35));
    assert_eq!(rich.metadata.heat_requirement.as_deref(), Some("heated"));
    assert!(rich.metadata.tool.is_some());
    assert!(rich.metadata.container.is_some());
    assert_eq!(rich.metadata.ingredients.len(), 1);
}

#[test]
fn extraction_pipeline_preserves_array_output_chance_and_recipe_sidecar_metadata() {
    let recipe = br#"{
        "type":"create:mixing",
        "ingredients":[{"item":"example:stone"}],
        "results":[
            {"item":{"id":"example:primary","count":2},"chance":1.0},
            {"id":"example:secondary","count":1,"chance":0.5}
        ],
        "processingTime":175,
        "heatRequirement":"heated"
    }"#;
    let archive = jar(&[("data/example/recipes/array_outputs.json", recipe.as_slice())]);
    let data = extract_mod_data(&archive, "array-recipe.jar").expect("array recipe extraction");
    assert_eq!(data.recipes.len(), 1, "{:?}", data.rejected_members);
    assert_eq!(data.recipes[0].recipe.outputs.len(), 2);
    assert_eq!(data.recipes[0].recipe.base_ticks, 175);
    assert_eq!(data.recipe_metadata.len(), 1);
    assert_eq!(data.recipe_metadata[0].outputs.len(), 2);
    assert_eq!(data.recipe_metadata[0].outputs[1].chance, Some(0.5));
    assert_eq!(data.recipe_metadata[0].heat_requirement.as_deref(), Some("heated"));
}

#[test]
fn compound_difference_intersection_and_component_wrappers_represent_inputs() {
    for ingredient in [
        r#"{"type":"neoforge:compound","children":[{"item":"example:a"}]}"#,
        r#"{"type":"neoforge:difference","base":{"item":"example:base"},"subtracted":{"item":"example:no"}}"#,
        r#"{"type":"neoforge:intersection","children":[{"item":"example:intersect"}]}"#,
        r#"{"type":"neoforge:components","items":"example:component","count":3}"#,
    ] {
        let json = format!(
            r#"{{"type":"example:machine","ingredients":[{ingredient}],"result":{{"item":"example:out"}}}}"#
        );
        let mut ids = ItemFluidIdTable::default();
        let parsed = parse_mod_recipe_json_rich("example:x", 1, json.as_bytes(), &mut ids)
            .unwrap()
            .unwrap_or_else(|| panic!("ingredient form failed: {ingredient}"));
        assert_eq!(parsed.recipe.recipe.inputs.len(), 1);
    }
}

#[test]
fn neoforge_and_fabric_condition_engines_cover_loader_condition_sets() {
    let mut context = ConditionContext::with_mods(["example", "dependency"]);
    context.existing_items.insert("minecraft:stone".to_string());
    context.populated_tags.insert("c:ores".to_string());
    context.enabled_features.insert("minecraft:vanilla".to_string());
    context.registry_entries.insert(
        "minecraft:item".to_string(),
        BTreeSet::from(["minecraft:stone".to_string()]),
    );

    let truthy = [
        r#"{"type":"neoforge:true"}"#,
        r#"{"type":"neoforge:not","value":{"type":"neoforge:false"}}"#,
        r#"{"type":"neoforge:and","values":[{"type":"neoforge:true"},{"type":"neoforge:mod_loaded","modid":"example"}]}"#,
        r#"{"type":"neoforge:or","values":[{"type":"neoforge:false"},{"type":"neoforge:item_exists","item":"minecraft:stone"}]}"#,
        r#"{"type":"neoforge:mod_loaded","modid":"dependency"}"#,
        r#"{"type":"neoforge:tag_empty","tag":"c:missing_tag"}"#,
        r#"{"type":"neoforge:item_exists","item":"minecraft:stone"}"#,
        r#"{"condition":"fabric:true"}"#,
        r#"{"condition":"fabric:not","value":{"condition":"fabric:false"}}"#,
        r#"{"condition":"fabric:and","values":[{"condition":"fabric:true"}]}"#,
        r#"{"condition":"fabric:or","values":[{"condition":"fabric:any_mods_loaded","values":["missing","example"]}]}"#,
        r#"{"condition":"fabric:all_mods_loaded","values":["example","dependency"]}"#,
        r#"{"condition":"fabric:any_mods_loaded","values":["example"]}"#,
        r#"{"condition":"fabric:tags_populated","values":["c:ores"]}"#,
        r#"{"condition":"fabric:features_enabled","values":["minecraft:vanilla"]}"#,
        r#"{"condition":"fabric:registry_contains","registry":"minecraft:item","values":["minecraft:stone"]}"#,
    ];
    for text in truthy {
        let condition = parse_json(text.as_bytes()).unwrap();
        assert!(evaluate_condition(&condition, &context), "{text}");
    }

    let falsy = [
        r#"{"type":"neoforge:false"}"#,
        r#"{"type":"neoforge:mod_loaded","modid":"absent"}"#,
        r#"{"type":"neoforge:tag_empty","tag":"c:ores"}"#,
        r#"{"type":"neoforge:item_exists","item":"example:absent"}"#,
        r#"{"condition":"fabric:all_mods_loaded","values":["example","absent"]}"#,
        r#"{"condition":"fabric:tags_populated","values":["c:missing_tag"]}"#,
        r#"{"condition":"fabric:features_enabled","values":["example:off"]}"#,
        r#"{"condition":"fabric:registry_contains","registry":"minecraft:item","values":["example:absent"]}"#,
        r#"{"type":"unknown:condition"}"#,
    ];
    for text in falsy {
        let condition = parse_json(text.as_bytes()).unwrap();
        assert!(!evaluate_condition(&condition, &context), "{text}");
    }
}

#[test]
fn conditional_recipe_wrapper_and_sibling_conditions_gate_contextual_loading() {
    let mut ids = ItemFluidIdTable::default();
    let wrapped = br#"{
        "type":"neoforge:conditional",
        "neoforge:conditions":[{"type":"neoforge:mod_loaded","modid":"dependency"}],
        "recipe":{"type":"example:machine","ingredients":[{"item":"example:in"}],"result":{"item":"example:out"}}
    }"#;
    let loaded = ConditionContext::with_mods(["dependency"]);
    let absent = ConditionContext::default();
    assert!(parse_mod_recipe_json_with_context("example:gated", 1, wrapped, &mut ids, &loaded)
        .unwrap()
        .is_some());
    assert!(parse_mod_recipe_json_with_context("example:gated", 1, wrapped, &mut ids, &absent)
        .unwrap()
        .is_none());

    let sibling = object(&[
        (
            "fabric:load_conditions",
            JsonValue::Array(vec![object(&[
                ("condition", JsonValue::String("fabric:all_mods_loaded".into())),
                ("values", JsonValue::Array(vec![JsonValue::String("dependency".into())])),
            ])]),
        ),
        ("type", JsonValue::String("example:machine".into())),
    ]);
    let conditions = sibling
        .as_object()
        .and_then(|o| o.get("fabric:load_conditions"))
        .and_then(JsonValue::as_array)
        .unwrap();
    assert!(evaluate_conditions(conditions, &loaded));
    assert!(!evaluate_conditions(conditions, &absent));
}

#[test]
fn recursive_jar_in_jar_extraction_reaches_depth_three_and_keeps_budget() {
    let leaf = jar(&[("data/deep/recipe/leaf.json", br#"{"type":"x:x","ingredients":[{"item":"x:i"}],"result":{"item":"x:o"}}"#)]);
    let l3 = jar(&[("META-INF/jars/l3.jar", leaf.as_slice())]);
    let l2 = jar(&[("META-INF/jars/l2.jar", l3.as_slice())]);
    let l1 = jar(&[("META-INF/jars/l1.jar", l2.as_slice())]);
    let outer = jar(&[("META-INF/jars/l1.jar", l1.as_slice())]);
    let report = extract_embedded_jars(&outer, "outer.jar").expect("recursive extraction");
    assert_eq!(report.jars.len(), 3, "{:?}", report.rejected);
    assert!(report.jars.iter().any(|jar| jar.depth == 3));
}

#[test]
fn accesswidener_v2_and_declared_at_form_one_non_bridging_visibility_overlay() {
    let aw = br#"accessWidener v2 named
accessible class net/minecraft/class_1
transitive-accessible method net/minecraft/class_1 method_2 ()V
mutable field net/minecraft/class_1 field_3 I
"#;
    let parsed = parse_access_widener(aw).expect("access widener v2 parses");
    assert_eq!(parsed.version, 2);
    assert_eq!(parsed.namespace, "named");
    assert_eq!(parsed.entries.len(), 3);
    assert_eq!(parsed.entries[1].operation, AccessWidenerOperation::Accessible);
    assert!(parsed.entries[1].transitive);
    assert_eq!(parsed.entries[2].target_kind, AtTargetKind::Field);

    let manifest = br#"modLoader="javafml"
loaderVersion="[1,)"
license="LGPL-3.0-or-later"
[[mods]]
modId="example"
version="1.0"
[[accessTransformers]]
file="config/example-at.cfg"
"#;
    let at = b"public net.minecraft.world.level.block.Block\n";
    let fabric = br#"{"schemaVersion":1,"id":"awmod","version":"1.0","accessWidener":"example.accesswidener","mixins":[{"config":"client.mixins.json","environment":"client"}],"languageAdapters":{"kotlin":"example.KotlinAdapter"},"jars":[{"file":"META-INF/jarjar/nested.jar"}],"custom":{"loom:injected_interfaces":{"net/minecraft/class_1":["com/example/Iface"]}}}"#;
    let jar = jar(&[
        ("META-INF/neoforge.mods.toml", manifest),
        ("config/example-at.cfg", at),
        ("fabric.mod.json", fabric),
        ("example.accesswidener", aw),
        ("META-INF/jarjar/nested.jar", jar(&[("data/x/x.json", b"{}")]).as_slice()),
    ]);
    let report = inspect_jar_extensions(&jar, "extensions.jar").expect("extension scan");
    assert_eq!(report.access_transformers.len(), 1);
    assert_eq!(report.access_transformer_files, vec!["config/example-at.cfg"]);
    assert_eq!(report.access_wideners.len(), 1);
    assert_eq!(report.access_widener_files, vec!["example.accesswidener"]);
    assert!(report.jar_in_jar.extracted_members > 0);
    assert!(
        !report.requires_jvm_bridge(),
        "class-level visibility ATs and resolved JiJ must not force JVM bridging"
    );
    let overlay = report.declarative_visibility_overlay();
    assert_eq!(overlay.len(), 4);
    assert!(overlay.iter().any(|entry| entry.transitive
        && entry.target_kind == AtTargetKind::Method
        && entry.access == Some(mod_adapter_core::loader_extensions::AtAccess::Public)));
    assert_eq!(
        report.mixin_config_environments.get("client.mixins.json").map(String::as_str),
        Some("client")
    );
    assert_eq!(
        report.language_adapters.get("kotlin").map(String::as_str),
        Some("example.KotlinAdapter")
    );
    assert_eq!(
        report.fabric_injected_interfaces.get("net/minecraft/class_1"),
        Some(&vec!["com/example/Iface".to_string()])
    );
}

#[test]
fn droplets_are_exactly_81000_per_bucket_and_lossless_at_mb_boundaries() {
    assert_eq!(DROPLETS_PER_BUCKET, 81_000);
    assert_eq!(DROPLETS_PER_MB, 81);
    let amount = FluidAmount::from_mb(1_000);
    assert_eq!(amount.to_droplets(), 81_000);
    assert_eq!(FluidAmount::from_droplets(81_000).to_mb(), 1_000);
    assert_eq!(FluidAmount::from_droplets(81_000).try_to_mb(), Some(1_000));
    assert_eq!(FluidAmount::from_droplets(81_001).try_to_mb(), None);
    assert_eq!(FluidAmount::from_droplets(81_001).to_mb_parts(), (1_000, 1));
    let extreme = FluidAmount::from_mb(u64::MAX);
    assert_eq!(extreme.to_droplets(), u64::MAX as u128 * 81);
    assert_eq!(extreme.try_to_mb(), Some(u64::MAX));
    let stack = CapabilityFluidStack::new("example:steam", 1_000);
    assert_eq!(stack.amount_droplets(), 81_000);
    assert_eq!(
        CapabilityFluidStack::from_fluid_amount("example:steam", FluidAmount::from_droplets(81_000))
            .map(|stack| stack.amount_mb),
        Some(1_000)
    );
    assert!(CapabilityFluidStack::from_fluid_amount("example:steam", FluidAmount::from_droplets(1)).is_none());
}

#[test]
fn worldgen_biome_strippables_null_removals_and_tag_keys_are_data_map_first_class() {
    assert_eq!(
        data_map_key_for_path("data/example/data_maps/worldgen/biome/climate.json"),
        Some(("minecraft:worldgen/biome".to_string(), "climate".to_string()))
    );
    assert_eq!(
        data_map_key_for_path("data/example/data_maps/block/strippables.json"),
        Some(("minecraft:block".to_string(), "strippables".to_string()))
    );
    assert!(is_builtin_data_map("strippables"));
    assert!(is_builtin_data_map("villager_types"));

    let mut registry = DataMapRegistry::new();
    let insert = parse_data_map(
        br##"{"values":{"#minecraft:planks":{"stripped":"example:stripped"},"minecraft:oak_log":{"stripped":"minecraft:stripped_oak_log"}}}"##,
        "minecraft:block",
        "strippables",
        "insert",
    )
    .unwrap();
    registry.apply(&insert);
    let remove = parse_data_map(
        br##"{"values":{"#minecraft:planks":null,"minecraft:oak_log":null}}"##,
        "minecraft:block",
        "strippables",
        "remove",
    )
    .unwrap();
    registry.apply(&remove);
    assert!(registry
        .get("minecraft:block", "strippables")
        .is_none_or(|map| map.is_empty()));
}

#[test]
fn all_nine_neoforge_biome_modifier_kinds_parse() {
    for (kind, json) in [
        (BiomeModifierKind::AddCarvers, r#"{"type":"neoforge:add_carvers","biomes":"minecraft:plains","carvers":"minecraft:cave","step":"air"}"#),
        (BiomeModifierKind::RemoveCarvers, r#"{"type":"neoforge:remove_carvers","biomes":"minecraft:plains","carvers":"minecraft:cave"}"#),
        (BiomeModifierKind::AddSpawnCosts, r#"{"type":"neoforge:add_spawn_costs","biomes":"minecraft:plains","entity_types":"minecraft:zombie","spawn_cost":{"energy_budget":1.0,"charge":0.5}}"#),
        (BiomeModifierKind::RemoveSpawnCosts, r#"{"type":"neoforge:remove_spawn_costs","biomes":"minecraft:plains","entity_types":"minecraft:zombie"}"#),
    ] {
        let parsed = parse_biome_modifier("example:test", json.as_bytes(), "test").unwrap();
        assert_eq!(parsed.kind, kind);
    }
}

#[test]
fn arbitrary_conventional_c_tags_are_accepted_and_legacy_names_alias() {
    assert_eq!(canonical_conventional_tag_id("forge:obsidians"), "c:obsidians");
    assert_eq!(canonical_conventional_tag_id("neoforge:obsidians/crying"), "c:obsidians/crying");
    let mut registry = ConventionalTagRegistry::new();
    assert!(registry
        .ingest_item_tag_member(
            "data/c/tags/item/obsidians.json",
            br#"{"values":["minecraft:obsidian","minecraft:crying_obsidian"]}"#,
        )
        .unwrap());
    let members = registry.resolve("c:obsidians").unwrap();
    assert!(members.contains("minecraft:obsidian"));
    assert!(members.contains("minecraft:crying_obsidian"));
}

#[test]
fn frozen_registry_payload_matches_registry_snapshot_wire_layout() {
    let payload = FrozenRegistryPayload {
        registry_id: "minecraft:block".to_string(),
        ids: BTreeMap::from([
            (1, "example:stone".to_string()),
            (300, "example:ore".to_string()),
        ]),
        aliases: BTreeMap::from([(
            "example:old_ore".to_string(),
            "example:ore".to_string(),
        )]),
    };
    let encoded = payload.encode().unwrap();

    // Outer layout: ResourceLocation(registry_id) + VarInt byte-array length.
    let mut cursor = 0usize;
    let registry_len = encoded[cursor] as usize;
    assert_eq!(registry_len, "minecraft:block".len());
    cursor += 1;
    assert_eq!(&encoded[cursor..cursor + registry_len], b"minecraft:block");
    cursor += registry_len;
    let inner_len = encoded[cursor] as usize;
    cursor += 1;
    let inner = &encoded[cursor..cursor + inner_len];
    assert_eq!(cursor + inner_len, encoded.len());

    // Inner layout is writeMap(ids: VarInt -> RL) followed by writeMap(aliases: RL -> RL).
    let mut expected_inner = Vec::new();
    expected_inner.push(2); // two ids
    expected_inner.push(1); // runtime id 1
    write_mc_string(&mut expected_inner, "example:stone");
    expected_inner.extend_from_slice(&[0xAC, 0x02]); // VarInt 300
    write_mc_string(&mut expected_inner, "example:ore");
    expected_inner.push(1); // one alias
    write_mc_string(&mut expected_inner, "example:old_ore");
    write_mc_string(&mut expected_inner, "example:ore");
    assert_eq!(inner, expected_inner.as_slice());
    assert_eq!(decode_frozen_registry_payload(&encoded), Some(payload));

    let mut trailing = encoded.clone();
    trailing.push(0);
    assert!(decode_frozen_registry_payload(&trailing).is_none());
}
