// SPDX-License-Identifier: LGPL-3.0-or-later
//! Create Aeronautics sub-world synchronization payloads.
//!
//! SW6D v1 uses canonical big-endian fields in this order:
//! `SW6D | u8 version | u16 count | records`, where each record is
//! `u64 id | 3*f64 position | 4*f32 quaternion (x,y,z,w) | 3*f32 velocity |
//! 3*f32 angular velocity | u8 flags`. A frame is a complete snapshot; ids
//! absent from it are removed. SHLM v1 is `SHLM | u8 version | u64 id |
//! 4*f32 controls | u8 flags`.

use std::collections::BTreeMap;

pub const SUBWORLD_SYNC_CHANNEL: &str = "megu:subworld_sync_v1";
pub const SUBWORLD_HELM_CHANNEL: &str = "megu:subworld_helm_v1";
pub const SUBWORLD_SYNC_MAGIC: [u8; 4] = *b"SW6D";
pub const SUBWORLD_HELM_MAGIC: [u8; 4] = *b"SHLM";
const VERSION: u8 = 1;
const HEADER_LEN: usize = 7;
const RECORD_LEN: usize = 73;
const MAX_RECORDS: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SubWorldPose {
    pub subworld_id: u64,
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

fn read<const N: usize>(bytes: &[u8], offset: &mut usize) -> Result<[u8; N], String> {
    let end = offset.checked_add(N).ok_or("SW6D offset overflow")?;
    let slice = bytes
        .get(*offset..end)
        .ok_or("truncated sub-world payload")?;
    *offset = end;
    Ok(slice.try_into().expect("slice length checked"))
}

fn finite_unit_quaternion(q: [f32; 4]) -> bool {
    if !q.iter().all(|v| v.is_finite()) {
        return false;
    }
    let norm2: f32 = q.iter().map(|v| v * v).sum();
    norm2.is_finite() && (norm2.sqrt() - 1.0).abs() <= 1.0e-3
}

/// Decode a complete, strictly bounded SW6D v1 snapshot.
pub fn decode_sync_frame(bytes: &[u8]) -> Result<SubWorldSyncFrame, String> {
    if bytes.len() < HEADER_LEN || bytes.get(..4) != Some(&SUBWORLD_SYNC_MAGIC) {
        return Err("invalid SW6D header".into());
    }
    if bytes[4] != VERSION {
        return Err(format!("unsupported SW6D version {}", bytes[4]));
    }
    let count = u16::from_be_bytes(bytes[5..7].try_into().expect("header length checked")) as usize;
    if count > MAX_RECORDS {
        return Err("SW6D record count exceeds limit".into());
    }
    let expected = HEADER_LEN
        .checked_add(count.checked_mul(RECORD_LEN).ok_or("SW6D size overflow")?)
        .ok_or("SW6D size overflow")?;
    if bytes.len() != expected {
        return Err("SW6D length does not match record count".into());
    }
    let mut offset = HEADER_LEN;
    let mut poses = Vec::with_capacity(count);
    let mut ids = std::collections::BTreeSet::new();
    for _ in 0..count {
        let subworld_id = u64::from_be_bytes(read(bytes, &mut offset)?);
        if !ids.insert(subworld_id) {
            return Err("duplicate SW6D subworld id".into());
        }
        let mut pos = [0.0; 3];
        for value in &mut pos {
            *value = f64::from_be_bytes(read(bytes, &mut offset)?);
        }
        let mut quat = [0.0; 4];
        for value in &mut quat {
            *value = f32::from_be_bytes(read(bytes, &mut offset)?);
        }
        let mut vel = [0.0; 3];
        for value in &mut vel {
            *value = f32::from_be_bytes(read(bytes, &mut offset)?);
        }
        let mut ang_vel = [0.0; 3];
        for value in &mut ang_vel {
            *value = f32::from_be_bytes(read(bytes, &mut offset)?);
        }
        let flags = read::<1>(bytes, &mut offset)?[0];
        if !pos.iter().all(|v| v.is_finite() && (*v as f32).is_finite())
            || !vel.iter().chain(ang_vel.iter()).all(|v| v.is_finite())
            || !finite_unit_quaternion(quat)
        {
            return Err("SW6D contains non-finite or invalid pose values".into());
        }
        poses.push(SubWorldPose {
            subworld_id,
            pos,
            quat,
            vel,
            ang_vel,
            flags,
        });
    }
    Ok(SubWorldSyncFrame { poses })
}

/// Encode a canonical big-endian SHLM v1 command. Control values are finite
/// and clamped to `[-1, 1]`; non-finite values become zero.
pub fn encode_helm_cmd(cmd: &SubWorldHelmCmd) -> Vec<u8> {
    let mut out = Vec::with_capacity(30);
    out.extend_from_slice(&SUBWORLD_HELM_MAGIC);
    out.push(VERSION);
    out.extend_from_slice(&cmd.subworld_id.to_be_bytes());
    for value in [
        cmd.throttle,
        cmd.yaw_rate,
        cmd.pitch_rate,
        cmd.altitude_delta,
    ] {
        let value = if value.is_finite() {
            value.clamp(-1.0, 1.0)
        } else {
            0.0
        };
        out.extend_from_slice(&value.to_be_bytes());
    }
    out.push(cmd.flags);
    out
}

/// Decode a strictly-sized SHLM v1 command (useful for tests and adapters).
pub fn decode_helm_cmd(bytes: &[u8]) -> Result<SubWorldHelmCmd, String> {
    if bytes.len() != 30 || bytes.get(..4) != Some(&SUBWORLD_HELM_MAGIC) || bytes[4] != VERSION {
        return Err("invalid SHLM payload".into());
    }
    let subworld_id = u64::from_be_bytes(bytes[5..13].try_into().expect("length checked"));
    let mut controls = [0.0; 4];
    for (index, value) in controls.iter_mut().enumerate() {
        let start = 13 + index * 4;
        *value = f32::from_be_bytes(bytes[start..start + 4].try_into().expect("length checked"));
        if !value.is_finite() || !(-1.0..=1.0).contains(value) {
            return Err("SHLM control is not finite or clamped".into());
        }
    }
    Ok(SubWorldHelmCmd {
        subworld_id,
        throttle: controls[0],
        yaw_rate: controls[1],
        pitch_rate: controls[2],
        altitude_delta: controls[3],
        flags: bytes[29],
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

    fn frame_bytes(poses: &[SubWorldPose]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&SUBWORLD_SYNC_MAGIC);
        out.push(VERSION);
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
        let source = frame_bytes(&[pose(7, 4.5, [0.0, 0.0, 0.0, 1.0])]);
        assert_eq!(decode_sync_frame(&source).unwrap().poses[0].subworld_id, 7);
        assert!(decode_sync_frame(&source[..source.len() - 1]).is_err());
        let mut corrupt = source.clone();
        corrupt[0] ^= 1;
        assert!(decode_sync_frame(&corrupt).is_err());
        let mut nan = source.clone();
        nan[15..23].copy_from_slice(&f64::NAN.to_be_bytes());
        assert!(decode_sync_frame(&nan).is_err());
        let mut non_unit = source;
        non_unit[39..43].copy_from_slice(&2.0f32.to_be_bytes());
        assert!(decode_sync_frame(&non_unit).is_err());
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
