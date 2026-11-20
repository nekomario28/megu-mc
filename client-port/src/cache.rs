// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

use std::collections::{BTreeSet, HashMap};
use std::ops::Range;

use crate::{
    edge_corner_index, mesh_section, Block, Direction, Light, MissingNeighbor, Quad, Section,
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

/// Owns section snapshots and meshes. Repeated changes are coalesced until
/// rebuilding. Use one cache per world or dimension.
#[derive(Default)]
pub struct MeshCache {
    sections: HashMap<SectionCoord, Section>,
    meshes: HashMap<SectionCoord, Vec<Quad>>,
    dirty: BTreeSet<SectionCoord>,
    missing: MissingNeighbor,
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
