// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

use megu_client::protocol::DimensionMeta;
use megu_client::world::{BlockTable, WorldState};
use megu_kernel::skeletal_anim::{
    encode_animation_trigger, parse_animation_json, parse_geometry_json, AnimationError,
    AnimationTrigger, BoneMask, LoopMode, ANM1_BYTES,
};

const GEOMETRY: &str = r#"{
  "format_version":"1.12.0",
  "minecraft:geometry":[{
    "description":{"identifier":"geometry.test_actor"},
    "bones":[
      {"name":"root","pivot":[0,0,0]},
      {"name":"leg","parent":"root","pivot":[0,-1,0]},
      {"name":"arm","parent":"root","pivot":[1,0,0]}
    ]
  }]
}"#;

const ANIMATIONS: &str = r#"{
  "format_version":"1.8.0",
  "animations":{
    "animation.test.walk":{
      "loop":true,"animation_length":1.0,
      "bones":{
        "root":{"position":{"0.0":[0,0,0],"1.0":[2,0,0]}},
        "leg":{"rotation":{"0.0":[0,0,0],"1.0":[30,0,0]}}
      }
    },
    "animation.test.cast":{
      "loop":false,"animation_length":1.0,
      "bones":{
        "root":{"position":[20,0,0]},
        "arm":{"rotation":{"0.0":[0,0,0],"1.0":[0,0,90]}}
      }
    }
  }
}"#;

fn payload(entity_id: i32, layer: u8, clip_id: u16, loop_mode: LoopMode) -> [u8; ANM1_BYTES] {
    encode_animation_trigger(AnimationTrigger {
        entity_id,
        layer,
        clip_id,
        speed: 1.0,
        fade_ticks: 0,
        loop_mode,
        restart: true,
    })
    .unwrap()
}

#[test]
fn bedrock_animations_layer_and_despawn_in_world_state() {
    let rig = parse_geometry_json(GEOMETRY).unwrap();
    let mut world = WorldState::new(DimensionMeta::default(), BlockTable::builtin());
    world
        .animations
        .register_model_json("test_actor", GEOMETRY, ANIMATIONS)
        .unwrap();

    // JSON object order does not define the wire ID. Names sort once at registration.
    let walk_id = world
        .animations
        .clip_id("test_actor", "animation.test.walk")
        .unwrap();
    let cast_id = world
        .animations
        .clip_id("test_actor", "animation.test.cast")
        .unwrap();
    assert_eq!((cast_id, walk_id), (0, 1));

    let upper_body = BoneMask::only(&rig, &["arm"]).unwrap();
    world
        .animations
        .bind_entity_with_layers(
            42,
            "test_actor",
            vec![(BoneMask::all(&rig), 1.0), (upper_body, 1.0)],
        )
        .unwrap();
    world.animations.bind_entity(43, "test_actor").unwrap();
    assert_eq!(world.animations.entity_count(), 2);

    world
        .ingest_anm1_payload(&payload(42, 0, walk_id, LoopMode::Loop))
        .unwrap();
    world
        .ingest_anm1_payload(&payload(42, 1, cast_id, LoopMode::Once))
        .unwrap();
    world
        .ingest_anm1_payload(&payload(43, 0, walk_id, LoopMode::Loop))
        .unwrap();
    world.advance_animations(0.5).unwrap();

    let layered = world.evaluate_entity_pose(42).unwrap().unwrap();
    let walking = world.evaluate_entity_pose(43).unwrap().unwrap();
    assert_eq!(layered.len(), 3);
    assert_eq!(layered[rig.bone_index("root").unwrap()], walking[0]);
    assert_eq!(layered[rig.bone_index("leg").unwrap()], walking[1]);
    assert_ne!(layered[rig.bone_index("arm").unwrap()], walking[2]);
    assert!((layered[0][12] - 1.0).abs() < 1.0e-5);
    assert_ne!(
        layered[0],
        [1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0]
    );

    assert!(world.despawn_entity(42));
    assert!(!world.despawn_entity(42));
    assert!(world.evaluate_entity_pose(42).unwrap().is_none());
    assert_eq!(world.animations.entity_count(), 1);
    world.reset_dimension(DimensionMeta::default());
    assert_eq!(world.animations.entity_count(), 0);
    assert!(world.evaluate_entity_pose(43).unwrap().is_none());
}

#[test]
fn corrupt_anm1_and_unknown_targets_fail_without_mutating_pose() {
    let mut world = WorldState::new(DimensionMeta::default(), BlockTable::builtin());
    world
        .animations
        .register_model_json("test_actor", GEOMETRY, ANIMATIONS)
        .unwrap();
    world.animations.bind_entity(7, "test_actor").unwrap();
    let before = world.evaluate_entity_pose(7).unwrap().unwrap();
    let walk_id = world
        .animations
        .clip_id("test_actor", "animation.test.walk")
        .unwrap();
    let valid = payload(7, 0, walk_id, LoopMode::Loop);

    let mut bad_magic = valid;
    bad_magic[0] = b'X';
    let mut bad_speed = valid;
    bad_speed[11..15].copy_from_slice(&f32::NAN.to_le_bytes());
    let mut bad_mode = valid;
    bad_mode[17] = 255;

    for invalid in [
        bad_magic.as_slice(),
        bad_speed.as_slice(),
        bad_mode.as_slice(),
        &valid[..ANM1_BYTES - 1],
    ] {
        assert_eq!(
            world.ingest_anm1_payload(invalid),
            Err(AnimationError::InvalidWire)
        );
        assert_eq!(world.evaluate_entity_pose(7).unwrap().unwrap(), before);
    }
    assert_eq!(
        world.ingest_anm1_payload(&payload(8, 0, walk_id, LoopMode::Loop)),
        Err(AnimationError::InvalidWire)
    );
    assert_eq!(
        world.ingest_anm1_payload(&payload(7, 0, 4095, LoopMode::Loop)),
        Err(AnimationError::UnknownClip)
    );
    assert_eq!(world.evaluate_entity_pose(7).unwrap().unwrap(), before);
    assert_eq!(
        world.advance_animations(f32::INFINITY),
        Err(AnimationError::InvalidTime)
    );
    assert_eq!(world.evaluate_entity_pose(7).unwrap().unwrap(), before);

    world.ingest_anm1_payload(&valid).unwrap();
    world.advance_animations(0.25).unwrap();
    assert_ne!(world.evaluate_entity_pose(7).unwrap().unwrap(), before);
}

#[test]
fn explicit_wire_ids_and_loop_mode_overrides_are_applied() {
    let rig = parse_geometry_json(GEOMETRY).unwrap();
    let walk = parse_animation_json(ANIMATIONS)
        .unwrap()
        .into_iter()
        .find(|clip| clip.name == "animation.test.walk")
        .unwrap();
    assert_eq!(walk.loop_mode, LoopMode::Loop);

    let mut world = WorldState::new(DimensionMeta::default(), BlockTable::builtin());
    world
        .animations
        .register_model_with_clip_ids("actor", rig, vec![(77, walk)])
        .unwrap();
    assert_eq!(world.animations.clip_id("actor", "animation.test.walk"), Some(77));
    world.animations.bind_entity(9, "actor").unwrap();

    // The ANM1 Once flag overrides the JSON file's Loop setting.
    world
        .ingest_anm1_payload(&payload(9, 0, 77, LoopMode::Once))
        .unwrap();
    world.advance_animations(1.25).unwrap();
    let finished = world.evaluate_entity_pose(9).unwrap().unwrap();
    assert_eq!(finished[0][12], 0.0);

    world
        .ingest_anm1_payload(&payload(9, 0, 77, LoopMode::Loop))
        .unwrap();
    world.advance_animations(1.25).unwrap();
    let looping = world.evaluate_entity_pose(9).unwrap().unwrap();
    assert!((looping[0][12] - 0.5).abs() < 1.0e-5);
}
