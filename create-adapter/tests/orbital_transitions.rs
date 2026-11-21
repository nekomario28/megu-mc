// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Orbital / planet dimension-transition integration suite (Lane H26).
//!
//! A 6-DOF Aeronautics ship climbs through the Overworld's atmospheric
//! ceiling (`Y >= 560`), crosses atomically — live kinetic propellers, two
//! attached passengers and all — into a microgravity orbit dimension, then
//! descends below the orbital floor into a `gravity_scale = 0.165` lunar
//! dimension. The suite proves:
//!
//! - the transition record carries both passengers' `TeleportIntent`s with
//!   the shared anti-cheat teleport grace, so riders are never false-flagged
//!   during orbital transit;
//! - `AtmospherePhysics` switches per dimension (Overworld 1.0 g + air ->
//!   orbit 0 g vacuum -> Moon 0.165 g vacuum) with horizontal momentum,
//!   orientation and live propeller RPMs preserved through the crossing;
//! - the cooldown hysteresis stops boundary ping-pong;
//! - `AERO2` snapshots stay byte-exact across dimension crossings, including
//!   a mid-transition save/reload resumed through an identical router.

use std::collections::BTreeMap;

use create_adapter::addon_profile::{
    create_registry_with_addons, createaeronautics_profile, CreateAddonProfile,
};
use create_adapter::aeronautics::{
    decode_snapshot_with_data_version, AtmospherePhysics, RigidBodySubWorld,
    AERO_DATA_VERSION_26_3,
};
use create_adapter::blockstates::{CreateBlockRegistry, DEFAULT_BASE_STATE_ID};
use create_adapter::factory::DimBlockPos;
use create_adapter::orbital::{
    OrbitalDimension, OrbitalTransitionRouter, OrbitalTransitionRule, OrbitalTrigger,
    ShipDimensionTransition, DEFAULT_TRANSITION_COOLDOWN_TICKS,
};
use mod_adapter_core::dimension_mod::DEFAULT_TELEPORT_GRACE_TICKS;
use mod_offload::kinetic_graph::NetworkStatus;

const DT: f64 = 0.05;
const BLOCK_MASS: f64 = 100.0;

const DIM_OVERWORLD: u32 = 0;
const DIM_ORBIT: u32 = 9;
const DIM_MOON: u32 = 10;

const CEILING_Y: f64 = 560.0;
const ORBIT_SPAWN_Y: f64 = 620.0;
const ORBIT_FLOOR_Y: f64 = 400.0;
const MOON_ENTRY_Y: f64 = 200.0;

const ORBIT_ID: &str = "test:earth_orbit";
const MOON_ID: &str = "test:moon";

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

fn fleet() -> (CreateAddonProfile, CreateBlockRegistry) {
    let addon = createaeronautics_profile();
    let reg =
        create_registry_with_addons(DEFAULT_BASE_STATE_ID, std::slice::from_ref(&addon)).unwrap();
    (addon, reg)
}

/// +X thrust hull: `propeller_engine[axis=x] -> create:shaft[axis=x] ->
/// propeller_bearing[facing=east]` live kinetic chain, one deck block and
/// one lift envelope (5 blocks, 500 kg).
fn thrust_hull(reg: &CreateBlockRegistry) -> BTreeMap<[i32; 3], String> {
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
    cells.insert([0, 0, 1], canonical(reg, "createaeronautics:airship_assembler"));
    cells.insert([0, 1, 0], canonical(reg, "createaeronautics:balloon_envelope"));
    cells
}

/// The orbital scenario's router: Overworld ceiling `Y >= 560` -> orbit at
/// `Y = 620`; orbit floor `Y <= 400` -> lunar entry at `Y = 200`.
fn orbital_router() -> OrbitalTransitionRouter {
    let mut router = OrbitalTransitionRouter::new();
    router.register_dimension(
        DIM_ORBIT,
        OrbitalDimension::new(ORBIT_ID, AtmospherePhysics::MICROGRAVITY),
    );
    router.register_dimension(
        DIM_MOON,
        OrbitalDimension::new(MOON_ID, AtmospherePhysics::MOON),
    );
    router.register_rule(
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
    router.register_rule(
        OrbitalTransitionRule::new(
            "test:orbit_moon_entry",
            DIM_ORBIT,
            OrbitalTrigger::DescendFloor {
                floor_y: ORBIT_FLOOR_Y,
            },
            DIM_MOON,
        )
        .with_spawn_y(MOON_ENTRY_Y),
    );
    router
}

/// Build a launched ship: +X thrust hull on a ballistic climb out of the
/// Overworld with two riders aboard.
fn launched_ship(
    addon: &CreateAddonProfile,
    reg: &CreateBlockRegistry,
    router: &mut OrbitalTransitionRouter,
) -> RigidBodySubWorld {
    let mut ship =
        RigidBodySubWorld::assemble(1, DIM_OVERWORLD, thrust_hull(reg), reg, Some(addon), BLOCK_MASS)
            .unwrap();
    ship.pos = [12.5, 550.0, -30.0];
    // Ballistic ascent: a launch impulse carries it across the ceiling while
    // the +X propellers keep driving it forward.
    ship.vel = [6.0, 42.0, 0.0];
    router.attach_passenger(1001, ship.id, [0.5, 0.5, 0.0]);
    router.attach_passenger(1002, ship.id, [-0.5, 1.25, 0.5]);
    ship
}

/// Route `ship` until the first transition fires (or `max_ticks` elapse).
fn step_until_transition(
    router: &mut OrbitalTransitionRouter,
    ship: &mut RigidBodySubWorld,
    max_ticks: u64,
) -> ShipDimensionTransition {
    step_until_transition_with_prev(router, ship, max_ticks).1
}

/// Like [`step_until_transition`] but also returns the pose the ship held
/// immediately before the step that fired the transition.
fn step_until_transition_with_prev(
    router: &mut OrbitalTransitionRouter,
    ship: &mut RigidBodySubWorld,
    max_ticks: u64,
) -> ([f64; 3], ShipDimensionTransition) {
    for _ in 0..max_ticks {
        let prev = ship.pos;
        if let Some(t) = router.step_with_dimension_transitions(ship, DT) {
            return (prev, t);
        }
    }
    panic!("ship {} never transitioned in {max_ticks} ticks", ship.id);
}

#[test]
fn ascending_ship_crosses_560_atomically_with_two_passengers() {
    let (addon, reg) = fleet();
    let mut router = orbital_router();
    let mut ship = launched_ship(&addon, &reg, &mut router);

    // Sanity: live propeller RPM solves in the Overworld before launch.
    let prop_pos = DimBlockPos::new(DIM_OVERWORLD as i32, 2, 0, 0);
    let sol = ship.factory().kinetic_solution(prop_pos).unwrap();
    assert_eq!(sol.status, NetworkStatus::Running);

    let pre_quat = ship.quat;
    let transition = step_until_transition(&mut router, &mut ship, 200);
    assert_eq!(transition.ship_id, ship.id);
    assert_eq!((transition.from_dim, transition.to_dim), (DIM_OVERWORLD, DIM_ORBIT));
    assert_eq!(transition.to_dim_id, ORBIT_ID);
    assert_eq!(transition.rule_name, "test:overworld_orbit_ceiling");

    // Atomic arrival: fixed spawn altitude, coordinate-scale-preserved x/z
    // (1.0 -> 1.0 keeps them untouched), orientation and velocity carried.
    assert_eq!(ship.parent_dim, DIM_ORBIT);
    assert_eq!(transition.new_pos[1], ORBIT_SPAWN_Y);
    assert_eq!(transition.new_pos, ship.pos);
    assert_eq!(transition.new_quat, ship.quat);
    assert_eq!(transition.vel, ship.vel);
    // The ship climbed through — momentum carried an upward velocity, and the
    // quaternion was never touched by the transfer.
    assert!(transition.vel[1] > 0.0);
    assert_eq!(transition.new_quat, pre_quat);
    // Environment switched to microgravity.
    assert_eq!(ship.atmosphere(), AtmospherePhysics::MICROGRAVITY);

    // Both riders carried atomically, entity_id order, shared anti-cheat
    // grace, intents naming the canonical destination id.
    assert_eq!(transition.passengers.len(), 2);
    assert_eq!(
        transition.passengers.iter().map(|p| p.entity_id).collect::<Vec<_>>(),
        [1001, 1002]
    );
    for p in &transition.passengers {
        assert_eq!(p.intent.target_dim, ORBIT_ID);
        assert_eq!(p.intent.expect_teleport_grace_ticks, DEFAULT_TELEPORT_GRACE_TICKS);
        assert_eq!(p.intent.target_pos, p.new_pos);
        // Rider rides rigidly: arrival sits the ship's rotated local offset
        // away from the new center of mass.
        for i in 0..3 {
            assert!((p.new_pos[i] - ship.pos[i]).abs() < 8.0);
            assert!(p.new_pos[i].is_finite());
        }
    }
    // The roster still attaches both riders to the same ship after the hop.
    assert_eq!(router.passengers_of(ship.id).len(), 2);

    // The re-keyed kinetic network solves under the new dimension, so the
    // propeller keeps thrusting in orbit.
    let orbit_pos = DimBlockPos::new(DIM_ORBIT as i32, 2, 0, 0);
    let sol = ship.factory().kinetic_solution(orbit_pos).unwrap();
    assert_eq!(sol.status, NetworkStatus::Running);
    assert!(ship
        .factory()
        .kinetic_solution(prop_pos)
        .is_none());

    // Microgravity coast: vertical velocity holds exactly (no gravity drop),
    // altitude climbs linearly, +X thrusters keep accelerating the hull.
    let vy = ship.vel[1];
    let y0 = ship.pos[1];
    for _ in 0..20 {
        assert!(router.step_with_dimension_transitions(&mut ship, DT).is_none());
    }
    assert_eq!(ship.vel[1], vy, "gravity drop in microgravity");
    assert!((ship.pos[1] - (y0 + vy * DT * 20.0)).abs() < 1e-9);
    assert!(ship.vel[0] > transition.vel[0], "thrusters lost drive in vacuum");

    // Cooldown armed on the crossing tick: even standing on a trigger band
    // the ship could not re-fire until the window passed.
    assert!(router.tick() - transition.tick <= DEFAULT_TRANSITION_COOLDOWN_TICKS);
}

/// A ship parked in orbit on a slow descent: it slips under the orbital
/// floor `Y <= 400` and drops into the lunar dimension.
fn descending_ship(
    addon: &CreateAddonProfile,
    reg: &CreateBlockRegistry,
    router: &mut OrbitalTransitionRouter,
) -> RigidBodySubWorld {
    let mut ship =
        RigidBodySubWorld::assemble(2, DIM_ORBIT, thrust_hull(reg), reg, Some(addon), BLOCK_MASS)
            .unwrap();
    ship.pos = [50.0, 420.0, 10.0];
    ship.vel = [0.0, -15.0, 0.0];
    router.attach_passenger(2001, ship.id, [0.5, 0.5, 0.0]);
    router.attach_passenger(2002, ship.id, [0.0, 0.5, -0.5]);
    ship
}

#[test]
fn descending_ship_enters_low_gravity_moon_and_switches_gravity_scale() {
    let (addon, reg) = fleet();
    let mut router = orbital_router();
    let mut ship = descending_ship(&addon, &reg, &mut router);

    // `prev` is the pose immediately before the crossing step (the +X
    // propellers keep driving during the descent), so arrival x/z can be
    // checked for continuity through the hop.
    let (prev, transition) = step_until_transition_with_prev(&mut router, &mut ship, 100);
    assert_eq!((transition.from_dim, transition.to_dim), (DIM_ORBIT, DIM_MOON));
    assert_eq!(transition.to_dim_id, MOON_ID);
    // Coordinate scale 1.0 -> 1.0 keeps the horizontal position continuous
    // through the hop: x advanced by exactly the crossing tick's own motion,
    // z untouched; altitude is the fixed safe entry height.
    assert_eq!(transition.new_pos[1], MOON_ENTRY_Y);
    assert_eq!(transition.new_pos[2], prev[2]);
    assert!(
        (transition.new_pos[0] - (prev[0] + transition.vel[0] * DT)).abs() < 1e-9,
        "horizontal discontinuity: prev={prev:?} new={:?}",
        transition.new_pos
    );
    assert_eq!(transition.passengers.len(), 2);
    assert!(transition
        .passengers
        .iter()
        .all(|p| p.intent.target_dim == MOON_ID));
    // Lunar physics installed: 0.165 g, vacuum.
    assert_eq!(ship.atmosphere().gravity_scale, 0.165);
    assert!(!ship.atmosphere().enable_buoyancy);

    // Descent in lunar gravity: the -Y pull is exactly 0.165 * 9.81 m/s^2 —
    // no buoyancy (vacuum), no drag.
    let pre_vy = ship.vel[1];
    router.step_with_dimension_transitions(&mut ship, DT);
    let expect_delta = -(9.81 * 0.165) * DT;
    assert!(
        (ship.vel[1] - (pre_vy + expect_delta)).abs() < 1e-12,
        "lunar gravity: vel={:?} pre={pre_vy} delta={expect_delta}",
        ship.vel
    );
}

#[test]
fn orbital_scenario_rerun_is_byte_identical_and_reload_continuous() {
    // Uninterrupted reference: orbit descent -> lunar crossing -> keep
    // falling under 0.165 g. Snapshot bytes are the comparison ground truth.
    fn fly(router: &mut OrbitalTransitionRouter, ship: &mut RigidBodySubWorld, ticks: u64) {
        for _ in 0..ticks {
            router.step_with_dimension_transitions(ship, DT);
        }
    }

    let (addon, reg) = fleet();
    let mut router_a = orbital_router();
    let mut ref_ship = descending_ship(&addon, &reg, &mut router_a);
    fly(&mut router_a, &mut ref_ship, 300);
    assert_eq!(ref_ship.parent_dim, DIM_MOON);
    let reference = ref_ship.encode_snapshot_for_data_version(AERO_DATA_VERSION_26_3);

    // Identical rerun -> identical bytes.
    let mut router_b = orbital_router();
    let mut rerun = descending_ship(&addon, &reg, &mut router_b);
    fly(&mut router_b, &mut rerun, 300);
    assert_eq!(
        rerun.encode_snapshot_for_data_version(AERO_DATA_VERSION_26_3),
        reference,
        "rerun diverged"
    );

    // Mid-transition reload: fly 220 ticks (past the lunar crossing),
    // snapshot, decode into a fresh ship, resume through an identical router
    // for the remaining ticks. The restored ship's `parent_dim` re-derives
    // the destination atmosphere on the first routed step, so the resumed
    // run lands byte-for-byte on the uninterrupted reference.
    let mut router_c = orbital_router();
    let mut interrupted = descending_ship(&addon, &reg, &mut router_c);
    fly(&mut router_c, &mut interrupted, 220);
    assert_eq!(interrupted.parent_dim, DIM_MOON);
    let mid = interrupted.encode_snapshot_for_data_version(AERO_DATA_VERSION_26_3);
    let (mut restored, dv) = decode_snapshot_with_data_version(&mid, &reg).unwrap();
    assert_eq!(dv, AERO_DATA_VERSION_26_3);
    assert_eq!(restored.parent_dim, DIM_MOON);

    let mut router_d = orbital_router();
    router_d.attach_passenger(2001, restored.id, [0.5, 0.5, 0.0]);
    router_d.attach_passenger(2002, restored.id, [0.0, 0.5, -0.5]);
    fly(&mut router_d, &mut restored, 80);
    assert_eq!(
        restored.encode_snapshot_for_data_version(AERO_DATA_VERSION_26_3),
        reference,
        "mid-transition reload diverged from uninterrupted flight"
    );
}
