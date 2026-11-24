// SPDX-License-Identifier: LGPL-3.0-or-later

//! `discover_addon_from_jar`: a synthetic addon namespace (`my_custom_factory`)
//! that appears in NO built-in profile must yield real kinetic specs,
//! directional blockstates, transport/fluid blocks, and processing recipes —
//! proven end-to-end inside a live `CreateFactory`.

use create_adapter::addon_profile::{
    discover_addon_from_jar, discover_addon_namespaces, AddonProcessingRecipe, ConnectorKind,
};
use create_adapter::blockstates::{CreateBlockRole, DEFAULT_BASE_STATE_ID};
use create_adapter::data_loader::ItemFluidIdTable;
use create_adapter::factory::{CreateFactory, DimBlockPos};
use mod_adapter_core::blockstates::UniversalBlockRole;
use mod_offload::kinetic_graph::NetworkStatus;
use mod_offload::recipe_processor::{Resource, ResourceKind, Stack};

fn build_stored_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, data) in entries {
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);
    }
    out
}

/// A complete unknown Create addon: kinetic source + consumer + gearbox,
/// belt/pipe/tank transport, ores, and every processing recipe family.
fn my_custom_factory_jar() -> Vec<u8> {
    build_stored_zip(&[
        (
            "assets/my_custom_factory/blockstates/diesel_engine.json",
            br#"{"variants": {"axis=x": {}, "axis=y": {}, "axis=z": {}}}"#,
        ),
        (
            "assets/my_custom_factory/blockstates/alternator.json",
            br#"{"variants": {"axis=x": {}, "axis=y": {}, "axis=z": {}}}"#,
        ),
        (
            "assets/my_custom_factory/blockstates/custom_gearbox.json",
            br#"{"variants": {"axis=x": {}, "axis=y": {}, "axis=z": {}}}"#,
        ),
        (
            "assets/my_custom_factory/blockstates/factory_belt.json",
            br#"{"variants": {"facing=north": {}, "facing=south": {}, "facing=east": {}, "facing=west": {}}}"#,
        ),
        (
            "assets/my_custom_factory/blockstates/fluid_pipe.json",
            br#"{"variants": {"": {}}}"#,
        ),
        (
            "assets/my_custom_factory/blockstates/fluid_tank.json",
            br#"{"variants": {"": {}}}"#,
        ),
        (
            "assets/my_custom_factory/blockstates/tin_ore.json",
            br#"{"variants": {"": {}}}"#,
        ),
        (
            "data/my_custom_factory/tags/block/ores.json",
            br#"{"values": ["my_custom_factory:tin_ore"]}"#,
        ),
        (
            "data/c/tags/block/ores.json",
            br#"{"values": ["my_custom_factory:tin_ore", "othermod:ore"]}"#,
        ),
        (
            "data/my_custom_factory/create_kinetics/diesel.json",
            br#"{
              "mod_id": "my_custom_factory",
              "kinetic_sources": [
                {"id": "my_custom_factory:diesel_engine", "rpm": 128, "capacity": 4096.0, "axis": "z"}
              ],
              "kinetic_consumers": [
                {"id": "my_custom_factory:alternator", "impact": 16.0, "min_rpm": 8.0}
              ],
              "connectors": [
                {"id": "my_custom_factory:custom_gearbox", "kind": "gearbox"}
              ]
            }"#,
        ),
        (
            "data/my_custom_factory/recipes/assembly/press_plate.json",
            br#"{
              "type": "my_custom_factory:sequenced_assembly",
              "ingredient": {"item": "my_custom_factory:raw_plate"},
              "transitionalItem": {"id": "my_custom_factory:unfinished_plate"},
              "loops": 1,
              "sequence": [
                {"type": "my_custom_factory:deploying", "held_item": "my_custom_factory:die"},
                {"type": "my_custom_factory:pressing"}
              ],
              "results": [{"id": "my_custom_factory:pressed_plate", "count": 1}]
            }"#,
        ),
        (
            "data/my_custom_factory/recipe/mixing/alloy.json",
            br#"{"type": "my_custom_factory:mixing", "ingredients": [{"item": "my_custom_factory:ingot_a"}], "results": [{"id": "my_custom_factory:alloy", "count": 2}]}"#,
        ),
        (
            "data/my_custom_factory/recipes/cutting/rod.json",
            br#"{"type": "my_custom_factory:cutting", "ingredients": [{"item": "my_custom_factory:bar"}], "results": [{"id": "my_custom_factory:rod", "count": 4}]}"#,
        ),
        (
            "data/my_custom_factory/recipes/filling/oil.json",
            br#"{"type": "my_custom_factory:filling", "ingredients": [{"item": "my_custom_factory:can"}, {"fluid": "my_custom_factory:oil", "amount": 250}], "results": [{"id": "my_custom_factory:oiled_can", "count": 1}]}"#,
        ),
        (
            "data/my_custom_factory/recipes/pressing/flatten.json",
            br#"{"type": "my_custom_factory:pressing", "ingredients": [{"item": "my_custom_factory:sheet_blank"}], "results": [{"id": "my_custom_factory:flattened", "count": 1}]}"#,
        ),
        (
            "data/minecraft/recipes/crafting/ignored.json",
            br##"{"type": "minecraft:crafting_shaped", "pattern": ["#"], "key": {"#": {"item": "minecraft:stick"}}, "result": {"id": "minecraft:torch", "count": 4}}"##,
        ),
    ])
}

#[test]
fn unknown_addon_jar_yields_full_profile_without_builtin_names() {
    let jar = my_custom_factory_jar();
    let discovered = discover_addon_from_jar(&jar, "my_custom_factory").unwrap();
    assert_eq!(discovered.skipped_compressed_entries, 0);
    let profile = &discovered.profile;
    assert_eq!(profile.mod_id, "my_custom_factory");

    // Real kinetic specs came from create_kinetics, not heuristics.
    let source = profile
        .kinetic_sources
        .iter()
        .find(|s| s.id == "my_custom_factory:diesel_engine")
        .expect("diesel engine source");
    assert_eq!(source.base_rpm, 128);
    assert_eq!(source.stress_capacity, 4096.0);
    assert_eq!(source.axis, create_adapter::factory::Axis::Z);
    let consumer = profile
        .kinetic_consumers
        .iter()
        .find(|c| c.id == "my_custom_factory:alternator")
        .expect("alternator consumer");
    assert_eq!(consumer.stress_impact, 16.0);
    assert_eq!(consumer.min_rpm, 8.0);
    assert!(profile.connectors.iter().any(|c| {
        c.id == "my_custom_factory:custom_gearbox" && c.kind == ConnectorKind::Gearbox
    }));

    // Directional blockstates became real property tables.
    assert!(profile
        .block_properties
        .get("my_custom_factory:diesel_engine")
        .is_some_and(|p| p.iter().any(|d| d.name == "axis")));
    assert!(profile
        .block_properties
        .get("my_custom_factory:factory_belt")
        .is_some_and(|p| p.iter().any(|d| d.name == "facing")));

    // Transport/fluid blocks mirrored into the universal offload tier.
    let offload = &profile.offload;
    assert!(offload.blocks.iter().any(|b| {
        b.id == "my_custom_factory:factory_belt" && b.role == UniversalBlockRole::TransportPipe
    }));
    assert!(offload.blocks.iter().any(|b| {
        b.id == "my_custom_factory:fluid_pipe" && b.role == UniversalBlockRole::FluidPipe
    }));
    assert!(offload
        .tank_capacity_mb
        .contains_key("my_custom_factory:fluid_tank"));

    // Ore hidden via the shared c:ores tag only (not the addon's own tag
    // namespace rules) plus name-pattern coverage.
    assert!(profile.hidden_blocks.contains(&"my_custom_factory:tin_ore".to_string()));

    // Every processing recipe family was extracted.
    let kinds: Vec<&str> = profile
        .processing_recipes
        .iter()
        .map(|r| match r {
            AddonProcessingRecipe::SequencedAssembly(_) => "assembly",
            AddonProcessingRecipe::Pressing { .. } => "pressing",
            AddonProcessingRecipe::Mixing { .. } => "mixing",
            AddonProcessingRecipe::Milling { .. } => "milling",
            AddonProcessingRecipe::Crushing { .. } => "crushing",
            AddonProcessingRecipe::Cutting { .. } => "cutting",
            AddonProcessingRecipe::Filling { .. } => "filling",
            AddonProcessingRecipe::Compacting(_) => "compacting",
            AddonProcessingRecipe::Splashing(_) => "splashing",
            AddonProcessingRecipe::Haunting(_) => "haunting",
            AddonProcessingRecipe::Deploying(_) => "deploying",
            AddonProcessingRecipe::Emptying(_) => "emptying",
            AddonProcessingRecipe::ItemApplication(_) => "item_application",
            AddonProcessingRecipe::SandpaperPolishing(_) => "sandpaper_polishing",
            AddonProcessingRecipe::MechanicalCrafting(_) => "mechanical_crafting",
            AddonProcessingRecipe::GenericMachine(_) => "generic_machine",
        })
        .collect();
    for expected in ["assembly", "pressing", "mixing", "cutting", "filling"] {
        assert!(kinds.contains(&expected), "missing recipe kind {expected}");
    }
    assert!(profile.processing_recipes.iter().any(|r| matches!(
        r,
        AddonProcessingRecipe::Filling { fluid_id, amount_mb: 250, .. }
            if fluid_id == "my_custom_factory:oil"
    )));
    // The minecraft:crafting_shaped decoy was ignored.
    assert_eq!(profile.processing_recipes.len(), 5);
}

#[test]
fn discovered_profile_runs_inside_live_factory_world() {
    let jar = my_custom_factory_jar();
    let discovered = discover_addon_from_jar(&jar, "my_custom_factory").unwrap();
    let profile = discovered.profile;

    // Convert the discovered mixing recipe into a processor `Recipe` before
    // the profile moves into the factory.
    let mut ids = ItemFluidIdTable::new();
    let mixing = profile
        .processing_recipes
        .iter()
        .find_map(|r| match r {
            AddonProcessingRecipe::Mixing { .. } => r.to_processor_recipe(&mut ids, 1, 40),
            _ => None,
        })
        .expect("discovered mixing recipe converts");
    assert!(profile
        .processing_recipes
        .iter()
        .any(|r| matches!(r, AddonProcessingRecipe::SequencedAssembly(_)
            if r.to_processor_recipe(&mut ItemFluidIdTable::new(), 2, 40).is_none())));
    let ingot_a = ids.item_id("my_custom_factory:ingot_a").unwrap();
    let alloy = ids.item_id("my_custom_factory:alloy").unwrap();

    let mut factory = CreateFactory::new_with_addons(DEFAULT_BASE_STATE_ID, &[profile]).unwrap();
    let reg = factory.registry();

    // Registry expanded the real axis property table for addon blocks.
    let engine_default = reg.default_state("my_custom_factory:diesel_engine").unwrap();
    let engine_z = reg.with_property(engine_default, "axis", "z").unwrap();
    assert_eq!(
        reg.encode_canonical(engine_z),
        Some("my_custom_factory:diesel_engine[axis=z]")
    );
    let alternator_default = reg.default_state("my_custom_factory:alternator").unwrap();
    let alternator_z = reg.with_property(alternator_default, "axis", "z").unwrap();
    let role = reg
        .block_def_for_state(engine_z)
        .map(|d| d.role);
    assert_eq!(role, Some(CreateBlockRole::Generator { driven_rpm: 128 }));

    // Live world: Z-axis diesel engine directly feeding a Z-axis alternator.
    let p_engine = DimBlockPos::new(0, 4, 64, 4);
    let p_alternator = DimBlockPos::new(0, 4, 64, 5);
    factory.place_block(p_engine, engine_z);
    factory.place_block(p_alternator, alternator_z);

    let sol_engine = factory.kinetic_solution(p_engine).unwrap();
    let sol_alternator = factory.kinetic_solution(p_alternator).unwrap();
    assert_eq!(sol_engine.status, NetworkStatus::Running);
    assert_eq!(sol_alternator.status, NetworkStatus::Running);
    // 128 RPM drive, 4096 SU capacity vs 16 SU/RPM * 128 RPM = 2048 SU load.
    assert!((sol_alternator.speed - 128.0).abs() < 1e-9);
    assert!(sol_alternator.capacity >= 4096.0 * 128.0);
    assert!(sol_alternator.load >= 2048.0);

    // The factory keeps ticking deterministically with the addon blocks live.
    for tick in 1..=4u64 {
        let outputs = factory.step_tick(tick, 8, None).unwrap();
        assert!(outputs.iter().all(|o| o.report.processed <= 8));
    }
    assert_eq!(
        factory.kinetic_solution(p_alternator).unwrap().status,
        NetworkStatus::Running
    );

    // The discovered mixing recipe actually executes inside the live world:
    // the engine's 128 RPM drives the processor, ingot_a becomes 2 alloy.
    factory
        .add_processor(p_engine, 7, 64, 64, vec![mixing])
        .unwrap();
    factory
        .insert_processor_ingredient(
            p_engine,
            7,
            Stack {
                resource: Resource {
                    kind: ResourceKind::Item,
                    id: ingot_a,
                },
                amount: 1,
            },
        )
        .unwrap();
    for tick in 5..=40u64 {
        factory.step_tick(tick, 64, None).unwrap();
    }
    let extracted = factory
        .extract_processor_product(
            p_engine,
            7,
            Stack {
                resource: Resource {
                    kind: ResourceKind::Item,
                    id: alloy,
                },
                amount: 2,
            },
        )
        .unwrap();
    assert_eq!(extracted, 2);
}

#[test]
fn namespace_discovery_and_fail_closed_edges() {
    let jar = my_custom_factory_jar();
    let mut ids = mod_adapter_core::data_loader::ItemFluidIdTable::new();
    let data = mod_adapter_core::data_loader::load_from_stored_zip(&jar, &mut ids).unwrap();
    let namespaces = discover_addon_namespaces(&data);
    assert!(namespaces.contains("my_custom_factory"));
    assert!(!namespaces.contains("minecraft"));
    assert!(!namespaces.contains("c"));
    assert!(!namespaces.contains("create"));

    // Invalid namespaces fail closed.
    for bad in ["", "bad ns", "a:b", "a/b"] {
        assert!(discover_addon_from_jar(&jar, bad).is_err(), "accepted '{bad}'");
    }

    // A recognized-but-malformed processing recipe aborts the scan: append a
    // second stored entry with invalid JSON under the recipe dir.
    let mut corrupted = my_custom_factory_jar();
    let name = "data/my_custom_factory/recipes/pressing/broken.json";
    corrupted.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
    corrupted.extend_from_slice(&20u16.to_le_bytes());
    corrupted.extend_from_slice(&0u16.to_le_bytes());
    corrupted.extend_from_slice(&0u16.to_le_bytes());
    corrupted.extend_from_slice(&0u16.to_le_bytes());
    corrupted.extend_from_slice(&0u16.to_le_bytes());
    corrupted.extend_from_slice(&0u32.to_le_bytes());
    let payload = b"{truncated";
    corrupted.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    corrupted.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    corrupted.extend_from_slice(&(name.len() as u16).to_le_bytes());
    corrupted.extend_from_slice(&0u16.to_le_bytes());
    corrupted.extend_from_slice(name.as_bytes());
    corrupted.extend_from_slice(payload);
    assert!(discover_addon_from_jar(&corrupted, "my_custom_factory").is_err());

    // A kinetics override document that is not valid JSON fails closed too.
    let bad_kinetics = build_stored_zip(&[(
        "data/my_custom_factory/create_kinetics/bad.json",
        b"{not json",
    )]);
    assert!(discover_addon_from_jar(&bad_kinetics, "my_custom_factory").is_err());
}
