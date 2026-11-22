// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
use std::collections::HashMap;

use mc_spine_kernel::skeletal_anim::{
    decode_animation_trigger, encode_animation_trigger, parse_animation_json, parse_geometry_json,
    parse_geometry_rigs_json, parse_jem_model, parse_jpm_model, AnimationClip, AnimationController,
    AnimationError, AnimationTrigger,
    BoneDef, BoneMask, BoneTrack, BoneTransform, InterpolationMode, Keyframe, LoopMode,
    SkeletonRig, ANIMATION_CHANNEL, ANM1_BYTES, MAX_BONES, MAX_JSON_DEPTH,
};

fn bone(name: &str, parent: Option<usize>, pivot: [f32; 3]) -> BoneDef {
    BoneDef {
        name: name.to_owned(),
        parent,
        pivot,
        bind_rotation_deg: [0.0; 3],
    }
}

fn approx(actual: f32, expected: f32) {
    assert!((actual - expected).abs() < 0.0001, "{actual} != {expected}");
}

fn clip(name: &str, bone_name: &str, position: [f32; 3]) -> AnimationClip {
    AnimationClip {
        name: name.to_owned(),
        length_sec: 1.0,
        loop_mode: LoopMode::HoldOnLastFrame,
        tracks: HashMap::from([(
            bone_name.to_owned(),
            BoneTrack {
                position: vec![Keyframe::new(0.0, position, InterpolationMode::Linear)],
                ..Default::default()
            },
        )]),
    }
}

#[test]
fn forward_kinematics_resolves_child_first_source_order_and_relative_pivots() {
    let rig = SkeletonRig::new(vec![
        bone("hand", Some(1), [3.0, 0.0, 0.0]),
        bone("shoulder", None, [1.0, 0.0, 0.0]),
    ])
    .unwrap();
    assert_eq!(rig.evaluation_order(), &[1, 0]);
    let mut poses = vec![BoneTransform::default(); 2];
    poses[1].rotation_deg[2] = 90.0;
    let matrices = rig.evaluate_world_matrices(&poses).unwrap();
    approx(matrices[1][12], 1.0);
    approx(matrices[1][13], 0.0);
    approx(matrices[0][12], 1.0);
    approx(matrices[0][13], 2.0);
    approx(matrices[0][0], 0.0);
    approx(matrices[0][1], 1.0);

    poses[0].position = [1.0, 0.0, 0.0];
    let matrices = rig.evaluate_world_matrices(&poses).unwrap();
    approx(matrices[0][13], 3.0);
    assert_eq!(
        rig.evaluate_world_matrices(&[]),
        Err(AnimationError::InvalidRig)
    );
    poses[0].scale[0] = f32::NAN;
    assert_eq!(
        rig.evaluate_world_matrices(&poses),
        Err(AnimationError::InvalidRig)
    );
}

#[test]
fn rig_rejects_invalid_parents_cycles_names_and_non_finite_data() {
    assert!(matches!(
        SkeletonRig::new(vec![bone("a", Some(0), [0.0; 3])]),
        Err(AnimationError::InvalidRig)
    ));
    assert!(matches!(
        SkeletonRig::new(vec![bone("a", Some(2), [0.0; 3])]),
        Err(AnimationError::InvalidRig)
    ));
    assert!(matches!(
        SkeletonRig::new(vec![
            bone("a", Some(1), [0.0; 3]),
            bone("b", Some(0), [0.0; 3])
        ]),
        Err(AnimationError::InvalidRig)
    ));
    assert!(matches!(
        SkeletonRig::new(vec![bone("a", None, [0.0; 3]), bone("a", None, [0.0; 3])]),
        Err(AnimationError::InvalidRig)
    ));
    assert!(matches!(
        SkeletonRig::new(vec![bone("", None, [0.0; 3])]),
        Err(AnimationError::InvalidRig)
    ));
    assert!(matches!(
        SkeletonRig::new(vec![bone("a", None, [f32::INFINITY, 0.0, 0.0])]),
        Err(AnimationError::InvalidRig)
    ));
    assert!(matches!(
        SkeletonRig::new(vec![bone("a", None, [0.0; 3]); MAX_BONES + 1]),
        Err(AnimationError::LimitExceeded)
    ));
}

#[test]
fn step_linear_and_catmullrom_interpolation_and_pre_post() {
    let mut track = BoneTrack {
        position: vec![
            Keyframe::new(0.0, [0.0; 3], InterpolationMode::Step),
            Keyframe::new(1.0, [10.0, 0.0, 0.0], InterpolationMode::Linear),
            Keyframe::new(2.0, [0.0; 3], InterpolationMode::CatmullRom),
            Keyframe::new(3.0, [10.0, 0.0, 0.0], InterpolationMode::Linear),
            Keyframe::new(4.0, [0.0; 3], InterpolationMode::Linear),
        ],
        ..Default::default()
    };
    approx(track.sample(0.5).unwrap().position[0], 0.0);
    approx(track.sample(1.5).unwrap().position[0], 5.0);
    approx(track.sample(2.5).unwrap().position[0], 5.0);
    let mut smooth = track.clone();
    smooth.position[1].interpolation = InterpolationMode::CatmullRom;
    smooth.position[3].value[0] = 0.0;
    approx(smooth.sample(1.5).unwrap().position[0], 5.625);

    track.position[2].pre = Some([3.0, 0.0, 0.0]);
    approx(track.sample(1.5).unwrap().position[0], 6.5);
    approx(track.sample(2.0).unwrap().position[0], 0.0);
    track.rotation = vec![Keyframe::new(
        0.0,
        [0.0, 180.0, 0.0],
        InterpolationMode::Linear,
    )];
    track.scale = vec![Keyframe::new(0.0, [2.0; 3], InterpolationMode::Linear)];
    assert_eq!(track.sample(0.0).unwrap().rotation_deg[1], 180.0);
    assert_eq!(track.sample(0.0).unwrap().scale, [2.0; 3]);
    track.position[2].time_sec = 1.0;
    assert!(track.validate().is_err());
    assert_eq!(
        track.sample(f32::INFINITY),
        Err(AnimationError::InvalidTime)
    );
}

#[test]
fn bedrock_geometry_and_animation_json_load_and_sample() {
    let geometry = r#"{
      "format_version":"1.12.0",
      "minecraft:geometry":[{"description":{"identifier":"geometry.example"},"bones":[
        {"name":"arm","parent":"body","pivot":[2,3,0],"rotation":[0,0,10]},
        {"name":"body","pivot":[0,1,0]}
      ]}]
    }"#;
    let rigs = parse_geometry_rigs_json(geometry).unwrap();
    assert_eq!(rigs[0].0, "geometry.example");
    let rig = parse_geometry_json(geometry).unwrap();
    assert_eq!(rig.bones()[0].parent, Some(1));
    assert_eq!(rig.bones()[0].bind_rotation_deg[2], 10.0);

    let data = r#"{
      "format_version":"1.8.0",
      "animations":{
        "animation.example.swing":{
          "loop":"hold_on_last_frame","animation_length":2.0,
          "bones":{"arm":{
            "position":{"0.0":[0,0,0],"1.0":{"pre":[2,0,0],"post":[4,0,0],"lerp_mode":"catmullrom"},"2.0":[8,0,0]},
            "rotation":{"0.0":[0,0,0],"2.0":[0,90,0]},
            "scale":1.5
          }}
        },
        "animation.example.walk":{"loop":true,"bones":{"body":{"position":[1,0,0]}}}
      }
    }"#;
    let clips = parse_animation_json(data).unwrap();
    assert_eq!(clips.len(), 2);
    let swing = &clips[0];
    assert_eq!(swing.loop_mode, LoopMode::HoldOnLastFrame);
    approx(swing.sample_pose(&rig, 0.5).unwrap()[0].position[0], 1.0);
    approx(swing.sample_pose(&rig, 1.0).unwrap()[0].position[0], 4.0);
    approx(swing.sample_pose(&rig, 10.0).unwrap()[0].position[0], 8.0);
    approx(
        swing.sample_pose(&rig, 1.0).unwrap()[0].rotation_deg[1],
        45.0,
    );
    approx(swing.sample_pose(&rig, 1.0).unwrap()[0].scale[0], 1.5);
    assert_eq!(clips[1].loop_mode, LoopMode::Loop);
    let mut controller = AnimationController::new(rig, clips).unwrap();
    let layer = controller
        .add_layer(BoneMask::all(&parse_geometry_json(geometry).unwrap()), 1.0)
        .unwrap();
    controller
        .play(layer, "animation.example.swing", 0.0, 1.0, true)
        .unwrap();
    controller.advance(1.0).unwrap();
    approx(controller.evaluate_pose().unwrap()[0].position[0], 4.0);
}

#[test]
fn json_loader_fails_closed_for_malformed_cycles_and_resource_limits() {
    for json in [
        r#"{"minecraft:geometry":[{"bones":[{"name":"a","parent":"b"},{"name":"b","parent":"a"}]}]}"#,
        r#"{"minecraft:geometry":[{"bones":[{"name":"a","parent":"missing"}]}]}"#,
        r#"{"minecraft:geometry":[{"bones":[{"name":"a"},{"name":"a"}]}]}"#,
        r#"{"minecraft:geometry":[{"bones":[{"name":"a","pivot":[1e300,0,0]}]}]}"#,
        r#"{"minecraft:geometry":[{"bones":[],"bones":[]}]}"#,
        r#"{"minecraft:geometry":[{"bones":[{"name":"\uD800"}]}]}"#,
        r#"{"minecraft:geometry":[{"bones":[{"name":"a","pivot":[01,0,0]}]}]}"#,
        r#"{"minecraft:geometry":[]}"#,
        r#"{"minecraft:geometry":[{"bones":[]}]} trailing"#,
    ] {
        assert!(parse_geometry_json(json).is_err(), "accepted {json}");
    }
    for json in [
        r#"{"animations":{"animation.a":{"animation_length":1,"bones":{"arm":{"rotation":{"0":[0,0,0],"0.0":[0,0,0]}}}}}}"#,
        r#"{"animations":{"animation.a":{"animation_length":1,"bones":{"arm":{"position":{"-1":[0,0,0]}}}}}}"#,
        r#"{"animations":{"animation.a":{"animation_length":1,"bones":{"arm":{"position":["query.anim_time",0,0]}}}}}"#,
        r#"{"animations":{"animation.a":{"animation_length":1,"bones":{"arm":{"rotation":{"0":{"post":[0,0,0],"lerp_mode":"mystery"}}}}}}}"#,
        r#"{"animations":{"animation.a":{"animation_length":0.5,"bones":{"arm":{"position":{"1":[0,0,0]}}}}}}"#,
        r#"{"animations":{"animation.a":{"loop":"unknown"}}}"#,
    ] {
        assert!(parse_animation_json(json).is_err(), "accepted {json}");
    }
    assert_eq!(
        parse_geometry_json(&"x".repeat(1_048_577)).err(),
        Some(AnimationError::LimitExceeded)
    );
    let nested = format!(
        "{}0{}",
        "[".repeat(MAX_JSON_DEPTH + 2),
        "]".repeat(MAX_JSON_DEPTH + 2)
    );
    assert_eq!(
        parse_animation_json(&nested).err(),
        Some(AnimationError::LimitExceeded)
    );
}

#[test]
fn upper_lower_masks_concurrent_layers_and_interrupted_crossfade_remain_continuous() {
    let rig = SkeletonRig::new(vec![
        bone("legs", None, [0.0; 3]),
        bone("upper", Some(0), [0.0; 3]),
    ])
    .unwrap();
    let mut controller = AnimationController::new(
        rig.clone(),
        vec![
            clip("walk", "legs", [4.0, 0.0, 0.0]),
            clip("cast", "upper", [0.0, 10.0, 0.0]),
            clip("slash", "upper", [0.0, 20.0, 0.0]),
        ],
    )
    .unwrap();
    let lower = controller
        .add_layer(BoneMask::only(&rig, &["legs"]).unwrap(), 1.0)
        .unwrap();
    let upper = controller
        .add_layer(BoneMask::only(&rig, &["upper"]).unwrap(), 1.0)
        .unwrap();
    controller.play(lower, "walk", 0.0, 1.0, true).unwrap();
    controller.play(upper, "cast", 1.0, 1.0, true).unwrap();
    let initial = controller.evaluate_pose().unwrap();
    approx(initial[0].position[0], 4.0);
    approx(initial[1].position[1], 0.0);
    controller.advance(0.5).unwrap();
    let half = controller.evaluate_pose().unwrap();
    approx(half[0].position[0], 4.0);
    approx(half[1].position[1], 5.0);
    controller.play(upper, "slash", 2.0, 2.0, true).unwrap();
    approx(controller.evaluate_pose().unwrap()[1].position[1], 5.0);
    controller.advance(1.0).unwrap();
    approx(controller.evaluate_pose().unwrap()[1].position[1], 12.5);
    controller.advance(1.0).unwrap();
    approx(controller.evaluate_pose().unwrap()[1].position[1], 20.0);
    controller.play(upper, "slash", 0.5, 0.5, false).unwrap();
    assert_eq!(controller.layers()[upper].current_clip(), Some("slash"));
    assert!(controller
        .add_layer(
            BoneMask {
                weights: vec![f32::NAN, 1.0]
            },
            1.0
        )
        .is_err());
    assert!(controller.play(upper, "missing", 0.0, 1.0, true).is_err());
    assert!(controller.advance(-1.0).is_err());
}

#[test]
fn once_loop_and_hold_follow_distinct_end_behavior() {
    let rig = SkeletonRig::new(vec![bone("arm", None, [0.0; 3])]).unwrap();
    let mut value = clip("test", "arm", [8.0, 0.0, 0.0]);
    value.loop_mode = LoopMode::Once;
    approx(value.sample_pose(&rig, 1.0).unwrap()[0].position[0], 8.0);
    approx(value.sample_pose(&rig, 2.0).unwrap()[0].position[0], 0.0);
    value.loop_mode = LoopMode::HoldOnLastFrame;
    approx(value.sample_pose(&rig, 2.0).unwrap()[0].position[0], 8.0);
    value
        .tracks
        .get_mut("arm")
        .unwrap()
        .position
        .push(Keyframe::new(1.0, [0.0; 3], InterpolationMode::Linear));
    value.loop_mode = LoopMode::Loop;
    approx(value.sample_pose(&rig, 1.5).unwrap()[0].position[0], 4.0);
}

#[test]
fn anm1_roundtrip_byte_layout_and_hostile_payloads() {
    assert_eq!(ANIMATION_CHANNEL, "mc-spine:anim_v1");
    let event = AnimationTrigger {
        entity_id: 0x01020304,
        layer: 3,
        clip_id: 123,
        speed: 1.25,
        fade_ticks: 12,
        loop_mode: LoopMode::HoldOnLastFrame,
        restart: true,
    };
    let encoded = encode_animation_trigger(event).unwrap();
    assert_eq!(encoded.len(), ANM1_BYTES);
    assert_eq!(&encoded[..4], b"ANM1");
    assert_eq!(&encoded[4..8], &[4, 3, 2, 1]);
    assert_eq!(decode_animation_trigger(&encoded).unwrap(), event);
    let mut larger = encoded.to_vec();
    larger.push(0);
    assert!(decode_animation_trigger(&larger).is_err());
    assert!(decode_animation_trigger(&encoded[..18]).is_err());
    for (index, value) in [(0, b'X'), (8, 16), (17, 3), (18, 2)] {
        let mut bad = encoded;
        bad[index] = value;
        assert_eq!(
            decode_animation_trigger(&bad),
            Err(AnimationError::InvalidWire)
        );
    }
    let mut bad = encoded;
    bad[11..15].copy_from_slice(&f32::NAN.to_bits().to_le_bytes());
    assert!(decode_animation_trigger(&bad).is_err());
    bad = encoded;
    bad[4..8].copy_from_slice(&(-1_i32).to_le_bytes());
    assert!(decode_animation_trigger(&bad).is_err());
    bad = encoded;
    bad[15..17].copy_from_slice(&1201_u16.to_le_bytes());
    assert!(decode_animation_trigger(&bad).is_err());
    assert!(encode_animation_trigger(AnimationTrigger {
        speed: -0.1,
        ..event
    })
    .is_err());
}

#[test]
fn jem_model_parses_part_tree_into_rig() {
    let jem = r#"{
        "textureSize": [64, 64],
        "models": [
            {
                "part": "body",
                "translate": [0, -24, 0],
                "submodels": [
                    {
                        "part": "head",
                        "id": "antenna",
                        "translate": [0, -6, 0],
                        "rotate": [15, 0, 0],
                        "submodel": {"part": "tip", "translate": [0, -2, 0]}
                    }
                ]
            },
            {"part": "leg1", "translate": [-2, -6, 4]}
        ]
    }"#;
    let rig = parse_jem_model(jem).unwrap();
    let bones = rig.bones();
    assert_eq!(bones.len(), 4);
    assert_eq!(bones[0].name, "body");
    assert_eq!(bones[0].parent, None);
    assert_eq!(bones[1].name, "antenna");
    assert_eq!(bones[1].parent, Some(0));
    assert_eq!(bones[1].bind_rotation_deg, [15.0, 0.0, 0.0]);
    assert_eq!(bones[2].name, "tip");
    assert_eq!(bones[2].parent, Some(1));
    assert_eq!(bones[3].name, "leg1");
    // Malformed/missing structure and duplicate ids fail closed.
    assert!(parse_jem_model("{}").is_err());
    let dup = r#"{"models":[{"part":"a"},{"part":"a"}]}"#;
    assert!(parse_jem_model(dup).is_err());
}

#[test]
fn jpm_model_parses_root_part_tree() {
    let jpm = r#"{
        "textureSize": [32, 32],
        "id": "halo",
        "translate": [0, -28, 0],
        "submodels": [{"id": "ring", "translate": [0, -1, 0], "rotate": [0, 45, 0]}]
    }"#;
    let rig = parse_jpm_model(jpm).unwrap();
    let bones = rig.bones();
    assert_eq!(bones.len(), 2);
    assert_eq!(bones[0].name, "halo");
    assert_eq!(bones[0].pivot, [0.0, -28.0, 0.0]);
    assert_eq!(bones[1].name, "ring");
    assert_eq!(bones[1].parent, Some(0));
    assert_eq!(bones[1].bind_rotation_deg, [0.0, 45.0, 0.0]);
    assert!(parse_jpm_model("[]").is_err());
}
