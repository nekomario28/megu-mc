// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Bedrock-quality hardening suite for the Create contraption adapter:
//! Super Glue connectivity isolation (unglued floors/walls are never ripped),
//! fail-closed immovable guards, exact quarter-turn bearing rotation, bounded
//! linear travel stops and bounce, minecart heading turns, onboard storage
//! conservation, deterministic drill/harvester/deployer actors, checked
//! disassembly onto occupied cells, and Save v2 snapshot round-trips.

use std::collections::{BTreeMap, BTreeSet};

use create_adapter::blockstates::{
    core_create_registry, CreateBlockRegistry, DEFAULT_BASE_STATE_ID,
};
use create_adapter::contraption_adapter::{
    is_immovable_block_id, ActorCell, ActorWorldEdit, ContraptionActor, ContraptionActorKind,
    ContraptionError, ContraptionKind, ContraptionStorage, ContraptionWorldQuery, CropCell,
    LiveContraption, SuperGlueBox, SuperGlueVolume, WorldBlockEdit, WorldItemDrop,
    DEFAULT_MAX_CONTRAPTION_BLOCKS, IMMOVABLE_BLOCK_IDS,
};
use create_adapter::factory::DimBlockPos;
use mod_offload::contraption::Aabb;

const DIM: i32 = 0;
const ANCHOR: [i32; 3] = [10, 64, 10];

/// Synthetic runtime state ids standing in for vanilla blocks; names resolve
/// through `name_map()` exactly like the server's palette lookup would.
const STONE: u32 = 90_001;
const DEEPSLATE: u32 = 90_002;
const CHASSIS: u32 = 90_003;
const PLANKS: u32 = 90_004;

fn reg() -> CreateBlockRegistry {
    core_create_registry(DEFAULT_BASE_STATE_ID)
}

fn shaft() -> u32 {
    reg().default_state("create:shaft").unwrap()
}

fn never_sticky(_: u32) -> bool {
    false
}

fn never_immovable(_: u32) -> bool {
    false
}

/// Stub world surface for actor ticks: `cells` answers `cell_at`,
/// `place_states` resolves deployer item -> placed block state.
#[derive(Default)]
struct StubWorld {
    cells: BTreeMap<[i32; 3], ActorCell>,
    place_states: BTreeMap<String, u32>,
}

impl StubWorld {
    fn cell(mut self, pos: [i32; 3], cell: ActorCell) -> Self {
        self.cells.insert(pos, cell);
        self
    }

    fn place(mut self, item: &str, state: u32) -> Self {
        self.place_states.insert(item.to_string(), state);
        self
    }
}

impl ContraptionWorldQuery for StubWorld {
    fn cell_at(&self, pos: [i32; 3]) -> ActorCell {
        self.cells.get(&pos).cloned().unwrap_or_else(ActorCell::air)
    }

    fn place_state_for(&self, item_id: &str) -> Option<u32> {
        self.place_states.get(item_id).copied()
    }
}

fn assemble_one(kind: ContraptionKind) -> LiveContraption {
    let mut world = BTreeMap::new();
    world.insert(ANCHOR, shaft());
    LiveContraption::assemble_from_world(
        1,
        DIM,
        ANCHOR,
        ANCHOR,
        &world,
        kind,
        DEFAULT_MAX_CONTRAPTION_BLOCKS,
    )
    .unwrap()
    .0
}

fn coords(edits: &[WorldBlockEdit]) -> BTreeSet<(i32, i32, i32)> {
    edits.iter().map(|e| (e.pos.x, e.pos.y, e.pos.z)).collect()
}

// ---------- Super Glue connectivity & immovable guard ----------

#[test]
fn glued_platform_lifts_cleanly_unglued_floor_and_wall_stay() {
    let mut world = BTreeMap::new();
    // 2x2x2 glued platform at x 0..=1, y 64..=65, z 0..=1.
    for x in 0..=1 {
        for y in 64..=65 {
            for z in 0..=1 {
                world.insert([x, y, z], STONE);
            }
        }
    }
    // Unglued stone floor directly beneath plus a touching side wall — all
    // plain adjacency, so none of it may be ripped into the contraption.
    for x in 0..=1 {
        for z in 0..=1 {
            world.insert([x, 63, z], DEEPSLATE);
        }
    }
    world.insert([2, 64, 0], DEEPSLATE);
    world.insert([2, 65, 0], DEEPSLATE);

    let glue = SuperGlueVolume::from_boxes(vec![SuperGlueBox::new([0, 64, 0], [1, 65, 1])]);
    let (contraption, removals) = LiveContraption::assemble_with_glue(
        1,
        DIM,
        [0, 64, 0],
        [0, 64, 0],
        &world,
        &glue,
        never_sticky,
        never_immovable,
        ContraptionKind::Bearing {
            angular_deg_per_tick: 15.0,
        },
        DEFAULT_MAX_CONTRAPTION_BLOCKS,
    )
    .unwrap();

    assert_eq!(contraption.block_count(), 8);
    let removed = coords(&removals);
    assert_eq!(removed.len(), 8);
    // Every removal is a platform cell; every floor/wall cell is untouched.
    for pos in &removed {
        assert!(glue.contains([pos.0, pos.1, pos.2]));
    }
    for floor in [[0, 63, 0], [1, 63, 1], [2, 64, 0], [2, 65, 0]] {
        assert!(!removed.contains(&(floor[0], floor[1], floor[2])));
    }
}

#[test]
fn adjacent_cells_join_only_when_one_glue_box_covers_both() {
    let mut world = BTreeMap::new();
    world.insert([0, 64, 0], STONE);
    world.insert([1, 64, 0], STONE);
    world.insert([2, 64, 0], STONE);

    // Glue welded only the (0)-(1) seam: cell 2 stays in the world.
    let glue = SuperGlueVolume::from_boxes(vec![SuperGlueBox::new([0, 64, 0], [1, 64, 0])]);
    let (contraption, removals) = LiveContraption::assemble_with_glue(
        1,
        DIM,
        [0, 64, 0],
        [0, 64, 0],
        &world,
        &glue,
        never_sticky,
        never_immovable,
        ContraptionKind::Linear {
            velocity_per_tick: [0.0; 3],
        },
        DEFAULT_MAX_CONTRAPTION_BLOCKS,
    )
    .unwrap();
    assert_eq!(contraption.block_count(), 2);
    assert_eq!(removals.len(), 2);
    assert!(!coords(&removals).contains(&(2, 64, 0)));

    // A box covering only the seed cell welds nothing — the seed rides alone.
    let glue = SuperGlueVolume::from_boxes(vec![SuperGlueBox::new([0, 64, 0], [0, 64, 0])]);
    let (contraption, removals) = LiveContraption::assemble_with_glue(
        2,
        DIM,
        [0, 64, 0],
        [0, 64, 0],
        &world,
        &glue,
        never_sticky,
        never_immovable,
        ContraptionKind::Linear {
            velocity_per_tick: [0.0; 3],
        },
        DEFAULT_MAX_CONTRAPTION_BLOCKS,
    )
    .unwrap();
    assert_eq!(contraption.block_count(), 1);
    assert_eq!(removals.len(), 1);
}

#[test]
fn radial_sticky_rule_pulls_neighbors_without_glue() {
    let mut world = BTreeMap::new();
    // Chassis chain: a chassis grabs every adjacent block; grabbed blocks that
    // are themselves chassis keep the chain going. Plain stone beyond the
    // last chassis is never pulled.
    world.insert([0, 64, 0], CHASSIS);
    world.insert([1, 64, 0], CHASSIS);
    world.insert([2, 64, 0], STONE);
    world.insert([0, 65, 0], PLANKS);
    world.insert([3, 64, 0], STONE); // adjacent only to plain stone — stays
    world.insert([2, 65, 0], PLANKS); // adjacent to grabbed stone (not sticky) — stays

    let is_sticky = |id: u32| id == CHASSIS;
    let (contraption, removals) = LiveContraption::assemble_with_glue(
        1,
        DIM,
        [0, 64, 0],
        [0, 64, 0],
        &world,
        &SuperGlueVolume::new(),
        is_sticky,
        never_immovable,
        ContraptionKind::Linear {
            velocity_per_tick: [0.0; 3],
        },
        DEFAULT_MAX_CONTRAPTION_BLOCKS,
    )
    .unwrap();

    // Seed chassis + chained chassis + both of its grabs: 4 cells.
    assert_eq!(contraption.block_count(), 4);
    let removed = coords(&removals);
    assert!(removed.contains(&(2, 64, 0)));
    assert!(removed.contains(&(0, 65, 0)));
    assert!(!removed.contains(&(3, 64, 0)));
    assert!(!removed.contains(&(2, 65, 0)));
}

#[test]
fn every_immovable_block_fails_glued_assembly_closed() {
    for (index, name) in IMMOVABLE_BLOCK_IDS.iter().enumerate() {
        let bad_state = 91_000 + index as u32;
        let names = BTreeMap::from([(bad_state, *name)]);
        let is_immovable = |id: u32| names.get(&id).copied().is_some_and(is_immovable_block_id);

        let mut world = BTreeMap::new();
        world.insert([0, 64, 0], STONE);
        world.insert([1, 64, 0], bad_state);
        let glue = SuperGlueVolume::from_boxes(vec![SuperGlueBox::new([0, 64, 0], [1, 64, 0])]);

        let err = match LiveContraption::assemble_with_glue(
            1,
            DIM,
            [0, 64, 0],
            [0, 64, 0],
            &world,
            &glue,
            never_sticky,
            is_immovable,
            ContraptionKind::Linear {
                velocity_per_tick: [0.0; 3],
            },
            DEFAULT_MAX_CONTRAPTION_BLOCKS,
        ) {
            Err(e) => e,
            Ok(_) => panic!("{name}: assembly must fail closed"),
        };
        assert_eq!(
            err,
            ContraptionError::ImmovableBlock { pos: [1, 64, 0] },
            "{name} must stop assembly at its own cell"
        );
    }
}

#[test]
fn unglued_immovable_neighbor_is_never_ripped() {
    let bedrock = 91_000;
    let names = BTreeMap::from([(bedrock, "minecraft:bedrock")]);
    let is_immovable = |id: u32| names.get(&id).copied().is_some_and(is_immovable_block_id);

    let mut world = BTreeMap::new();
    world.insert([0, 64, 0], STONE);
    world.insert([1, 64, 0], bedrock);
    // Glue covers ONLY the stone cell: bedrock stays in the world and is
    // never even visited — assembly succeeds with just the stone.
    let glue = SuperGlueVolume::from_boxes(vec![SuperGlueBox::new([0, 64, 0], [0, 64, 0])]);
    let (contraption, removals) = LiveContraption::assemble_with_glue(
        1,
        DIM,
        [0, 64, 0],
        [0, 64, 0],
        &world,
        &glue,
        never_sticky,
        is_immovable,
        ContraptionKind::Linear {
            velocity_per_tick: [0.0; 3],
        },
        DEFAULT_MAX_CONTRAPTION_BLOCKS,
    )
    .unwrap();
    assert_eq!(contraption.block_count(), 1);
    assert_eq!(removals.len(), 1);
    assert!(!coords(&removals).contains(&(1, 64, 0)));
}

#[test]
fn immovable_block_id_list_is_exact() {
    assert_eq!(IMMOVABLE_BLOCK_IDS.len(), 7);
    for name in IMMOVABLE_BLOCK_IDS {
        assert!(is_immovable_block_id(name), "{name} must be immovable");
    }
    for name in [
        "minecraft:stone",
        "minecraft:deepslate",
        "create:shaft",
        "minecraft:barrier", // handled by the world's breakable flag instead
        "",
    ] {
        assert!(!is_immovable_block_id(name), "{name} must be movable");
    }
    // SuperGlueBox normalizes inverted corners.
    let b = SuperGlueBox::new([5, 5, 5], [0, 0, 0]);
    assert_eq!(b.min, [0, 0, 0]);
    assert_eq!(b.max, [5, 5, 5]);
    assert!(b.contains([3, 3, 3]));
    assert!(!b.contains([6, 0, 0]));
}

// ---------- Bearing rotation, carry precision, conservation ----------

#[test]
fn bearing_rotates_through_every_exact_quarter_turn() {
    let mut world = BTreeMap::new();
    world.insert(ANCHOR, shaft());
    world.insert([11, 64, 10], shaft());
    world.insert([12, 64, 10], shaft());
    let (mut c, _) = LiveContraption::assemble_from_world(
        1,
        DIM,
        ANCHOR,
        ANCHOR,
        &world,
        ContraptionKind::Bearing {
            angular_deg_per_tick: 15.0,
        },
        DEFAULT_MAX_CONTRAPTION_BLOCKS,
    )
    .unwrap();

    // Grid disassembly follows Create's convention (rotation about the
    // bearing center); the rigid pose itself pivots at the anchor cell
    // corner, so the physically swept cells sit one cell off on flipped axes.
    let expected = [
        // After 90 deg: arm swings to -Z.
        (
            BTreeSet::from([(10, 64, 10), (10, 64, 9), (10, 64, 8)]),
            BTreeSet::from([(10, 64, 9), (10, 64, 8), (10, 64, 7)]),
        ),
        // After 180 deg: arm lies along -X.
        (
            BTreeSet::from([(10, 64, 10), (9, 64, 10), (8, 64, 10)]),
            BTreeSet::from([(9, 64, 9), (8, 64, 9), (7, 64, 9)]),
        ),
        // After 270 deg: arm points +Z.
        (
            BTreeSet::from([(10, 64, 10), (10, 64, 11), (10, 64, 12)]),
            BTreeSet::from([(9, 64, 10), (9, 64, 11), (9, 64, 12)]),
        ),
        // After 360 deg: back to +X exactly.
        (
            BTreeSet::from([(10, 64, 10), (11, 64, 10), (12, 64, 10)]),
            BTreeSet::from([(10, 64, 10), (11, 64, 10), (12, 64, 10)]),
        ),
    ];
    for (quarter, (want_grid, want_phys)) in expected.iter().enumerate() {
        for _ in 0..6 {
            c.step_tick().unwrap();
        }
        let yaw = ((quarter + 1) * 90) % 360;
        assert!(
            (c.yaw_deg - yaw as f32).abs() < 1e-6,
            "quarter {}",
            quarter + 1
        );
        assert_eq!(
            &coords(&c.disassemble_to_world()),
            want_grid,
            "quarter {}",
            quarter + 1
        );
        // The rotated arm really occupies its physical cells for collision.
        for cell in want_phys {
            let probe = Aabb::new(
                [
                    cell.0 as f64 + 0.2,
                    cell.1 as f64 + 0.2,
                    cell.2 as f64 + 0.2,
                ],
                [
                    cell.0 as f64 + 0.8,
                    cell.1 as f64 + 0.8,
                    cell.2 as f64 + 0.8,
                ],
            )
            .unwrap();
            assert!(c.collides(probe), "quarter {} cell {cell:?}", quarter + 1);
        }
    }
    assert_eq!(c.yaw_deg, 0.0);
}

#[test]
fn passenger_carry_full_revolution_has_zero_nan_and_zero_drift() {
    let mut world = BTreeMap::new();
    world.insert(ANCHOR, shaft());
    world.insert([11, 64, 10], shaft());
    world.insert([12, 64, 10], shaft());
    let (mut c, _) = LiveContraption::assemble_from_world(
        1,
        DIM,
        ANCHOR,
        ANCHOR,
        &world,
        ContraptionKind::Bearing {
            angular_deg_per_tick: 15.0,
        },
        DEFAULT_MAX_CONTRAPTION_BLOCKS,
    )
    .unwrap();

    let start = [12.5, 65.0, 10.5];
    let mut passenger = start;
    for _ in 0..24 {
        c.step_tick().unwrap();
        let delta = c.passenger_delta(passenger).unwrap();
        assert!(delta.iter().all(|v| v.is_finite()), "NaN carry delta");
        for (axis, d) in delta.iter().enumerate() {
            passenger[axis] += d;
        }
        assert!(passenger.iter().all(|v| v.is_finite()));
    }
    // A complete 360-degree loop lands the rider back on the start point.
    assert!((passenger[0] - start[0]).abs() < 1e-6, "{passenger:?}");
    assert!((passenger[1] - start[1]).abs() < 1e-6, "{passenger:?}");
    assert!((passenger[2] - start[2]).abs() < 1e-6, "{passenger:?}");
    assert_eq!(c.yaw_deg, 0.0);
}

#[test]
fn disassembly_conserves_every_block_and_state_exactly() {
    let mut world = BTreeMap::new();
    world.insert([0, 64, 0], STONE);
    world.insert([1, 64, 0], PLANKS);
    world.insert([0, 64, 1], shaft());
    world.insert([1, 64, 1], DEEPSLATE);
    let glue = SuperGlueVolume::from_boxes(vec![SuperGlueBox::new([0, 64, 0], [1, 64, 1])]);
    let (mut c, _) = LiveContraption::assemble_with_glue(
        1,
        DIM,
        [0, 64, 0],
        [0, 64, 0],
        &world,
        &glue,
        never_sticky,
        never_immovable,
        ContraptionKind::Bearing {
            angular_deg_per_tick: 15.0,
        },
        DEFAULT_MAX_CONTRAPTION_BLOCKS,
    )
    .unwrap();
    for _ in 0..6 {
        c.step_tick().unwrap();
    }

    let mut want: Vec<u32> = world.values().copied().collect();
    want.sort_unstable();
    let edits = c.disassemble_to_world();
    assert_eq!(edits.len(), world.len());
    assert_eq!(
        coords(&edits).len(),
        world.len(),
        "no overlapping placements"
    );
    let mut got: Vec<u32> = edits.iter().filter_map(|e| e.state_id).collect();
    got.sort_unstable();
    assert_eq!(got, want, "every block state comes back exactly once");
}

// ---------- Bounded linear drivers (piston / gantry / pulley) ----------

#[test]
fn bounded_linear_stops_at_max_offset_when_reverse_disabled() {
    let mut c = assemble_one(ContraptionKind::BoundedLinear {
        velocity_per_tick: [0.6, 0.0, 0.0],
        min_offset: [0.0; 3],
        max_offset: [1.0, 0.0, 0.0],
        reverse_at_bounds: false,
    });

    assert!(c.step_tick().unwrap());
    assert!((c.translation[0] - 10.6).abs() < 1e-12);
    // Second tick clamps exactly at max_offset; velocity is stopped.
    assert!(c.step_tick().unwrap());
    assert_eq!(c.translation[0], 11.0);
    // Held at the stop: no more motion, pose reports unchanged.
    assert!(!c.step_tick().unwrap());
    assert_eq!(c.translation[0], 11.0);
    assert_eq!(c.block_count(), 1);
}

#[test]
fn bounded_linear_reverses_cleanly_at_both_travel_ends() {
    let mut c = assemble_one(ContraptionKind::BoundedLinear {
        velocity_per_tick: [0.6, 0.0, 0.0],
        min_offset: [0.0; 3],
        max_offset: [1.0, 0.0, 0.0],
        reverse_at_bounds: true,
    });

    // Out: 10 -> 10.6 -> clamp 11.0 (bounce).
    c.step_tick().unwrap();
    c.step_tick().unwrap();
    assert_eq!(c.translation[0], 11.0);
    // Back: 11.0 -> 10.4 -> clamp 10.0 (bounce).
    c.step_tick().unwrap();
    assert!((c.translation[0] - 10.4).abs() < 1e-12);
    c.step_tick().unwrap();
    assert_eq!(c.translation[0], 10.0);
    // Bounce carries it forward again — it never sticks or drifts.
    c.step_tick().unwrap();
    assert!((c.translation[0] - 10.6).abs() < 1e-12);
    c.step_tick().unwrap();
    assert_eq!(c.translation[0], 11.0);
}

#[test]
fn bounded_linear_rejects_inverted_bounds() {
    let mut world = BTreeMap::new();
    world.insert(ANCHOR, shaft());
    let err = match LiveContraption::assemble_from_world(
        1,
        DIM,
        ANCHOR,
        ANCHOR,
        &world,
        ContraptionKind::BoundedLinear {
            velocity_per_tick: [0.1; 3],
            min_offset: [2.0, 0.0, 0.0],
            max_offset: [1.0, 5.0, 5.0],
            reverse_at_bounds: true,
        },
        DEFAULT_MAX_CONTRAPTION_BLOCKS,
    ) {
        Err(e) => e,
        Ok(_) => panic!("inverted bounds must fail closed"),
    };
    assert_eq!(err, ContraptionError::InvalidDriverBounds);
}

// ---------- Minecart contraption ----------

#[test]
fn minecart_heading_turn_preserves_cells_and_passenger() {
    let mut world = BTreeMap::new();
    world.insert(ANCHOR, shaft());
    world.insert([11, 64, 10], shaft());
    let (mut c, _) = LiveContraption::assemble_from_world(
        1,
        DIM,
        ANCHOR,
        ANCHOR,
        &world,
        ContraptionKind::Minecart {
            speed_per_tick: 1.0,
            heading_yaw: 0.0,
        },
        DEFAULT_MAX_CONTRAPTION_BLOCKS,
    )
    .unwrap();

    let mut passenger = [11.5, 65.0, 10.5];
    c.step_tick().unwrap();
    assert_eq!(c.translation, [11.0, 64.0, 10.0]);
    let delta = c.passenger_delta(passenger).unwrap();
    for (axis, d) in delta.iter().enumerate() {
        passenger[axis] += d;
    }

    // Curved rail: heading swings 90 degrees; the whole body yaws with it.
    c.kind = ContraptionKind::Minecart {
        speed_per_tick: 1.0,
        heading_yaw: 90.0,
    };
    c.step_tick().unwrap();
    assert_eq!(c.yaw_deg, 90.0);
    // Heading 90 -> -Z with exactly zero +X residue (trig snap).
    assert_eq!(c.translation, [11.0, 64.0, 9.0]);
    let delta = c.passenger_delta(passenger).unwrap();
    assert!(delta.iter().all(|v| v.is_finite()));
    for (axis, d) in delta.iter().enumerate() {
        passenger[axis] += d;
    }
    assert!(passenger.iter().all(|v| v.is_finite()));

    // Every local cell survives the turn: disassembling at yaw 90 places the
    // two-block arm rotated about the anchor point, nothing lost or doubled.
    let edits = c.disassemble_to_world();
    assert_eq!(edits.len(), 2);
    let placed = coords(&edits);
    assert_eq!(placed, BTreeSet::from([(11, 64, 9), (11, 64, 8)]));
    assert!(edits.iter().all(|e| e.state_id == Some(shaft())));
    assert_eq!(c.block_count(), 2);
}

#[test]
fn minecart_opposing_headings_travel_exactly() {
    let mut c = assemble_one(ContraptionKind::Minecart {
        speed_per_tick: 0.5,
        heading_yaw: 270.0,
    });
    c.step_tick().unwrap();
    // Heading 270 -> +Z: cos 270 snaps to 0, -sin 270 = +1.
    assert_eq!(c.translation, [10.0, 64.0, 10.5]);
    assert_eq!(c.yaw_deg, 270.0);
    c.kind = ContraptionKind::Minecart {
        speed_per_tick: 0.5,
        heading_yaw: 180.0,
    };
    c.step_tick().unwrap();
    // Heading 180 -> -X exactly.
    assert_eq!(c.translation, [9.5, 64.0, 10.5]);
}

// ---------- Onboard storage ----------

#[test]
fn contraption_storage_enforces_slot_bound_and_conserves_items() {
    let mut storage = ContraptionStorage::new(1);
    assert_eq!(storage.insert_item("minecraft:stone", 3), 0);
    assert_eq!(storage.total_count_of("minecraft:stone"), 3);
    // Second distinct item needs a free slot -> full remainder back.
    assert_eq!(storage.insert_item("minecraft:dirt", 4), 4);
    // Stacking a held item always fits.
    assert_eq!(storage.insert_item("minecraft:stone", 2), 0);
    assert_eq!(storage.total_items(), 5);

    // Extraction never exceeds what is stored; empty slots are freed.
    assert_eq!(storage.extract_item("minecraft:stone", 10), 5);
    assert_eq!(storage.used_slots(), 0);
    assert_eq!(storage.insert_item("minecraft:dirt", 4), 0);
    assert_eq!(storage.extract_item("minecraft:dirt", 2), 2);
    assert_eq!(storage.extract_item("minecraft:dirt", 2), 2);
    assert!(storage.is_empty());

    // Unfiltered extraction is deterministic (smallest id first).
    let mut storage = ContraptionStorage::new(4);
    storage.insert_item("mod:bbb", 1);
    storage.insert_item("mod:aaa", 1);
    assert_eq!(storage.extract_any(1), Some(("mod:aaa".to_string(), 1)));
}

// ---------- Actors ----------

#[test]
fn mechanical_drill_breaks_soft_block_into_storage() {
    let mut c = assemble_one(ContraptionKind::Bearing {
        angular_deg_per_tick: 0.0,
    });
    c.storage = ContraptionStorage::new(4);
    c.actors.push(ContraptionActor {
        local: [0, 0, 0],
        facing: [1, 0, 0],
        kind: ContraptionActorKind::MechanicalDrill,
    });
    // Drill at ANCHOR faces +X -> sweeps (11, 64, 10).
    let world = StubWorld::default().cell([11, 64, 10], ActorCell::block("minecraft:stone", true));
    let report = c.step_actors(&world);
    assert_eq!(
        report.edits,
        [ActorWorldEdit::BreakBlock {
            pos: DimBlockPos::new(DIM, 11, 64, 10)
        }]
    );
    assert!(report.drops.is_empty());
    assert_eq!(c.storage.total_count_of("minecraft:stone"), 1);
}

#[test]
fn mechanical_drill_refuses_immovable_and_unbreakable_cells() {
    let mut c = assemble_one(ContraptionKind::Bearing {
        angular_deg_per_tick: 0.0,
    });
    c.storage = ContraptionStorage::new(4);
    c.actors.push(ContraptionActor {
        local: [0, 0, 0],
        facing: [1, 0, 0],
        kind: ContraptionActorKind::MechanicalDrill,
    });

    // The immovable list is enforced by the adapter even when the world marks
    // the cell breakable — bedrock is never drilled and nothing is stored.
    for name in ["minecraft:bedrock", "minecraft:obsidian"] {
        let world = StubWorld::default().cell([11, 64, 10], ActorCell::block(name, true));
        let report = c.step_actors(&world);
        assert!(report.edits.is_empty(), "{name}");
        assert!(report.drops.is_empty(), "{name}");
    }
    // World-flagged unbreakable (e.g. a protected claim block) also holds.
    let world =
        StubWorld::default().cell([11, 64, 10], ActorCell::block("minecraft:barrier", false));
    let report = c.step_actors(&world);
    assert!(report.edits.is_empty());
    // Air is a no-op.
    let report = c.step_actors(&StubWorld::default());
    assert!(report.edits.is_empty() && report.drops.is_empty());
    assert!(c.storage.is_empty());
}

#[test]
fn full_storage_overflow_emits_world_drop_nothing_is_lost() {
    let mut c = assemble_one(ContraptionKind::Bearing {
        angular_deg_per_tick: 0.0,
    });
    // Capacity zero = no onboard chest at all: every drop must go to the world.
    c.actors.push(ContraptionActor {
        local: [0, 0, 0],
        facing: [1, 0, 0],
        kind: ContraptionActorKind::MechanicalDrill,
    });
    let world = StubWorld::default().cell([11, 64, 10], ActorCell::block("minecraft:stone", true));
    let report = c.step_actors(&world);
    assert_eq!(report.edits.len(), 1);
    assert_eq!(
        report.drops,
        [WorldItemDrop {
            pos: DimBlockPos::new(DIM, 11, 64, 10),
            item_id: "minecraft:stone".to_string(),
            count: 1,
        }]
    );
    assert!(c.storage.is_empty());

    // A full single-slot chest holding a DIFFERENT item overflows the same way.
    c.storage = ContraptionStorage::new(1);
    assert_eq!(c.storage.insert_item("minecraft:dirt", 5), 0);
    let report = c.step_actors(&world);
    assert_eq!(report.drops.len(), 1);
    assert_eq!(report.drops[0].item_id, "minecraft:stone");
    // Conservation: stored dirt untouched, exactly one stone item exists.
    assert_eq!(c.storage.total_count_of("minecraft:dirt"), 5);
    assert_eq!(c.storage.total_count_of("minecraft:stone"), 0);
}

#[test]
fn mechanical_harvester_resets_mature_crop_and_skips_immature() {
    let mut c = assemble_one(ContraptionKind::Bearing {
        angular_deg_per_tick: 0.0,
    });
    c.storage = ContraptionStorage::new(4);
    c.actors.push(ContraptionActor {
        local: [0, 0, 0],
        facing: [1, 0, 0],
        kind: ContraptionActorKind::MechanicalHarvester,
    });

    let mature = ActorCell {
        name: "minecraft:wheat".to_string(),
        breakable: true,
        crop: Some(CropCell {
            age: 7,
            max_age: 7,
            drop_item: "minecraft:wheat".to_string(),
            drop_count: 2,
        }),
    };
    let world = StubWorld::default().cell([11, 64, 10], mature);
    let report = c.step_actors(&world);
    // Crop resets to age 0 — the block is NEVER broken (soil stays farmed).
    assert_eq!(
        report.edits,
        [ActorWorldEdit::ResetCrop {
            pos: DimBlockPos::new(DIM, 11, 64, 10)
        }]
    );
    assert!(report.drops.is_empty());
    assert_eq!(c.storage.total_count_of("minecraft:wheat"), 2);

    let immature = ActorCell {
        name: "minecraft:wheat".to_string(),
        breakable: true,
        crop: Some(CropCell {
            age: 3,
            max_age: 7,
            drop_item: "minecraft:wheat".to_string(),
            drop_count: 2,
        }),
    };
    let world = StubWorld::default().cell([11, 64, 10], immature);
    let report = c.step_actors(&world);
    assert!(report.edits.is_empty() && report.drops.is_empty());
    assert_eq!(c.storage.total_count_of("minecraft:wheat"), 2);
}

#[test]
fn mechanical_deployer_consumes_item_only_on_real_placement() {
    let mut c = assemble_one(ContraptionKind::Bearing {
        angular_deg_per_tick: 0.0,
    });
    c.storage = ContraptionStorage::new(4);
    assert_eq!(c.storage.insert_item("create:shaft", 3), 0);
    c.actors.push(ContraptionActor {
        local: [0, 0, 0],
        facing: [1, 0, 0],
        kind: ContraptionActorKind::MechanicalDeployer {
            filter_item: Some("create:shaft".to_string()),
        },
    });
    let state = reg().default_state("create:shaft").unwrap();
    let air_world = StubWorld::default().place("create:shaft", state);

    // Empty cell + resolvable state + held item -> exactly one consumed.
    let report = c.step_actors(&air_world);
    assert_eq!(
        report.edits,
        [ActorWorldEdit::PlaceBlock {
            pos: DimBlockPos::new(DIM, 11, 64, 10),
            state_id: state,
            item_id: "create:shaft".to_string(),
        }]
    );
    assert_eq!(c.storage.total_count_of("create:shaft"), 2);

    // Occupied cell -> no placement, no consumption.
    let blocked = StubWorld::default()
        .cell([11, 64, 10], ActorCell::block("minecraft:stone", false))
        .place("create:shaft", state);
    let report = c.step_actors(&blocked);
    assert!(report.edits.is_empty());
    assert_eq!(c.storage.total_count_of("create:shaft"), 2);

    // No place-state mapping for the filtered item -> no consumption.
    let unresolvable = StubWorld::default();
    let report = c.step_actors(&unresolvable);
    assert!(report.edits.is_empty());
    assert_eq!(c.storage.total_count_of("create:shaft"), 2);

    // Filtered item absent from storage -> nothing placed, nothing lost.
    c.storage = ContraptionStorage::new(4);
    c.storage.insert_item("minecraft:stone", 2);
    let report = c.step_actors(&air_world);
    assert!(report.edits.is_empty());
    assert_eq!(c.storage.total_count_of("minecraft:stone"), 2);
}

#[test]
fn unfiltered_deployer_uses_deterministic_item_choice() {
    let mut c = assemble_one(ContraptionKind::Bearing {
        angular_deg_per_tick: 0.0,
    });
    c.storage = ContraptionStorage::new(4);
    c.storage.insert_item("mod:bbb", 1);
    c.storage.insert_item("mod:aaa", 1);
    c.actors.push(ContraptionActor {
        local: [0, 0, 0],
        facing: [1, 0, 0],
        kind: ContraptionActorKind::MechanicalDeployer { filter_item: None },
    });
    let world = StubWorld::default()
        .place("mod:aaa", 41)
        .place("mod:bbb", 42);
    let report = c.step_actors(&world);
    assert_eq!(
        report.edits,
        [ActorWorldEdit::PlaceBlock {
            pos: DimBlockPos::new(DIM, 11, 64, 10),
            state_id: 41,
            item_id: "mod:aaa".to_string(),
        }]
    );
    assert_eq!(c.storage.total_count_of("mod:bbb"), 1);
}

#[test]
fn drill_and_deployer_loop_conserves_every_item() {
    let mut c = assemble_one(ContraptionKind::Bearing {
        angular_deg_per_tick: 0.0,
    });
    c.storage = ContraptionStorage::new(4);
    // Drill sweeps +X, deployer sweeps -X.
    c.actors.push(ContraptionActor {
        local: [0, 0, 0],
        facing: [1, 0, 0],
        kind: ContraptionActorKind::MechanicalDrill,
    });
    c.actors.push(ContraptionActor {
        local: [0, 0, 0],
        facing: [-1, 0, 0],
        kind: ContraptionActorKind::MechanicalDeployer { filter_item: None },
    });

    let world = StubWorld::default()
        .cell([11, 64, 10], ActorCell::block("minecraft:stone", true))
        .place("minecraft:stone", 99);
    let mut mined = 0_u32;
    let mut placed = 0_u32;
    let mut dropped = 0_u32;
    for _ in 0..2 {
        let report = c.step_actors(&world);
        for edit in &report.edits {
            match edit {
                ActorWorldEdit::BreakBlock { .. } => mined += 1,
                ActorWorldEdit::PlaceBlock { .. } => placed += 1,
                ActorWorldEdit::ResetCrop { .. } => {}
            }
        }
        for drop in &report.drops {
            dropped += drop.count;
        }
    }
    // Same-tick chain: mined stone was deployable immediately, so storage is
    // empty and every mined item resolved to a placement or drop — zero loss,
    // zero duplication.
    assert_eq!(mined, 2);
    assert_eq!(placed, 2);
    assert_eq!(dropped, 0);
    assert_eq!(c.storage.total_items(), 0);
    assert_eq!(placed + dropped + c.storage.total_items() as u32, mined);
}

#[test]
fn rotated_actor_still_sweeps_its_facing_cell() {
    // A drill on a bearing at yaw 90 sweeps the rotated -Z cell.
    let mut world = BTreeMap::new();
    world.insert(ANCHOR, shaft());
    let (mut c, _) = LiveContraption::assemble_from_world(
        1,
        DIM,
        ANCHOR,
        ANCHOR,
        &world,
        ContraptionKind::Bearing {
            angular_deg_per_tick: 15.0,
        },
        DEFAULT_MAX_CONTRAPTION_BLOCKS,
    )
    .unwrap();
    c.storage = ContraptionStorage::new(4);
    c.actors.push(ContraptionActor {
        local: [0, 0, 0],
        facing: [1, 0, 0],
        kind: ContraptionActorKind::MechanicalDrill,
    });
    for _ in 0..6 {
        c.step_tick().unwrap();
    }
    // The pose pivots at the anchor cell corner: at yaw 90 the cell a block
    // at local +X would physically occupy is (10,64,8), matching the drill's
    // swept world cell.
    let world = StubWorld::default().cell([10, 64, 8], ActorCell::block("minecraft:stone", true));
    let report = c.step_actors(&world);
    assert_eq!(
        report.edits,
        [ActorWorldEdit::BreakBlock {
            pos: DimBlockPos::new(DIM, 10, 64, 8)
        }]
    );
}

// ---------- Checked disassembly ----------

#[test]
fn disassemble_to_world_checked_reports_exact_conflicts() {
    let mut world = BTreeMap::new();
    world.insert(ANCHOR, shaft());
    world.insert([11, 64, 10], shaft());
    world.insert([12, 64, 10], shaft());
    let (mut c, _) = LiveContraption::assemble_from_world(
        1,
        DIM,
        ANCHOR,
        ANCHOR,
        &world,
        ContraptionKind::Bearing {
            angular_deg_per_tick: 15.0,
        },
        DEFAULT_MAX_CONTRAPTION_BLOCKS,
    )
    .unwrap();
    for _ in 0..6 {
        c.step_tick().unwrap();
    }
    // Targets after the 90-degree turn: (10,64,10), (10,64,9), (10,64,8).
    let occupied = DimBlockPos::new(DIM, 10, 64, 9);
    let conflicts = match c.disassemble_to_world_checked(|pos| pos == occupied) {
        Err(c) => c,
        Ok(_) => panic!("occupied immovable cell must fail closed"),
    };
    assert_eq!(conflicts, vec![occupied]);

    // All cells free -> identical to the unchecked result.
    let edits = c.disassemble_to_world_checked(|_| false).unwrap();
    assert_eq!(edits, c.disassemble_to_world());
}

// ---------- Save v2 snapshot ----------

#[test]
fn save_v2_snapshot_round_trip_preserves_storage_actors_and_pose() {
    let registry = reg();
    let mut world = BTreeMap::new();
    world.insert(ANCHOR, shaft());
    world.insert([11, 64, 10], shaft());
    let (mut c, _) = LiveContraption::assemble_from_world(
        9,
        DIM,
        ANCHOR,
        ANCHOR,
        &world,
        ContraptionKind::Bearing {
            angular_deg_per_tick: 15.0,
        },
        DEFAULT_MAX_CONTRAPTION_BLOCKS,
    )
    .unwrap();
    c.storage = ContraptionStorage::new(4);
    c.storage.insert_item("minecraft:stone", 7);
    c.storage.insert_item("create:shaft", 2);
    c.actors.push(ContraptionActor {
        local: [0, 0, 0],
        facing: [1, 0, 0],
        kind: ContraptionActorKind::MechanicalDeployer {
            filter_item: Some("create:shaft".to_string()),
        },
    });
    for _ in 0..6 {
        c.step_tick().unwrap();
    }

    let snap = c.snapshot(&registry);
    let restored = LiveContraption::restore(&snap, &registry).unwrap();
    assert_eq!(restored.block_count(), 2);
    assert!((restored.yaw_deg - 90.0).abs() < 1e-6);
    assert_eq!(restored.translation, c.translation);
    assert_eq!(restored.storage, c.storage);
    assert_eq!(restored.actors, c.actors);
    // A second snapshot of the restore is byte-for-byte the same model.
    assert_eq!(restored.snapshot(&registry), snap);
}

// ---------- Fail-closed bounds ----------

#[test]
fn oversized_contraption_fails_closed() {
    let mut world = BTreeMap::new();
    let count = DEFAULT_MAX_CONTRAPTION_BLOCKS + 1;
    let mut glue = SuperGlueVolume::new();
    glue.add_box(SuperGlueBox::new(
        [0, 64, 0],
        [DEFAULT_MAX_CONTRAPTION_BLOCKS as i32, 64, 0],
    ));
    for x in 0..count as i32 {
        world.insert([x, 64, 0], STONE);
    }
    let err = match LiveContraption::assemble_with_glue(
        1,
        DIM,
        [0, 64, 0],
        [0, 64, 0],
        &world,
        &glue,
        never_sticky,
        never_immovable,
        ContraptionKind::Linear {
            velocity_per_tick: [0.0; 3],
        },
        DEFAULT_MAX_CONTRAPTION_BLOCKS,
    ) {
        Err(e) => e,
        Ok(_) => panic!("oversized contraption must fail closed"),
    };
    assert_eq!(
        err,
        ContraptionError::TooLarge {
            count: DEFAULT_MAX_CONTRAPTION_BLOCKS + 1,
            limit: DEFAULT_MAX_CONTRAPTION_BLOCKS,
        }
    );
}

#[test]
fn nan_and_infinite_drivers_fail_closed_everywhere() {
    let mut world = BTreeMap::new();
    world.insert(ANCHOR, shaft());

    // NaN linear velocity is rejected at assembly.
    let err = LiveContraption::assemble_from_world(
        1,
        DIM,
        ANCHOR,
        ANCHOR,
        &world,
        ContraptionKind::Linear {
            velocity_per_tick: [f64::NAN, 0.0, 0.0],
        },
        DEFAULT_MAX_CONTRAPTION_BLOCKS,
    )
    .err()
    .unwrap();
    assert_eq!(err, ContraptionError::NonFiniteDriver);

    // Infinite minecart speed is rejected at assembly.
    let err = LiveContraption::assemble_from_world(
        2,
        DIM,
        ANCHOR,
        ANCHOR,
        &world,
        ContraptionKind::Minecart {
            speed_per_tick: f64::INFINITY,
            heading_yaw: 0.0,
        },
        DEFAULT_MAX_CONTRAPTION_BLOCKS,
    )
    .err()
    .unwrap();
    assert_eq!(err, ContraptionError::NonFiniteDriver);

    // A driver corrupted after assembly fails the tick without moving.
    let mut c = assemble_one(ContraptionKind::Linear {
        velocity_per_tick: [0.5, 0.0, 0.0],
    });
    c.step_tick().unwrap();
    c.kind = ContraptionKind::Bearing {
        angular_deg_per_tick: f32::NAN,
    };
    let err = c.step_tick().err().unwrap();
    assert_eq!(err, ContraptionError::NonFiniteDriver);
    assert_eq!(c.translation, [10.5, 64.0, 10.0]);
    assert_eq!(c.yaw_deg, 0.0);
}
