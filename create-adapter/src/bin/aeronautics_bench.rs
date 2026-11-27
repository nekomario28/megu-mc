// SPDX-License-Identifier: LGPL-3.0-or-later

//! Microbenchmark for `create-adapter`'s 6-DOF aeronautics engine:
//! 100 simultaneous flying rigid-body sub-worlds, each carrying an internal
//! live Create kinetic network (propeller engine -> shaft -> propeller
//! bearing) producing real thrust every 20 TPS step.

use std::hint::black_box;
use std::time::Instant;

use create_adapter::addon_profile::{create_registry_with_addons, createaeronautics_profile};
use create_adapter::aeronautics::{RigidBodySubWorld, SEA_LEVEL_AIR_DENSITY};
use create_adapter::blockstates::{CreateBlockRegistry, DEFAULT_BASE_STATE_ID};

fn canonical_state(
    reg: &CreateBlockRegistry,
    id: &str,
    prop: Option<(&str, &str)>,
) -> Result<String, String> {
    let base = reg
        .default_state(id)
        .ok_or_else(|| format!("missing block {id}"))?;
    let state = match prop {
        Some((key, value)) => reg
            .with_property(base, key, value)
            .ok_or_else(|| format!("block {id} has no state with {key}={value}"))?,
        None => base,
    };
    reg.encode_canonical(state)
        .map(str::to_string)
        .ok_or_else(|| format!("block {id} state {state} has no canonical encoding"))
}

fn main() -> Result<(), String> {
    let addon = createaeronautics_profile();
    let reg = create_registry_with_addons(DEFAULT_BASE_STATE_ID, std::slice::from_ref(&addon))
        .map_err(|e| format!("failed to build createaeronautics registry: {e:?}"))?;

    const SHIPS: usize = 100;
    let mut ships: Vec<RigidBodySubWorld> = (0..SHIPS)
        .map(|i| -> Result<RigidBodySubWorld, String> {
            let mut cells = std::collections::BTreeMap::new();
            cells.insert(
                [0, 0, 0],
                canonical_state(
                    &reg,
                    "createaeronautics:propeller_engine",
                    Some(("axis", "x")),
                )?,
            );
            cells.insert(
                [1, 0, 0],
                canonical_state(&reg, "create:shaft", Some(("axis", "x")))?,
            );
            cells.insert(
                [2, 0, 0],
                canonical_state(
                    &reg,
                    "createaeronautics:propeller_bearing",
                    Some(("facing", "east")),
                )?,
            );
            cells.insert(
                [0, 1, 0],
                canonical_state(&reg, "createaeronautics:balloon_envelope", None)?,
            );
            cells.insert(
                [0, 0, 1],
                canonical_state(&reg, "createaeronautics:rudder", None)?,
            );
            let mut sub = RigidBodySubWorld::assemble(
                (i + 1) as u64,
                0,
                cells,
                &reg,
                Some(&addon),
                100.0,
            )
            .map_err(|e| format!("failed to assemble ship {}: {e:?}", i + 1))?;
            sub.pos = [(i * 8) as f64, 96.0, (i * 3) as f64];
            Ok(sub)
        })
        .collect::<Result<Vec<_>, _>>()?;

    const TICKS: u64 = 20_000;
    let t0 = Instant::now();
    for _ in 0..TICKS {
        for ship in ships.iter_mut() {
            ship.step(0.05, 9.81, SEA_LEVEL_AIR_DENSITY);
        }
    }
    let elapsed = t0.elapsed().as_nanos() as f64;
    let ship_ticks = TICKS as f64 * SHIPS as f64;
    let mut drift = 0.0f64;
    for ship in &ships {
        assert!(ship.pos.iter().all(|v| v.is_finite()));
        assert!(ship.vel.iter().all(|v| v.is_finite()));
        drift += ship.pos[0];
    }
    black_box(drift);

    println!(
        "aeronautics_bench ships={} ticks={} ns_per_ship_tick={:.1} us_per_world_tick={:.2}",
        SHIPS,
        TICKS,
        elapsed / ship_ticks,
        elapsed / TICKS as f64 / 1_000.0
    );
    Ok(())
}
