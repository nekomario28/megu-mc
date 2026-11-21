// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

use std::collections::{BTreeSet, HashMap};
use std::ops::Range;

use crate::cull::box_in_frustum;
use crate::packed::{depth_key, is_translucent};
use crate::{
    edge_corner_index, mesh_section, Block, Direction, Light, MissingNeighbor, Quad, Section, EDGE,
    EDGE_CORNER_CELLS, FACE_CELLS,
};

/// Signed section coordinates in one world/dimension, measured in 16-block units.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SectionCoord(pub i32, pub i32, pub i32);

impl SectionCoord {
    fn offset(self, [dx, dy, dz]: [i32; 3]) -> Option<Self> {
        Some(Self(
            self.0.checked_add(dx)?,
            self.1.checked_add(dy)?,
            self.2.checked_add(dz)?,
        ))
    }

    fn neighbors(self) -> impl Iterator<Item = (Self, [i32; 3])> {
        (-1..=1).flat_map(move |y| {
            (-1..=1).flat_map(move |z| {
                (-1..=1).filter_map(move |x| {
                    let offset = [x, y, z];
                    if offset == [0; 3] {
                        return None;
                    }
                    Some((self.offset(offset)?, offset))
                })
            })
        })
    }
}

fn boundary_range(side: i32) -> Range<usize> {
    match side {
        -1 => 0..1,
        1 => 15..16,
        _ => 0..16,
    }
}

fn boundary(offset: [i32; 3]) -> impl Iterator<Item = [usize; 3]> {
    boundary_range(offset[1]).flat_map(move |y| {
        boundary_range(offset[2])
            .flat_map(move |z| boundary_range(offset[0]).map(move |x| [x, y, z]))
    })
}

/// Default camera displacement, in blocks, that re-triggers a translucent
/// sort. Below it the previous back-to-front order is kept.
pub const DEFAULT_RESORT_THRESHOLD: f32 = 1.0;

/// Owns section snapshots and meshes. Repeated changes are coalesced until
/// rebuilding. Use one cache per world or dimension.
pub struct MeshCache {
    sections: HashMap<SectionCoord, Section>,
    meshes: HashMap<SectionCoord, Vec<Quad>>,
    dirty: BTreeSet<SectionCoord>,
    missing: MissingNeighbor,
    /// Camera position of the last committed translucent sort.
    sort_origin: Option<[f32; 3]>,
    /// Camera displacement in blocks that forces the next sort.
    resort_threshold: f32,
}

impl Default for MeshCache {
    fn default() -> Self {
        Self {
            sections: HashMap::new(),
            meshes: HashMap::new(),
            dirty: BTreeSet::new(),
            missing: MissingNeighbor::default(),
            sort_origin: None,
            resort_threshold: DEFAULT_RESORT_THRESHOLD,
        }
    }
}

impl MeshCache {
    pub fn new(missing: MissingNeighbor) -> Self {
        Self {
            missing,
            ..Self::default()
        }
    }

    /// Inserts or replaces a section, invalidating neighbors whose sampled
    /// borders changed. Returns whether the section snapshot changed.
    pub fn insert(&mut self, coord: SectionCoord, section: Section) -> bool {
        let old = self.sections.get(&coord);
        if old == Some(&section) {
            return false;
        }
        self.dirty.insert(coord);
        for (neighbor, offset) in coord.neighbors() {
            if self.sections.contains_key(&neighbor)
                && boundary(offset).any(|[x, y, z]| {
                    let before = old.map_or_else(|| self.missing.sample(), |s| s.get(x, y, z));
                    before != section.get(x, y, z)
                })
            {
                self.dirty.insert(neighbor);
            }
        }
        self.sections.insert(coord, section);
        true
    }

    pub fn remove(&mut self, coord: SectionCoord) -> bool {
        let Some(old) = self.sections.remove(&coord) else {
            return false;
        };
        self.meshes.remove(&coord);
        self.dirty.remove(&coord);
        for (neighbor, offset) in coord.neighbors() {
            if self.sections.contains_key(&neighbor)
                && boundary(offset).any(|[x, y, z]| old.get(x, y, z) != self.missing.sample())
            {
                self.dirty.insert(neighbor);
            }
        }
        true
    }

    /// Updates block metadata and both light nibbles. Missing sections, invalid
    /// local coordinates and repeated values leave the queue alone.
    pub fn set_cell(
        &mut self,
        coord: SectionCoord,
        [x, y, z]: [usize; 3],
        block: Block,
        light: Light,
    ) -> bool {
        let Some(section) = self.sections.get_mut(&coord) else {
            return false;
        };
        if !section.set(x, y, z, block, light) {
            return false;
        }
        self.dirty.insert(coord);
        for (neighbor, offset) in coord.neighbors() {
            if self.sections.contains_key(&neighbor)
                && offset
                    .into_iter()
                    .zip([x, y, z])
                    .all(|(side, i)| boundary_range(side).contains(&i))
            {
                self.dirty.insert(neighbor);
            }
        }
        true
    }

    /// A clean mesh, including an empty slice for a clean empty section.
    pub fn mesh(&self, coord: SectionCoord) -> Option<&[Quad]> {
        if self.dirty.contains(&coord) {
            return None;
        }
        self.meshes.get(&coord).map(Vec::as_slice)
    }

    pub fn pending(&self) -> usize {
        self.dirty.len()
    }

    /// Rebuilds at most `limit` loaded sections in coordinate order and
    /// returns the rebuilt coordinates.
    pub fn rebuild_dirty(&mut self, limit: usize) -> Vec<SectionCoord> {
        let mut rebuilt = Vec::new();
        for _ in 0..limit {
            let Some(coord) = self.dirty.pop_first() else {
                break;
            };
            let mesh = self.build(coord);
            self.meshes.insert(coord, mesh);
            rebuilt.push(coord);
        }
        rebuilt
    }

    /// Rebuilds at most `limit` dirty sections, in-frustum and near-camera
    /// first. `camera` is a world block position and `planes` the frustum
    /// planes in `box_in_frustum` form. Sections outside the frustum still
    /// rebuild, but only after every closer in-frustum section.
    pub fn rebuild_dirty_ordered(
        &mut self,
        limit: usize,
        camera: [f32; 3],
        planes: &[[f32; 4]],
    ) -> Vec<SectionCoord> {
        let mut order: Vec<SectionCoord> = self.dirty.iter().copied().collect();
        order.sort_by(|a, b| {
            Self::queue_key(*a, camera, planes).cmp(&Self::queue_key(*b, camera, planes))
        });
        let mut rebuilt = Vec::new();
        for coord in order.into_iter().take(limit) {
            self.dirty.remove(&coord);
            let mesh = self.build(coord);
            self.meshes.insert(coord, mesh);
            rebuilt.push(coord);
        }
        rebuilt
    }

    /// Dirty-queue ordering key: in-frustum first, then ascending squared
    /// camera distance to the nearest point of the section box. Coordinates
    /// keep their `BTreeSet` order as the final tie-break.
    fn queue_key(coord: SectionCoord, camera: [f32; 3], planes: &[[f32; 4]]) -> (bool, u64) {
        let min = [
            coord.0 as f32 * EDGE as f32,
            coord.1 as f32 * EDGE as f32,
            coord.2 as f32 * EDGE as f32,
        ];
        let mut dist2 = 0.0f32;
        for (axis, &p) in camera.iter().enumerate() {
            let lo = min[axis];
            let hi = lo + EDGE as f32;
            let gap = if p < lo {
                lo - p
            } else if p > hi {
                p - hi
            } else {
                0.0
            };
            dist2 += gap * gap;
        }
        // Quantize the distance so the key is `Ord`; ties fall back to the
        // set's coordinate order.
        (
            !box_in_frustum(min, planes),
            dist2.max(0.0).to_bits() as u64,
        )
    }

    /// Camera displacement in blocks that forces a translucent re-sort.
    pub fn resort_threshold(&self) -> f32 {
        self.resort_threshold
    }

    /// Sets the re-sort threshold; negative values clamp to zero.
    pub fn set_resort_threshold(&mut self, blocks: f32) {
        self.resort_threshold = blocks.max(0.0);
    }

    /// Whether [`resort_translucent`](Self::resort_translucent) would run:
    /// true until the first committed sort and whenever `camera` moved at
    /// least `resort_threshold` blocks since the last one.
    pub fn translucent_resort_due(&self, camera: [f32; 3]) -> bool {
        let Some(origin) = self.sort_origin else {
            return true;
        };
        let dist2: f64 = camera
            .iter()
            .zip(origin)
            .map(|(&c, o)| f64::from(c - o) * f64::from(c - o))
            .sum();
        dist2 >= f64::from(self.resort_threshold) * f64::from(self.resort_threshold)
    }

    /// Incremental translucent re-sort. While the camera stays inside
    /// `resort_threshold` of the last sort this is a no-op and returns an
    /// empty list. Once the camera crosses it, every clean mesh has its
    /// translucent quads (the `is_translucent` set: translucent blocks and
    /// water) reordered back-to-front with the same depth key and tie-break
    /// as `mc_spine_client_mesh_sort_translucent`, and `camera` is recorded
    /// as the new sort origin. Dirty sections are skipped: their rebuild
    /// replaces the order anyway. Returns the coordinates whose translucent
    /// quad order actually changed.
    pub fn resort_translucent(&mut self, camera: [f32; 3]) -> Vec<SectionCoord> {
        if !self.translucent_resort_due(camera) {
            return Vec::new();
        }
        self.sort_origin = Some(camera);
        let mut changed = Vec::new();
        for (&coord, mesh) in self.meshes.iter_mut() {
            if self.dirty.contains(&coord) {
                continue;
            }
            let local = [
                camera[0] - coord.0 as f32 * EDGE as f32,
                camera[1] - coord.1 as f32 * EDGE as f32,
                camera[2] - coord.2 as f32 * EDGE as f32,
            ];
            if Self::sort_back_to_front(mesh, local) {
                changed.push(coord);
            }
        }
        changed.sort_unstable();
        changed
    }

    /// Reorders `mesh`'s translucent quads by decreasing `depth_key`, keeping
    /// every other quad in place and ascending input index for ties. Returns
    /// whether the order changed.
    fn sort_back_to_front(mesh: &mut [Quad], camera: [f32; 3]) -> bool {
        let mut order: Vec<(f64, u32)> = mesh
            .iter()
            .enumerate()
            .filter(|(_, q)| is_translucent(q))
            .map(|(i, q)| (depth_key(q, camera), i as u32))
            .collect();
        if order.len() < 2 {
            return false;
        }
        order.sort_unstable_by(|(l, i), (r, j)| {
            r.partial_cmp(l)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(i.cmp(j))
        });
        if order.iter().map(|&(_, i)| i).is_sorted() {
            return false;
        }
        let sorted: Vec<Quad> = order.iter().map(|&(_, i)| mesh[i as usize]).collect();
        let mut slots: Vec<usize> = order.iter().map(|&(_, i)| i as usize).collect();
        slots.sort_unstable();
        for (slot, quad) in slots.into_iter().zip(sorted) {
            mesh[slot] = quad;
        }
        true
    }

    fn build(&self, coord: SectionCoord) -> Vec<Quad> {
        let mut view = self.sections[&coord].view();
        view.missing = self.missing;
        let mut borders = [[self.missing.sample(); FACE_CELLS]; 6];
        let mut edges = [self.missing.sample(); EDGE_CORNER_CELLS];
        for (neighbor, offset) in coord.neighbors() {
            let Some(section) = self.sections.get(&neighbor) else {
                continue;
            };
            let direction = Direction::ALL
                .into_iter()
                .find(|&d| d.offset([0; 3]) == offset);
            for p @ [x, y, z] in boundary(offset.map(|n| -n)) {
                let sample = section.get(x, y, z);
                if let Some(direction) = direction {
                    borders[direction as usize][direction.border_index(p)] = sample;
                } else {
                    let halo_pos = std::array::from_fn(|axis| p[axis] as i32 + offset[axis] * 16);
                    edges[edge_corner_index(halo_pos).expect("edge or corner")] = sample;
                }
            }
        }
        view.borders = borders.each_ref().map(Some);
        view.edges_and_corners = Some(&edges);
        mesh_section(&view)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Layer, NO_FLUID};

    /// Axis-aligned box frustum: inside `min ..= max` in world blocks.
    fn box_planes(min: [f32; 3], max: [f32; 3]) -> [[f32; 4]; 6] {
        [
            [1.0, 0.0, 0.0, -min[0]],
            [-1.0, 0.0, 0.0, max[0]],
            [0.0, 1.0, 0.0, -min[1]],
            [0.0, -1.0, 0.0, max[1]],
            [0.0, 0.0, 1.0, -min[2]],
            [0.0, 0.0, -1.0, max[2]],
        ]
    }

    fn quad_at(x: f32, layer: Layer) -> Quad {
        Quad {
            positions: [
                [x, 1.0, 1.0],
                [x + 1.0, 1.0, 1.0],
                [x + 1.0, 1.0, 2.0],
                [x, 1.0, 2.0],
            ],
            state_id: 1,
            tint: 0,
            flow: [0.0; 2],
            ao: [3; 4],
            block_light: [0; 4],
            sky_light: [15; 4],
            face_light: Light::new(0, 15),
            direction: Direction::Up,
            layer,
            diagonal: 0,
            fluid_kind: NO_FLUID,
            flags: 0,
            reserved: 0,
        }
    }

    #[test]
    fn ordered_rebuild_runs_in_frustum_near_camera_first() {
        let mut cache = MeshCache::default();
        let near = SectionCoord(0, 0, 0);
        let mid = SectionCoord(1, 0, 0);
        let far = SectionCoord(5, 0, 0);
        let behind = SectionCoord(-2, 0, 0);
        for coord in [far, behind, mid, near] {
            cache.insert(coord, Section::filled(Block::AIR, Light::new(0, 15)));
        }
        // Inside x >= 0; camera at block (8, 8, 8).
        let planes = box_planes([0.0, -1024.0, -1024.0], [1024.0; 3]);
        let out = cache.rebuild_dirty_ordered(usize::MAX, [8.0, 8.0, 8.0], &planes);
        assert_eq!(out, [near, mid, far, behind]);

        for coord in [far, behind, mid, near] {
            cache.dirty.insert(coord);
        }
        let out = cache.rebuild_dirty_ordered(2, [8.0, 8.0, 8.0], &planes);
        assert_eq!(out, [near, mid]);
        assert_eq!(cache.pending(), 2);
    }

    #[test]
    fn translucent_resort_is_gated_on_camera_threshold() {
        let mut cache = MeshCache::default();
        let coord = SectionCoord(0, 0, 0);
        let stale = SectionCoord(0, 0, 1);
        let near_quad = quad_at(2.0, Layer::Translucent);
        let far_quad = quad_at(6.0, Layer::Translucent);
        let opaque_quad = quad_at(9.0, Layer::Opaque);
        cache
            .meshes
            .insert(coord, vec![near_quad, far_quad, opaque_quad]);
        cache.meshes.insert(stale, vec![near_quad, far_quad]);
        cache.dirty.insert(stale);

        // First sort always runs; camera at x=4 makes x=6.5 the far quad.
        assert!(cache.translucent_resort_due([4.0, 8.0, 8.0]));
        assert_eq!(cache.resort_translucent([4.0, 8.0, 8.0]), [coord]);
        assert_eq!(cache.meshes[&coord][0], far_quad);
        assert_eq!(cache.meshes[&coord][1], near_quad);
        // The opaque quad and the dirty section are untouched.
        assert_eq!(cache.meshes[&coord][2], opaque_quad);
        assert_eq!(cache.meshes[&stale][0], near_quad);

        // Small movement stays under the threshold: no work, origin kept.
        assert!(!cache.translucent_resort_due([4.5, 8.0, 8.0]));
        assert!(cache.resort_translucent([4.5, 8.0, 8.0]).is_empty());
        assert_eq!(cache.meshes[&coord][0], far_quad);

        // Crossing the threshold re-sorts; x=5 flips which quad is farther.
        assert!(cache.translucent_resort_due([5.0, 8.0, 8.0]));
        assert_eq!(cache.resort_translucent([5.0, 8.0, 8.0]), [coord]);
        assert_eq!(cache.meshes[&coord][0], near_quad);
        assert_eq!(cache.meshes[&coord][1], far_quad);

        // A threshold of zero makes every movement a re-sort trigger, but an
        // unchanged order still reports nothing.
        cache.set_resort_threshold(0.0);
        assert!(cache.translucent_resort_due([5.0, 8.0, 8.0]));
        assert!(cache.resort_translucent([5.0, 8.0, 8.0]).is_empty());
        assert_eq!(cache.meshes[&coord][0], near_quad);
    }
}
