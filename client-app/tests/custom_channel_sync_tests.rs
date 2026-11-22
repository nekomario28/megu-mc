// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

use mc_spine_client::protocol::{
    decode_custom_channel_event, decode_custom_payload_packet, dispatch_custom_channel_to_hosts,
    ClientCustomChannelEvent, DecodedMagicCastState, DecodedMotionHand, DecodedMotionPhase,
    CHANNEL_ANIM_V1, CHANNEL_MAGIC_V1, CHANNEL_MOTION_V1, CHANNEL_VFX_V1,
};
use mc_spine_client::render::ClientVfxHost;
use mc_spine_client::render_graph::GpuPipelineKind;
use mc_spine_client::world::WorldAnimationHost;
use mc_spine_client_mesh::vfx_engine::{DynamicColoredLight, Vfx1Packet, VfxCommand};
use mc_spine_kernel::skeletal_anim::{encode_animation_trigger, AnimationTrigger, LoopMode};

const GEOMETRY: &str = r#"{
  "format_version":"1.12.0",
  "minecraft:geometry":[{"description":{"identifier":"geometry.test"},
    "bones":[{"name":"root","pivot":[0,0,0]}]}]
}"#;

const ANIMATIONS: &str = r#"{
  "format_version":"1.8.0",
  "animations":{"animation.test.move":{"loop":true,"animation_length":1.0,
    "bones":{"root":{"position":{"0.0":[0,0,0],"1.0":[2,0,0]}}}}}
}"#;

fn write_varint(out: &mut Vec<u8>, mut value: u32) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        out.push(byte | if value == 0 { 0 } else { 0x80 });
        if value == 0 {
            break;
        }
    }
}

fn write_string(out: &mut Vec<u8>, value: &str) {
    write_varint(out, value.len() as u32);
    out.extend_from_slice(value.as_bytes());
}

fn framed(channel: &str, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::new();
    write_string(&mut frame, channel);
    frame.extend_from_slice(payload);
    frame
}

fn magic_payload(cast_tag: u8) -> Vec<u8> {
    let mut bytes = b"MAG1".to_vec();
    bytes.extend_from_slice(&7.5f32.to_le_bytes());
    bytes.extend_from_slice(&20.0f32.to_le_bytes());
    bytes.push(cast_tag);
    if cast_tag != 0 {
        write_string(&mut bytes, "spell:flame");
        if cast_tag == 1 {
            write_varint(&mut bytes, 400);
            write_varint(&mut bytes, 125);
        } else {
            write_varint(&mut bytes, 5);
            write_varint(&mut bytes, 200);
            write_varint(&mut bytes, 10);
            bytes.extend_from_slice(&2.5f32.to_le_bytes());
        }
    }
    write_varint(&mut bytes, 2);
    write_string(&mut bytes, "spell:flame");
    write_varint(&mut bytes, 200);
    write_string(&mut bytes, "spell:ice");
    write_varint(&mut bytes, u32::MAX);
    write_varint(&mut bytes, 1);
    write_string(&mut bytes, "status:shield");
    bytes.push(3);
    write_varint(&mut bytes, 150);
    bytes.extend_from_slice(&3.25f32.to_le_bytes());
    bytes
}

fn motion_payload(hand: u8, phase: u8) -> Vec<u8> {
    let mut bytes = b"MOT1".to_vec();
    bytes.extend_from_slice(&0x0123_4567_89ab_cdef_u64.to_be_bytes());
    bytes.extend_from_slice(&0x1234_5678_u32.to_be_bytes());
    bytes.extend_from_slice(&257u16.to_be_bytes());
    bytes.push(hand);
    bytes.extend_from_slice(&1.5f32.to_be_bytes());
    bytes.extend_from_slice(&12345u32.to_be_bytes());
    bytes.push(phase);
    bytes
}

fn vfx_payload() -> Vec<u8> {
    Vfx1Packet {
        commands: vec![VfxCommand::SpawnLight {
            id: 42,
            light: DynamicColoredLight {
                pos: [1.0, 2.0, 3.0],
                rgb: [1.0, 0.5, 0.25],
                radius: 6.0,
                intensity: 2.0,
                ttl_ticks: 20,
                priority: 2,
            },
        }],
    }
    .encode()
    .unwrap()
}

fn anim_payload(clip_id: u16) -> Vec<u8> {
    encode_animation_trigger(AnimationTrigger {
        entity_id: 77,
        layer: 0,
        clip_id,
        speed: 1.0,
        fade_ticks: 0,
        loop_mode: LoopMode::Loop,
        restart: true,
    })
    .unwrap()
    .to_vec()
}

#[test]
fn vfx_and_anim_channels_dispatch_to_live_hosts() {
    let mut vfx = ClientVfxHost::new(GpuPipelineKind::ClassicLowSpec);
    let mut anim = WorldAnimationHost::new();
    anim.register_model_json("test", GEOMETRY, ANIMATIONS)
        .unwrap();
    anim.bind_entity(77, "test").unwrap();
    let before = anim.evaluate_entity_pose(77).unwrap().unwrap();

    let vfx_bytes = vfx_payload();
    assert_eq!(
        decode_custom_payload_packet(&framed(CHANNEL_VFX_V1, &vfx_bytes)).unwrap(),
        ClientCustomChannelEvent::Vfx(vfx_bytes.clone())
    );
    assert_eq!(
        dispatch_custom_channel_to_hosts(CHANNEL_VFX_V1, &vfx_bytes, &mut vfx, &mut anim).unwrap(),
        ClientCustomChannelEvent::Vfx(vfx_bytes)
    );
    let output = vfx.frame_output([0.0, 0.0, 1.0]);
    assert_eq!(output.lights.len(), 1);
    assert_eq!(output.lights[0].pos, [1.0, 2.0, 3.0]);

    let anim_bytes = anim_payload(anim.clip_id("test", "animation.test.move").unwrap());
    assert_eq!(
        decode_custom_payload_packet(&framed(CHANNEL_ANIM_V1, &anim_bytes)).unwrap(),
        ClientCustomChannelEvent::Anim(anim_bytes.clone())
    );
    assert_eq!(
        dispatch_custom_channel_to_hosts(CHANNEL_ANIM_V1, &anim_bytes, &mut vfx, &mut anim)
            .unwrap(),
        ClientCustomChannelEvent::Anim(anim_bytes)
    );
    anim.advance(0.5).unwrap();
    assert_ne!(anim.evaluate_entity_pose(77).unwrap().unwrap(), before);
}

#[test]
fn mag1_cast_variants_decode_exact_wire_fields() {
    for (tag, expected) in [
        (
            1,
            DecodedMagicCastState::CastTime {
                total_ticks: 400,
                elapsed_ticks: 125,
            },
        ),
        (
            2,
            DecodedMagicCastState::Channeled {
                interval_ticks: 5,
                max_ticks: 200,
                elapsed_ticks: 10,
                mana_per_pulse: 2.5,
            },
        ),
    ] {
        let bytes = magic_payload(tag);
        let event = decode_custom_payload_packet(&framed(CHANNEL_MAGIC_V1, &bytes)).unwrap();
        assert_eq!(
            event,
            decode_custom_channel_event(CHANNEL_MAGIC_V1, &bytes).unwrap()
        );
        let ClientCustomChannelEvent::Magic(sync) = event else {
            panic!("expected decoded MAG1 event");
        };
        assert_eq!((sync.mana, sync.max_mana), (7.5, 20.0));
        let cast = sync.cast.unwrap();
        assert_eq!(cast.spell_id, "spell:flame");
        assert_eq!(cast.state, expected);
        assert_eq!(sync.cooldowns["spell:flame"], 200);
        assert_eq!(sync.cooldowns["spell:ice"], u32::MAX);
        assert_eq!(sync.statuses.len(), 1);
        let status = &sync.statuses[0];
        assert_eq!((&*status.id, status.amplifier), ("status:shield", 3));
        assert_eq!(
            (status.remaining_ticks, status.absorption_remaining),
            (150, 3.25)
        );
    }
    let ClientCustomChannelEvent::Magic(sync) =
        decode_custom_channel_event(CHANNEL_MAGIC_V1, &magic_payload(0)).unwrap()
    else {
        panic!("expected MAG1 event");
    };
    assert!(sync.cast.is_none());
}

#[test]
fn mot1_hand_and_phase_variants_decode_fixed_width_big_endian_fields() {
    for (hand_byte, hand) in [
        (0, DecodedMotionHand::MainHand),
        (1, DecodedMotionHand::OffHand),
    ] {
        for (phase_byte, phase) in [
            (0, DecodedMotionPhase::Idle),
            (1, DecodedMotionPhase::Anticipation),
            (2, DecodedMotionPhase::ActiveHitWindow),
            (3, DecodedMotionPhase::Recovery),
        ] {
            let bytes = motion_payload(hand_byte, phase_byte);
            let event = decode_custom_payload_packet(&framed(CHANNEL_MOTION_V1, &bytes)).unwrap();
            let ClientCustomChannelEvent::Motion(motion) = event else {
                panic!("expected MOT1 event");
            };
            assert_eq!(motion.entity_id, 0x0123_4567_89ab_cdef);
            assert_eq!(motion.action_id, 0x1234_5678);
            assert_eq!(motion.combo_index, 257);
            assert_eq!(motion.hand, hand);
            assert_eq!(motion.speed_multiplier, 1.5);
            assert_eq!(motion.elapsed_ticks, 12345);
            assert_eq!(motion.phase, phase);
        }
    }
}

#[test]
fn dispatch_passes_through_decoded_magic_motion_and_unknown_channels() {
    let mut vfx = ClientVfxHost::new(GpuPipelineKind::ClassicLowSpec);
    let mut anim = WorldAnimationHost::new();
    for (channel, bytes) in [
        (CHANNEL_MAGIC_V1, magic_payload(1)),
        (CHANNEL_MOTION_V1, motion_payload(1, 2)),
    ] {
        assert_eq!(
            dispatch_custom_channel_to_hosts(channel, &bytes, &mut vfx, &mut anim).unwrap(),
            decode_custom_channel_event(channel, &bytes).unwrap()
        );
    }

    let bytes = b"\0arbitrary\xff";
    let expected = ClientCustomChannelEvent::Unknown {
        channel: "example:unrecognized".into(),
        payload: bytes.to_vec(),
    };
    assert_eq!(
        decode_custom_payload_packet(&framed("example:unrecognized", bytes)).unwrap(),
        expected
    );
    assert_eq!(
        dispatch_custom_channel_to_hosts("example:unrecognized", bytes, &mut vfx, &mut anim)
            .unwrap(),
        expected
    );
    assert!(vfx.frame_output([0.0, 0.0, 1.0]).lights.is_empty());
}

#[test]
fn all_recognized_channels_reject_truncation_trailing_nan_and_wrong_magic() {
    let inputs = [
        (CHANNEL_VFX_V1, vfx_payload()),
        (CHANNEL_ANIM_V1, anim_payload(0)),
        (CHANNEL_MAGIC_V1, magic_payload(1)),
        (CHANNEL_MOTION_V1, motion_payload(0, 1)),
    ];
    let mut vfx = ClientVfxHost::new(GpuPipelineKind::ClassicLowSpec);
    let mut anim = WorldAnimationHost::new();
    for (channel, valid) in inputs {
        let mut bad_magic = valid.clone();
        bad_magic[0] = b'X';
        let mut extra = valid.clone();
        extra.push(0);
        let mut nan = valid.clone();
        let nan_offset = match channel {
            CHANNEL_VFX_V1 => 10,    // First SpawnLight position component, little endian.
            CHANNEL_ANIM_V1 => 11,   // Animation speed, little endian.
            CHANNEL_MAGIC_V1 => 4,   // Mana, little endian.
            CHANNEL_MOTION_V1 => 19, // Speed multiplier, big endian.
            _ => unreachable!(),
        };
        let nan_bytes = if channel == CHANNEL_MOTION_V1 {
            f32::NAN.to_be_bytes()
        } else {
            f32::NAN.to_le_bytes()
        };
        nan[nan_offset..nan_offset + 4].copy_from_slice(&nan_bytes);
        for invalid in [&bad_magic[..], &valid[..valid.len() - 1], &extra, &nan] {
            assert!(
                decode_custom_channel_event(channel, invalid).is_err(),
                "{channel} accepted malformed channel bytes"
            );
            assert!(
                dispatch_custom_channel_to_hosts(channel, invalid, &mut vfx, &mut anim).is_err(),
                "{channel} dispatched malformed bytes"
            );
            assert!(
                decode_custom_payload_packet(&framed(channel, invalid)).is_err(),
                "{channel} accepted malformed packet body"
            );
        }
    }
    assert!(vfx.frame_output([0.0, 0.0, 1.0]).lights.is_empty());
    assert!(decode_custom_payload_packet(&[0x80]).is_err());
    assert!(decode_custom_payload_packet(&[10, b'x']).is_err());
}

#[test]
fn mag1_and_mot1_reject_invalid_tags_ranges_counts_and_order() {
    let mut magic = magic_payload(0);
    magic[12] = 255;
    assert!(decode_custom_channel_event(CHANNEL_MAGIC_V1, &magic).is_err());

    let mut zero_duration = b"MAG1".to_vec();
    zero_duration.extend_from_slice(&1f32.to_le_bytes());
    zero_duration.extend_from_slice(&2f32.to_le_bytes());
    zero_duration.push(1);
    write_string(&mut zero_duration, "spell:test");
    write_varint(&mut zero_duration, 0);
    write_varint(&mut zero_duration, 1);
    write_varint(&mut zero_duration, 0);
    write_varint(&mut zero_duration, 0);
    assert!(decode_custom_channel_event(CHANNEL_MAGIC_V1, &zero_duration).is_err());

    let mut excessive = b"MAG1".to_vec();
    excessive.extend_from_slice(&1f32.to_le_bytes());
    excessive.extend_from_slice(&2f32.to_le_bytes());
    excessive.push(0);
    write_varint(&mut excessive, 513);
    assert!(decode_custom_channel_event(CHANNEL_MAGIC_V1, &excessive).is_err());

    let mut unordered = b"MAG1".to_vec();
    unordered.extend_from_slice(&1f32.to_le_bytes());
    unordered.extend_from_slice(&2f32.to_le_bytes());
    unordered.push(0);
    write_varint(&mut unordered, 2);
    for id in ["spell:z", "spell:a"] {
        write_string(&mut unordered, id);
        write_varint(&mut unordered, 1);
    }
    write_varint(&mut unordered, 0);
    assert!(decode_custom_channel_event(CHANNEL_MAGIC_V1, &unordered).is_err());

    let valid = motion_payload(0, 1);
    for (offset, value) in [(18, 2), (27, 4)] {
        let mut invalid = valid.clone();
        invalid[offset] = value;
        assert!(decode_custom_channel_event(CHANNEL_MOTION_V1, &invalid).is_err());
    }
    for speed in [0.24f32, 4.01, f32::INFINITY] {
        let mut invalid = valid.clone();
        invalid[19..23].copy_from_slice(&speed.to_be_bytes());
        assert!(decode_custom_channel_event(CHANNEL_MOTION_V1, &invalid).is_err());
    }
}
