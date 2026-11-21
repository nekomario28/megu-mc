// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Create Contraption assembly, rigid motion, passenger carry, actor collision,
//! grid-snapped disassembly, and name-based Save v2 snapshot codec backed by
//! [`mod_offload::contraption::Contraption`].

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use mod_offload::contraption::{Aabb, BlockCluster, ClusterError, Contraption, Pose};

use crate::blockstates::CreateBlockRegistry;
use crate::factory::DimBlockPos;

/// Maximum block count allowed in one assembled contraption (safety bound).
pub const DEFAULT_MAX_CONTRAPTION_BLOCKS: usize = 2048;

/// Kind of Create contraption motion driver.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ContraptionKind {
    /// Mechanical Bearing rotating about world Y (yaw degrees/tick = RPM * 0.3).
    Bearing { angular_deg_per_tick: f32 },
    /// Mechanical Piston or Gantry translating along a linear velocity vector.
    Linear { velocity_per_tick: [f64; 3] },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContraptionError {
    EmptyAssembly,
    TooLarge { count: usize, limit: usize },
    Cluster(ClusterError),
    InvalidPose,
    /// A world coordinate or local offset fell outside `i32` range.
    CoordinateOverflow,
    CorruptSnapshot(String),
}

/// Active assembled contraption with rigid pose, previous pose for passenger
/// carry delta, and sparse local block cluster.
pub struct LiveContraption {
    pub id: u32,
    pub dim: i32,
    pub anchor: [i32; 3],
    pub kind: ContraptionKind,
    pub yaw_deg: f32,
    pub pitch_deg: f32,
    pub translation: [f64; 3],
    previous_pose: Pose,
    inner: Contraption,
}

/// World-space block edit produced on assembly (removals) or disassembly
/// (placements snapped to integer world grid).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorldBlockEdit {
    pub pos: DimBlockPos,
    /// `None` removes the block (sets air); `Some(state_id)` places it.
    pub state_id: Option<u32>,
}

/// Name-based Save v2 snapshot for a live contraption (survives runtime state
/// ID reordering across mod/addon updates).
#[derive(Clone, Debug, PartialEq)]
pub struct ContraptionSnapshot {
    pub id: u32,
    pub dim: i32,
    pub anchor: [i32; 3],
    pub kind: ContraptionKind,
    pub yaw_deg: f32,
    pub pitch_deg: f32,
    pub translation: [f64; 3],
    /// Local `(x, y, z)` -> canonical blockstate string (or `"vanilla:<id>"`).
    pub cells: Vec<([i32; 3], String)>,
}

impl LiveContraption {
    /// Assemble a contraption by 6-connected flood-fill from `seed` over
    /// `attached_blocks` (representing superglued / chassis-bound world blocks).
    /// Returns the assembled [`LiveContraption`] and the list of world-block
    /// removals (`state_id = None`) the server should commit via CAS.
    pub fn assemble_from_world(
        id: u32,
        dim: i32,
        anchor: [i32; 3],
        seed: [i32; 3],
        attached_blocks: &BTreeMap<[i32; 3], u32>,
        kind: ContraptionKind,
        max_blocks: usize,
    ) -> Result<(Self, Vec<WorldBlockEdit>), ContraptionError> {
        if !attached_blocks.contains_key(&seed) {
            return Err(ContraptionError::EmptyAssembly);
        }

        let mut visited = BTreeSet::new();
        let mut queue = VecDeque::new();
        visited.insert(seed);
        queue.push_back(seed);

        let mut local_cells = Vec::new();
        let mut world_removals = Vec::new();

        while let Some(pos) = queue.pop_front() {
            if local_cells.len() >= max_blocks {
                return Err(ContraptionError::TooLarge {
                    count: local_cells.len() + 1,
                    limit: max_blocks,
                });
            }
            let state_id = attached_blocks[&pos];
            let mut local = [0_i32; 3];
            for axis in 0..3 {
                local[axis] = pos[axis]
                    .checked_sub(anchor[axis])
                    .ok_or(ContraptionError::CoordinateOverflow)?;
            }
            local_cells.push((local, state_id));
            world_removals.push(WorldBlockEdit {
                pos: DimBlockPos::new(dim, pos[0], pos[1], pos[2]),
                state_id: None,
            });

            const DIRS: [[i32; 3]; 6] = [
                [1, 0, 0],
                [-1, 0, 0],
                [0, 1, 0],
                [0, -1, 0],
                [0, 0, 1],
                [0, 0, -1],
            ];
            for d in DIRS {
                // A neighbour past the i32 edge cannot be a map key; skip it
                // instead of panicking on `pos + d` overflow.
                let (Some(nx), Some(ny), Some(nz)) = (
                    pos[0].checked_add(d[0]),
                    pos[1].checked_add(d[1]),
                    pos[2].checked_add(d[2]),
                ) else {
                    continue;
                };
                let n = [nx, ny, nz];
                if attached_blocks.contains_key(&n) && visited.insert(n) {
                    queue.push_back(n);
                }
            }
        }

        let cluster = BlockCluster::new(local_cells).map_err(ContraptionError::Cluster)?;
        let translation = [
            f64::from(anchor[0]),
            f64::from(anchor[1]),
            f64::from(anchor[2]),
        ];
        let pose =
            Pose::from_yaw_pitch(translation, 0.0, 0.0).ok_or(ContraptionError::InvalidPose)?;
        let inner = Contraption::new(cluster, pose);
        Ok((
            Self {
                id,
                dim,
                anchor,
                kind,
                yaw_deg: 0.0,
                pitch_deg: 0.0,
                translation,
                previous_pose: pose,
                inner,
            },
            world_removals,
        ))
    }

    pub fn block_count(&self) -> usize {
        self.inner.cluster().blocks().len()
    }

    pub fn world_aabb(&self) -> Option<Aabb> {
        self.inner.world_aabb()
    }

    pub fn pose(&self) -> Pose {
        self.inner.pose()
    }

    /// Advance the contraption by one game tick according to its motion driver.
    /// Returns `true` if the rigid pose changed.
    pub fn step_tick(&mut self) -> Result<bool, ContraptionError> {
        self.previous_pose = self.inner.pose();
        match self.kind {
            ContraptionKind::Bearing {
                angular_deg_per_tick,
            } => {
                self.yaw_deg = (self.yaw_deg + angular_deg_per_tick).rem_euclid(360.0);
            }
            ContraptionKind::Linear { velocity_per_tick } => {
                for (dst, vel) in self.translation.iter_mut().zip(velocity_per_tick) {
                    *dst += vel;
                }
            }
        }
        let next = Pose::from_yaw_pitch(self.translation, self.yaw_deg, self.pitch_deg)
            .ok_or(ContraptionError::InvalidPose)?;
        Ok(self.inner.set_pose(next).is_some())
    }

    /// Exact oriented-voxel collision check against an actor world AABB.
    pub fn collides(&self, actor_bounds: Aabb) -> bool {
        self.inner.intersects(actor_bounds)
    }

    /// Compute the rigid displacement of a carried passenger anchor from the
    /// previous tick's pose to the current tick's pose.
    pub fn passenger_delta(&self, previous_anchor: [f64; 3]) -> Option<[f64; 3]> {
        self.inner
            .pose()
            .carried_delta(self.previous_pose, previous_anchor)
    }

    /// Disassemble the contraption by snapping its current rigid rotation to
    /// the nearest 90° increment and translation to integer world coordinates.
    pub fn disassemble_to_world(&self) -> Vec<WorldBlockEdit> {
        let quarter_turns = (self.yaw_deg / 90.0).round() as i32;
        let rot = quarter_turns.rem_euclid(4);
        let base_x = self.translation[0].round() as i32;
        let base_y = self.translation[1].round() as i32;
        let base_z = self.translation[2].round() as i32;

        self.inner
            .cluster()
            .blocks()
            .map(|([lx, ly, lz], state_id)| {
                // Right-handed rotation about +Y in 90-degree increments matches
                // Pose::from_yaw_pitch's matrix [[cos, 0, sin], [0, 1, 0], [-sin, 0, cos]].
                let (rx, rz) = match rot {
                    0 => (lx, lz),
                    1 => (lz, -lx),
                    2 => (-lx, -lz),
                    3 => (-lz, lx),
                    _ => unreachable!(),
                };
                WorldBlockEdit {
                    // `base_*` saturates at the i32 edge after `as i32` on a
                    // huge translation; clamp the final sum too so a long
                    // Linear run cannot panic on `base + r` overflow.
                    pos: DimBlockPos::new(
                        self.dim,
                        base_x.saturating_add(rx),
                        base_y.saturating_add(ly),
                        base_z.saturating_add(rz),
                    ),
                    state_id: Some(state_id),
                }
            })
            .collect()
    }

    /// Capture a Save v2 name-based snapshot using `registry` for Create states
    /// and `"vanilla:<id>"` for vanilla states.
    pub fn snapshot(&self, registry: &CreateBlockRegistry) -> ContraptionSnapshot {
        let cells = self
            .inner
            .cluster()
            .blocks()
            .map(|(local, state_id)| {
                let encoded = registry
                    .encode_canonical(state_id)
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("vanilla:{state_id}"));
                (local, encoded)
            })
            .collect();
        ContraptionSnapshot {
            id: self.id,
            dim: self.dim,
            anchor: self.anchor,
            kind: self.kind,
            yaw_deg: self.yaw_deg,
            pitch_deg: self.pitch_deg,
            translation: self.translation,
            cells,
        }
    }

    /// Restore a contraption from a Save v2 snapshot against a (possibly
    /// reordered) [`CreateBlockRegistry`].
    pub fn restore(
        snapshot: &ContraptionSnapshot,
        registry: &CreateBlockRegistry,
    ) -> Result<Self, ContraptionError> {
        let mut local_cells = Vec::with_capacity(snapshot.cells.len());
        for (local, name) in &snapshot.cells {
            let state_id = if let Some(raw) = name.strip_prefix("vanilla:") {
                raw.parse::<u32>()
                    .map_err(|_| ContraptionError::CorruptSnapshot(name.clone()))?
            } else {
                registry
                    .decode_canonical(name)
                    .ok_or_else(|| ContraptionError::CorruptSnapshot(name.clone()))?
            };
            local_cells.push((*local, state_id));
        }
        let cluster = BlockCluster::new(local_cells).map_err(ContraptionError::Cluster)?;
        let pose = Pose::from_yaw_pitch(snapshot.translation, snapshot.yaw_deg, snapshot.pitch_deg)
            .ok_or(ContraptionError::InvalidPose)?;
        let inner = Contraption::new(cluster, pose);
        Ok(Self {
            id: snapshot.id,
            dim: snapshot.dim,
            anchor: snapshot.anchor,
            kind: snapshot.kind,
            yaw_deg: snapshot.yaw_deg,
            pitch_deg: snapshot.pitch_deg,
            translation: snapshot.translation,
            previous_pose: pose,
            inner,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blockstates::{core_create_registry, DEFAULT_BASE_STATE_ID};

    #[test]
    fn bearing_contraption_assemble_rotate_carry_passenger_and_disassemble() {
        let reg = core_create_registry(DEFAULT_BASE_STATE_ID);
        let shaft = reg.default_state("create:shaft").unwrap();
        let mut world = BTreeMap::new();
        // 3-block horizontal arm at anchor (10, 64, 10) extending along +X.
        world.insert([10, 64, 10], shaft);
        world.insert([11, 64, 10], shaft);
        world.insert([12, 64, 10], shaft);
        // Unattached distant block must not be pulled in.
        world.insert([20, 64, 20], 1);

        let (mut contraption, removals) = LiveContraption::assemble_from_world(
            7,
            0,
            [10, 64, 10],
            [10, 64, 10],
            &world,
            ContraptionKind::Bearing {
                angular_deg_per_tick: 15.0,
            },
            DEFAULT_MAX_CONTRAPTION_BLOCKS,
        )
        .unwrap();
        assert_eq!(contraption.block_count(), 3);
        assert_eq!(removals.len(), 3);

        // Passenger standing on block (12, 64, 10) center = (12.5, 65.0, 10.5).
        let mut passenger = [12.5, 65.0, 10.5];
        for _ in 0..6 {
            assert!(contraption.step_tick().unwrap());
            let delta = contraption.passenger_delta(passenger).unwrap();
            for i in 0..3 {
                passenger[i] += delta[i];
            }
        }
        // After 6 * 15 = 90 degrees about Y around (10, 64, 10),
        // local (+2.5, +1.0, +0.5) maps to (+0.5, +1.0, -2.5) -> world (10.5, 65.0, 7.5).
        assert!((passenger[0] - 10.5).abs() < 1e-6);
        assert!((passenger[1] - 65.0).abs() < 1e-6);
        assert!((passenger[2] - 7.5).abs() < 1e-6);

        // Collision hits rotated arm at (10, 64, 8), misses old (12, 64, 10).
        let hit_box = Aabb::new([10.2, 64.2, 8.2], [10.8, 64.8, 8.8]).unwrap();
        let miss_box = Aabb::new([12.2, 64.2, 10.2], [12.8, 64.8, 10.8]).unwrap();
        assert!(contraption.collides(hit_box));
        assert!(!contraption.collides(miss_box));

        // Disassemble snaps cleanly to (10,64,10), (10,64,9), (10,64,8).
        let placed = contraption.disassemble_to_world();
        let coords: BTreeSet<(i32, i32, i32)> =
            placed.iter().map(|e| (e.pos.x, e.pos.y, e.pos.z)).collect();
        assert!(coords.contains(&(10, 64, 10)));
        assert!(coords.contains(&(10, 64, 9)));
        assert!(coords.contains(&(10, 64, 8)));

        // Save v2 snapshot round-trip preserves pose and blocks cleanly.
        let snap = contraption.snapshot(&reg);
        let restored = LiveContraption::restore(&snap, &reg).unwrap();
        assert_eq!(restored.block_count(), 3);
        assert!((restored.yaw_deg - 90.0).abs() < 1e-6);
    }

    #[test]
    fn assembly_at_i32_edges_fails_closed_or_skips() {
        let reg = core_create_registry(DEFAULT_BASE_STATE_ID);
        let shaft = reg.default_state("create:shaft").unwrap();

        // Seed at the +X edge: the +X neighbour overflows i32 and must be
        // skipped, not panic; local coords still resolve.
        let mut world = BTreeMap::new();
        world.insert([i32::MAX, 64, 10], shaft);
        world.insert([i32::MAX - 1, 64, 10], shaft);
        let (c, _) = LiveContraption::assemble_from_world(
            1,
            0,
            [i32::MAX, 64, 10],
            [i32::MAX, 64, 10],
            &world,
            ContraptionKind::Linear {
                velocity_per_tick: [0.0; 3],
            },
            DEFAULT_MAX_CONTRAPTION_BLOCKS,
        )
        .unwrap();
        assert_eq!(c.block_count(), 2);

        // Anchor on the opposite edge makes `pos - anchor` overflow i32;
        // the assembly must report CoordinateOverflow, not panic.
        let mut world = BTreeMap::new();
        world.insert([i32::MAX, 64, 10], shaft);
        let err = match LiveContraption::assemble_from_world(
            2,
            0,
            [i32::MIN, 64, 10],
            [i32::MAX, 64, 10],
            &world,
            ContraptionKind::Linear {
                velocity_per_tick: [0.0; 3],
            },
            DEFAULT_MAX_CONTRAPTION_BLOCKS,
        ) {
            Err(e) => e,
            Ok(_) => panic!("expected CoordinateOverflow"),
        };
        assert_eq!(err, ContraptionError::CoordinateOverflow);
    }

    #[test]
    fn disassemble_saturates_after_huge_translation() {
        let reg = core_create_registry(DEFAULT_BASE_STATE_ID);
        let shaft = reg.default_state("create:shaft").unwrap();
        let mut world = BTreeMap::new();
        world.insert([10, 64, 10], shaft);
        world.insert([11, 64, 10], shaft);
        let (mut c, _) = LiveContraption::assemble_from_world(
            3,
            0,
            [10, 64, 10],
            [10, 64, 10],
            &world,
            ContraptionKind::Linear {
                velocity_per_tick: [1.0e300; 3],
            },
            DEFAULT_MAX_CONTRAPTION_BLOCKS,
        )
        .unwrap();
        // Drive the translation far past the i32 range, then disassemble:
        // `round() as i32` saturates and the final `base + r` must not panic.
        c.translation = [1.0e300, -1.0e300, 1.0e300];
        let edits = c.disassemble_to_world();
        assert_eq!(edits.len(), 2);
        // Saturated to the i32 edges rather than wrapping or panicking.
        assert!(edits.iter().all(|e| e.pos.x == i32::MAX));
        assert!(edits.iter().all(|e| e.pos.y == i32::MIN));
    }
}
