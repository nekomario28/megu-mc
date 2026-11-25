// SPDX-License-Identifier: LGPL-3.0-or-later
//! Protocol 767 visual-state decode and world-cache update regressions.

use megu_client::protocol::DimensionMeta;
use megu_client::protocol::{
    decode_boss_bar_action, decode_entity_metadata, decode_extended_play_event,
    decode_particle_options, pack_position, write_varint, DecodedBossBarAction,
    DecodedEntityMetadataValue, DecodedParticleOptions, PlayEvent, VibrationSource,
};
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
