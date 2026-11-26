// SPDX-License-Identifier: LGPL-3.0-or-later
//! Create Aeronautics sub-world synchronization payloads.
//!
//! The wire codec is the canonical implementation in
//! `megu_proto::subworld_wire`: `SW6D` v2 carries counted baselines and
//! single-record spawn/pose/despawn deltas with dimension, revision, pivot
//! and voxel-hull metadata, while `SHLM` v2 carries the versioned helm
//! command. This module keeps the GPU client's small public compatibility
//! surface and converts canonical records into render-ready poses.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use megu_proto::subworld_wire::{
    decode_shlm_command, decode_sw6d_frame, encode_shlm_command, pivot_adjusted_position,
    sanitize_shlm_command, SubworldFrameMode, SubworldHelmCommand,
    SHLM_MAGIC as CANONICAL_SHLM_MAGIC, SW6D_MAGIC as CANONICAL_SW6D_MAGIC,
};

pub const SUBWORLD_SYNC_CHANNEL: &str = "megu:subworld_sync_v1";
pub const SUBWORLD_HELM_CHANNEL: &str = "megu:subworld_helm_v1";
pub const SUBWORLD_SYNC_MAGIC: [u8; 4] = CANONICAL_SW6D_MAGIC;
pub const SUBWORLD_HELM_MAGIC: [u8; 4] = CANONICAL_SHLM_MAGIC;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SubWorldPose {
    pub subworld_id: u64,
    /// Render-origin position (`center of mass - rotated pivot`). This is the
    /// translation the voxel-grid model matrix needs; the center-of-mass
    /// position itself remains available through [`decode_sync_frame_detailed`].
    pub pos: [f64; 3],
    pub quat: [f32; 4],
    pub vel: [f32; 3],
    pub ang_vel: [f32; 3],
    pub flags: u8,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SubWorldSyncFrame {
    pub poses: Vec<SubWorldPose>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SubWorldHelmCmd {
    pub subworld_id: u64,
    pub throttle: f32,
    pub yaw_rate: f32,
    pub pitch_rate: f32,
    pub altitude_delta: f32,
    pub flags: u8,
}

static HELM_SEQ: AtomicU64 = AtomicU64::new(0);

/// Decode the canonical wire frame without discarding its mode, dimension,
/// revision, pivot or voxel-hull payload.
pub fn decode_sync_frame_detailed(
    bytes: &[u8],
) -> Result<megu_proto::subworld_wire::SubworldSyncFrame, String> {
    decode_sw6d_frame(bytes)
}

fn render_pose(record: &megu_proto::subworld_wire::SubworldPoseRecord) -> SubWorldPose {
    SubWorldPose {
        subworld_id: record.subworld_id,
        pos: pivot_adjusted_position(record.pos, record.quat, record.pivot),
        quat: record.quat,
        vel: record.vel,
        ang_vel: record.ang_vel,
        flags: record.flags,
    }
}

/// Decode one `SW6D` frame into render-ready poses. Complete baselines,
/// spawns and pose deltas decode through the same canonical wire contract;
/// despawn deltas intentionally produce no live pose. Legacy v1 counted
/// snapshots remain accepted by the shared decoder.
pub fn decode_sync_frame(bytes: &[u8]) -> Result<SubWorldSyncFrame, String> {
    let frame = decode_sync_frame_detailed(bytes)?;
    if frame.mode == SubworldFrameMode::Despawn {
        return Ok(SubWorldSyncFrame { poses: Vec::new() });
    }
    Ok(SubWorldSyncFrame {
        poses: frame.records.iter().map(render_pose).collect(),
    })
}

/// Encode a canonical big-endian `SHLM` v2 command. Control values are finite
/// and clamped to `[-1, 1]`; non-finite values become zero. The compatibility
/// struct predates explicit pitch/roll separation and sequence numbers, so
/// roll is neutral and a process-local monotonically increasing sequence is
/// stamped on every frame.
pub fn encode_helm_cmd(cmd: &SubWorldHelmCmd) -> Vec<u8> {
    let canonical = sanitize_shlm_command(SubworldHelmCommand {
        subworld_id: cmd.subworld_id,
        seq: HELM_SEQ.fetch_add(1, Ordering::Relaxed).saturating_add(1),
        throttle: cmd.throttle,
        yaw: cmd.yaw_rate,
        pitch: cmd.pitch_rate,
        roll: 0.0,
        vertical: cmd.altitude_delta,
        flags: cmd.flags,
    });
    encode_shlm_command(&canonical).unwrap_or_default()
}

/// Decode a canonical `SHLM` v2 command into the GPU client's compatibility
/// control shape (useful for tests and adapters). The wire sequence number is
/// validated as part of the frame but intentionally not exposed by this old
/// struct.
pub fn decode_helm_cmd(bytes: &[u8]) -> Result<SubWorldHelmCmd, String> {
    let cmd = decode_shlm_command(bytes)?;
    Ok(SubWorldHelmCmd {
        subworld_id: cmd.subworld_id,
        throttle: cmd.throttle,
        yaw_rate: cmd.yaw,
        pitch_rate: cmd.pitch,
        altitude_delta: cmd.vertical,
        flags: cmd.flags,
    })
}

#[derive(Clone, Copy, Debug)]
struct PosePair {
    previous: SubWorldPose,
    current: SubWorldPose,
}

/// Snapshot-driven pose history for render-rate interpolation of server ticks.
#[derive(Clone, Debug, Default)]
pub struct SubWorldPoseInterpolator {
    poses: BTreeMap<u64, PosePair>,
}

impl SubWorldPoseInterpolator {
    pub fn ingest(&mut self, frame: &SubWorldSyncFrame) {
        let mut next = BTreeMap::new();
        for pose in &frame.poses {
            let previous = self
                .poses
                .get(&pose.subworld_id)
                .map_or(*pose, |old| old.current);
            next.insert(
                pose.subworld_id,
                PosePair {
                    previous,
                    current: *pose,
                },
            );
        }
        self.poses = next;
    }

    pub fn poses(&self) -> impl Iterator<Item = (u64, SubWorldPose)> + '_ {
        self.poses.iter().map(|(id, pair)| (*id, pair.current))
    }

    pub fn interpolate(&self, id: u64, alpha: f32) -> Option<SubWorldPose> {
        let pair = self.poses.get(&id)?;
        let t = if alpha.is_finite() {
            alpha.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let mut pose = pair.current;
        for i in 0..3 {
            pose.pos[i] =
                pair.previous.pos[i] + (pair.current.pos[i] - pair.previous.pos[i]) * t as f64;
            pose.vel[i] = lerp(pair.previous.vel[i], pair.current.vel[i], t);
            pose.ang_vel[i] = lerp(pair.previous.ang_vel[i], pair.current.ang_vel[i], t);
        }
        pose.quat = slerp(pair.previous.quat, pair.current.quat, t);
        Some(pose)
    }
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

fn slerp(a: [f32; 4], mut b: [f32; 4], t: f32) -> [f32; 4] {
    let mut dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    if dot < 0.0 {
        dot = -dot;
        for value in &mut b {
            *value = -*value;
        }
    }
    if dot > 0.9995 {
        let mut q = [0.0; 4];
        for i in 0..4 {
            q[i] = lerp(a[i], b[i], t);
        }
        let norm = q.iter().map(|v| v * v).sum::<f32>().sqrt();
        for value in &mut q {
            *value /= norm;
        }
        return q;
    }
    let theta = dot.clamp(-1.0, 1.0).acos();
    let sin_theta = theta.sin();
    let wa = ((1.0 - t) * theta).sin() / sin_theta;
    let wb = (t * theta).sin() / sin_theta;
    let mut q = [0.0; 4];
    for i in 0..4 {
        q[i] = a[i] * wa + b[i] * wb;
    }
    q
}

#[cfg(test)]
mod tests {
    use super::*;
    use megu_proto::subworld_wire::{
        encode_sw6d_frame, SubworldPoseRecord, SubworldSyncFrame as WireFrame,
    };

    fn wire_frame_bytes(poses: &[SubWorldPose]) -> Vec<u8> {
        let records = poses
            .iter()
            .map(|pose| SubworldPoseRecord {
                subworld_id: pose.subworld_id,
                dimension_id: 0,
                revision: 1,
                pos: pose.pos,
                quat: pose.quat,
                vel: pose.vel,
                ang_vel: pose.ang_vel,
                pivot: [0.0; 3],
                flags: pose.flags,
                voxels: Vec::new(),
            })
            .collect();
        encode_sw6d_frame(&WireFrame {
            mode: SubworldFrameMode::Baseline,
            records,
        })
        .expect("test frame encodes")
    }

    fn legacy_v1_frame_bytes(poses: &[SubWorldPose]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&SUBWORLD_SYNC_MAGIC);
        out.push(1);
        out.extend_from_slice(&(poses.len() as u16).to_be_bytes());
        for pose in poses {
            out.extend_from_slice(&pose.subworld_id.to_be_bytes());
            for value in pose.pos {
                out.extend_from_slice(&value.to_be_bytes());
            }
            for value in pose.quat {
                out.extend_from_slice(&value.to_be_bytes());
            }
            for value in pose.vel {
                out.extend_from_slice(&value.to_be_bytes());
            }
            for value in pose.ang_vel {
                out.extend_from_slice(&value.to_be_bytes());
            }
            out.push(pose.flags);
        }
        out
    }

    fn pose(id: u64, x: f64, quat: [f32; 4]) -> SubWorldPose {
        SubWorldPose {
            subworld_id: id,
            pos: [x, 2.0, 3.0],
            quat,
            vel: [1.0; 3],
            ang_vel: [0.0; 3],
            flags: 0,
        }
    }

    #[test]
    fn sw6d_roundtrip_and_fail_closed_inputs() {
        let source = wire_frame_bytes(&[pose(7, 4.5, [0.0, 0.0, 0.0, 1.0])]);
        assert_eq!(decode_sync_frame(&source).unwrap().poses[0].subworld_id, 7);
        assert!(decode_sync_frame(&source[..source.len() - 1]).is_err());
        let mut corrupt = source.clone();
        corrupt[0] ^= 1;
        assert!(decode_sync_frame(&corrupt).is_err());
        let mut nan = source.clone();
        // First f64 position component in the v2 record.
        nan[28..36].copy_from_slice(&f64::NAN.to_be_bytes());
        assert!(decode_sync_frame(&nan).is_err());
        let mut non_unit = source;
        // First quaternion x component after id/dimension/revision/position.
        non_unit[60..64].copy_from_slice(&2.0f32.to_be_bytes());
        assert!(decode_sync_frame(&non_unit).is_err());
    }

    #[test]
    fn legacy_counted_sw6d_v1_snapshot_still_decodes() {
        let source = legacy_v1_frame_bytes(&[pose(8, 6.5, [0.0, 0.0, 0.0, 1.0])]);
        let decoded = decode_sync_frame(&source).expect("legacy v1 snapshot");
        assert_eq!(decoded.poses[0].pos[0], 6.5);
    }

    #[test]
    fn shlm_roundtrip_clamps_and_sanitizes() {
        let cmd = SubWorldHelmCmd {
            subworld_id: 9,
            throttle: 4.0,
            yaw_rate: -2.0,
            pitch_rate: f32::NAN,
            altitude_delta: 0.5,
            flags: 3,
        };
        let bytes = encode_helm_cmd(&cmd);
        let decoded = decode_helm_cmd(&bytes).unwrap();
        assert_eq!(
            decoded,
            SubWorldHelmCmd {
                throttle: 1.0,
                yaw_rate: -1.0,
                pitch_rate: 0.0,
                ..cmd
            }
        );
        assert!(decode_helm_cmd(&bytes[..bytes.len() - 1]).is_err());
    }

    #[test]
    fn interpolator_lerps_translation_and_slerps_rotation() {
        let mut history = SubWorldPoseInterpolator::default();
        history.ingest(&SubWorldSyncFrame {
            poses: vec![pose(1, 0.0, [0.0, 0.0, 0.0, 1.0])],
        });
        history.ingest(&SubWorldSyncFrame {
            poses: vec![pose(1, 10.0, [0.0, 0.0, 1.0, 0.0])],
        });
        let middle = history.interpolate(1, 0.5).unwrap();
        assert!((middle.pos[0] - 5.0).abs() < 1e-9);
        assert!((middle.quat.iter().map(|v| v * v).sum::<f32>() - 1.0).abs() < 1e-5);
    }
}
