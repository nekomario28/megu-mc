// SPDX-License-Identifier: LGPL-3.0-or-later

use megu_proto::subworld_wire::{
    decode_shlm_command, decode_sw6d_frame, encode_shlm_command, encode_sw6d_frame,
    legacy_dimension_id, pivot_adjusted_position, SubworldFrameMode, SubworldHelmCommand,
    SubworldPoseRecord, SubworldSyncFrame, MAX_SW6D_RECORDS, SHLM_FLAG_DISASSEMBLE,
};

fn record(id: u64) -> SubworldPoseRecord {
    SubworldPoseRecord {
        subworld_id: id,
        dimension_id: 3,
        revision: 1_000_000_000_000,
        pos: [10.0, 64.0, -20.0],
        quat: [0.0, 0.0, 0.0, 1.0],
        vel: [1.0, 2.0, 3.0],
        ang_vel: [0.1, 0.2, 0.3],
        pivot: [2.5, 0.5, -1.0],
        flags: 7,
        voxels: vec![([0, 0, 0], 42), ([4, 0, -1], 43)],
    }
}

#[test]
fn sw6d_v2_baseline_roundtrip_preserves_hull_metadata_and_layout() {
    let frame = SubworldSyncFrame {
        mode: SubworldFrameMode::Baseline,
        records: vec![record(9)],
    };
    let bytes = encode_sw6d_frame(&frame).expect("encode");
    assert_eq!(&bytes[..8], b"SW6D\x02\x00\x00\x01");
    assert_eq!(u64::from_be_bytes(bytes[8..16].try_into().unwrap()), 9);
    assert_eq!(u32::from_be_bytes(bytes[16..20].try_into().unwrap()), 3);
    assert_eq!(
        u64::from_be_bytes(bytes[20..28].try_into().unwrap()),
        1_000_000_000_000
    );
    assert_eq!(decode_sw6d_frame(&bytes).expect("decode"), frame);
    assert!(decode_sw6d_frame(&bytes[..bytes.len() - 1]).is_err());
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(decode_sw6d_frame(&trailing).is_err());
}

#[test]
fn sw6d_v2_delta_contracts_and_large_count_fail_closed() {
    for mode in [
        SubworldFrameMode::Spawn,
        SubworldFrameMode::Pose,
        SubworldFrameMode::Despawn,
    ] {
        let mut delta = record(4);
        if mode != SubworldFrameMode::Spawn {
            delta.voxels.clear();
        }
        let frame = SubworldSyncFrame {
            mode,
            records: vec![delta],
        };
        assert_eq!(
            decode_sw6d_frame(&encode_sw6d_frame(&frame).unwrap()).unwrap(),
            frame
        );
    }

    let pose_with_voxels = SubworldSyncFrame {
        mode: SubworldFrameMode::Pose,
        records: vec![record(4)],
    };
    assert!(encode_sw6d_frame(&pose_with_voxels).is_err());
    let empty_delta = SubworldSyncFrame {
        mode: SubworldFrameMode::Pose,
        records: Vec::new(),
    };
    assert!(encode_sw6d_frame(&empty_delta).is_err());

    let oversized = SubworldSyncFrame {
        mode: SubworldFrameMode::Baseline,
        records: (0..=MAX_SW6D_RECORDS)
            .map(|id| {
                let mut r = record(id as u64 + 1);
                r.voxels.clear();
                r
            })
            .collect(),
    };
    assert!(encode_sw6d_frame(&oversized).is_err());
}

#[test]
fn legacy_sw6d_v1_counted_snapshot_decodes_as_baseline() {
    let mut bytes = b"SW6D\x01\x00\x01".to_vec();
    bytes.extend_from_slice(&77u64.to_be_bytes());
    for value in [4.0f64, 5.0, 6.0] {
        bytes.extend_from_slice(&value.to_be_bytes());
    }
    for value in [0.0f32, 0.0, 0.0, 1.0] {
        bytes.extend_from_slice(&value.to_be_bytes());
    }
    for _ in 0..6 {
        bytes.extend_from_slice(&0.0f32.to_be_bytes());
    }
    bytes.push(0);
    let decoded = decode_sw6d_frame(&bytes).expect("legacy v1");
    assert_eq!(decoded.mode, SubworldFrameMode::Baseline);
    assert_eq!(decoded.records[0].subworld_id, 77);
    assert_eq!(decoded.records[0].dimension_id, 0);
    assert_eq!(decoded.records[0].pivot, [0.0; 3]);
}

#[test]
fn pivot_adjusted_position_uses_rotated_center_of_mass_offset() {
    // Identity rotation: model origin = COM world position - local COM pivot.
    assert_eq!(
        pivot_adjusted_position([10.0, 20.0, 30.0], [0.0, 0.0, 0.0, 1.0], [2.0, 3.0, 4.0]),
        [8.0, 17.0, 26.0]
    );
    // 180 degrees about Y flips local X/Z before subtraction.
    assert_eq!(
        pivot_adjusted_position([10.0, 20.0, 30.0], [0.0, 1.0, 0.0, 0.0], [2.0, 3.0, 4.0]),
        [12.0, 17.0, 34.0]
    );
}

#[test]
fn shlm_v2_roundtrip_preserves_seq_controls_and_flags() {
    let cmd = SubworldHelmCommand {
        subworld_id: 91,
        seq: 0x0102_0304_0506_0708,
        throttle: 1.0,
        yaw: -0.5,
        pitch: 0.25,
        roll: -0.125,
        vertical: 0.75,
        flags: SHLM_FLAG_DISASSEMBLE,
    };
    let bytes = encode_shlm_command(&cmd).expect("encode");
    assert_eq!(bytes.len(), 42);
    assert_eq!(&bytes[..5], b"SHLM\x02");
    assert_eq!(u64::from_be_bytes(bytes[5..13].try_into().unwrap()), 91);
    assert_eq!(
        u64::from_be_bytes(bytes[13..21].try_into().unwrap()),
        0x0102_0304_0506_0708
    );
    assert_eq!(decode_shlm_command(&bytes).expect("decode"), cmd);
    assert!(decode_shlm_command(&bytes[..bytes.len() - 1]).is_err());
    let mut bad = cmd;
    bad.throttle = 1.5;
    assert!(encode_shlm_command(&bad).is_err());
}

#[test]
fn legacy_dimension_fallback_pins_vanilla_slots() {
    assert_eq!(legacy_dimension_id("minecraft:overworld"), 0);
    assert_eq!(legacy_dimension_id("minecraft:the_nether"), 1);
    assert_eq!(legacy_dimension_id("minecraft:the_end"), 2);
    assert_ne!(legacy_dimension_id("ad_astra:moon"), 0);
    assert!(legacy_dimension_id("ad_astra:moon") & 0x8000_0000 != 0);
}
