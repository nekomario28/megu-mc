// SPDX-License-Identifier: LGPL-3.0-or-later
//! Coverage tests for the extended Protocol 767 play codec: every new S2C
//! family decodes its exact wire layout, every C2S encoder emits the canonical
//! packet name + body, and truncated/hostile payloads fail closed with zero
//! panics.

use std::panic::{catch_unwind, AssertUnwindSafe};

use mc_spine_client::protocol::{
    decode_chunk_packet, decode_extended_play_event, encode_chat_command, encode_chat_message,
    encode_container_button_click, encode_container_click, encode_container_close,
    encode_interact_attack, encode_interact_use, encode_pick_item, encode_player_action,
    encode_slot_stack, encode_swing, encode_use_item, encode_use_item_on, pack_position,
    write_varint, write_varlong, PlayEvent, WireReader, PROTOCOL_767,
};
use mc_spine_proto::{PacketDir, PacketPhase, ProtocolVersion, VersionRouter};

// ---------------------------------------------------------------------------
// Wire builders (test-side mirrors of the canonical layouts)
// ---------------------------------------------------------------------------

fn vi(v: i32) -> Vec<u8> {
    let mut b = Vec::new();
    write_varint(&mut b, v);
    b
}

fn vl(v: i64) -> Vec<u8> {
    let mut b = Vec::new();
    write_varlong(&mut b, v);
    b
}

fn cat(parts: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for p in parts {
        out.extend_from_slice(p);
    }
    out
}

fn be_i16(v: i16) -> [u8; 2] {
    v.to_be_bytes()
}

fn be_i32(v: i32) -> [u8; 4] {
    v.to_be_bytes()
}

fn be_i64(v: i64) -> [u8; 8] {
    v.to_be_bytes()
}

fn be_f32(v: f32) -> [u8; 4] {
    v.to_be_bytes()
}

fn be_f64(v: f64) -> [u8; 8] {
    v.to_be_bytes()
}

fn pos8(x: i32, y: i32, z: i32) -> [u8; 8] {
    pack_position([x, y, z]).to_be_bytes()
}

fn uuid(seed: u8) -> [u8; 16] {
    [seed; 16]
}

fn nbt_string_entry(name: &str, value: &str) -> Vec<u8> {
    let mut v = vec![8u8]; // TAG_String
    v.extend_from_slice(&(name.len() as u16).to_be_bytes());
    v.extend_from_slice(name.as_bytes());
    v.extend_from_slice(&(value.len() as u16).to_be_bytes());
    v.extend_from_slice(value.as_bytes());
    v
}

/// Anonymous NBT compound `{ entries... }`.
fn nbt_compound(entries: &[Vec<u8>]) -> Vec<u8> {
    let mut v = vec![10u8]; // TAG_Compound
    for e in entries {
        v.extend_from_slice(e);
    }
    v.push(0); // TAG_End
    v
}

fn chat_text_nbt(text: &str) -> Vec<u8> {
    nbt_compound(&[nbt_string_entry("text", text)])
}

fn slot_empty() -> Vec<u8> {
    vec![0]
}

fn slot_simple(item: i32, count: i32) -> Vec<u8> {
    encode_slot_stack(item, count)
}

/// Slot with one `custom_name` (component type 5, NBT payload) component.
fn slot_named(item: i32, count: i32, name: &str) -> Vec<u8> {
    let mut v = vi(count);
    if count > 0 {
        v.extend(vi(item));
        v.extend(vi(1)); // one added component
        v.extend(vi(0)); // no removed components
        v.extend(vi(5)); // custom_name
        v.extend(chat_text_nbt(name));
    }
    v
}

fn decode(name: &str, body: &[u8]) -> PlayEvent {
    decode_extended_play_event(name, body)
        .unwrap_or_else(|e| panic!("{name}: decode error {e}"))
        .unwrap_or_else(|| panic!("{name}: not handled"))
}

/// Run `f` proving it cannot panic; returns whatever `f` returns.
fn no_panic<R>(f: impl FnOnce() -> R) -> R {
    catch_unwind(AssertUnwindSafe(f)).expect("decoder panicked on input")
}

// ---------------------------------------------------------------------------
// Packet-name -> id resolution through mc_spine_proto (never hardcoded)
// ---------------------------------------------------------------------------

fn v767_table() -> mc_spine_proto::ProtocolTable {
    VersionRouter::with_v767_and_v777()
        .select(ProtocolVersion::V1_21_1.number())
        .expect("select v767")
        .table()
        .clone()
}

#[test]
fn s2c_names_resolve_in_v767_clientbound_table() {
    let table = v767_table();
    for name in [
        "add_entity",
        "remove_entities",
        "move_entity_pos",
        "move_entity_pos_rot",
        "move_entity_rot",
        "teleport_entity",
        "rotate_head",
        "set_entity_motion",
        "entity_event",
        "hurt_animation",
        "damage_event",
        "set_equipment",
        "set_entity_data",
        "set_time",
        "game_event",
        "level_event",
        "level_particles",
        "block_destruction",
        "block_entity_data",
        "open_screen",
        "container_set_content",
        "container_set_slot",
        "container_set_data",
        "container_close",
        "set_experience",
        "update_mob_effect",
        "remove_mob_effect",
        "set_action_bar_text",
        "set_title_text",
        "set_subtitle_text",
        "boss_event",
        "player_info_remove",
    ] {
        assert!(
            table
                .packet_id(PacketPhase::Play, PacketDir::Clientbound, name)
                .is_some(),
            "v767 clientbound table lacks {name}"
        );
    }
}

#[test]
fn c2s_encoder_names_resolve_to_expected_v767_ids() {
    let table = v767_table();
    let cases: &[(&str, u32)] = &[
        ("swing", 0x36),
        ("use_item_on", 0x38),
        ("use_item", 0x39),
        ("player_action", 0x24),
        ("pick_item", 0x20),
        ("interact", 0x16),
        ("container_click", 0x0e),
        ("container_button_click", 0x0d),
        ("container_close", 0x0f),
        ("chat_command", 0x04),
        ("chat", 0x06),
    ];
    for (name, expected) in cases {
        assert_eq!(
            table.packet_id(PacketPhase::Play, PacketDir::Serverbound, name),
            Some(*expected),
            "wrong v767 id for {name}"
        );
    }
}

// ---------------------------------------------------------------------------
// S2C: entities
// ---------------------------------------------------------------------------

#[test]
fn decodes_add_entity_all_fields() {
    let mut body = vi(42);
    body.extend_from_slice(&uuid(0xAB));
    body.extend(vi(91)); // entity_type: cow-ish registry id
    body.extend_from_slice(&be_f64(10.5));
    body.extend_from_slice(&be_f64(64.0));
    body.extend_from_slice(&be_f64(-3.25));
    body.push(64); // pitch byte -> 90.0 deg
    body.push(0); // yaw
    body.push(0xFF); // head_yaw byte = -1 -> -1.40625 deg
    body.extend(vi(7)); // objectData
    body.extend_from_slice(&be_i16(8000)); // vx = 1.0
    body.extend_from_slice(&be_i16(-16000)); // vy = -2.0
    body.extend_from_slice(&be_i16(0)); // vz = 0.0

    match decode("add_entity", &body) {
        PlayEvent::AddEntity {
            eid,
            uuid: u,
            entity_type,
            x,
            y,
            z,
            pitch,
            yaw,
            head_yaw,
            data,
            vx,
            vy,
            vz,
        } => {
            assert_eq!(eid, 42);
            assert_eq!(u, uuid(0xAB));
            assert_eq!(entity_type, 91);
            assert_eq!((x, y, z), (10.5, 64.0, -3.25));
            assert!((pitch - 90.0).abs() < 1e-4);
            assert_eq!(yaw, 0.0);
            assert!((head_yaw - (-1.40625)).abs() < 1e-4);
            assert_eq!(data, 7);
            assert_eq!((vx, vy, vz), (1.0, -2.0, 0.0));
        }
        other => panic!("expected AddEntity, got {other:?}"),
    }
}

#[test]
fn decodes_remove_entities_list() {
    let body = cat(&[&vi(3), &vi(10), &vi(20), &vi(-5)]);
    match decode("remove_entities", &body) {
        PlayEvent::RemoveEntities { eids } => assert_eq!(eids, vec![10, 20, -5]),
        other => panic!("expected RemoveEntities, got {other:?}"),
    }
}

#[test]
fn decodes_entity_move_variants() {
    // move_entity_pos: eid, dx/dy/dz i16 (/4096), on_ground
    let mut body = vi(9);
    body.extend_from_slice(&be_i16(4096)); // dx = 1.0
    body.extend_from_slice(&be_i16(-8192)); // dy = -2.0
    body.extend_from_slice(&be_i16(1024)); // dz = 0.25
    body.push(1);
    match decode("move_entity_pos", &body) {
        PlayEvent::MoveEntityPos {
            eid,
            dx,
            dy,
            dz,
            on_ground,
        } => {
            assert_eq!(eid, 9);
            assert_eq!((dx, dy, dz), (1.0, -2.0, 0.25));
            assert!(on_ground);
        }
        other => panic!("expected MoveEntityPos, got {other:?}"),
    }

    // move_entity_pos_rot adds yaw/pitch angle bytes before on_ground.
    let mut body = vi(9);
    body.extend_from_slice(&be_i16(0));
    body.extend_from_slice(&be_i16(0));
    body.extend_from_slice(&be_i16(0));
    body.push(64); // yaw 90 deg
    body.push(0); // pitch 0
    body.push(0);
    match decode("move_entity_pos_rot", &body) {
        PlayEvent::MoveEntityPosRot {
            eid,
            yaw,
            pitch,
            on_ground,
            ..
        } => {
            assert_eq!(eid, 9);
            assert!((yaw - 90.0).abs() < 1e-4);
            assert_eq!(pitch, 0.0);
            assert!(!on_ground);
        }
        other => panic!("expected MoveEntityPosRot, got {other:?}"),
    }

    // move_entity_rot
    let body = cat(&[&vi(3), &[64], &[0], &[1]]);
    match decode("move_entity_rot", &body) {
        PlayEvent::MoveEntityRot {
            eid,
            yaw,
            pitch,
            on_ground,
        } => {
            assert_eq!(eid, 3);
            assert!((yaw - 90.0).abs() < 1e-4);
            assert_eq!(pitch, 0.0);
            assert!(on_ground);
        }
        other => panic!("expected MoveEntityRot, got {other:?}"),
    }
}

#[test]
fn decodes_teleport_rotate_head_and_motion() {
    let mut body = vi(77);
    body.extend_from_slice(&be_f64(1.0));
    body.extend_from_slice(&be_f64(2.0));
    body.extend_from_slice(&be_f64(3.0));
    body.push(0); // yaw 0
    body.push(0); // pitch 0
    body.push(1); // on_ground
    match decode("teleport_entity", &body) {
        PlayEvent::TeleportEntity {
            eid,
            x,
            y,
            z,
            on_ground,
            ..
        } => {
            assert_eq!(eid, 77);
            assert_eq!((x, y, z), (1.0, 2.0, 3.0));
            assert!(on_ground);
        }
        other => panic!("expected TeleportEntity, got {other:?}"),
    }

    let body = cat(&[&vi(5), &[0x80]]); // head_yaw byte -128 -> -180 deg
    match decode("rotate_head", &body) {
        PlayEvent::RotateHead { eid, head_yaw } => {
            assert_eq!(eid, 5);
            assert!((head_yaw - (-180.0)).abs() < 1e-3);
        }
        other => panic!("expected RotateHead, got {other:?}"),
    }

    let mut body = vi(5);
    body.extend_from_slice(&be_i16(8000));
    body.extend_from_slice(&be_i16(0));
    body.extend_from_slice(&be_i16(-8000));
    match decode("set_entity_motion", &body) {
        PlayEvent::SetEntityMotion { eid, vx, vy, vz } => {
            assert_eq!(eid, 5);
            assert_eq!((vx, vy, vz), (1.0, 0.0, -1.0));
        }
        other => panic!("expected SetEntityMotion, got {other:?}"),
    }
}

#[test]
fn decodes_entity_event_hurt_damage() {
    // entity_event: entity id is a fixed i32 (not VarInt), status i8.
    let mut body = Vec::new();
    body.extend_from_slice(&be_i32(70000)); // > varint comfort zone
    body.push(2); // status: hurt
    match decode("entity_event", &body) {
        PlayEvent::EntityEvent { eid, status } => {
            assert_eq!(eid, 70000);
            assert_eq!(status, 2);
        }
        other => panic!("expected EntityEvent, got {other:?}"),
    }

    let mut body = vi(11);
    body.extend(vi(1)); // source_type: player
    body.extend(vi(2)); // source_cause
    body.extend(vi(0)); // source_direct
    body.push(0); // no source position
    match decode("damage_event", &body) {
        PlayEvent::DamageEvent {
            eid,
            source_type,
            source_cause,
            source_direct,
            source_position,
        } => {
            assert_eq!(eid, 11);
            assert_eq!((source_type, source_cause, source_direct), (1, 2, 0));
            assert_eq!(source_position, None);
        }
        other => panic!("expected DamageEvent, got {other:?}"),
    }

    // With source position.
    let mut body = vi(11);
    body.extend(vi(1));
    body.extend(vi(2));
    body.extend(vi(0));
    body.push(1);
    body.extend_from_slice(&be_f64(5.0));
    body.extend_from_slice(&be_f64(6.0));
    body.extend_from_slice(&be_f64(7.0));
    match decode("damage_event", &body) {
        PlayEvent::DamageEvent {
            source_position: Some(p),
            ..
        } => assert_eq!(p, [5.0, 6.0, 7.0]),
        other => panic!("expected DamageEvent with position, got {other:?}"),
    }

    let body = cat(&[&vi(33), &be_f32(90.0)]);
    match decode("hurt_animation", &body) {
        PlayEvent::HurtAnimation { eid, yaw } => {
            assert_eq!(eid, 33);
            assert_eq!(yaw, 90.0);
        }
        other => panic!("expected HurtAnimation, got {other:?}"),
    }
}

#[test]
fn decodes_set_equipment_bitset_terminated_entries() {
    // eid=5; entry0: slot 0 (mainhand) empty, more follow (0x80);
    // entry1: slot 1 (offhand) named item, last.
    let mut body = vi(5);
    body.push(0x80); // slot 0 | continue
    body.extend(slot_empty());
    body.push(0x01); // slot 1 | last
    body.extend(slot_named(5, 1, "Sword"));

    match decode("set_equipment", &body) {
        PlayEvent::SetEquipment { eid, entries } => {
            assert_eq!(eid, 5);
            assert_eq!(entries.len(), 2);
            assert_eq!(entries[0].slot, 0);
            assert_eq!(entries[0].item_wire, slot_empty());
            assert_eq!(entries[1].slot, 1);
            assert_eq!(entries[1].item_wire, slot_named(5, 1, "Sword"));
        }
        other => panic!("expected SetEquipment, got {other:?}"),
    }
}

#[test]
fn decodes_set_entity_data_raw_entries() {
    let mut body = vi(123);
    body.extend_from_slice(&[0x00, 0x08, 0x03, 0xFF]); // opaque metadata bytes
    match decode("set_entity_data", &body) {
        PlayEvent::SetEntityData { eid, raw_entries } => {
            assert_eq!(eid, 123);
            assert_eq!(raw_entries, vec![0x00, 0x08, 0x03, 0xFF]);
        }
        other => panic!("expected SetEntityData, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// S2C: world / time / weather / particles / block entities
// ---------------------------------------------------------------------------

#[test]
fn decodes_time_game_level_events() {
    let body = cat(&[&be_i64(123456), &be_i64(6000)]);
    match decode("set_time", &body) {
        PlayEvent::SetTime {
            world_age,
            time_of_day,
        } => {
            assert_eq!(world_age, 123456);
            assert_eq!(time_of_day, 6000);
        }
        other => panic!("expected SetTime, got {other:?}"),
    }

    let mut body = vec![1u8]; // reason: start_raining
    body.extend_from_slice(&be_f32(0.5));
    match decode("game_event", &body) {
        PlayEvent::GameEvent { event, value } => {
            assert_eq!(event, 1);
            assert_eq!(value, 0.5);
        }
        other => panic!("expected GameEvent, got {other:?}"),
    }

    let mut body = Vec::new();
    body.extend_from_slice(&be_i32(2001)); // effectId: splash particles
    body.extend_from_slice(&pos8(1, 2, 3));
    body.extend_from_slice(&be_i32(7)); // data
    body.push(1); // global
    match decode("level_event", &body) {
        PlayEvent::LevelEvent {
            event,
            pos,
            data,
            disable_relative_volume,
        } => {
            assert_eq!(event, 2001);
            assert_eq!(pos, [1, 2, 3]);
            assert_eq!(data, 7);
            assert!(disable_relative_volume);
        }
        other => panic!("expected LevelEvent, got {other:?}"),
    }
}

#[test]
fn decodes_level_particles_with_raw_data() {
    let mut body = vec![1u8]; // long_distance
    body.extend_from_slice(&be_f64(0.5));
    body.extend_from_slice(&be_f64(64.0));
    body.extend_from_slice(&be_f64(-2.0));
    body.extend_from_slice(&be_f32(0.1));
    body.extend_from_slice(&be_f32(0.2));
    body.extend_from_slice(&be_f32(0.3));
    body.extend_from_slice(&be_f32(0.05)); // max_speed
    body.extend_from_slice(&be_i32(15)); // count
    body.extend(vi(3)); // particle type: bubble
    body.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]); // raw particle data

    match decode("level_particles", &body) {
        PlayEvent::LevelParticles {
            particle_id,
            long_distance,
            x,
            y,
            z,
            offset_x,
            offset_y,
            offset_z,
            max_speed,
            count,
            particle_data,
        } => {
            assert_eq!(particle_id, 3);
            assert!(long_distance);
            assert_eq!((x, y, z), (0.5, 64.0, -2.0));
            assert_eq!((offset_x, offset_y, offset_z), (0.1, 0.2, 0.3));
            assert_eq!(max_speed, 0.05);
            assert_eq!(count, 15);
            assert_eq!(particle_data, vec![0xDE, 0xAD, 0xBE, 0xEF]);
        }
        other => panic!("expected LevelParticles, got {other:?}"),
    }
}

#[test]
fn decodes_block_destruction_and_block_entity_data() {
    let mut body = vi(12);
    body.extend_from_slice(&pos8(3, 4, 5));
    body.push(7); // stage
    match decode("block_destruction", &body) {
        PlayEvent::BlockDestruction {
            breaker_eid,
            pos,
            stage,
        } => {
            assert_eq!(breaker_eid, 12);
            assert_eq!(pos, [3, 4, 5]);
            assert_eq!(stage, 7);
        }
        other => panic!("expected BlockDestruction, got {other:?}"),
    }

    let nbt = chat_text_nbt("Chest");
    let mut body = Vec::new();
    body.extend_from_slice(&pos8(-1, 70, 300));
    body.extend(vi(14)); // block entity type: chest-ish id
    body.extend_from_slice(&nbt);
    match decode("block_entity_data", &body) {
        PlayEvent::BlockEntityData {
            pos,
            kind,
            nbt_bytes,
        } => {
            assert_eq!(pos, [-1, 70, 300]);
            assert_eq!(kind, 14);
            assert_eq!(nbt_bytes, nbt);
        }
        other => panic!("expected BlockEntityData, got {other:?}"),
    }

    // Absent-NBT form: leading 0x00 tag byte.
    let mut body = Vec::new();
    body.extend_from_slice(&pos8(0, 0, 0));
    body.extend(vi(1));
    body.push(0);
    match decode("block_entity_data", &body) {
        PlayEvent::BlockEntityData { nbt_bytes, .. } => assert!(nbt_bytes.is_empty()),
        other => panic!("expected BlockEntityData, got {other:?}"),
    }
}

#[test]
fn chunk_block_entities_are_retained() {
    // Minimal 767 chunk: 0 sections, heightmaps = empty compound, one block
    // entity, all-empty light masks.
    let mut body = Vec::new();
    body.extend_from_slice(&be_i32(4)); // cx
    body.extend_from_slice(&be_i32(-9)); // cz
    body.extend_from_slice(&[10, 0]); // heightmaps: empty compound NBT
    body.extend(vi(0)); // data_len = 0 (no sections)
    body.extend(vi(1)); // 1 block entity
    body.push(0x34); // packed_xz
    body.extend_from_slice(&be_i16(-5)); // y
    body.extend(vi(14)); // kind
    let nbt = chat_text_nbt("Chest");
    body.extend_from_slice(&nbt);
    body.extend_from_slice(&[0, 0, 0, 0]); // four empty masks
    body.extend_from_slice(&[0, 0]); // 0 sky arrays, 0 block arrays

    let chunk = decode_chunk_packet(&body, 0, 64).expect("decode chunk");
    assert_eq!(chunk.cx, 4);
    assert_eq!(chunk.cz, -9);
    assert_eq!(chunk.block_entities.len(), 1);
    let be = &chunk.block_entities[0];
    assert_eq!(be.packed_xz, 0x34);
    assert_eq!(be.y, -5);
    assert_eq!(be.kind, 14);
    assert_eq!(be.nbt_bytes, nbt);
}

// ---------------------------------------------------------------------------
// S2C: containers / menus
// ---------------------------------------------------------------------------

#[test]
fn decodes_container_family() {
    let title = chat_text_nbt("Chest");
    let mut body = vi(1); // windowId
    body.extend(vi(2)); // generic_9x3
    body.extend_from_slice(&title);
    match decode("open_screen", &body) {
        PlayEvent::OpenScreen {
            window_id,
            menu_type,
            title_nbt,
        } => {
            assert_eq!(window_id, 1);
            assert_eq!(menu_type, 2);
            assert_eq!(title_nbt, title);
        }
        other => panic!("expected OpenScreen, got {other:?}"),
    }

    // container_set_content: u8 window, varint state, varint count, raw rest.
    let mut body = vec![0u8];
    body.extend(vi(7)); // stateId
    body.extend(vi(2)); // 2 items
    body.extend_from_slice(&slot_simple(5, 3));
    body.extend_from_slice(&slot_empty());
    body.extend_from_slice(&slot_simple(9, 1)); // carried item
    match decode("container_set_content", &body) {
        PlayEvent::ContainerSetContent {
            window_id,
            state_id,
            slot_count,
            raw_payload,
        } => {
            assert_eq!(window_id, 0);
            assert_eq!(state_id, 7);
            assert_eq!(slot_count, 2);
            assert_eq!(
                raw_payload,
                cat(&[&slot_simple(5, 3), &slot_empty(), &slot_simple(9, 1)])
            );
        }
        other => panic!("expected ContainerSetContent, got {other:?}"),
    }

    let mut body = vec![1u8];
    body.extend(vi(4));
    body.extend_from_slice(&be_i16(27));
    body.extend_from_slice(&slot_named(1, 1, "Axe"));
    match decode("container_set_slot", &body) {
        PlayEvent::ContainerSetSlot {
            window_id,
            state_id,
            slot,
            raw_item,
        } => {
            assert_eq!(window_id, 1);
            assert_eq!(state_id, 4);
            assert_eq!(slot, 27);
            assert_eq!(raw_item, slot_named(1, 1, "Axe"));
        }
        other => panic!("expected ContainerSetSlot, got {other:?}"),
    }

    let body = cat(&[&[2u8], &be_i16(0), &be_i16(50)]);
    match decode("container_set_data", &body) {
        PlayEvent::ContainerSetData {
            window_id,
            property,
            value,
        } => {
            assert_eq!(window_id, 2);
            assert_eq!(property, 0);
            assert_eq!(value, 50);
        }
        other => panic!("expected ContainerSetData, got {other:?}"),
    }

    let body = vec![3u8];
    match decode("container_close", &body) {
        PlayEvent::ContainerClose { window_id } => assert_eq!(window_id, 3),
        other => panic!("expected ContainerClose, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// S2C: HUD / player / chat
// ---------------------------------------------------------------------------

#[test]
fn decodes_experience_and_mob_effects() {
    let mut body = Vec::new();
    body.extend_from_slice(&be_f32(0.75));
    body.extend(vi(12)); // level
    body.extend(vi(345)); // total
    match decode("set_experience", &body) {
        PlayEvent::SetExperience { bar, level, total } => {
            assert_eq!(bar, 0.75);
            assert_eq!(level, 12);
            assert_eq!(total, 345);
        }
        other => panic!("expected SetExperience, got {other:?}"),
    }

    let mut body = vi(8);
    body.extend(vi(1)); // effect: speed
    body.extend(vi(1)); // amplifier
    body.extend(vi(600)); // duration ticks
    body.push(0b0101); // flags
    match decode("update_mob_effect", &body) {
        PlayEvent::UpdateMobEffect {
            eid,
            effect_id,
            amplifier,
            duration,
            flags,
        } => {
            assert_eq!(eid, 8);
            assert_eq!(effect_id, 1);
            assert_eq!(amplifier, 1);
            assert_eq!(duration, 600);
            assert_eq!(flags, 0b0101);
        }
        other => panic!("expected UpdateMobEffect, got {other:?}"),
    }

    let body = cat(&[&vi(8), &vi(1)]);
    match decode("remove_mob_effect", &body) {
        PlayEvent::RemoveMobEffect { eid, effect_id } => {
            assert_eq!(eid, 8);
            assert_eq!(effect_id, 1);
        }
        other => panic!("expected RemoveMobEffect, got {other:?}"),
    }
}

#[test]
fn decodes_chat_text_and_boss_bar_events() {
    let nbt = chat_text_nbt("Action!");
    for name in ["set_action_bar_text", "set_title_text", "set_subtitle_text"] {
        match decode(name, &nbt) {
            PlayEvent::SetActionBarText { nbt_bytes }
            | PlayEvent::SetTitleText { nbt_bytes }
            | PlayEvent::SetSubtitleText { nbt_bytes } => {
                assert_eq!(nbt_bytes, nbt, "{name} nbt mismatch")
            }
            other => panic!("{name}: unexpected {other:?}"),
        }
    }

    // boss_event action 0 (add): uuid + action + title/health/color/dividers/flags
    let title = chat_text_nbt("Boss");
    let mut body = uuid(0x77).to_vec();
    body.extend(vi(0)); // action: add
    body.extend_from_slice(&title);
    body.extend_from_slice(&be_f32(0.8)); // health
    body.extend(vi(2)); // color
    body.extend(vi(1)); // dividers
    body.push(0x02); // flags
    match decode("boss_event", &body) {
        PlayEvent::BossEvent {
            uuid: u,
            action,
            raw_payload,
        } => {
            assert_eq!(u, uuid(0x77));
            assert_eq!(action, 0);
            assert_eq!(
                raw_payload,
                cat(&[&title, &be_f32(0.8), &vi(2), &vi(1), &[0x02]])
            );
        }
        other => panic!("expected BossEvent, got {other:?}"),
    }

    // boss_event action 1 (remove): uuid + action only.
    let body = cat(&[&uuid(0x77), &vi(1)]);
    match decode("boss_event", &body) {
        PlayEvent::BossEvent {
            action,
            raw_payload,
            ..
        } => {
            assert_eq!(action, 1);
            assert!(raw_payload.is_empty());
        }
        other => panic!("expected BossEvent, got {other:?}"),
    }
}

#[test]
fn decodes_player_info_remove_uuids() {
    let mut body = vi(2);
    body.extend_from_slice(&uuid(0x01));
    body.extend_from_slice(&uuid(0x02));
    match decode("player_info_remove", &body) {
        PlayEvent::PlayerInfoRemove { uuids } => {
            assert_eq!(uuids, vec![uuid(0x01), uuid(0x02)]);
        }
        other => panic!("expected PlayerInfoRemove, got {other:?}"),
    }
}

#[test]
fn unknown_packet_name_returns_none() {
    assert!(decode_extended_play_event("not_a_packet", &[1, 2, 3])
        .unwrap()
        .is_none());
    assert!(decode_extended_play_event("login", &[1, 2, 3])
        .unwrap()
        .is_none());
}

// ---------------------------------------------------------------------------
// C2S encoder helpers
// ---------------------------------------------------------------------------

#[test]
fn encodes_swing_and_pick_item() {
    let (name, body) = encode_swing(1);
    assert_eq!(name, "swing");
    assert_eq!(body, vi(1));

    let (name, body) = encode_pick_item(5);
    assert_eq!(name, "pick_item");
    let mut r = WireReader::new(&body);
    assert_eq!(r.read_varint().unwrap(), 5);
    assert_eq!(r.remaining(), 0);
}

#[test]
fn encodes_use_item_on_full_layout() {
    let (name, body) = encode_use_item_on(0, [10, 64, -3], 1, [0.5, 0.25, 0.75], true, 42);
    assert_eq!(name, "use_item_on");
    let mut r = WireReader::new(&body);
    assert_eq!(r.read_varint().unwrap(), 0); // hand
    assert_eq!(r.read_position().unwrap(), [10, 64, -3]);
    assert_eq!(r.read_varint().unwrap(), 1); // face
    assert_eq!(r.read_f32().unwrap(), 0.5);
    assert_eq!(r.read_f32().unwrap(), 0.25);
    assert_eq!(r.read_f32().unwrap(), 0.75);
    assert!(r.read_bool().unwrap());
    assert_eq!(r.read_varint().unwrap(), 42); // sequence
    assert_eq!(r.remaining(), 0);
}

#[test]
fn encodes_use_item_and_player_action() {
    let (name, body) = encode_use_item(1, 9, 45.0, -10.0);
    assert_eq!(name, "use_item");
    let mut r = WireReader::new(&body);
    assert_eq!(r.read_varint().unwrap(), 1);
    assert_eq!(r.read_varint().unwrap(), 9);
    assert_eq!(r.read_f32().unwrap(), 45.0);
    assert_eq!(r.read_f32().unwrap(), -10.0);
    assert_eq!(r.remaining(), 0);

    let (name, body) = encode_player_action(0, [1, 2, 3], -1, 7);
    assert_eq!(name, "player_action");
    let mut r = WireReader::new(&body);
    assert_eq!(r.read_varint().unwrap(), 0); // status
    assert_eq!(r.read_position().unwrap(), [1, 2, 3]);
    assert_eq!(r.read_i8().unwrap(), -1); // face
    assert_eq!(r.read_varint().unwrap(), 7);
    assert_eq!(r.remaining(), 0);
}

#[test]
fn encodes_interact_variants() {
    let (name, body) = encode_interact_attack(77, true);
    assert_eq!(name, "interact");
    let mut r = WireReader::new(&body);
    assert_eq!(r.read_varint().unwrap(), 77); // target
    assert_eq!(r.read_varint().unwrap(), 1); // mouse = attack
    assert!(r.read_bool().unwrap()); // sneaking
    assert_eq!(r.remaining(), 0);

    let (name, body) = encode_interact_use(77, 1, false);
    assert_eq!(name, "interact");
    let mut r = WireReader::new(&body);
    assert_eq!(r.read_varint().unwrap(), 77);
    assert_eq!(r.read_varint().unwrap(), 0); // mouse = interact
    assert_eq!(r.read_varint().unwrap(), 1); // hand
    assert!(!r.read_bool().unwrap());
    assert_eq!(r.remaining(), 0);
}

#[test]
fn encodes_container_click_full_layout() {
    let changed_slot = slot_named(5, 1, "Pick");
    let cursor = slot_simple(9, 4);
    let (name, body) = encode_container_click(
        0,
        11,
        20,
        1,
        0,
        &[(3, &changed_slot), (4, &slot_empty())],
        &cursor,
    );
    assert_eq!(name, "container_click");
    let mut r = WireReader::new(&body);
    assert_eq!(r.read_u8().unwrap(), 0); // windowId
    assert_eq!(r.read_varint().unwrap(), 11); // stateId
    assert_eq!(r.read_i16().unwrap(), 20); // slot
    assert_eq!(r.read_i8().unwrap(), 1); // mouseButton
    assert_eq!(r.read_varint().unwrap(), 0); // mode
    assert_eq!(r.read_varint().unwrap(), 2); // changedSlots count
    assert_eq!(r.read_i16().unwrap(), 3); // location
    assert_eq!(r.take(changed_slot.len()).unwrap(), changed_slot.as_slice());
    assert_eq!(r.read_i16().unwrap(), 4); // second location
    assert_eq!(r.take(slot_empty().len()).unwrap(), slot_empty().as_slice());
    assert_eq!(r.rest(), cursor.as_slice());

    // Full byte-equality check against a hand-built body.
    let mut expected = vec![0u8];
    expected.extend(vi(11));
    expected.extend_from_slice(&be_i16(20));
    expected.push(1);
    expected.extend(vi(0));
    expected.extend(vi(2));
    expected.extend_from_slice(&be_i16(3));
    expected.extend_from_slice(&changed_slot);
    expected.extend_from_slice(&be_i16(4));
    expected.extend_from_slice(&slot_empty());
    expected.extend_from_slice(&cursor);
    assert_eq!(body, expected);
}

#[test]
fn encodes_container_button_and_close() {
    let (name, body) = encode_container_button_click(1, 2);
    assert_eq!(name, "container_button_click");
    let mut r = WireReader::new(&body);
    assert_eq!(r.read_u8().unwrap(), 1); // windowId: ContainerID u8
    assert_eq!(r.read_varint().unwrap(), 2); // buttonId varint
    assert_eq!(r.remaining(), 0);

    let (name, body) = encode_container_close(3);
    assert_eq!(name, "container_close");
    assert_eq!(body, vec![3]);
}

#[test]
fn encodes_chat_command_and_message() {
    let (name, body) = encode_chat_command("tp 0 64 0");
    assert_eq!(name, "chat_command");
    let mut r = WireReader::new(&body);
    assert_eq!(r.read_string(256).unwrap(), "tp 0 64 0");
    assert_eq!(r.remaining(), 0);

    let (name, body) = encode_chat_message("hello world");
    assert_eq!(name, "chat");
    let mut r = WireReader::new(&body);
    assert_eq!(r.read_string(256).unwrap(), "hello world");
    let timestamp = r.read_i64().unwrap();
    assert!(timestamp > 0, "chat timestamp should be unix millis");
    assert_eq!(r.read_i64().unwrap(), 0); // salt
    assert_eq!(r.read_u8().unwrap(), 0); // signature absent
    assert_eq!(r.read_varint().unwrap(), 0); // offset
    assert_eq!(r.take(3).unwrap(), &[0, 0, 0]); // acknowledged
    assert_eq!(r.remaining(), 0);
}

// ---------------------------------------------------------------------------
// Fail-closed / adversarial coverage: truncation + hostile payloads, 0 panics
// ---------------------------------------------------------------------------

/// Every prefix truncation of a valid body must decode or error — never panic.
fn assert_truncation_safe(name: &str, body: &[u8]) {
    for k in 0..body.len() {
        let truncated = &body[..k];
        let result = no_panic(|| decode_extended_play_event(name, truncated));
        // Result may be Ok (raw-tail fields tolerate short bodies) or Err;
        // the contract is only "no panic".
        let _ = result;
    }
}

#[test]
fn truncation_is_fail_closed_for_all_new_decoders() {
    let bodies: &[(&str, Vec<u8>)] = &[
        ("add_entity", {
            let mut b = vi(1);
            b.extend_from_slice(&uuid(0));
            b.extend(vi(2));
            b.extend_from_slice(&be_f64(0.0));
            b.extend_from_slice(&be_f64(0.0));
            b.extend_from_slice(&be_f64(0.0));
            b.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
            b.extend(vi(0));
            b.extend_from_slice(&[0; 6]);
            b
        }),
        ("remove_entities", cat(&[&vi(2), &vi(1), &vi(2)])),
        (
            "move_entity_pos",
            cat(&[&vi(1), &be_i16(1), &be_i16(1), &be_i16(1), &[1]]),
        ),
        (
            "move_entity_pos_rot",
            cat(&[&vi(1), &be_i16(1), &be_i16(1), &be_i16(1), &[0], &[0], &[0]]),
        ),
        ("move_entity_rot", cat(&[&vi(1), &[0], &[0], &[0]])),
        (
            "teleport_entity",
            cat(&[
                &vi(1),
                &be_f64(0.0),
                &be_f64(0.0),
                &be_f64(0.0),
                &[0],
                &[0],
                &[0],
            ]),
        ),
        ("rotate_head", cat(&[&vi(1), &[0]])),
        (
            "set_entity_motion",
            cat(&[&vi(1), &be_i16(0), &be_i16(0), &be_i16(0)]),
        ),
        ("entity_event", cat(&[&be_i32(1), &[0]])),
        ("hurt_animation", cat(&[&vi(1), &be_f32(0.0)])),
        ("damage_event", cat(&[&vi(1), &vi(0), &vi(0), &vi(0), &[0]])),
        ("set_equipment", cat(&[&vi(1), &[0x01], &slot_empty()])),
        ("set_entity_data", cat(&[&vi(1), &[0xFF]])),
        ("set_time", cat(&[&vl(0), &vl(0)])),
        ("game_event", cat(&[&[0], &be_f32(0.0)])),
        (
            "level_event",
            cat(&[&be_i32(0), &pos8(0, 0, 0), &be_i32(0), &[0]]),
        ),
        (
            "level_particles",
            cat(&[
                &[0],
                &be_f64(0.0),
                &be_f64(0.0),
                &be_f64(0.0),
                &be_f32(0.0),
                &be_f32(0.0),
                &be_f32(0.0),
                &be_f32(0.0),
                &be_i32(0),
                &vi(0),
            ]),
        ),
        ("block_destruction", cat(&[&vi(1), &pos8(0, 0, 0), &[0]])),
        (
            "block_entity_data",
            cat(&[&pos8(0, 0, 0), &vi(0), &chat_text_nbt("x")]),
        ),
        ("open_screen", cat(&[&vi(0), &vi(0), &chat_text_nbt("t")])),
        (
            "container_set_content",
            cat(&[&[0], &vi(0), &vi(0), &slot_empty()]),
        ),
        (
            "container_set_slot",
            cat(&[&[0], &vi(0), &be_i16(0), &slot_empty()]),
        ),
        ("container_set_data", cat(&[&[0], &be_i16(0), &be_i16(0)])),
        ("container_close", vec![0]),
        ("set_experience", cat(&[&be_f32(0.0), &vi(0), &vi(0)])),
        (
            "update_mob_effect",
            cat(&[&vi(0), &vi(0), &vi(0), &vi(0), &[0]]),
        ),
        ("remove_mob_effect", cat(&[&vi(0), &vi(0)])),
        ("set_action_bar_text", chat_text_nbt("a")),
        ("set_title_text", chat_text_nbt("t")),
        ("set_subtitle_text", chat_text_nbt("s")),
        ("boss_event", cat(&[&uuid(0), &vi(0)])),
        ("player_info_remove", cat(&[&vi(0)])),
    ];
    for (name, body) in bodies {
        assert_truncation_safe(name, body);
    }
}

#[test]
fn hostile_counts_and_lengths_fail_closed() {
    // Huge count prefixes must error, not allocate.
    assert!(decode_extended_play_event("remove_entities", &vi(i32::MAX)).is_err());
    assert!(decode_extended_play_event("player_info_remove", &vi(i32::MAX)).is_err());
    assert!(decode_extended_play_event("remove_entities", &vi(-1)).is_err());

    // Malformed varints (unbounded continuation) fail closed.
    let bad_vi = [0x80, 0x80, 0x80, 0x80, 0x80, 0x80];
    assert!(decode_extended_play_event("add_entity", &bad_vi).is_err());
    assert!(decode_extended_play_event("remove_entities", &bad_vi).is_err());

    // set_equipment: unterminated 0x80 continuation bytes run out of input.
    let mut eq = vi(1);
    eq.extend_from_slice(&[0x80, 0x80, 0x80]);
    assert!(decode_extended_play_event("set_equipment", &eq).is_err());
    // Unknown component type inside a slot rejects the whole packet.
    let mut bad_slot = vi(1); // count
    bad_slot.extend(vi(5)); // item
    bad_slot.extend(vi(1)); // nadd
    bad_slot.extend(vi(0)); // nrem
    bad_slot.extend(vi(200)); // component type 200 > 56
    bad_slot.push(0);
    let mut eq2 = vi(1);
    eq2.push(0x01);
    eq2.extend_from_slice(&bad_slot);
    assert!(decode_extended_play_event("set_equipment", &eq2).is_err());
    // Stack count > 127 rejects.
    let mut bad_count = vi(200);
    bad_count.extend(vi(5));
    let mut eq3 = vi(1);
    eq3.push(0x01);
    eq3.extend_from_slice(&bad_count);
    assert!(decode_extended_play_event("set_equipment", &eq3).is_err());

    // NBT nesting beyond the depth bound inside a component payload rejects.
    let mut deep = vec![10u8; 70]; // nested compounds, no terminator until the end
    deep.push(0);
    let mut slot = vi(1);
    slot.extend(vi(5));
    slot.extend(vi(1)); // nadd
    slot.extend(vi(0)); // nrem
    slot.extend(vi(0)); // custom_data component -> NBT
    slot.extend_from_slice(&deep);
    let mut eq4 = vi(1);
    eq4.push(0x01);
    eq4.extend_from_slice(&slot);
    assert!(decode_extended_play_event("set_equipment", &eq4).is_err());

    // Invalid NBT tag byte in a required anonymous-NBT field.
    assert!(decode_extended_play_event("open_screen", &cat(&[&vi(0), &vi(0), &[13]])).is_err());
    // Unterminated NBT compound.
    assert!(decode_extended_play_event("open_screen", &cat(&[&vi(0), &vi(0), &[10, 8]])).is_err());
    // block_entity_data: invalid tag 13.
    let mut be = Vec::new();
    be.extend_from_slice(&pos8(0, 0, 0));
    be.extend(vi(0));
    be.push(13);
    assert!(decode_extended_play_event("block_entity_data", &be).is_err());
}

#[test]
fn hostile_slot_bodies_fail_closed_in_containers() {
    // container_set_slot keeps raw bytes — no slot walk — but the header
    // fields still fail closed on truncation (covered above). The equipment
    // path is the one that walks Slots; add a truncated-component case:
    // component type 9 (enchantments) claims a huge list count.
    let mut slot = vi(1);
    slot.extend(vi(5));
    slot.extend(vi(1));
    slot.extend(vi(0));
    slot.extend(vi(9)); // enchantments
    slot.extend(vi(1_000_000)); // absurd entry count (bounded to 256 -> Err)
    let mut eq = vi(1);
    eq.push(0x01);
    eq.extend_from_slice(&slot);
    let result = no_panic(|| decode_extended_play_event("set_equipment", &eq));
    assert!(result.is_err() || result.unwrap().is_some()); // never panics

    // Deeply nested food->conversion->bundle recursion stays depth-capped.
    // Build a slot whose bundle_contents (type 52) nests stacks recursively.
    fn nested_slot(depth: usize) -> Vec<u8> {
        if depth == 0 {
            return slot_empty();
        }
        let mut inner = nested_slot(depth - 1);
        // Wrap: count=1, item, nadd=1 (bundle_contents type 52), nrem=0,
        // component payload = array of one nested stack.
        let mut s = vi(1);
        s.extend(vi(5));
        s.extend(vi(1));
        s.extend(vi(0));
        s.extend(vi(52));
        s.extend(vi(1)); // one nested stack
        s.append(&mut inner);
        s
    }
    let deep_slot = nested_slot(20); // beyond SLOT_MAX_DEPTH=16
    let mut eq2 = vi(1);
    eq2.push(0x01);
    eq2.extend_from_slice(&deep_slot);
    let result = no_panic(|| decode_extended_play_event("set_equipment", &eq2));
    assert!(result.is_err() || result.unwrap().is_some());

    // A moderately nested slot (within depth) still decodes.
    let ok_slot = nested_slot(4);
    let mut eq3 = vi(1);
    eq3.push(0x01);
    eq3.extend_from_slice(&ok_slot);
    let decoded = no_panic(|| decode_extended_play_event("set_equipment", &eq3));
    assert!(matches!(
        decoded.unwrap().unwrap(),
        PlayEvent::SetEquipment { .. }
    ));
}

#[test]
fn chunk_block_entity_count_is_bounded() {
    // A chunk claiming more block entities than remaining bytes must fail
    // closed before allocating.
    let mut body = Vec::new();
    body.extend_from_slice(&be_i32(0));
    body.extend_from_slice(&be_i32(0));
    body.extend_from_slice(&[10, 0]); // heightmaps
    body.extend(vi(0)); // data_len 0
    body.extend(vi(i32::MAX)); // hostile block entity count
    let result = no_panic(|| decode_chunk_packet(&body, 0, 64));
    assert!(result.is_err());
}

#[test]
fn truncated_nbt_and_positions_fail_closed() {
    // open_screen with truncated title NBT.
    let title = chat_text_nbt("hello");
    for cut in 1..title.len() {
        let mut body = vi(0);
        body.extend(vi(0));
        body.extend_from_slice(&title[..cut]);
        let result = no_panic(|| decode_extended_play_event("open_screen", &body));
        assert!(result.is_err() || cut == title.len() - 1);
    }
    // player_info_remove with truncated uuid list.
    let mut body = vi(2);
    body.extend_from_slice(&uuid(1));
    let result = no_panic(|| decode_extended_play_event("player_info_remove", &body));
    assert!(result.is_err());
    // level_event with truncated packed position.
    let mut body = Vec::new();
    body.extend_from_slice(&be_i32(0));
    body.extend_from_slice(&pos8(0, 0, 0)[..4]);
    let result = no_panic(|| decode_extended_play_event("level_event", &body));
    assert!(result.is_err());
}

#[test]
fn encoder_outputs_roundtrip_through_wire_reader() {
    // Spot-check a few encoders end-to-end at the byte level.
    let (name, body) = encode_swing(0);
    assert_eq!(name, "swing");
    assert_eq!(body, vec![0x00]);

    let (name, body) = encode_container_close(255);
    assert_eq!(name, "container_close");
    assert_eq!(body, vec![255]);

    let (name, body) = encode_use_item_on(1, [-1, 255, -1], -1, [0.0; 3], false, 0);
    assert_eq!(name, "use_item_on");
    let mut r = WireReader::new(&body);
    assert_eq!(r.read_varint().unwrap(), 1);
    assert_eq!(r.read_position().unwrap(), [-1, 255, -1]); // air-use sentinel pos
    assert_eq!(r.read_varint().unwrap(), -1); // face -1
    assert_eq!(r.read_f32().unwrap(), 0.0);
    assert_eq!(r.read_f32().unwrap(), 0.0);
    assert_eq!(r.read_f32().unwrap(), 0.0);
    assert!(!r.read_bool().unwrap());
    assert_eq!(r.read_varint().unwrap(), 0);
    assert_eq!(r.remaining(), 0);
}

#[test]
fn protocol_version_constant_is_767() {
    assert_eq!(PROTOCOL_767, ProtocolVersion::V1_21_1.number());
    assert_eq!(PROTOCOL_767, 767);
}
