// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Create: Aeronautics-style 6-DOF rigid-body sub-worlds: sparse local voxel
//! grids with an embedded live [`CreateFactory`] (propellers, shafts, belts
//! and machines keep ticking in flight), a deterministic fixed-step 20 TPS
//! integrator (thrust, rudder torque, altitude-tapered buoyancy, quadratic
//! drag, Euler angular dynamics), exact coordinate transforms and passenger
//! carry deltas for `mc-spine`'s `ac_note_carried_motion` anti-cheat hook,
//! and a deterministic binary `AERO` v1 Save codec.

use std::collections::{BTreeMap, BTreeSet};

use mod_adapter_core::neoforge_wire::{
    read_mc_string, read_varint, write_mc_string, write_varint,
};
use mod_adapter_core::version_compat::{data_version_for, McVersion};
use mod_offload::kinetic_graph::NetworkStatus;

use crate::addon_profile::{CreateAddonProfile, PropulsionSpec};
use crate::blockstates::CreateBlockRegistry;
use crate::factory::{Axis, CreateFactory, DimBlockPos};

/// Deterministic binary snapshot magic (`"AERO"`).
pub const AERO_MAGIC: [u8; 4] = *b"AERO";
/// Snapshot format version.
pub const AERO_VERSION: u8 = 1;
/// `AERO2` version-tagged envelope version byte (header + `aero_v1` payload).
pub const AERO2_VERSION: u8 = 2;
/// Mojang `DataVersion` for the `1.21.1` target (`3955`): stamped on `AERO2`
/// envelopes encoded for that target and reported when decoding a legacy
/// `AERO` v1 snapshot that carries no version tag of its own.
pub const AERO_DATA_VERSION_1_21_1: i32 = 3955;
/// Mojang `DataVersion` for the latest vanilla `26.3` target (`5023`).
pub const AERO_DATA_VERSION_26_3: i32 = 5023;
/// Default per-block mass (kg) when the caller has no better table.
pub const DEFAULT_BLOCK_MASS: f64 = 100.0;
/// Sea-level air density normalization for `step`'s `air_density` argument.
pub const SEA_LEVEL_AIR_DENSITY: f64 = 1.0;
/// Hard linear speed bound (m/s); exceeded components are renormalized.
pub const MAX_SPEED: f64 = 256.0;
/// Hard angular speed bound (rad/s).
pub const MAX_OMEGA: f64 = 32.0;
/// Per-region work budget for the embedded kinetic/transport schedulers.
const INTERNAL_REGION_STEPS: usize = 1_024;
/// Maximum canonical blockstate string length accepted by the codec.
const MAX_BLOCKSTATE_LEN: usize = 32_767;

#[derive(Clone, Debug, PartialEq)]
pub enum AeronauticsError {
    EmptyAssembly,
    InvalidMass,
    CorruptSnapshot(String),
}

/// A spinning propeller/thruster mount fixed to the sub-world. `axis` is the
/// local thrust axis; live RPM is read from the embedded [`CreateFactory`]
/// every [`RigidBodySubWorld::step`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PropellerMount {
    pub local: [i32; 3],
    pub axis: Axis,
    /// RPM floor below which the mount produces no thrust.
    pub min_rpm: f64,
    /// Newtons of thrust per solved RPM (signed; negative RPM reverses).
    pub thrust_per_rpm: f64,
}

/// A rudder/helm mount converting local airflow into control torque
/// (weathercock model): `τ[torque_axis] -= gain * v_local[sense_axis]`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RudderMount {
    pub local: [i32; 3],
    pub torque_axis: Axis,
    pub sense_axis: Axis,
    pub gain: f64,
}

/// A flying 6-DOF rigid-body sub-world generalizing Create's 1-axis
/// contraptions: compatible with Create: Aeronautics, Create: Simulated,
/// Clockwork, and Valkyrien Skies-style airships.
///
/// - `pos` is the world-space center of mass; `quat` is the unit quaternion
///   `[x, y, z, w]` mapping local -> world.
/// - `vel` is m/s and `omega` is rad/s in the world frame; `step(dt, ..)`
///   takes `dt` in seconds (fixed 0.05 s at 20 TPS).
/// - `blocks` is the sparse local voxel grid keyed by local block position,
///   storing canonical `"namespace:block[k=v]"` blockstate strings so a
///   reordered runtime registry remaps cleanly (Save v2 rule).
pub struct RigidBodySubWorld {
    pub id: u64,
    /// Owning dimension (`0` overworld .. `3+` dynamic custom dimensions).
    pub parent_dim: u32,
    pub pos: [f64; 3],
    pub quat: [f64; 4],
    pub vel: [f64; 3],
    pub omega: [f64; 3],
    pub blocks: BTreeMap<[i32; 3], String>,
    factory: CreateFactory,
    propulsion: PropulsionSpec,
    block_mass: f64,
    tick: u64,
    /// `block_id -> min_rpm` gate for propeller mounts (from the addon's
    /// kinetic consumer table; restored from mount data on decode).
    min_rpms: BTreeMap<String, f64>,
    pub mass: f64,
    pub center_of_mass: [f64; 3],
    pub inertia_diag: [f64; 3],
    pub buoyancy_blocks: u32,
    pub propellers: Vec<PropellerMount>,
    pub rudders: Vec<RudderMount>,
}

impl RigidBodySubWorld {
    /// Assemble a sub-world from a sparse local block map of canonical
    /// blockstate strings. `addon` supplies the propulsion model and
    /// `min_rpm` table (pass `Some(&createaeronautics_profile())` or a
    /// discovered profile; `None` uses [`PropulsionSpec::aeronautics_default`]
    /// with name-heuristic mounts). The embedded [`CreateFactory`] is rebuilt
    /// from `registry` so kinetic networks inside the airship keep ticking.
    pub fn assemble(
        id: u64,
        parent_dim: u32,
        cells: BTreeMap<[i32; 3], String>,
        registry: &CreateBlockRegistry,
        addon: Option<&CreateAddonProfile>,
        block_mass: f64,
    ) -> Result<Self, AeronauticsError> {
        if cells.is_empty() {
            return Err(AeronauticsError::EmptyAssembly);
        }
        if !(block_mass.is_finite() && block_mass > 0.0) {
            return Err(AeronauticsError::InvalidMass);
        }
        let min_rpms: BTreeMap<String, f64> = addon
            .map(|a| {
                a.kinetic_consumers
                    .iter()
                    .map(|c| (c.id.clone(), c.min_rpm))
                    .collect()
            })
            .unwrap_or_default();
        let propulsion = addon
            .and_then(|a| a.propulsion.clone())
            .unwrap_or_else(PropulsionSpec::aeronautics_default);
        let mut sub = Self {
            id,
            parent_dim,
            pos: [0.0; 3],
            quat: [0.0, 0.0, 0.0, 1.0],
            vel: [0.0; 3],
            omega: [0.0; 3],
            blocks: cells,
            factory: CreateFactory::new(registry.clone()),
            propulsion,
            block_mass,
            tick: 0,
            min_rpms,
            mass: 0.0,
            center_of_mass: [0.0; 3],
            inertia_diag: [0.0; 3],
            buoyancy_blocks: 0,
            propellers: Vec::new(),
            rudders: Vec::new(),
        };
        sub.rebuild_factory();
        sub.recompute_physics();
        Ok(sub)
    }

    /// The embedded live factory (kinetic networks, transport, fluids).
    pub fn factory(&self) -> &CreateFactory {
        &self.factory
    }

    /// Ticks actually stepped (drives the internal region schedulers).
    pub fn tick(&self) -> u64 {
        self.tick
    }

    /// Active propulsion model for this sub-world.
    pub fn propulsion(&self) -> &PropulsionSpec {
        &self.propulsion
    }

    /// Place/replace a local block (canonical blockstate string), updating the
    /// embedded kinetic graph and recomputing mass/CoM/inertia/mounts.
    pub fn set_block(&mut self, local: [i32; 3], canonical: impl Into<String>) {
        self.blocks.insert(local, canonical.into());
        self.rebuild_factory();
        self.recompute_physics();
    }

    /// Remove a local block and recompute physical properties.
    pub fn remove_block(&mut self, local: [i32; 3]) -> bool {
        if self.blocks.remove(&local).is_none() {
            return false;
        }
        self.rebuild_factory();
        self.recompute_physics();
        true
    }

    fn rebuild_factory(&mut self) {
        self.factory = CreateFactory::new(self.factory.registry().clone());
        let dim = self.parent_dim as i32;
        for (local, canonical) in &self.blocks {
            if let Some(state_id) = self.factory.registry().decode_canonical(canonical) {
                let pos = DimBlockPos::new(dim, local[0], local[1], local[2]);
                let _ = self.factory.place_block(pos, state_id);
            }
        }
    }

    /// Recompute mass, center of mass, diagonal inertia tensor, buoyancy
    /// count, and propeller/rudder mounts from the current `blocks` map.
    /// Called on assemble and on every `set_block`/`remove_block` edit.
    pub fn recompute_physics(&mut self) {
        let n = self.blocks.len() as f64;
        self.mass = n * self.block_mass;
        let mut com = [0.0f64; 3];
        for p in self.blocks.keys() {
            for i in 0..3 {
                com[i] += f64::from(p[i]) + 0.5;
            }
        }
        for c in &mut com {
            *c = sanitize(*c / n.max(1e-9));
        }
        self.center_of_mass = com;

        let mut inertia = [0.0f64; 3];
        for p in self.blocks.keys() {
            let d = [
                f64::from(p[0]) + 0.5 - com[0],
                f64::from(p[1]) + 0.5 - com[1],
                f64::from(p[2]) + 0.5 - com[2],
            ];
            inertia[0] += self.block_mass * (d[1] * d[1] + d[2] * d[2]);
            inertia[1] += self.block_mass * (d[0] * d[0] + d[2] * d[2]);
            inertia[2] += self.block_mass * (d[0] * d[0] + d[1] * d[1]);
        }
        for i in &mut inertia {
            *i = sanitize(*i).max(1e-9);
        }
        self.inertia_diag = inertia;

        let prop_set: BTreeSet<&str> = self
            .propulsion
            .propeller_blocks
            .iter()
            .map(String::as_str)
            .collect();
        let buoy_set: BTreeSet<&str> = self
            .propulsion
            .buoyancy_blocks
            .iter()
            .map(String::as_str)
            .collect();
        let rud_set: BTreeSet<&str> = self
            .propulsion
            .rudder_blocks
            .iter()
            .map(String::as_str)
            .collect();

        self.buoyancy_blocks = 0;
        self.propellers.clear();
        self.rudders.clear();
        for (local, canonical) in &self.blocks {
            let block_id = canonical_block_id(canonical);
            let path = id_path(block_id);
            if prop_set.contains(block_id) || path.contains("propeller") {
                self.propellers.push(PropellerMount {
                    local: *local,
                    axis: axis_from_canonical(canonical),
                    min_rpm: self.min_rpms.get(block_id).copied().unwrap_or(0.0),
                    thrust_per_rpm: self.propulsion.thrust_per_rpm,
                });
            } else if rud_set.contains(block_id) || path.contains("rudder") {
                self.rudders.push(RudderMount {
                    local: *local,
                    torque_axis: Axis::Y,
                    sense_axis: Axis::X,
                    gain: self.propulsion.rudder_torque,
                });
            } else if buoy_set.contains(block_id) || is_buoyant_path(path) {
                self.buoyancy_blocks = self.buoyancy_blocks.saturating_add(1);
            }
        }
    }

    /// Live solved RPM of the propeller mount's kinetic node (0 when
    /// unpowered/overstressed or when the block was never registered).
    fn propeller_rpm(&self, mount: &PropellerMount) -> f64 {
        let pos = DimBlockPos::new(
            self.parent_dim as i32,
            mount.local[0],
            mount.local[1],
            mount.local[2],
        );
        match self.factory.kinetic_solution(pos) {
            Some(sol) if sol.status == NetworkStatus::Running && sol.speed.is_finite() => sol.speed,
            _ => 0.0,
        }
    }

    fn buoyancy_force(&self, air_density: f64) -> f64 {
        let p = &self.propulsion;
        let span = (p.max_lift_altitude - p.lift_full_altitude).max(1e-9);
        let taper = ((p.max_lift_altitude - self.pos[1]) / span).clamp(0.0, 1.0);
        sanitize(f64::from(self.buoyancy_blocks) * p.lift_per_buoyancy_block * air_density * taper)
    }

    /// Advance the whole sub-world one deterministic fixed step: the embedded
    /// [`CreateFactory`] ticks first (live RPMs), then local propeller thrust
    /// and rudder torque are rotated into the world frame, gravity,
    /// altitude-tapered buoyancy, quadratic drag and Euler angular dynamics
    /// integrate `vel`/`pos`/`omega`/`quat`. `gravity` is the downward
    /// acceleration applied along -Y (e.g. `9.81`). Every input and derived
    /// value is sanitized: the integrator can never produce NaN/Inf and
    /// speeds stay bounded by [`MAX_SPEED`]/[`MAX_OMEGA`].
    pub fn step(&mut self, dt: f64, gravity: f64, air_density: f64) {
        if !dt.is_finite() || dt <= 0.0 {
            return;
        }
        let gravity = if gravity.is_finite() { gravity } else { 0.0 };
        let air_density = if air_density.is_finite() && air_density >= 0.0 {
            air_density
        } else {
            0.0
        };
        self.tick = self.tick.saturating_add(1);
        let _ = self.factory.step_tick(self.tick, INTERNAL_REGION_STEPS, None);

        let conj = quat_conjugate(self.quat);
        // Ship-relative airflow and spin in the local (body) frame.
        let v_local = quat_rotate(conj, self.vel);
        let omega_local_in = quat_rotate(conj, self.omega);

        // Local thrust from live propeller RPMs.
        let mut thrust_local = [0.0f64; 3];
        for mount in &self.propellers {
            let rpm = self.propeller_rpm(mount);
            if rpm.abs() >= mount.min_rpm {
                thrust_local[mount.axis.index()] += rpm * mount.thrust_per_rpm;
            }
        }
        let thrust_world = quat_rotate(self.quat, thrust_local);

        // Rudder weathercock torque + angular damping (local frame). Control
        // authority scales with dynamic pressure (~ρ·v).
        let mut torque_local = [0.0f64; 3];
        for rudder in &self.rudders {
            torque_local[rudder.torque_axis.index()] -=
                rudder.gain * air_density * v_local[rudder.sense_axis.index()];
        }
        for i in 0..3 {
            torque_local[i] -= self.propulsion.angular_drag_coeff * air_density * omega_local_in[i];
        }

        // World-frame linear force balance.
        let speed = norm3(self.vel);
        let drag = self.propulsion.drag_coeff * air_density * speed;
        let mut accel = [0.0f64; 3];
        for i in 0..3 {
            let force = sanitize(
                thrust_world[i] - drag * self.vel[i]
                    + if i == 1 {
                        self.buoyancy_force(air_density) - self.mass * gravity
                    } else {
                        0.0
                    },
            );
            accel[i] = force / self.mass.max(1e-9);
        }
        for (v, &a) in self.vel.iter_mut().zip(&accel) {
            *v = sanitize(*v + a * dt);
        }
        clamp_magnitude(&mut self.vel, MAX_SPEED);
        for (p, &v) in self.pos.iter_mut().zip(&self.vel) {
            *p = sanitize(*p + v * dt);
        }

        // Euler angular dynamics in the body frame: I*ω̇ = τ - ω×(I·ω).
        let mut omega_local = omega_local_in;
        let iw = [
            omega_local[0] * self.inertia_diag[0],
            omega_local[1] * self.inertia_diag[1],
            omega_local[2] * self.inertia_diag[2],
        ];
        let gyro = cross3(omega_local, iw);
        for i in 0..3 {
            let alpha = sanitize(
                (torque_local[i] - gyro[i]) / self.inertia_diag[i].max(1e-9),
            );
            omega_local[i] = sanitize(omega_local[i] + alpha * dt);
        }
        self.omega = quat_rotate(self.quat, omega_local);
        clamp_magnitude(&mut self.omega, MAX_OMEGA);

        // q̇ = 1/2 * [ω_world, 0] ⊗ q (world-frame angular velocity).
        let wq = [self.omega[0], self.omega[1], self.omega[2], 0.0];
        let qdot = quat_mul(wq, self.quat);
        for (q, &dq) in self.quat.iter_mut().zip(&qdot) {
            *q += 0.5 * dq * dt;
        }
        self.quat = quat_normalize(self.quat);
    }

    /// Exact world-space displacement of a carried passenger over `dt` given
    /// their rigid-body point `local_offset` (the rider's local position
    /// measured from [`Self::center_of_mass`]): `(v + ω × (R·offset)) * dt`.
    /// Feeds `mc-spine`'s `ac_note_carried_motion` hook so anti-cheat never
    /// rubber-bands airship pilots or passengers.
    pub fn passenger_world_delta(&self, local_offset: [f64; 3], dt: f64) -> [f64; 3] {
        let r_world = quat_rotate(self.quat, local_offset);
        let tangential = cross3(self.omega, r_world);
        [
            (self.vel[0] + tangential[0]) * dt,
            (self.vel[1] + tangential[1]) * dt,
            (self.vel[2] + tangential[2]) * dt,
        ]
    }

    /// Local -> world transform of point `p` (local frame is centered on
    /// [`Self::center_of_mass`], rotated by `quat`, translated by `pos`).
    pub fn local_to_world(&self, p: [f64; 3]) -> [f64; 3] {
        let rel = [
            p[0] - self.center_of_mass[0],
            p[1] - self.center_of_mass[1],
            p[2] - self.center_of_mass[2],
        ];
        let r = quat_rotate(self.quat, rel);
        [self.pos[0] + r[0], self.pos[1] + r[1], self.pos[2] + r[2]]
    }

    /// Exact inverse of [`Self::local_to_world`] via the quaternion conjugate.
    pub fn world_to_local(&self, w: [f64; 3]) -> [f64; 3] {
        let rel = [
            w[0] - self.pos[0],
            w[1] - self.pos[1],
            w[2] - self.pos[2],
        ];
        let l = quat_rotate(quat_conjugate(self.quat), rel);
        [
            l[0] + self.center_of_mass[0],
            l[1] + self.center_of_mass[1],
            l[2] + self.center_of_mass[2],
        ]
    }

    /// Encode this sub-world as an `AERO2` version-tagged envelope: the
    /// legacy `AERO` v1 payload wrapped in a header carrying the Mojang
    /// `data_version` the snapshot was written for (e.g.
    /// [`AERO_DATA_VERSION_1_21_1`] = 3955, [`AERO_DATA_VERSION_26_3`] =
    /// 5023), the payload length, and an IEEE CRC32 of the payload. The
    /// inner `aero_v1` byte format is unchanged, so envelopes are fully
    /// additive. Decode with [`decode_snapshot_with_data_version`].
    pub fn encode_snapshot_for_data_version(&self, data_version: i32) -> Vec<u8> {
        let payload = encode_aeronautics_snapshot(self);
        let mut out = Vec::with_capacity(AERO2_HEADER_LEN + payload.len());
        out.extend_from_slice(&AERO_MAGIC);
        out.push(AERO2_VERSION);
        out.extend_from_slice(&data_version.to_be_bytes());
        out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        out.extend_from_slice(&crc32_ieee(&payload).to_be_bytes());
        out.extend_from_slice(&payload);
        out
    }
}

// ---------------------------------------------------------------------------
// Deterministic `AERO` v1 binary snapshot codec
// ---------------------------------------------------------------------------

/// Encode a sub-world snapshot: pose, velocities, physical properties,
/// propulsion spec, mounts, sparse blocks, and tick counter — everything the
/// decoder needs to rebuild the embedded kinetic state byte-for-byte.
pub fn encode_aeronautics_snapshot(sub: &RigidBodySubWorld) -> Vec<u8> {
    let mut out = Vec::with_capacity(256 + sub.blocks.len() * 24);
    out.extend_from_slice(&AERO_MAGIC);
    out.push(AERO_VERSION);
    w_u64(&mut out, sub.id);
    w_u32(&mut out, sub.parent_dim);
    w_f64s(&mut out, &sub.pos);
    w_f64s(&mut out, &sub.quat);
    w_f64s(&mut out, &sub.vel);
    w_f64s(&mut out, &sub.omega);
    w_f64(&mut out, sub.mass);
    w_f64s(&mut out, &sub.center_of_mass);
    w_f64s(&mut out, &sub.inertia_diag);
    w_f64(&mut out, sub.block_mass);
    w_u64(&mut out, sub.tick);
    w_u32(&mut out, sub.buoyancy_blocks);

    let p = &sub.propulsion;
    for v in [
        p.thrust_per_rpm,
        p.lift_per_buoyancy_block,
        p.drag_coeff,
        p.angular_drag_coeff,
        p.rudder_torque,
        p.lift_full_altitude,
        p.max_lift_altitude,
    ] {
        w_f64(&mut out, v);
    }
    for ids in [&p.propeller_blocks, &p.buoyancy_blocks, &p.rudder_blocks] {
        write_varint(&mut out, ids.len() as u32);
        for id in ids {
            write_mc_string(&mut out, id);
        }
    }

    write_varint(&mut out, sub.propellers.len() as u32);
    for m in &sub.propellers {
        w_i32s(&mut out, &m.local);
        out.push(m.axis.index() as u8);
        w_f64(&mut out, m.min_rpm);
        w_f64(&mut out, m.thrust_per_rpm);
    }
    write_varint(&mut out, sub.rudders.len() as u32);
    for m in &sub.rudders {
        w_i32s(&mut out, &m.local);
        out.push(m.torque_axis.index() as u8);
        out.push(m.sense_axis.index() as u8);
        w_f64(&mut out, m.gain);
    }

    write_varint(&mut out, sub.blocks.len() as u32);
    for (local, canonical) in &sub.blocks {
        w_i32s(&mut out, local);
        write_mc_string(&mut out, canonical);
    }
    out
}

/// Decode an `AERO` v1 snapshot, rebuilding the embedded [`CreateFactory`]
/// from `registry` (canonical strings remap cleanly across reordered
/// registries). Fails closed on any structural inconsistency; never trusts
/// non-finite state.
pub fn decode_aeronautics_snapshot(
    bytes: &[u8],
    registry: &CreateBlockRegistry,
) -> Result<RigidBodySubWorld, AeronauticsError> {
    if bytes.len() < 5 || bytes[..4] != AERO_MAGIC || bytes[4] != AERO_VERSION {
        return Err(AeronauticsError::CorruptSnapshot("bad magic/version".into()));
    }
    let mut c = 5usize;
    let corrupt = |what: &str| AeronauticsError::CorruptSnapshot(what.to_string());
    let id = r_u64(bytes, &mut c).ok_or_else(|| corrupt("truncated id"))?;
    let parent_dim = r_u32(bytes, &mut c).ok_or_else(|| corrupt("truncated dim"))?;
    let pos = r_f64s(bytes, &mut c).ok_or_else(|| corrupt("truncated pos"))?;
    let quat = r_quat(bytes, &mut c).ok_or_else(|| corrupt("truncated quat"))?;
    let vel = r_f64s(bytes, &mut c).ok_or_else(|| corrupt("truncated vel"))?;
    let omega = r_f64s(bytes, &mut c).ok_or_else(|| corrupt("truncated omega"))?;
    let mass = r_f64(bytes, &mut c).ok_or_else(|| corrupt("truncated mass"))?;
    let center_of_mass = r_f64s(bytes, &mut c).ok_or_else(|| corrupt("truncated com"))?;
    let inertia_diag = r_f64s(bytes, &mut c).ok_or_else(|| corrupt("truncated inertia"))?;
    let block_mass = r_f64(bytes, &mut c).ok_or_else(|| corrupt("truncated block_mass"))?;
    let tick = r_u64(bytes, &mut c).ok_or_else(|| corrupt("truncated tick"))?;
    let buoyancy_blocks = r_u32(bytes, &mut c).ok_or_else(|| corrupt("truncated buoyancy"))?;

    let mut propulsion = PropulsionSpec::aeronautics_default();
    for slot in [
        &mut propulsion.thrust_per_rpm,
        &mut propulsion.lift_per_buoyancy_block,
        &mut propulsion.drag_coeff,
        &mut propulsion.angular_drag_coeff,
        &mut propulsion.rudder_torque,
        &mut propulsion.lift_full_altitude,
        &mut propulsion.max_lift_altitude,
    ] {
        *slot = r_f64(bytes, &mut c).ok_or_else(|| corrupt("truncated propulsion"))?;
    }
    for ids in [
        &mut propulsion.propeller_blocks,
        &mut propulsion.buoyancy_blocks,
        &mut propulsion.rudder_blocks,
    ] {
        *ids = r_string_list(bytes, &mut c).ok_or_else(|| corrupt("truncated propulsion lists"))?;
    }

    let n_props = r_count(bytes, &mut c).ok_or_else(|| corrupt("bad propeller count"))?;
    let mut propellers = Vec::with_capacity(n_props.min(1024));
    for _ in 0..n_props {
        let local = r_i32s(bytes, &mut c).ok_or_else(|| corrupt("truncated propeller"))?;
        let axis = r_axis(bytes, &mut c).ok_or_else(|| corrupt("bad propeller axis"))?;
        let min_rpm = r_f64(bytes, &mut c).ok_or_else(|| corrupt("truncated propeller"))?;
        let thrust_per_rpm = r_f64(bytes, &mut c).ok_or_else(|| corrupt("truncated propeller"))?;
        propellers.push(PropellerMount {
            local,
            axis,
            min_rpm,
            thrust_per_rpm,
        });
    }
    let n_rudders = r_count(bytes, &mut c).ok_or_else(|| corrupt("bad rudder count"))?;
    let mut rudders = Vec::with_capacity(n_rudders.min(1024));
    for _ in 0..n_rudders {
        let local = r_i32s(bytes, &mut c).ok_or_else(|| corrupt("truncated rudder"))?;
        let torque_axis = r_axis(bytes, &mut c).ok_or_else(|| corrupt("bad rudder axis"))?;
        let sense_axis = r_axis(bytes, &mut c).ok_or_else(|| corrupt("bad rudder axis"))?;
        let gain = r_f64(bytes, &mut c).ok_or_else(|| corrupt("truncated rudder"))?;
        rudders.push(RudderMount {
            local,
            torque_axis,
            sense_axis,
            gain,
        });
    }

    let n_blocks = r_count(bytes, &mut c).ok_or_else(|| corrupt("bad block count"))?;
    let mut blocks = BTreeMap::new();
    for _ in 0..n_blocks {
        let local = r_i32s(bytes, &mut c).ok_or_else(|| corrupt("truncated blocks"))?;
        let canonical = read_mc_string(bytes, &mut c, MAX_BLOCKSTATE_LEN)
            .ok_or_else(|| corrupt("bad blockstate string"))?;
        blocks.insert(local, canonical);
    }
    if c != bytes.len() {
        return Err(corrupt("trailing bytes"));
    }
    // Rebuild the min_rpm gate table from the stored mounts so later edits
    // keep the profile's RPM floors.
    let mut min_rpms = BTreeMap::new();
    for m in &propellers {
        if let Some(canonical) = blocks.get(&m.local) {
            min_rpms.insert(canonical_block_id(canonical).to_string(), m.min_rpm);
        }
    }

    if !(mass.is_finite() && mass > 0.0)
        || !pos.iter().all(|v| v.is_finite())
        || !vel.iter().all(|v| v.is_finite())
        || !omega.iter().all(|v| v.is_finite())
        || !center_of_mass.iter().all(|v| v.is_finite())
        || !inertia_diag.iter().all(|v| v.is_finite() && *v > 0.0)
        || !block_mass.is_finite()
        || block_mass <= 0.0
    {
        return Err(corrupt("non-finite physical state"));
    }
    let quat = quat_normalize(quat);

    let mut sub = RigidBodySubWorld {
        id,
        parent_dim,
        pos,
        quat,
        vel,
        omega,
        blocks,
        factory: CreateFactory::new(registry.clone()),
        propulsion,
        block_mass,
        tick,
        min_rpms,
        mass,
        center_of_mass,
        inertia_diag,
        buoyancy_blocks,
        propellers,
        rudders,
    };
    sub.rebuild_factory();
    Ok(sub)
}

// ---------------------------------------------------------------------------
// `AERO2` version-tagged envelope (legacy `aero_v1` payload + data_version)
// ---------------------------------------------------------------------------

/// Byte length of the `AERO2` envelope header: 4 magic + 1 version +
/// 4 `data_version` + 4 `payload_len` + 4 CRC32.
const AERO2_HEADER_LEN: usize = 17;

/// Whether `data_version` is a Mojang `DataVersion` the shared
/// [`mod_adapter_core::version_compat`] table recognizes — the
/// `1.20.4`-`1.21.5` line plus the `26.3` target. `AERO2` decode fails
/// closed on anything else so a corrupt or unknown version tag can never
/// silently restore a sub-world.
pub fn is_known_aero_data_version(data_version: i32) -> bool {
    [
        McVersion::V1_20_4,
        McVersion::V1_20_6,
        McVersion::V1_21_0,
        McVersion::V1_21_1,
        McVersion::V1_21_3,
        McVersion::V1_21_4,
        McVersion::V1_21_5,
        McVersion::V26_3,
    ]
    .iter()
    .any(|&v| data_version_for(v) == Some(data_version))
}

/// Decode an `AERO2` version-tagged envelope *or* a legacy `AERO` v1
/// snapshot, returning the restored sub-world and the `data_version` the
/// snapshot claims. Legacy v1 inputs report [`AERO_DATA_VERSION_1_21_1`]
/// (`3955`, the target the format shipped under). `AERO2` payloads decode
/// through the unchanged `aero_v1` path, so a `3955` or `5023` envelope
/// restores identically to its bare `AERO` v1 bytes.
///
/// Fails closed on bad magic/version byte, truncated header or payload,
/// payload-length mismatch, CRC32 mismatch, unknown `data_version`, or any
/// corruption inside the wrapped `aero_v1` payload.
pub fn decode_snapshot_with_data_version(
    bytes: &[u8],
    registry: &CreateBlockRegistry,
) -> Result<(RigidBodySubWorld, i32), AeronauticsError> {
    let corrupt = |what: &str| AeronauticsError::CorruptSnapshot(what.to_string());
    if bytes.len() < 5 || bytes[..4] != AERO_MAGIC {
        return Err(corrupt("bad magic"));
    }
    if bytes[4] == AERO_VERSION {
        let sub = decode_aeronautics_snapshot(bytes, registry)?;
        return Ok((sub, AERO_DATA_VERSION_1_21_1));
    }
    if bytes[4] != AERO2_VERSION {
        return Err(corrupt("bad magic/version"));
    }
    let header = bytes
        .get(..AERO2_HEADER_LEN)
        .ok_or_else(|| corrupt("truncated AERO2 header"))?;
    let data_version = i32::from_be_bytes([header[5], header[6], header[7], header[8]]);
    let payload_len = u32::from_be_bytes([header[9], header[10], header[11], header[12]]) as usize;
    let crc = u32::from_be_bytes([header[13], header[14], header[15], header[16]]);
    let payload = &bytes[AERO2_HEADER_LEN..];
    if payload.len() != payload_len {
        return Err(corrupt("payload length mismatch"));
    }
    if !is_known_aero_data_version(data_version) {
        return Err(corrupt("unknown data_version"));
    }
    if crc32_ieee(payload) != crc {
        return Err(corrupt("crc32 mismatch"));
    }
    let sub = decode_aeronautics_snapshot(payload, registry)?;
    Ok((sub, data_version))
}

/// IEEE 802.3 CRC32 (poly `0xEDB88320`, reflected, init/xorout `0xFFFFFFFF`)
/// of `data` — the checksum zlib/JAR internals use, computed with a
/// zero-dependency compile-time table so `AERO2` envelopes detect
/// corruption before the inner `aero_v1` decoder reads a single byte.
fn crc32_ieee(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc = CRC32_IEEE_TABLE[((crc ^ u32::from(b)) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

const CRC32_IEEE_TABLE: [u32; 256] = crc32_table();

const fn crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

// ---------------------------------------------------------------------------
// Quaternion / vector math (deterministic f64, NaN-sanitized)
// ---------------------------------------------------------------------------

fn sanitize(v: f64) -> f64 {
    if v.is_finite() {
        v
    } else {
        0.0
    }
}

fn norm3(v: [f64; 3]) -> f64 {
    (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt()
}

fn clamp_magnitude(v: &mut [f64; 3], max: f64) {
    let n = norm3(*v);
    if n.is_finite() && n > max {
        let s = max / n;
        for c in v.iter_mut() {
            *c *= s;
        }
    }
}

fn cross3(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

/// Hamilton product `a ⊗ b` for quaternions `[x, y, z, w]`.
fn quat_mul(a: [f64; 4], b: [f64; 4]) -> [f64; 4] {
    [
        a[3] * b[0] + a[0] * b[3] + a[1] * b[2] - a[2] * b[1],
        a[3] * b[1] - a[0] * b[2] + a[1] * b[3] + a[2] * b[0],
        a[3] * b[2] + a[0] * b[1] - a[1] * b[0] + a[2] * b[3],
        a[3] * b[3] - a[0] * b[0] - a[1] * b[1] - a[2] * b[2],
    ]
}

fn quat_conjugate(q: [f64; 4]) -> [f64; 4] {
    [-q[0], -q[1], -q[2], q[3]]
}

fn quat_normalize(q: [f64; 4]) -> [f64; 4] {
    let n = (q[0] * q[0] + q[1] * q[1] + q[2] * q[2] + q[3] * q[3]).sqrt();
    if !n.is_finite() || n < 1e-12 {
        return [0.0, 0.0, 0.0, 1.0];
    }
    [q[0] / n, q[1] / n, q[2] / n, q[3] / n]
}

/// Rotate vector `v` by unit quaternion `q` (`q ⊗ [v,0] ⊗ q̄`).
fn quat_rotate(q: [f64; 4], v: [f64; 3]) -> [f64; 3] {
    let qv = [q[0], q[1], q[2]];
    let t = cross3(qv, v);
    let t2 = [2.0 * t[0], 2.0 * t[1], 2.0 * t[2]];
    let u = cross3(qv, t2);
    [
        v[0] + q[3] * t2[0] + u[0],
        v[1] + q[3] * t2[1] + u[1],
        v[2] + q[3] * t2[2] + u[2],
    ]
}

// ---------------------------------------------------------------------------
// Codec scalar/string helpers (big-endian scalars, VarInt counts, MC strings)
// ---------------------------------------------------------------------------

fn w_f64(out: &mut Vec<u8>, v: f64) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn w_f64s(out: &mut Vec<u8>, vs: &[f64]) {
    for v in vs {
        w_f64(out, *v);
    }
}

fn w_i32s(out: &mut Vec<u8>, vs: &[i32; 3]) {
    for v in vs {
        out.extend_from_slice(&v.to_be_bytes());
    }
}

fn w_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn w_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn r_u64(bytes: &[u8], c: &mut usize) -> Option<u64> {
    let s = bytes.get(*c..*c + 8)?;
    *c += 8;
    Some(u64::from_be_bytes(s.try_into().ok()?))
}

fn r_u32(bytes: &[u8], c: &mut usize) -> Option<u32> {
    let s = bytes.get(*c..*c + 4)?;
    *c += 4;
    Some(u32::from_be_bytes(s.try_into().ok()?))
}

fn r_f64(bytes: &[u8], c: &mut usize) -> Option<f64> {
    let s = bytes.get(*c..*c + 8)?;
    *c += 8;
    Some(f64::from_be_bytes(s.try_into().ok()?))
}

fn r_f64s(bytes: &[u8], c: &mut usize) -> Option<[f64; 3]> {
    Some([
        r_f64(bytes, c)?,
        r_f64(bytes, c)?,
        r_f64(bytes, c)?,
    ])
}

fn r_quat(bytes: &[u8], c: &mut usize) -> Option<[f64; 4]> {
    Some([
        r_f64(bytes, c)?,
        r_f64(bytes, c)?,
        r_f64(bytes, c)?,
        r_f64(bytes, c)?,
    ])
}

fn r_i32s(bytes: &[u8], c: &mut usize) -> Option<[i32; 3]> {
    let mut out = [0i32; 3];
    for v in &mut out {
        let s = bytes.get(*c..*c + 4)?;
        *c += 4;
        *v = i32::from_be_bytes(s.try_into().ok()?);
    }
    Some(out)
}

fn r_axis(bytes: &[u8], c: &mut usize) -> Option<Axis> {
    let b = *bytes.get(*c)?;
    *c += 1;
    match b {
        0 => Some(Axis::X),
        1 => Some(Axis::Y),
        2 => Some(Axis::Z),
        _ => None,
    }
}

fn r_count(bytes: &[u8], c: &mut usize) -> Option<usize> {
    let n = read_varint(bytes, c)? as usize;
    // Each record needs at least 1 byte; an inflated count is provably corrupt.
    (n <= bytes.len()).then_some(n)
}

fn r_string_list(bytes: &[u8], c: &mut usize) -> Option<Vec<String>> {
    let n = r_count(bytes, c)?;
    let mut out = Vec::with_capacity(n.min(1024));
    for _ in 0..n {
        out.push(read_mc_string(bytes, c, MAX_BLOCKSTATE_LEN)?);
    }
    Some(out)
}

fn canonical_block_id(canonical: &str) -> &str {
    canonical.split('[').next().unwrap_or(canonical)
}

fn id_path(id: &str) -> &str {
    id.split_once(':').map(|(_, p)| p).unwrap_or(id)
}

fn is_buoyant_path(path: &str) -> bool {
    path.contains("balloon")
        || path.contains("envelope")
        || path.contains("blimp")
        || path.contains("levitat")
        || path.contains("float")
}

/// Rotation axis declared by a canonical blockstate string's `axis=` or
/// `facing=` property (default `Y` when absent — Create's fallback).
fn axis_from_canonical(canonical: &str) -> Axis {
    let inner = canonical
        .split_once('[')
        .and_then(|(_, rest)| rest.strip_suffix(']'))
        .unwrap_or("");
    let mut axis_val = None;
    let mut facing_val = None;
    for pair in inner.split(',') {
        if let Some((k, v)) = pair.split_once('=') {
            match k.trim() {
                "axis" => axis_val = Some(v.trim()),
                "facing" => facing_val = Some(v.trim()),
                _ => {}
            }
        }
    }
    axis_val
        .and_then(Axis::from_name)
        .or_else(|| facing_val.and_then(axis_from_facing))
        .unwrap_or(Axis::Y)
}

fn axis_from_facing(facing: &str) -> Option<Axis> {
    match facing {
        "east" | "west" => Some(Axis::X),
        "up" | "down" => Some(Axis::Y),
        "north" | "south" => Some(Axis::Z),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::addon_profile::{
        builtin_create_addon_profiles, create_registry_with_addons, createaeronautics_profile,
        PropulsionSpec,
    };
    use crate::blockstates::DEFAULT_BASE_STATE_ID;

    fn aero_registry() -> (CreateAddonProfile, CreateBlockRegistry) {
        let addon = createaeronautics_profile();
        let reg =
            create_registry_with_addons(DEFAULT_BASE_STATE_ID, std::slice::from_ref(&addon))
                .unwrap();
        (addon, reg)
    }

    fn canonical(reg: &CreateBlockRegistry, id: &str) -> String {
        reg.encode_canonical(reg.default_state(id).unwrap())
            .unwrap()
            .to_string()
    }

    fn canonical_with(reg: &CreateBlockRegistry, id: &str, prop: &str, val: &str) -> String {
        let st = reg
            .with_property(reg.default_state(id).unwrap(), prop, val)
            .unwrap();
        reg.encode_canonical(st).unwrap().to_string()
    }

    fn assert_finite(sub: &RigidBodySubWorld) {
        for v in sub.pos.iter().chain(&sub.vel).chain(&sub.omega).chain(&sub.quat) {
            assert!(
                v.is_finite(),
                "non-finite state: pos={:?} vel={:?} omega={:?} quat={:?}",
                sub.pos,
                sub.vel,
                sub.omega,
                sub.quat
            );
        }
    }

    #[test]
    fn assembly_computes_mass_com_inertia_and_mounts() {
        let (addon, reg) = aero_registry();
        let mut cells = BTreeMap::new();
        // Symmetric 4-block 2x2x1 raft centered on (1.0, 0.5, 0.5) plus
        // balloon, propeller bearing (facing=east -> X thrust) and rudder.
        let deck = canonical(&reg, "createaeronautics:airship_assembler");
        let envelope = canonical(&reg, "createaeronautics:balloon_envelope");
        let prop = canonical_with(
            &reg,
            "createaeronautics:propeller_bearing",
            "facing",
            "east",
        );
        let rudder = canonical(&reg, "createaeronautics:rudder");
        cells.insert([0, 0, 0], deck.clone());
        cells.insert([1, 0, 0], deck.clone());
        cells.insert([0, 0, 1], deck.clone());
        cells.insert([1, 0, 1], deck);
        cells.insert([0, 1, 0], envelope);
        cells.insert([2, 0, 0], prop);
        cells.insert([1, 0, 2], rudder);

        let sub = RigidBodySubWorld::assemble(1, 0, cells, &reg, Some(&addon), 100.0).unwrap();
        assert_eq!(sub.blocks.len(), 7);
        assert_eq!(sub.mass, 700.0);
        // CoM = mean of cell centers.
        let expected_com = [
            (0.5 + 1.5 + 0.5 + 1.5 + 0.5 + 2.5 + 1.5) / 7.0,
            4.5 / 7.0,
            (0.5 + 0.5 + 1.5 + 1.5 + 0.5 + 0.5 + 2.5) / 7.0,
        ];
        for (&got, &want) in sub.center_of_mass.iter().zip(&expected_com) {
            assert!((got - want).abs() < 1e-9);
        }
        assert_eq!(sub.buoyancy_blocks, 1);
        assert_eq!(sub.propellers.len(), 1);
        assert_eq!(sub.propellers[0].axis, Axis::X);
        assert_eq!(sub.propellers[0].min_rpm, 4.0);
        assert_eq!(sub.rudders.len(), 1);
        assert_eq!(sub.rudders[0].torque_axis, Axis::Y);
        // Inertia diagonal: I_x = Σ m(dy²+dz²) etc., all strictly positive.
        for &i in &sub.inertia_diag {
            assert!(i > 0.0 && i.is_finite());
        }
        // Symmetric deck contributes equal x/z inertia about its own axes.
        let mut manual = [0.0f64; 3];
        for p in sub.blocks.keys() {
            let d = [
                f64::from(p[0]) + 0.5 - expected_com[0],
                f64::from(p[1]) + 0.5 - expected_com[1],
                f64::from(p[2]) + 0.5 - expected_com[2],
            ];
            manual[0] += 100.0 * (d[1] * d[1] + d[2] * d[2]);
            manual[1] += 100.0 * (d[0] * d[0] + d[2] * d[2]);
            manual[2] += 100.0 * (d[0] * d[0] + d[1] * d[1]);
        }
        for (&got, &want) in sub.inertia_diag.iter().zip(&manual) {
            assert!((got - want).abs() < 1e-9);
        }

        // Edits recompute physicals.
        let mut sub = sub;
        sub.remove_block([0, 1, 0]);
        assert_eq!(sub.mass, 600.0);
        assert_eq!(sub.buoyancy_blocks, 0);
        sub.set_block([0, 1, 0], canonical(&reg, "createaeronautics:levitation_core"));
        assert_eq!(sub.buoyancy_blocks, 1);
    }

    #[test]
    fn live_kinetic_propeller_generates_real_thrust_and_forward_acceleration() {
        let (addon, reg) = aero_registry();
        let mut cells = BTreeMap::new();
        // Kinetic chain along +X: propeller_engine[axis=x] drives
        // create:shaft[axis=x] -> propeller_bearing[facing=east] (axis X).
        cells.insert(
            [0, 0, 0],
            canonical_with(&reg, "createaeronautics:propeller_engine", "axis", "x"),
        );
        cells.insert([1, 0, 0], canonical_with(&reg, "create:shaft", "axis", "x"));
        cells.insert(
            [2, 0, 0],
            canonical_with(&reg, "createaeronautics:propeller_bearing", "facing", "east"),
        );
        cells.insert([0, 1, 0], canonical(&reg, "createaeronautics:balloon_envelope"));

        let mut sub =
            RigidBodySubWorld::assemble(1, 0, cells, &reg, Some(&addon), 100.0).unwrap();
        assert_eq!(sub.propellers.len(), 2); // engine + bearing both thrust
        let rpm = sub
            .factory()
            .kinetic_solution(DimBlockPos::new(0, 2, 0, 0))
            .unwrap();
        assert_eq!(rpm.status, NetworkStatus::Running);
        assert!((rpm.speed - 64.0).abs() < 1e-9);

        sub.pos = [0.0, 100.0, 0.0];
        for _ in 0..100 {
            sub.step(0.05, 9.81, SEA_LEVEL_AIR_DENSITY);
        }
        assert_finite(&sub);
        // 2 mounts * 64 RPM * 150 N/RPM = 19200 N over 400 kg -> +X flight.
        assert!(
            sub.vel[0] > 0.0,
            "expected +X thrust velocity, got {:?}",
            sub.vel
        );
        assert!(sub.pos[0] > 0.0);
        // Buoyancy (1 * 1200 N) < weight (400 * 9.81 N) -> sinks but bounded.
        assert!(sub.vel[1] < 0.0 && sub.vel[1].abs() < 20.0);
    }

    #[test]
    fn buoyancy_equilibrium_holds_without_divergence_over_2000_ticks() {
        let (addon, reg) = aero_registry();
        let mut cells = BTreeMap::new();
        let envelope = canonical(&reg, "createaeronautics:balloon_envelope");
        let deck = canonical(&reg, "createaeronautics:airship_assembler");
        cells.insert([0, 0, 0], deck.clone());
        cells.insert([1, 0, 0], deck);
        cells.insert([0, 1, 0], envelope.clone());
        cells.insert([1, 1, 0], envelope);
        let block_mass = 100.0;
        let mass = 4.0 * block_mass;
        // Exact equilibrium: 2 envelopes lift exactly `mass * g`.
        let mut addon = addon;
        addon.propulsion = Some(PropulsionSpec {
            lift_per_buoyancy_block: mass * 9.81 / 2.0,
            ..PropulsionSpec::aeronautics_default()
        });
        let mut sub =
            RigidBodySubWorld::assemble(1, 0, cells, &reg, Some(&addon), block_mass).unwrap();
        sub.pos = [0.0, 100.0, 0.0];
        for _ in 0..2_000 {
            sub.step(0.05, 9.81, SEA_LEVEL_AIR_DENSITY);
        }
        assert_finite(&sub);
        assert!(
            sub.vel[1].abs() < 1e-6,
            "equilibrium drift: vel={:?} pos={:?}",
            sub.vel,
            sub.pos
        );
        assert!((sub.pos[1] - 100.0).abs() < 0.1);
        assert_eq!(sub.quat, [0.0, 0.0, 0.0, 1.0]);
    }

    #[test]
    fn rotation_transforms_roundtrip_and_passenger_delta_matches_finite_difference() {
        let (addon, reg) = aero_registry();
        let mut cells = BTreeMap::new();
        cells.insert([0, 0, 0], canonical(&reg, "createaeronautics:airship_assembler"));
        cells.insert([2, 0, 0], canonical(&reg, "createaeronautics:rudder"));
        let mut sub =
            RigidBodySubWorld::assemble(1, 3, cells, &reg, Some(&addon), 100.0).unwrap();
        // Non-trivial orientation and motion.
        sub.quat = quat_normalize([0.1, 0.3, -0.2, 0.9]);
        sub.pos = [10.0, 80.0, -4.0];
        sub.vel = [1.0, 2.0, 3.0];
        sub.omega = [0.0, 0.4, 0.0];

        // Exact local<->world round-trip via conjugate.
        for p in [[0.0, 0.0, 0.0], [1.25, -3.5, 0.75], [-8.0, 2.0, 9.9]] {
            let back = sub.world_to_local(sub.local_to_world(p));
            for i in 0..3 {
                assert!((back[i] - p[i]).abs() < 1e-12, "round-trip drift: {back:?}");
            }
        }

        // Formula pinned: (v + ω × (R·offset)) * dt.
        let offset = [0.5, 1.0, -2.0];
        let r = quat_rotate(sub.quat, offset);
        let expect = [
            (sub.vel[0] + (sub.omega[1] * r[2] - sub.omega[2] * r[1])) * 0.05,
            (sub.vel[1] + (sub.omega[2] * r[0] - sub.omega[0] * r[2])) * 0.05,
            (sub.vel[2] + (sub.omega[0] * r[1] - sub.omega[1] * r[0])) * 0.05,
        ];
        let got = sub.passenger_world_delta(offset, 0.05);
        for i in 0..3 {
            assert_eq!(got[i], expect[i]);
        }

        // Finite-difference consistency: with gravity/drag disabled and a
        // ship carrying no propellers, ω and v are constant over the step, so
        // the first-order delta matches the actual pose change to O(|ω|²·dt²).
        let local_pos = [
            sub.center_of_mass[0] + offset[0],
            sub.center_of_mass[1] + offset[1],
            sub.center_of_mass[2] + offset[2],
        ];
        let before = sub.local_to_world(local_pos);
        let delta = sub.passenger_world_delta(offset, 0.05);
        sub.step(0.05, 0.0, 0.0); // no gravity, no aero forces
        let after = sub.local_to_world(local_pos);
        for i in 0..3 {
            let fd = after[i] - before[i];
            assert!(
                (fd - delta[i]).abs() < 2e-3,
                "axis {i}: finite-diff {fd} vs delta {}",
                delta[i]
            );
        }
    }

    #[test]
    fn aero_v1_snapshot_roundtrips_byte_for_byte_and_keeps_ticking() {
        let (addon, reg) = aero_registry();
        let mut cells = BTreeMap::new();
        cells.insert(
            [0, 0, 0],
            canonical_with(&reg, "createaeronautics:propeller_engine", "axis", "x"),
        );
        cells.insert(
            [1, 0, 0],
            canonical_with(&reg, "createaeronautics:propeller_bearing", "facing", "east"),
        );
        cells.insert([0, 1, 0], canonical(&reg, "createaeronautics:balloon_envelope"));
        cells.insert([0, 0, 1], canonical(&reg, "createaeronautics:rudder"));

        let mut sub =
            RigidBodySubWorld::assemble(42, 3, cells, &reg, Some(&addon), 100.0).unwrap();
        sub.pos = [5.0, 90.0, -7.5];
        for _ in 0..10 {
            sub.step(0.05, 9.81, SEA_LEVEL_AIR_DENSITY);
        }

        let snap = encode_aeronautics_snapshot(&sub);
        assert_eq!(&snap[..5], b"AERO\x01");
        let mut restored = decode_aeronautics_snapshot(&snap, &reg).unwrap();
        assert_eq!(restored.id, 42);
        assert_eq!(restored.parent_dim, 3);
        assert_eq!(restored.pos, sub.pos);
        assert_eq!(restored.quat, sub.quat);
        assert_eq!(restored.vel, sub.vel);
        assert_eq!(restored.blocks, sub.blocks);
        assert_eq!(restored.propellers, sub.propellers);
        assert_eq!(restored.rudders, sub.rudders);
        // Re-encode is byte-identical.
        assert_eq!(encode_aeronautics_snapshot(&restored), snap);
        // The restored kinetic network still solves and produces thrust.
        for _ in 0..5 {
            restored.step(0.05, 9.81, SEA_LEVEL_AIR_DENSITY);
        }
        assert_finite(&restored);
        assert!(restored.vel[0] > 0.0);

        // Truncation and trailing garbage fail closed.
        assert!(decode_aeronautics_snapshot(&snap[..snap.len() - 3], &reg).is_err());
        let mut padded = snap.clone();
        padded.push(0);
        assert!(decode_aeronautics_snapshot(&padded, &reg).is_err());
    }

    #[test]
    fn crc32_ieee_known_vector_and_data_version_table() {
        // The canonical CRC32 check vector (`zlib` "123456789" -> 0xCBF43926).
        assert_eq!(crc32_ieee(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32_ieee(b""), 0);
        // The stamped constants equal the shared version table's rows exactly
        // and the known-version gate covers both canonical targets.
        assert_eq!(
            data_version_for(McVersion::V1_21_1),
            Some(AERO_DATA_VERSION_1_21_1)
        );
        assert_eq!(
            data_version_for(McVersion::V26_3),
            Some(AERO_DATA_VERSION_26_3)
        );
        assert!(is_known_aero_data_version(3955));
        assert!(is_known_aero_data_version(5023));
        for dv in [i32::MIN, -1, 0, 1, 3956, 5024, 9999, i32::MAX] {
            assert!(!is_known_aero_data_version(dv), "data_version {dv}");
        }
    }

    #[test]
    fn aero2_envelope_roundtrips_data_version_and_legacy_defaults_to_3955() {
        let (addon, reg) = aero_registry();
        let mut cells = BTreeMap::new();
        cells.insert(
            [0, 0, 0],
            canonical_with(&reg, "createaeronautics:propeller_engine", "axis", "x"),
        );
        cells.insert(
            [1, 0, 0],
            canonical_with(&reg, "createaeronautics:propeller_bearing", "facing", "east"),
        );
        cells.insert([0, 1, 0], canonical(&reg, "createaeronautics:balloon_envelope"));

        let mut sub =
            RigidBodySubWorld::assemble(7, 3, cells, &reg, Some(&addon), 100.0).unwrap();
        sub.pos = [2.0, 64.0, -1.0];
        for _ in 0..10 {
            sub.step(0.05, 9.81, SEA_LEVEL_AIR_DENSITY);
        }
        let inner = encode_aeronautics_snapshot(&sub);

        for dv in [AERO_DATA_VERSION_1_21_1, AERO_DATA_VERSION_26_3] {
            let env = sub.encode_snapshot_for_data_version(dv);
            assert_eq!(&env[..5], b"AERO\x02");
            // The wrapped payload is the untouched `aero_v1` byte stream.
            assert_eq!(&env[17..], inner.as_slice());
            let (restored, got) = decode_snapshot_with_data_version(&env, &reg).unwrap();
            assert_eq!(got, dv);
            assert_eq!(encode_aeronautics_snapshot(&restored), inner);
        }

        // Legacy `AERO` v1 inputs decode through the same entry point and
        // report the 1.21.1 data version (3955).
        let (restored, got) = decode_snapshot_with_data_version(&inner, &reg).unwrap();
        assert_eq!(got, AERO_DATA_VERSION_1_21_1);
        assert_eq!(got, 3955);
        assert_eq!(encode_aeronautics_snapshot(&restored), inner);
    }

    #[test]
    fn aero2_envelope_fails_closed_on_corruption() {
        let (addon, reg) = aero_registry();
        let mut cells = BTreeMap::new();
        cells.insert([0, 0, 0], canonical(&reg, "createaeronautics:airship_assembler"));
        cells.insert([0, 1, 0], canonical(&reg, "createaeronautics:balloon_envelope"));
        let sub = RigidBodySubWorld::assemble(1, 0, cells, &reg, Some(&addon), 100.0).unwrap();
        let env = sub.encode_snapshot_for_data_version(AERO_DATA_VERSION_26_3);

        // Truncated header, truncated payload, trailing bytes.
        assert!(decode_snapshot_with_data_version(&env[..10], &reg).is_err());
        assert!(decode_snapshot_with_data_version(&env[..16], &reg).is_err());
        assert!(decode_snapshot_with_data_version(&env[..env.len() - 2], &reg).is_err());
        let mut padded = env.clone();
        padded.push(0);
        assert!(decode_snapshot_with_data_version(&padded, &reg).is_err());

        // Flipped payload byte: length still matches, CRC catches it.
        let mut bad = env.clone();
        let n = bad.len();
        bad[n - 1] ^= 0xFF;
        assert!(decode_snapshot_with_data_version(&bad, &reg).is_err());

        // Corrupted CRC field.
        let mut bad = env.clone();
        bad[13] ^= 0xFF;
        assert!(decode_snapshot_with_data_version(&bad, &reg).is_err());

        // Unknown / negative / zero data_version tags fail closed.
        for dv in [0i32, -1, 9999, i32::MAX] {
            let mut bad = env.clone();
            bad[5..9].copy_from_slice(&dv.to_be_bytes());
            assert!(
                decode_snapshot_with_data_version(&bad, &reg).is_err(),
                "data_version {dv} must fail closed"
            );
        }

        // Bad magic and unknown envelope version byte.
        let mut bad = env.clone();
        bad[0] = b'X';
        assert!(decode_snapshot_with_data_version(&bad, &reg).is_err());
        let mut bad = env.clone();
        bad[4] = 0x03;
        assert!(decode_snapshot_with_data_version(&bad, &reg).is_err());

        // A well-formed envelope (valid CRC/len/version tag) wrapping a
        // non-`aero_v1` payload still fails inside the inner decoder.
        let junk = b"not a snapshot";
        let mut bad = Vec::new();
        bad.extend_from_slice(&AERO_MAGIC);
        bad.push(AERO2_VERSION);
        bad.extend_from_slice(&AERO_DATA_VERSION_26_3.to_be_bytes());
        bad.extend_from_slice(&(junk.len() as u32).to_be_bytes());
        bad.extend_from_slice(&crc32_ieee(junk).to_be_bytes());
        bad.extend_from_slice(junk);
        assert!(decode_snapshot_with_data_version(&bad, &reg).is_err());
    }

    #[test]
    fn all_builtin_addons_compose_into_one_registry() {
        let addons = builtin_create_addon_profiles();
        let reg = create_registry_with_addons(DEFAULT_BASE_STATE_ID, &addons).unwrap();
        let mut factory = CreateFactory::new(reg);
        // An addon motor drives an addon propeller natively on the flagship tier.
        let motor = factory
            .registry()
            .with_property(
                factory
                    .registry()
                    .default_state("createaddition:electric_motor")
                    .unwrap(),
                "axis",
                "x",
            )
            .unwrap();
        let shaft = factory
            .registry()
            .with_property(
                factory.registry().default_state("create:shaft").unwrap(),
                "axis",
                "x",
            )
            .unwrap();
        factory.place_block(DimBlockPos::new(0, 0, 64, 0), motor);
        factory.place_block(DimBlockPos::new(0, 1, 64, 0), shaft);
        let sol = factory
            .kinetic_solution(DimBlockPos::new(0, 1, 64, 0))
            .unwrap();
        assert_eq!(sol.status, NetworkStatus::Running);
        assert!((sol.speed - 96.0).abs() < 1e-9);
    }
}
