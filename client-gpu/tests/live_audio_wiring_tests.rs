// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! End-to-end live-session sound routing through decoded chunk acoustics.

use mc_spine_client::protocol::{
    decode_chunk_packet, ClientPlayEvent, PlayEvent, TeleportSync, WireSoundEvent,
};
use mc_spine_client_gpu::live_session::LiveSession;

fn varint(bytes: &mut Vec<u8>, mut value: u32) {
    loop {
        let part = (value & 0x7f) as u8;
        value >>= 7;
        bytes.push(part | if value == 0 { 0 } else { 0x80 });
        if value == 0 {
            break;
        }
    }
}

fn decoded_chunk_with_wall(state: u32) -> mc_spine_client::protocol::DecodedChunk {
    // Protocol 767, -64..320, section y=0..15 filled with the given state.
    let mut sections = Vec::new();
    for section in 0..24 {
        sections.extend_from_slice(
            &(if section == 4 && state != 0 {
                4096i16
            } else {
                0
            })
            .to_be_bytes(),
        );
        sections.push(0); // single-valued block palette
        varint(&mut sections, if section == 4 { state } else { 0 });
        varint(&mut sections, 0); // no packed block values
        sections.push(0); // biome palette
        varint(&mut sections, 0);
        varint(&mut sections, 0);
    }
    let mut packet = Vec::new();
    packet.extend_from_slice(&0i32.to_be_bytes());
    packet.extend_from_slice(&0i32.to_be_bytes());
    packet.extend_from_slice(&[10, 0]); // empty unnamed heightmap compound
    varint(&mut packet, sections.len() as u32);
    packet.extend_from_slice(&sections);
    for _ in 0..7 {
        varint(&mut packet, 0); // block entities, light masks and arrays
    }
    decode_chunk_packet(&packet, 24, 64).expect("decoded real chunk")
}

fn sound_event() -> WireSoundEvent {
    WireSoundEvent {
        name: Some("minecraft:block.stone.break".to_string()),
        registry_id: None,
        category: 1,
        position: Some([18.0, 3.62, 8.0]),
        entity_id: None,
        volume: 2.0,
        pitch: 1.0,
        seed: 0,
        fixed_range: None,
    }
}

fn sound_loudness_through_chunk(state: u32) -> f32 {
    let mut session = LiveSession::offline_with_null_audio();
    assert_eq!(session.audio_sink_label(), "null");
    session
        .apply_event(ClientPlayEvent::Play(PlayEvent::Teleport(TeleportSync {
            x: -2.0,
            y: 2.0,
            z: 8.0,
            yaw: 0.0,
            pitch: 0.0,
            flags: 0,
            teleport_id: 1,
        })))
        .unwrap();
    session
        .apply_event(ClientPlayEvent::Play(PlayEvent::ChunkData(Box::new(
            decoded_chunk_with_wall(state),
        ))))
        .unwrap();
    assert_eq!(session.world.loaded_chunks(), 1);

    session.ingest_sound_event(sound_event());
    session.pump_audio_frames(480);
    let cues = session.drain_subtitle_cues();
    assert_eq!(cues.len(), 1, "live sound must feed the subtitle UI");
    assert!(cues[0].loudness.is_finite());
    assert!(cues[0].distance_m > 19.0);
    assert!(session.drain_subtitle_cues().is_empty());
    assert_eq!(session.audio_stats().events_submitted, 1);
    assert_eq!(session.audio_stats().subtitle_cues, 1);
    cues[0].loudness
}

#[test]
fn decoded_stone_wall_attenuates_spatial_audio_and_emits_subtitle() {
    let open_air = sound_loudness_through_chunk(0);
    let stone = sound_loudness_through_chunk(1);
    assert!(open_air > 0.0, "open-air sound must be audible");
    assert!(
        stone < open_air * 0.85,
        "stone chunk must occlude sound, open={open_air}, stone={stone}"
    );
}

#[test]
fn tactile_environment_hint_reaches_live_audio_mix() {
    let mut session = LiveSession::offline_with_null_audio();
    session.step_tactile_frame(0.05, 18_000, true, true, 0, 1.0, true);
    let hint = session.latest_audio_hint();
    assert_eq!(session.audio_environment_hint(), hint);
    assert!(hint.underwater_lowpass > 0.0);
    assert!(hint.cave_reverb_mix > 0.0);
    assert!(hint.rain_eaves_occlusion > 0.0);

    session.step_tactile_frame(0.05, 18_001, true, false, 15, 0.0, false);
    assert_eq!(
        session.audio_environment_hint(),
        session.latest_audio_hint()
    );
    assert_eq!(session.audio_environment_hint().underwater_lowpass, 0.0);
}

#[test]
fn entity_sound_uses_tracked_position() {
    let mut session = LiveSession::offline_with_null_audio();
    session.set_entity_position(67, [12.0, 4.0, 8.0]);
    let mut event = sound_event();
    event.position = None;
    event.entity_id = Some(67);
    session.ingest_sound_event(event);
    session.pump_audio_frames(480);
    let cues = session.drain_subtitle_cues();
    assert_eq!(cues.len(), 1);
    assert!(cues[0].distance_m > 10.0);
    assert_eq!(session.audio_stats().entity_pos_unresolved, 0);
}
