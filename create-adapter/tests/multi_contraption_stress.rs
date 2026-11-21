// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Multi-contraption / tall-dimension determinism suite (Lane H19).
//!
//! Several independent 6-DOF Aeronautics airships — each carrying a live
//! embedded [`CreateFactory`] kinetic network — fly simultaneously across
//! isolated dimensions: the Overworld (`dim 0`), the Nether (`dim 1`), and a
//! tall custom dimension (`dim 7`) cruising at `Y = +800` in buoyancy
//! equilibrium. The suite proves:
//!
//! - live kinetic propellers keep producing real thrust in every dimension,
//! - re-running the same 500-tick scenario yields **byte-identical** `AERO2`
//!   snapshots (fully deterministic physics + serialization),
//! - a mid-flight `AERO2` save/reload at tick 250 plus 250 more ticks lands
//!   byte-for-byte on the uninterrupted 500-tick result.

use std::collections::BTreeMap;

use create_adapter::addon_profile::{
    create_registry_with_addons, createaeronautics_profile, CreateAddonProfile,
};
use create_adapter::aeronautics::{
    decode_snapshot_with_data_version, encode_aeronautics_snapshot, RigidBodySubWorld,
    AERO_DATA_VERSION_1_21_1, AERO_DATA_VERSION_26_3, SEA_LEVEL_AIR_DENSITY,
};
use create_adapter::blockstates::{CreateBlockRegistry, DEFAULT_BASE_STATE_ID};
use create_adapter::factory::DimBlockPos;
use mod_offload::kinetic_graph::NetworkStatus;

const DT: f64 = 0.05;
const GRAVITY: f64 = 9.81;
const BLOCK_MASS: f64 = 100.0;
const FLIGHT_TICKS: u64 = 500;
const MID_FLIGHT_TICKS: u64 = 250;

const DIM_OVERWORLD: u32 = 0;
const DIM_NETHER: u32 = 1;
const DIM_TALL: u32 = 7;
const TALL_CRUISE_Y: f64 = 800.0;

fn canonical(reg: &CreateBlockRegistry, id: &str) -> String {
    reg.encode_canonical(reg.default_state(id).unwrap())
        .unwrap()
        .to_string()
}

fn canonical_with(reg: &CreateBlockRegistry, id: &str, prop: &str, val: &str) -> String {
    let st = reg
        .with_property(reg.default_state(id).unwrap(), prop, val)
        .unwrap();
    reg.encode_canonical(st).unwrap().to_string()
}

/// Reference dual-ship addon set: the built-in 1.21.x Aeronautics profile
/// plus a tall-dimension variant whose lift band is retuned so envelopes
/// hold buoyancy equilibrium at `Y = +800` (the stock profile's
/// `max_lift_altitude` of 320 gives zero lift up there).
struct Fleet {
    reg: CreateBlockRegistry,
    aero: CreateAddonProfile,
    tall: CreateAddonProfile,
}

fn fleet() -> Fleet {
    let aero = createaeronautics_profile();
    let mut tall = createaeronautics_profile();
    let mut spec = tall.propulsion.clone().unwrap();
    spec.lift_full_altitude = TALL_CRUISE_Y;
    spec.max_lift_altitude = TALL_CRUISE_Y * 2.0;
    // The tall ship carries 6 blocks (600 kg) on 2 envelopes; at the
    // full-lift taper 2 * lift_per_block == mass * g bit-exactly, pinning
    // buoyancy equilibrium at Y = +800.
    spec.lift_per_buoyancy_block = 6.0 * BLOCK_MASS * GRAVITY / 2.0;
    tall.propulsion = Some(spec);
    let reg =
        create_registry_with_addons(DEFAULT_BASE_STATE_ID, std::slice::from_ref(&aero)).unwrap();
    Fleet { reg, aero, tall }
}

fn assemble(
    fleet: &Fleet,
    id: u64,
    dim: u32,
    cells: BTreeMap<[i32; 3], String>,
    tall_variant: bool,
    pos: [f64; 3],
) -> RigidBodySubWorld {
    let addon = if tall_variant {
        &fleet.tall
    } else {
        &fleet.aero
    };
    let mut sub =
        RigidBodySubWorld::assemble(id, dim, cells, &fleet.reg, Some(addon), BLOCK_MASS).unwrap();
    sub.pos = pos;
    sub
}

/// The three-ship, three-dimension scenario: every ship mounts a live
/// `propeller_engine -> shaft -> propeller_bearing` kinetic chain that the
/// embedded factory solves at real RPM, plus lift envelopes. The Overworld
/// ship also carries a weathercock rudder so angular dynamics (yaw torque,
/// Euler spin integration, quaternion precession) run under load.
fn build_ships() -> (Fleet, Vec<RigidBodySubWorld>) {
    let fleet = fleet();
    let reg = &fleet.reg;

    // Overworld ship: +X thrust, one envelope (net sinker under gravity).
    let mut cells = BTreeMap::new();
    cells.insert(
        [0, 0, 0],
        canonical_with(reg, "createaeronautics:propeller_engine", "axis", "x"),
    );
    cells.insert([1, 0, 0], canonical_with(reg, "create:shaft", "axis", "x"));
    cells.insert(
        [2, 0, 0],
        canonical_with(reg, "createaeronautics:propeller_bearing", "facing", "east"),
    );
    cells.insert(
        [0, 1, 0],
        canonical(reg, "createaeronautics:balloon_envelope"),
    );
    cells.insert([0, 0, 1], canonical(reg, "createaeronautics:rudder"));
    let overworld = assemble(&fleet, 1, DIM_OVERWORLD, cells, false, [0.0, 100.0, 0.0]);

    // Nether ship: +Z thrust chain, two envelopes.
    let mut cells = BTreeMap::new();
    cells.insert(
        [0, 0, 0],
        canonical_with(reg, "createaeronautics:propeller_engine", "axis", "z"),
    );
    cells.insert([0, 0, 1], canonical_with(reg, "create:shaft", "axis", "z"));
    cells.insert(
        [0, 0, 2],
        canonical_with(
            reg,
            "createaeronautics:propeller_bearing",
            "facing",
            "south",
        ),
    );
    cells.insert(
        [0, 1, 0],
        canonical(reg, "createaeronautics:balloon_envelope"),
    );
    cells.insert(
        [1, 1, 0],
        canonical(reg, "createaeronautics:balloon_envelope"),
    );
    let nether = assemble(&fleet, 2, DIM_NETHER, cells, false, [0.0, 60.0, 0.0]);

    // Tall custom dimension ship at Y = +800: deck + hull assembler, two
    // envelopes retuned for exact buoyancy equilibrium at cruise altitude,
    // +X thrust chain.
    let mut cells = BTreeMap::new();
    cells.insert(
        [0, 0, 0],
        canonical(reg, "createaeronautics:airship_assembler"),
    );
    cells.insert(
        [1, 0, 0],
        canonical(reg, "createaeronautics:airship_assembler"),
    );
    cells.insert(
        [0, 1, 0],
        canonical(reg, "createaeronautics:balloon_envelope"),
    );
    cells.insert(
        [1, 1, 0],
        canonical(reg, "createaeronautics:balloon_envelope"),
    );
    cells.insert(
        [2, 0, 0],
        canonical_with(reg, "createaeronautics:propeller_engine", "axis", "x"),
    );
    cells.insert(
        [3, 0, 0],
        canonical_with(reg, "createaeronautics:propeller_bearing", "facing", "east"),
    );
    let tall = assemble(&fleet, 3, DIM_TALL, cells, true, [0.0, TALL_CRUISE_Y, 0.0]);

    (fleet, vec![overworld, nether, tall])
}

fn fly(ships: &mut [RigidBodySubWorld], ticks: u64) {
    for _ in 0..ticks {
        for ship in ships.iter_mut() {
            ship.step(DT, GRAVITY, SEA_LEVEL_AIR_DENSITY);
        }
    }
}

fn speed_of(ship: &RigidBodySubWorld) -> f64 {
    (ship.vel[0] * ship.vel[0] + ship.vel[1] * ship.vel[1] + ship.vel[2] * ship.vel[2]).sqrt()
}

fn aero2_fleet_snapshots(ships: &[RigidBodySubWorld], data_version: i32) -> Vec<Vec<u8>> {
    ships
        .iter()
        .map(|s| s.encode_snapshot_for_data_version(data_version))
        .collect()
}

fn assert_finite(ship: &RigidBodySubWorld) {
    for v in ship
        .pos
        .iter()
        .chain(&ship.vel)
        .chain(&ship.omega)
        .chain(&ship.quat)
    {
        assert!(v.is_finite(), "non-finite state in ship {}", ship.id);
    }
}

#[test]
fn multi_dimension_fleet_flies_with_live_kinetics_buoyancy_and_carry_deltas() {
    let (_fleet, mut ships) = build_ships();
    assert_eq!(ships.len(), 3);
    let dims: Vec<u32> = ships.iter().map(|s| s.parent_dim).collect();
    assert_eq!(dims, [DIM_OVERWORLD, DIM_NETHER, DIM_TALL]);

    // Live kinetic propellers: every ship's embedded factory solves its
    // propeller mounts at Running RPM in its OWN dimension, and the same
    // local coordinates in a foreign dimension resolve to nothing
    // (per-dimension isolation).
    for ship in &ships {
        assert!(
            !ship.propellers.is_empty(),
            "ship {} has no propellers",
            ship.id
        );
        for mount in &ship.propellers {
            let pos = DimBlockPos::new(
                ship.parent_dim as i32,
                mount.local[0],
                mount.local[1],
                mount.local[2],
            );
            let sol = ship
                .factory()
                .kinetic_solution(pos)
                .unwrap_or_else(|| panic!("ship {} mount {:?} unsolved", ship.id, mount.local));
            assert_eq!(
                sol.status,
                NetworkStatus::Running,
                "ship {} mount {:?} not spinning",
                ship.id,
                mount.local
            );
            assert!(sol.speed.abs() >= 4.0, "ship {} RPM too low", ship.id);
            for other in [DIM_OVERWORLD, DIM_NETHER, DIM_TALL] {
                if other == ship.parent_dim {
                    continue;
                }
                let foreign =
                    DimBlockPos::new(other as i32, mount.local[0], mount.local[1], mount.local[2]);
                assert!(
                    ship.factory().kinetic_solution(foreign).is_none(),
                    "ship {} kinetic state leaked into dim {other}",
                    ship.id
                );
            }
        }
    }

    // Early flight: every ship's propellers convert solved RPM into real
    // acceleration along its mount axis (ship 1's weathercock rudder also
    // starts yawing it, so it is checked on speed, not heading).
    fly(&mut ships, 50);
    assert!(
        speed_of(&ships[0]) > 5.0,
        "overworld ship produced no thrust"
    );
    assert!(ships[1].vel[2] > 0.0, "nether ship produced no +Z thrust");
    assert!(ships[2].vel[0] > 0.0, "tall ship produced no +X thrust");
    assert!((ships[2].pos[1] - TALL_CRUISE_Y).abs() < 1e-9 && ships[2].vel[1] == 0.0);

    fly(&mut ships, FLIGHT_TICKS - 50);

    for ship in &ships {
        assert_finite(ship);
        assert_eq!(ship.tick(), FLIGHT_TICKS);
        // Sustained powered flight: every ship still carries real speed.
        assert!(
            speed_of(ship) > 1.0,
            "ship {} is not flying: vel={:?}",
            ship.id,
            ship.vel
        );
        // Every ship left its origin (horizontal displacement).
        let horizontal = (ship.pos[0] * ship.pos[0] + ship.pos[2] * ship.pos[2]).sqrt();
        assert!(horizontal > 1.0, "ship {} did not move", ship.id);
    }

    // The rudder ship genuinely exercised 6-DOF angular dynamics: nonzero
    // world-frame yaw rate and a rotated (still unit) quaternion.
    assert!(
        ships[0].omega[1].abs() > 1e-6,
        "rudder never torqued ship 1"
    );
    assert_ne!(ships[0].quat, [0.0, 0.0, 0.0, 1.0]);
    let qn = ships[0].quat.iter().map(|q| q * q).sum::<f64>().sqrt();
    assert!((qn - 1.0).abs() < 1e-9);

    // The rudder-free ships fly a perfectly straight course: no angular
    // state ever developed.
    for ship in &ships[1..] {
        assert_eq!(ship.omega, [0.0; 3]);
        assert_eq!(ship.quat, [0.0, 0.0, 0.0, 1.0]);
    }

    // Tall-dimension ship holds buoyancy equilibrium at Y = +800 while its
    // propellers still drive it forward (+X).
    let tall = &ships[2];
    assert_eq!(tall.buoyancy_blocks, 2);
    assert!(
        tall.vel[1].abs() < 1e-9,
        "tall ship drifted from equilibrium: vel={:?}",
        tall.vel
    );
    assert!((tall.pos[1] - TALL_CRUISE_Y).abs() < 1e-9);
    assert!(tall.vel[0] > 0.0 && tall.pos[0] > 0.0);

    // Passenger carry deltas track each ship's live velocity (anti-cheat
    // hook): finite and nonzero mid-flight.
    for ship in &ships {
        let delta = ship.passenger_world_delta([0.5, 1.0, -0.5], DT);
        assert!(delta.iter().all(|d| d.is_finite()));
        assert!(
            delta.iter().any(|d| d.abs() > 0.0),
            "ship {} produced a zero carry delta",
            ship.id
        );
    }
    // Exact formula pin on the tall ship (mirrors the unit-level invariant).
    let r_world = {
        let q = tall.quat;
        let o = [0.5, 1.0, -0.5];
        let qv = [q[0], q[1], q[2]];
        let t = [
            qv[1] * o[2] - qv[2] * o[1],
            qv[2] * o[0] - qv[0] * o[2],
            qv[0] * o[1] - qv[1] * o[0],
        ];
        let t2 = [2.0 * t[0], 2.0 * t[1], 2.0 * t[2]];
        let u = [
            qv[1] * t2[2] - qv[2] * t2[1],
            qv[2] * t2[0] - qv[0] * t2[2],
            qv[0] * t2[1] - qv[1] * t2[0],
        ];
        [
            o[0] + q[3] * t2[0] + u[0],
            o[1] + q[3] * t2[1] + u[1],
            o[2] + q[3] * t2[2] + u[2],
        ]
    };
    let tangential = [
        tall.omega[1] * r_world[2] - tall.omega[2] * r_world[1],
        tall.omega[2] * r_world[0] - tall.omega[0] * r_world[2],
        tall.omega[0] * r_world[1] - tall.omega[1] * r_world[0],
    ];
    let expect = [
        (tall.vel[0] + tangential[0]) * DT,
        (tall.vel[1] + tangential[1]) * DT,
        (tall.vel[2] + tangential[2]) * DT,
    ];
    let got = tall.passenger_world_delta([0.5, 1.0, -0.5], DT);
    assert_eq!(got, expect);
}

#[test]
fn rerun_of_500_tick_multi_ship_scenario_is_byte_identical() {
    let (_f1, mut ships_a) = build_ships();
    let (_f2, mut ships_b) = build_ships();
    fly(&mut ships_a, FLIGHT_TICKS);
    fly(&mut ships_b, FLIGHT_TICKS);

    for dv in [AERO_DATA_VERSION_1_21_1, AERO_DATA_VERSION_26_3] {
        let a = aero2_fleet_snapshots(&ships_a, dv);
        let b = aero2_fleet_snapshots(&ships_b, dv);
        assert_eq!(a, b, "data_version {dv}: rerun diverged");
    }
    // Bare `aero_v1` encodings agree too (envelope payload is unchanged).
    let v1_a: Vec<Vec<u8>> = ships_a.iter().map(encode_aeronautics_snapshot).collect();
    let v1_b: Vec<Vec<u8>> = ships_b.iter().map(encode_aeronautics_snapshot).collect();
    assert_eq!(v1_a, v1_b);
}

#[test]
fn mid_flight_aero2_reload_matches_uninterrupted_500_tick_flight() {
    let (_f, mut reference) = build_ships();
    fly(&mut reference, FLIGHT_TICKS);
    let reference_snaps = aero2_fleet_snapshots(&reference, AERO_DATA_VERSION_26_3);

    // Interrupted run: save `AERO2` envelopes mid-flight at tick 250, decode
    // them back (rebuilding each embedded factory from canonical strings),
    // then fly the remaining 250 ticks.
    let (fleet, mut interrupted) = build_ships();
    fly(&mut interrupted, MID_FLIGHT_TICKS);
    let mid_snaps = aero2_fleet_snapshots(&interrupted, AERO_DATA_VERSION_26_3);
    let mut restored: Vec<RigidBodySubWorld> = mid_snaps
        .iter()
        .map(|snap| {
            let (sub, dv) =
                decode_snapshot_with_data_version(snap, &fleet.reg).expect("mid-flight snapshot");
            assert_eq!(dv, AERO_DATA_VERSION_26_3);
            sub
        })
        .collect();
    fly(&mut restored, MID_FLIGHT_TICKS);
    let resumed_snaps = aero2_fleet_snapshots(&restored, AERO_DATA_VERSION_26_3);
    assert_eq!(
        resumed_snaps, reference_snaps,
        "save/reload at tick 250 diverged from the uninterrupted flight"
    );

    // The same mid-flight continuity holds through the legacy `aero_v1`
    // codec: a bare `AERO` v1 snapshot restores (reporting data_version
    // 3955) and the remaining flight lands byte-identically.
    let (_f2, mut legacy_run) = build_ships();
    fly(&mut legacy_run, MID_FLIGHT_TICKS);
    let legacy_mid: Vec<Vec<u8>> = legacy_run.iter().map(encode_aeronautics_snapshot).collect();
    let mut legacy_restored: Vec<RigidBodySubWorld> = legacy_mid
        .iter()
        .map(|snap| {
            let (sub, dv) =
                decode_snapshot_with_data_version(snap, &fleet.reg).expect("legacy snapshot");
            assert_eq!(dv, AERO_DATA_VERSION_1_21_1);
            sub
        })
        .collect();
    fly(&mut legacy_restored, MID_FLIGHT_TICKS);
    let legacy_final = aero2_fleet_snapshots(&legacy_restored, AERO_DATA_VERSION_26_3);
    assert_eq!(legacy_final, reference_snaps);
}
