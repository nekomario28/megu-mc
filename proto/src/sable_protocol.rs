// SPDX-License-Identifier: LGPL-3.0-or-later
//! Clean-room wire contract for **Sable 1.0.5** (`sable:*`) and
//! **Create: Aeronautics 1.0.2** (`simulated:*`).
//!
//! Everything below was derived from the shipped mod JARs with `javap -c`
//! (constant pools, `write`/`read` call sequences and constructor shapes) and
//! from the vanilla codecs they call. No mod source was consulted or copied.
//! The JARs are only needed while verifying; this module is pure `std` and the
//! tests never read them.
//!
//! # Channels
//!
//! `Sable.sablePath(path)` is `ResourceLocation.fromNamespaceAndPath("sable",
//! path)` and `Simulated.path(path)` is
//! `ResourceLocation.tryBuild("simulated", path)`, so the `TYPE` payload ids
//! declared in each packet class are the channels below. [`SABLE_CHANNELS`] and
//! [`SIMULATED_CHANNELS`] carry them in `SableTCPPackets.init()` registration
//! order; [`SABLE_NAMESPACE_RULE`] / [`SIMULATED_NAMESPACE_RULE`] are what a
//! `channel_router` firewall needs to stop dropping them.
//!
//! # Pose and snapshot framing
//!
//! ```text
//! Pose3d = 3*f64 position | 4*f32 orientation(x,y,z,w) | 3*f64 rotation_point   (64 bytes)
//! Entry  = i64 plot_coordinate | Pose3d | 3*f32 linear_velocity | 3*f32 angular_velocity
//!          (96 bytes)
//! Batch  = i32 interpolation_tick | VarInt count | count * Entry
//! Info   = i32 msSinceLast | i32 gameTick | bool stopped
//! ```
//!
//! `SableBufferUtils.write(ByteBuf, Pose3dc)` writes exactly `position`,
//! `orientation` (through `d2f`) and `rotationPoint`; `scale` is **not** on the
//! wire and a freshly constructed `Pose3d` decodes with `(1, 1, 1)` — mirrored
//! by [`SablePose3d::read`], which always yields
//! [`SablePose3d::IDENTITY_SCALE`].
//!
//! # UDP framing
//!
//! `SableUDPPacketEncoder` writes `[u8 type ordinal][body]` into one datagram
//! and `SableUDPPacketDecoder` rejects a type ordinal outside
//! `SableUDPPacketType.VALUES` and rejects *any* trailing byte, so the datagram
//! is framed by its own boundary: there is no length prefix in Sable 1.0.5.
//! [`SableUdpFrame`] reproduces that byte for byte.
//! [`SableUdpFrame::encode_relay_frame`] additionally offers megu's own
//! length-prefixed form (`[VarInt21 len][u8 type][body]`, the vanilla frame
//! width) for carrying the same frame inside a reliable stream; that prefix is
//! **not** mod bytes. A body is decoded by the [`SableSnapshotBatch`],
//! [`SableSnapshotInfo`], [`SableUdpEcho`] or [`SableUdpAuth`] `decode`, all of
//! which reject trailing bytes the way `SableUDPPacketDecoder` does.
//!
//! # Interpolation
//!
//! [`ClientInterpolationState`] is `ClientSableInterpolationState`: a
//! fractional, tick-stamped pointer that lerps its speed estimate by `0.05`
//! toward `msPerTick / max(1ms, msSinceLastUpdate)` and is clamped to
//! `[mostRecentTick - delay, mostRecentTick + 1.5]`.
//! [`SableSnapshotInterpolator`] is `SubLevelSnapshotInterpolator`: a tick-keyed
//! ring that keeps six ticks, `nlerp`s inside a segment and extrapolates one
//! segment past the newest sample only while the server has not sent
//! `stop_moving_sub_level` ([`SableSnapshotInterpolator::receive_stop`]).
//! [`SablePose3d::slerp_to`] is the render-rate pose
//! (`ClientSubLevel.renderPose`) where orientation is `slerp`ed but position is
//! `lerp`ed.
//!
//! # Camera-relative precision
//!
//! Plot coordinates reach the millions, where a single `f32` step is 12.5 cm.
//! [`SablePose3d::camera_relative_transform`] therefore evaluates
//! `position - rotation_point - camera + chunk_origin` entirely in `f64` and
//! casts each finished component to `f32` exactly once, so a renderer never
//! sees the quantisation of a `1.28M`-scale coordinate.
//!
//! # Not modelled
//!
//! `sable:physics_property` and `sable:floating_material` carry data-driven
//! registry payloads (`PhysicsBlockPropertiesDefinition` with tag selectors and
//! per-state override maps), and `sable:change_sub_level_name` /
//! `sable:enter_gizmo_mode` are UI-state messages. Only their channels are
//! registered here; their bodies are not binary-fixed shapes this crate can own
//! without the surrounding registry context.

use crate::wire::{BlockPos, ProtoError, WireReader, WireWriter, MAX_STRING_CHARS};

/// Namespace of every Sable payload id.
pub const SABLE_NAMESPACE: &str = "sable";
/// Namespace of every Create: Aeronautics `simulated` payload id.
pub const SIMULATED_NAMESPACE: &str = "simulated";

/// Firewall rule that admits the whole `sable` namespace.
pub const SABLE_NAMESPACE_RULE: &str = "sable:*";
/// Firewall rule that admits the whole `simulated` namespace.
pub const SIMULATED_NAMESPACE_RULE: &str = "simulated:*";

/// `sable:snapshot_packet` — `ClientboundSableSnapshotDualPacket` (TCP *and* UDP).
pub const SABLE_SNAPSHOT_CHANNEL: &str = "sable:snapshot_packet";
/// `sable:snapshot_info_packet` — `ClientboundSableSnapshotInfoDualPacket`.
pub const SABLE_SNAPSHOT_INFO_CHANNEL: &str = "sable:snapshot_info_packet";
/// `sable:stop_moving_sub_level` — the server says the plot is at rest.
pub const SABLE_STOP_MOVING_CHANNEL: &str = "sable:stop_moving_sub_level";
/// `sable:change_sub_level_name` — display-name update.
pub const SABLE_CHANGE_NAME_CHANNEL: &str = "sable:change_sub_level_name";
/// `sable:start_tracking_sub_level` — first snapshot of a plot.
pub const SABLE_START_TRACKING_CHANNEL: &str = "sable:start_tracking_sub_level";
/// `sable:finalize_sub_level` — the plot's stored contents are final.
pub const SABLE_FINALIZE_CHANNEL: &str = "sable:finalize_sub_level";
/// `sable:stop_tracking_sub_level` — the client drops the plot.
pub const SABLE_STOP_TRACKING_CHANNEL: &str = "sable:stop_tracking_sub_level";
/// `sable:change_bounds_sublevel` — the exact spelling has no `_` before `sublevel`.
pub const SABLE_CHANGE_BOUNDS_CHANNEL: &str = "sable:change_bounds_sublevel";
/// `sable:freeze_player` — pin a player to a sub-level local position.
pub const SABLE_FREEZE_PLAYER_CHANNEL: &str = "sable:freeze_player";
/// `sable:physics_property` — data-driven physics block properties.
pub const SABLE_PHYSICS_PROPERTY_CHANNEL: &str = "sable:physics_property";
/// `sable:floating_material` — floating-block material definition.
pub const SABLE_FLOATING_MATERIAL_CHANNEL: &str = "sable:floating_material";
/// `sable:recently_split_sub_level` — split lineage and starting pose.
pub const SABLE_RECENTLY_SPLIT_CHANNEL: &str = "sable:recently_split_sub_level";
/// `sable:udp_activation` — hands the client its UDP authentication state.
pub const SABLE_UDP_ACTIVATION_CHANNEL: &str = "sable:udp_activation";
/// `sable:enter_gizmo_mode` — opens the server-move gizmo.
pub const SABLE_ENTER_GIZMO_CHANNEL: &str = "sable:enter_gizmo_mode";
/// `sable:punch_sub_level` — serverbound punch with a local hit ray.
pub const SABLE_PUNCH_CHANNEL: &str = "sable:punch_sub_level";
/// `sable:gizmo_move_sub_level` — serverbound gizmo drag.
pub const SABLE_GIZMO_MOVE_CHANNEL: &str = "sable:gizmo_move_sub_level";

/// `simulated:assemble` — assemble the contraption at a block position.
pub const SIMULATED_ASSEMBLE_CHANNEL: &str = "simulated:assemble";
/// `simulated:steering_wheel_update` — the exact path, not `steering_wheel`.
pub const SIMULATED_STEERING_WHEEL_CHANNEL: &str = "simulated:steering_wheel_update";
/// `simulated:throttle_lever_signal` — the exact path, not `throttle_lever`.
pub const SIMULATED_THROTTLE_LEVER_CHANNEL: &str = "simulated:throttle_lever_signal";

/// Every `sable:*` payload, in `SableTCPPackets.init()` registration order.
pub const SABLE_CHANNELS: [&str; 16] = [
    SABLE_SNAPSHOT_CHANNEL,
    SABLE_SNAPSHOT_INFO_CHANNEL,
    SABLE_STOP_MOVING_CHANNEL,
    SABLE_CHANGE_NAME_CHANNEL,
    SABLE_START_TRACKING_CHANNEL,
    SABLE_FINALIZE_CHANNEL,
    SABLE_STOP_TRACKING_CHANNEL,
    SABLE_CHANGE_BOUNDS_CHANNEL,
    SABLE_FREEZE_PLAYER_CHANNEL,
    SABLE_PHYSICS_PROPERTY_CHANNEL,
    SABLE_FLOATING_MATERIAL_CHANNEL,
    SABLE_RECENTLY_SPLIT_CHANNEL,
    SABLE_UDP_ACTIVATION_CHANNEL,
    SABLE_ENTER_GIZMO_CHANNEL,
    SABLE_PUNCH_CHANNEL,
    SABLE_GIZMO_MOVE_CHANNEL,
];

/// Every modeled `simulated:*` payload.
pub const SIMULATED_CHANNELS: [&str; 3] = [
    SIMULATED_ASSEMBLE_CHANNEL,
    SIMULATED_STEERING_WHEEL_CHANNEL,
    SIMULATED_THROTTLE_LEVER_CHANNEL,
];

/// Bytes a wire-encoded [`SablePose3d`] occupies.
pub const SABLE_POSE_WIRE_BYTES: usize = 3 * 8 + 4 * 4 + 3 * 8;
/// Bytes a wire-encoded [`SableSnapshotEntry`] occupies.
pub const SABLE_SNAPSHOT_ENTRY_WIRE_BYTES: usize = 8 + SABLE_POSE_WIRE_BYTES + 3 * 4 + 3 * 4;
/// Entries the network layer may pack into one snapshot packet.
///
/// `SubLevelTrackingSystem` drains its per-player ticket list in `16`-entry
/// chunks (`bipush 16` on both the UDP and the TCP send path), while the packet
/// reader itself loops over whatever `VarInt` count it is given. This bound is
/// therefore both the encoder's contract and the decoder's allocation guard.
pub const MAX_SABLE_SNAPSHOT_ENTRIES: usize = 16;

/// Largest datagram a Sable UDP frame may occupy without fragmenting:
/// a 1500-byte Ethernet payload minus 20 bytes IPv4 and 8 bytes UDP.
pub const MAX_SABLE_UDP_DATAGRAM_BYTES: usize = 1472;

/// Largest redstone signal a throttle lever may transmit (`0..=15`).
pub const MAX_THROTTLE_SIGNAL: u8 = 15;

/// `SubLevelTrackingSystem`'s position change gate, in blocks.
pub const MOVEMENT_POSITION_TOLERANCE: f64 = 9.375e-4;
/// `SubLevelTrackingSystem`'s rotation change gate, in degrees.
pub const MOVEMENT_ROTATION_TOLERANCE_DEGREES: f64 = 0.015;
/// The same gate in radians — the unit `Pose3dc.withinTolerance` receives.
pub const MOVEMENT_ROTATION_TOLERANCE_RADIANS: f64 =
    MOVEMENT_ROTATION_TOLERANCE_DEGREES * std::f64::consts::PI / 180.0;

/// Default `SableClientConfig.INTERPOLATION_DELAY`, in ticks.
pub const DEFAULT_INTERPOLATION_DELAY_TICKS: f64 = 1.5;
/// Upper bound the config accepts for the interpolation delay, in ticks.
pub const MAX_INTERPOLATION_DELAY_TICKS: f64 = 100.0;
/// How far the tick pointer may run ahead of the newest snapshot, in ticks.
pub const INTERPOLATION_LEAD_LIMIT_TICKS: f64 = 1.5;
/// Ticks of history `SubLevelSnapshotInterpolator.tick` keeps.
pub const INTERPOLATION_WINDOW_TICKS: f64 = 6.0;
/// Weight the server tick-speed estimate converges with, per tick.
pub const TICK_SPEED_LERP_FACTOR: f64 = 0.05;
/// Hard ceiling on buffered snapshots per sub-level.
///
/// Sable relies on the tick-based trim alone, so a peer that raises `gameTick`
/// in every snapshot while no render tick ever runs would grow its buffer
/// without bound. This cap drops the oldest sample instead.
pub const MAX_BUFFERED_SNAPSHOTS: usize = 64;

/// Default `SubLevelContainer.DEFAULT_LOG_PLOT_SIZE`: a plot spans
/// `2^7 = 128` chunks per axis.
pub const DEFAULT_PLOT_LOG_SIZE: u8 = 7;
/// Largest plot edge this crate will describe (`2^10` chunks per axis).
pub const MAX_PLOT_LOG_SIZE: u8 = 10;

/// `true` when `channel` lives in the `sable` namespace.
pub fn is_sable_channel(channel: &str) -> bool {
    matches!(
        channel.split_once(':'),
        Some((namespace, _)) if namespace == SABLE_NAMESPACE
    )
}

/// `true` when `channel` lives in the `simulated` namespace.
pub fn is_simulated_channel(channel: &str) -> bool {
    matches!(
        channel.split_once(':'),
        Some((namespace, _)) if namespace == SIMULATED_NAMESPACE
    )
}

/// Identity quaternion in wire order `(x, y, z, w)`.
const IDENTITY_ORIENTATION: [f32; 4] = [0.0, 0.0, 0.0, 1.0];

fn ensure_finite_f64(values: &[f64], what: &'static str) -> Result<(), ProtoError> {
    if values.iter().all(|value| value.is_finite()) {
        Ok(())
    } else {
        Err(ProtoError::Malformed(what))
    }
}

fn ensure_finite_f32(values: &[f32], what: &'static str) -> Result<(), ProtoError> {
    if values.iter().all(|value| value.is_finite()) {
        Ok(())
    } else {
        Err(ProtoError::Malformed(what))
    }
}

/// A quaternion in wire order `(x, y, z, w)` scaled back to unit length.
fn quat_normalize(quat: [f32; 4]) -> [f32; 4] {
    let norm_sq = quat.iter().map(|value| value * value).sum::<f32>();
    if norm_sq <= f32::MIN_POSITIVE {
        return IDENTITY_ORIENTATION;
    }
    let inverse = norm_sq.sqrt().recip();
    [
        quat[0] * inverse,
        quat[1] * inverse,
        quat[2] * inverse,
        quat[3] * inverse,
    ]
}

fn quat_dot(from: [f32; 4], to: [f32; 4]) -> f64 {
    from.iter()
        .zip(to.iter())
        .map(|(a, b)| f64::from(*a) * f64::from(*b))
        .sum()
}

/// Normalized linear interpolation — the blend `Pose3dc.lerp` uses in tick
/// space, where a constant angular velocity is not required.
fn quat_nlerp(from: [f32; 4], to: [f32; 4], delta: f32) -> [f32; 4] {
    let from = quat_normalize(from);
    let mut to = quat_normalize(to);
    // Take the short way round: q and -q are the same rotation.
    if quat_dot(from, to) < 0.0 {
        to = [-to[0], -to[1], -to[2], -to[3]];
    }
    quat_normalize([
        from[0] + (to[0] - from[0]) * delta,
        from[1] + (to[1] - from[1]) * delta,
        from[2] + (to[2] - from[2]) * delta,
        from[3] + (to[3] - from[3]) * delta,
    ])
}

/// Spherical interpolation at a constant angular rate — the render-rate blend
/// `ClientSubLevel.renderPose` applies to `orientation`.
fn quat_slerp(from: [f32; 4], to: [f32; 4], delta: f32) -> [f32; 4] {
    let from = quat_normalize(from);
    let mut to = quat_normalize(to);
    let mut dot = quat_dot(from, to);
    if dot < 0.0 {
        dot = -dot;
        to = [-to[0], -to[1], -to[2], -to[3]];
    }
    if dot > 0.999_5 {
        // Numerically degenerate: nlerp and slerp agree to f32 precision.
        return quat_nlerp(from, to, delta);
    }
    let theta = dot.clamp(-1.0, 1.0).acos();
    let sin_theta = theta.sin();
    let weight_from = ((1.0 - f64::from(delta)) * theta).sin() / sin_theta;
    let weight_to = (f64::from(delta) * theta).sin() / sin_theta;
    quat_normalize([
        (f64::from(from[0]) * weight_from + f64::from(to[0]) * weight_to) as f32,
        (f64::from(from[1]) * weight_from + f64::from(to[1]) * weight_to) as f32,
        (f64::from(from[2]) * weight_from + f64::from(to[2]) * weight_to) as f32,
        (f64::from(from[3]) * weight_from + f64::from(to[3]) * weight_to) as f32,
    ])
}

/// Angle between two rotations in radians — `Quaterniondc.div(other).angle()`.
///
/// Computed as `2 * atan2(|v|, |w|)` of the relative rotation instead of the
/// textbook `2 * acos(w)`, which loses all of its significant digits exactly
/// where the `0.015` degree tracking gate lives: at `2.6e-4` rad the `acos` form
/// cannot see the difference in `f32` at all.
fn quat_angle(from: [f32; 4], to: [f32; 4]) -> f64 {
    let [ax, ay, az, aw] = quat_normalize(from).map(f64::from);
    let [bx, by, bz, bw] = quat_normalize(to).map(f64::from);
    // `from * conj(to)`: the scalar part is the dot product, the vector part is
    // the cross term.
    let w = ax * bx + ay * by + az * bz + aw * bw;
    let x = -aw * bx + bw * ax - (ay * bz - az * by);
    let y = -aw * by + bw * ay - (az * bx - ax * bz);
    let z = -aw * bz + bw * az - (ax * by - ay * bx);
    let vector = (x * x + y * y + z * z).sqrt();
    2.0 * vector.atan2(w.abs())
}

/// Column-major `3x3` rotation matrix of a quaternion, computed in `f64` so the
/// single cast to `f32` is the only precision loss.
fn quat_rotation_matrix(quat: [f32; 4]) -> [f64; 9] {
    let [x, y, z, w] = quat_normalize(quat).map(f64::from);
    let (x2, y2, z2) = (x + x, y + y, z + z);
    let (xx, xy, xz) = (x * x2, x * y2, x * z2);
    let (yy, yz, zz) = (y * y2, y * z2, z * z2);
    let (wx, wy, wz) = (w * x2, w * y2, w * z2);
    [
        1.0 - (yy + zz),
        xy + wz,
        xz - wy,
        xy - wz,
        1.0 - (xx + zz),
        yz + wx,
        xz + wy,
        yz - wx,
        1.0 - (xx + yy),
    ]
}

fn lerp3(from: [f64; 3], to: [f64; 3], delta: f64) -> [f64; 3] {
    [
        from[0] + (to[0] - from[0]) * delta,
        from[1] + (to[1] - from[1]) * delta,
        from[2] + (to[2] - from[2]) * delta,
    ]
}

fn read_vec3f64(reader: &mut WireReader<'_>) -> Result<[f64; 3], ProtoError> {
    let mut out = [0.0; 3];
    for slot in &mut out {
        *slot = reader.read_f64()?;
    }
    Ok(out)
}

fn read_vec3f32(reader: &mut WireReader<'_>) -> Result<[f32; 3], ProtoError> {
    let mut out = [0.0; 3];
    for slot in &mut out {
        *slot = reader.read_f32()?;
    }
    Ok(out)
}

fn write_vec3f64(writer: &mut WireWriter, values: [f64; 3]) {
    for value in values {
        writer.write_f64(value);
    }
}

fn write_vec3f32(writer: &mut WireWriter, values: [f32; 3]) {
    for value in values {
        writer.write_f32(value);
    }
}

fn write_block_pos(writer: &mut WireWriter, pos: [i32; 3]) {
    writer.write_position(BlockPos::new(pos[0], pos[1], pos[2]));
}

fn write_bounded_string(writer: &mut WireWriter, text: &str) -> Result<(), ProtoError> {
    let units: usize = text.chars().map(char::len_utf16).sum();
    if units > MAX_STRING_CHARS {
        return Err(ProtoError::StringTooLong {
            units,
            max: MAX_STRING_CHARS,
        });
    }
    writer.write_string(text);
    Ok(())
}

/// A `dev.ryanhcode.sable.companion.math.Pose3d` as it appears on the wire.
///
/// The orientation is stored at wire precision (`f32`, order `x, y, z, w`) even
/// though the mod keeps it as a `Quaterniond`, because every transmitted value
/// has already been through that cast. `scale` is local-only: it is never
/// encoded, and a decoded pose always carries
/// [`SablePose3d::IDENTITY_SCALE`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SablePose3d {
    /// World-space position of the sub-level origin.
    pub position: [f64; 3],
    /// Rotation quaternion as `(x, y, z, w)`.
    pub orientation: [f32; 4],
    /// Pivot the orientation rotates around, in world space.
    pub rotation_point: [f64; 3],
    /// Local scale. Local-only, never transmitted.
    pub scale: [f64; 3],
}

impl SablePose3d {
    /// Scale a decoded pose always carries, matching `new Pose3d()`.
    pub const IDENTITY_SCALE: [f64; 3] = [1.0, 1.0, 1.0];

    /// The identity pose: origin, no rotation, unit scale.
    pub const IDENTITY: Self = Self {
        position: [0.0; 3],
        orientation: IDENTITY_ORIENTATION,
        rotation_point: [0.0; 3],
        scale: Self::IDENTITY_SCALE,
    };

    pub fn new(
        position: [f64; 3],
        orientation: [f32; 4],
        rotation_point: [f64; 3],
        scale: [f64; 3],
    ) -> Self {
        Self {
            position,
            orientation,
            rotation_point,
            scale,
        }
    }

    /// Reject values no encoder can produce and no renderer can consume.
    fn validate(&self) -> Result<(), ProtoError> {
        ensure_finite_f64(&self.position, "sable pose position is not finite")?;
        ensure_finite_f64(
            &self.rotation_point,
            "sable pose rotation point is not finite",
        )?;
        ensure_finite_f64(&self.scale, "sable pose scale is not finite")?;
        ensure_finite_f32(&self.orientation, "sable pose orientation is not finite")?;
        let norm_sq = self
            .orientation
            .iter()
            .map(|value| f64::from(*value) * f64::from(*value))
            .sum::<f64>();
        // A real encoder only emits unit quaternions; anything outside a factor
        // of two is a corrupted or hostile packet rather than rounding noise.
        if !(0.25..=4.0).contains(&norm_sq) {
            return Err(ProtoError::Malformed(
                "sable pose orientation is not a unit quaternion",
            ));
        }
        Ok(())
    }

    /// Append the 64-byte wire form. `scale` is deliberately omitted.
    pub fn write(&self, writer: &mut WireWriter) -> Result<(), ProtoError> {
        self.validate()?;
        write_vec3f64(writer, self.position);
        write_vec3f32(
            writer,
            [
                self.orientation[0],
                self.orientation[1],
                self.orientation[2],
            ],
        );
        writer.write_f32(self.orientation[3]);
        write_vec3f64(writer, self.rotation_point);
        Ok(())
    }

    /// Read 64 bytes; the returned pose always has unit scale.
    pub fn read(reader: &mut WireReader<'_>) -> Result<Self, ProtoError> {
        let position = read_vec3f64(reader)?;
        let orientation = [
            reader.read_f32()?,
            reader.read_f32()?,
            reader.read_f32()?,
            reader.read_f32()?,
        ];
        let rotation_point = read_vec3f64(reader)?;
        let pose = Self {
            position,
            orientation,
            rotation_point,
            scale: Self::IDENTITY_SCALE,
        };
        pose.validate()?;
        Ok(pose)
    }

    /// Encode one bare pose.
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        let mut writer = WireWriter::with_capacity(SABLE_POSE_WIRE_BYTES);
        self.write(&mut writer)?;
        Ok(writer.into_vec())
    }

    /// Decode one bare pose, rejecting trailing bytes.
    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        let mut reader = WireReader::new(bytes);
        let pose = Self::read(&mut reader)?;
        reader.finish()?;
        Ok(pose)
    }

    /// Tick-space blend: positions lerp, orientation `nlerp`s. A `delta` above
    /// one extrapolates along the same segment.
    pub fn nlerp_to(&self, other: &Self, delta: f64) -> Self {
        Self {
            position: lerp3(self.position, other.position, delta),
            orientation: quat_nlerp(self.orientation, other.orientation, delta as f32),
            rotation_point: lerp3(self.rotation_point, other.rotation_point, delta),
            scale: lerp3(self.scale, other.scale, delta),
        }
    }

    /// Render-rate blend: positions lerp, orientation `slerp`s at a constant
    /// angular rate, matching `ClientSubLevel.renderPose(float)`.
    pub fn slerp_to(&self, other: &Self, partial_tick: f32) -> Self {
        let delta = f64::from(partial_tick);
        Self {
            position: lerp3(self.position, other.position, delta),
            orientation: quat_slerp(self.orientation, other.orientation, partial_tick),
            rotation_point: lerp3(self.rotation_point, other.rotation_point, delta),
            scale: lerp3(self.scale, other.scale, delta),
        }
    }

    /// `Pose3dc.withinTolerance(other, position_tolerance, rotation_tolerance)`:
    /// both the position and the rotation point must stay within
    /// `position_tolerance`, and the relative rotation within
    /// `rotation_tolerance` radians.
    pub fn within_tolerance(
        &self,
        other: &Self,
        position_tolerance: f64,
        rotation_tolerance: f64,
    ) -> bool {
        let limit = position_tolerance * position_tolerance;
        let distance = |axis: usize| {
            let delta = self.position[axis] - other.position[axis];
            delta * delta
        };
        if distance(0) + distance(1) + distance(2) > limit {
            return false;
        }
        let pivot = |axis: usize| {
            let delta = self.rotation_point[axis] - other.rotation_point[axis];
            delta * delta
        };
        if pivot(0) + pivot(1) + pivot(2) > limit {
            return false;
        }
        quat_angle(self.orientation, other.orientation) <= rotation_tolerance
    }

    /// Camera-relative render transform.
    ///
    /// `position - rotation_point - camera + chunk_origin` is evaluated in
    /// `f64`, so every intermediate keeps the full precision of a
    /// `1_280_000.0`-scale plot coordinate and the result is cast to `f32` once.
    /// `basis` is the column-major rotation, scaled per column, the layout
    /// `Pose3dc.bakeIntoMatrix` produces before the view translation.
    pub fn camera_relative_transform(
        &self,
        camera: [f64; 3],
        chunk_origin: [f64; 3],
    ) -> SableRenderTransform {
        let mut translation = [0.0f64; 3];
        for (axis, slot) in translation.iter_mut().enumerate() {
            *slot =
                self.position[axis] - self.rotation_point[axis] - camera[axis] + chunk_origin[axis];
        }
        let rotation = quat_rotation_matrix(self.orientation);
        let mut basis = [0.0f32; 9];
        for (index, slot) in basis.iter_mut().enumerate() {
            let column = index / 3;
            let row = index % 3;
            *slot = (rotation[column * 3 + row] * self.scale[column]) as f32;
        }
        SableRenderTransform {
            basis,
            translation: [
                translation[0] as f32,
                translation[1] as f32,
                translation[2] as f32,
            ],
        }
    }
}

/// A camera-relative `3x3` basis plus `f32` translation, ready for a shader.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SableRenderTransform {
    /// Column-major rotation and scale.
    pub basis: [f32; 9],
    /// Camera-relative translation of the sub-level origin.
    pub translation: [f32; 3],
}

impl SableRenderTransform {
    /// `basis * local + translation`.
    pub fn apply(&self, local: [f32; 3]) -> [f32; 3] {
        let mut out = [0.0f32; 3];
        for (row, slot) in out.iter_mut().enumerate() {
            *slot = self.basis[row].mul_add(
                local[0],
                self.basis[3 + row].mul_add(
                    local[1],
                    self.basis[6 + row].mul_add(local[2], self.translation[row]),
                ),
            );
        }
        out
    }
}

/// One `ClientboundSableSnapshotDualPacket.Entry`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SableSnapshotEntry {
    /// Chunk-packed plot identity, `ChunkPos.asLong`.
    pub plot_coordinate: i64,
    /// Plot pose for the packet's interpolation tick.
    pub pose: SablePose3d,
    /// Linear velocity in blocks per tick.
    pub linear_velocity: [f32; 3],
    /// Angular velocity in radians per tick.
    pub angular_velocity: [f32; 3],
}

impl SableSnapshotEntry {
    /// Append the 96-byte wire form.
    pub fn write(&self, writer: &mut WireWriter) -> Result<(), ProtoError> {
        ensure_finite_f32(
            &self.linear_velocity,
            "sable snapshot linear velocity is not finite",
        )?;
        ensure_finite_f32(
            &self.angular_velocity,
            "sable snapshot angular velocity is not finite",
        )?;
        writer.write_i64(self.plot_coordinate);
        self.pose.write(writer)?;
        write_vec3f32(writer, self.linear_velocity);
        write_vec3f32(writer, self.angular_velocity);
        Ok(())
    }

    /// Read 96 bytes.
    pub fn read(reader: &mut WireReader<'_>) -> Result<Self, ProtoError> {
        let plot_coordinate = reader.read_i64()?;
        let pose = SablePose3d::read(reader)?;
        let linear_velocity = read_vec3f32(reader)?;
        let angular_velocity = read_vec3f32(reader)?;
        ensure_finite_f32(
            &linear_velocity,
            "sable snapshot linear velocity is not finite",
        )?;
        ensure_finite_f32(
            &angular_velocity,
            "sable snapshot angular velocity is not finite",
        )?;
        Ok(Self {
            plot_coordinate,
            pose,
            linear_velocity,
            angular_velocity,
        })
    }

    /// Encode one bare entry.
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        let mut writer = WireWriter::with_capacity(SABLE_SNAPSHOT_ENTRY_WIRE_BYTES);
        self.write(&mut writer)?;
        Ok(writer.into_vec())
    }

    /// Decode one bare entry, rejecting trailing bytes.
    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        let mut reader = WireReader::new(bytes);
        let entry = Self::read(&mut reader)?;
        reader.finish()?;
        Ok(entry)
    }
}

/// A whole `ClientboundSableSnapshotDualPacket` body.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SableSnapshotBatch {
    /// Server tick the entries belong to.
    pub interpolation_tick: i32,
    /// At most [`MAX_SABLE_SNAPSHOT_ENTRIES`] entries.
    pub entries: Vec<SableSnapshotEntry>,
}

impl SableSnapshotBatch {
    /// How many entries still fit an unfragmented datagram of `max_bytes`.
    ///
    /// The header is charged at its widest: one type byte, the `i32`
    /// interpolation tick and a five-byte `VarInt` count.
    pub fn entry_capacity_for_datagram(max_bytes: usize) -> usize {
        let overhead = 1 + 4 + 5;
        max_bytes
            .saturating_sub(overhead)
            .checked_div(SABLE_SNAPSHOT_ENTRY_WIRE_BYTES)
            .unwrap_or(0)
            .min(MAX_SABLE_SNAPSHOT_ENTRIES)
    }

    /// Append `i32 interpolation_tick | VarInt count | entries`.
    pub fn write(&self, writer: &mut WireWriter) -> Result<(), ProtoError> {
        if self.entries.len() > MAX_SABLE_SNAPSHOT_ENTRIES {
            return Err(ProtoError::LengthOutOfBounds {
                length: self.entries.len(),
                max: MAX_SABLE_SNAPSHOT_ENTRIES,
            });
        }
        let count = i32::try_from(self.entries.len())
            .map_err(|_| ProtoError::Malformed("sable snapshot batch is not encodable"))?;
        writer.write_i32(self.interpolation_tick);
        writer.write_varint(count);
        for entry in &self.entries {
            entry.write(writer)?;
        }
        Ok(())
    }

    /// Read `i32 interpolation_tick | VarInt count | entries`, rejecting a
    /// count above [`MAX_SABLE_SNAPSHOT_ENTRIES`] before allocating.
    pub fn read(reader: &mut WireReader<'_>) -> Result<Self, ProtoError> {
        let interpolation_tick = reader.read_i32()?;
        let count = reader.read_count(MAX_SABLE_SNAPSHOT_ENTRIES)?;
        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
            entries.push(SableSnapshotEntry::read(reader)?);
        }
        Ok(Self {
            interpolation_tick,
            entries,
        })
    }
}

/// One `ClientboundSableSnapshotInfoDualPacket` body.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SableSnapshotInfo {
    /// Milliseconds since the previous info packet, as the server measured it.
    pub ms_since_last: i32,
    /// Server game tick the info refers to.
    pub game_tick: i32,
    /// `true` once the server has parked every plot it tracks for the client.
    pub stopped: bool,
}

impl SableSnapshotInfo {
    /// Append `i32 | i32 | bool`.
    pub fn write(&self, writer: &mut WireWriter) -> Result<(), ProtoError> {
        writer.write_i32(self.ms_since_last);
        writer.write_i32(self.game_tick);
        writer.write_bool(self.stopped);
        Ok(())
    }

    /// Read `i32 | i32 | bool`.
    pub fn read(reader: &mut WireReader<'_>) -> Result<Self, ProtoError> {
        Ok(Self {
            ms_since_last: reader.read_i32()?,
            game_tick: reader.read_i32()?,
            stopped: reader.read_bool()?,
        })
    }
}

/// `SableUDPPacketType` ordinals, in declaration order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum SableUdpPacketKind {
    /// `PING` — a text echo, used to probe a UDP path.
    Ping = 0,
    /// `SNAPSHOT` — a [`SableSnapshotBatch`] body.
    Snapshot = 1,
    /// `SNAPSHOT_INFO` — a [`SableSnapshotInfo`] body.
    SnapshotInfo = 2,
    /// `AUTH` — the authentication token.
    Auth = 3,
    /// `KEEP_ALIVE_CLIENTBOUND` — empty body.
    KeepAliveClientbound = 4,
    /// `ALIVE_SERVERBOUND` — empty body.
    AliveServerbound = 5,
}

impl SableUdpPacketKind {
    /// The wire ordinal `SableUDPPacketType.ordinal()` yields.
    pub const fn ordinal(self) -> u8 {
        self as u8
    }

    /// Parse an ordinal, rejecting anything outside `VALUES`.
    pub const fn from_ordinal(ordinal: u8) -> Result<Self, ProtoError> {
        match ordinal {
            0 => Ok(Self::Ping),
            1 => Ok(Self::Snapshot),
            2 => Ok(Self::SnapshotInfo),
            3 => Ok(Self::Auth),
            4 => Ok(Self::KeepAliveClientbound),
            5 => Ok(Self::AliveServerbound),
            _ => Err(ProtoError::InvalidDiscriminant {
                value: ordinal as i64,
                what: "sable udp packet type",
            }),
        }
    }
}

/// One Sable UDP frame: `[u8 type ordinal][body]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SableUdpFrame {
    pub packet_type: SableUdpPacketKind,
    pub payload: Vec<u8>,
}

impl SableUdpFrame {
    /// Encode the datagram body exactly as `SableUDPPacketEncoder` lays it out.
    pub fn encode_datagram(&self) -> Result<Vec<u8>, ProtoError> {
        let total = self.payload.len().saturating_add(1);
        if total > MAX_SABLE_UDP_DATAGRAM_BYTES {
            return Err(ProtoError::FrameTooLarge {
                length: total,
                max: MAX_SABLE_UDP_DATAGRAM_BYTES,
            });
        }
        let mut out = Vec::with_capacity(total);
        out.push(self.packet_type.ordinal());
        out.extend_from_slice(&self.payload);
        Ok(out)
    }

    /// Decode `[u8 type ordinal][body]`, rejecting an unknown ordinal and an
    /// oversized datagram.
    pub fn decode_datagram(datagram: &[u8]) -> Result<Self, ProtoError> {
        if datagram.len() > MAX_SABLE_UDP_DATAGRAM_BYTES {
            return Err(ProtoError::FrameTooLarge {
                length: datagram.len(),
                max: MAX_SABLE_UDP_DATAGRAM_BYTES,
            });
        }
        let (first, payload) = datagram.split_first().ok_or(ProtoError::UnexpectedEof {
            needed: 1,
            remaining: 0,
        })?;
        Ok(Self {
            packet_type: SableUdpPacketKind::from_ordinal(*first)?,
            payload: payload.to_vec(),
        })
    }

    /// Encode megu's stream relay form `[VarInt21 len][u8 type][body]`.
    ///
    /// The `VarInt` prefix is this crate's framing for carrying a Sable datagram
    /// inside a byte stream; Sable itself only ever sends the bare datagram
    /// form.
    pub fn encode_relay_frame(&self) -> Result<Vec<u8>, ProtoError> {
        let datagram = self.encode_datagram()?;
        let length = i32::try_from(datagram.len())
            .map_err(|_| ProtoError::Malformed("sable relay frame is not encodable"))?;
        let mut writer = WireWriter::with_capacity(datagram.len() + 3);
        writer.write_varint(length);
        writer.raw(&datagram);
        Ok(writer.into_vec())
    }

    /// Decode `[VarInt21 len][u8 type][body]` and require the declared length to
    /// cover the rest of the slice exactly.
    pub fn decode_relay_frame(frame: &[u8]) -> Result<Self, ProtoError> {
        let mut reader = WireReader::new(frame);
        let length = reader.read_count(MAX_SABLE_UDP_DATAGRAM_BYTES)?;
        let datagram = reader.take(length)?;
        reader.finish()?;
        Self::decode_datagram(datagram)
    }
}

/// `SableUDPEchoPacket` (`PING`): one bounded UTF-8 string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SableUdpEcho {
    pub text: String,
}

impl SableUdpEcho {
    /// Encode the `PING` payload.
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        let mut writer = WireWriter::new();
        write_bounded_string(&mut writer, &self.text)?;
        Ok(writer.into_vec())
    }

    /// Decode a `PING` payload, rejecting trailing bytes.
    pub fn decode(payload: &[u8]) -> Result<Self, ProtoError> {
        let mut reader = WireReader::new(payload);
        let text = reader.read_string(MAX_STRING_CHARS)?;
        reader.finish()?;
        Ok(Self { text })
    }
}

/// `SableUDPAuthenticationPacket` (`AUTH`): one bounded UTF-8 token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SableUdpAuth {
    pub token: String,
}

impl SableUdpAuth {
    /// Encode the `AUTH` payload.
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        let mut writer = WireWriter::new();
        write_bounded_string(&mut writer, &self.token)?;
        Ok(writer.into_vec())
    }

    /// Decode an `AUTH` payload, rejecting trailing bytes.
    pub fn decode(payload: &[u8]) -> Result<Self, ProtoError> {
        let mut reader = WireReader::new(payload);
        let token = reader.read_string(MAX_STRING_CHARS)?;
        reader.finish()?;
        Ok(Self { token })
    }
}

/// `SteeringWheelPacket` (`simulated:steering_wheel_update`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SteeringWheelControl {
    /// `true` when the player let go of the wheel.
    pub should_stop: bool,
    /// Wheel angle the client wants the server to adopt, in radians.
    pub target_angle: f32,
    /// The wheel block entity's position.
    pub pos: [i32; 3],
}

impl SteeringWheelControl {
    /// Append `bool | f32 | packed block position`.
    pub fn write(&self, writer: &mut WireWriter) -> Result<(), ProtoError> {
        ensure_finite_f32(&[self.target_angle], "steering wheel angle is not finite")?;
        writer.write_bool(self.should_stop);
        writer.write_f32(self.target_angle);
        write_block_pos(writer, self.pos);
        Ok(())
    }

    /// Read `bool | f32 | packed block position`. The flag is strict: vanilla's
    /// `readBoolean` is `byte != 0`, but a wheel command carrying anything other
    /// than `0`/`1` is corrupt input, not "held".
    pub fn read(reader: &mut WireReader<'_>) -> Result<Self, ProtoError> {
        let should_stop = reader.read_bool()?;
        let target_angle = reader.read_f32()?;
        ensure_finite_f32(&[target_angle], "steering wheel angle is not finite")?;
        let pos = reader.read_position()?;
        Ok(Self {
            should_stop,
            target_angle,
            pos: [pos.x, pos.y, pos.z],
        })
    }

    /// Encode the whole `simulated:steering_wheel_update` body.
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        let mut writer = WireWriter::new();
        self.write(&mut writer)?;
        Ok(writer.into_vec())
    }

    /// Decode the whole body, rejecting trailing bytes.
    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        let mut reader = WireReader::new(bytes);
        let value = Self::read(&mut reader)?;
        reader.finish()?;
        Ok(value)
    }
}

/// `ThrottleLeverSignalPacket` (`simulated:throttle_lever_signal`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThrottleLeverSignal {
    /// The lever block entity's position.
    pub pos: [i32; 3],
    /// Redstone strength the lever should emit, `0..=15`.
    pub signal: u8,
}

impl ThrottleLeverSignal {
    fn validate_signal(signal: i32) -> Result<u8, ProtoError> {
        if !(0..=i32::from(MAX_THROTTLE_SIGNAL)).contains(&signal) {
            return Err(ProtoError::Malformed(
                "throttle lever signal is outside 0..=15",
            ));
        }
        Ok(signal as u8)
    }

    /// Append `packed block position | VarInt signal`.
    ///
    /// The mod writes the signal through `ByteBufCodecs.INT`, which is a
    /// `VarInt` on the wire, not a fixed `i32`.
    pub fn write(&self, writer: &mut WireWriter) -> Result<(), ProtoError> {
        Self::validate_signal(i32::from(self.signal))?;
        write_block_pos(writer, self.pos);
        writer.write_varint(i32::from(self.signal));
        Ok(())
    }

    /// Read `packed block position | VarInt signal`.
    pub fn read(reader: &mut WireReader<'_>) -> Result<Self, ProtoError> {
        let pos = reader.read_position()?;
        let signal = Self::validate_signal(reader.read_varint()?)?;
        Ok(Self {
            pos: [pos.x, pos.y, pos.z],
            signal,
        })
    }

    /// Encode the whole `simulated:throttle_lever_signal` body.
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        let mut writer = WireWriter::new();
        self.write(&mut writer)?;
        Ok(writer.into_vec())
    }

    /// Decode the whole body, rejecting trailing bytes.
    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        let mut reader = WireReader::new(bytes);
        let value = Self::read(&mut reader)?;
        reader.finish()?;
        Ok(value)
    }
}

/// The block-position extent of one sub-level plot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SablePlotBounds {
    /// Inclusive minimum block corner.
    pub min: [i32; 3],
    /// Inclusive maximum block corner.
    pub max: [i32; 3],
}

impl SablePlotBounds {
    /// Append six big-endian `i32`s in `minX, minY, minZ, maxX, maxY, maxZ` order.
    pub fn write(&self, writer: &mut WireWriter) {
        for value in self.min {
            writer.write_i32(value);
        }
        for value in self.max {
            writer.write_i32(value);
        }
    }

    /// Read six big-endian `i32`s.
    pub fn read(reader: &mut WireReader<'_>) -> Result<Self, ProtoError> {
        Ok(Self {
            min: [reader.read_i32()?, reader.read_i32()?, reader.read_i32()?],
            max: [reader.read_i32()?, reader.read_i32()?, reader.read_i32()?],
        })
    }
}

/// `ChunkPos.asLong` for a plot coordinate: `x` in the low word, `z` in the high.
pub const fn pack_plot_coordinate(plot: [i32; 2]) -> i64 {
    (plot[0] as i64 & 0xffff_ffff) | ((plot[1] as i64 & 0xffff_ffff) << 32)
}

/// Inverse of [`pack_plot_coordinate`].
pub const fn unpack_plot_coordinate(packed: i64) -> [i32; 2] {
    [packed as i32, (packed >> 32) as i32]
}

/// `LevelPlot`'s chunk-space geometry for one plot edge length.
///
/// Sable's shifts are Java `int` shifts, which wrap on overflow; the
/// `wrapping_*` calls here reproduce that instead of panicking in debug builds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlotLayout {
    log_size: u8,
}

impl PlotLayout {
    /// A layout whose plot spans `2^log_size` chunks per axis.
    pub fn new(log_size: u8) -> Result<Self, ProtoError> {
        if log_size > MAX_PLOT_LOG_SIZE {
            return Err(ProtoError::LengthOutOfBounds {
                length: usize::from(log_size),
                max: usize::from(MAX_PLOT_LOG_SIZE),
            });
        }
        Ok(Self { log_size })
    }

    /// The layout `SubLevelContainer.DEFAULT_LOG_PLOT_SIZE` selects.
    pub const fn default_layout() -> Self {
        Self {
            log_size: DEFAULT_PLOT_LOG_SIZE,
        }
    }

    pub const fn log_size(self) -> u8 {
        self.log_size
    }

    fn shift(self) -> u32 {
        u32::from(self.log_size)
    }

    /// `LevelPlot.getChunkMin()` — the first chunk the plot owns.
    pub fn chunk_min(self, plot: [i32; 2]) -> [i32; 2] {
        [
            plot[0].wrapping_shl(self.shift()),
            plot[1].wrapping_shl(self.shift()),
        ]
    }

    /// `LevelPlot.getChunkMax()` — the last chunk the plot owns.
    pub fn chunk_max(self, plot: [i32; 2]) -> [i32; 2] {
        [
            plot[0]
                .wrapping_add(1)
                .wrapping_shl(self.shift())
                .wrapping_sub(1),
            plot[1]
                .wrapping_add(1)
                .wrapping_shl(self.shift())
                .wrapping_sub(1),
        ]
    }

    /// `LevelPlot.contains(ChunkPos)`.
    pub fn contains_chunk(self, plot: [i32; 2], chunk: [i32; 2]) -> bool {
        chunk[0] >> self.shift() == plot[0] && chunk[1] >> self.shift() == plot[1]
    }

    /// `LevelPlot.toLocal(ChunkPos)` — world chunk to plot-local chunk.
    pub fn to_local(self, plot: [i32; 2], chunk: [i32; 2]) -> [i32; 2] {
        [
            chunk[0].wrapping_sub(plot[0].wrapping_shl(self.shift())),
            chunk[1].wrapping_sub(plot[1].wrapping_shl(self.shift())),
        ]
    }

    /// `LevelPlot.toGlobal(ChunkPos)` — plot-local chunk to world chunk.
    pub fn to_global(self, plot: [i32; 2], local: [i32; 2]) -> [i32; 2] {
        [
            local[0].wrapping_add(plot[0].wrapping_shl(self.shift())),
            local[1].wrapping_add(plot[1].wrapping_shl(self.shift())),
        ]
    }

    /// `LevelPlot.contains(double, double)` — block-space containment.
    pub fn contains_position(self, plot: [i32; 2], x: f64, z: f64) -> bool {
        let shift = self.shift() + 4;
        let min_x = f64::from(plot[0].wrapping_shl(shift));
        let min_z = f64::from(plot[1].wrapping_shl(shift));
        let max_x = f64::from(plot[0].wrapping_add(1).wrapping_shl(shift));
        let max_z = f64::from(plot[1].wrapping_add(1).wrapping_shl(shift));
        x >= min_x && x < max_x && z >= min_z && z < max_z
    }
}

/// `true` when `next` is close enough to `previous` that the server should not
/// send a movement update — `SubLevelTrackingSystem`'s change gate.
pub fn within_movement_tolerance(previous: &SablePose3d, next: &SablePose3d) -> bool {
    previous.within_tolerance(
        next,
        MOVEMENT_POSITION_TOLERANCE,
        MOVEMENT_ROTATION_TOLERANCE_RADIANS,
    )
}

/// What the tracker owes the client after evaluating one sub-level.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackingTicket {
    /// The pose moved past the gate: send a snapshot and update the memory.
    Move,
    /// The pose settled: send one `stop_moving_sub_level`, then nothing.
    Stop,
    /// Already stopped and still within tolerance.
    Idle,
}

/// The server-side half of the tracking change gate.
#[derive(Clone, Copy, Debug)]
pub struct MovementChangeGate {
    last_networked: SablePose3d,
    stopped: bool,
}

impl MovementChangeGate {
    /// Start tracking with `initial` as the last networked pose.
    pub fn new(initial: SablePose3d) -> Self {
        Self {
            last_networked: initial,
            stopped: false,
        }
    }

    pub fn last_networked(&self) -> &SablePose3d {
        &self.last_networked
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped
    }

    /// Classify the current logical pose, updating the gate's memory.
    pub fn evaluate(&mut self, logical: &SablePose3d) -> TrackingTicket {
        if within_movement_tolerance(&self.last_networked, logical) {
            if self.stopped {
                TrackingTicket::Idle
            } else {
                self.stopped = true;
                TrackingTicket::Stop
            }
        } else {
            self.last_networked = *logical;
            self.stopped = false;
            TrackingTicket::Move
        }
    }
}

/// `ClientSableInterpolationState` without the Minecraft client around it.
///
/// Call [`ClientInterpolationState::receive_info`] for every
/// `snapshot_info_packet` and [`ClientInterpolationState::tick`] once per client
/// tick with the level's tick rate; [`ClientInterpolationState::tick_pointer`]
/// is then the fractional server tick to sample sub-level poses at.
#[derive(Clone, Copy, Debug)]
pub struct ClientInterpolationState {
    most_recent_tick: f64,
    received_first_update: bool,
    interpolation_tick: f64,
    estimated_server_tick_speed: f64,
    server_ms_from_last_update: f32,
    stopped: bool,
    interpolation_delay: f64,
    latest_delay: f64,
    most_recent_interpolation_tick: f64,
    last_interpolation_tick: f64,
}

impl Default for ClientInterpolationState {
    fn default() -> Self {
        Self::new(DEFAULT_INTERPOLATION_DELAY_TICKS)
    }
}

impl ClientInterpolationState {
    /// Build a state whose pointer trails the server by `interpolation_delay`
    /// ticks, clamped to the range `SableClientConfig` accepts.
    pub fn new(interpolation_delay: f64) -> Self {
        let interpolation_delay = if interpolation_delay.is_finite() {
            interpolation_delay.clamp(0.0, MAX_INTERPOLATION_DELAY_TICKS)
        } else {
            DEFAULT_INTERPOLATION_DELAY_TICKS
        };
        Self {
            most_recent_tick: -1.0,
            received_first_update: false,
            interpolation_tick: 0.0,
            estimated_server_tick_speed: 0.0,
            server_ms_from_last_update: 0.0,
            stopped: false,
            interpolation_delay,
            latest_delay: 0.0,
            most_recent_interpolation_tick: 0.0,
            last_interpolation_tick: 0.0,
        }
    }

    /// `receiveInfo(msSinceLast, gameTick, stopped)`.
    ///
    /// A tick at or before the newest one is dropped. A first update, or the
    /// first update after a stop, re-seeds the pointer on the server tick with a
    /// full-tick speed estimate.
    pub fn receive_info(&mut self, ms_since_last: i32, game_tick: i32, stopped: bool) {
        let game_tick = f64::from(game_tick);
        if game_tick < self.most_recent_tick {
            return;
        }
        if !self.received_first_update || (self.stopped && !stopped) {
            self.interpolation_tick = game_tick;
            self.estimated_server_tick_speed = 1.0;
            self.received_first_update = true;
        }
        self.stopped = stopped;
        self.most_recent_tick = game_tick;
        self.server_ms_from_last_update = ms_since_last as f32;
    }

    /// Advance one client tick at `tick_rate` server ticks per second.
    pub fn tick(&mut self, tick_rate: f32) {
        if !self.received_first_update {
            return;
        }
        let ms_per_tick = 1000.0f32 / tick_rate;
        if !self.stopped {
            let observed = f64::from(ms_per_tick / 1.0f32.max(self.server_ms_from_last_update));
            // A zero tick rate makes `ms_per_tick` infinite, and Sable's
            // `lerp` then turns `inf - inf` into a NaN that poisons every later
            // tick. Keep the last finite estimate instead.
            if observed.is_finite() {
                self.estimated_server_tick_speed +=
                    TICK_SPEED_LERP_FACTOR * (observed - self.estimated_server_tick_speed);
            }
        }
        self.interpolation_tick += self.estimated_server_tick_speed;
        let lower = self.most_recent_tick - self.interpolation_delay;
        let upper = self.most_recent_tick + INTERPOLATION_LEAD_LIMIT_TICKS;
        if !self.interpolation_tick.is_finite() {
            self.interpolation_tick = lower;
        }
        // `upper > lower` always holds — the delay is non-negative and the lead
        // limit is positive — so `clamp` cannot panic here.
        self.interpolation_tick = self.interpolation_tick.clamp(lower, upper);
        self.latest_delay =
            self.most_recent_tick - self.interpolation_tick + self.interpolation_delay;
        self.last_interpolation_tick = self.most_recent_interpolation_tick;
        self.most_recent_interpolation_tick = self.tick_pointer();
    }

    /// The fractional server tick to sample poses at.
    pub fn tick_pointer(&self) -> f64 {
        self.interpolation_tick - self.interpolation_delay
    }

    pub fn interpolation_delay(&self) -> f64 {
        self.interpolation_delay
    }

    /// How far behind the newest server tick the pointer currently sits.
    pub fn latest_delay(&self) -> f64 {
        self.latest_delay
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped
    }

    pub fn received_first_update(&self) -> bool {
        self.received_first_update
    }

    pub fn most_recent_tick(&self) -> f64 {
        self.most_recent_tick
    }

    pub fn interpolation_tick(&self) -> f64 {
        self.interpolation_tick
    }

    pub fn estimated_server_tick_speed(&self) -> f64 {
        self.estimated_server_tick_speed
    }

    pub fn most_recent_interpolation_tick(&self) -> f64 {
        self.most_recent_interpolation_tick
    }

    pub fn last_interpolation_tick(&self) -> f64 {
        self.last_interpolation_tick
    }
}

/// One buffered tick of a sub-level's pose.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SableSnapshot {
    pub game_tick: i32,
    pub pose: SablePose3d,
}

/// `(int) (tickPointer - 6.0)`, the window trim bound.
///
/// Java's narrowing cast is undefined outside the `int` range; Rust's `as`
/// saturates, which keeps a corrupted pointer from silently pinning every
/// buffered snapshot alive.
fn trim_cutoff(tick_pointer: f64) -> i32 {
    (tick_pointer - INTERPOLATION_WINDOW_TICKS) as i32
}

/// `SubLevelSnapshotInterpolator`: a tick-keyed pose ring with `nlerp` between
/// samples and one gated extrapolation segment past the newest sample.
#[derive(Clone, Debug)]
pub struct SableSnapshotInterpolator {
    buffer: std::collections::VecDeque<SableSnapshot>,
    running: SablePose3d,
    stopped: bool,
}

impl SableSnapshotInterpolator {
    /// Start from `initial_pose`, which is what the client renders until the
    /// first snapshot arrives.
    pub fn new(initial_pose: SablePose3d) -> Self {
        Self {
            buffer: std::collections::VecDeque::new(),
            running: initial_pose,
            stopped: false,
        }
    }

    /// Record one snapshot. A repeat of the newest tick adds nothing and still
    /// clears the stop flag, like `receiveSnapshot`.
    pub fn receive_snapshot(&mut self, game_tick: i32, pose: SablePose3d) {
        if self
            .buffer
            .back()
            .is_none_or(|last| last.game_tick != game_tick)
        {
            if self.buffer.len() == MAX_BUFFERED_SNAPSHOTS {
                self.buffer.pop_front();
            }
            self.buffer.push_back(SableSnapshot { game_tick, pose });
        }
        self.stopped = false;
    }

    /// `receiveStop`: disable extrapolation until the next snapshot.
    pub fn receive_stop(&mut self) {
        self.stopped = true;
    }

    /// Drop samples older than the six-tick window, then re-sample at
    /// `tick_pointer`. An empty buffer leaves the running pose untouched.
    pub fn tick(&mut self, tick_pointer: f64) {
        let cutoff = trim_cutoff(tick_pointer);
        while self
            .buffer
            .front()
            .is_some_and(|snapshot| snapshot.game_tick < cutoff)
        {
            self.buffer.pop_front();
        }
        if self.buffer.is_empty() {
            return;
        }
        self.running = self.sample_at(tick_pointer);
    }

    /// Sample the tick space without mutating the interpolator.
    ///
    /// An exact tick match returns that snapshot; otherwise the two bracketing
    /// snapshots are `nlerp`ed. Past the newest sample the result extrapolates
    /// one segment back the way it came, unless [`Self::receive_stop`] was
    /// called or no older sample exists. Before the oldest sample the earliest
    /// sample is held.
    pub fn sample_at(&self, tick: f64) -> SablePose3d {
        let mut previous: Option<(usize, &SableSnapshot)> = None;
        let mut next: Option<&SableSnapshot> = None;
        for (index, snapshot) in self.buffer.iter().enumerate() {
            let snapshot_tick = f64::from(snapshot.game_tick);
            if snapshot_tick == tick {
                return snapshot.pose;
            }
            if snapshot_tick < tick {
                previous = Some((index, snapshot));
            } else {
                next = Some(snapshot);
                break;
            }
        }
        match (previous, next) {
            (Some((_, previous)), Some(next)) => {
                let span = f64::from(next.game_tick) - f64::from(previous.game_tick);
                let delta = if span > 0.0 {
                    (tick - f64::from(previous.game_tick)) / span
                } else {
                    0.0
                };
                previous.pose.nlerp_to(&next.pose, delta)
            }
            (Some((index, previous)), None) => {
                if index == 0 || self.stopped {
                    return previous.pose;
                }
                let older = &self.buffer[index - 1];
                let span = f64::from(previous.game_tick) - f64::from(older.game_tick);
                let ahead = (tick - f64::from(previous.game_tick)).clamp(0.0, 1.0);
                let delta = if span > 0.0 { 1.0 + ahead / span } else { 1.0 };
                older.pose.nlerp_to(&previous.pose, delta)
            }
            (None, Some(next)) => next.pose,
            (None, None) => self.running,
        }
    }

    /// The pose the last [`Self::tick`] sampled.
    pub fn interpolated_pose(&self) -> &SablePose3d {
        &self.running
    }

    /// Overwrite the running pose's pivot and position, the way
    /// `setFirstPoses` seeds a freshly tracked plot.
    pub fn set_first_poses(
        &mut self,
        rotation_point_source: &SablePose3d,
        position_source: &SablePose3d,
    ) {
        self.running.rotation_point = rotation_point_source.rotation_point;
        self.running.position = position_source.position;
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped
    }

    /// Buffered snapshots, oldest first.
    pub fn snapshots(&self) -> &std::collections::VecDeque<SableSnapshot> {
        &self.buffer
    }
}

/// Emit `encode`/`decode` for a type with `write`/`read` methods.
macro_rules! framed_codec {
    ($name:ident) => {
        impl $name {
            /// Encode the packet body.
            pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
                let mut writer = WireWriter::new();
                self.write(&mut writer)?;
                Ok(writer.into_vec())
            }

            /// Decode a whole packet body, rejecting trailing bytes.
            pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
                let mut reader = WireReader::new(bytes);
                let value = Self::read(&mut reader)?;
                reader.finish()?;
                Ok(value)
            }
        }
    };
}

framed_codec!(SableSnapshotBatch);
framed_codec!(SableSnapshotInfo);

/// `sable:start_tracking_sub_level` — everything needed to instantiate a plot.
#[derive(Clone, Debug, PartialEq)]
pub struct SableTrackingStart {
    pub plot_coordinate: i64,
    /// Sub-level UUID as Java writes it: most-significant bits first.
    pub sub_level_id: [u8; 16],
    /// The pose the plot already had on the server.
    pub last_pose: SablePose3d,
    /// The pose the client should start from.
    pub pose: SablePose3d,
    pub bounds: SablePlotBounds,
    /// Optional display name; `None` writes a `false` flag and no text.
    pub name: Option<String>,
    pub game_tick: i32,
}

impl SableTrackingStart {
    /// Append the body.
    pub fn write(&self, writer: &mut WireWriter) -> Result<(), ProtoError> {
        writer.write_i64(self.plot_coordinate);
        writer.write_uuid(&self.sub_level_id);
        self.last_pose.write(writer)?;
        self.pose.write(writer)?;
        self.bounds.write(writer);
        match &self.name {
            Some(name) => {
                writer.write_bool(true);
                write_bounded_string(writer, name)?;
            }
            None => writer.write_bool(false),
        }
        writer.write_i32(self.game_tick);
        Ok(())
    }

    /// Read the body.
    pub fn read(reader: &mut WireReader<'_>) -> Result<Self, ProtoError> {
        let plot_coordinate = reader.read_i64()?;
        let sub_level_id = reader.read_uuid()?;
        let last_pose = SablePose3d::read(reader)?;
        let pose = SablePose3d::read(reader)?;
        let bounds = SablePlotBounds::read(reader)?;
        let name = if reader.read_bool()? {
            Some(reader.read_string(MAX_STRING_CHARS)?)
        } else {
            None
        };
        let game_tick = reader.read_i32()?;
        Ok(Self {
            plot_coordinate,
            sub_level_id,
            last_pose,
            pose,
            bounds,
            name,
            game_tick,
        })
    }
}

framed_codec!(SableTrackingStart);

/// A packet whose whole body is one plot coordinate:
/// `sable:stop_tracking_sub_level`, `sable:finalize_sub_level` and
/// `sable:stop_moving_sub_level`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SablePlotSignal {
    pub plot_coordinate: i64,
}

impl SablePlotSignal {
    /// Append the single `i64`.
    pub fn write(&self, writer: &mut WireWriter) -> Result<(), ProtoError> {
        writer.write_i64(self.plot_coordinate);
        Ok(())
    }

    /// Read the single `i64`.
    pub fn read(reader: &mut WireReader<'_>) -> Result<Self, ProtoError> {
        Ok(Self {
            plot_coordinate: reader.read_i64()?,
        })
    }
}

framed_codec!(SablePlotSignal);

/// `sable:change_bounds_sublevel`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SableChangeBounds {
    pub plot_coordinate: i64,
    pub bounds: SablePlotBounds,
}

impl SableChangeBounds {
    /// Append `i64 plot | 6*i32 bounds`.
    pub fn write(&self, writer: &mut WireWriter) -> Result<(), ProtoError> {
        writer.write_i64(self.plot_coordinate);
        self.bounds.write(writer);
        Ok(())
    }

    /// Read `i64 plot | 6*i32 bounds`.
    pub fn read(reader: &mut WireReader<'_>) -> Result<Self, ProtoError> {
        Ok(Self {
            plot_coordinate: reader.read_i64()?,
            bounds: SablePlotBounds::read(reader)?,
        })
    }
}

framed_codec!(SableChangeBounds);

/// `sable:udp_activation`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SableUdpActivation {
    pub player_id: [u8; 16],
}

impl SableUdpActivation {
    /// Append the 16-byte player UUID.
    pub fn write(&self, writer: &mut WireWriter) -> Result<(), ProtoError> {
        writer.write_uuid(&self.player_id);
        Ok(())
    }

    /// Read the 16-byte player UUID.
    pub fn read(reader: &mut WireReader<'_>) -> Result<Self, ProtoError> {
        Ok(Self {
            player_id: reader.read_uuid()?,
        })
    }
}

framed_codec!(SableUdpActivation);

/// `sable:freeze_player`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SableFreezePlayer {
    pub sub_level_id: [u8; 16],
    /// Local-space position the player is pinned to.
    pub local_position: [f64; 3],
}

impl SableFreezePlayer {
    /// Append `UUID | 3*f64`.
    pub fn write(&self, writer: &mut WireWriter) -> Result<(), ProtoError> {
        ensure_finite_f64(&self.local_position, "freeze player position is not finite")?;
        writer.write_uuid(&self.sub_level_id);
        write_vec3f64(writer, self.local_position);
        Ok(())
    }

    /// Read `UUID | 3*f64`.
    pub fn read(reader: &mut WireReader<'_>) -> Result<Self, ProtoError> {
        let sub_level_id = reader.read_uuid()?;
        let local_position = read_vec3f64(reader)?;
        ensure_finite_f64(&local_position, "freeze player position is not finite")?;
        Ok(Self {
            sub_level_id,
            local_position,
        })
    }
}

framed_codec!(SableFreezePlayer);

/// `sable:punch_sub_level` — serverbound punch with a local hit ray.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SablePunchSubLevel {
    /// The punched block, in sub-level local space.
    pub punched_block: [i32; 3],
    /// Local-space eye position of the hit.
    pub local_position: [f64; 3],
    /// Local-space direction of the hit.
    pub direction: [f64; 3],
}

impl SablePunchSubLevel {
    /// Append `packed block | 3*f64 | 3*f64`.
    pub fn write(&self, writer: &mut WireWriter) -> Result<(), ProtoError> {
        ensure_finite_f64(&self.local_position, "punch position is not finite")?;
        ensure_finite_f64(&self.direction, "punch direction is not finite")?;
        write_block_pos(writer, self.punched_block);
        write_vec3f64(writer, self.local_position);
        write_vec3f64(writer, self.direction);
        Ok(())
    }

    /// Read `packed block | 3*f64 | 3*f64`.
    pub fn read(reader: &mut WireReader<'_>) -> Result<Self, ProtoError> {
        let punched_block = reader.read_position()?;
        let local_position = read_vec3f64(reader)?;
        let direction = read_vec3f64(reader)?;
        ensure_finite_f64(&local_position, "punch position is not finite")?;
        ensure_finite_f64(&direction, "punch direction is not finite")?;
        Ok(Self {
            punched_block: [punched_block.x, punched_block.y, punched_block.z],
            local_position,
            direction,
        })
    }
}

framed_codec!(SablePunchSubLevel);

/// `sable:gizmo_move_sub_level` — serverbound gizmo drag.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SableGizmoMove {
    pub sub_level_id: [u8; 16],
    /// Target world position of the sub-level origin.
    pub position: [f64; 3],
}

impl SableGizmoMove {
    /// Append `UUID | 3*f64`.
    pub fn write(&self, writer: &mut WireWriter) -> Result<(), ProtoError> {
        ensure_finite_f64(&self.position, "gizmo position is not finite")?;
        writer.write_uuid(&self.sub_level_id);
        write_vec3f64(writer, self.position);
        Ok(())
    }

    /// Read `UUID | 3*f64`.
    pub fn read(reader: &mut WireReader<'_>) -> Result<Self, ProtoError> {
        let sub_level_id = reader.read_uuid()?;
        let position = read_vec3f64(reader)?;
        ensure_finite_f64(&position, "gizmo position is not finite")?;
        Ok(Self {
            sub_level_id,
            position,
        })
    }
}

framed_codec!(SableGizmoMove);

/// `sable:recently_split_sub_level`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SableRecentlySplit {
    pub split_sub_level_id: [u8; 16],
    pub split_from_id: [u8; 16],
    /// Pose the split-off plot starts at.
    pub pose: SablePose3d,
}

impl SableRecentlySplit {
    /// Append `UUID | UUID | Pose3d`.
    pub fn write(&self, writer: &mut WireWriter) -> Result<(), ProtoError> {
        writer.write_uuid(&self.split_sub_level_id);
        writer.write_uuid(&self.split_from_id);
        self.pose.write(writer)?;
        Ok(())
    }

    /// Read `UUID | UUID | Pose3d`.
    pub fn read(reader: &mut WireReader<'_>) -> Result<Self, ProtoError> {
        Ok(Self {
            split_sub_level_id: reader.read_uuid()?,
            split_from_id: reader.read_uuid()?,
            pose: SablePose3d::read(reader)?,
        })
    }
}

framed_codec!(SableRecentlySplit);

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(text: &str) -> Vec<u8> {
        let digits: String = text.split_whitespace().collect();
        assert!(
            digits.len().is_multiple_of(2),
            "hex needs whole bytes: {text}"
        );
        digits
            .as_bytes()
            .chunks(2)
            .map(|pair| {
                let pair = std::str::from_utf8(pair).expect("ascii hex");
                u8::from_str_radix(pair, 16).expect("hex byte")
            })
            .collect()
    }

    fn sample_pose() -> SablePose3d {
        SablePose3d::new(
            [0.5, 64.25, -128.125],
            [0.0, 0.0, 0.0, 1.0],
            [0.5, 64.0, -128.0],
            SablePose3d::IDENTITY_SCALE,
        )
    }

    fn shift_pose(x: f64) -> SablePose3d {
        let mut pose = SablePose3d::IDENTITY;
        pose.position[0] = x;
        pose
    }

    fn about_z(angle: f64) -> [f32; 4] {
        let half = (angle / 2.0) as f32;
        [0.0, 0.0, half.sin(), half.cos()]
    }

    #[test]
    fn channel_registry_is_exact() {
        assert_eq!(SABLE_CHANNELS.len(), 16);
        assert_eq!(SABLE_SNAPSHOT_CHANNEL, "sable:snapshot_packet");
        assert_eq!(SABLE_SNAPSHOT_INFO_CHANNEL, "sable:snapshot_info_packet");
        assert_eq!(
            SABLE_START_TRACKING_CHANNEL,
            "sable:start_tracking_sub_level"
        );
        assert_eq!(SABLE_STOP_TRACKING_CHANNEL, "sable:stop_tracking_sub_level");
        assert_eq!(SABLE_FINALIZE_CHANNEL, "sable:finalize_sub_level");
        assert_eq!(SABLE_CHANGE_BOUNDS_CHANNEL, "sable:change_bounds_sublevel");
        assert_eq!(SABLE_UDP_ACTIVATION_CHANNEL, "sable:udp_activation");
        assert_eq!(SABLE_PHYSICS_PROPERTY_CHANNEL, "sable:physics_property");
        assert_eq!(SABLE_FLOATING_MATERIAL_CHANNEL, "sable:floating_material");
        assert_eq!(
            SABLE_RECENTLY_SPLIT_CHANNEL,
            "sable:recently_split_sub_level"
        );
        assert_eq!(SABLE_STOP_MOVING_CHANNEL, "sable:stop_moving_sub_level");
        assert_eq!(SABLE_FREEZE_PLAYER_CHANNEL, "sable:freeze_player");
        assert_eq!(SABLE_PUNCH_CHANNEL, "sable:punch_sub_level");
        assert_eq!(SABLE_GIZMO_MOVE_CHANNEL, "sable:gizmo_move_sub_level");
        assert_eq!(SIMULATED_ASSEMBLE_CHANNEL, "simulated:assemble");
        assert_eq!(
            SIMULATED_STEERING_WHEEL_CHANNEL,
            "simulated:steering_wheel_update"
        );
        assert_eq!(
            SIMULATED_THROTTLE_LEVER_CHANNEL,
            "simulated:throttle_lever_signal"
        );
        for channel in SABLE_CHANNELS.iter().chain(SIMULATED_CHANNELS.iter()) {
            assert_eq!(
                crate::channel_router::validate_channel_name(channel),
                Ok(())
            );
        }
        assert!(is_sable_channel("sable:snapshot_packet"));
        assert!(is_simulated_channel("simulated:assemble"));
        assert!(!is_sable_channel("simulated:assemble"));
        assert!(!is_simulated_channel("sable:freeze_player"));
        assert!(!is_sable_channel("sableish:freeze_player"));
        assert!(!is_sable_channel("noseparator"));
        assert_eq!(SABLE_NAMESPACE_RULE, "sable:*");
        assert_eq!(SIMULATED_NAMESPACE_RULE, "simulated:*");
    }

    #[test]
    fn pose_bytes_match_the_mod_layout() {
        let pose = SablePose3d::new(
            [0.0, 1.0, -1.0],
            [0.0, 0.0, 0.0, 1.0],
            [0.0, 0.0, 0.0],
            SablePose3d::IDENTITY_SCALE,
        );
        let encoded = pose.encode().expect("encode");
        let expected = hex("0000000000000000 3ff0000000000000 bff0000000000000 \
             00000000 00000000 00000000 3f800000 \
             0000000000000000 0000000000000000 0000000000000000");
        assert_eq!(encoded, expected);
        assert_eq!(encoded.len(), SABLE_POSE_WIRE_BYTES);
        assert_eq!(SablePose3d::decode(&encoded).expect("decode"), pose);
    }

    #[test]
    fn pose_scale_is_not_on_the_wire() {
        let mut scaled = sample_pose();
        scaled.scale = [2.0, 3.0, 4.0];
        assert_eq!(
            scaled.encode().expect("encode"),
            sample_pose().encode().expect("encode")
        );
        let decoded = SablePose3d::decode(&scaled.encode().expect("encode")).expect("decode");
        assert_eq!(decoded.scale, SablePose3d::IDENTITY_SCALE);
        assert_eq!(decoded, sample_pose());
    }

    #[test]
    fn pose_rejects_non_finite_components() {
        let mut pose = sample_pose();
        pose.position[1] = f64::NAN;
        assert_eq!(
            pose.encode(),
            Err(ProtoError::Malformed("sable pose position is not finite"))
        );
        let mut pose = sample_pose();
        pose.rotation_point[0] = f64::INFINITY;
        assert!(pose.encode().is_err());
        let mut pose = sample_pose();
        pose.orientation[2] = f32::NAN;
        assert!(pose.encode().is_err());
        let mut pose = sample_pose();
        pose.scale[0] = f64::NEG_INFINITY;
        assert!(pose.encode().is_err());
        // Hand-built wire bytes must fail closed too.
        let mut bytes = sample_pose().encode().expect("encode");
        bytes[24..28].copy_from_slice(&f32::NAN.to_be_bytes());
        assert!(SablePose3d::decode(&bytes).is_err());
        let mut bytes = sample_pose().encode().expect("encode");
        bytes[0..8].copy_from_slice(&f64::INFINITY.to_be_bytes());
        assert!(SablePose3d::decode(&bytes).is_err());
    }

    #[test]
    fn pose_rejects_non_unit_orientation() {
        let mut pose = sample_pose();
        pose.orientation = [0.0, 0.0, 0.0, 0.0];
        assert_eq!(
            pose.encode(),
            Err(ProtoError::Malformed(
                "sable pose orientation is not a unit quaternion"
            ))
        );
        pose.orientation = [0.0, 0.0, 0.0, 3.0];
        assert!(pose.encode().is_err());
        pose.orientation = [0.0, 0.0, 0.0, -1.0];
        assert!(pose.encode().is_ok());
    }

    #[test]
    fn pose_decode_rejects_truncation_and_trailing_bytes() {
        let bytes = sample_pose().encode().expect("encode");
        for length in [0, 1, 23, 24, 39, 40, 63] {
            assert!(SablePose3d::decode(&bytes[..length]).is_err(), "{length}");
        }
        let mut longer = bytes.clone();
        longer.push(0);
        assert_eq!(
            SablePose3d::decode(&longer),
            Err(ProtoError::Malformed("trailing bytes after packet body"))
        );
        assert_eq!(SablePose3d::decode(&bytes), Ok(sample_pose()));
    }

    #[test]
    fn snapshot_entry_is_ninety_six_bytes() {
        let entry = SableSnapshotEntry {
            plot_coordinate: pack_plot_coordinate([0x1234, -0x5678]),
            pose: sample_pose(),
            linear_velocity: [0.25, -0.5, 1.0],
            angular_velocity: [-0.125, 0.0, 0.0625],
        };
        let encoded = entry.encode().expect("encode");
        assert_eq!(encoded.len(), SABLE_SNAPSHOT_ENTRY_WIRE_BYTES);
        assert_eq!(SableSnapshotEntry::decode(&encoded).expect("decode"), entry);
        // The plot coordinate is a plain big-endian i64 at the front, the pose
        // follows, then the two f32 triples.
        assert_eq!(
            &encoded[..8],
            &pack_plot_coordinate([0x1234, -0x5678]).to_be_bytes()
        );
        assert_eq!(
            &encoded[8..(8 + SABLE_POSE_WIRE_BYTES)],
            &sample_pose().encode().expect("pose")
        );
        assert_eq!(
            &encoded[8 + SABLE_POSE_WIRE_BYTES..12 + SABLE_POSE_WIRE_BYTES],
            &0.25f32.to_be_bytes()
        );
        let mut broken = entry;
        broken.angular_velocity[1] = f32::INFINITY;
        assert!(broken.encode().is_err());
        let mut broken = entry;
        broken.linear_velocity[0] = f32::NAN;
        assert!(broken.encode().is_err());
    }

    #[test]
    fn snapshot_batch_roundtrip_and_entry_cap() {
        let entries = (0..MAX_SABLE_SNAPSHOT_ENTRIES)
            .map(|index| SableSnapshotEntry {
                plot_coordinate: pack_plot_coordinate([index as i32, 0]),
                pose: sample_pose(),
                linear_velocity: [0.0; 3],
                angular_velocity: [0.0; 3],
            })
            .collect::<Vec<_>>();
        let batch = SableSnapshotBatch {
            interpolation_tick: 4_242,
            entries: entries.clone(),
        };
        let encoded = batch.encode().expect("encode");
        // i32 tick, one-byte VarInt count, then the entries.
        assert_eq!(&encoded[..4], &4_242i32.to_be_bytes());
        assert_eq!(encoded[4], MAX_SABLE_SNAPSHOT_ENTRIES as u8);
        assert_eq!(
            encoded.len(),
            4 + 1 + MAX_SABLE_SNAPSHOT_ENTRIES * SABLE_SNAPSHOT_ENTRY_WIRE_BYTES
        );
        assert_eq!(SableSnapshotBatch::decode(&encoded).expect("decode"), batch);

        let oversized = SableSnapshotBatch {
            interpolation_tick: 0,
            entries: entries
                .iter()
                .copied()
                .chain(std::iter::once(entries[0]))
                .collect(),
        };
        assert_eq!(
            oversized.encode(),
            Err(ProtoError::LengthOutOfBounds {
                length: MAX_SABLE_SNAPSHOT_ENTRIES + 1,
                max: MAX_SABLE_SNAPSHOT_ENTRIES,
            })
        );
        // A forged count above the cap fails before any entry is allocated.
        let mut forged = 0i32.to_be_bytes().to_vec();
        forged.push(17);
        assert_eq!(
            SableSnapshotBatch::decode(&forged),
            Err(ProtoError::LengthOutOfBounds {
                length: 17,
                max: MAX_SABLE_SNAPSHOT_ENTRIES,
            })
        );
        // A negative count is not a count.
        let mut negative = 0i32.to_be_bytes().to_vec();
        negative.extend_from_slice(&hex("ff ff ff ff 0f"));
        assert_eq!(
            SableSnapshotBatch::decode(&negative),
            Err(ProtoError::NegativeLength)
        );
        // Truncated entry and trailing garbage both fail closed.
        assert!(SableSnapshotBatch::decode(&encoded[..encoded.len() - 1]).is_err());
        let mut longer = encoded;
        longer.push(0);
        assert!(SableSnapshotBatch::decode(&longer).is_err());
        assert_eq!(
            SableSnapshotBatch::entry_capacity_for_datagram(MAX_SABLE_UDP_DATAGRAM_BYTES),
            15
        );
        assert_eq!(SableSnapshotBatch::entry_capacity_for_datagram(9), 0);
        assert_eq!(
            SableSnapshotBatch::entry_capacity_for_datagram(usize::MAX),
            MAX_SABLE_SNAPSHOT_ENTRIES
        );
        assert_eq!(
            SableSnapshotBatch::default(),
            SableSnapshotBatch {
                interpolation_tick: 0,
                entries: Vec::new()
            }
        );
    }

    #[test]
    fn snapshot_info_matches_the_info_packet() {
        let info = SableSnapshotInfo {
            ms_since_last: 49,
            game_tick: 1_234,
            stopped: true,
        };
        let encoded = info.encode().expect("encode");
        assert_eq!(&encoded[..4], &49i32.to_be_bytes());
        assert_eq!(&encoded[4..8], &1_234i32.to_be_bytes());
        assert_eq!(encoded[8], 1);
        assert_eq!(SableSnapshotInfo::decode(&encoded), Ok(info));
        let mut broken = encoded.clone();
        broken[8] = 2;
        assert!(SableSnapshotInfo::decode(&broken).is_err());
        let mut longer = encoded;
        longer.push(0);
        assert!(SableSnapshotInfo::decode(&longer).is_err());
        assert_eq!(
            SableSnapshotInfo::decode(&[0; 8]),
            Err(ProtoError::UnexpectedEof {
                needed: 1,
                remaining: 0
            })
        );
    }

    #[test]
    fn udp_frame_is_type_then_body() {
        let frame = SableUdpFrame {
            packet_type: SableUdpPacketKind::SnapshotInfo,
            payload: vec![0xde, 0xad, 0xbe, 0xef],
        };
        let datagram = frame.encode_datagram().expect("encode");
        assert_eq!(datagram, hex("02 deadbeef"));
        assert_eq!(SableUdpFrame::decode_datagram(&datagram), Ok(frame));
        // Every declared ordinal round-trips.
        for ordinal in 0..=5u8 {
            let kind = SableUdpPacketKind::from_ordinal(ordinal).expect("ordinal");
            assert_eq!(kind.ordinal(), ordinal);
            let frame = SableUdpFrame {
                packet_type: kind,
                payload: Vec::new(),
            };
            assert_eq!(
                SableUdpFrame::decode_datagram(&frame.encode_datagram().expect("encode")),
                Ok(frame)
            );
        }
        assert_eq!(
            SableUdpPacketKind::from_ordinal(6),
            Err(ProtoError::InvalidDiscriminant {
                value: 6,
                what: "sable udp packet type"
            })
        );
        assert_eq!(
            SableUdpFrame::decode_datagram(&[]),
            Err(ProtoError::UnexpectedEof {
                needed: 1,
                remaining: 0
            })
        );
        assert!(SableUdpFrame::decode_datagram(&[6]).is_err());
        // The datagram decoder keeps trailing bytes in the payload; the typed
        // payload decoders are what reject them.
        let padded = hex("02 00000001 0000");
        let frame = SableUdpFrame::decode_datagram(&padded).expect("decode");
        assert_eq!(frame.payload.len(), 6);
        assert!(SableSnapshotInfo::decode(&frame.payload).is_err());
    }

    #[test]
    fn udp_frame_rejects_oversized_datagrams() {
        let frame = SableUdpFrame {
            packet_type: SableUdpPacketKind::AliveServerbound,
            payload: vec![0; MAX_SABLE_UDP_DATAGRAM_BYTES],
        };
        assert_eq!(
            frame.encode_datagram(),
            Err(ProtoError::FrameTooLarge {
                length: MAX_SABLE_UDP_DATAGRAM_BYTES + 1,
                max: MAX_SABLE_UDP_DATAGRAM_BYTES,
            })
        );
        assert!(
            SableUdpFrame::decode_datagram(&vec![0; MAX_SABLE_UDP_DATAGRAM_BYTES + 1]).is_err()
        );
        // A maximal frame still fits.
        let full = SableUdpFrame {
            packet_type: SableUdpPacketKind::AliveServerbound,
            payload: vec![0; MAX_SABLE_UDP_DATAGRAM_BYTES - 1],
        };
        assert_eq!(
            full.encode_datagram().expect("encode").len(),
            MAX_SABLE_UDP_DATAGRAM_BYTES
        );
    }

    #[test]
    fn udp_relay_frame_prefixes_the_datagram_length() {
        let frame = SableUdpFrame {
            packet_type: SableUdpPacketKind::KeepAliveClientbound,
            payload: Vec::new(),
        };
        let relay = frame.encode_relay_frame().expect("encode");
        assert_eq!(relay, hex("01 04"));
        assert_eq!(SableUdpFrame::decode_relay_frame(&relay), Ok(frame));
        // A declared length that does not cover the slice exactly is rejected.
        assert!(SableUdpFrame::decode_relay_frame(&hex("02 04")).is_err());
        assert!(SableUdpFrame::decode_relay_frame(&hex("01 04 00")).is_err());
        assert!(SableUdpFrame::decode_relay_frame(&[]).is_err());
        // The declared length is capped before it can be trusted.
        let mut huge = hex("80 80 80 80 04");
        huge.push(4);
        assert!(SableUdpFrame::decode_relay_frame(&huge).is_err());
        // A multi-byte length prefix still resolves.
        let big = SableUdpFrame {
            packet_type: SableUdpPacketKind::Snapshot,
            payload: vec![7; 200],
        };
        let relay = big.encode_relay_frame().expect("encode");
        assert_eq!(&relay[..2], &hex("c9 01")[..]);
        assert_eq!(SableUdpFrame::decode_relay_frame(&relay), Ok(big));
    }

    #[test]
    fn udp_string_payloads_are_bounded() {
        let echo = SableUdpEcho {
            text: "probe".to_string(),
        };
        let payload = echo.encode().expect("encode");
        assert_eq!(payload, hex("05 70726f6265"));
        assert_eq!(SableUdpEcho::decode(&payload), Ok(echo));
        let mut longer = payload;
        longer.push(0);
        assert!(SableUdpEcho::decode(&longer).is_err());
        let auth = SableUdpAuth {
            token: "tok".to_string(),
        };
        assert_eq!(
            SableUdpAuth::decode(&auth.encode().expect("encode")),
            Ok(auth)
        );
        let too_long = SableUdpEcho {
            text: "x".repeat(MAX_STRING_CHARS + 1),
        };
        assert!(too_long.encode().is_err());
        assert!(SableUdpEcho::decode(&hex("05 70726f62")).is_err());
    }

    #[test]
    fn steering_wheel_is_bool_float_and_packed_position() {
        let wheel = SteeringWheelControl {
            should_stop: true,
            target_angle: -0.5,
            pos: [10, 64, -30],
        };
        let encoded = wheel.encode().expect("encode");
        assert_eq!(encoded.len(), 1 + 4 + 8);
        assert_eq!(encoded[0], 1);
        assert_eq!(&encoded[1..5], &(-0.5f32).to_be_bytes());
        assert_eq!(
            &encoded[5..],
            &BlockPos::new(10, 64, -30).pack().to_be_bytes()
        );
        assert_eq!(SteeringWheelControl::decode(&encoded), Ok(wheel));
        // A flag byte other than 0 or 1 is corrupt input.
        let mut broken = encoded.clone();
        broken[0] = 2;
        assert!(SteeringWheelControl::decode(&broken).is_err());
        let mut non_finite = wheel;
        non_finite.target_angle = f32::NAN;
        assert!(non_finite.encode().is_err());
        let mut trailing = encoded;
        trailing.push(0);
        assert!(SteeringWheelControl::decode(&trailing).is_err());
        // A NaN angle on the wire is rejected as well.
        let mut nan_wire = wheel.encode().expect("encode");
        nan_wire[1..5].copy_from_slice(&f32::NAN.to_be_bytes());
        assert!(SteeringWheelControl::decode(&nan_wire).is_err());
    }

    #[test]
    fn throttle_lever_signal_is_a_bounded_varint() {
        let lever = ThrottleLeverSignal {
            pos: [-1, 70, 3],
            signal: MAX_THROTTLE_SIGNAL,
        };
        let encoded = lever.encode().expect("encode");
        assert_eq!(encoded.len(), 8 + 1);
        assert_eq!(encoded[8], MAX_THROTTLE_SIGNAL);
        assert_eq!(ThrottleLeverSignal::decode(&encoded), Ok(lever));
        for signal in 0..=MAX_THROTTLE_SIGNAL {
            let lever = ThrottleLeverSignal {
                pos: [0, 0, 0],
                signal,
            };
            assert_eq!(
                ThrottleLeverSignal::decode(&lever.encode().expect("encode")),
                Ok(lever)
            );
        }
        let out_of_range = ThrottleLeverSignal {
            pos: [0, 0, 0],
            signal: MAX_THROTTLE_SIGNAL + 1,
        };
        assert_eq!(
            out_of_range.encode(),
            Err(ProtoError::Malformed(
                "throttle lever signal is outside 0..=15"
            ))
        );
        let mut forged = BlockPos::new(0, 0, 0).pack().to_be_bytes().to_vec();
        forged.push(16);
        assert!(ThrottleLeverSignal::decode(&forged).is_err());
        // A negative encoding is a five-byte VarInt and must also be rejected.
        let mut negative = BlockPos::new(0, 0, 0).pack().to_be_bytes().to_vec();
        negative.extend_from_slice(&hex("ff ff ff ff 0f"));
        assert!(ThrottleLeverSignal::decode(&negative).is_err());
    }

    #[test]
    fn plot_geometry_matches_level_plot() {
        let layout = PlotLayout::new(7).expect("layout");
        assert_eq!(layout, PlotLayout::default_layout());
        assert_eq!(layout.log_size(), DEFAULT_PLOT_LOG_SIZE);
        let plot = [3, -2];
        assert_eq!(layout.chunk_min(plot), [384, -256]);
        assert_eq!(layout.chunk_max(plot), [511, -129]);
        assert!(layout.contains_chunk(plot, [384, -256]));
        assert!(layout.contains_chunk(plot, [511, -129]));
        assert!(!layout.contains_chunk(plot, [512, -129]));
        assert!(!layout.contains_chunk(plot, [383, -256]));
        for local in [[0, 0], [127, 127], [64, 3]] {
            let global = layout.to_global(plot, local);
            assert!(layout.contains_chunk(plot, global));
            assert_eq!(layout.to_local(plot, global), local);
        }
        // Block space: the plot starts at chunk_min * 16 and ends one block
        // before the chunk after chunk_max.
        assert!(layout.contains_position(plot, 384.0 * 16.0, -256.0 * 16.0));
        assert!(layout.contains_position(plot, 512.0 * 16.0 - 0.001, -129.0 * 16.0 + 15.999));
        assert!(!layout.contains_position(plot, 512.0 * 16.0, 0.0));
        assert!(!layout.contains_position(plot, 0.0, -256.0 * 16.0 - 0.001));
        assert_eq!(
            PlotLayout::new(MAX_PLOT_LOG_SIZE + 1),
            Err(ProtoError::LengthOutOfBounds {
                length: usize::from(MAX_PLOT_LOG_SIZE + 1),
                max: usize::from(MAX_PLOT_LOG_SIZE),
            })
        );
        // A zero-length shift is identity.
        let flat = PlotLayout::new(0).expect("layout");
        assert_eq!(flat.chunk_min([5, -5]), [5, -5]);
        assert_eq!(flat.chunk_max([5, -5]), [5, -5]);
        // Plot coordinates are `ChunkPos.asLong`: x low word, z high word.
        for plot in [
            [0, 0],
            [1, -1],
            [-2_000_000, 2_000_000],
            [i32::MAX, i32::MIN],
        ] {
            let packed = pack_plot_coordinate(plot);
            assert_eq!(unpack_plot_coordinate(packed), plot, "{plot:?}");
        }
        assert_eq!(
            pack_plot_coordinate([3, -2]),
            (3i64 & 0xffff_ffff) | ((-2i64 & 0xffff_ffff) << 32)
        );
    }

    #[test]
    fn movement_gate_uses_the_tracker_tolerances() {
        let base = SablePose3d::new(
            [100.0, 64.0, -100.0],
            [0.0, 0.0, 0.0, 1.0],
            [100.0, 64.0, -100.0],
            SablePose3d::IDENTITY_SCALE,
        );
        // Half the position gate in each axis is still within tolerance.
        let mut nudged = base;
        nudged.position[0] += MOVEMENT_POSITION_TOLERANCE * 0.5;
        nudged.position[1] -= MOVEMENT_POSITION_TOLERANCE * 0.5;
        assert!(within_movement_tolerance(&base, &nudged));
        // Just past the gate in one axis is not.
        let mut moved = base;
        moved.position[2] += MOVEMENT_POSITION_TOLERANCE * 1.01;
        assert!(!within_movement_tolerance(&base, &moved));
        // The rotation point is gated too.
        let mut pivoted = base;
        pivoted.rotation_point[1] += MOVEMENT_POSITION_TOLERANCE * 2.0;
        assert!(!within_movement_tolerance(&base, &pivoted));
        // Rotation: half the 0.015 degree gate is inside, 1.5x is outside.
        let inside = MOVEMENT_ROTATION_TOLERANCE_RADIANS * 0.5;
        let outside = MOVEMENT_ROTATION_TOLERANCE_RADIANS * 1.5;
        let mut rotated = base;
        rotated.orientation = about_z(inside);
        assert!(within_movement_tolerance(&base, &rotated));
        rotated.orientation = about_z(outside);
        assert!(!within_movement_tolerance(&base, &rotated));
        assert!((quat_angle(base.orientation, about_z(outside)) - outside).abs() < 1e-6);
        assert!((quat_angle(base.orientation, about_z(inside)) - inside).abs() < 1e-7);
        // A 90 degree roll is far outside every gate.
        rotated.orientation = about_z(std::f64::consts::FRAC_PI_2);
        assert!(!within_movement_tolerance(&base, &rotated));
        assert_eq!(MOVEMENT_ROTATION_TOLERANCE_DEGREES, 0.015);
        assert_eq!(MOVEMENT_POSITION_TOLERANCE, 9.375e-4);
    }

    #[test]
    fn movement_gate_emits_move_then_stop_then_idle() {
        let base = SablePose3d::IDENTITY;
        let mut gate = MovementChangeGate::new(base);
        assert_eq!(gate.evaluate(&base), TrackingTicket::Stop);
        assert!(gate.is_stopped());
        assert_eq!(gate.evaluate(&base), TrackingTicket::Idle);
        let mut moved = base;
        moved.position[0] = 1.0;
        assert_eq!(gate.evaluate(&moved), TrackingTicket::Move);
        assert!(!gate.is_stopped());
        assert_eq!(gate.last_networked().position[0], 1.0);
        assert_eq!(gate.evaluate(&moved), TrackingTicket::Stop);
        assert_eq!(gate.evaluate(&moved), TrackingTicket::Idle);
    }

    fn seeded_state() -> ClientInterpolationState {
        let mut state = ClientInterpolationState::new(DEFAULT_INTERPOLATION_DELAY_TICKS);
        state.receive_info(50, 100, false);
        state
    }

    #[test]
    fn interpolation_state_seeds_on_the_first_info() {
        let mut state = ClientInterpolationState::default();
        assert!(!state.received_first_update());
        state.tick(20.0);
        assert_eq!(state.interpolation_tick(), 0.0);
        state.receive_info(50, 100, false);
        assert!(state.received_first_update());
        assert_eq!(state.interpolation_tick(), 100.0);
        assert_eq!(state.estimated_server_tick_speed(), 1.0);
        assert_eq!(state.most_recent_tick(), 100.0);
        assert_eq!(
            state.tick_pointer(),
            100.0 - DEFAULT_INTERPOLATION_DELAY_TICKS
        );
        assert_eq!(
            state.interpolation_delay(),
            DEFAULT_INTERPOLATION_DELAY_TICKS
        );
        // Older ticks are ignored outright.
        state.receive_info(10, 99, true);
        assert_eq!(state.most_recent_tick(), 100.0);
        assert!(!state.is_stopped());
        // The delay is clamped to the config range, and garbage falls back.
        assert_eq!(
            ClientInterpolationState::new(-5.0).interpolation_delay(),
            0.0
        );
        assert_eq!(
            ClientInterpolationState::new(1_000.0).interpolation_delay(),
            MAX_INTERPOLATION_DELAY_TICKS
        );
        assert_eq!(
            ClientInterpolationState::new(f64::NAN).interpolation_delay(),
            DEFAULT_INTERPOLATION_DELAY_TICKS
        );
    }

    #[test]
    fn interpolation_state_converges_and_stays_clamped() {
        let mut state = seeded_state();
        // A 50 ms cadence at 20 tps is exactly one tick per client tick.
        for _ in 0..20 {
            state.tick(20.0);
            let next = state.most_recent_tick() as i32 + 1;
            state.receive_info(50, next, false);
        }
        assert!((state.estimated_server_tick_speed() - 1.0).abs() < 1e-6);
        let recent = state.most_recent_tick();
        let pointer = state.tick_pointer();
        assert_eq!(recent, 120.0);
        assert!(pointer > recent - DEFAULT_INTERPOLATION_DELAY_TICKS - 1.0);
        assert!(pointer < recent + INTERPOLATION_LEAD_LIMIT_TICKS);
        // A long stall clamps at the lead limit instead of running away.
        for _ in 0..200 {
            state.tick(20.0);
        }
        assert!(
            (state.interpolation_tick() - (recent + INTERPOLATION_LEAD_LIMIT_TICKS)).abs() < 1e-9
        );
        assert!((state.latest_delay()).abs() < 1e-9);
        assert_eq!(
            state.last_interpolation_tick(),
            state.most_recent_interpolation_tick()
        );
        assert_eq!(state.most_recent_interpolation_tick(), state.tick_pointer());
    }

    #[test]
    fn interpolation_speed_adapts_to_a_slower_server() {
        let mut state = ClientInterpolationState::new(0.0);
        state.receive_info(200, 100, false);
        for _ in 0..400 {
            state.tick(20.0);
        }
        // 50 ms per tick over a 200 ms cadence is a quarter tick per tick.
        assert!((state.estimated_server_tick_speed() - 0.25).abs() < 0.01);
        // A stopped ticker still advances the estimate toward the target.
        let mut zero_rate = seeded_state();
        for _ in 0..64 {
            zero_rate.tick(0.0);
        }
        assert!(zero_rate.interpolation_tick() >= zero_rate.most_recent_tick());
    }

    #[test]
    fn interpolation_state_stop_freezes_the_speed_estimate() {
        let mut state = seeded_state();
        state.receive_info(50, 101, true);
        let speed = state.estimated_server_tick_speed();
        for _ in 0..10 {
            state.tick(20.0);
        }
        assert_eq!(state.estimated_server_tick_speed(), speed);
        assert!(state.is_stopped());
        // A restart re-seeds the pointer on the server tick.
        state.receive_info(50, 120, false);
        assert_eq!(state.interpolation_tick(), 120.0);
        assert_eq!(state.estimated_server_tick_speed(), 1.0);
        assert!(!state.is_stopped());
    }

    #[test]
    fn interpolator_nlerps_inside_a_segment() {
        let mut interpolator = SableSnapshotInterpolator::new(SablePose3d::IDENTITY);
        interpolator.receive_snapshot(100, shift_pose(0.0));
        interpolator.receive_snapshot(102, shift_pose(2.0));
        assert_eq!(interpolator.sample_at(100.0), shift_pose(0.0));
        assert_eq!(interpolator.sample_at(102.0), shift_pose(2.0));
        assert_eq!(interpolator.sample_at(101.0), shift_pose(1.0));
        let quarter = interpolator.sample_at(100.5);
        assert!((quarter.position[0] - 0.5).abs() < 1e-9);
        // Orientation blends too: a 90 degree step is 45 degrees halfway.
        let mut rotated = SablePose3d::IDENTITY;
        rotated.orientation = about_z(std::f64::consts::FRAC_PI_2);
        let mut interpolator = SableSnapshotInterpolator::new(SablePose3d::IDENTITY);
        interpolator.receive_snapshot(0, SablePose3d::IDENTITY);
        interpolator.receive_snapshot(2, rotated);
        let halfway = interpolator.sample_at(1.0);
        let angle = quat_angle(SablePose3d::IDENTITY.orientation, halfway.orientation);
        assert!(
            (angle - std::f64::consts::FRAC_PI_4).abs() < 1e-4,
            "{angle}"
        );
    }

    #[test]
    fn interpolator_extrapolates_one_segment_until_stopped() {
        let mut interpolator = SableSnapshotInterpolator::new(SablePose3d::IDENTITY);
        interpolator.receive_snapshot(98, shift_pose(0.0));
        interpolator.receive_snapshot(100, shift_pose(2.0));
        // Half a tick past the newest sample continues the last segment.
        let ahead = interpolator.sample_at(100.5);
        assert!(
            (ahead.position[0] - 2.5).abs() < 1e-9,
            "{:?}",
            ahead.position
        );
        // Extrapolation is capped at one segment's worth of `clamp`.
        let far = interpolator.sample_at(110.0);
        assert!((far.position[0] - 3.0).abs() < 1e-9, "{:?}", far.position);
        interpolator.receive_stop();
        assert_eq!(interpolator.sample_at(100.5), shift_pose(2.0));
        // A new snapshot clears the stop flag again.
        interpolator.receive_snapshot(102, shift_pose(4.0));
        assert!(!interpolator.is_stopped());
        assert!((interpolator.sample_at(102.5).position[0] - 4.5).abs() < 1e-9);
        // With no older sample there is nothing to extrapolate along.
        let mut lonely = SableSnapshotInterpolator::new(SablePose3d::IDENTITY);
        lonely.receive_snapshot(10, shift_pose(3.0));
        assert_eq!(lonely.sample_at(11.0), shift_pose(3.0));
        assert_eq!(lonely.sample_at(10.0), shift_pose(3.0));
    }

    #[test]
    fn interpolator_holds_the_early_sample_before_the_window() {
        let mut interpolator = SableSnapshotInterpolator::new(SablePose3d::IDENTITY);
        interpolator.receive_snapshot(50, shift_pose(1.0));
        interpolator.receive_snapshot(52, shift_pose(3.0));
        // Before the oldest sample the oldest is held, not extrapolated.
        assert_eq!(interpolator.sample_at(40.0), shift_pose(1.0));
        // An empty interpolator keeps its running pose.
        let empty = SableSnapshotInterpolator::new(shift_pose(7.0));
        assert_eq!(empty.sample_at(1.0), shift_pose(7.0));
        assert!(empty.snapshots().is_empty());
    }

    #[test]
    fn interpolator_keeps_the_six_tick_window() {
        let mut interpolator = SableSnapshotInterpolator::new(SablePose3d::IDENTITY);
        for tick in 0..20 {
            interpolator.receive_snapshot(tick, shift_pose(f64::from(tick)));
        }
        assert_eq!(interpolator.snapshots().len(), 20);
        interpolator.tick(19.0);
        let cutoff = 19.0 - INTERPOLATION_WINDOW_TICKS;
        assert!(interpolator
            .snapshots()
            .iter()
            .all(|snapshot| f64::from(snapshot.game_tick) >= cutoff));
        assert_eq!(interpolator.snapshots().len(), 7);
        assert_eq!(
            interpolator
                .snapshots()
                .front()
                .map(|snapshot| snapshot.game_tick),
            Some(13)
        );
        assert!((interpolator.interpolated_pose().position[0] - 19.0).abs() < 1e-9);
        // An empty buffer leaves the running pose alone.
        let mut empty = SableSnapshotInterpolator::new(shift_pose(5.0));
        empty.tick(100.0);
        assert_eq!(empty.interpolated_pose().position[0], 5.0);
        assert_eq!(trim_cutoff(f64::NAN), 0);
        assert_eq!(trim_cutoff(1.0e30), i32::MAX);
        assert_eq!(trim_cutoff(0.0), -6);
    }

    #[test]
    fn interpolator_caps_the_buffer() {
        let mut interpolator = SableSnapshotInterpolator::new(SablePose3d::IDENTITY);
        let ticks = MAX_BUFFERED_SNAPSHOTS as i32 + 10;
        for tick in 0..ticks {
            interpolator.receive_snapshot(tick, shift_pose(f64::from(tick)));
        }
        assert_eq!(interpolator.snapshots().len(), MAX_BUFFERED_SNAPSHOTS);
        assert_eq!(
            interpolator
                .snapshots()
                .back()
                .map(|snapshot| snapshot.game_tick),
            Some(ticks - 1)
        );
        // A repeated tick neither duplicates nor reorders.
        interpolator.receive_snapshot(ticks - 1, shift_pose(0.0));
        assert_eq!(interpolator.snapshots().len(), MAX_BUFFERED_SNAPSHOTS);
        assert_eq!(
            interpolator
                .snapshots()
                .back()
                .map(|snapshot| snapshot.pose.position[0]),
            Some(f64::from(ticks - 1))
        );
    }

    #[test]
    fn set_first_poses_seeds_pivot_and_position() {
        let mut interpolator = SableSnapshotInterpolator::new(SablePose3d::IDENTITY);
        let mut pivot_source = SablePose3d::IDENTITY;
        pivot_source.rotation_point = [1.0, 2.0, 3.0];
        let mut position_source = SablePose3d::IDENTITY;
        position_source.position = [4.0, 5.0, 6.0];
        interpolator.set_first_poses(&pivot_source, &position_source);
        assert_eq!(
            interpolator.interpolated_pose().rotation_point,
            [1.0, 2.0, 3.0]
        );
        assert_eq!(interpolator.interpolated_pose().position, [4.0, 5.0, 6.0]);
    }

    #[test]
    fn camera_relative_translation_is_centimetre_exact_at_1_28_million() {
        let base = 1_280_000.0;
        let pose = SablePose3d::new(
            [base + 0.3, 70.5, -base - 0.7],
            [0.0, 0.0, 0.0, 1.0],
            [base, 64.0, -base],
            SablePose3d::IDENTITY_SCALE,
        );
        let transform = pose.camera_relative_transform([0.0; 3], [0.0; 3]);
        assert!((f64::from(transform.translation[0]) - 0.3).abs() < 1e-6);
        assert!((f64::from(transform.translation[1]) - 6.5).abs() < 1e-6);
        assert!((f64::from(transform.translation[2]) + 0.7).abs() < 1e-6);
        // Casting the absolute coordinates to `f32` before subtracting loses
        // centimetres: the f32 spacing at 1.28e6 is 0.125.
        let naive = pose.position[0] as f32 - pose.rotation_point[0] as f32;
        assert!(
            (f64::from(naive) - 0.3).abs() > 0.02,
            "naive f32 path was unexpectedly exact: {naive}"
        );
        // A chunk-relative origin keeps the same precision.
        let shifted = SablePose3d::new(
            [base + 0.3, 70.5, -base - 0.7],
            [0.0, 0.0, 0.0, 1.0],
            [0.0; 3],
            SablePose3d::IDENTITY_SCALE,
        )
        .camera_relative_transform([0.0; 3], [-base, 0.0, base]);
        assert!((f64::from(shifted.translation[0]) - 0.3).abs() < 1e-5);
        assert!((f64::from(shifted.translation[2]) + 0.7).abs() < 1e-5);
        // The camera is subtracted in f64 as well.
        let at_camera = pose.camera_relative_transform([10.0, 4.0, -20.0], [0.0; 3]);
        assert!((f64::from(at_camera.translation[0]) + 9.7).abs() < 1e-6);
        assert!((f64::from(at_camera.translation[1]) - 2.5).abs() < 1e-6);
        assert!((f64::from(at_camera.translation[2]) - 19.3).abs() < 1e-6);
        // The basis is the identity for the identity orientation.
        assert_eq!(
            transform.basis,
            [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0]
        );
        let applied = transform.apply([1.0, 2.0, 3.0]);
        assert!((applied[0] - (1.0 + transform.translation[0])).abs() < 1e-6);
        assert!((applied[1] - (2.0 + transform.translation[1])).abs() < 1e-6);
        assert!((applied[2] - (3.0 + transform.translation[2])).abs() < 1e-6);
        // Scale multiplies each basis column.
        let mut scaled = SablePose3d::IDENTITY;
        scaled.scale = [2.0, 3.0, 4.0];
        let scaled = scaled.camera_relative_transform([0.0; 3], [0.0; 3]);
        assert_eq!(scaled.basis, [2.0, 0.0, 0.0, 0.0, 3.0, 0.0, 0.0, 0.0, 4.0]);
        // A 90 degree roll about Z maps +X to +Y.
        let mut yawed = SablePose3d::IDENTITY;
        yawed.orientation = about_z(std::f64::consts::FRAC_PI_2);
        let rotated = yawed.camera_relative_transform([0.0; 3], [0.0; 3]);
        let mapped = rotated.apply([1.0, 0.0, 0.0]);
        assert!(mapped[1] > 0.99, "{mapped:?}");
        assert!(mapped[0].abs() < 1e-5, "{mapped:?}");
        assert!(mapped[2].abs() < 1e-5, "{mapped:?}");
    }

    #[test]
    fn render_rate_slerp_rotates_uniformly() {
        let mut target = SablePose3d::IDENTITY;
        target.orientation = about_z(std::f64::consts::FRAC_PI_2);
        let halfway = SablePose3d::IDENTITY.slerp_to(&target, 0.5);
        let angle = quat_angle(SablePose3d::IDENTITY.orientation, halfway.orientation);
        assert!(
            (angle - std::f64::consts::FRAC_PI_4).abs() < 1e-5,
            "{angle}"
        );
        // slerp keeps a constant angular rate; nlerp lags behind it.
        let quarter_slerp = SablePose3d::IDENTITY.slerp_to(&target, 0.25);
        let quarter_nlerp = SablePose3d::IDENTITY.nlerp_to(&target, 0.25);
        let slerp_angle = quat_angle(SablePose3d::IDENTITY.orientation, quarter_slerp.orientation);
        let nlerp_angle = quat_angle(SablePose3d::IDENTITY.orientation, quarter_nlerp.orientation);
        assert!(
            (slerp_angle - std::f64::consts::FRAC_PI_4 / 2.0).abs() < 1e-5,
            "{slerp_angle}"
        );
        assert!(nlerp_angle < slerp_angle, "{nlerp_angle} {slerp_angle}");
        assert!((slerp_angle - nlerp_angle).abs() < 0.02);
        // Endpoints are exact.
        assert_eq!(
            SablePose3d::IDENTITY.slerp_to(&target, 0.0),
            SablePose3d::IDENTITY
        );
        let end = SablePose3d::IDENTITY.slerp_to(&target, 1.0);
        assert!(quat_angle(end.orientation, target.orientation).abs() < 1e-5);
        // Scale and pivot still lerp at render rate.
        let mut scaled = target;
        scaled.scale = [2.0, 2.0, 2.0];
        scaled.rotation_point = [4.0, 0.0, 0.0];
        let blended = SablePose3d::IDENTITY.slerp_to(&scaled, 0.25);
        assert_eq!(blended.scale, [1.25, 1.25, 1.25]);
        assert_eq!(blended.rotation_point, [1.0, 0.0, 0.0]);
        // Rotating by the negated quaternion is the same rotation.
        let mut negated = target;
        negated.orientation = negated.orientation.map(|value| -value);
        assert!(quat_angle(target.orientation, negated.orientation).abs() < 1e-6);
        assert!(quat_angle([0.0, 0.0, 0.0, 1.0], [1.0, 0.0, 0.0, 0.0]) > 3.0);
        // A degenerate quaternion normalizes back to identity rather than NaN.
        assert_eq!(quat_normalize([0.0, 0.0, 0.0, 0.0]), IDENTITY_ORIENTATION);
    }

    fn sample_bounds() -> SablePlotBounds {
        SablePlotBounds {
            min: [-64, -32, -64],
            max: [64, 32, 64],
        }
    }

    #[test]
    fn lifecycle_packets_roundtrip() {
        let start = SableTrackingStart {
            plot_coordinate: pack_plot_coordinate([2, -3]),
            sub_level_id: [0xab; 16],
            last_pose: sample_pose(),
            pose: SablePose3d::IDENTITY,
            bounds: sample_bounds(),
            name: Some("galleon".to_string()),
            game_tick: 7,
        };
        assert_eq!(
            SableTrackingStart::decode(&start.encode().expect("encode")),
            Ok(start.clone())
        );
        let unnamed = SableTrackingStart {
            name: None,
            ..start.clone()
        };
        assert_eq!(
            SableTrackingStart::decode(&unnamed.encode().expect("encode")),
            Ok(unnamed)
        );
        // The optional-name flag is a strict boolean at a known offset.
        let with_name = start.encode().expect("encode");
        let flag_index = 8 + 16 + 2 * SABLE_POSE_WIRE_BYTES + 24;
        assert_eq!(with_name[flag_index], 1);
        assert_eq!(with_name[flag_index + 1], 7u8);
        let mut broken = with_name;
        broken[flag_index] = 2;
        assert!(SableTrackingStart::decode(&broken).is_err());
        // An over-long name is rejected instead of writing a 32 KiB body.
        let huge = SableTrackingStart {
            name: Some("n".repeat(MAX_STRING_CHARS + 1)),
            ..start
        };
        assert!(huge.encode().is_err());

        for signal in [
            SablePlotSignal { plot_coordinate: 0 },
            SablePlotSignal {
                plot_coordinate: -1,
            },
        ] {
            assert_eq!(
                SablePlotSignal::decode(&signal.encode().expect("encode")),
                Ok(signal)
            );
        }

        let change = SableChangeBounds {
            plot_coordinate: pack_plot_coordinate([-1, 1]),
            bounds: sample_bounds(),
        };
        let encoded = change.encode().expect("encode");
        assert_eq!(encoded.len(), 8 + 24);
        assert_eq!(&encoded[8..12], &(-64i32).to_be_bytes());
        assert_eq!(SableChangeBounds::decode(&encoded), Ok(change));

        let activation = SableUdpActivation { player_id: [9; 16] };
        assert_eq!(
            SableUdpActivation::decode(&activation.encode().expect("encode")),
            Ok(activation)
        );

        let freeze = SableFreezePlayer {
            sub_level_id: [1; 16],
            local_position: [1.5, -2.5, 3.5],
        };
        assert_eq!(
            SableFreezePlayer::decode(&freeze.encode().expect("encode")),
            Ok(freeze)
        );

        let punch = SablePunchSubLevel {
            punched_block: [-3, 200, 8],
            local_position: [0.5, 0.5, 0.5],
            direction: [0.0, -1.0, 0.0],
        };
        assert_eq!(
            SablePunchSubLevel::decode(&punch.encode().expect("encode")),
            Ok(punch)
        );

        let gizmo = SableGizmoMove {
            sub_level_id: [7; 16],
            position: [10.0, 20.0, 30.0],
        };
        assert_eq!(
            SableGizmoMove::decode(&gizmo.encode().expect("encode")),
            Ok(gizmo)
        );

        let split = SableRecentlySplit {
            split_sub_level_id: [2; 16],
            split_from_id: [3; 16],
            pose: sample_pose(),
        };
        let encoded = split.encode().expect("encode");
        assert_eq!(encoded.len(), 32 + SABLE_POSE_WIRE_BYTES);
        assert_eq!(SableRecentlySplit::decode(&encoded), Ok(split));

        let mut non_finite = punch;
        non_finite.direction[0] = f64::NAN;
        assert!(non_finite.encode().is_err());
        let mut non_finite = gizmo;
        non_finite.position[2] = f64::INFINITY;
        assert!(non_finite.encode().is_err());
        let mut non_finite = freeze;
        non_finite.local_position[0] = f64::NAN;
        assert!(non_finite.encode().is_err());
    }

    #[test]
    fn lifecycle_decoders_reject_truncation_and_trailing_bytes() {
        let start = SableTrackingStart {
            plot_coordinate: 0,
            sub_level_id: [0; 16],
            last_pose: SablePose3d::IDENTITY,
            pose: SablePose3d::IDENTITY,
            bounds: SablePlotBounds {
                min: [0; 3],
                max: [0; 3],
            },
            name: None,
            game_tick: 0,
        };
        let encoded = start.encode().expect("encode");
        assert_eq!(encoded.len(), 8 + 16 + 64 + 64 + 24 + 1 + 4);
        for length in [0, 8, 24, 32, 96, encoded.len() - 1] {
            assert!(
                SableTrackingStart::decode(&encoded[..length]).is_err(),
                "{length}"
            );
        }
        let mut longer = encoded;
        longer.push(0);
        assert!(SableTrackingStart::decode(&longer).is_err());
        assert_eq!(
            SableChangeBounds::decode(&[0; 31]),
            Err(ProtoError::UnexpectedEof {
                needed: 4,
                remaining: 3
            })
        );
        assert_eq!(
            SablePlotSignal::decode(&[0; 8]),
            Ok(SablePlotSignal { plot_coordinate: 0 })
        );
        assert!(SablePlotSignal::decode(&[0; 7]).is_err());
        assert!(SablePlotSignal::decode(&[0; 9]).is_err());
        assert!(SableUdpActivation::decode(&[0; 15]).is_err());
        assert!(SableFreezePlayer::decode(&[0; 39]).is_err());
        assert!(SableRecentlySplit::decode(&[0; 95]).is_err());
    }
}
