// SPDX-License-Identifier: LGPL-3.0-or-later
//! Canonical 6-DOF sub-world wire codecs.
//!
//! `SW6D` v2 is one counted, versioned frame format used for both complete
//! baselines and single-record spawn/pose/despawn deltas:
//!
//! ```text
//! SW6D | u8 version(2) | u8 mode | u16 record_count | records...
//! record = u64 subworld_id | u32 dimension_id | u64 revision
//!        | 3*f64 position | 4*f32 quaternion(x,y,z,w)
//!        | 3*f32 velocity | 3*f32 angular_velocity | 3*f32 pivot
//!        | u8 flags | u32 voxel_count | voxel_count * (3*i16 offset + u32 state)
//! ```
//!
//! Modes are `0` baseline (a complete counted snapshot), `1` spawn, `2` pose
//! and `3` despawn. Delta modes carry exactly one record. Pose/despawn
//! records carry no voxel payload; spawn and baseline records may carry the
//! hull voxels. All integers are big-endian. `SW6D` v1 counted pose snapshots
//! are accepted as legacy input so older client tests/tools fail soft rather
//! than silently dropping the channel.
//!
//! `SHLM` v2 is the single canonical helm command:
//! `SHLM | u8 version(2) | u64 subworld_id | u64 seq | f32 throttle |
//! f32 yaw | f32 pitch | f32 roll | f32 vertical | u8 flags`.

pub const SW6D_MAGIC: [u8; 4] = *b"SW6D";
pub const SW6D_VERSION_V1: u8 = 1;
pub const SW6D_VERSION: u8 = 2;
pub const SHLM_MAGIC: [u8; 4] = *b"SHLM";
pub const SHLM_VERSION: u8 = 2;
pub const SHLM_FLAG_DISASSEMBLE: u8 = 0x01;
pub const MAX_SW6D_RECORDS: usize = 4_096;
pub const MAX_SW6D_VOXELS: usize = 65_536;

const SW6D_HEADER_LEN: usize = 4 + 1 + 1 + 2;
const SW6D_RECORD_BASE_LEN: usize = 8 + 4 + 8 + 3 * 8 + 4 * 4 + 3 * 4 + 3 * 4 + 3 * 4 + 1 + 4;
const SW6D_VOXEL_LEN: usize = 3 * 2 + 4;
const SHLM_LEN: usize = 4 + 1 + 8 + 8 + 5 * 4 + 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubworldFrameMode {
    Baseline,
    Spawn,
    Pose,
    Despawn,
}

impl SubworldFrameMode {
    fn wire(self) -> u8 {
        match self {
            Self::Baseline => 0,
            Self::Spawn => 1,
            Self::Pose => 2,
            Self::Despawn => 3,
        }
    }

    fn from_wire(value: u8) -> Result<Self, String> {
        match value {
            0 => Ok(Self::Baseline),
            1 => Ok(Self::Spawn),
            2 => Ok(Self::Pose),
            3 => Ok(Self::Despawn),
            _ => Err("unknown SW6D frame mode".into()),
        }
    }

    pub fn is_delta(self) -> bool {
        self != Self::Baseline
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SubworldPoseRecord {
    pub subworld_id: u64,
    pub dimension_id: u32,
    pub revision: u64,
    /// Center-of-mass position in world space.
    pub pos: [f64; 3],
    pub quat: [f32; 4],
    pub vel: [f32; 3],
    pub ang_vel: [f32; 3],
    /// Center-of-mass pivot in the hull's local voxel coordinates.
    pub pivot: [f32; 3],
    pub flags: u8,
    pub voxels: Vec<([i16; 3], u32)>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SubworldSyncFrame {
    pub mode: SubworldFrameMode,
    pub records: Vec<SubworldPoseRecord>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SubworldHelmCommand {
    pub subworld_id: u64,
    pub seq: u64,
    pub throttle: f32,
    pub yaw: f32,
    pub pitch: f32,
    pub roll: f32,
    pub vertical: f32,
    pub flags: u8,
}

fn read_exact<const N: usize>(bytes: &[u8], cursor: &mut usize) -> Result<[u8; N], String> {
    let end = cursor.checked_add(N).ok_or("wire offset overflow")?;
    let slice = bytes.get(*cursor..end).ok_or("truncated wire payload")?;
    *cursor = end;
    slice
        .try_into()
        .map_err(|_| "truncated wire payload".to_string())
}

fn read_f32(bytes: &[u8], cursor: &mut usize) -> Result<f32, String> {
    Ok(f32::from_be_bytes(read_exact(bytes, cursor)?))
}

fn read_f64(bytes: &[u8], cursor: &mut usize) -> Result<f64, String> {
    Ok(f64::from_be_bytes(read_exact(bytes, cursor)?))
}

fn finite_unit_quaternion(quat: [f32; 4]) -> bool {
    if !quat.iter().all(|v| v.is_finite()) {
        return false;
    }
    let norm = quat
        .iter()
        .map(|v| f64::from(*v) * f64::from(*v))
        .sum::<f64>()
        .sqrt();
    norm.is_finite() && (norm - 1.0).abs() <= 1.0e-3
}

fn validate_pose(record: &SubworldPoseRecord, allow_voxels: bool) -> Result<(), String> {
    if !record
        .pos
        .iter()
        .all(|v| v.is_finite() && (*v as f32).is_finite())
        || !record
            .vel
            .iter()
            .chain(record.ang_vel.iter())
            .all(|v| v.is_finite())
        || !record.pivot.iter().all(|v| v.is_finite())
        || !finite_unit_quaternion(record.quat)
    {
        return Err("SW6D contains non-finite or invalid pose values".into());
    }
    if record.voxels.len() > MAX_SW6D_VOXELS || (!allow_voxels && !record.voxels.is_empty()) {
        return Err("invalid SW6D voxel payload".into());
    }
    Ok(())
}

fn validate_frame(frame: &SubworldSyncFrame) -> Result<(), String> {
    if frame.records.len() > MAX_SW6D_RECORDS {
        return Err("SW6D record count exceeds limit".into());
    }
    if frame.mode.is_delta() && frame.records.len() != 1 {
        return Err("SW6D delta frames carry exactly one record".into());
    }
    let mut ids = std::collections::BTreeSet::new();
    for record in &frame.records {
        if !ids.insert(record.subworld_id) {
            return Err("duplicate SW6D subworld id".into());
        }
        let allow_voxels = matches!(
            frame.mode,
            SubworldFrameMode::Baseline | SubworldFrameMode::Spawn
        );
        validate_pose(record, allow_voxels)?;
    }
    Ok(())
}

/// Encode one canonical `SW6D` v2 baseline or delta frame.
pub fn encode_sw6d_frame(frame: &SubworldSyncFrame) -> Result<Vec<u8>, String> {
    validate_frame(frame)?;
    let voxel_len: usize = frame
        .records
        .iter()
        .map(|r| r.voxels.len() * SW6D_VOXEL_LEN)
        .sum();
    let mut out = Vec::with_capacity(
        SW6D_HEADER_LEN + frame.records.len() * SW6D_RECORD_BASE_LEN + voxel_len,
    );
    out.extend_from_slice(&SW6D_MAGIC);
    out.push(SW6D_VERSION);
    out.push(frame.mode.wire());
    out.extend_from_slice(&(frame.records.len() as u16).to_be_bytes());
    for record in &frame.records {
        out.extend_from_slice(&record.subworld_id.to_be_bytes());
        out.extend_from_slice(&record.dimension_id.to_be_bytes());
        out.extend_from_slice(&record.revision.to_be_bytes());
        for value in record.pos {
            out.extend_from_slice(&value.to_be_bytes());
        }
        for value in record.quat {
            out.extend_from_slice(&value.to_be_bytes());
        }
        for value in record.vel {
            out.extend_from_slice(&value.to_be_bytes());
        }
        for value in record.ang_vel {
            out.extend_from_slice(&value.to_be_bytes());
        }
        for value in record.pivot {
            out.extend_from_slice(&value.to_be_bytes());
        }
        out.push(record.flags);
        out.extend_from_slice(&(record.voxels.len() as u32).to_be_bytes());
        for (offset, state) in &record.voxels {
            for value in offset {
                out.extend_from_slice(&value.to_be_bytes());
            }
            out.extend_from_slice(&state.to_be_bytes());
        }
    }
    Ok(out)
}

fn decode_sw6d_v2(bytes: &[u8]) -> Result<SubworldSyncFrame, String> {
    if bytes.len() < SW6D_HEADER_LEN {
        return Err("truncated SW6D header".into());
    }
    let mode = SubworldFrameMode::from_wire(bytes[5])?;
    let count = usize::from(u16::from_be_bytes(
        bytes[6..8].try_into().map_err(|_| "truncated SW6D count")?,
    ));
    if count > MAX_SW6D_RECORDS {
        return Err("SW6D record count exceeds limit".into());
    }
    let mut cursor = SW6D_HEADER_LEN;
    let mut records = Vec::with_capacity(count);
    for _ in 0..count {
        let subworld_id = u64::from_be_bytes(read_exact(bytes, &mut cursor)?);
        let dimension_id = u32::from_be_bytes(read_exact(bytes, &mut cursor)?);
        let revision = u64::from_be_bytes(read_exact(bytes, &mut cursor)?);
        let mut pos = [0.0; 3];
        for value in &mut pos {
            *value = read_f64(bytes, &mut cursor)?;
        }
        let mut quat = [0.0; 4];
        for value in &mut quat {
            *value = read_f32(bytes, &mut cursor)?;
        }
        let mut vel = [0.0; 3];
        for value in &mut vel {
            *value = read_f32(bytes, &mut cursor)?;
        }
        let mut ang_vel = [0.0; 3];
        for value in &mut ang_vel {
            *value = read_f32(bytes, &mut cursor)?;
        }
        let mut pivot = [0.0; 3];
        for value in &mut pivot {
            *value = read_f32(bytes, &mut cursor)?;
        }
        let flags = read_exact::<1>(bytes, &mut cursor)?[0];
        let voxel_count = u32::from_be_bytes(read_exact(bytes, &mut cursor)?) as usize;
        if voxel_count > MAX_SW6D_VOXELS
            || voxel_count
                .checked_mul(SW6D_VOXEL_LEN)
                .is_none_or(|len| len > bytes.len().saturating_sub(cursor))
        {
            return Err("invalid or oversized SW6D voxel count".into());
        }
        let mut voxels = Vec::with_capacity(voxel_count);
        for _ in 0..voxel_count {
            let mut offset = [0i16; 3];
            for value in &mut offset {
                *value = i16::from_be_bytes(read_exact(bytes, &mut cursor)?);
            }
            let state = u32::from_be_bytes(read_exact(bytes, &mut cursor)?);
            voxels.push((offset, state));
        }
        records.push(SubworldPoseRecord {
            subworld_id,
            dimension_id,
            revision,
            pos,
            quat,
            vel,
            ang_vel,
            pivot,
            flags,
            voxels,
        });
    }
    if cursor != bytes.len() {
        return Err("trailing SW6D bytes".into());
    }
    let frame = SubworldSyncFrame { mode, records };
    validate_frame(&frame)?;
    Ok(frame)
}

/// Decode the legacy v1 counted pose snapshot into canonical baseline records.
/// Missing pivot/dimension/revision/hull fields are zero defaults: v1 simply
/// did not carry them.
fn decode_sw6d_v1(bytes: &[u8]) -> Result<SubworldSyncFrame, String> {
    const LEGACY_HEADER_LEN: usize = 7;
    const LEGACY_RECORD_LEN: usize = 73;
    if bytes.len() < LEGACY_HEADER_LEN {
        return Err("truncated SW6D v1 header".into());
    }
    let count = usize::from(u16::from_be_bytes(
        bytes[5..7].try_into().map_err(|_| "truncated SW6D count")?,
    ));
    if count > MAX_SW6D_RECORDS || bytes.len() != LEGACY_HEADER_LEN + count * LEGACY_RECORD_LEN {
        return Err("SW6D v1 length does not match record count".into());
    }
    let mut cursor = LEGACY_HEADER_LEN;
    let mut records = Vec::with_capacity(count);
    let mut ids = std::collections::BTreeSet::new();
    for _ in 0..count {
        let subworld_id = u64::from_be_bytes(read_exact(bytes, &mut cursor)?);
        if !ids.insert(subworld_id) {
            return Err("duplicate SW6D subworld id".into());
        }
        let mut pos = [0.0; 3];
        for value in &mut pos {
            *value = read_f64(bytes, &mut cursor)?;
        }
        let mut quat = [0.0; 4];
        for value in &mut quat {
            *value = read_f32(bytes, &mut cursor)?;
        }
        let mut vel = [0.0; 3];
        for value in &mut vel {
            *value = read_f32(bytes, &mut cursor)?;
        }
        let mut ang_vel = [0.0; 3];
        for value in &mut ang_vel {
            *value = read_f32(bytes, &mut cursor)?;
        }
        let flags = read_exact::<1>(bytes, &mut cursor)?[0];
        let record = SubworldPoseRecord {
            subworld_id,
            dimension_id: 0,
            revision: 0,
            pos,
            quat,
            vel,
            ang_vel,
            pivot: [0.0; 3],
            flags,
            voxels: Vec::new(),
        };
        validate_pose(&record, false)?;
        records.push(record);
    }
    Ok(SubworldSyncFrame {
        mode: SubworldFrameMode::Baseline,
        records,
    })
}

/// Decode one canonical `SW6D` frame. Legacy v1 counted snapshots decode as
/// baselines; v2 decodes the exact mode and hull metadata written by
/// [`encode_sw6d_frame`]. Truncation, trailing bytes, duplicates, bad
/// quaternions and oversized voxel payloads all fail closed.
pub fn decode_sw6d_frame(bytes: &[u8]) -> Result<SubworldSyncFrame, String> {
    if bytes.len() < 6 || bytes.get(..4) != Some(&SW6D_MAGIC) {
        return Err("invalid SW6D header".into());
    }
    match bytes[4] {
        SW6D_VERSION_V1 => decode_sw6d_v1(bytes),
        SW6D_VERSION => decode_sw6d_v2(bytes),
        version => Err(format!("unsupported SW6D version {version}")),
    }
}

/// Validate one helm command. Controls are finite and within `[-1, 1]`.
pub fn validate_shlm_command(cmd: &SubworldHelmCommand) -> Result<(), String> {
    if ![cmd.throttle, cmd.yaw, cmd.pitch, cmd.roll, cmd.vertical]
        .iter()
        .all(|v| v.is_finite() && (-1.0..=1.0).contains(v))
    {
        return Err("SHLM controls must be finite and within -1..=1".into());
    }
    Ok(())
}

/// Encode a canonical `SHLM` v2 command, rejecting out-of-contract controls.
pub fn encode_shlm_command(cmd: &SubworldHelmCommand) -> Result<Vec<u8>, String> {
    validate_shlm_command(cmd)?;
    let mut out = Vec::with_capacity(SHLM_LEN);
    out.extend_from_slice(&SHLM_MAGIC);
    out.push(SHLM_VERSION);
    out.extend_from_slice(&cmd.subworld_id.to_be_bytes());
    out.extend_from_slice(&cmd.seq.to_be_bytes());
    for value in [cmd.throttle, cmd.yaw, cmd.pitch, cmd.roll, cmd.vertical] {
        out.extend_from_slice(&value.to_be_bytes());
    }
    out.push(cmd.flags);
    Ok(out)
}

/// Sanitize operator input into the clamped canonical command shape used by
/// the client sender: non-finite controls become zero and finite controls are
/// clamped to `[-1, 1]`.
pub fn sanitize_shlm_command(mut cmd: SubworldHelmCommand) -> SubworldHelmCommand {
    for value in [
        &mut cmd.throttle,
        &mut cmd.yaw,
        &mut cmd.pitch,
        &mut cmd.roll,
        &mut cmd.vertical,
    ] {
        *value = if value.is_finite() {
            value.clamp(-1.0, 1.0)
        } else {
            0.0
        };
    }
    cmd
}

/// Decode a strictly-sized canonical `SHLM` v2 command.
pub fn decode_shlm_command(bytes: &[u8]) -> Result<SubworldHelmCommand, String> {
    if bytes.len() != SHLM_LEN || bytes.get(..4) != Some(&SHLM_MAGIC) || bytes[4] != SHLM_VERSION {
        return Err("invalid SHLM payload".into());
    }
    let mut cursor = 5;
    let subworld_id = u64::from_be_bytes(read_exact(bytes, &mut cursor)?);
    let seq = u64::from_be_bytes(read_exact(bytes, &mut cursor)?);
    let mut controls = [0.0; 5];
    for value in &mut controls {
        *value = read_f32(bytes, &mut cursor)?;
    }
    let cmd = SubworldHelmCommand {
        subworld_id,
        seq,
        throttle: controls[0],
        yaw: controls[1],
        pitch: controls[2],
        roll: controls[3],
        vertical: controls[4],
        flags: read_exact::<1>(bytes, &mut cursor)?[0],
    };
    validate_shlm_command(&cmd)?;
    Ok(cmd)
}

/// Rotate a local vector by a unit quaternion in `[x,y,z,w]` order.
pub fn rotate_subworld_vector(quat: [f32; 4], vector: [f32; 3]) -> [f64; 3] {
    let [x, y, z, w] = quat.map(f64::from);
    let [vx, vy, vz] = vector.map(f64::from);
    // q*v*q^-1 expanded as v + 2*w*(qxyz × v) + 2*(qxyz × (qxyz × v)).
    let tx = 2.0 * (y * vz - z * vy);
    let ty = 2.0 * (z * vx - x * vz);
    let tz = 2.0 * (x * vy - y * vx);
    [
        vx + w * tx + (y * tz - z * ty),
        vy + w * ty + (z * tx - x * tz),
        vz + w * tz + (x * ty - y * tx),
    ]
}

/// World-space origin of the local voxel grid. `pos` is the center of mass,
/// while `pivot` is that center in hull-local coordinates, so mesh rendering
/// must use `pos - R*pivot` as the model translation.
pub fn pivot_adjusted_position(pos: [f64; 3], quat: [f32; 4], pivot: [f32; 3]) -> [f64; 3] {
    let rotated = rotate_subworld_vector(quat, pivot);
    [
        pos[0] - rotated[0],
        pos[1] - rotated[1],
        pos[2] - rotated[2],
    ]
}

/// Deterministic numeric fallback for a legacy string dimension id. Vanilla
/// slots remain 0/1/2; non-vanilla names occupy the high-bit namespace so
/// they cannot collide with small registry slots. New producers should carry
/// the real registry slot directly instead of deriving from a name.
pub fn legacy_dimension_id(name: &str) -> u32 {
    match name {
        "minecraft:overworld" | "overworld" => 0,
        "minecraft:the_nether" | "the_nether" => 1,
        "minecraft:the_end" | "the_end" => 2,
        _ => {
            let mut hash = 2_166_136_261u32;
            for byte in name.as_bytes() {
                hash ^= u32::from(*byte);
                hash = hash.wrapping_mul(16_777_619);
            }
            0x8000_0000 | (hash & 0x7fff_ffff)
        }
    }
}
