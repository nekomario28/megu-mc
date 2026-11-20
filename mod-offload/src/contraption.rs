// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! A sparse paletted cluster shares one rigid pose and one cached world AABB.
//! Collision queries refine that bound against occupied unit cells.

use std::cmp::Ordering;
use std::collections::BTreeMap;

pub type BlockPos = [i32; 3];
pub type Vec3 = [f64; 3];

const AXES: [Vec3; 3] = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Aabb {
    min: Vec3,
    max: Vec3,
}

impl Aabb {
    pub fn new(min: Vec3, max: Vec3) -> Option<Self> {
        (0..3)
            .all(|i| min[i].is_finite() && max[i].is_finite() && min[i] <= max[i])
            .then_some(Self { min, max })
    }

    pub fn min(self) -> Vec3 {
        self.min
    }

    pub fn max(self) -> Vec3 {
        self.max
    }

    /// Closed intervals. Touching faces count as contact.
    pub fn intersects(self, other: Self) -> bool {
        (0..3).all(|i| self.min[i] <= other.max[i] && other.min[i] <= self.max[i])
    }

    fn union(self, other: Self) -> Self {
        Self {
            min: std::array::from_fn(|i| self.min[i].min(other.min[i])),
            max: std::array::from_fn(|i| self.max[i].max(other.max[i])),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pose {
    rotation: [Vec3; 3],
    translation: Vec3,
}

impl Default for Pose {
    fn default() -> Self {
        Self {
            rotation: AXES,
            translation: [0.0; 3],
        }
    }
}

impl Pose {
    /// Rotate about the local X axis (pitch), then the world Y axis (yaw).
    /// Both angles are right-handed degrees around the local-origin pivot.
    pub fn from_yaw_pitch(translation: Vec3, yaw: f32, pitch: f32) -> Option<Self> {
        let (sy, cy) = f64::from(yaw).to_radians().sin_cos();
        let (sp, cp) = f64::from(pitch).to_radians().sin_cos();
        Self::new(
            [
                [cy, sy * sp, sy * cp],
                [0.0, cp, -sp],
                [-sy, cy * sp, cy * cp],
            ],
            translation,
        )
    }

    /// Construct a proper rigid transform from a row-major rotation matrix
    /// and a world-space translation. The pivot is the local origin.
    pub fn new(rotation: [Vec3; 3], translation: Vec3) -> Option<Self> {
        if !translation.iter().all(|value| value.is_finite()) {
            return None;
        }
        for (i, &row) in rotation.iter().enumerate() {
            for (j, &other) in rotation.iter().enumerate() {
                let expected = f64::from(i == j);
                if (dot(row, other) - expected).abs().partial_cmp(&1e-9) != Some(Ordering::Less) {
                    return None;
                }
            }
        }
        let determinant = dot(rotation[0], cross(rotation[1], rotation[2]));
        (determinant > 0.0).then_some(Self {
            rotation,
            translation,
        })
    }

    pub fn rotation(self) -> [Vec3; 3] {
        self.rotation
    }

    pub fn translation(self) -> Vec3 {
        self.translation
    }

    /// World-space displacement of a point attached to the previous pose.
    pub fn carried_delta(self, previous: Self, anchor: Vec3) -> Option<Vec3> {
        if !anchor.iter().all(|value| value.is_finite()) {
            return None;
        }
        if self.rotation == previous.rotation {
            let delta = std::array::from_fn(|i| self.translation[i] - previous.translation[i]);
            return delta.iter().all(|value| value.is_finite()).then_some(delta);
        }
        let relative = std::array::from_fn(|i| anchor[i] - previous.translation[i]);
        let local = std::array::from_fn(|i| dot(previous.rotation.map(|row| row[i]), relative));
        let delta = std::array::from_fn(|i| {
            let rotation_change =
                std::array::from_fn(|j| self.rotation[i][j] - previous.rotation[i][j]);
            self.translation[i] - previous.translation[i] + dot(rotation_change, local)
        });
        delta.iter().all(|value| value.is_finite()).then_some(delta)
    }

    fn cell_center(self, position: BlockPos) -> Vec3 {
        let local = position.map(|value| f64::from(value) + 0.5);
        std::array::from_fn(|i| dot(self.rotation[i], local) + self.translation[i])
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClusterError {
    DuplicatePosition(BlockPos),
    PaletteFull,
}

#[derive(Debug, Default)]
pub struct BlockCluster {
    palette: Vec<u32>,
    cells: Vec<(BlockPos, u32)>,
}

impl BlockCluster {
    /// Each supplied position is occupied. An omitted position is empty,
    /// leaving every u32 state (including zero) available to the adapter.
    pub fn new(mut blocks: Vec<(BlockPos, u32)>) -> Result<Self, ClusterError> {
        blocks.sort_unstable_by_key(|&(position, _)| position);
        Self::from_sorted(blocks)
    }

    fn from_sorted(
        blocks: impl IntoIterator<Item = (BlockPos, u32)>,
    ) -> Result<Self, ClusterError> {
        let mut cluster = Self::default();
        let mut indices = BTreeMap::new();
        for (position, state) in blocks {
            if cluster
                .cells
                .last()
                .is_some_and(|&(previous, _)| previous == position)
            {
                return Err(ClusterError::DuplicatePosition(position));
            }
            let index = match indices.entry(state) {
                std::collections::btree_map::Entry::Occupied(entry) => *entry.get(),
                std::collections::btree_map::Entry::Vacant(entry) => {
                    let index = u32::try_from(cluster.palette.len())
                        .map_err(|_| ClusterError::PaletteFull)?;
                    cluster.palette.push(state);
                    *entry.insert(index)
                }
            };
            cluster.cells.push((position, index));
        }
        Ok(cluster)
    }

    pub fn palette(&self) -> &[u32] {
        &self.palette
    }

    pub fn blocks(&self) -> impl ExactSizeIterator<Item = (BlockPos, u32)> + '_ {
        self.cells
            .iter()
            .map(|&(position, index)| (position, self.palette[index as usize]))
    }

    pub fn state_at(&self, position: BlockPos) -> Option<u32> {
        let index = self
            .cells
            .binary_search_by_key(&position, |&(cell, _)| cell)
            .ok()?;
        Some(self.palette[self.cells[index].1 as usize])
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockDelta {
    pub position: BlockPos,
    pub state: Option<u32>,
}

pub struct Contraption {
    cluster: BlockCluster,
    pose: Pose,
    bounds: Option<Aabb>,
}

impl Contraption {
    pub fn new(cluster: BlockCluster, pose: Pose) -> Self {
        let bounds = cluster_bounds(&cluster, pose);
        Self {
            cluster,
            pose,
            bounds,
        }
    }

    pub fn cluster(&self) -> &BlockCluster {
        &self.cluster
    }

    pub fn pose(&self) -> Pose {
        self.pose
    }

    pub fn world_aabb(&self) -> Option<Aabb> {
        self.bounds
    }

    /// Emit a single pose delta on motion. Local cell states keep their positions
    /// and are shared across all poses.
    pub fn set_pose(&mut self, pose: Pose) -> Option<Pose> {
        if pose == self.pose {
            return None;
        }
        self.pose = pose;
        self.bounds = cluster_bounds(&self.cluster, pose);
        Some(pose)
    }

    /// Merge sorted, unique edits into the local palette in one pass.
    pub(crate) fn apply_deltas(&mut self, edits: &[BlockDelta]) -> Result<(), ClusterError> {
        let mut blocks = self.cluster.blocks().peekable();
        let mut edits = edits.iter().peekable();
        let merged = std::iter::from_fn(move || loop {
            let Some(edit) = edits.peek() else {
                return blocks.next();
            };
            if let Some(&(pos, _)) = blocks.peek() {
                match pos.cmp(&edit.position) {
                    Ordering::Less => return blocks.next(),
                    Ordering::Equal => {
                        blocks.next();
                    }
                    Ordering::Greater => {}
                }
            }
            let edit = edits.next().unwrap();
            if let Some(state) = edit.state {
                return Some((edit.position, state));
            }
        });
        let next = BlockCluster::from_sorted(merged)?;
        self.bounds = cluster_bounds(&next, self.pose);
        self.cluster = next;
        Ok(())
    }

    /// Diff by local position and state so palette reindexing stays internal.
    pub fn replace_cluster(&mut self, next: BlockCluster) -> Vec<BlockDelta> {
        let mut deltas = Vec::new();
        let mut old = self.cluster.blocks().peekable();
        let mut new = next.blocks().peekable();
        while let (Some(&(a, before)), Some(&(b, after))) = (old.peek(), new.peek()) {
            match a.cmp(&b) {
                Ordering::Less => {
                    deltas.push(BlockDelta {
                        position: a,
                        state: None,
                    });
                    old.next();
                }
                Ordering::Equal => {
                    if before != after {
                        deltas.push(BlockDelta {
                            position: b,
                            state: Some(after),
                        });
                    }
                    old.next();
                    new.next();
                }
                Ordering::Greater => {
                    deltas.push(BlockDelta {
                        position: b,
                        state: Some(after),
                    });
                    new.next();
                }
            }
        }
        deltas.extend(old.map(|(position, _)| BlockDelta {
            position,
            state: None,
        }));
        deltas.extend(new.map(|(position, state)| BlockDelta {
            position,
            state: Some(state),
        }));
        if !deltas.is_empty() {
            self.bounds = cluster_bounds(&next, self.pose);
            self.cluster = next;
        }
        deltas
    }

    /// Broadphase against the cluster AABB, then test the world AABB against
    /// occupied oriented unit boxes using the separating axis theorem.
    pub fn intersects(&self, query: Aabb) -> bool {
        if !self.bounds.is_some_and(|bounds| bounds.intersects(query)) {
            return false;
        }
        let center: Vec3 = std::array::from_fn(|i| query.min[i] * 0.5 + query.max[i] * 0.5);
        let half = std::array::from_fn(|i| query.max[i] * 0.5 - query.min[i] * 0.5);
        let local_axes: [Vec3; 3] = std::array::from_fn(|i| self.pose.rotation.map(|row| row[i]));
        self.cluster.cells.iter().any(|&(position, _)| {
            let cell = self.pose.cell_center(position);
            let displacement = std::array::from_fn(|i| cell[i] - center[i]);
            let separated = |axis: Vec3| {
                let world_radius = dot(half, axis.map(f64::abs));
                let cell_radius = local_axes
                    .iter()
                    .map(|&local| dot(local, axis).abs())
                    .sum::<f64>()
                    * 0.5;
                dot(displacement, axis).abs() > world_radius + cell_radius
            };
            !AXES.iter().chain(&local_axes).copied().any(separated)
                && !AXES.iter().any(|&world| {
                    local_axes
                        .iter()
                        .any(|&local| separated(cross(world, local)))
                })
        })
    }
}

fn cluster_bounds(cluster: &BlockCluster, pose: Pose) -> Option<Aabb> {
    let half = pose
        .rotation
        .map(|row| row.iter().map(|v| v.abs()).sum::<f64>() * 0.5);
    cluster
        .cells
        .iter()
        .map(|&(position, _)| {
            let center = pose.cell_center(position);
            Aabb {
                min: std::array::from_fn(|i| center[i] - half[i]),
                max: std::array::from_fn(|i| center[i] + half[i]),
            }
        })
        .reduce(Aabb::union)
}

fn dot(a: Vec3, b: Vec3) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross(a: Vec3, b: Vec3) -> Vec3 {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moving_cluster_preserves_palette_and_rejects_gaps() {
        let cluster =
            BlockCluster::new(vec![([0, 0, 0], 0), ([3, 0, 0], u32::MAX), ([3, 0, 1], 0)]).unwrap();
        let mut train = Contraption::new(cluster, Pose::default());
        assert_eq!(train.cluster().palette(), &[0, u32::MAX]);
        assert!(train
            .world_aabb()
            .unwrap()
            .intersects(Aabb::new([1.25, 0.0, 0.0], [1.75, 1.0, 1.0]).unwrap()));
        assert!(!train.intersects(Aabb::new([1.25, 0.0, 0.0], [1.75, 1.0, 1.0]).unwrap()));
        let pose = Pose::new(
            [[0.0, -1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]],
            [10.0, 2.0, -4.0],
        )
        .unwrap();
        assert_eq!(train.set_pose(pose), Some(pose));
        assert_eq!(train.set_pose(pose), None);
        assert_eq!(
            train.world_aabb(),
            Aabb::new([9.0, 2.0, -4.0], [10.0, 6.0, -2.0])
        );
        assert!(train.intersects(Aabb::new([8.0, 5.5, -3.75], [9.0, 5.9, -3.25]).unwrap()));
        assert!(!train.intersects(Aabb::new([0.0; 3], [1.0; 3]).unwrap()));
        assert_eq!(train.cluster().state_at([0, 0, 0]), Some(0));
        assert_eq!(train.cluster().state_at([3, 0, 0]), Some(u32::MAX));
    }

    #[test]
    fn rotation_refines_transformed_cell_bounds() {
        let c = std::f64::consts::FRAC_1_SQRT_2;
        let pose = Pose::new([[c, -c, 0.0], [c, c, 0.0], [0.0, 0.0, 1.0]], [0.0; 3]).unwrap();
        let cluster = BlockCluster::new(vec![([0; 3], 5)]).unwrap();
        let train = Contraption::new(cluster, pose);
        let corner = Aabb::new([0.6, 0.0, 0.2], [0.7, 0.1, 0.6]).unwrap();
        assert!(train.world_aabb().unwrap().intersects(corner));
        assert!(!train.intersects(corner));
        assert!(train.intersects(Aabb::new([-0.1, 0.6, 0.2], [0.1, 0.8, 0.6]).unwrap()));
    }

    #[test]
    fn sparse_bounds_follow_occupied_positions_after_rotation() {
        let c = std::f64::consts::FRAC_1_SQRT_2;
        let pose = Pose::new([[c, -c, 0.0], [c, c, 0.0], [0.0, 0.0, 1.0]], [0.0; 3]).unwrap();
        let cluster = BlockCluster::new(vec![([0; 3], 1), ([10, 10, 0], 2)]).unwrap();
        let bounds = Contraption::new(cluster, pose).world_aabb().unwrap();
        assert!((bounds.min()[0] + c).abs() < 1e-12);
        assert!((bounds.max()[0] - c).abs() < 1e-12);
    }

    #[test]
    fn cluster_delta_is_keyed_by_position_and_suppresses_noops() {
        let first =
            BlockCluster::new(vec![([-2, 0, 0], 9), ([0, 0, 0], 5), ([1, 0, 0], 4)]).unwrap();
        let mut train = Contraption::new(first, Pose::default());
        let next =
            || BlockCluster::new(vec![([0, 0, 0], 5), ([1, 0, 0], 2), ([2, 0, 0], 9)]).unwrap();
        assert_eq!(
            train.replace_cluster(next()),
            [
                BlockDelta {
                    position: [-2, 0, 0],
                    state: None
                },
                BlockDelta {
                    position: [1, 0, 0],
                    state: Some(2)
                },
                BlockDelta {
                    position: [2, 0, 0],
                    state: Some(9)
                },
            ]
        );
        assert!(train.replace_cluster(next()).is_empty());
        assert_eq!(train.world_aabb(), Aabb::new([0.0; 3], [3.0, 1.0, 1.0]));
    }

    #[test]
    fn invalid_input_is_rejected_and_empty_clusters_have_no_collider() {
        assert!(matches!(
            BlockCluster::new(vec![([0; 3], 0), ([0; 3], 1)]),
            Err(ClusterError::DuplicatePosition(_))
        ));
        assert!(Pose::new([[-1.0, 0.0, 0.0], AXES[1], AXES[2]], [0.0; 3]).is_none());
        assert!(Pose::new([[f64::NAN, 0.0, 0.0], AXES[1], AXES[2]], [0.0; 3]).is_none());
        assert!(Aabb::new([0.0; 3], [-1.0; 3]).is_none());
        let empty = Contraption::new(BlockCluster::default(), Pose::default());
        assert_eq!(empty.world_aabb(), None);
    }
}
