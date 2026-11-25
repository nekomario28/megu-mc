// SPDX-License-Identifier: LGPL-3.0-or-later

//! Create: Aeronautics-style 6-DOF rigid-body sub-worlds: sparse local voxel
//! grids with an embedded live [`CreateFactory`] (propellers, shafts, belts
//! and machines keep ticking in flight), a deterministic fixed-step 20 TPS
//! integrator (thrust, rudder torque, altitude-tapered buoyancy, quadratic
//! drag, Euler angular dynamics), exact coordinate transforms and passenger
//! carry deltas for `megu`'s `ac_note_carried_motion` anti-cheat hook,
//! and a deterministic binary `AERO` v1 Save codec.

use std::collections::{BTreeMap, BTreeSet};

use megu_core::world_view::{SubLevelTransform, WorldView};
use mod_adapter_core::neoforge_wire::{read_mc_string, read_varint, write_mc_string, write_varint};
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
/// Per-substep solver default: 4 substeps per 20 TPS tick = 80 Hz
/// internal integration, matching the update rate real rigid-body mods
/// target so resting contacts never tunnel.
pub const DEFAULT_PHYSICS_SUBSTEPS: u32 = 4;
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
/// S2C channel carrying versioned 6-DOF sub-world snapshots and pose deltas.
pub const SUBWORLD_SYNC_CHANNEL: &str = "megu:subworld_sync_v1";
/// C2S channel carrying versioned 6-DOF helm input.
pub const SUBWORLD_HELM_CHANNEL: &str = "megu:subworld_helm_v1";
/// Maximum voxel records in one complete sub-world spawn frame.
pub const MAX_SUBWORLD_VOXELS: usize = 65_536;
const SUBWORLD_SYNC_MAGIC: [u8; 4] = *b"SW6D";
const SUBWORLD_SYNC_VERSION: u8 = 1;

/// Kind of update carried by [`SubWorldSyncFrame`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubWorldSyncKind {
    FullSpawn,
    PoseDelta,
    Despawn,
}

/// Version 1 server-to-client sub-world state. Quaternion order is `[x,y,z,w]`.
#[derive(Clone, Debug, PartialEq)]
pub struct SubWorldSyncFrame {
    pub kind: SubWorldSyncKind,
    pub subworld_id: u64,
    pub dim_id: String,
    pub pos: [f64; 3],
    pub quat: [f32; 4],
    pub vel: [f32; 3],
    pub ang_vel: [f32; 3],
    pub revision: u32,
    pub voxels: Vec<([i16; 3], u32)>,
}

impl SubWorldSyncFrame {
    /// Encode the bounded `SW6D` v1 frame, rejecting invalid poses and oversized data.
    pub fn encode(&self) -> Result<Vec<u8>, String> {
        if self.dim_id.is_empty() || self.dim_id.len() > u16::MAX as usize {
            return Err("invalid subworld dimension id length".into());
        }
        if self.pos.iter().any(|v| !v.is_finite())
            || self.vel.iter().chain(&self.ang_vel).any(|v| !v.is_finite())
        {
            return Err("non-finite subworld pose".into());
        }
        let qnorm = self
            .quat
            .iter()
            .map(|v| f64::from(*v) * f64::from(*v))
            .sum::<f64>();
        if self.quat.iter().any(|v| !v.is_finite()) || (qnorm - 1.0).abs() > 1.0e-3 {
            return Err("subworld quaternion must be a normalized unit quaternion".into());
        }
        if (self.kind == SubWorldSyncKind::FullSpawn && self.voxels.len() > MAX_SUBWORLD_VOXELS)
            || (self.kind != SubWorldSyncKind::FullSpawn && !self.voxels.is_empty())
        {
            return Err("invalid subworld voxel list".into());
        }
        let mut out = Vec::with_capacity(100 + self.dim_id.len() + self.voxels.len() * 10);
        out.extend_from_slice(&SUBWORLD_SYNC_MAGIC);
        out.push(SUBWORLD_SYNC_VERSION);
        out.push(match self.kind {
            SubWorldSyncKind::FullSpawn => 0,
            SubWorldSyncKind::PoseDelta => 1,
            SubWorldSyncKind::Despawn => 2,
        });
        out.extend_from_slice(&self.subworld_id.to_be_bytes());
        out.extend_from_slice(&(self.dim_id.len() as u16).to_be_bytes());
        out.extend_from_slice(self.dim_id.as_bytes());
        for v in self.pos {
            out.extend_from_slice(&v.to_be_bytes());
        }
        for v in self.quat {
            out.extend_from_slice(&v.to_be_bytes());
        }
        for v in self.vel {
            out.extend_from_slice(&v.to_be_bytes());
        }
        for v in self.ang_vel {
            out.extend_from_slice(&v.to_be_bytes());
        }
        out.extend_from_slice(&self.revision.to_be_bytes());
        if self.kind == SubWorldSyncKind::FullSpawn {
            out.extend_from_slice(&(self.voxels.len() as u32).to_be_bytes());
            for (offset, state) in &self.voxels {
                for v in offset {
                    out.extend_from_slice(&v.to_be_bytes());
                }
                out.extend_from_slice(&state.to_be_bytes());
            }
        }
        Ok(out)
    }

    /// Decode one complete v1 frame. Trailing and truncated bytes are rejected.
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        const BASE_LEN: usize = 4 + 1 + 1 + 8 + 2 + 3 * 8 + 4 * 4 + 3 * 4 + 3 * 4 + 4;
        if bytes.len() < BASE_LEN {
            return Err("truncated subworld sync frame".into());
        }
        let mut c = 0usize;
        let take = |c: &mut usize, n: usize| -> Result<&[u8], String> {
            let end = c
                .checked_add(n)
                .ok_or_else(|| "subworld frame length overflow".to_string())?;
            let value = bytes
                .get(*c..end)
                .ok_or_else(|| "truncated subworld sync frame".to_string())?;
            *c = end;
            Ok(value)
        };
        if take(&mut c, 4)? != SUBWORLD_SYNC_MAGIC {
            return Err("bad subworld sync magic".into());
        }
        if take(&mut c, 1)?[0] != SUBWORLD_SYNC_VERSION {
            return Err("unsupported subworld sync version".into());
        }
        let kind = match take(&mut c, 1)?[0] {
            0 => SubWorldSyncKind::FullSpawn,
            1 => SubWorldSyncKind::PoseDelta,
            2 => SubWorldSyncKind::Despawn,
            _ => return Err("unknown subworld sync kind".into()),
        };
        let subworld_id = u64::from_be_bytes(
            take(&mut c, 8)?
                .try_into()
                .map_err(|_| "truncated subworld id")?,
        );
        let dim_len = usize::from(u16::from_be_bytes(
            take(&mut c, 2)?
                .try_into()
                .map_err(|_| "truncated dimension length")?,
        ));
        if dim_len == 0 {
            return Err("empty subworld dimension id".into());
        }
        let dim_id = std::str::from_utf8(take(&mut c, dim_len)?)
            .map_err(|_| "invalid subworld dimension UTF-8")?
            .to_owned();
        let read_f64 = |c: &mut usize| -> Result<f64, String> {
            Ok(f64::from_be_bytes(
                take(c, 8)?.try_into().map_err(|_| "truncated f64")?,
            ))
        };
        let read_f32 = |c: &mut usize| -> Result<f32, String> {
            Ok(f32::from_be_bytes(
                take(c, 4)?.try_into().map_err(|_| "truncated f32")?,
            ))
        };
        let mut pos = [0.0; 3];
        for v in &mut pos {
            *v = read_f64(&mut c)?;
        }
        let mut quat = [0.0; 4];
        for v in &mut quat {
            *v = read_f32(&mut c)?;
        }
        let mut vel = [0.0; 3];
        for v in &mut vel {
            *v = read_f32(&mut c)?;
        }
        let mut ang_vel = [0.0; 3];
        for v in &mut ang_vel {
            *v = read_f32(&mut c)?;
        }
        let revision = u32::from_be_bytes(
            take(&mut c, 4)?
                .try_into()
                .map_err(|_| "truncated revision")?,
        );
        let mut voxels = Vec::new();
        if kind == SubWorldSyncKind::FullSpawn {
            let count = u32::from_be_bytes(
                take(&mut c, 4)?
                    .try_into()
                    .map_err(|_| "truncated voxel count")?,
            ) as usize;
            if count > MAX_SUBWORLD_VOXELS
                || count
                    .checked_mul(10)
                    .is_none_or(|n| n > bytes.len().saturating_sub(c))
            {
                return Err("invalid or oversized subworld voxel count".into());
            }
            voxels.reserve(count);
            for _ in 0..count {
                let mut offset = [0i16; 3];
                for v in &mut offset {
                    *v = i16::from_be_bytes(
                        take(&mut c, 2)?
                            .try_into()
                            .map_err(|_| "truncated voxel offset")?,
                    );
                }
                let state = u32::from_be_bytes(
                    take(&mut c, 4)?
                        .try_into()
                        .map_err(|_| "truncated voxel state")?,
                );
                voxels.push((offset, state));
            }
        }
        if c != bytes.len() {
            return Err("trailing subworld sync bytes".into());
        }
        let frame = Self {
            kind,
            subworld_id,
            dim_id,
            pos,
            quat,
            vel,
            ang_vel,
            revision,
            voxels,
        };
        // Reuse the encoder's finite and quaternion validation.
        frame.encode()?;
        Ok(frame)
    }
}

/// Client-to-server bounded helm controls. Every scalar is finite and clamped to `[-1, 1]`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SubWorldHelmInput {
    pub subworld_id: u64,
    pub throttle: f32,
    pub pitch: f32,
    pub yaw: f32,
    pub roll: f32,
    pub buoyancy_delta: f32,
    pub disassemble: bool,
}

impl SubWorldHelmInput {
    pub fn encode(&self) -> Result<Vec<u8>, String> {
        let values = [
            self.throttle,
            self.pitch,
            self.yaw,
            self.roll,
            self.buoyancy_delta,
        ];
        if values
            .iter()
            .any(|v| !v.is_finite() || !(-1.0..=1.0).contains(v))
        {
            return Err("helm controls must be finite and within -1..=1".into());
        }
        let mut out = Vec::with_capacity(29);
        out.extend_from_slice(&self.subworld_id.to_be_bytes());
        for v in values {
            out.extend_from_slice(&v.to_be_bytes());
        }
        out.push(u8::from(self.disassemble));
        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() != 29 {
            return Err("invalid helm frame length".into());
        }
        let subworld_id =
            u64::from_be_bytes(bytes[..8].try_into().map_err(|_| "truncated helm id")?);
        let mut values = [0.0f32; 5];
        for (i, value) in values.iter_mut().enumerate() {
            let start = 8 + i * 4;
            *value = f32::from_be_bytes(
                bytes[start..start + 4]
                    .try_into()
                    .map_err(|_| "truncated helm value")?,
            );
        }
        if bytes[28] > 1 {
            return Err("invalid helm disassemble flag".into());
        }
        let frame = Self {
            subworld_id,
            throttle: values[0],
            pitch: values[1],
            yaw: values[2],
            roll: values[3],
            buoyancy_delta: values[4],
            disassemble: bytes[28] != 0,
        };
        frame.encode()?;
        Ok(frame)
    }
}

/// Per-dimension environment physics for [`RigidBodySubWorld::step_with_atmosphere`].
///
/// Multiplies a caller-supplied base gravitational acceleration by
/// `gravity_scale` (Overworld `1.0`, microgravity `0.0`, Moon `0.165`, Mars
/// `0.38`), scales every aerodynamic term through `air_density` (thrust is
/// never scaled — propellers and rocket engines accelerate in vacuum), and
/// gates the buoyancy lift term on `enable_buoyancy`. `vacuum_damping` is an
/// optional artificial-drag station-keeping term (`F = -k * v`) that applies
/// even at zero air density so a drifting hull in space can settle cleanly;
/// `0.0` disables it bit-identically.
///
/// [`AtmospherePhysics::OVERWORLD`] reproduces the legacy `step(dt, g, 1.0)`
/// force balance exactly: `1.0 * lift` and a skipped zero damping term keep
/// the arithmetic unchanged.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AtmospherePhysics {
    /// Multiplier on the base gravitational acceleration passed to
    /// [`RigidBodySubWorld::step_with_atmosphere`].
    pub gravity_scale: f64,
    /// Air density scaling buoyancy lift, aerodynamic drag, rudder authority,
    /// and angular damping (`1.0` = sea level, `0.0` = vacuum).
    pub air_density: f64,
    /// Whether envelope/levitation blocks produce lift in this dimension.
    pub enable_buoyancy: bool,
    /// Optional artificial linear drag (`N per m/s`) applied in any
    /// atmosphere — the stabilizer that bleeds off drift in vacuum.
    /// `0.0` disables the term entirely.
    pub vacuum_damping: f64,
}

impl AtmospherePhysics {
    /// The default profile every sub-world carries until a router installs a
    /// per-dimension environment: identical to the original `step` physics.
    pub const OVERWORLD: Self = Self {
        gravity_scale: 1.0,
        air_density: SEA_LEVEL_AIR_DENSITY,
        enable_buoyancy: true,
        vacuum_damping: 0.0,
    };

    /// Vacuum/microgravity profile: no gravity, no atmosphere, no buoyancy;
    /// thrusters still accelerate and only `vacuum_damping` can bleed speed.
    pub const MICROGRAVITY: Self = Self {
        gravity_scale: 0.0,
        air_density: 0.0,
        enable_buoyancy: false,
        vacuum_damping: 0.0,
    };

    /// A low-gravity planetary body: proportional gravity, caller-declared
    /// (possibly zero) air density, buoyancy on only when there is an
    /// atmosphere to float in.
    pub const fn low_gravity(gravity_scale: f64, air_density: f64) -> Self {
        Self {
            gravity_scale,
            air_density,
            enable_buoyancy: air_density > 0.0,
            vacuum_damping: 0.0,
        }
    }

    /// Lunar-style profile (`gravity_scale = 0.165`, vacuum).
    pub const MOON: Self = Self::low_gravity(0.165, 0.0);
    /// Mars-style profile (`gravity_scale = 0.38`, thin air for drag).
    pub const MARS: Self = Self::low_gravity(0.38, 0.06);

    /// Derive physics from a shared
    /// [`mod_adapter_core::dimension_mod::DimensionEnvProfile`] gravity
    /// multiplier plus the caller's air-density model for that dimension.
    /// Environments declaring zero or negative gravity get no buoyancy.
    pub fn from_env_profile(
        env: &mod_adapter_core::dimension_mod::DimensionEnvProfile,
        air_density: f64,
    ) -> Self {
        let air_density = if air_density.is_finite() && air_density >= 0.0 {
            air_density
        } else {
            0.0
        };
        Self {
            gravity_scale: env.gravity_multiplier,
            air_density,
            enable_buoyancy: env.gravity_multiplier > 0.0 && air_density > 0.0,
            vacuum_damping: 0.0,
        }
    }
}

impl Default for AtmospherePhysics {
    fn default() -> Self {
        Self::OVERWORLD
    }
}

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

/// Per-material mass table: `(path_substring, mass_kg)` pairs checked in
/// order against the block's resource path (first match wins), scaled so
/// the heaviest blocks a ship can plausibly carry dominate the lighter
/// hull/insulation materials by realistic density ratios. This is a
/// gameplay-scaled table (a vanilla-scale "kg per block"), not a
/// geological survey — what matters is that an obsidian keel outweighs a
/// wool balloon by roughly the same ratio real materials do.
const MATERIAL_MASS_TABLE: &[(&str, f64)] = &[
    // Composite Create/Aeronautics machinery before raw material names: an
    // `andesite_propeller` is mostly wooden blades and a small casing, not a
    // solid andesite cube — calibrated so a one-envelope test ship retains
    // positive net lift under helm control. An airship
    // assembler is dense machinery: metal gearing in a stone casing.
    ("propeller", 100.0),
    ("airship_assembler", 192.0),
    // Ultra-dense exotic hull materials.
    ("netherite", 4000.0),
    ("ancient_debris", 4000.0),
    ("bedrock", 2000.0),
    ("obsidian", 1200.0),
    // Metals: structural mass a real hull would carry.
    ("raw_gold", 1500.0),
    ("gold", 1900.0),
    ("raw_copper", 650.0),
    ("copper", 900.0),
    ("anvil", 800.0),
    ("raw_iron", 700.0),
    ("iron", 800.0),
    ("chain", 400.0),
    ("lantern", 100.0),
    // Dense rock.
    ("deepslate", 300.0),
    ("blackstone", 280.0),
    ("basalt", 280.0),
    ("end_stone", 260.0),
    ("netherrack", 180.0),
    ("stone", 200.0),
    ("andesite", 200.0),
    ("granite", 200.0),
    ("diorite", 200.0),
    ("tuff", 190.0),
    ("calcite", 190.0),
    ("brick", 220.0),
    ("concrete", 240.0),
    ("terracotta", 180.0),
    ("magma", 260.0),
    // Loose sediments.
    ("sandstone", 200.0),
    ("sand", 160.0),
    ("gravel", 160.0),
    ("dirt", 120.0),
    ("grass_block", 120.0),
    ("mud", 130.0),
    ("clay", 140.0),
    ("soul_soil", 110.0),
    ("ice", 90.0),
    ("snow", 60.0),
    // Manufactured light materials.
    ("glass", 25.0),
    ("wool", 15.0),
    ("carpet", 15.0),
    // Wood.
    ("log", 70.0),
    ("stem", 70.0),
    ("hyphae", 70.0),
    ("wood", 65.0),
    ("planks", 60.0),
    ("slab", 40.0),
    ("stairs", 60.0),
    ("fence", 30.0),
    ("door", 40.0),
    ("trapdoor", 20.0),
    ("ladder", 10.0),
    // Aeronautics-grade ultralights: the whole point of a balloon hull.
    ("sail", 10.0),
    ("canvas", 10.0),
    ("envelope", 8.0),
    ("balloon", 8.0),
    ("blimp", 8.0),
    // Flora and utility blocks.
    ("leaves", 5.0),
    ("torch", 2.0),
    ("flower", 2.0),
    ("sapling", 2.0),
    ("cactus", 8.0),
    ("lava", 280.0),
    ("water", 100.0),
];

/// Per-block mass (kg) for `block_id` (`"namespace:path[props]"` or bare
/// `"namespace:path"`): the first [`MATERIAL_MASS_TABLE`] substring match
/// on the resource path, `default_mass` when nothing matches. The table
/// keys on the *path* so `"minecraft:smooth_stone"` and
/// `"minecraft:stone_stairs"` both hit the stone entry.
pub fn material_mass_kg(block_id: &str, default_mass: f64) -> f64 {
    let path = id_path(canonical_block_id(block_id));
    for (needle, mass) in MATERIAL_MASS_TABLE {
        if path.contains(needle) {
            return *mass;
        }
    }
    default_mass
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
    /// Per-dimension environment physics consumed by
    /// [`Self::step_with_atmosphere`]; [`AtmospherePhysics::OVERWORLD`] until a
    /// dimension router installs the destination's profile. Not part of the
    /// `AERO` snapshot — `parent_dim` is, so reloads re-derive the profile
    /// from the dimension they restore into.
    atmosphere: AtmospherePhysics,
    tick: u64,
    /// `block_id -> min_rpm` gate for propeller mounts (from the addon's
    /// kinetic consumer table; restored from mount data on decode).
    min_rpms: BTreeMap<String, f64>,
    pub mass: f64,
    pub center_of_mass: [f64; 3],
    /// Body-frame inertia diagonal `[Ixx, Iyy, Izz]` — kept populated for
    /// backwards compatibility; the authoritative rotational state is the
    /// full [`Self::inertia_tensor`].
    pub inertia_diag: [f64; 3],
    /// Full symmetric 3×3 body-frame inertia tensor (row-major,
    /// off-diagonal products of inertia included). Always
    /// positive-definite — the unit-cube self-inertia term keeps even a
    /// one-block or collinear ship invertible.
    pub inertia_tensor: [[f64; 3]; 3],
    /// Exact inverse of [`Self::inertia_tensor`] (row-major), maintained
    /// alongside so the angular integrator never inverts on the hot path.
    pub inv_inertia_tensor: [[f64; 3]; 3],
    /// Per-block-id mass overrides (kg) consulted before
    /// [`material_mass_kg`]'s table — a ship builder can declare heavier
    /// plating or lighter composites without forking the table.
    pub mass_overrides: BTreeMap<String, f64>,
    /// Physics substeps per [`Self::step_env_with_contacts`] call
    /// (`4` = 80 Hz internal integration per 20 TPS tick).
    pub substeps: u32,
    /// Dormancy flag: `false` marks a stably-resting ship that the contact
    /// solver leaves completely untouched. Any helm input, block edit or
    /// external impulse wakes it.
    pub awake: bool,
    /// Solver bookkeeping: consecutive supported low-energy substeps seen
    /// by the contact backend. Sleeping takes hysteresis — a parked hull
    /// limit-cycles at tiny velocities as gravity refeeds `v` each substep,
    /// so the backend parks only after [`BuiltinContactSolver::min_sleep_substeps`]
    /// straight quiet substeps. Reset by [`Self::wake`]. Not serialized:
    /// a restored ship simply re-times.
    pub rest_substeps: u32,
    pub buoyancy_blocks: u32,
    pub propellers: Vec<PropellerMount>,
    pub rudders: Vec<RudderMount>,
    /// Player helm input `[throttle, pitch, yaw, roll, buoyancy_delta]`.
    helm: [f64; 5],
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
            atmosphere: AtmospherePhysics::OVERWORLD,
            tick: 0,
            min_rpms,
            mass: 0.0,
            center_of_mass: [0.0; 3],
            inertia_diag: [0.0; 3],
            inertia_tensor: [[0.0; 3]; 3],
            inv_inertia_tensor: [[0.0; 3]; 3],
            mass_overrides: BTreeMap::new(),
            substeps: DEFAULT_PHYSICS_SUBSTEPS,
            awake: true,
            rest_substeps: 0,
            buoyancy_blocks: 0,
            propellers: Vec::new(),
            rudders: Vec::new(),
            helm: [0.0; 5],
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

    /// Per-dimension environment physics this sub-world steps under when
    /// [`Self::step_with_atmosphere`] is used. Defaults to
    /// [`AtmospherePhysics::OVERWORLD`].
    pub fn atmosphere(&self) -> AtmospherePhysics {
        self.atmosphere
    }

    /// Install the environment profile of the dimension the sub-world
    /// currently flies in. [`crate::orbital::OrbitalTransitionRouter`] calls
    /// this on every dimension crossing and once per routed step.
    pub fn set_atmosphere(&mut self, atmosphere: AtmospherePhysics) {
        self.atmosphere = atmosphere;
    }

    /// Move the sub-world into another dimension atomically: `parent_dim` and
    /// `pos` switch to `new_dim`/`new_pos` while `vel`, `omega`, `quat`,
    /// mounts, blocks and mass are preserved untouched (horizontal momentum
    /// and orientation ride through the crossing), then the embedded
    /// [`CreateFactory`] is rebuilt so every kinetic node re-keys onto the
    /// new dimension and live propeller RPMs keep solving there.
    pub fn transfer_to_dimension(&mut self, new_dim: u32, new_pos: [f64; 3]) {
        self.parent_dim = new_dim;
        for (position, value) in self.pos.iter_mut().zip(new_pos) {
            *position = sanitize(value);
        }
        self.rebuild_factory();
    }

    /// Place/replace a local block (canonical blockstate string), updating the
    /// embedded kinetic graph and recomputing mass/CoM/inertia/mounts.
    /// Editing a flying ship shifts its mass-weighted center of mass;
    /// [`Self::pos`] is translated by the world-space COM delta so every
    /// *existing* block keeps its exact world position — a mid-flight
    /// block place never teleports the hull.
    pub fn set_block(&mut self, local: [i32; 3], canonical: impl Into<String>) {
        self.wake();
        self.blocks.insert(local, canonical.into());
        self.rebuild_factory();
        self.preserve_world_geometry_during_physics_update();
    }

    /// Remove a local block and recompute physical properties. Like
    /// [`Self::set_block`], the world-space COM shift is compensated in
    /// [`Self::pos`] so the remaining hull stays world-exact.
    pub fn remove_block(&mut self, local: [i32; 3]) -> bool {
        if self.blocks.remove(&local).is_none() {
            return false;
        }
        self.wake();
        self.rebuild_factory();
        self.preserve_world_geometry_during_physics_update();
        true
    }

    /// Re-run [`Self::recompute_physics`], then translate `pos` by the
    /// world-frame COM delta `R·Δcom`: `local_to_world` is
    /// `pos + R·(p − com)`, so `pos' = pos + R·Δcom` cancels the COM
    /// shift exactly and every pre-existing cell's world position is
    /// preserved bit-for-bit.
    fn preserve_world_geometry_during_physics_update(&mut self) {
        let old_com = self.center_of_mass;
        self.recompute_physics();
        let delta_local = [
            self.center_of_mass[0] - old_com[0],
            self.center_of_mass[1] - old_com[1],
            self.center_of_mass[2] - old_com[2],
        ];
        if delta_local.iter().all(|&d| d == 0.0) {
            return;
        }
        let delta_world = quat_rotate(self.quat, delta_local);
        for (p, &d) in self.pos.iter_mut().zip(&delta_world) {
            *p = sanitize(*p + d);
        }
    }

    /// Register a per-block-id mass override (kg) consulted before
    /// [`material_mass_kg`]'s table — e.g. declare heavier plating or a
    /// fictional ultralight alloy. Recomputes physics immediately; a
    /// non-positive or non-finite `mass_kg` is rejected (`false`).
    pub fn set_block_mass_override(&mut self, block_id: &str, mass_kg: f64) -> bool {
        if !(mass_kg.is_finite() && mass_kg > 0.0) {
            return false;
        }
        self.wake();
        self.mass_overrides
            .insert(canonical_block_id(block_id).to_string(), mass_kg);
        self.preserve_world_geometry_during_physics_update();
        true
    }

    /// Clear a mass override previously set by
    /// [`Self::set_block_mass_override`]; returns `true` when one existed.
    pub fn clear_block_mass_override(&mut self, block_id: &str) -> bool {
        if self
            .mass_overrides
            .remove(canonical_block_id(block_id))
            .is_none()
        {
            return false;
        }
        self.wake();
        self.preserve_world_geometry_during_physics_update();
        true
    }

    /// The mass (kg) of one block id on *this* ship: `mass_overrides`
    /// first, then [`material_mass_kg`] with `block_mass` as fallback.
    fn block_mass_kg(&self, canonical: &str) -> f64 {
        let id = canonical_block_id(canonical);
        self.mass_overrides
            .get(id)
            .copied()
            .unwrap_or_else(|| material_mass_kg(id, self.block_mass))
    }

    /// Dormancy: mark the ship awake so the next contact step actually
    /// runs. Called by every mutating entry point.
    pub fn wake(&mut self) {
        self.awake = true;
        self.rest_substeps = 0;
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

    /// Recompute mass, center of mass, the full symmetric 3×3 inertia
    /// tensor (and its exact inverse), buoyancy count, and
    /// propeller/rudder mounts from the current `blocks` map.
    ///
    /// Mass is mass-weighted: `M = Σ m_i` with `m_i` from
    /// [`Self::block_mass_kg`], `com = Σ m_i (p_i + 0.5) / M`, and the
    /// tensor carries real off-diagonal products of inertia
    /// (`I_xy = −Σ m_i d_x d_y`) plus each voxel's unit-cube
    /// self-inertia `m_i/6` on the diagonal — the term that keeps even a
    /// one-block or collinear ship's tensor strictly positive-definite
    /// and exactly invertible. `inertia_diag` stays populated as
    /// `[Ixx, Iyy, Izz]` for backwards compatibility. Called on assemble
    /// and on every `set_block`/`remove_block` edit.
    pub fn recompute_physics(&mut self) {
        // Mass-weighted total mass and center of mass.
        let mut mass = 0.0f64;
        let mut com = [0.0f64; 3];
        for (p, canonical) in &self.blocks {
            let m = self.block_mass_kg(canonical);
            mass += m;
            for i in 0..3 {
                com[i] += m * (f64::from(p[i]) + 0.5);
            }
        }
        self.mass = sanitize(mass).max(1e-9);
        for c in &mut com {
            *c = sanitize(*c / self.mass);
        }
        self.center_of_mass = com;
        self.refresh_inertia_tensor();

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
            if is_propulsion_block(block_id) || prop_set.contains(block_id) {
                self.propellers.push(PropellerMount {
                    local: *local,
                    axis: axis_from_canonical(canonical),
                    min_rpm: self.min_rpms.get(block_id).copied().unwrap_or(0.0),
                    thrust_per_rpm: self.propulsion.thrust_per_rpm,
                });
            } else if is_control_block(block_id)
                || rud_set.contains(block_id)
                || path.contains("rudder")
            {
                self.rudders.push(RudderMount {
                    local: *local,
                    torque_axis: Axis::Y,
                    sense_axis: Axis::X,
                    gain: self.propulsion.rudder_torque,
                });
            } else if is_lift_block(block_id)
                || buoy_set.contains(block_id)
                || is_buoyant_path(path)
            {
                self.buoyancy_blocks = self.buoyancy_blocks.saturating_add(1);
            }
        }
    }

    /// Recompute `inertia_tensor` / `inv_inertia_tensor` / `inertia_diag`
    /// from the current blocks around the *current* `center_of_mass`:
    /// parallel-axis term per voxel plus the unit-cube self-inertia `m/6`
    /// so the matrix stays positive-definite — and exactly invertible —
    /// even for a single block or a collinear rod. The snapshot decoder
    /// calls this to rehydrate the off-diagonal products of inertia that
    /// the legacy `AERO` v1 format (diagonal only) never serialized.
    fn refresh_inertia_tensor(&mut self) {
        let com = self.center_of_mass;
        let mut tensor = [[0.0f64; 3]; 3];
        for (p, canonical) in &self.blocks {
            let m = self.block_mass_kg(canonical);
            let d = [
                f64::from(p[0]) + 0.5 - com[0],
                f64::from(p[1]) + 0.5 - com[1],
                f64::from(p[2]) + 0.5 - com[2],
            ];
            let self_inertia = m / 6.0;
            tensor[0][0] += m * (d[1] * d[1] + d[2] * d[2]) + self_inertia;
            tensor[1][1] += m * (d[0] * d[0] + d[2] * d[2]) + self_inertia;
            tensor[2][2] += m * (d[0] * d[0] + d[1] * d[1]) + self_inertia;
            tensor[0][1] -= m * d[0] * d[1];
            tensor[0][2] -= m * d[0] * d[2];
            tensor[1][2] -= m * d[1] * d[2];
        }
        tensor[1][0] = tensor[0][1];
        tensor[2][0] = tensor[0][2];
        tensor[2][1] = tensor[1][2];
        for row in &mut tensor {
            for v in row.iter_mut() {
                *v = sanitize(*v);
            }
        }
        self.inertia_tensor = tensor;
        self.inv_inertia_tensor =
            mat3_invert(&tensor).unwrap_or_else(|| diag3([1.0, 1.0, 1.0]));
        self.inertia_diag = [
            tensor[0][0].max(1e-9),
            tensor[1][1].max(1e-9),
            tensor[2][2].max(1e-9),
        ];
    }

    /// Apply validated pilot controls to this sub-world.
    pub fn apply_helm_input(&mut self, input: &SubWorldHelmInput) -> Result<(), String> {
        input.encode()?;
        if input.subworld_id != self.id {
            return Err("helm input targets a different subworld".into());
        }
        self.helm = [
            input.throttle,
            input.pitch,
            input.yaw,
            input.roll,
            input.buoyancy_delta,
        ]
        .map(f64::from);
        if self.helm.iter().any(|h| h.abs() > 1e-12) {
            self.wake();
        }
        Ok(())
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
        self.step_env(dt, gravity, air_density, 1.0, 0.0);
    }

    /// Advance one step under [`Self::atmosphere`]'s per-dimension profile:
    /// effective gravity is `base_gravity * gravity_scale` (microgravity
    /// `0.0`, Moon `0.165` of `base_gravity`, Mars `0.38`), `air_density`
    /// scales every aerodynamic term, `enable_buoyancy` gates the lift term,
    /// and `vacuum_damping` optionally bleeds linear velocity even in space.
    /// [`AtmospherePhysics::OVERWORLD`] at `base_gravity = 9.81` is
    /// bit-identical to `step(dt, 9.81, 1.0)`.
    pub fn step_with_atmosphere(&mut self, dt: f64, base_gravity: f64) {
        let atmosphere = self.atmosphere;
        let gravity = if base_gravity.is_finite() {
            sanitize(base_gravity * atmosphere.gravity_scale)
        } else {
            0.0
        };
        self.step_env(
            dt,
            gravity,
            atmosphere.air_density,
            if atmosphere.enable_buoyancy { 1.0 } else { 0.0 },
            atmosphere.vacuum_damping,
        );
    }

    /// Shared integrator behind [`Self::step`] and
    /// [`Self::step_with_atmosphere`]. `buoyancy_scale` multiplies the lift
    /// term (`1.0` leaves the legacy force balance untouched);
    /// `vacuum_damping <= 0.0` skips the artificial-drag branch entirely so
    /// the default path is arithmetic-identical to the original `step`.
    fn step_env(
        &mut self,
        dt: f64,
        gravity: f64,
        air_density: f64,
        buoyancy_scale: f64,
        vacuum_damping: f64,
    ) {
        if !dt.is_finite() || dt <= 0.0 {
            return;
        }
        self.tick = self.tick.saturating_add(1);
        let _ = self
            .factory
            .step_tick(self.tick, INTERNAL_REGION_STEPS, None);
        self.integrate_forces(dt, gravity, air_density, buoyancy_scale, vacuum_damping);
    }

    /// One force/integration step over `dt` seconds *without* ticking the
    /// embedded factory — the unit [`Self::step_env`] and each substep of
    /// [`Self::step_env_with_contacts`] share. Angular dynamics run in the
    /// world frame against the full inertia tensor:
    /// `ω̇ = I_w⁻¹ (τ − ω × (I_w ω))` with `I_w = R I Rᵀ`.
    fn integrate_forces(
        &mut self,
        dt: f64,
        gravity: f64,
        air_density: f64,
        buoyancy_scale: f64,
        vacuum_damping: f64,
    ) {
        let gravity = if gravity.is_finite() { gravity } else { 0.0 };
        let air_density = if air_density.is_finite() && air_density >= 0.0 {
            air_density
        } else {
            0.0
        };
        let buoyancy_scale = if buoyancy_scale.is_finite() {
            buoyancy_scale
        } else {
            0.0
        };
        let vacuum_damping = if vacuum_damping.is_finite() && vacuum_damping > 0.0 {
            vacuum_damping
        } else {
            0.0
        };

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
        // Helm throttle is a direct bounded actuator. It remains usable for
        // blocks whose upstream kinetic node is absent from the local registry.
        let direct_thrust = self.helm[0] * self.propulsion.thrust_per_rpm * 64.0;
        if !self.propellers.is_empty() {
            thrust_local[0] += direct_thrust;
        }
        let thrust_world = quat_rotate(self.quat, thrust_local);

        // Rudder weathercock torque + angular damping (local frame). Control
        // authority scales with dynamic pressure (~ρ·v).
        let mut torque_local = [0.0f64; 3];
        for rudder in &self.rudders {
            torque_local[rudder.torque_axis.index()] -=
                rudder.gain * air_density * v_local[rudder.sense_axis.index()];
        }
        torque_local[0] += self.helm[1] * self.mass * 9.81;
        torque_local[1] += self.helm[2] * self.mass * 9.81;
        torque_local[2] += self.helm[3] * self.mass * 9.81;
        for i in 0..3 {
            torque_local[i] -= self.propulsion.angular_drag_coeff * air_density * omega_local_in[i];
        }

        // World-frame linear force balance.
        let speed = norm3(self.vel);
        let drag = self.propulsion.drag_coeff * air_density * speed;
        let mut accel = [0.0f64; 3];
        for i in 0..3 {
            let mut force = sanitize(
                thrust_world[i] - drag * self.vel[i]
                    + if i == 1 {
                        buoyancy_scale
                            * self.buoyancy_force(air_density)
                            * (1.0 + self.helm[4]).max(0.0)
                            - self.mass * gravity
                    } else {
                        0.0
                    },
            );
            // Optional artificial station-keeping drag (`F = -k * v`); the
            // branch is skipped outright when disabled so the default path
            // keeps the legacy arithmetic bit-for-bit.
            if vacuum_damping > 0.0 {
                force = sanitize(force - vacuum_damping * self.vel[i]);
            }
            accel[i] = force / self.mass.max(1e-9);
        }
        for (v, &a) in self.vel.iter_mut().zip(&accel) {
            *v = sanitize(*v + a * dt);
        }
        clamp_magnitude(&mut self.vel, MAX_SPEED);
        for (p, &v) in self.pos.iter_mut().zip(&self.vel) {
            *p = sanitize(*p + v * dt);
        }

        // Euler angular dynamics in the *world* frame against the full
        // tensor: ω̇ = I_w⁻¹·(τ_w − ω × (I_w·ω)) with I_w = R·I·Rᵀ.
        let torque_world = quat_rotate(self.quat, torque_local);
        let rot = quat_to_mat3(self.quat);
        let rot_t = mat3_transpose(rot);
        let i_world = mat3_mul(mat3_mul(rot, self.inertia_tensor), rot_t);
        let i_world_inv = mat3_mul(mat3_mul(rot, self.inv_inertia_tensor), rot_t);
        let gyro = cross3(self.omega, mat3_vec_mul(i_world, self.omega));
        let rhs = [
            sanitize(torque_world[0] - gyro[0]),
            sanitize(torque_world[1] - gyro[1]),
            sanitize(torque_world[2] - gyro[2]),
        ];
        let alpha = mat3_vec_mul(i_world_inv, rhs);
        for (w, &a) in self.omega.iter_mut().zip(&alpha) {
            *w = sanitize(*w + a * dt);
        }
        clamp_magnitude(&mut self.omega, MAX_OMEGA);

        // q̇ = 1/2 * [ω_world, 0] ⊗ q (world-frame angular velocity).
        self.quat = integrate_quat(self.quat, self.omega, dt);
    }

    /// Advance one game tick through `dt` seconds of terrain/peer contact
    /// physics: the embedded factory ticks *once* (it is a game tick, not
    /// a solver substep), then `self.substeps` force-integration substeps
    /// run at `dt / substeps` each — 80 Hz with the default
    /// [`DEFAULT_PHYSICS_SUBSTEPS`] — with `backend` resolving
    /// voxel-vs-terrain and ship-vs-ship contacts after every substep.
    ///
    /// `terrain_solid` answers "is world cell occupied" in the parent
    /// dimension; `other_ships` are the awake peer sub-worlds this ship
    /// may collide with. Dormancy is exact: when `self.awake` is `false`
    /// the body is left untouched — zero force work, zero contact work —
    /// and a `substeps` of `0` degrades gracefully to a single step.
    /// `step`/`step_with_atmosphere` keep their legacy single-step path;
    /// call this entry point only where contacts matter.
    #[allow(clippy::too_many_arguments)]
    pub fn step_env_with_contacts(
        &mut self,
        dt: f64,
        gravity: f64,
        air_density: f64,
        buoyancy_scale: f64,
        vacuum_damping: f64,
        backend: &mut dyn SubLevelPhysicsBackend,
        terrain_solid: &dyn Fn([i32; 3]) -> bool,
        other_ships: &mut [&mut RigidBodySubWorld],
    ) {
        if !dt.is_finite() || dt <= 0.0 {
            return;
        }
        self.tick = self.tick.saturating_add(1);
        let _ = self
            .factory
            .step_tick(self.tick, INTERNAL_REGION_STEPS, None);
        if !self.awake {
            return;
        }
        let n = self.substeps.clamp(1, 64);
        let h = dt / f64::from(n);
        for _ in 0..n {
            if !self.awake {
                // The solver put the body to sleep mid-tick: remaining
                // substeps are skipped entirely (zero work dormancy).
                break;
            }
            self.integrate_forces(h, gravity, air_density, buoyancy_scale, vacuum_damping);
            backend.resolve_substep(self, h, terrain_solid, other_ships);
        }
    }

    /// Exact finite-step world-space displacement of a rigid point:
    /// `Δx = (p₁ + R₁·r) − (p₀ + R₀·r)` for a body that moved
    /// `(pos, quat)` -> `(next_pos, next_quat)` carrying a point at
    /// COM-relative local offset `r`. Chord-exact for any rotation
    /// magnitude — no small-angle approximation, no radial drift on
    /// high-RPM hulls.
    pub fn passenger_world_delta_exact(
        prev_pos: [f64; 3],
        prev_quat: [f64; 4],
        next_pos: [f64; 3],
        next_quat: [f64; 4],
        local_offset: [f64; 3],
    ) -> [f64; 3] {
        let r0 = quat_rotate(prev_quat, local_offset);
        let r1 = quat_rotate(next_quat, local_offset);
        [
            (next_pos[0] + r1[0]) - (prev_pos[0] + r0[0]),
            (next_pos[1] + r1[1]) - (prev_pos[1] + r0[1]),
            (next_pos[2] + r1[2]) - (prev_pos[2] + r0[2]),
        ]
    }

    /// Exact world-space displacement of a carried passenger over `dt`
    /// given their COM-relative local offset: the ship is forward-simulated
    /// through the *same* quaternion integrator `step` uses
    /// (`q₁ = integrate(q₀, ω, dt)`, `p₁ = p₀ + v·dt`) and the finite-step
    /// delta `(p₁ + R₁·r) − (p₀ + R₀·r)` returned — a 90° turn at a
    /// 20-block lever arm moves the rider along the true chord, not the
    /// tangent, so high-RPM hulls exhibit zero radial drift. Feeds
    /// `megu`'s `ac_note_carried_motion` hook so anti-cheat never
    /// rubber-bands airship pilots or passengers.
    pub fn passenger_world_delta(&self, local_offset: [f64; 3], dt: f64) -> [f64; 3] {
        if !dt.is_finite() || dt <= 0.0 {
            return [0.0; 3];
        }
        let next_pos = [
            sanitize(self.pos[0] + self.vel[0] * dt),
            sanitize(self.pos[1] + self.vel[1] * dt),
            sanitize(self.pos[2] + self.vel[2] * dt),
        ];
        if self.omega.iter().all(|&w| w == 0.0) {
            // Pure translation: rotation terms cancel exactly, and returning
            // `v·dt` directly avoids the round-off artifact of subtracting
            // two large, nearly equal world positions. This is the same
            // finite-step displacement, not the old tangent approximation.
            return [
                sanitize(self.vel[0] * dt),
                sanitize(self.vel[1] * dt),
                sanitize(self.vel[2] * dt),
            ];
        }
        let next_quat = integrate_quat(self.quat, self.omega, dt);
        Self::passenger_world_delta_exact(
            self.pos,
            self.quat,
            next_pos,
            next_quat,
            local_offset,
        )
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
        let rel = [w[0] - self.pos[0], w[1] - self.pos[1], w[2] - self.pos[2]];
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

/// Canonical [`WorldView`] surface for an airship sub-level: local block
/// cells read/write through the sparse `blocks` map via the embedded
/// registry's canonical-state codec, and [`Self::world_transform`]
/// reports the live rigid transform — `rotation` is `quat` as a 3×3,
/// `translation` is `pos`, `pivot` is `center_of_mass` — so callers see
/// exactly the `pos + R·(p − com)` mapping [`Self::local_to_world`]
/// documents.
impl WorldView for RigidBodySubWorld {
    fn block_state_at(&self, pos: [i32; 3]) -> Option<u32> {
        let canonical = self.blocks.get(&pos)?;
        self.factory.registry().decode_canonical(canonical)
    }

    fn set_block_state(&mut self, pos: [i32; 3], state: u32) -> bool {
        let Some(canonical) = self.factory.registry().encode_canonical(state) else {
            return false;
        };
        let canonical = canonical.to_string();
        self.set_block(pos, canonical);
        true
    }

    fn world_transform(&self) -> SubLevelTransform {
        SubLevelTransform {
            rotation: quat_to_mat3(self.quat),
            translation: self.pos,
            pivot: self.center_of_mass,
        }
    }
}

// ---------------------------------------------------------------------------
// SubLevelPhysicsBackend: pluggable rigid-body contact solver contract
// ---------------------------------------------------------------------------

/// Rigid-body/contact solver backend decoupling Minecraft gameplay
/// semantics (propulsion, buoyancy, assembly/disassembly, passenger
/// transforms — all owned by [`RigidBodySubWorld`]) from *how* contacts
/// are resolved. [`RigidBodySubWorld::step_env_with_contacts`] drives
/// `substeps` force-integration substeps per game tick and calls
/// [`Self::resolve_substep`] after each; a `rapier3d-f64` adapter can
/// replace [`BuiltinContactSolver`] behind this trait without touching
/// any gameplay code.
///
/// The backend mutates `ship` in place (impulses + positional
/// depenetration) and may apply equal-and-opposite impulses to entries of
/// `other_ships`, waking them. It must be deterministic: same inputs,
/// same floating-point result.
pub trait SubLevelPhysicsBackend {
    /// Resolve contacts for `ship` over one sub-step of `dt` seconds:
    /// voxel-vs-terrain contacts against `terrain_solid` cells and
    /// ship-vs-ship contacts against `other_ships`. Called after
    /// `integrate_forces` so `ship` reflects this sub-step's free motion.
    fn resolve_substep(
        &mut self,
        ship: &mut RigidBodySubWorld,
        dt: f64,
        terrain_solid: &dyn Fn([i32; 3]) -> bool,
        other_ships: &mut [&mut RigidBodySubWorld],
    );

    /// Backend identifier for diagnostics (`"builtin"`).
    fn backend_name(&self) -> &'static str;
}

/// Compact deterministic built-in contact solver: impulse-based normal
/// reaction with restitution, Coulomb friction, positional depenetration
/// and resting-sleep dormancy — strictly scoped to stable resting,
/// landing and basic ship-ship collision so `rapier3d-f64` can replace it
/// later. OBB-style voxel tests: terrain contacts probe each occupied
/// voxel's 8 world-space corners against the solid cell set; ship-vs-ship
/// contacts run voxel world-AABB overlap under a bounding-sphere broad
/// phase.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BuiltinContactSolver {
    /// Coefficient of restitution for hard impacts (`0` = perfectly
    /// plastic). Suppressed below [`Self::restitution_cutoff`] approach
    /// speed so resting stacks never jitter.
    pub restitution: f64,
    /// Approach speed (m/s) above which restitution applies.
    pub restitution_cutoff: f64,
    /// Coulomb friction coefficient `μ` clamping tangential impulse.
    pub friction: f64,
    /// Fraction of contact depth corrected positionally per sub-step.
    pub depenetration: f64,
    /// Maximum positional correction applied per contact (m).
    pub max_depenetration: f64,
    /// Extra linear/angular velocity bleed (`1/s`) applied while the hull
    /// is supported — lets sequential-impulse corner jitter decay to the
    /// sleep threshold instead of shivering forever.
    pub contact_damping: f64,
    /// Speed²+spin² threshold under which a *supported* substep counts
    /// toward sleep. Loose enough to admit the gravity-refeed limit cycle
    /// (~0.05–0.2 m/s) of a truly parked hull.
    pub sleep_epsilon2: f64,
    /// Consecutive supported quiet substeps required before the body is
    /// parked — the hysteresis that keeps a momentary contact pause from
    /// freezing a still-moving ship.
    pub min_sleep_substeps: u32,
}

impl Default for BuiltinContactSolver {
    fn default() -> Self {
        Self {
            restitution: 0.15,
            restitution_cutoff: 0.5,
            friction: 0.5,
            depenetration: 0.8,
            max_depenetration: 0.5,
            contact_damping: 6.0,
            sleep_epsilon2: 0.04,
            min_sleep_substeps: 16,
        }
    }
}

/// The eight local corners of a voxel cell (offsets `{0,1}³`).
const VOXEL_CORNER_OFFSETS: [[f64; 3]; 8] = [
    [0.0, 0.0, 0.0],
    [1.0, 0.0, 0.0],
    [0.0, 1.0, 0.0],
    [0.0, 0.0, 1.0],
    [1.0, 1.0, 0.0],
    [1.0, 0.0, 1.0],
    [0.0, 1.0, 1.0],
    [1.0, 1.0, 1.0],
];

/// The six axis-aligned unit directions (terrain face candidates).
const FACE_DIRS: [[i32; 3]; 6] = [
    [1, 0, 0],
    [-1, 0, 0],
    [0, 1, 0],
    [0, -1, 0],
    [0, 0, 1],
    [0, 0, -1],
];

/// `R·I⁻¹·Rᵀ` — the world-space inverse inertia of `ship`.
fn world_inv_inertia(ship: &RigidBodySubWorld) -> [[f64; 3]; 3] {
    let rot = quat_to_mat3(ship.quat);
    let rot_t = mat3_transpose(rot);
    mat3_mul(mat3_mul(rot, ship.inv_inertia_tensor), rot_t)
}

/// Point velocity of `ship` at world offset `r` from its COM:
/// `v + ω × r`.
fn contact_point_velocity(ship: &RigidBodySubWorld, r: [f64; 3]) -> [f64; 3] {
    let spin = cross3(ship.omega, r);
    [
        ship.vel[0] + spin[0],
        ship.vel[1] + spin[1],
        ship.vel[2] + spin[2],
    ]
}

/// Apply impulse `j` (kg·m/s) at world point `at`: `Δv = j/m`,
/// `Δω = I_w⁻¹ (r × j)` using the world inverse inertia computed once by
/// the caller.
fn apply_impulse(
    ship: &mut RigidBodySubWorld,
    i_world_inv: [[f64; 3]; 3],
    impulse: [f64; 3],
    at: [f64; 3],
) {
    let inv_m = 1.0 / ship.mass.max(1e-9);
    for (v, &j) in ship.vel.iter_mut().zip(&impulse) {
        *v = sanitize(*v + j * inv_m);
    }
    let r = [
        at[0] - ship.pos[0],
        at[1] - ship.pos[1],
        at[2] - ship.pos[2],
    ];
    let d_omega = mat3_vec_mul(i_world_inv, cross3(r, impulse));
    for (w, &dw) in ship.omega.iter_mut().zip(&d_omega) {
        *w = sanitize(*w + dw);
    }
    clamp_magnitude(&mut ship.vel, MAX_SPEED);
    clamp_magnitude(&mut ship.omega, MAX_OMEGA);
}

/// Impulse-denominator for direction `dir` at offset `r`:
/// `k = 1/m + (r×d)·I_w⁻¹·(r×d)`.
fn impulse_denominator(inv_m: f64, i_world_inv: [[f64; 3]; 3], r: [f64; 3], dir: [f64; 3]) -> f64 {
    let rxn = cross3(r, dir);
    let term = mat3_vec_mul(i_world_inv, rxn);
    inv_m + rxn[0] * term[0] + rxn[1] * term[1] + rxn[2] * term[2]
}

/// World-space AABB `(min, max)` of the voxel at `local` on `ship`.
fn voxel_world_aabb(ship: &RigidBodySubWorld, local: [i32; 3]) -> ([f64; 3], [f64; 3]) {
    let mut min = [f64::INFINITY; 3];
    let mut max = [f64::NEG_INFINITY; 3];
    for off in VOXEL_CORNER_OFFSETS {
        let w = ship.local_to_world([
            f64::from(local[0]) + off[0],
            f64::from(local[1]) + off[1],
            f64::from(local[2]) + off[2],
        ]);
        for i in 0..3 {
            min[i] = min[i].min(w[i]);
            max[i] = max[i].max(w[i]);
        }
    }
    (min, max)
}

/// Bounding-sphere radius of `ship`: farthest voxel-center distance from
/// COM plus one half-diagonal (√3/2).
fn ship_bounding_radius(ship: &RigidBodySubWorld) -> f64 {
    let mut r2 = 0.0f64;
    for local in ship.blocks.keys() {
        let d = [
            f64::from(local[0]) + 0.5 - ship.center_of_mass[0],
            f64::from(local[1]) + 0.5 - ship.center_of_mass[1],
            f64::from(local[2]) + 0.5 - ship.center_of_mass[2],
        ];
        r2 = r2.max(d[0] * d[0] + d[1] * d[1] + d[2] * d[2]);
    }
    r2.sqrt() + 0.87
}

impl BuiltinContactSolver {
    /// Effective restitution: hard impacts bounce, slow contacts stay
    /// plastic so a resting hull never chatters.
    fn restitution_at(&self, approach_speed: f64) -> f64 {
        if approach_speed > self.restitution_cutoff {
            self.restitution
        } else {
            0.0
        }
    }

    /// Collect voxel-corner-vs-terrain contacts: every world-space corner
    /// of every occupied voxel sitting inside a solid cell becomes a
    /// `(point, normal, depth)` contact whose normal is the shortest exit
    /// direction through a non-solid neighbor face.
    fn collect_terrain_contacts(
        ship: &RigidBodySubWorld,
        terrain_solid: &dyn Fn([i32; 3]) -> bool,
    ) -> Vec<([f64; 3], [f64; 3], f64)> {
        let mut contacts = Vec::new();
        for local in ship.blocks.keys() {
            for off in VOXEL_CORNER_OFFSETS {
                let w = ship.local_to_world([
                    f64::from(local[0]) + off[0],
                    f64::from(local[1]) + off[1],
                    f64::from(local[2]) + off[2],
                ]);
                let cell = [
                    w[0].floor() as i32,
                    w[1].floor() as i32,
                    w[2].floor() as i32,
                ];
                if !terrain_solid(cell) {
                    continue;
                }
                // Shallowest exit through a non-solid neighbor face.
                let mut best: Option<([f64; 3], f64)> = None;
                for dir in FACE_DIRS {
                    let axis = if dir[0] != 0 { 0 } else if dir[1] != 0 { 1 } else { 2 };
                    let sign = dir[axis];
                    let depth = if sign > 0 {
                        f64::from(cell[axis] + 1) - w[axis]
                    } else {
                        w[axis] - f64::from(cell[axis])
                    };
                    if depth <= 0.0 {
                        continue;
                    }
                    let mut neighbor = cell;
                    neighbor[axis] += sign;
                    if terrain_solid(neighbor) {
                        continue;
                    }
                    if best.is_none_or(|(_, d)| depth < d) {
                        best = Some((
                            [f64::from(dir[0]), f64::from(dir[1]), f64::from(dir[2])],
                            depth,
                        ));
                    }
                }
                if let Some((normal, depth)) = best {
                    contacts.push((w, normal, depth));
                }
            }
        }
        contacts
    }

    /// Collect ship-vs-ship contacts as `(point, normal, depth,
    /// other_index)` — `normal` pushes `ship` away from `other_ships[i]`.
    /// Broad phase rejects distant ships by bounding sphere; narrow phase
    /// tests voxel world-AABB overlap and picks the minimum-overlap axis.
    fn collect_ship_contacts(
        ship: &RigidBodySubWorld,
        other_ships: &[&mut RigidBodySubWorld],
    ) -> Vec<([f64; 3], [f64; 3], f64, usize)> {
        let mut contacts = Vec::new();
        let self_radius = ship_bounding_radius(ship);
        for (oi, other) in other_ships.iter().enumerate() {
            let span = [
                ship.pos[0] - other.pos[0],
                ship.pos[1] - other.pos[1],
                ship.pos[2] - other.pos[2],
            ];
            let reach = self_radius + ship_bounding_radius(other) + 1.0;
            if span[0] * span[0] + span[1] * span[1] + span[2] * span[2] > reach * reach {
                continue;
            }
            for la in ship.blocks.keys() {
                let (a_min, a_max) = voxel_world_aabb(ship, *la);
                for lb in other.blocks.keys() {
                    let (b_min, b_max) = voxel_world_aabb(other, *lb);
                    let mut overlap = [0.0f64; 3];
                    let mut touching = true;
                    for axis in 0..3 {
                        overlap[axis] = a_max[axis].min(b_max[axis]) - a_min[axis].max(b_min[axis]);
                        if overlap[axis] <= 0.0 {
                            touching = false;
                            break;
                        }
                    }
                    if !touching {
                        continue;
                    }
                    // Minimum-overlap axis becomes the contact normal,
                    // oriented from the other ship's voxel toward ours.
                    let axis = (0..3)
                        .min_by(|&a, &b| overlap[a].total_cmp(&overlap[b]))
                        .unwrap_or(1);
                    let a_c = (a_min[axis] + a_max[axis]) * 0.5;
                    let b_c = (b_min[axis] + b_max[axis]) * 0.5;
                    let mut normal = [0.0f64; 3];
                    normal[axis] = if a_c >= b_c { 1.0 } else { -1.0 };
                    let point = [
                        (a_min[0].max(b_min[0]) + a_max[0].min(b_max[0])) * 0.5,
                        (a_min[1].max(b_min[1]) + a_max[1].min(b_max[1])) * 0.5,
                        (a_min[2].max(b_min[2]) + a_max[2].min(b_max[2])) * 0.5,
                    ];
                    contacts.push((point, normal, overlap[axis], oi));
                }
            }
        }
        contacts
    }
}

impl SubLevelPhysicsBackend for BuiltinContactSolver {
    fn resolve_substep(
        &mut self,
        ship: &mut RigidBodySubWorld,
        dt: f64,
        terrain_solid: &dyn Fn([i32; 3]) -> bool,
        other_ships: &mut [&mut RigidBodySubWorld],
    ) {
        let inv_m = 1.0 / ship.mass.max(1e-9);
        let i_world_inv = world_inv_inertia(ship);
        let mut supported = false;

        // -- Voxel vs terrain: sequential impulses + positional correction.
        for (point, normal, depth) in Self::collect_terrain_contacts(ship, terrain_solid) {
            supported = supported || normal[1] > 0.5;
            let r = [
                point[0] - ship.pos[0],
                point[1] - ship.pos[1],
                point[2] - ship.pos[2],
            ];
            let v_c = contact_point_velocity(ship, r);
            let vn = v_c[0] * normal[0] + v_c[1] * normal[1] + v_c[2] * normal[2];
            if vn < 0.0 {
                let e = self.restitution_at(-vn);
                let k = impulse_denominator(inv_m, i_world_inv, r, normal).max(1e-12);
                let j_normal = -(1.0 + e) * vn / k;
                apply_impulse(ship, i_world_inv, [j_normal * normal[0], j_normal * normal[1], j_normal * normal[2]], point);
                // Coulomb friction: stop tangential sliding, clamped by μ·j_n.
                let v2 = contact_point_velocity(ship, r);
                let tvn = v2[0] * normal[0] + v2[1] * normal[1] + v2[2] * normal[2];
                let tangential = [
                    v2[0] - tvn * normal[0],
                    v2[1] - tvn * normal[1],
                    v2[2] - tvn * normal[2],
                ];
                let t_len = norm3(tangential);
                if t_len > 1e-9 {
                    let t_hat = [tangential[0] / t_len, tangential[1] / t_len, tangential[2] / t_len];
                    let kt = impulse_denominator(inv_m, i_world_inv, r, t_hat).max(1e-12);
                    let jt = (t_len / kt).min(self.friction * j_normal);
                    apply_impulse(
                        ship,
                        i_world_inv,
                        [-jt * t_hat[0], -jt * t_hat[1], -jt * t_hat[2]],
                        point,
                    );
                }
            }
            // Positional depenetration: the corner exits the terrain cell
            // along the contact normal — capped per contact so a deeply
            // embedded voxel recovers over substeps, not explosively.
            let corr = depth.min(self.max_depenetration).max(0.0) * self.depenetration;
            for i in 0..3 {
                ship.pos[i] = sanitize(ship.pos[i] + normal[i] * corr);
            }
        }

        // -- Ship vs ship: paired impulse keeps total linear momentum.
        for (point, normal, depth, oi) in Self::collect_ship_contacts(ship, other_ships) {
            let other = &mut *other_ships[oi];
            let inv_mo = 1.0 / other.mass.max(1e-9);
            let i_world_inv_o = world_inv_inertia(other);
            let ra = [
                point[0] - ship.pos[0],
                point[1] - ship.pos[1],
                point[2] - ship.pos[2],
            ];
            let rb = [
                point[0] - other.pos[0],
                point[1] - other.pos[1],
                point[2] - other.pos[2],
            ];
            let rel = {
                let va = contact_point_velocity(ship, ra);
                let vb = contact_point_velocity(other, rb);
                [va[0] - vb[0], va[1] - vb[1], va[2] - vb[2]]
            };
            let rn = rel[0] * normal[0] + rel[1] * normal[1] + rel[2] * normal[2];
            if rn < 0.0 {
                let e = self.restitution_at(-rn);
                let k = (impulse_denominator(inv_m, i_world_inv, ra, normal)
                    + impulse_denominator(inv_mo, i_world_inv_o, rb, normal))
                .max(1e-12);
                let j = -(1.0 + e) * rn / k;
                let impulse = [j * normal[0], j * normal[1], j * normal[2]];
                apply_impulse(ship, i_world_inv, impulse, point);
                apply_impulse(other, i_world_inv_o, [-impulse[0], -impulse[1], -impulse[2]], point);
                other.wake();
            }
            // Positional depenetration split by inverse mass.
            let share = inv_m / (inv_m + inv_mo);
            let corr = depth.min(self.max_depenetration).max(0.0) * self.depenetration;
            for i in 0..3 {
                ship.pos[i] = sanitize(ship.pos[i] + normal[i] * corr * share);
                other.pos[i] = sanitize(other.pos[i] - normal[i] * corr * (1.0 - share));
            }
        }

        // Contact damping: while the hull is supported, bleed residual
        // linear+angular jitter so corner-probe noise from sequential
        // impulses actually decays toward the sleep threshold instead of
        // holding the body at a low-grade shiver forever.
        if supported {
            let damp = (1.0 - self.contact_damping * dt).max(0.0);
            for v in &mut ship.vel {
                *v *= damp;
            }
            for w in &mut ship.omega {
                *w *= damp;
            }
        }

        // Resting-sleep dormancy with hysteresis: count consecutive
        // supported quiet substeps and park only after `min_sleep_substeps`
        // straight — zero solver work until something wakes the ship.
        if supported
            && norm3(ship.vel) * norm3(ship.vel) + norm3(ship.omega) * norm3(ship.omega)
                < self.sleep_epsilon2
        {
            ship.rest_substeps = ship.rest_substeps.saturating_add(1);
            if ship.rest_substeps >= self.min_sleep_substeps {
                ship.rest_substeps = 0;
                ship.awake = false;
            }
        } else if supported {
            ship.rest_substeps = 0;
        }
    }

    fn backend_name(&self) -> &'static str {
        "builtin"
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
        return Err(AeronauticsError::CorruptSnapshot(
            "bad magic/version".into(),
        ));
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
        atmosphere: AtmospherePhysics::OVERWORLD,
        tick,
        min_rpms,
        mass,
        center_of_mass,
        inertia_diag,
        inertia_tensor: diag3(inertia_diag),
        inv_inertia_tensor: diag3([
            1.0 / inertia_diag[0].max(1e-9),
            1.0 / inertia_diag[1].max(1e-9),
            1.0 / inertia_diag[2].max(1e-9),
        ]),
        mass_overrides: BTreeMap::new(),
        substeps: DEFAULT_PHYSICS_SUBSTEPS,
        awake: true,
        rest_substeps: 0,
        buoyancy_blocks,
        propellers,
        rudders,
        helm: [0.0; 5],
    };
    // Legacy snapshots carry only `inertia_diag`; recompute the full
    // tensor from the (deterministic) block set + stored COM so a
    // restored asymmetric hull keeps the writer's exact products of
    // inertia instead of silently going diagonal.
    sub.refresh_inertia_tensor();
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
    megu_core::hash::crc32_ieee(data)
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
    // Already unit to within fp noise: dividing through would only
    // perturb the low bits — and would break the byte-for-byte snapshot
    // roundtrip, since decode re-normalizes every decoded quaternion.
    if (n - 1.0).abs() <= 1e-15 {
        return q;
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

/// Unit quaternion `q` as a row-major 3×3 rotation matrix (`world = R·local`),
/// the same basis `quat_rotate` uses.
fn quat_to_mat3(q: [f64; 4]) -> [[f64; 3]; 3] {
    let (x, y, z, w) = (q[0], q[1], q[2], q[3]);
    [
        [1.0 - 2.0 * (y * y + z * z), 2.0 * (x * y - z * w), 2.0 * (x * z + y * w)],
        [2.0 * (x * y + z * w), 1.0 - 2.0 * (x * x + z * z), 2.0 * (y * z - x * w)],
        [2.0 * (x * z - y * w), 2.0 * (y * z + x * w), 1.0 - 2.0 * (x * x + y * y)],
    ]
}

/// Advance `q` by `ω` (world frame, rad/s) for `dt` seconds:
/// `q̇ = ½ [ω, 0] ⊗ q`, Euler step + renormalize — the identical
/// integrator [`RigidBodySubWorld::integrate_forces`] uses so passenger
/// deltas and the pose trajectory never diverge.
fn integrate_quat(q: [f64; 4], omega: [f64; 3], dt: f64) -> [f64; 4] {
    let wq = [omega[0], omega[1], omega[2], 0.0];
    let qdot = quat_mul(wq, q);
    let mut out = q;
    for (c, &dq) in out.iter_mut().zip(&qdot) {
        *c += 0.5 * dq * dt;
    }
    quat_normalize(out)
}

/// Diagonal 3×3 matrix with `d` on the diagonal.
fn diag3(d: [f64; 3]) -> [[f64; 3]; 3] {
    [[d[0], 0.0, 0.0], [0.0, d[1], 0.0], [0.0, 0.0, d[2]]]
}

fn mat3_transpose(m: [[f64; 3]; 3]) -> [[f64; 3]; 3] {
    std::array::from_fn(|i| std::array::from_fn(|j| m[j][i]))
}

fn mat3_vec_mul(m: [[f64; 3]; 3], v: [f64; 3]) -> [f64; 3] {
    std::array::from_fn(|i| m[i][0] * v[0] + m[i][1] * v[1] + m[i][2] * v[2])
}

fn mat3_mul(a: [[f64; 3]; 3], b: [[f64; 3]; 3]) -> [[f64; 3]; 3] {
    std::array::from_fn(|i| {
        std::array::from_fn(|j| a[i][0] * b[0][j] + a[i][1] * b[1][j] + a[i][2] * b[2][j])
    })
}

/// Exact 3×3 inverse via the adjugate; `None` for a singular or
/// non-finite matrix (callers fall back to a safe diagonal). Body
/// inertia tensors are positive-definite by construction, so the `None`
/// path is a defensive guard, not a reachable failure.
fn mat3_invert(m: &[[f64; 3]; 3]) -> Option<[[f64; 3]; 3]> {
    let c = |r0: usize, r1: usize, c0: usize, c1: usize| {
        m[r0][c0] * m[r1][c1] - m[r0][c1] * m[r1][c0]
    };
    let cof = [
        [c(1, 2, 1, 2), -c(1, 2, 0, 2), c(1, 2, 0, 1)],
        [-c(0, 2, 1, 2), c(0, 2, 0, 2), -c(0, 2, 0, 1)],
        [c(0, 1, 1, 2), -c(0, 1, 0, 2), c(0, 1, 0, 1)],
    ];
    let det = m[0][0] * cof[0][0] + m[0][1] * cof[0][1] + m[0][2] * cof[0][2];
    if !det.is_finite() || det.abs() < 1e-30 {
        return None;
    }
    // Inverse is the transposed cofactor matrix scaled by 1/det.
    Some(std::array::from_fn(|i| {
        std::array::from_fn(|j| sanitize(cof[j][i] / det))
    }))
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
    Some([r_f64(bytes, c)?, r_f64(bytes, c)?, r_f64(bytes, c)?])
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

fn is_propulsion_block(id: &str) -> bool {
    let path = id_path(id);
    id == "aeronautics:andesite_propeller"
        || id == "aeronautics:gyroscopic_propeller_bearing"
        || (id.starts_with("simulated:") && path.contains("portable_engine"))
        || (id.starts_with("simulated:") && path.contains("propeller"))
        || path.contains("thruster")
}

fn is_lift_block(id: &str) -> bool {
    let path = id_path(id);
    id == "aeronautics:adjustable_burner"
        || (id.starts_with("aeronautics:") && (path.contains("envelope") || path.contains("sail")))
        || (id.starts_with("simulated:") && path.contains("sail"))
        || path.contains("balloon")
}

fn is_control_block(id: &str) -> bool {
    let path = id_path(id);
    (id.starts_with("simulated:") && (path.contains("handle") || path.contains("helm")))
        || path.contains("rudder")
        || path == "helm"
        || path == "cockpit"
        || id == "simulated:altitude_sensor"
        || id == "simulated:analog_transmission"
        || id == "offroad:wheel_mount"
        || id == "offroad:rockcutting_wheel"
        || id == "offroad:borehead_bearing"
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
        let reg = create_registry_with_addons(DEFAULT_BASE_STATE_ID, std::slice::from_ref(&addon))
            .unwrap();
        (addon, reg)
    }

    #[test]
    fn real_aeronautics_133_ids_classify_as_physics_and_controls() {
        let (addon, reg) = aero_registry();
        let ids = [
            "aeronautics:andesite_propeller",
            "aeronautics:adjustable_burner",
            "aeronautics:red_envelope",
            "aeronautics:blue_envelope_encased_shaft",
            "simulated:black_portable_engine",
            "simulated:small_propeller",
            "simulated:symmetric_sail",
            "simulated:altitude_sensor",
            "simulated:analog_transmission",
            "simulated:steering_handle",
            "offroad:wheel_mount",
        ];
        let cells = ids
            .iter()
            .enumerate()
            .map(|(i, id)| ([i as i32, 0, 0], (*id).to_owned()))
            .collect();
        let sub = RigidBodySubWorld::assemble(1, 0, cells, &reg, Some(&addon), 100.0).unwrap();
        assert!(sub.propellers.len() >= 3);
        assert!(sub.buoyancy_blocks >= 4);
        assert!(sub.rudders.len() >= 4);
    }

    #[test]
    fn subworld_sync_frame_roundtrip_and_bounds_fail_closed() {
        let frame = SubWorldSyncFrame {
            kind: SubWorldSyncKind::FullSpawn,
            subworld_id: 17,
            dim_id: "minecraft:overworld".into(),
            pos: [1.25, 64.0, -2.5],
            quat: [0.0, 0.0, 0.0, 1.0],
            vel: [0.5, 0.0, -0.25],
            ang_vel: [0.0, 0.1, 0.0],
            revision: 3,
            voxels: vec![([0, 0, 0], 1), ([2, -1, 3], 42)],
        };
        let bytes = frame.encode().unwrap();
        assert_eq!(SubWorldSyncFrame::decode(&bytes).unwrap(), frame);
        assert!(SubWorldSyncFrame::decode(&bytes[..bytes.len() - 1]).is_err());
        let mut oversized = bytes.clone();
        let count_at = 84 + frame.dim_id.len();
        oversized[count_at..count_at + 4]
            .copy_from_slice(&((MAX_SUBWORLD_VOXELS as u32) + 1).to_be_bytes());
        assert!(SubWorldSyncFrame::decode(&oversized).is_err());
        let mut invalid_quat = frame.clone();
        invalid_quat.quat = [0.0, 0.0, 0.0, 0.0];
        assert!(invalid_quat.encode().is_err());
        let mut pose = SubWorldSyncFrame {
            kind: SubWorldSyncKind::PoseDelta,
            voxels: Vec::new(),
            ..frame
        };
        pose.pos[0] = f64::NAN;
        assert!(pose.encode().is_err());
    }

    #[test]
    fn subworld_helm_frame_roundtrip_and_non_finite_rejection() {
        let input = SubWorldHelmInput {
            subworld_id: 99,
            throttle: 0.5,
            pitch: -0.2,
            yaw: 1.0,
            roll: 0.0,
            buoyancy_delta: -0.4,
            disassemble: false,
        };
        let bytes = input.encode().unwrap();
        assert_eq!(SubWorldHelmInput::decode(&bytes).unwrap(), input);
        assert!(SubWorldHelmInput::decode(&bytes[..28]).is_err());
        let mut nan = bytes;
        nan[8..12].copy_from_slice(&f32::NAN.to_be_bytes());
        assert!(SubWorldHelmInput::decode(&nan).is_err());
        let invalid = SubWorldHelmInput {
            throttle: f32::INFINITY,
            ..input
        };
        assert!(invalid.encode().is_err());
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
        for v in sub
            .pos
            .iter()
            .chain(&sub.vel)
            .chain(&sub.omega)
            .chain(&sub.quat)
        {
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
        // Mass is the sum of per-block material masses (the envelope is
        // an ultralight, the rest fall back to the default block mass).
        let mass_of = |id: &str| material_mass_kg(id, 100.0);
        let expected_mass: f64 = sub.blocks.values().map(|id| mass_of(id)).sum();
        assert_eq!(sub.mass, expected_mass);
        // CoM is the mass-weighted mean of cell centers.
        let mut weighted = [0.0f64; 3];
        for (p, id) in &sub.blocks {
            let m = mass_of(id);
            for i in 0..3 {
                weighted[i] += m * (f64::from(p[i]) + 0.5);
            }
        }
        let expected_com = [
            weighted[0] / expected_mass,
            weighted[1] / expected_mass,
            weighted[2] / expected_mass,
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
        // Inertia diagonal: I_x = Σ m(dy²+dz²) + m/6 etc., all strictly positive.
        for &i in &sub.inertia_diag {
            assert!(i > 0.0 && i.is_finite());
        }
        let mut manual = [0.0f64; 3];
        for (p, id) in &sub.blocks {
            let m = mass_of(id);
            let d = [
                f64::from(p[0]) + 0.5 - expected_com[0],
                f64::from(p[1]) + 0.5 - expected_com[1],
                f64::from(p[2]) + 0.5 - expected_com[2],
            ];
            manual[0] += m * (d[1] * d[1] + d[2] * d[2]) + m / 6.0;
            manual[1] += m * (d[0] * d[0] + d[2] * d[2]) + m / 6.0;
            manual[2] += m * (d[0] * d[0] + d[1] * d[1]) + m / 6.0;
        }
        for (&got, &want) in sub.inertia_diag.iter().zip(&manual) {
            assert!((got - want).abs() < 1e-9);
        }

        // Edits recompute physicals.
        let mut sub = sub;
        sub.remove_block([0, 1, 0]);
        assert_eq!(
            sub.mass,
            expected_mass - material_mass_kg("createaeronautics:balloon_envelope", 100.0)
        );
        assert_eq!(sub.buoyancy_blocks, 0);
        sub.set_block(
            [0, 1, 0],
            canonical(&reg, "createaeronautics:levitation_core"),
        );
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
            canonical_with(
                &reg,
                "createaeronautics:propeller_bearing",
                "facing",
                "east",
            ),
        );
        cells.insert(
            [0, 1, 0],
            canonical(&reg, "createaeronautics:balloon_envelope"),
        );

        let mut sub = RigidBodySubWorld::assemble(1, 0, cells, &reg, Some(&addon), 100.0).unwrap();
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
        cells.insert([1, 0, 0], deck.clone());
        cells.insert([0, 1, 0], envelope.clone());
        cells.insert([1, 1, 0], envelope.clone());
        let block_mass = 100.0;
        // Mass comes from the per-material table: assembler decks weigh
        // the default mass, envelopes are ultralight.
        let mass = 2.0 * material_mass_kg(&deck, block_mass)
            + 2.0 * material_mass_kg(&envelope, block_mass);
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
        cells.insert(
            [0, 0, 0],
            canonical(&reg, "createaeronautics:airship_assembler"),
        );
        cells.insert([2, 0, 0], canonical(&reg, "createaeronautics:rudder"));
        let mut sub = RigidBodySubWorld::assemble(1, 3, cells, &reg, Some(&addon), 100.0).unwrap();
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

        // Formula pinned: the exact finite-step rigid-body chord
        // `(p₁ + R₁·r) − (p₀ + R₀·r)` with `p₁ = p₀ + v·dt` and `q₁`
        // from the same quaternion integrator `step` uses.
        let offset = [0.5, 1.0, -2.0];
        let next_pos = [
            sub.pos[0] + sub.vel[0] * 0.05,
            sub.pos[1] + sub.vel[1] * 0.05,
            sub.pos[2] + sub.vel[2] * 0.05,
        ];
        let next_quat = integrate_quat(sub.quat, sub.omega, 0.05);
        let expect = RigidBodySubWorld::passenger_world_delta_exact(
            sub.pos, sub.quat, next_pos, next_quat, offset,
        );
        let got = sub.passenger_world_delta(offset, 0.05);
        for i in 0..3 {
            assert_eq!(got[i], expect[i]);
        }

        // Finite-difference consistency: with gravity/drag disabled the
        // delta reproduces the actual pose change up to the gyroscopic
        // term — on an asymmetric hull ω precesses under `ω × (Iw·ω)`
        // even torque-free, so the tolerance covers O(|ω|²·dt²) drift.
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
            canonical_with(
                &reg,
                "createaeronautics:propeller_bearing",
                "facing",
                "east",
            ),
        );
        cells.insert(
            [0, 1, 0],
            canonical(&reg, "createaeronautics:balloon_envelope"),
        );
        cells.insert([0, 0, 1], canonical(&reg, "createaeronautics:rudder"));

        let mut sub = RigidBodySubWorld::assemble(42, 3, cells, &reg, Some(&addon), 100.0).unwrap();
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
            canonical_with(
                &reg,
                "createaeronautics:propeller_bearing",
                "facing",
                "east",
            ),
        );
        cells.insert(
            [0, 1, 0],
            canonical(&reg, "createaeronautics:balloon_envelope"),
        );

        let mut sub = RigidBodySubWorld::assemble(7, 3, cells, &reg, Some(&addon), 100.0).unwrap();
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
        cells.insert(
            [0, 0, 0],
            canonical(&reg, "createaeronautics:airship_assembler"),
        );
        cells.insert(
            [0, 1, 0],
            canonical(&reg, "createaeronautics:balloon_envelope"),
        );
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

    // -- P0-A: per-material mass + full 3x3 inertia tensor -------------

    fn single_ship(blocks: &[([i32; 3], &str)]) -> RigidBodySubWorld {
        let (_addon, reg) = aero_registry();
        let cells: BTreeMap<[i32; 3], String> = blocks
            .iter()
            .map(|(p, id)| (*p, (*id).to_string()))
            .collect();
        RigidBodySubWorld::assemble(7, 0, cells, &reg, None, DEFAULT_BLOCK_MASS).unwrap()
    }

    #[test]
    fn material_mass_table_weighs_hulls_and_balloons_realistically() {
        assert_eq!(material_mass_kg("minecraft:obsidian", 100.0), 1200.0);
        assert_eq!(material_mass_kg("minecraft:iron_block", 100.0), 800.0);
        assert_eq!(material_mass_kg("minecraft:stone", 100.0), 200.0);
        assert_eq!(material_mass_kg("minecraft:cobblestone", 100.0), 200.0);
        assert_eq!(material_mass_kg("minecraft:oak_log[axis=y]", 100.0), 70.0);
        assert_eq!(material_mass_kg("minecraft:white_wool", 100.0), 15.0);
        assert_eq!(material_mass_kg("aeronautics:red_envelope", 100.0), 8.0);
        assert_eq!(
            material_mass_kg("aeronautics:andesite_propeller", 100.0),
            100.0,
            "composite propellers weigh like mixed machinery, not solid andesite"
        );
        assert_eq!(
            material_mass_kg("createaeronautics:airship_assembler", 100.0),
            192.0,
            "assembly machinery is dense metal gearing in a stone casing"
        );
        assert_eq!(
            material_mass_kg("somemod:mystery_alloy", 100.0),
            100.0,
            "unlisted materials fall back to the default mass"
        );
    }

    #[test]
    fn mass_weighted_com_and_full_inertia_tensor_are_exact() {
        // Two uniform stones: COM lands on the midpoint cell boundary,
        // diagonal terms get the m/6 self-inertia floor.
        let sub = single_ship(&[([0, 0, 0], "minecraft:stone"), ([1, 0, 0], "minecraft:stone")]);
        assert!((sub.mass - 400.0).abs() < 1e-9);
        assert!((sub.center_of_mass[0] - 1.0).abs() < 1e-12);
        assert!((sub.center_of_mass[1] - 0.5).abs() < 1e-12);
        // Iyy = Izz = 2*(200*0.25) + 2*(200/6); Ixx = 2*(200/6).
        let s = 200.0 / 6.0;
        assert!((sub.inertia_tensor[0][0] - 2.0 * s).abs() < 1e-9);
        assert!((sub.inertia_tensor[1][1] - (100.0 + 2.0 * s)).abs() < 1e-9);
        assert_eq!(sub.inertia_diag[0], sub.inertia_tensor[0][0]);
        assert_eq!(sub.inertia_diag[1], sub.inertia_tensor[1][1]);
        assert_eq!(sub.inertia_diag[2], sub.inertia_tensor[2][2]);
        // I·I⁻¹ == identity.
        let product = mat3_mul(sub.inertia_tensor, sub.inv_inertia_tensor);
        for (i, row) in product.iter().enumerate() {
            for (j, got) in row.iter().enumerate() {
                let want = if i == j { 1.0 } else { 0.0 };
                assert!(
                    (*got - want).abs() < 1e-9,
                    "I·I⁻¹[{i}][{j}] = {got}"
                );
            }
        }
    }

    #[test]
    fn asymmetric_materials_shift_com_and_produce_products_of_inertia() {
        // Obsidian keel far out on +X, wool superstructure on +Y.
        let sub = single_ship(&[
            ([0, 0, 0], "minecraft:white_wool"),
            ([0, 1, 0], "minecraft:white_wool"),
            ([4, 0, 0], "minecraft:obsidian"),
        ]);
        let m = 15.0 + 15.0 + 1200.0;
        assert!((sub.mass - m).abs() < 1e-9);
        // Mass-weighted COM: x pulled hard toward the obsidian.
        let cx = (15.0 * 0.5 + 15.0 * 0.5 + 1200.0 * 4.5) / m;
        let cy = (15.0 * 0.5 + 15.0 * 1.5 + 1200.0 * 0.5) / m;
        assert!((sub.center_of_mass[0] - cx).abs() < 1e-12);
        assert!((sub.center_of_mass[1] - cy).abs() < 1e-12);
        assert!(
            sub.inertia_tensor[0][1].abs() > 1e-9,
            "an asymmetric hull must carry a nonzero Ixy product of inertia"
        );
        // Symmetric + invertible.
        assert_eq!(sub.inertia_tensor[0][1], sub.inertia_tensor[1][0]);
        assert_eq!(sub.inertia_tensor[0][2], sub.inertia_tensor[2][0]);
        assert_eq!(sub.inertia_tensor[1][2], sub.inertia_tensor[2][1]);
        let product = mat3_mul(sub.inertia_tensor, sub.inv_inertia_tensor);
        for (i, row) in product.iter().enumerate() {
            for (j, got) in row.iter().enumerate() {
                let want = if i == j { 1.0 } else { 0.0 };
                assert!((*got - want).abs() < 1e-9);
            }
        }
    }

    #[test]
    fn single_block_and_collinear_ships_have_invertible_tensors() {
        let single = single_ship(&[([0, 0, 0], "minecraft:stone")]);
        // I = m/6 on the diagonal even with zero parallel-axis offset.
        assert!((single.inertia_tensor[0][0] - 200.0 / 6.0).abs() < 1e-9);
        let product = mat3_mul(single.inertia_tensor, single.inv_inertia_tensor);
        assert!((product[0][0] - 1.0).abs() < 1e-9);
        // Three blocks in a row: a collinear rod still inverts.
        let rod = single_ship(&[
            ([0, 0, 0], "minecraft:stone"),
            ([1, 0, 0], "minecraft:stone"),
            ([2, 0, 0], "minecraft:stone"),
        ]);
        let product = mat3_mul(rod.inertia_tensor, rod.inv_inertia_tensor);
        assert!((product[1][1] - 1.0).abs() < 1e-9);
    }

    #[test]
    fn mass_overrides_beat_the_table() {
        let mut sub = single_ship(&[([0, 0, 0], "minecraft:stone")]);
        assert!((sub.mass - 200.0).abs() < 1e-9);
        assert!(sub.set_block_mass_override("minecraft:stone", 950.0));
        assert!((sub.mass - 950.0).abs() < 1e-9);
        assert!(sub.clear_block_mass_override("minecraft:stone"));
        assert!((sub.mass - 200.0).abs() < 1e-9);
        assert!(!sub.set_block_mass_override("minecraft:stone", f64::NAN));
        assert!(!sub.set_block_mass_override("minecraft:stone", -5.0));
    }

    // -- P0-B: exact finite-step passenger displacement -----------------

    #[test]
    fn passenger_delta_small_angle_matches_tangent_approximation() {
        let mut sub = single_ship(&[([0, 0, 0], "minecraft:stone")]);
        sub.pos = [10.0, 100.0, 10.0];
        sub.vel = [1.0, 0.0, 0.0];
        sub.omega = [0.0, 0.01, 0.0];
        let r = [2.0, 0.0, 0.0];
        let dt = 0.001;
        let d = sub.passenger_world_delta(r, dt);
        // (v + ω × (R·r))·dt: ω×r = [0,0,-0.02] → [0.001, 0, -0.00002].
        assert!((d[0] - 0.001).abs() < 1e-6, "{}", d[0]);
        assert!(d[1].abs() < 1e-9);
        assert!((d[2] + 0.00002).abs() < 1e-6, "{}", d[2]);
    }

    #[test]
    fn passenger_delta_pure_translation_is_exact_velocity_step() {
        let mut sub = single_ship(&[([0, 0, 0], "minecraft:stone")]);
        sub.pos = [1.0e6, 100.0, -1.0e6];
        sub.vel = [97.97957773551134, -0.25, 3.5];
        sub.omega = [0.0; 3];
        assert_eq!(
            sub.passenger_world_delta([0.5, 1.0, -0.5], 0.05),
            [
                sub.vel[0] * 0.05,
                sub.vel[1] * 0.05,
                sub.vel[2] * 0.05,
            ],
            "with no rotation the finite-step rotational terms cancel exactly"
        );
    }

    #[test]
    fn passenger_delta_exact_quarter_turn_has_zero_radial_drift() {
        let mut sub = single_ship(&[([0, 0, 0], "minecraft:stone")]);
        sub.pos = [0.0, 100.0, 0.0];
        sub.vel = [0.0; 3];
        // ω·dt = 2 → integrated turn is exactly 90° about +Y
        // (2·atan(ωdt/2) = 2·atan(1) = π/2).
        sub.omega = [0.0, 40.0, 0.0];
        let d = sub.passenger_world_delta([20.0, 0.0, 0.0], 0.05);
        // Exact chord: r0=(20,0,0) -> r1=(0,0,-20), Δ = (-20,0,-20).
        assert!((d[0] + 20.0).abs() < 1e-9, "{}", d[0]);
        assert!(d[1].abs() < 1e-9);
        assert!((d[2] + 20.0).abs() < 1e-9, "{}", d[2]);
        // Zero radial drift: the endpoint stays exactly on the 20-block
        // sphere — the old tangent step placed it at r·√(1+θ²) ≈ 25.8.
        let endpoint_r = norm3([d[0] + 20.0, d[1], d[2]]);
        assert!((endpoint_r - 20.0).abs() < 1e-9, "radial drift {}", endpoint_r - 20.0);
    }

    #[test]
    fn passenger_world_delta_exact_is_chord_exact_between_poses() {
        let q0 = [0.0, 0.0, 0.0, 1.0];
        let s45 = std::f64::consts::FRAC_1_SQRT_2;
        let q90y = [0.0, s45, 0.0, s45];
        let d = RigidBodySubWorld::passenger_world_delta_exact(
            [0.0; 3],
            q0,
            [0.0; 3],
            q90y,
            [20.0, 0.0, 0.0],
        );
        assert!((d[0] + 20.0).abs() < 1e-12);
        assert!((d[2] + 20.0).abs() < 1e-12);
        // Translation and rotation compose.
        let d = RigidBodySubWorld::passenger_world_delta_exact(
            [1.0, 2.0, 3.0],
            q0,
            [5.0, 6.0, 7.0],
            q90y,
            [20.0, 0.0, 0.0],
        );
        assert!((d[0] + 16.0).abs() < 1e-12, "{}", d[0]); // (5+0)-(1+20)
        assert!((d[1] - 4.0).abs() < 1e-12);
        assert!((d[2] + 16.0).abs() < 1e-12); // (7-20)-(3+0)
    }

    // -- P0-D: dynamic COM conservation on block edits ------------------

    #[test]
    fn block_edits_shift_pos_so_existing_voxels_stay_world_exact() {
        let mut sub = single_ship(&[([0, 0, 0], "minecraft:stone"), ([1, 0, 0], "minecraft:stone")]);
        // Nontrivial pose: 45° about +Y so the COM delta rotates too.
        let s = (45.0_f64 / 2.0).to_radians().sin();
        let c = (45.0_f64 / 2.0).to_radians().cos();
        sub.quat = [0.0, s, 0.0, c];
        sub.pos = [10.0, 100.0, 10.0];
        let before: Vec<[f64; 3]> = sub
            .blocks
            .keys()
            .map(|p| sub.local_to_world([p[0] as f64 + 0.5, p[1] as f64 + 0.5, p[2] as f64 + 0.5]))
            .collect();

        // A heavy keel far off-axis must shift the COM — and `pos` must
        // absorb the shift so the two old voxels keep their world cells.
        sub.set_block([4, 0, 0], "minecraft:obsidian");
        for (p, &w0) in sub.blocks.keys().take(2).zip(&before) {
            let w1 = sub.local_to_world([p[0] as f64 + 0.5, p[1] as f64 + 0.5, p[2] as f64 + 0.5]);
            for i in 0..3 {
                assert!(
                    (w0[i] - w1[i]).abs() < 1e-12,
                    "voxel {p:?} drifted on set_block: {w0:?} -> {w1:?}"
                );
            }
        }

        // Removing the keel restores the original COM — and again the
        // remaining voxels must not move in world space.
        let mid: Vec<[f64; 3]> = sub
            .blocks
            .keys()
            .map(|p| sub.local_to_world([p[0] as f64 + 0.5, p[1] as f64 + 0.5, p[2] as f64 + 0.5]))
            .collect();
        assert!(sub.remove_block([4, 0, 0]));
        for (p, &w0) in sub.blocks.keys().zip(&mid[..2]) {
            let w1 = sub.local_to_world([p[0] as f64 + 0.5, p[1] as f64 + 0.5, p[2] as f64 + 0.5]);
            for i in 0..3 {
                assert!((w0[i] - w1[i]).abs() < 1e-12);
            }
        }
    }

    // -- P0-C: substepped contact solver --------------------------------

    #[test]
    fn airship_lands_and_rests_on_flat_ground() {
        let mut sub = single_ship(&[([0, 0, 0], "minecraft:stone")]);
        sub.pos = [10.0, 66.0, 10.0];
        sub.vel = [0.0, -4.0, 0.0];
        let terrain = |cell: [i32; 3]| cell[1] < 64;
        let mut solver = BuiltinContactSolver::default();
        let mut others: [&mut RigidBodySubWorld; 0] = [];
        let mut last_pos = sub.pos;
        let mut settled_tick = None;
        for tick in 0..400 {
            sub.step_env_with_contacts(
                0.05,
                9.81,
                1.0,
                0.0,
                0.0,
                &mut solver,
                &terrain,
                &mut others,
            );
            if !sub.awake && settled_tick.is_none() {
                settled_tick = Some(tick);
                last_pos = sub.pos;
            }
        }
        assert!(settled_tick.is_some(), "the hull never came to rest");
        // Lowest voxel corner rests at the floor surface y=64:
        // pos.y ≈ com.y + 64 → 0.5 + 64 = 64.5.
        assert!(
            (sub.pos[1] - 64.5).abs() < 0.15,
            "resting height {} (want ≈64.5)",
            sub.pos[1]
        );
        // No tunneling: bottom corner must never have sunk below the floor.
        let corner = sub.local_to_world([0.0, 0.0, 0.0]);
        assert!(corner[1] >= 63.999, "sank to {}", corner[1]);
        // Rest is stable: once asleep the pose is bit-frozen.
        for _ in 0..20 {
            sub.step_env_with_contacts(
                0.05,
                9.81,
                1.0,
                0.0,
                0.0,
                &mut solver,
                &terrain,
                &mut others,
            );
        }
        assert_eq!(sub.pos, last_pos, "a sleeping ship does zero work");
        assert!(norm3(sub.vel) < 0.05 && norm3(sub.omega) < 0.05);
    }

    #[test]
    fn airship_bounces_off_a_wall() {
        let mut sub = single_ship(&[([0, 0, 0], "minecraft:stone")]);
        sub.pos = [8.0, 100.0, 10.0];
        sub.vel = [5.0, 0.0, 0.0];
        // Wall: every cell with x >= 10 solid.
        let terrain = |cell: [i32; 3]| cell[0] >= 10;
        let mut solver = BuiltinContactSolver::default();
        let mut others: [&mut RigidBodySubWorld; 0] = [];
        for _ in 0..40 {
            sub.step_env_with_contacts(
                0.05,
                0.0,
                0.0,
                0.0,
                0.0,
                &mut solver,
                &terrain,
                &mut others,
            );
        }
        assert!(
            sub.vel[0] < 0.0,
            "the wall must reflect the ship, vel.x = {}",
            sub.vel[0]
        );
        // Never tunneled: max-x corner stays at-or-below x=10.
        let corner = sub.local_to_world([1.0, 0.0, 0.0]);
        assert!(corner[0] <= 10.001, "tunneled to {}", corner[0]);
    }

    #[test]
    fn ship_to_ship_collision_conserves_linear_momentum() {
        let mut a = single_ship(&[([0, 0, 0], "minecraft:stone")]);
        let mut b = single_ship(&[([0, 0, 0], "minecraft:stone")]);
        a.pos = [0.0, 100.0, 0.0];
        a.vel = [6.0, 0.0, 0.0];
        b.pos = [3.0, 100.0, 0.0];
        b.vel = [-6.0, 0.0, 0.0];
        let momentum = |a: &RigidBodySubWorld, b: &RigidBodySubWorld| -> [f64; 3] {
            [
                a.mass * a.vel[0] + b.mass * b.vel[0],
                a.mass * a.vel[1] + b.mass * b.vel[1],
                a.mass * a.vel[2] + b.mass * b.vel[2],
            ]
        };
        let p0 = momentum(&a, &b);
        let mut solver = BuiltinContactSolver::default();
        for _ in 0..20 {
            {
                let mut others = [&mut b];
                a.step_env_with_contacts(
                    0.05, 0.0, 0.0, 0.0, 0.0, &mut solver, &|_| false, &mut others,
                );
            }
            let p = momentum(&a, &b);
            for i in 0..3 {
                assert!(
                    (p[i] - p0[i]).abs() < 1e-6,
                    "momentum not conserved on axis {i}: {p:?} vs {p0:?}"
                );
            }
        }
        // Head-on equal-mass collision at e=0.15: the ships must actually
        // have exchanged velocity — A should be moving back or at least
        // slowed drastically.
        assert!(a.vel[0] < 3.0, "no collision response: vel={:?}", a.vel);
    }

    // -- Common foundation: WorldView on the sub-world ------------------

    #[test]
    fn world_view_reads_writes_and_transforms_like_local_to_world() {
        let mut sub = single_ship(&[([0, 0, 0], "create:shaft")]);
        sub.pos = [10.0, 100.0, 10.0];
        // Reads go through the embedded registry's canonical codec.
        let state = sub
            .factory
            .registry()
            .decode_canonical("create:shaft")
            .expect("shaft decodes");
        assert_eq!(WorldView::block_state_at(&sub, [0, 0, 0]), Some(state));
        assert_eq!(WorldView::block_state_at(&sub, [9, 9, 9]), None);
        // Writes route through set_block (canonical re-encode).
        let casing = sub
            .factory
            .registry()
            .decode_canonical("create:andesite_casing")
            .expect("casing decodes");
        assert!(sub.set_block_state([1, 0, 0], casing));
        assert!(sub.blocks.contains_key(&[1, 0, 0]));
        assert!(!sub.set_block_state([2, 0, 0], u32::MAX), "bad id refused");
        // The transform is exactly the ship's rigid map: cell centers map
        // through pos + R·(p - com) — identical to local_to_world.
        let t = WorldView::world_transform(&sub);
        for local in [[0.0, 0.0, 0.0], [0.5, 0.5, 0.5], [3.0, -2.0, 7.0]] {
            let via_view = t.local_to_world(local).unwrap();
            let via_ship = sub.local_to_world(local);
            for i in 0..3 {
                assert!((via_view[i] - via_ship[i]).abs() < 1e-12);
            }
        }
    }




}
