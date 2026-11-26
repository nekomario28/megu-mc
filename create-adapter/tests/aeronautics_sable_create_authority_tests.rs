// SPDX-License-Identifier: LGPL-3.0-or-later

//! Audit 3 authority/transaction integration coverage for Create, Sable-style
//! rail replication, and Aeronautics orbital ownership.

use std::collections::{BTreeMap, BTreeSet};

use create_adapter::addon_profile::{create_registry_with_addons, createaeronautics_profile};
use create_adapter::aeronautics::RigidBodySubWorld;
use create_adapter::blockstates::{core_create_registry, CreateBlockRegistry, DEFAULT_BASE_STATE_ID};
use create_adapter::contraption_adapter::{
    ActorCell, ActorWorldEdit, ContraptionActor, ContraptionActorKind,
    ContraptionKind, ContraptionStorage, ContraptionWorldQuery, LiveContraption,
};
use create_adapter::factory::DimBlockPos;
use create_adapter::offload_bridge::{OffloadBridge, OffloadBudget};
use create_adapter::orbital::{
    OrbitalDimension, OrbitalTransitionRouter, OrbitalTransitionRule, OrbitalTrigger,
};
use create_adapter::rail_network::{
    ArticulatedTrain, CarriageSpec, ChunkKey, DimId as RailDimId, TrackGauge, TrainId,
    TrainSchedule,
};
use create_adapter::schematic::{
    parse_nbt_schematic, SchematicNbt, SchematicPlacementError, SchematicPlacementPlan,
};
use create_adapter::aeronautics::AtmospherePhysics;

const DIM: i32 = 0;
const ANCHOR: [i32; 3] = [10, 64, 10];

fn create_registry() -> CreateBlockRegistry {
    core_create_registry(DEFAULT_BASE_STATE_ID)
}

fn assemble_one() -> LiveContraption {
    let shaft = create_registry().default_state("create:shaft").unwrap();
    let mut world = BTreeMap::new();
    world.insert(ANCHOR, shaft);
    LiveContraption::assemble_from_world(
        1,
        DIM,
        ANCHOR,
        ANCHOR,
        &world,
        ContraptionKind::Bearing {
            angular_deg_per_tick: 0.0,
        },
        4096,
    )
    .unwrap()
    .0
}

#[derive(Default)]
struct StubWorld {
    cells: BTreeMap<[i32; 3], ActorCell>,
}

impl ContraptionWorldQuery for StubWorld {
    fn cell_at(&self, pos: [i32; 3]) -> ActorCell {
        self.cells.get(&pos).cloned().unwrap_or_else(ActorCell::air)
    }
}

#[test]
fn staged_actor_commit_preserves_items_on_failed_world_edit_and_overflow() {
    let mut contraption = assemble_one();
    contraption.storage = ContraptionStorage::new(1);
    assert_eq!(contraption.storage.insert_item("minecraft:dirt", 7), 0);
    contraption.actors.push(ContraptionActor {
        local: [0, 0, 0],
        facing: [1, 0, 0],
        kind: ContraptionActorKind::MechanicalDrill,
    });
    let mut cells = BTreeMap::new();
    cells.insert([11, 64, 10], ActorCell::block("minecraft:stone", true));
    let world = StubWorld { cells };

    let batch = contraption.step_actors_staged(&world);
    assert_eq!(batch.edits.len(), 1);
    assert_eq!(
        batch.edits[0].edit,
        ActorWorldEdit::BreakBlock {
            pos: DimBlockPos::new(DIM, 11, 64, 10)
        }
    );
    assert_eq!(
        batch.edits[0].expected_pre_state.name,
        "minecraft:stone"
    );
    assert!(batch.edits[0].inventory_delta.is_empty());
    assert_eq!(batch.edits[0].overflow_drops.len(), 1);
    assert_eq!(batch.edits[0].overflow_drops[0].item_id, "minecraft:stone");
    // Staging alone is side-effect free.
    assert_eq!(contraption.storage.total_count_of("minecraft:stone"), 0);
    assert_eq!(contraption.storage.total_count_of("minecraft:dirt"), 7);

    // Failed world edit: no credit, no drop, no loss.
    let failed = contraption.commit_staged_actor_inventory(&batch, &[false]);
    assert!(failed.committed_edits.is_empty());
    assert!(failed.drops.is_empty());
    assert_eq!(contraption.storage.total_count_of("minecraft:stone"), 0);
    assert_eq!(contraption.storage.total_count_of("minecraft:dirt"), 7);

    // Successful world edit: the one-slot inventory is occupied by dirt, so
    // the mined stone is surfaced as overflow rather than disappearing.
    let committed = contraption.commit_staged_actor_inventory(&batch, &[true]);
    assert_eq!(committed.committed_edits, [0]);
    assert_eq!(committed.drops.len(), 1);
    assert_eq!(committed.drops[0].item_id, "minecraft:stone");
    assert_eq!(committed.drops[0].count, 1);
    assert_eq!(contraption.storage.total_count_of("minecraft:stone"), 0);
    assert_eq!(contraption.storage.total_count_of("minecraft:dirt"), 7);
}

fn small_ship(id: u64, dim: u32) -> RigidBodySubWorld {
    let addon = createaeronautics_profile();
    let reg = create_registry_with_addons(DEFAULT_BASE_STATE_ID, std::slice::from_ref(&addon))
        .unwrap();
    let mut cells = BTreeMap::new();
    let state = reg.default_state("createaeronautics:airship_assembler").unwrap();
    cells.insert(
        [0, 0, 0],
        reg.encode_canonical(state).unwrap().to_string(),
    );
    RigidBodySubWorld::assemble(id, dim, cells, &reg, Some(&addon), 100.0).unwrap()
}

fn cooldown_router() -> OrbitalTransitionRouter {
    let mut router = OrbitalTransitionRouter::new();
    router.set_cooldown_ticks(40);
    router.register_dimension(
        9,
        OrbitalDimension::new("test:orbit", AtmospherePhysics::MICROGRAVITY),
    );
    router.register_rule(OrbitalTransitionRule::new(
        "test:ascend",
        0,
        OrbitalTrigger::AscendCeiling { ceiling_y: 100.0 },
        9,
    ));
    router
}

#[test]
fn orbital_cooldown_uses_absolute_server_tick_not_fleet_step_count() {
    let mut router = cooldown_router();
    let mut ship = small_ship(1, 0);
    ship.pos = [0.0, 101.0, 0.0];
    let transition = router
        .step_with_dimension_transitions_at(&mut ship, 0.05, 100)
        .expect("ship crosses at absolute tick 100");
    assert_eq!(transition.tick, 100);
    assert!(router.in_cooldown(ship.id));

    // Ten fleet members stepped in the same server tick must not advance the
    // first ship's cooldown ten times. This is the audit regression shape.
    let mut fleet: Vec<_> = (10..20).map(|id| small_ship(id, 0)).collect();
    for other in &mut fleet {
        let _ = router.step_with_dimension_transitions_at(other, 0.05, 101);
    }
    assert_eq!(router.tick(), 101);
    assert!(router.in_cooldown(ship.id));

    // After the absolute hysteresis window, cooldown expires exactly once.
    router.set_server_tick(141);
    assert!(!router.in_cooldown(ship.id));
}

#[test]
fn orbital_transfer_commit_is_single_owner_and_carries_riders() {
    let mut router = cooldown_router();
    let mut ship = small_ship(7, 0);
    ship.pos = [0.0, 101.0, 0.0];
    assert!(router.attach_passenger(501, ship.id, [0.0, 1.0, 0.0]));
    assert!(router.attach_passenger(502, ship.id, [1.0, 1.0, 0.0]));
    let transition = router
        .step_with_dimension_transitions_at(&mut ship, 0.05, 200)
        .expect("transition fires");
    assert_eq!(transition.passengers.len(), 2);

    let commits = router.drain_transfer_commits();
    assert_eq!(commits.len(), 1);
    assert_eq!(commits[0].ship_id, ship.id);
    assert_eq!((commits[0].from_dim, commits[0].to_dim), (0, 9));
    assert_eq!(commits[0].target_pos, ship.pos);
    assert_eq!(commits[0].carried_riders, [501, 502]);
    // Ownership transferred: no second consumer can commit the same transfer.
    assert!(router.drain_transfer_commits().is_empty());
}

fn straight_control(a: [f64; 3], b: [f64; 3]) -> [[f64; 3]; 4] {
    let lerp = |t: f64| {
        [
            a[0] + (b[0] - a[0]) * t,
            a[1] + (b[1] - a[1]) * t,
            a[2] + (b[2] - a[2]) * t,
        ]
    };
    [a, lerp(1.0 / 3.0), lerp(2.0 / 3.0), b]
}

fn free_train(id: u64) -> ArticulatedTrain {
    ArticulatedTrain::new(
        TrainId(id),
        vec![CarriageSpec {
            length: 4.0,
            leading_bogey_offset: 1.5,
            trailing_bogey_offset: 1.5,
            gauge: TrackGauge::Standard,
        }],
        4.0,
        0.5,
        1.0,
        TrainSchedule::default(),
    )
    .unwrap()
}

#[test]
fn offload_bridge_rail_lifecycle_ticks_replicates_and_roundtrips_snapshot() {
    let mut bridge = OffloadBridge::new();
    let dim = RailDimId::new("minecraft:overworld");
    let a = bridge.rail_add_node(dim.clone(), [0.0, 64.0, 0.0]);
    let b = bridge.rail_add_node(dim.clone(), [40.0, 64.0, 0.0]);
    let edge = bridge
        .rail_add_edge(
            a,
            b,
            TrackGauge::Standard,
            straight_control([0.0, 64.0, 0.0], [40.0, 64.0, 0.0]),
        )
        .unwrap();
    bridge.rail_add_block(&[edge]).unwrap();
    bridge
        .rail_network_mut()
        .add_train(free_train(3), edge, 0.0)
        .unwrap();
    bridge.rail_chunk_loaded(ChunkKey::new(dim, 0, 0));

    let before = bridge.rail_network().trains[&TrainId(3)].pos_s;
    let events = bridge.step_rail_network();
    let after = bridge.rail_network().trains[&TrainId(3)].pos_s;
    assert!(after > before, "train must advance virtually/loaded");
    assert!(events.iter().any(|event| matches!(
        event,
        create_adapter::rail_network::TransitEvent::RenderModeChanged { .. }
    )));
    assert!(!bridge.drain_rail_events().is_empty());
    assert!(bridge.drain_rail_events().is_empty());

    let payloads = bridge.rail_replication_payloads();
    assert_eq!(payloads.len(), 1);
    assert_eq!(payloads[0].train, TrainId(3));
    assert!(payloads[0].pos[0] > 0.0);
    assert!(payloads[0].speed > 0.0);

    let snapshot = bridge.rail_save_bytes();
    let before_positions: Vec<_> = bridge
        .rail_network()
        .trains
        .values()
        .map(|t| (t.id, t.pos_edge, t.pos_s, t.speed))
        .collect();
    let mut restored = OffloadBridge::new();
    restored.rail_load_bytes(&snapshot).unwrap();
    let after_positions: Vec<_> = restored
        .rail_network()
        .trains
        .values()
        .map(|t| (t.id, t.pos_edge, t.pos_s, t.speed))
        .collect();
    assert_eq!(before_positions, after_positions);

    // Track edits are occupancy-safe and durable through the same snapshot.
    assert!(restored.rail_remove_edge(edge));
    assert!(restored.rail_save_bytes().len() >= 13);
}

// ---- Minimal NBT writer for schematic fixtures ----
fn w_str(out: &mut Vec<u8>, name: &str, value: &str) {
    out.push(8);
    out.extend_from_slice(&(name.len() as u16).to_be_bytes());
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(&(value.len() as u16).to_be_bytes());
    out.extend_from_slice(value.as_bytes());
}
fn w_i32(out: &mut Vec<u8>, name: &str, value: i32) {
    out.push(3);
    out.extend_from_slice(&(name.len() as u16).to_be_bytes());
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(&value.to_be_bytes());
}
fn w_int_list(out: &mut Vec<u8>, name: &str, values: &[i32]) {
    out.push(9);
    out.extend_from_slice(&(name.len() as u16).to_be_bytes());
    out.extend_from_slice(name.as_bytes());
    out.push(3);
    out.extend_from_slice(&(values.len() as i32).to_be_bytes());
    for value in values {
        out.extend_from_slice(&value.to_be_bytes());
    }
}
fn w_list_header(out: &mut Vec<u8>, name: &str, elem: u8, len: i32) {
    out.push(9);
    out.extend_from_slice(&(name.len() as u16).to_be_bytes());
    out.extend_from_slice(name.as_bytes());
    out.push(elem);
    out.extend_from_slice(&len.to_be_bytes());
}
fn w_compound_header(out: &mut Vec<u8>, name: &str) {
    out.push(10);
    out.extend_from_slice(&(name.len() as u16).to_be_bytes());
    out.extend_from_slice(name.as_bytes());
}

fn schematic_with_block_entity(pos: [i32; 3], size: [i32; 3]) -> Vec<u8> {
    let mut out = vec![10, 0, 0];
    w_int_list(&mut out, "size", &size);
    w_list_header(&mut out, "palette", 10, 1);
    w_str(&mut out, "Name", "minecraft:chest");
    out.push(0);
    w_list_header(&mut out, "blocks", 10, 1);
    w_int_list(&mut out, "pos", &pos);
    w_i32(&mut out, "state", 0);
    w_compound_header(&mut out, "nbt");
    w_str(&mut out, "id", "minecraft:chest");
    w_list_header(&mut out, "Items", 10, 1);
    w_str(&mut out, "id", "minecraft:iron_ingot");
    out.push(1);
    out.extend_from_slice(&(5u16).to_be_bytes());
    out.extend_from_slice(b"Count");
    out.push(3);
    out.push(0); // end item compound
    out.push(0); // end nbt compound
    out.push(0); // end block entry
    out.push(0); // end root
    out
}

#[test]
fn schematic_retains_block_entity_nbt_and_preflight_is_atomic() {
    let bytes = schematic_with_block_entity([0, 0, 0], [2, 2, 2]);
    let structure = parse_nbt_schematic(&bytes).unwrap();
    let nbt = structure.blocks[0].nbt.as_ref().expect("block entity retained");
    let SchematicNbt::Compound(fields) = nbt else {
        panic!("block entity must decode as compound: {nbt:?}");
    };
    assert!(fields.iter().any(|(name, value)| {
        name == "id" && matches!(value, SchematicNbt::String(v) if v == "minecraft:chest")
    }));
    assert!(fields.iter().any(|(name, _)| name == "Items"));

    let mut writes = Vec::new();
    let plan = SchematicPlacementPlan::preflight(
        &structure,
        [100, 64, 100],
        |_| Some(77),
        |_| true,
    )
    .unwrap();
    plan.apply(|pos, state, nbt| writes.push((pos, state, nbt.cloned())));
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].0, [100, 64, 100]);
    assert_eq!(writes[0].1, 77);
    assert!(writes[0].2.is_some(), "placement carries retained NBT");

    // Collision rejection happens before mutation: no partial plan exists.
    let err = SchematicPlacementPlan::preflight(&structure, [100, 64, 100], |_| Some(77), |_| {
        false
    })
    .unwrap_err();
    assert!(matches!(err, SchematicPlacementError::Obstructed { .. }));

    // A block outside the declared structure bounds is rejected atomically.
    let bad = parse_nbt_schematic(&schematic_with_block_entity([2, 0, 0], [2, 2, 2])).unwrap();
    let err = SchematicPlacementPlan::preflight(&bad, [0, 64, 0], |_| Some(77), |_| true)
        .unwrap_err();
    assert!(matches!(
        err,
        SchematicPlacementError::BlockOutsideStructure { .. }
    ));
}

#[test]
fn offload_tick_advances_rail_without_factory_or_contraption() {
    let mut bridge = OffloadBridge::new();
    let dim = RailDimId::new("minecraft:overworld");
    let a = bridge.rail_add_node(dim.clone(), [0.0, 64.0, 0.0]);
    let b = bridge.rail_add_node(dim.clone(), [20.0, 64.0, 0.0]);
    let edge = bridge
        .rail_add_edge(
            a,
            b,
            TrackGauge::Standard,
            straight_control([0.0, 64.0, 0.0], [20.0, 64.0, 0.0]),
        )
        .unwrap();
    bridge
        .rail_network_mut()
        .add_train(free_train(8), edge, 0.0)
        .unwrap();
    let report = bridge.step_tick(1, OffloadBudget::new(128)).unwrap();
    assert!(bridge.rail_network().trains[&TrainId(8)].pos_s > 0.0);
    assert!(report.rail_events > 0 || bridge.rail_network().tick == 1);
}

#[test]
fn chunk_unload_keeps_train_advancing_virtually() {
    let mut bridge = OffloadBridge::new();
    let dim = RailDimId::new("minecraft:overworld");
    let a = bridge.rail_add_node(dim.clone(), [0.0, 64.0, 0.0]);
    let b = bridge.rail_add_node(dim.clone(), [40.0, 64.0, 0.0]);
    let edge = bridge
        .rail_add_edge(
            a,
            b,
            TrackGauge::Standard,
            straight_control([0.0, 64.0, 0.0], [40.0, 64.0, 0.0]),
        )
        .unwrap();
    bridge
        .rail_network_mut()
        .add_train(free_train(9), edge, 0.0)
        .unwrap();
    let chunk = ChunkKey::new(dim, 0, 0);
    bridge.rail_chunk_loaded(chunk.clone());
    bridge.step_rail_network();
    bridge.rail_chunk_unloaded(&chunk);
    let before = bridge.rail_network().trains[&TrainId(9)].pos_s;
    bridge.step_rail_network();
    assert!(bridge.rail_network().trains[&TrainId(9)].pos_s > before);
    assert_eq!(
        bridge.rail_network().trains[&TrainId(9)].render_mode,
        create_adapter::rail_network::TrainRenderMode::Virtual
    );
    let mut all = BTreeSet::new();
    all.insert(chunk);
    bridge.set_loaded_rail_chunks(all);
}
