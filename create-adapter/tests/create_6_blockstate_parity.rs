// SPDX-License-Identifier: LGPL-3.0-or-later
//! Wave 5E (`FIX-CREATE-6-BLOCKSTATES-BELT-SPEED-SU-PARITY`) regression
//! tests: JAR-derived blockstate ingestion, the Create 6.0.x SU/RPM
//! constants, vanilla belt speed, destructive over-speed flagging,
//! controller-only belt stress, and the `toolbox_dyeing`/`item_copying`
//! curiosity recipe kinds.

use create_adapter::blockstates::{
    core_create_registry, create_registry_with_blockstates, CreateBlockRegistry, CreateBlockRole,
    DEFAULT_BASE_STATE_ID,
};
use create_adapter::data_loader::{load_from_stored_zip, CreateRecipeKind, ItemFluidIdTable};
use create_adapter::factory::{CreateFactory, DimBlockPos};
use create_adapter::factorio_sim::{
    ratio_fp, rpm_fp, rpm_to_belt_step, KineticKind, KineticNetworkPool, SUB_UNITS_PER_BLOCK,
};
use mod_offload::kinetic_graph::NetworkStatus;

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

fn state_count(registry: &CreateBlockRegistry, block_id: &str) -> usize {
    let def = registry.block_def(block_id).expect(block_id);
    let mut count = 1usize;
    for prop in &def.properties {
        count *= prop.values.len();
    }
    count
}

fn prop_names(registry: &CreateBlockRegistry, block_id: &str) -> Vec<String> {
    registry
        .block_def(block_id)
        .unwrap()
        .properties
        .iter()
        .map(|p| p.name.clone())
        .collect()
}

/// A synthetic JAR carrying `assets/*/blockstates/*.json` tables in the
/// exact Create 6.0.x spellings: belt's full 320-variant selector space,
/// shaft's bare `axis`, depot's empty selector, cogwheel with an extra
/// JAR-only `powered` property proving the merge overrides the fallback,
/// and an addon-namespace gearbox.
#[test]
fn jar_blockstates_ingest_through_stored_zip_loader() {
    let belt_bs = br#"{
      "variants": {
        "casing=false,facing=east,part=end,slope=downward,waterlogged=false": [],
        "casing=false,facing=north,part=start,slope=horizontal,waterlogged=false": [],
        "casing=true,facing=south,part=middle,slope=sideways,waterlogged=true": [],
        "casing=true,facing=west,part=pulley,slope=upward,waterlogged=true": [],
        "casing=false,facing=east,part=end,slope=vertical,waterlogged=false": []
      }
    }"#;
    let shaft_bs = br#"{"variants":{"axis=x":[],"axis=y":[],"axis=z":[]}}"#;
    let depot_bs = br#"{"variants":{"":[]}}"#;
    // JAR-only extra property: proves the harvested table replaces the
    // fallback def verbatim instead of merging property-by-property.
    let cogwheel_bs = br#"{
      "variants": {
        "axis=x,powered=false": [],
        "axis=y,powered=false": [],
        "axis=z,powered=false": [],
        "axis=x,powered=true": [],
        "axis=y,powered=true": [],
        "axis=z,powered=true": []
      }
    }"#;
    let gearbox_bs = br#"{"variants":{"axis=x":[],"axis=y":[],"axis=z":[]}}"#;
    let jar = build_stored_zip(&[
        ("assets/create/blockstates/belt.json", belt_bs),
        ("assets/create/blockstates/shaft.json", shaft_bs),
        ("assets/create/blockstates/depot.json", depot_bs),
        ("assets/create/blockstates/cogwheel.json", cogwheel_bs),
        ("assets/my_mod/blockstates/alloy_gearbox.json", gearbox_bs),
    ]);

    let mut ids = ItemFluidIdTable::new();
    let loaded = load_from_stored_zip(&jar, &mut ids).unwrap();
    assert!(loaded.blockstates.contains_key("create:belt"));
    assert!(loaded.blockstates.contains_key("my_mod:alloy_gearbox"));

    let reg = create_registry_with_blockstates(DEFAULT_BASE_STATE_ID, &loaded.blockstates)
        .expect("merged registry builds");

    // Belt: casing(2) x facing(4) x part(4) x slope(5) x waterlogged(2) = 320.
    assert_eq!(state_count(&reg, "create:belt"), 320);
    assert_eq!(
        prop_names(&reg, "create:belt"),
        vec!["casing", "facing", "part", "slope", "waterlogged"]
    );
    // Shaft keeps its JAR table (axis only — no phantom waterlogged).
    assert_eq!(state_count(&reg, "create:shaft"), 3);
    assert_eq!(prop_names(&reg, "create:shaft"), vec!["axis"]);
    // Depot's empty selector yields a single stateless variant.
    assert_eq!(state_count(&reg, "create:depot"), 1);
    // The JAR cogwheel table replaced the fallback wholesale: +powered.
    assert_eq!(state_count(&reg, "create:cogwheel"), 6);
    assert_eq!(
        prop_names(&reg, "create:cogwheel"),
        vec!["axis", "powered"]
    );
    // Addon block appended with the inferred structural role.
    let gearbox = reg.block_def("my_mod:alloy_gearbox").expect("addon block");
    assert_eq!(gearbox.role, CreateBlockRole::Gearbox);
    assert_eq!(state_count(&reg, "my_mod:alloy_gearbox"), 3);
}

/// Vanilla `BeltBlockEntity.getBeltMovementSpeed()` is `|rpm| / 480`
/// blocks per tick; with 480 sub-units per block the fixed-point step is
/// the signed RPM itself (64 RPM -> 64/480 b/t, not the old 7.5x-fast
/// `rpm * 256 / 64`).
#[test]
fn belt_speed_matches_vanilla_480_divisor() {
    assert_eq!(SUB_UNITS_PER_BLOCK, 480);
    assert_eq!(rpm_to_belt_step(64), 64);
    assert_eq!(rpm_to_belt_step(256), 256);
    assert_eq!(rpm_to_belt_step(-64), -64);
    let blocks_per_tick = f64::from(rpm_to_belt_step(64)) / f64::from(SUB_UNITS_PER_BLOCK);
    assert!((blocks_per_tick - 64.0 / 480.0).abs() < 1e-12);
}

/// Create 6.0.x `AllBlocks` stress constants: SU per RPM of capacity for
/// generators and SU per RPM of impact for consumers, plus driven RPM.
#[test]
fn kinetic_kind_constants_match_create_allblocks() {
    for (kind, capacity, rpm) in [
        (KineticKind::Waterwheel, 32, 8),
        (KineticKind::LargeWaterwheel, 128, 4),
        (KineticKind::Windmill, 512, 16),
        (KineticKind::SteamEngine, 1024, 64),
        (KineticKind::HandCrank, 8, 32),
        (KineticKind::CreativeMotor, 16384, 256),
    ] {
        assert_eq!(kind.default_capacity(), capacity, "{kind:?} capacity");
        assert_eq!(kind.default_drive_rpm_fp(), rpm_fp(rpm), "{kind:?} rpm");
    }
    for (kind, impact) in [
        (KineticKind::MechanicalPress, 8),
        (KineticKind::CrushingWheel, 8),
        (KineticKind::MechanicalMixer, 4),
        (KineticKind::Millstone, 4),
        (KineticKind::MechanicalPump, 4),
        (KineticKind::MechanicalBearing, 4),
        (KineticKind::Deployer, 4),
        (KineticKind::MechanicalDrill, 4),
        (KineticKind::MechanicalSaw, 4),
        (KineticKind::RopePulley, 4),
        (KineticKind::HosePulley, 4),
        (KineticKind::ElevatorPulley, 4),
        (KineticKind::WeightedEjector, 2),
        (KineticKind::MechanicalCrafter, 2),
        (KineticKind::MechanicalArm, 2),
        (KineticKind::EncasedFan, 2),
    ] {
        assert_eq!(kind.default_impact(), impact, "{kind:?} impact");
    }
}

/// The pool publishes Create's clamped ±256 RPM but flags every node whose
/// pre-clamp solved speed exceeded the ceiling — the destructive
/// over-speed signal vanilla `RotationPropagator` acts on.
#[test]
fn pool_flags_pre_clamp_over_256_rpm_without_hiding_it() {
    let mut pool = KineticNetworkPool::new();
    let motor = pool.add_node(KineticKind::CreativeMotor);
    let shaft = pool.add_node(KineticKind::Shaft);
    // 1:2 gear-up: 256 -> 512 RPM raw.
    pool.connect(motor, shaft, ratio_fp(2, 1)).unwrap();

    assert_eq!(pool.rpm(motor), rpm_fp(256));
    assert_eq!(pool.rpm(shaft), rpm_fp(256), "published speed clamps");
    assert!(!pool.is_oversped(motor), "256 RPM is not over-speed");
    assert!(pool.is_oversped(shaft), "512 RPM raw is destructive");
}

/// Same scenario through `CreateFactory`: the geared-down cogwheel is
/// flagged via `is_kinetic_node_oversped` while its published solution
/// stays clamped at -256 RPM.
#[test]
fn factory_flags_destructive_over_speed_on_geared_cogwheel() {
    let registry = core_create_registry(DEFAULT_BASE_STATE_ID);
    let motor = registry
        .with_property(
            registry.default_state("create:creative_motor").unwrap(),
            "facing",
            "north",
        )
        .unwrap();
    let large = registry
        .with_property(
            registry.default_state("create:large_cogwheel").unwrap(),
            "axis",
            "z",
        )
        .unwrap();
    let small = registry
        .with_property(registry.default_state("create:cogwheel").unwrap(), "axis", "z")
        .unwrap();

    let mut factory = CreateFactory::new(registry);
    let p_motor = DimBlockPos::new(0, 0, 64, 0);
    let p_large = DimBlockPos::new(0, 0, 64, 1);
    let p_small = DimBlockPos::new(0, 1, 65, 1);
    factory.place_block(p_motor, motor);
    factory.place_block(p_large, large);
    factory.place_block(p_small, small);

    assert_eq!(
        factory.kinetic_solution(p_small).unwrap().speed,
        -256.0,
        "published speed stays clamped"
    );
    assert!(!factory.is_kinetic_node_oversped(p_motor));
    assert!(!factory.is_kinetic_node_oversped(p_large));
    assert!(factory.is_kinetic_node_oversped(p_small));
    assert_eq!(
        factory.oversped_kinetic_nodes().collect::<Vec<_>>(),
        vec![p_small]
    );

    // Removing the block clears its flag.
    factory.remove_block(p_small);
    assert!(factory.oversped_kinetic_nodes().next().is_none());
}

/// In Create only a belt line's controller segment contributes kinetic
/// stress; every other segment is stress-free until marked.
#[test]
fn only_belt_controller_segment_applies_stress() {
    let registry = core_create_registry(DEFAULT_BASE_STATE_ID);
    let wheel = registry
        .with_property(
            registry.default_state("create:water_wheel").unwrap(),
            "facing",
            "east",
        )
        .unwrap();
    let belt = registry
        .decode_canonical(
            "create:belt[casing=false,facing=east,part=middle,slope=horizontal,waterlogged=false]",
        )
        .expect("canonical belt state");

    let mut factory = CreateFactory::new(registry);
    let p_wheel = DimBlockPos::new(0, 0, 64, 0);
    let p_belt = DimBlockPos::new(0, 1, 64, 0);
    factory.place_block(p_wheel, wheel);
    factory.place_block(p_belt, belt);

    let sol = factory.kinetic_solution(p_belt).unwrap();
    assert_eq!(sol.status, NetworkStatus::Running);
    assert_eq!(sol.speed, 8.0, "water wheel drives the belt at 8 RPM");
    assert_eq!(sol.load, 0.0, "non-controller belt segment: zero stress");

    // Marking the segment as the line's controller applies its 1 SU/RPM
    // impact scaled by the solved 8 RPM -> 8 SU of load.
    factory.set_belt_controller(p_belt, true);
    let sol = factory.kinetic_solution(p_belt).unwrap();
    assert_eq!(sol.load, 8.0);
    assert_eq!(sol.status, NetworkStatus::Running);

    // Non-belt positions are ignored by the controller API.
    assert!(factory.set_belt_controller(p_wheel, true).is_empty());

    factory.set_belt_controller(p_belt, false);
    assert_eq!(factory.kinetic_solution(p_belt).unwrap().load, 0.0);
}

/// `create:toolbox_dyeing` and `create:item_copying` declare a `type` but
/// no `ingredients`/`results`; the loader must still surface them as
/// recognized Create recipe kinds instead of dropping the entries.
#[test]
fn toolbox_dyeing_and_item_copying_recipes_are_not_dropped() {
    let jar = build_stored_zip(&[
        (
            "data/create/recipe/crafting/curiosities/toolbox_dyeing.json",
            br#"{"type":"create:toolbox_dyeing","category":"misc"}"#,
        ),
        (
            "data/create/recipe/crafting/curiosities/item_copying.json",
            br#"{"type":"create:item_copying","category":"misc"}"#,
        ),
    ]);
    let mut ids = ItemFluidIdTable::new();
    let loaded = load_from_stored_zip(&jar, &mut ids).unwrap();
    assert_eq!(loaded.recipes.len(), 2);
    let kinds: Vec<CreateRecipeKind> = loaded.recipes.iter().map(|r| r.kind).collect();
    assert!(kinds.contains(&CreateRecipeKind::ToolboxDyeing));
    assert!(kinds.contains(&CreateRecipeKind::ItemCopying));
    assert!(
        loaded
            .recipes
            .iter()
            .any(|r| r.name == "create:crafting/curiosities/toolbox_dyeing")
    );
    assert!(
        loaded
            .recipes
            .iter()
            .any(|r| r.name == "create:crafting/curiosities/item_copying")
    );
}

/// Sanity: the fallback catalog still matches the verified JAR state
/// counts (the in-crate unit test pins the same table).
#[test]
fn fallback_catalog_jar_state_counts() {
    let registry = core_create_registry(DEFAULT_BASE_STATE_ID);
    for (id, expected) in [
        ("create:water_wheel", 6),
        ("create:belt", 320),
        ("create:shaft", 3),
        ("create:large_water_wheel", 6),
        ("create:stressometer", 12),
        ("create:andesite_belt_funnel", 64),
        ("create:depot", 1),
        ("create:mechanical_pump", 6),
        ("create:chute", 40),
        ("create:andesite_funnel", 48),
    ] {
        assert_eq!(state_count(&registry, id), expected, "{id}");
    }
    // Spot-check canonical spellings of the corrected property tables.
    let wheel = registry
        .decode_canonical("create:water_wheel[facing=north]")
        .expect("water wheel facing state");
    assert_eq!(
        registry.encode_canonical(wheel),
        Some("create:water_wheel[facing=north]")
    );
    let pump = registry
        .decode_canonical("create:mechanical_pump[facing=up]")
        .expect("pump facing state");
    assert_eq!(
        registry.encode_canonical(pump),
        Some("create:mechanical_pump[facing=up]")
    );
}
