// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Integration tests for `WorldState`'s Protocol 767 live-state ingestion via
//! [`WorldState::apply_play_event`]: tracked entities (spawn, quantized
//! `VecDeltaCodec` movement, teleport, motion, hurt flash, removal, and the
//! 4096 cap), block entities (chunk payload, `block_entity_data`, and
//! `forget_chunk` pruning), world time/weather, and block-break overlays.

use megu_client::protocol::{
    DecodedBlockEntity, DecodedChunk, DecodedLightData, DecodedSection, DimensionMeta,
    EquipmentEntry, PlayEvent,
};
use megu_client::world::{
    BlockTable, WorldState, MAX_BLOCK_DESTRUCTIONS, MAX_TRACKED_BLOCK_ENTITIES,
    MAX_TRACKED_ENTITIES,
};

fn test_world() -> WorldState {
    WorldState::new(DimensionMeta::default(), BlockTable::builtin())
}

fn apply(world: &mut WorldState, event: PlayEvent) {
    world.apply_play_event(&event).expect("apply_play_event");
}

fn empty_light() -> DecodedLightData {
    DecodedLightData {
        sky_mask: Vec::new(),
        block_mask: Vec::new(),
        empty_sky_mask: Vec::new(),
        empty_block_mask: Vec::new(),
        sky_arrays: Vec::new(),
        block_arrays: Vec::new(),
    }
}

/// One-section (all air) chunk column; `min_y = -64`, so section 0 covers
/// world y `-64..=-49`.
fn chunk(cx: i32, cz: i32, block_entities: Vec<DecodedBlockEntity>) -> DecodedChunk {
    DecodedChunk {
        cx,
        cz,
        sections: vec![DecodedSection {
            non_air: 0,
            states: Box::new([0u32; 4096]),
            biomes: [0u32; 64],
        }],
        light: empty_light(),
        block_entities,
    }
}

fn add_entity(eid: i32, x: f64, y: f64, z: f64) -> PlayEvent {
    PlayEvent::AddEntity {
        eid,
        uuid: [7u8; 16],
        entity_type: 91,
        x,
        y,
        z,
        pitch: 0.0,
        yaw: 0.0,
        head_yaw: 0.0,
        data: 0,
        vx: 0.0,
        vy: 0.0,
        vz: 0.0,
    }
}

#[test]
fn entity_spawn_move_teleport_motion_hurt_and_remove() {
    let mut world = test_world();

    // -- spawn ---------------------------------------------------------
    apply(
        &mut world,
        PlayEvent::AddEntity {
            eid: 42,
            uuid: [7u8; 16],
            entity_type: 91,
            x: 10.5,
            y: 64.0,
            z: -3.25,
            pitch: 90.0,
            yaw: 0.0,
            head_yaw: -1.40625,
            data: 7,
            vx: 1.0,
            vy: -2.0,
            vz: 0.0,
        },
    );
    let e = world.entities.get(&42).expect("spawn tracked");
    assert_eq!(e.pos, [10.5, 64.0, -3.25]);
    assert_eq!(e.pos_q, [43008, 262144, -13312]);
    assert_eq!(e.vel, [1.0, -2.0, 0.0]);
    assert_eq!(e.entity_type, 91);
    assert_eq!(e.uuid, [7u8; 16]);
    assert_eq!(e.pitch_deg, 90.0);
    assert_eq!(e.head_yaw_deg, -1.40625);
    assert!(!e.on_ground);
    assert_eq!(e.hurt_ticks, 0);
    assert_eq!(e.equipment_slots, 0);

    // -- quantized VecDeltaCodec accumulation ---------------------------
    // Wire deltas are i16/4096 blocks; pos_q accumulates the exact integer.
    apply(
        &mut world,
        PlayEvent::MoveEntityPos {
            eid: 42,
            dx: 1.0,
            dy: -2.0,
            dz: 0.25,
            on_ground: true,
        },
    );
    let e = world.entities.get(&42).unwrap();
    assert_eq!(e.pos, [11.5, 62.0, -3.0]);
    assert_eq!(e.pos_q, [47104, 253952, -12288]);
    assert!(e.on_ground);

    apply(
        &mut world,
        PlayEvent::MoveEntityPosRot {
            eid: 42,
            dx: 0.25,
            dy: 0.0,
            dz: -0.5,
            yaw: 90.0,
            pitch: 45.0,
            on_ground: false,
        },
    );
    let e = world.entities.get(&42).unwrap();
    assert_eq!(e.pos, [11.75, 62.0, -3.5]);
    assert_eq!(e.pos_q, [48128, 253952, -14336]);
    assert_eq!(e.yaw_deg, 90.0);
    assert_eq!(e.pitch_deg, 45.0);
    assert!(!e.on_ground);

    // Rotation-only move leaves position untouched.
    apply(
        &mut world,
        PlayEvent::MoveEntityRot {
            eid: 42,
            yaw: 180.0,
            pitch: -90.0,
            on_ground: true,
        },
    );
    let e = world.entities.get(&42).unwrap();
    assert_eq!(e.pos, [11.75, 62.0, -3.5]);
    assert_eq!(e.pos_q, [48128, 253952, -14336]);
    assert_eq!(e.yaw_deg, 180.0);
    assert_eq!(e.pitch_deg, -90.0);
    assert!(e.on_ground);

    apply(
        &mut world,
        PlayEvent::RotateHead {
            eid: 42,
            head_yaw: 270.0,
        },
    );
    assert_eq!(world.entities.get(&42).unwrap().head_yaw_deg, 270.0);

    // -- teleport overwrites absolutely ----------------------------------
    apply(
        &mut world,
        PlayEvent::TeleportEntity {
            eid: 42,
            x: 100.0,
            y: 200.0,
            z: -50.0,
            yaw: 0.0,
            pitch: 0.0,
            on_ground: true,
        },
    );
    let e = world.entities.get(&42).unwrap();
    assert_eq!(e.pos, [100.0, 200.0, -50.0]);
    assert_eq!(e.pos_q, [409600, 819200, -204800]);
    assert!(e.on_ground);

    // -- velocity ---------------------------------------------------------
    apply(
        &mut world,
        PlayEvent::SetEntityMotion {
            eid: 42,
            vx: 0.5,
            vy: -0.25,
            vz: 0.125,
        },
    );
    assert_eq!(world.entities.get(&42).unwrap().vel, [0.5, -0.25, 0.125]);

    // -- hurt flash --------------------------------------------------------
    apply(
        &mut world,
        PlayEvent::HurtAnimation { eid: 42, yaw: 90.0 },
    );
    assert_eq!(world.entities.get(&42).unwrap().hurt_ticks, 10);
    world.tick_entities();
    assert_eq!(world.entities.get(&42).unwrap().hurt_ticks, 9);

    // `entity_event` status 2 is the same vanilla hurt flash; other
    // statuses leave the timer alone.
    world.tick_entities();
    apply(&mut world, PlayEvent::EntityEvent { eid: 42, status: 3 });
    assert_eq!(world.entities.get(&42).unwrap().hurt_ticks, 8);
    apply(&mut world, PlayEvent::EntityEvent { eid: 42, status: 2 });
    assert_eq!(world.entities.get(&42).unwrap().hurt_ticks, 10);

    // -- equipment slot bitmask ---------------------------------------------
    apply(
        &mut world,
        PlayEvent::SetEquipment {
            eid: 42,
            entries: vec![
                EquipmentEntry {
                    slot: 0,
                    item_wire: vec![0],
                },
                EquipmentEntry {
                    slot: 1,
                    item_wire: vec![0],
                },
                EquipmentEntry {
                    slot: 7,
                    item_wire: vec![0],
                },
                // Slot ids >= 8 are not representable in the u8 mask.
                EquipmentEntry {
                    slot: 9,
                    item_wire: vec![0],
                },
            ],
        },
    );
    assert_eq!(world.entities.get(&42).unwrap().equipment_slots, 0b1000_0011);

    // -- unknown ids are ignored --------------------------------------------
    apply(
        &mut world,
        PlayEvent::MoveEntityPos {
            eid: 777,
            dx: 1.0,
            dy: 0.0,
            dz: 0.0,
            on_ground: false,
        },
    );
    apply(
        &mut world,
        PlayEvent::TeleportEntity {
            eid: 777,
            x: 0.0,
            y: 0.0,
            z: 0.0,
            yaw: 0.0,
            pitch: 0.0,
            on_ground: false,
        },
    );
    assert!(!world.entities.contains_key(&777));

    // -- removal --------------------------------------------------------------
    apply(&mut world, PlayEvent::RemoveEntities { eids: vec![42, 999] });
    assert!(world.entities.is_empty());
}

#[test]
fn entity_tracking_is_bounded_at_4096() {
    let mut world = test_world();
    for eid in 0..(MAX_TRACKED_ENTITIES as i32 + 8) {
        apply(&mut world, add_entity(eid, eid as f64, 0.0, 0.0));
    }
    assert_eq!(world.entities.len(), MAX_TRACKED_ENTITIES);
    assert!(world.entities.contains_key(&0));
    // Excess spawns past the cap are ignored cleanly.
    assert!(!world.entities.contains_key(&(MAX_TRACKED_ENTITIES as i32 + 7)));

    // Re-spawning a tracked id still replaces its row at the cap.
    apply(&mut world, add_entity(0, 5.0, 6.0, 7.0));
    assert_eq!(world.entities.get(&0).unwrap().pos, [5.0, 6.0, 7.0]);
    assert_eq!(world.entities.len(), MAX_TRACKED_ENTITIES);
}

#[test]
fn block_entities_load_update_and_prune_with_chunks() {
    let mut world = test_world();

    // Production path: `apply_chunk` (what sim.rs / client-gpu call today).
    // packed_xz 0x34 -> local (3, 4); world [4*16+3, -60, -9*16+4] = [67,-60,-140].
    // y=-60 sits in section 0 so the block-update check below lands.
    world
        .apply_chunk(chunk(
            4,
            -9,
            vec![DecodedBlockEntity {
                packed_xz: 0x34,
                y: -60,
                kind: 14,
                nbt_bytes: vec![10, 0],
            }],
        ))
        .expect("apply_chunk");
    let be = world
        .block_entities
        .get(&[67, -60, -140])
        .expect("chunk block entity retained");
    assert_eq!(be.kind, 14);
    assert_eq!(be.nbt_bytes, vec![10, 0]);

    // Unified path: `apply_play_event`'s ChunkData arm for a second column.
    // packed_xz 0x00 -> local (0, 0); world [0, -5, 0].
    apply(
        &mut world,
        PlayEvent::ChunkData(Box::new(chunk(
            0,
            0,
            vec![DecodedBlockEntity {
                packed_xz: 0x00,
                y: -5,
                kind: 7,
                nbt_bytes: vec![1, 2, 3],
            }],
        ))),
    );
    let be = world
        .block_entities
        .get(&[0, -5, 0])
        .expect("ChunkData block entity retained");
    assert_eq!(be.kind, 7);
    assert_eq!(be.nbt_bytes, vec![1, 2, 3]);

    // `block_entity_data` upserts: replace the chunk-loaded record and
    // create a record for a not-yet-loaded position.
    apply(
        &mut world,
        PlayEvent::BlockEntityData {
            pos: [67, -60, -140],
            kind: 15,
            nbt_bytes: vec![9, 9],
        },
    );
    let be = world.block_entities.get(&[67, -60, -140]).unwrap();
    assert_eq!(be.kind, 15);
    assert_eq!(be.nbt_bytes, vec![9, 9]);
    apply(
        &mut world,
        PlayEvent::BlockEntityData {
            pos: [20, 2, 3],
            kind: 1,
            nbt_bytes: Vec::new(),
        },
    );
    assert!(world.block_entities.contains_key(&[20, 2, 3]));

    // A real block-state change retires the anchored block entity.
    apply(
        &mut world,
        PlayEvent::BlockUpdate {
            pos: [67, -60, -140],
            state_id: 1,
        },
    );
    assert!(!world.block_entities.contains_key(&[67, -60, -140]));
    // `block_entity_data` can re-anchor it afterwards.
    apply(
        &mut world,
        PlayEvent::BlockEntityData {
            pos: [67, -60, -140],
            kind: 15,
            nbt_bytes: vec![4],
        },
    );
    assert!(world.block_entities.contains_key(&[67, -60, -140]));

    // UnloadChunk prunes only that column's block entities.
    apply(&mut world, PlayEvent::UnloadChunk { cx: 4, cz: -9 });
    assert!(!world.block_entities.contains_key(&[67, -60, -140]));
    assert!(world.block_entities.contains_key(&[0, -5, 0]));
    assert!(world.block_entities.contains_key(&[20, 2, 3]));

    // forget_chunk(0,0) removes the second column's record; [20,2,3] lives
    // in chunk (1,0) and survives.
    world.forget_chunk(0, 0);
    assert!(!world.block_entities.contains_key(&[0, -5, 0]));
    assert!(world.block_entities.contains_key(&[20, 2, 3]));
}

#[test]
fn block_entity_tracking_is_bounded() {
    let mut world = test_world();
    for i in 0..(MAX_TRACKED_BLOCK_ENTITIES + 4) {
        apply(
            &mut world,
            PlayEvent::BlockEntityData {
                pos: [i as i32, 0, 0],
                kind: 1,
                nbt_bytes: Vec::new(),
            },
        );
    }
    assert_eq!(world.block_entities.len(), MAX_TRACKED_BLOCK_ENTITIES);
}

#[test]
fn set_time_and_weather_events_track_and_clamp() {
    let mut world = test_world();

    apply(
        &mut world,
        PlayEvent::SetTime {
            world_age: 123456,
            time_of_day: 6000,
        },
    );
    assert_eq!(world.world_age, 123456);
    assert_eq!(world.time_of_day, 6000);

    // start/stop rain drive the level directly.
    apply(&mut world, PlayEvent::GameEvent { event: 1, value: 0.0 });
    assert_eq!(world.rain_level, 1.0);
    apply(&mut world, PlayEvent::GameEvent { event: 2, value: 0.0 });
    assert_eq!(world.rain_level, 0.0);

    // level changes carry the float, clamped to 0.0..=1.0.
    apply(&mut world, PlayEvent::GameEvent { event: 7, value: 0.5 });
    assert_eq!(world.rain_level, 0.5);
    apply(&mut world, PlayEvent::GameEvent { event: 7, value: 1.5 });
    assert_eq!(world.rain_level, 1.0);
    apply(&mut world, PlayEvent::GameEvent { event: 7, value: -0.25 });
    assert_eq!(world.rain_level, 0.0);

    apply(&mut world, PlayEvent::GameEvent { event: 8, value: 0.25 });
    assert_eq!(world.thunder_level, 0.25);
    apply(&mut world, PlayEvent::GameEvent { event: 8, value: 2.0 });
    assert_eq!(world.thunder_level, 1.0);

    // NaN / infinite values are sanitized to 0.0 rather than poisoning state.
    apply(
        &mut world,
        PlayEvent::GameEvent {
            event: 7,
            value: f32::NAN,
        },
    );
    assert_eq!(world.rain_level, 0.0);
    apply(
        &mut world,
        PlayEvent::GameEvent {
            event: 8,
            value: f32::INFINITY,
        },
    );
    assert_eq!(world.thunder_level, 0.0);
    apply(
        &mut world,
        PlayEvent::GameEvent {
            event: 8,
            value: f32::NEG_INFINITY,
        },
    );
    assert_eq!(world.thunder_level, 0.0);

    // Unrelated game events leave weather untouched.
    apply(&mut world, PlayEvent::GameEvent { event: 3, value: 1.0 });
    assert_eq!(world.rain_level, 0.0);
    assert_eq!(world.thunder_level, 0.0);
}

#[test]
fn block_destruction_stages_update_and_clear() {
    let mut world = test_world();

    // Stages 0..=9 record and update the overlay.
    for stage in 0..=9i8 {
        apply(
            &mut world,
            PlayEvent::BlockDestruction {
                breaker_eid: 5,
                pos: [1, 2, 3],
                stage,
            },
        );
        let progress = world.block_destructions.get(&5).expect("overlay tracked");
        assert_eq!(progress.stage, stage as u8);
        assert_eq!(progress.pos, [1, 2, 3]);
    }

    // Stage 10 clears; the wire -1 sentinel (255 as u8) clears too.
    apply(
        &mut world,
        PlayEvent::BlockDestruction {
            breaker_eid: 5,
            pos: [1, 2, 3],
            stage: 10,
        },
    );
    assert!(!world.block_destructions.contains_key(&5));
    apply(
        &mut world,
        PlayEvent::BlockDestruction {
            breaker_eid: 5,
            pos: [1, 2, 3],
            stage: 4,
        },
    );
    apply(
        &mut world,
        PlayEvent::BlockDestruction {
            breaker_eid: 5,
            pos: [1, 2, 3],
            stage: -1,
        },
    );
    assert!(!world.block_destructions.contains_key(&5));

    // Breakers are independent; removing the entity clears its overlay.
    apply(
        &mut world,
        PlayEvent::BlockDestruction {
            breaker_eid: 6,
            pos: [9, 9, 9],
            stage: 2,
        },
    );
    apply(
        &mut world,
        PlayEvent::BlockDestruction {
            breaker_eid: 7,
            pos: [8, 8, 8],
            stage: 7,
        },
    );
    apply(&mut world, PlayEvent::RemoveEntities { eids: vec![6] });
    assert!(!world.block_destructions.contains_key(&6));
    assert!(world.block_destructions.contains_key(&7));
}

#[test]
fn block_destructions_are_bounded() {
    let mut world = test_world();
    for eid in 0..(MAX_BLOCK_DESTRUCTIONS as i32 + 4) {
        apply(
            &mut world,
            PlayEvent::BlockDestruction {
                breaker_eid: eid,
                pos: [0, 0, 0],
                stage: 1,
            },
        );
    }
    assert_eq!(world.block_destructions.len(), MAX_BLOCK_DESTRUCTIONS);

    // Updates to already-tracked breakers still land at the cap.
    apply(
        &mut world,
        PlayEvent::BlockDestruction {
            breaker_eid: 0,
            pos: [5, 5, 5],
            stage: 9,
        },
    );
    assert_eq!(
        world.block_destructions.get(&0).unwrap().stage,
        9,
        "tracked breaker updates are not capped"
    );
}
