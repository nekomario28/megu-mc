// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Lane WIRE-MINECUT-ORBITAL-SYNC: production-wiring proof that the live
//! [`OffloadBridge`] owns both surfaces the wiring audit previously flagged
//! as un-consumed:
//!
//! - `mod_offload::sync_bridge::OffloadSyncBridge` incremental dirty-node
//!   delta ingestion via [`OffloadBridge::ingest_external_deltas`] —
//!   kinetic/fluid/transport deltas land in the bridge-owned external
//!   [`Region`], `Me*` deltas route to the [`MeGraphTarget`], same-batch
//!   state coalescing, unchanged-delta skipping and event re-delivery all
//!   behave exactly as the standalone bridge documents;
//! - `create_adapter::orbital::OrbitalTransitionRouter` per-tick ship
//!   stepping via [`OffloadBridge::step_tick`] — a registered 6-DOF ship
//!   climbing through the `Y >= 560` ceiling transitions atomically into
//!   space (`ShipDimensionTransition` queued for the wire layer), while an
//!   empty bridge pays exactly zero orbital cost.

use std::collections::BTreeMap;

use create_adapter::addon_profile::{
    create_registry_with_addons, createaeronautics_profile, CreateAddonProfile,
};
use create_adapter::aeronautics::{AtmospherePhysics, RigidBodySubWorld};
use create_adapter::blockstates::{CreateBlockRegistry, DEFAULT_BASE_STATE_ID};
use create_adapter::offload_bridge::{
    BridgeError, ExternalGraphDelta, OffloadBridge, OffloadBudget,
};
use create_adapter::orbital::{OrbitalDimension, OrbitalTransitionRule, OrbitalTrigger};
use mod_offload::kinetic_graph::KineticNode;

const DIM_OVERWORLD: u32 = 0;
const DIM_ORBIT: u32 = 9;
const ORBIT_ID: &str = "test:earth_orbit";
const CEILING_Y: f64 = 560.0;
const ORBIT_SPAWN_Y: f64 = 620.0;
const BLOCK_MASS: f64 = 100.0;

fn canonical(reg: &CreateBlockRegistry, id: &str) -> String {
    reg.encode_canonical(reg.default_state(id).unwrap())
        .unwrap()
        .to_string()
}

fn fleet() -> (CreateAddonProfile, CreateBlockRegistry) {
    let addon = createaeronautics_profile();
    let reg =
        create_registry_with_addons(DEFAULT_BASE_STATE_ID, std::slice::from_ref(&addon)).unwrap();
    (addon, reg)
}

/// Minimal airborne hull — one shaft block is enough mass for the router;
/// the climb is driven by an assigned upward velocity, not propellers.
fn hull(reg: &CreateBlockRegistry) -> BTreeMap<[i32; 3], String> {
    let mut cells = BTreeMap::new();
    cells.insert([0, 0, 0], canonical(reg, "create:shaft"));
    cells
}

/// External/JVM graph deltas ingest through `ingest_external_deltas` with
/// the full `OffloadSyncBridge` contract: last-wins coalescing, skip on
/// unchanged re-delivery, event deltas always applied, fail-closed errors.
#[test]
fn offload_bridge_ingests_external_deltas_incrementally() {
    let mut bridge = OffloadBridge::new();
    // No ships registered: the tick pays exactly zero orbital cost.
    assert_eq!(bridge.orbital_ship_count(), 0);

    let node = KineticNode {
        capacity: 128.0,
        load: 0.0,
        driven_speed: Some(64.0),
    };
    let report = bridge
        .ingest_external_deltas(&[
            ExternalGraphDelta::KineticUpsert { id: 7, node },
            ExternalGraphDelta::TransportMachine {
                id: 3,
                capacity: 16,
                delay: 10,
            },
            ExternalGraphDelta::TransportInsert { id: 3, item: 42 },
            ExternalGraphDelta::TransportInsert { id: 3, item: 43 },
            // Same-batch duplicate state delta coalesces last-wins.
            ExternalGraphDelta::KineticUpsert { id: 7, node },
        ])
        .expect("batch within MAX_INGEST_DELTAS");
    assert_eq!(report.received, 5);
    assert_eq!(report.coalesced, 1);
    assert_eq!(report.applied, 4);
    assert!(report.failed.is_empty());

    // Kernel state landed in the bridge-owned external region.
    let target = bridge.external_graph_target();
    let solution = target
        .region()
        .kinetic
        .solution(7)
        .expect("ingested kinetic node solves");
    assert_eq!(solution.capacity, 128.0);
    let machine = target
        .region()
        .transport()
        .machine(3)
        .expect("ingested transport machine exists");
    assert_eq!(machine.input_items, 2);

    // Re-ingesting an unchanged state delta is skipped; the event delta
    // re-delivers (events are never deduplicated).
    let report = bridge
        .ingest_external_deltas(&[
            ExternalGraphDelta::KineticUpsert { id: 7, node },
            ExternalGraphDelta::TransportInsert { id: 3, item: 44 },
        ])
        .unwrap();
    assert_eq!(report.skipped, 1);
    assert_eq!(report.applied, 1);
    assert_eq!(
        bridge
            .external_graph_target()
            .region()
            .transport()
            .machine(3)
            .unwrap()
            .input_items,
        3
    );

    // Fail-closed: a malformed delta reports its failure without applying.
    let report = bridge
        .ingest_external_deltas(&[ExternalGraphDelta::FluidSet {
            id: 99,
            fluid: None,
            amount: 5,
        }])
        .unwrap();
    assert_eq!(report.failed.len(), 1);
    assert!(matches!(
        report.failed[0].error,
        BridgeError::InvalidDelta(_)
    ));
}

/// An ME delta lands in the bridge's `MeGraphTarget`, proving the
/// composite sink routes `Me*` families past the `Region` (which would
/// reject them as unsupported).
#[test]
fn offload_bridge_routes_me_deltas_to_me_target() {
    use mod_offload::me_network::MeNodeKind;

    let mut bridge = OffloadBridge::new();
    let report = bridge
        .ingest_external_deltas(&[ExternalGraphDelta::MeNodeUpsert {
            id: 11,
            kind: MeNodeKind::Device {
                requires_channel: false,
            },
        }])
        .unwrap();
    assert_eq!(report.applied, 1);
    assert!(report.failed.is_empty());
    assert!(bridge.external_graph_target().me().channels.node_kind(11).is_some());
}

/// A registered 6-DOF ship stepping through `OffloadBridge::step_tick`
/// crosses the `Y >= 560` ceiling atomically into the orbit dimension —
/// the same `OrbitalTransitionRouter` semantics the orbital suite pins,
/// now executed by the live bridge tick.
#[test]
fn offload_bridge_steps_orbital_ship_across_space_boundary() {
    let (addon, reg) = fleet();
    let mut bridge = OffloadBridge::new();
    bridge.register_orbital_dimension(
        DIM_ORBIT,
        OrbitalDimension::new(ORBIT_ID, AtmospherePhysics::MICROGRAVITY),
    );
    bridge.register_orbital_rule(
        OrbitalTransitionRule::new(
            "test:overworld_orbit_ceiling",
            DIM_OVERWORLD,
            OrbitalTrigger::AscendCeiling {
                ceiling_y: CEILING_Y,
            },
            DIM_ORBIT,
        )
        .with_spawn_y(ORBIT_SPAWN_Y),
    );

    let mut ship =
        RigidBodySubWorld::assemble(1, DIM_OVERWORLD, hull(&reg), &reg, Some(&addon), BLOCK_MASS)
            .unwrap();
    // Ballistic climb out of the Overworld: one tick at +60 m/s clears the
    // 560 ceiling; gravity only shaves ~0.49 m/s off that tick.
    ship.pos = [12.5, 559.0, -30.0];
    ship.vel = [0.0, 60.0, 0.0];
    bridge.register_orbital_ship(ship).unwrap();
    assert!(bridge.attach_orbital_passenger(1001, 1, [0.0, 1.0, 0.0]));

    let report = bridge.step_tick(1, OffloadBudget::new(1_024)).unwrap();
    assert_eq!(report.orbital_transitions, 1);

    // The ship re-parented atomically; the transition record queues for
    // the wire layer with the rider's TeleportIntent.
    let ship = bridge.orbital_ship(1).expect("ship stays registered");
    assert_eq!(ship.parent_dim, DIM_ORBIT);
    assert!((ship.pos[1] - ORBIT_SPAWN_Y).abs() < 1e-9);

    let transitions = bridge.take_orbital_transitions();
    assert_eq!(transitions.len(), 1);
    let transition = &transitions[0];
    assert_eq!(transition.ship_id, 1);
    assert_eq!(transition.from_dim, DIM_OVERWORLD);
    assert_eq!(transition.to_dim, DIM_ORBIT);
    assert_eq!(transition.to_dim_id, ORBIT_ID);
    assert_eq!(transition.rule_name, "test:overworld_orbit_ceiling");
    assert_eq!(transition.passengers.len(), 1);
    assert_eq!(transition.passengers[0].entity_id, 1001);

    // Second take drains nothing; the cooldown/orbit dim also stops any
    // immediate re-fire on the next tick.
    assert!(bridge.take_orbital_transitions().is_empty());
    let report = bridge.step_tick(2, OffloadBudget::new(1_024)).unwrap();
    assert_eq!(report.orbital_transitions, 0);
}
