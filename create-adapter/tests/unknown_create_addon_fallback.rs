// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};

use create_adapter::addon_profile::discover_addon_from_jar;
use create_adapter::blockstates::{CreateBlockRole, DEFAULT_BASE_STATE_ID};
use create_adapter::data_loader::{
    load_from_directory, load_from_stored_zip, parse_create_recipe_json, CreateRecipeKind,
    ItemFluidIdTable,
};
use create_adapter::factory::{Axis, CreateFactory};
use mod_adapter_core::blockstates::UniversalBlockRole;

const NAMESPACE: &str = "unlisted_create_extension";

fn stored_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, payload) in entries {
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&[0; 8]);
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(payload);
    }
    out
}

#[test]
fn standard_create_serializers_are_recognized_in_any_valid_namespace() {
    for (path, kind) in [
        ("crushing", CreateRecipeKind::Crushing),
        ("mixing", CreateRecipeKind::Mixing),
        ("pressing", CreateRecipeKind::Pressing),
        ("cutting", CreateRecipeKind::Cutting),
        ("compacting", CreateRecipeKind::Compacting),
        ("deploying", CreateRecipeKind::Deploying),
        ("filling", CreateRecipeKind::Filling),
        ("emptying", CreateRecipeKind::Emptying),
        ("splashing", CreateRecipeKind::Splashing),
        ("haunting", CreateRecipeKind::Haunting),
        ("sandpaper_polishing", CreateRecipeKind::SandpaperPolishing),
        ("mechanical_crafting", CreateRecipeKind::MechanicalCrafting),
        ("sequenced_assembly", CreateRecipeKind::SequencedAssembly),
        ("milling", CreateRecipeKind::Milling),
        ("item_application", CreateRecipeKind::ItemApplication),
    ] {
        assert_eq!(
            CreateRecipeKind::from_type_str(&format!("create:{path}")),
            Some(kind)
        );
        assert_eq!(
            CreateRecipeKind::from_type_str(&format!("{NAMESPACE}:{path}")),
            Some(kind)
        );
    }
    for invalid in [
        "pressing",
        ":pressing",
        "Uppercase:pressing",
        "bad/name:pressing",
        "unknown:unrelated",
        "unknown:pressing/extra",
        "unknown:pressing:extra",
    ] {
        assert_eq!(CreateRecipeKind::from_type_str(invalid), None, "{invalid}");
    }

    let mut ids = ItemFluidIdTable::new();
    let recipe = parse_create_recipe_json(
        "unlisted_create_extension:plate",
        7,
        br#"{"type":"unlisted_create_extension:pressing","ingredients":[{"item":"minecraft:iron_ingot"}],"results":[{"id":"unlisted_create_extension:plate","count":2}],"processing_time":40}"#,
        &mut ids,
    )
    .unwrap()
    .unwrap();
    assert_eq!(recipe.kind, CreateRecipeKind::Pressing);
    assert_eq!(recipe.recipe.inputs[0].amount, 1);
    assert_eq!(recipe.recipe.outputs[0].amount, 2);
    assert_eq!(recipe.recipe.base_ticks, 40);
}

#[test]
fn arbitrary_namespace_mechanical_grids_work_from_jar_and_directory() {
    let entries: &[(&str, &[u8])] = &[
        (
            "data/unlisted_create_extension/recipes/pressing/plate.json",
            br#"{"type":"unlisted_create_extension:pressing","ingredients":[{"item":"minecraft:iron_ingot"}],"results":[{"id":"unlisted_create_extension:plate"}]}"#,
        ),
        (
            "data/unlisted_create_extension/recipe/crafting/gadget.json",
            br##"{"type":"unlisted_create_extension:mechanical_crafting","pattern":["AA"," B"],"key":{"A":{"item":"minecraft:iron_ingot"},"B":{"item":"minecraft:stick"}},"result":{"id":"unlisted_create_extension:gadget","count":2}}"##,
        ),
        (
            "data/create/recipes/crafting/core.json",
            br##"{"type":"create:mechanical_crafting","pattern":["A"],"key":{"A":{"item":"minecraft:iron_ingot"}},"result":{"id":"create:core"}}"##,
        ),
        (
            "data/minecraft/recipes/crafting/sword.json",
            br##"{"type":"minecraft:crafting_shaped","pattern":["A"],"key":{"A":{"item":"minecraft:iron_ingot"}},"result":{"id":"minecraft:sword"}}"##,
        ),
    ];
    let mut ids = ItemFluidIdTable::new();
    let jar_data = load_from_stored_zip(&stored_zip(entries), &mut ids).unwrap();

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "create-addon-m30-{}-{}",
        std::process::id(),
        NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
    ));
    for (name, payload) in entries {
        let file = dir.join(name);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, payload).unwrap();
    }
    let mut directory_ids = ItemFluidIdTable::new();
    let directory_data = load_from_directory(&dir, &mut directory_ids).unwrap();
    fs::remove_dir_all(&dir).unwrap();

    for data in [jar_data, directory_data] {
        assert_eq!(data.recipes.len(), 3);
        let grid = data
            .recipes
            .iter()
            .find(|recipe| recipe.name == "unlisted_create_extension:crafting/gadget")
            .unwrap();
        assert_eq!(grid.kind, CreateRecipeKind::MechanicalCrafting);
        assert_eq!(grid.recipe.inputs.iter().map(|s| s.amount).sum::<u32>(), 3);
        assert_eq!(grid.recipe.outputs[0].amount, 2);
        assert!(data.recipes.iter().any(|recipe| {
            recipe.name == "unlisted_create_extension:pressing/plate"
                && recipe.kind == CreateRecipeKind::Pressing
        }));
        assert!(data.recipes.iter().any(|recipe| {
            recipe.name == "create:crafting/core"
                && recipe.kind == CreateRecipeKind::MechanicalCrafting
        }));
    }
}

#[test]
fn blockstate_kinetic_properties_and_metadata_convert_without_known_addon_table() {
    let jar = stored_zip(&[
        (
            "assets/unlisted_create_extension/blockstates/power_core.json",
            br#"{"variants":{"rpm=96,stress_capacity=1200,facing=east":{}}}"#,
        ),
        (
            "assets/unlisted_create_extension/blockstates/pulse_unit.json",
            br#"{"variants":{"stress_impact=6.5,axis=z":{}}}"#,
        ),
        (
            "assets/unlisted_create_extension/blockstates/mixer.json",
            br#"{"variants":{"stress_impact=8,facing=north":{}}}"#,
        ),
        (
            "data/unlisted_create_extension/recipes/mixing/alloy.json",
            br#"{"type":"unlisted_create_extension:mixing","ingredients":[{"item":"minecraft:iron_ingot"}],"results":[{"id":"unlisted_create_extension:alloy"}]}"#,
        ),
        (
            "assets/unlisted_create_extension/blockstates/decorative_pillar.json",
            br#"{"variants":{"facing=north":{},"facing=south":{}}}"#,
        ),
        (
            "assets/unlisted_create_extension/blockstates/adjustable_unit.json",
            br#"{"variants":{"rpm=64":{},"rpm=128":{}}}"#,
        ),
        (
            "data/unlisted_create_extension/create_kinetics/custom.json",
            br#"{"mod_id":"unlisted_create_extension","kinetic_sources":[{"id":"unlisted_create_extension:custom_dynamo","rpm":32,"stress_capacity":2048,"facing":"north"}],"kinetic_consumers":[{"id":"unlisted_create_extension:custom_machine","stress_impact":12}]}"#,
        ),
    ]);
    let profile = discover_addon_from_jar(&jar, NAMESPACE).unwrap().profile;
    let power = profile
        .kinetic_sources
        .iter()
        .find(|source| source.id == "unlisted_create_extension:power_core")
        .unwrap();
    assert_eq!(
        (power.base_rpm, power.stress_capacity, power.axis),
        (96, 1200.0, Axis::X)
    );
    let dynamo = profile
        .kinetic_sources
        .iter()
        .find(|source| source.id == "unlisted_create_extension:custom_dynamo")
        .unwrap();
    assert_eq!(
        (dynamo.base_rpm, dynamo.stress_capacity, dynamo.axis),
        (32, 2048.0, Axis::Z)
    );
    for (id, expected) in [
        ("unlisted_create_extension:pulse_unit", 6.5),
        ("unlisted_create_extension:custom_machine", 12.0),
    ] {
        let consumer = profile
            .kinetic_consumers
            .iter()
            .find(|c| c.id == id)
            .unwrap();
        assert_eq!(consumer.stress_impact, expected);
    }
    assert!(profile
        .decorative_blocks
        .contains(&"unlisted_create_extension:decorative_pillar".into()));
    assert!(profile
        .decorative_blocks
        .contains(&"unlisted_create_extension:adjustable_unit".into()));
    let universal = profile
        .offload
        .blocks
        .iter()
        .find(|b| b.id == "unlisted_create_extension:power_core")
        .unwrap();
    assert_eq!(universal.role, UniversalBlockRole::NetworkGenerator);
    assert_eq!(universal.capacity_factor, 1200.0);
    let mixer = profile
        .offload
        .blocks
        .iter()
        .find(|b| b.id == "unlisted_create_extension:mixer")
        .unwrap();
    assert_eq!(mixer.role, UniversalBlockRole::Processor);
    assert_eq!(mixer.load_factor, 8.0);
    assert_eq!(
        profile.offload.processor_recipe_types["unlisted_create_extension:mixer"],
        ["unlisted_create_extension:mixing"]
    );

    let factory = CreateFactory::new_with_addons(DEFAULT_BASE_STATE_ID, &[profile]).unwrap();
    let registry = factory.registry();
    let state = registry
        .default_state("unlisted_create_extension:power_core")
        .unwrap();
    assert_eq!(
        registry.block_def_for_state(state).unwrap().role,
        CreateBlockRole::Generator { driven_rpm: 96 }
    );
    assert_eq!(
        registry.block_def_for_state(state).unwrap().capacity_factor,
        1200.0
    );
    assert_eq!(registry.with_property(state, "facing", "east"), Some(state));
    let consumer = registry
        .default_state("unlisted_create_extension:pulse_unit")
        .unwrap();
    assert_eq!(
        registry.block_def_for_state(consumer).unwrap().role,
        CreateBlockRole::StressConsumer
    );
    assert_eq!(
        registry.block_def_for_state(consumer).unwrap().load_factor,
        6.5
    );
    let mixer = registry
        .default_state("unlisted_create_extension:mixer")
        .unwrap();
    assert_eq!(
        registry.block_def_for_state(mixer).unwrap().role,
        CreateBlockRole::Processor
    );
    assert_eq!(
        registry.block_def_for_state(mixer).unwrap().load_factor,
        8.0
    );
}
