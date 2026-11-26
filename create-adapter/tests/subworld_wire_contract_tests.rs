// SPDX-License-Identifier: LGPL-3.0-or-later

use create_adapter::aeronautics::{SubWorldHelmInput, SubWorldSyncFrame, SubWorldSyncKind};

#[allow(dead_code)]
#[path = "../../proto/src/subworld_wire.rs"]
mod canonical;

fn server_frame(kind: SubWorldSyncKind, voxels: Vec<([i16; 3], u32)>) -> SubWorldSyncFrame {
    SubWorldSyncFrame {
        kind,
        subworld_id: 42,
        dim_id: "minecraft:overworld".into(),
        pos: [12.0, 70.0, -8.0],
        quat: [0.0, 0.0, 0.0, 1.0],
        vel: [1.0, 0.0, -1.0],
        ang_vel: [0.0, 0.25, 0.0],
        revision: 9,
        voxels,
    }
}

#[test]
fn server_spawn_pose_and_despawn_encode_canonical_sw6d_deltas() {
    let spawn = server_frame(
        SubWorldSyncKind::FullSpawn,
        vec![([0, 0, 0], 10), ([3, 0, 0], 11)],
    );
    let spawn_bytes = spawn.encode().expect("spawn encodes");
    let decoded = canonical::decode_sw6d_frame(&spawn_bytes).expect("canonical spawn decode");
    assert_eq!(decoded.mode, canonical::SubworldFrameMode::Spawn);
    assert_eq!(decoded.records.len(), 1);
    let record = &decoded.records[0];
    assert_eq!(record.subworld_id, 42);
    assert_eq!(record.dimension_id, 0);
    assert_eq!(record.revision, 9);
    assert_eq!(record.voxels, vec![([0, 0, 0], 10), ([3, 0, 0], 11)]);
    // Uniform two-cell hull: local COM is the mean of cell centers (2.0, .5, .5).
    assert_eq!(record.pivot, [2.0, 0.5, 0.5]);

    let pose = server_frame(SubWorldSyncKind::PoseDelta, Vec::new());
    let pose_bytes = pose.encode().expect("pose encodes");
    let pose_decoded = canonical::decode_sw6d_frame(&pose_bytes).expect("canonical pose decode");
    assert_eq!(pose_decoded.mode, canonical::SubworldFrameMode::Pose);
    assert_eq!(pose_decoded.records[0].pivot, [2.0, 0.5, 0.5]);
    assert!(pose_decoded.records[0].voxels.is_empty());

    let despawn = server_frame(SubWorldSyncKind::Despawn, Vec::new());
    let despawn_bytes = despawn.encode().expect("despawn encodes");
    let despawn_decoded =
        canonical::decode_sw6d_frame(&despawn_bytes).expect("canonical despawn decode");
    assert_eq!(despawn_decoded.mode, canonical::SubworldFrameMode::Despawn);
}

#[test]
fn server_and_client_helm_shapes_decode_through_one_canonical_contract() {
    let input = SubWorldHelmInput {
        subworld_id: 42,
        throttle: 0.75,
        pitch: 0.25,
        yaw: -0.5,
        roll: 0.125,
        buoyancy_delta: -0.75,
        disassemble: true,
    };
    let server_bytes = input.encode().expect("server helm encodes");
    let canonical_decoded =
        canonical::decode_shlm_command(&server_bytes).expect("canonical decode");
    assert_eq!(canonical_decoded.subworld_id, 42);
    assert_eq!(canonical_decoded.throttle, 0.75);
    assert_eq!(canonical_decoded.yaw, -0.5);
    assert_eq!(canonical_decoded.pitch, 0.25);
    assert_eq!(canonical_decoded.roll, 0.125);
    assert_eq!(canonical_decoded.vertical, -0.75);
    assert_ne!(
        canonical_decoded.flags & canonical::SHLM_FLAG_DISASSEMBLE,
        0
    );

    // This is the field mapping the GPU client writes: yaw/pitch rates occupy
    // canonical yaw/pitch, altitude delta occupies vertical, roll is neutral.
    let client_bytes = canonical::encode_shlm_command(&canonical::SubworldHelmCommand {
        subworld_id: 42,
        seq: 77,
        throttle: 0.5,
        yaw: 0.25,
        pitch: -0.25,
        roll: 0.0,
        vertical: 1.0,
        flags: 0,
    })
    .expect("client-shaped command encodes");
    let server_decoded =
        SubWorldHelmInput::decode(&client_bytes).expect("server decodes client frame");
    assert_eq!(server_decoded.subworld_id, 42);
    assert_eq!(server_decoded.throttle, 0.5);
    assert_eq!(server_decoded.yaw, 0.25);
    assert_eq!(server_decoded.pitch, -0.25);
    assert_eq!(server_decoded.roll, 0.0);
    assert_eq!(server_decoded.buoyancy_delta, 1.0);
    assert!(!server_decoded.disassemble);
}
