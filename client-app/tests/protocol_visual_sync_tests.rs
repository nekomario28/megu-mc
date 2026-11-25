// SPDX-License-Identifier: LGPL-3.0-or-later
//! Protocol 767 visual-state decode and world-cache update regressions.

use megu_client::protocol::DimensionMeta;
use megu_client::protocol::{
    decode_boss_bar_action, decode_entity_metadata, decode_extended_play_event,
    decode_particle_options, pack_position, write_varint, DecodedBossBarAction,
    DecodedEntityMetadataValue, DecodedParticleOptions, PlayEvent, VibrationSource,
};
use megu_client::protocol::{visual_level_event, VisualLevelEvent};
use megu_client::render::project_render_snapshot;
use megu_client::world::{BlockTable, WorldState};

fn vi(value: i32) -> Vec<u8> {
    let mut out = Vec::new();
    write_varint(&mut out, value);
    out
}

fn f32be(value: f32) -> [u8; 4] {
    value.to_be_bytes()
}

fn test_world() -> WorldState {
    WorldState::new(DimensionMeta::default(), BlockTable::builtin())
}

fn spawn(world: &mut WorldState, eid: i32, entity_type: i32) {
    world
        .apply_play_event(&PlayEvent::AddEntity {
            eid,
            uuid: [eid as u8; 16],
            entity_type,
            x: f64::from(eid),
            y: 64.0,
            z: -2.0,
            pitch: 0.0,
            yaw: 90.0,
            head_yaw: 90.0,
            data: 0,
            vx: 0.0,
            vy: 0.0,
            vz: 0.0,
        })
        .unwrap();
}

fn metadata(index: u8, kind: u8, payload: &[u8], out: &mut Vec<u8>) {
    out.push(index);
    out.extend(vi(i32::from(kind)));
    out.extend_from_slice(payload);
}

fn nbt_string(name: &str, value: &str) -> Vec<u8> {
    let mut out = vec![8];
    out.extend_from_slice(&(name.len() as u16).to_be_bytes());
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(&(value.len() as u16).to_be_bytes());
    out.extend_from_slice(value.as_bytes());
    out
}

fn nbt_int(name: &str, value: i32) -> Vec<u8> {
    let mut out = vec![3];
    out.extend_from_slice(&(name.len() as u16).to_be_bytes());
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(&value.to_be_bytes());
    out
}

fn nbt_empty_list(name: &str) -> Vec<u8> {
    let mut out = vec![9];
    out.extend_from_slice(&(name.len() as u16).to_be_bytes());
    out.extend_from_slice(name.as_bytes());
    out.extend([0, 0, 0, 0, 0]);
    out
}

fn nbt_string_list(name: &str, values: &[&str]) -> Vec<u8> {
    let mut out = vec![9];
    out.extend_from_slice(&(name.len() as u16).to_be_bytes());
    out.extend_from_slice(name.as_bytes());
    out.push(8);
    out.extend_from_slice(&(values.len() as i32).to_be_bytes());
    for value in values {
        out.extend_from_slice(&(value.len() as u16).to_be_bytes());
        out.extend_from_slice(value.as_bytes());
    }
    out
}

fn nbt_named_compound(name: &str, fields: Vec<Vec<u8>>) -> Vec<u8> {
    let mut out = vec![10];
    out.extend_from_slice(&(name.len() as u16).to_be_bytes());
    out.extend_from_slice(name.as_bytes());
    for field in fields {
        out.extend(field);
    }
    out.push(0);
    out
}

fn nbt_compound(fields: Vec<Vec<u8>>) -> Vec<u8> {
    let mut out = vec![10];
    for field in fields {
        out.extend(field);
    }
    out.push(0);
    out
}

#[test]
fn typed_entity_visual_accessors_cover_shared_and_mob_metadata() {
    let mut world = test_world();
    for (eid, entity_type) in [
        (1, 87),
        (2, 87),
        (3, 23),
        (4, 113),
        (5, 88),
        (6, 119),
        (7, 116),
        (8, 33),
    ] {
        spawn(&mut world, eid, entity_type);
    }

    let mut shared = Vec::new();
    metadata(0, 0, &[0x7b], &mut shared); // all six shared visual flags
    metadata(6, 20, &vi(5), &mut shared);
    metadata(9, 3, &f32be(17.5), &mut shared);
    metadata(16, 8, &[1], &mut shared); // ageable baby flag
    shared.push(0xff);
    world
        .apply_play_event(&PlayEvent::SetEntityData {
            eid: 1,
            raw_entries: shared,
        })
        .unwrap();
    world
        .apply_play_event(&PlayEvent::Animate {
            eid: 1,
            animation: 0,
        })
        .unwrap();
    world
        .apply_play_event(&PlayEvent::HurtAnimation { eid: 1, yaw: 0.0 })
        .unwrap();
    let visual = world.entity_visual_state(1).unwrap();
    assert!(
        visual.on_fire
            && visual.crouching
            && visual.sprinting
            && visual.swimming
            && visual.invisible
            && visual.glowing
    );
    assert!(visual.is_baby);
    assert_eq!(
        (
            visual.health,
            visual.pose,
            visual.hurt_ticks,
            visual.swing_ticks
        ),
        (Some(17.5), Some(5), 10, 6)
    );

    let mut sheep = Vec::new();
    metadata(17, 0, &[0x13], &mut sheep);
    sheep.push(0xff);
    world
        .apply_play_event(&PlayEvent::SetEntityData {
            eid: 2,
            raw_entries: sheep,
        })
        .unwrap();
    assert_eq!(
        (
            world.entity_visual_state(2).unwrap().sheep_color,
            world.entity_visual_state(2).unwrap().sheep_sheared
        ),
        (Some(3), Some(true))
    );

    let mut creeper = Vec::new();
    metadata(16, 1, &vi(2), &mut creeper);
    metadata(17, 8, &[1], &mut creeper);
    creeper.push(0xff);
    world
        .apply_play_event(&PlayEvent::SetEntityData {
            eid: 3,
            raw_entries: creeper,
        })
        .unwrap();
    let visual = world.entity_visual_state(3).unwrap();
    assert_eq!(
        (visual.creeper_state, visual.creeper_charged),
        (Some(2), Some(true))
    );

    let mut villager = vec![17];
    villager.extend(vi(18));
    villager.extend(vi(2));
    villager.extend(vi(5));
    villager.extend(vi(3));
    villager.push(0xff);
    world
        .apply_play_event(&PlayEvent::SetEntityData {
            eid: 4,
            raw_entries: villager,
        })
        .unwrap();
    assert_eq!(
        world.entity_visual_state(4).unwrap().villager_data,
        Some((2, 5, 3))
    );

    let mut shulker = Vec::new();
    metadata(18, 0, &[7], &mut shulker);
    shulker.push(0xff);
    world
        .apply_play_event(&PlayEvent::SetEntityData {
            eid: 5,
            raw_entries: shulker,
        })
        .unwrap();
    assert_eq!(world.entity_visual_state(5).unwrap().shulker_peek, Some(7));
    for (eid, index, expected) in [(6, 16, 20), (7, 16, 90)] {
        let mut raw = Vec::new();
        metadata(index, 1, &vi(expected), &mut raw);
        raw.push(0xff);
        world
            .apply_play_event(&PlayEvent::SetEntityData {
                eid,
                raw_entries: raw,
            })
            .unwrap();
    }
    assert_eq!(
        world.entity_visual_state(6).unwrap().wither_invul_ticks,
        Some(20)
    );
    assert_eq!(world.entity_visual_state(7).unwrap().warden_anger, Some(90));
    let mut enderman = Vec::new();
    let mut carried = vec![1];
    carried.extend(vi(123));
    metadata(16, 15, &carried, &mut enderman);
    enderman.push(0xff);
    world
        .apply_play_event(&PlayEvent::SetEntityData {
            eid: 8,
            raw_entries: enderman,
        })
        .unwrap();
    assert_eq!(
        world.entity_visual_state(8).unwrap().enderman_carried_block,
        Some(123)
    );

    spawn(&mut world, 9, 58);
    let mut item = Vec::new();
    metadata(8, 7, &[0], &mut item);
    item.push(0xff);
    world
        .apply_play_event(&PlayEvent::SetEntityData {
            eid: 9,
            raw_entries: item,
        })
        .unwrap();
    assert_eq!(
        world.entity_visual_state(9).unwrap().dropped_item_stack,
        Some(vec![0])
    );
    spawn(&mut world, 10, 1); // area-effect cloud also uses entity-specific metadata indexes
    let mut unrelated = Vec::new();
    metadata(16, 1, &vi(2), &mut unrelated);
    metadata(17, 8, &[1], &mut unrelated);
    unrelated.push(0xff);
    world
        .apply_play_event(&PlayEvent::SetEntityData {
            eid: 10,
            raw_entries: unrelated,
        })
        .unwrap();
    let visual = world.entity_visual_state(10).unwrap();
    assert_eq!((visual.creeper_state, visual.creeper_charged), (None, None));
}

#[test]
fn block_entity_visual_state_uses_nbt_and_block_event_updates() {
    let mut world = test_world();
    let pos = [4, 70, -3];
    let raw = nbt_compound(vec![nbt_string("id", "minecraft:trapped_chest")]);
    world
        .apply_play_event(&PlayEvent::BlockEntityData {
            pos,
            kind: 1,
            nbt_bytes: raw,
        })
        .unwrap();
    assert_eq!(
        world.block_entity_visual_state(pos),
        Some(megu_client::world::BlockEntityVisualState::Chest {
            open_count: 0,
            trapped: true,
            ender: false
        })
    );
    let event_pos = megu_client::protocol::pack_position(pos)
        .to_be_bytes()
        .to_vec();
    let mut body = event_pos;
    body.extend([1, 3]);
    body.extend(vi(0));
    let event = decode_extended_play_event("block_event", &body)
        .unwrap()
        .unwrap();
    world.apply_play_event(&event).unwrap();
    assert_eq!(
        world.block_entity_visual_state(pos),
        Some(megu_client::world::BlockEntityVisualState::Chest {
            open_count: 3,
            trapped: true,
            ender: false
        })
    );
    let ender_pos = [6, 70, -3];
    world
        .apply_play_event(&PlayEvent::BlockEntityData {
            pos: ender_pos,
            kind: 1,
            nbt_bytes: nbt_compound(vec![nbt_string("id", "minecraft:ender_chest")]),
        })
        .unwrap();
    assert_eq!(
        world.block_entity_visual_state(ender_pos),
        Some(megu_client::world::BlockEntityVisualState::Chest {
            open_count: 0,
            trapped: false,
            ender: true,
        })
    );

    let sign_pos = [5, 70, -3];
    let sign = nbt_compound(vec![
        nbt_string("id", "minecraft:sign"),
        nbt_named_compound(
            "front_text",
            vec![nbt_string_list(
                "messages",
                &["line one", "line two", "", ""],
            )],
        ),
        nbt_named_compound(
            "back_text",
            vec![nbt_string_list("messages", &["back", "", "", ""])],
        ),
        vec![
            1, 0, 15, 105, 115, 95, 103, 108, 111, 119, 105, 110, 103, 95, 116, 101, 120, 116, 1,
        ],
    ]);
    world
        .apply_play_event(&PlayEvent::BlockEntityData {
            pos: sign_pos,
            kind: 2,
            nbt_bytes: sign,
        })
        .unwrap();
    assert_eq!(
        world.block_entity_visual_state(sign_pos),
        Some(megu_client::world::BlockEntityVisualState::Sign {
            front_lines: [
                "line one".into(),
                "line two".into(),
                String::new(),
                String::new()
            ],
            back_lines: ["back".into(), String::new(), String::new(), String::new()],
            glowing: true,
        })
    );

    let cases = [
        (
            [10, 70, 0],
            "minecraft:shulker_box",
            vec![nbt_int("Color", 4), nbt_int("Peek", 9)],
            "shulker",
        ),
        (
            [11, 70, 0],
            "minecraft:bell",
            vec![nbt_int("ticks", 8)],
            "bell",
        ),
        (
            [12, 70, 0],
            "minecraft:banner",
            vec![nbt_int("Base", 5), nbt_empty_list("Patterns")],
            "banner",
        ),
        (
            [13, 70, 0],
            "minecraft:beacon",
            vec![
                nbt_int("Levels", 4),
                nbt_int("Primary", 1),
                nbt_int("Secondary", 2),
            ],
            "beacon",
        ),
        ([14, 70, 0], "minecraft:enchanting_table", vec![], "enchant"),
        (
            [15, 70, 0],
            "minecraft:campfire",
            vec![nbt_empty_list("Items")],
            "campfire",
        ),
        (
            [16, 70, 0],
            "minecraft:trial_spawner",
            vec![nbt_int("ominous", 1), nbt_string("state", "active")],
            "trial",
        ),
        (
            [17, 70, 0],
            "minecraft:vault",
            vec![nbt_int("ominous", 1), nbt_string("state", "EJECTING")],
            "vault",
        ),
    ];
    for (pos, id, mut fields, _) in cases {
        fields.push(nbt_string("id", id));
        world
            .apply_play_event(&PlayEvent::BlockEntityData {
                pos,
                kind: 1,
                nbt_bytes: nbt_compound(fields),
            })
            .unwrap();
    }
    use megu_client::world::BlockEntityVisualState as B;
    assert_eq!(
        world.block_entity_visual_state([10, 70, 0]),
        Some(B::ShulkerBox {
            open_count: 9,
            color: 4
        })
    );
    assert_eq!(
        world.block_entity_visual_state([11, 70, 0]),
        Some(B::Bell {
            ring_dir: 0,
            ringing_ticks: 8
        })
    );
    assert_eq!(
        world.block_entity_visual_state([12, 70, 0]),
        Some(B::Banner {
            base_color: 5,
            pattern_count: 0
        })
    );
    assert_eq!(
        world.block_entity_visual_state([13, 70, 0]),
        Some(B::Beacon {
            level: 4,
            primary_effect: 1,
            secondary_effect: 2
        })
    );
    assert_eq!(
        world.block_entity_visual_state([14, 70, 0]),
        Some(B::EnchantingTable { book_open: false })
    );
    assert_eq!(
        world.block_entity_visual_state([15, 70, 0]),
        Some(B::Campfire { item_count: 0 })
    );
    assert_eq!(
        world.block_entity_visual_state([16, 70, 0]),
        Some(B::TrialSpawner {
            ominous: true,
            active: true
        })
    );
    assert_eq!(
        world.block_entity_visual_state([17, 70, 0]),
        Some(B::Vault {
            ominous: true,
            state: 3
        })
    );

    let blocks = BlockTable::from_json_bytes(
        br#"{"blocks":[["minecraft:piston",0,1]],"flags":[0],"full_sides":[0],"shapes":[{}],"collision_shapes":[[]]}"#,
    )
    .unwrap();
    let mut piston_world = WorldState::new(DimensionMeta::default(), blocks);
    let piston_pos = [18, 70, 0];
    piston_world
        .apply_play_event(&PlayEvent::BlockEvent {
            pos: piston_pos,
            action: 0,
            parameter: 2,
            block_type: 0,
        })
        .unwrap();
    assert_eq!(
        piston_world.block_entity_visual_state(piston_pos),
        Some(B::Piston {
            extending: true,
            facing: 2
        })
    );
    let generic_pos = [19, 70, 0];
    piston_world
        .apply_play_event(&PlayEvent::BlockEntityData {
            pos: generic_pos,
            kind: 1,
            nbt_bytes: nbt_compound(vec![nbt_string("id", "minecraft:unknown")]),
        })
        .unwrap();
    assert_eq!(
        piston_world.block_entity_visual_state(generic_pos),
        Some(B::Generic)
    );
}

#[test]
fn visual_level_event_mapping_and_render_snapshot_projection() {
    assert_eq!(
        visual_level_event(2001, 42),
        VisualLevelEvent::BlockDestroy { block_state: 42 }
    );
    assert_eq!(
        visual_level_event(2002, 0x123456),
        VisualLevelEvent::SplashPotion {
            rgb: 0x123456,
            instant: false
        }
    );
    assert_eq!(
        visual_level_event(2007, 0x123456),
        VisualLevelEvent::SplashPotion {
            rgb: 0x123456,
            instant: true
        }
    );
    assert_eq!(
        visual_level_event(2005, 6),
        VisualLevelEvent::Bonemeal { count: 6 }
    );
    assert_eq!(visual_level_event(1028, 0), VisualLevelEvent::DragonDeath);
    assert_eq!(visual_level_event(1023, 0), VisualLevelEvent::WitherSpawn);
    assert_eq!(visual_level_event(1038, 0), VisualLevelEvent::EndPortalOpen);
    assert_eq!(visual_level_event(1032, 0), VisualLevelEvent::PortalTravel);
    assert_eq!(
        visual_level_event(777, 12),
        VisualLevelEvent::Other {
            event: 777,
            data: 12
        }
    );

    let mut world = test_world();
    spawn(&mut world, 42, 3);
    world
        .apply_play_event(&PlayEvent::LevelEvent {
            event: 2001,
            pos: [1, 2, 3],
            data: 42,
            disable_relative_volume: true,
        })
        .unwrap();
    world
        .apply_play_event(&PlayEvent::LevelParticles {
            particle_id: 36,
            long_distance: false,
            x: 1.0,
            y: 2.0,
            z: 3.0,
            offset_x: 0.0,
            offset_y: 0.0,
            offset_z: 0.0,
            max_speed: 1.0,
            count: 1,
            particle_data: f32be(0.5).to_vec(),
        })
        .unwrap();
    world
        .apply_play_event(&PlayEvent::SetTime {
            world_age: 99,
            time_of_day: 18000,
        })
        .unwrap();
    world
        .apply_play_event(&PlayEvent::BlockEntityData {
            pos: [4, 70, 0],
            kind: 1,
            nbt_bytes: nbt_compound(vec![nbt_string("id", "minecraft:chest")]),
        })
        .unwrap();
    world
        .apply_play_event(&PlayEvent::GameEvent {
            event: 8,
            value: 0.75,
        })
        .unwrap();
    world
        .apply_play_event(&PlayEvent::GameEvent {
            event: 7,
            value: 0.25,
        })
        .unwrap();
    world.boss_bars.insert(
        [7; 16],
        megu_client::world::TrackedBossBar {
            name_nbt: Vec::new(),
            progress: 0.5,
            color: 1,
            divisions: 0,
            flags: 0,
        },
    );
    let snapshot = project_render_snapshot(&world);
    assert_eq!(snapshot.entities.len(), 1);
    assert_eq!(snapshot.entities[0].position, [42.0, 64.0, -2.0]);
    assert_eq!(snapshot.pending_particles.len(), 1);
    assert_eq!(snapshot.boss_bars.len(), 1);
    assert_eq!(
        snapshot.block_entities,
        vec![(
            [4, 70, 0],
            megu_client::world::BlockEntityVisualState::Chest {
                open_count: 0,
                trapped: false,
                ender: false
            }
        )]
    );
    assert_eq!(
        (
            snapshot.world_age,
            snapshot.time_of_day,
            snapshot.thunder_level
        ),
        (99, 18000, 0.75)
    );
    assert_eq!(snapshot.rain_level, 0.25);
    assert_eq!(
        snapshot.level_events,
        vec![(
            VisualLevelEvent::BlockDestroy { block_state: 42 },
            [1, 2, 3]
        )]
    );
}

#[test]
fn particle_payloads_decode_all_requested_option_shapes() {
    assert_eq!(
        decode_particle_options(2, &vi(31)).unwrap(),
        DecodedParticleOptions::BlockState(31)
    );
    assert_eq!(
        decode_particle_options(3, &vi(8)).unwrap(),
        DecodedParticleOptions::BlockState(8)
    );
    assert_eq!(
        decode_particle_options(29, &vi(9)).unwrap(),
        DecodedParticleOptions::BlockState(9)
    );
    assert_eq!(
        decode_particle_options(104, &vi(10)).unwrap(),
        DecodedParticleOptions::BlockState(10)
    );
    assert_eq!(
        decode_particle_options(
            14,
            &[f32be(0.1), f32be(0.2), f32be(0.3), f32be(1.5)].concat()
        )
        .unwrap(),
        DecodedParticleOptions::Dust {
            red: 0.1,
            green: 0.2,
            blue: 0.3,
            scale: 1.5
        }
    );
    let mut transition = Vec::new();
    for value in [0.1, 0.2, 0.3, 0.5, 0.7, 0.8, 0.9] {
        transition.extend_from_slice(&f32be(value));
    }
    assert_eq!(
        decode_particle_options(15, &transition).unwrap(),
        DecodedParticleOptions::DustColorTransition {
            from: [0.1, 0.2, 0.3],
            scale: 0.5,
            to: [0.7, 0.8, 0.9],
        }
    );
    assert_eq!(
        decode_particle_options(21, &0x7f12_3456i32.to_be_bytes()).unwrap(),
        DecodedParticleOptions::EntityEffect { argb: 0x7f12_3456 }
    );
    assert_eq!(
        decode_particle_options(36, &f32be(1.25)).unwrap(),
        DecodedParticleOptions::SculkCharge { roll: 1.25 }
    );
    assert_eq!(
        decode_particle_options(45, &[0]).unwrap(),
        DecodedParticleOptions::Item { slot_wire: vec![0] }
    );

    let mut vibration = vi(0); // block position source
    vibration.extend_from_slice(&pack_position([1, 2, 3]).to_be_bytes());
    vibration.extend(vi(27));
    assert_eq!(
        decode_particle_options(46, &vibration).unwrap(),
        DecodedParticleOptions::Vibration {
            source: VibrationSource::Block([1, 2, 3]),
            arrival_ticks: 27,
        }
    );
    let mut entity_vibration = vi(1);
    entity_vibration.extend(vi(72));
    entity_vibration.extend_from_slice(&f32be(1.6));
    entity_vibration.extend(vi(15));
    assert_eq!(
        decode_particle_options(46, &entity_vibration).unwrap(),
        DecodedParticleOptions::Vibration {
            source: VibrationSource::Entity {
                entity_id: 72,
                eye_height: 1.6
            },
            arrival_ticks: 15,
        }
    );
    assert_eq!(
        decode_particle_options(98, &vi(8)).unwrap(),
        DecodedParticleOptions::Shriek { delay: 8 }
    );
}

#[test]
fn entity_metadata_decodes_shared_flags_pose_and_entity_values() {
    let mut metadata = vec![0, 0]; // index 0, byte type
    metadata.push(0x19); // on fire, sprinting, glowing
    metadata.extend([6, 20]); // shared pose index, pose type
    metadata.extend(vi(5));
    metadata.extend([7, 8, 1]); // baby boolean at an entity-specific index
    metadata.push(0xff);
    let decoded = decode_entity_metadata(&metadata).unwrap();
    assert_eq!(decoded[0].value, DecodedEntityMetadataValue::Byte(0x19));
    assert_eq!(decoded[1].value, DecodedEntityMetadataValue::Pose(5));
    assert_eq!(decoded[2].value, DecodedEntityMetadataValue::Bool(true));
}

#[test]
fn packet_decode_and_world_updates_keep_visual_state() {
    let mut world = test_world();
    world
        .apply_play_event(&PlayEvent::AddEntity {
            eid: 42,
            uuid: [4; 16],
            entity_type: 1,
            x: 1.0,
            y: 2.0,
            z: 3.0,
            pitch: 0.0,
            yaw: 0.0,
            head_yaw: 0.0,
            data: 0,
            vx: 0.0,
            vy: 0.0,
            vz: 0.0,
        })
        .unwrap();

    let mut metadata = vec![0, 0, 0x19, 6, 20];
    metadata.extend(vi(5));
    metadata.push(0xff);
    let mut body = vi(42);
    body.extend_from_slice(&metadata);
    let event = decode_extended_play_event("set_entity_data", &body)
        .unwrap()
        .unwrap();
    world.apply_play_event(&event).unwrap();
    assert_eq!(world.entity_poses.get(&42), Some(&5));
    assert_eq!(world.entity_metadata[&42].len(), 2);

    let mut particle = vec![1]; // long distance
    for value in [1.0f64, 2.0, 3.0] {
        particle.extend_from_slice(&value.to_be_bytes());
    }
    for value in [0.1f32, 0.2, 0.3, 0.4] {
        particle.extend_from_slice(&value.to_be_bytes());
    }
    particle.extend_from_slice(&2i32.to_be_bytes()); // count
    particle.extend(vi(36)); // sculk_charge
    particle.extend_from_slice(&f32be(0.75));
    let event = decode_extended_play_event("level_particles", &particle)
        .unwrap()
        .unwrap();
    world.apply_play_event(&event).unwrap();
    assert_eq!(
        world.level_particle_options.back().unwrap(),
        &(36, DecodedParticleOptions::SculkCharge { roll: 0.75 })
    );

    world
        .apply_play_event(&PlayEvent::SetEquipment {
            eid: 42,
            entries: vec![
                megu_client::protocol::EquipmentEntry {
                    slot: 0,
                    item_wire: vec![0],
                },
                megu_client::protocol::EquipmentEntry {
                    slot: 5,
                    item_wire: vec![0],
                },
            ],
        })
        .unwrap();
    assert_eq!(world.entity_equipment.get(&(42, 5)), Some(&vec![0]));
    world
        .apply_play_event(&PlayEvent::Animate {
            eid: 42,
            animation: 0,
        })
        .unwrap();
    assert_eq!(world.entity_animation_ticks.get(&42), Some(&6));
    world
        .apply_play_event(&PlayEvent::HurtAnimation { eid: 42, yaw: 90.0 })
        .unwrap();
    assert_eq!(world.entities[&42].hurt_ticks, 10);

    let mut block_entity_body = megu_client::protocol::pack_position([4, 70, -3])
        .to_be_bytes()
        .to_vec();
    block_entity_body.extend(vi(1));
    block_entity_body.extend_from_slice(&[10, 0]); // empty root compound
    let event = decode_extended_play_event("block_entity_data", &block_entity_body)
        .unwrap()
        .unwrap();
    world.apply_play_event(&event).unwrap();
    assert!(matches!(
        world.block_entity_visual_nbt.get(&[4, 70, -3]),
        Some(megu_client::protocol::NbtValue::Compound(values)) if values.is_empty()
    ));
    world
        .apply_play_event(&PlayEvent::MerchantOffers {
            window_id: 3,
            offers: Vec::new(),
            villager_level: 4,
            villager_xp: 120,
            show_progress: true,
            can_restock: true,
        })
        .unwrap();
    assert_eq!(world.merchant_panels[&3].villager_level, 4);
}

#[test]
fn boss_effect_weather_time_and_container_updates_are_retained() {
    let mut world = test_world();
    let uuid = [9; 16];
    let mut add = vec![10, 0]; // empty anonymous compound title
    add.extend_from_slice(&f32be(0.8));
    add.extend(vi(2));
    add.extend(vi(3));
    add.push(5);
    let parsed = decode_boss_bar_action(0, &add).unwrap();
    assert!(matches!(
        parsed,
        DecodedBossBarAction::Add {
            color: 2,
            divisions: 3,
            flags: 5,
            ..
        }
    ));
    assert_eq!(
        decode_boss_bar_action(1, &[]).unwrap(),
        DecodedBossBarAction::Remove
    );
    assert_eq!(
        decode_boss_bar_action(2, &f32be(0.2)).unwrap(),
        DecodedBossBarAction::UpdateProgress(0.2)
    );
    assert_eq!(
        decode_boss_bar_action(3, &[10, 0]).unwrap(),
        DecodedBossBarAction::UpdateName(vec![10, 0])
    );
    let mut style = vi(4);
    style.extend(vi(6));
    assert_eq!(
        decode_boss_bar_action(4, &style).unwrap(),
        DecodedBossBarAction::UpdateStyle {
            color: 4,
            divisions: 6
        }
    );
    assert_eq!(
        decode_boss_bar_action(5, &[0x0b]).unwrap(),
        DecodedBossBarAction::UpdateFlags(0x0b)
    );
    world
        .apply_play_event(&PlayEvent::BossEvent {
            uuid,
            action: 0,
            raw_payload: add,
        })
        .unwrap();
    world
        .apply_play_event(&PlayEvent::BossEvent {
            uuid,
            action: 2,
            raw_payload: f32be(0.4).to_vec(),
        })
        .unwrap();
    assert_eq!(world.boss_bars[&uuid].progress, 0.4);
    world
        .apply_play_event(&PlayEvent::ContainerSetData {
            window_id: 3,
            property: 2,
            value: 180,
        })
        .unwrap();
    assert_eq!(world.container_properties.get(&(3, 2)), Some(&180));
    world
        .apply_play_event(&PlayEvent::UpdateMobEffect {
            eid: 42,
            effect_id: 5,
            amplifier: 1,
            duration: 4,
            flags: 3,
        })
        .unwrap();
    assert_eq!(world.mob_effects[&(42, 5)].duration, 4);
    world
        .apply_play_event(&PlayEvent::SetTime {
            world_age: 1234,
            time_of_day: 18000,
        })
        .unwrap();
    world
        .apply_play_event(&PlayEvent::GameEvent {
            event: 1,
            value: 0.0,
        })
        .unwrap();
    world
        .apply_play_event(&PlayEvent::GameEvent {
            event: 8,
            value: 0.6,
        })
        .unwrap();
    world
        .apply_play_event(&PlayEvent::GameEvent {
            event: 3,
            value: 2.0,
        })
        .unwrap();
    assert_eq!((world.world_age, world.time_of_day), (1234, 18000));
    assert_eq!((world.rain_level, world.thunder_level), (1.0, 0.6));
    assert_eq!(world.game_mode, Some(2));
    world
        .apply_play_event(&PlayEvent::BossEvent {
            uuid,
            action: 1,
            raw_payload: Vec::new(),
        })
        .unwrap();
    assert!(!world.boss_bars.contains_key(&uuid));
}
