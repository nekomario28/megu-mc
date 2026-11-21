// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Multi-draw render regions: an 8x4x8 slab of sections (256) shares one
//! vertex arena and one index arena. Section uploads carve variable-length
//! slabs out of the shared buffers with a first-fit allocator, so an unloaded
//! section's storage is reused by the next same-or-smaller upload instead of
//! growing the arena. `build_multidraw` folds every emitted section into one
//! [`MultiDrawCommand`] per (region, camera-facing bucket) pair, replacing the
//! per-section draw call with a single multi-draw submission.

use std::collections::{BTreeMap, HashMap};

use crate::cull::box_facing_mask;
use crate::packed::{pack_quad, PackedVertex};
use crate::{Quad, SectionCoord, EDGE};

/// Sections per region along X and Z, and the Y height respectively.
pub const REGION_X: usize = 8;
pub const REGION_Y: usize = 4;
pub const REGION_Z: usize = 8;
pub const REGION_SECTIONS: usize = REGION_X * REGION_Y * REGION_Z;

/// First-fit slab allocator over a shared element array. The free list is kept
/// sorted by offset and coalesced on release, so repeated unload/rebuild
/// cycles reuse the lowest matching range instead of growing the buffer.
#[derive(Default)]
struct Arena<T> {
    buf: Vec<T>,
    /// (offset, len) holes, sorted by offset, never adjacent.
    free: Vec<(usize, usize)>,
}

impl<T: Copy> Arena<T> {
    fn upload(&mut self, data: &[T]) -> u32 {
        if data.is_empty() {
            return 0;
        }
        for i in 0..self.free.len() {
            let (offset, len) = self.free[i];
            if len >= data.len() {
                if len == data.len() {
                    self.free.remove(i);
                } else {
                    self.free[i] = (offset + data.len(), len - data.len());
                }
                self.buf[offset..offset + data.len()].copy_from_slice(data);
                return offset as u32;
            }
        }
        let offset = self.buf.len();
        self.buf.extend_from_slice(data);
        offset as u32
    }

    fn release(&mut self, offset: usize, len: usize) {
        if len == 0 {
            return;
        }
        let pos = self.free.partition_point(|&(o, _)| o < offset);
        self.free.insert(pos, (offset, len));
        if pos + 1 < self.free.len() && self.free[pos].0 + self.free[pos].1 == self.free[pos + 1].0
        {
            self.free[pos].1 += self.free[pos + 1].1;
            self.free.remove(pos + 1);
        }
        if pos > 0 && self.free[pos - 1].0 + self.free[pos - 1].1 == self.free[pos].0 {
            self.free[pos - 1].1 += self.free[pos].1;
            self.free.remove(pos);
        }
    }

    fn free_len(&self) -> usize {
        self.free.iter().map(|&(_, len)| len).sum()
    }
}

/// One indirect draw in a [`MultiDrawCommand`], matching the
/// `glDrawElementsInstancedBaseVertexBaseInstance` parameter block.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrawElementsCommand {
    pub index_count: u32,
    pub instance_count: u32,
    pub first_index: u32,
    pub base_vertex: i32,
    pub base_instance: u32,
}

/// A slab of shared-arena storage owned by one section.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SectionSlab {
    /// First packed vertex in the region vertex arena.
    pub vertex_offset: u32,
    /// Packed vertex count (four per quad).
    pub vertices: u32,
    /// First index in the region index arena.
    pub index_offset: u32,
    /// Section-local index count (six per quad).
    pub indices: u32,
}

/// Occupancy and arena-usage counters for one [`RenderRegion`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RegionStats {
    /// Slots holding an uploaded section, including zero-quad uploads.
    pub sections: usize,
    /// Packed-vertex arena length and how much of it sits in the free list.
    pub vertex_words: usize,
    pub vertex_free: usize,
    /// Index arena length and how much of it sits in the free list.
    pub index_words: usize,
    pub index_free: usize,
}

/// One API-visible multi-draw submission: every section in a region that
/// faces the camera in the same direction bucket shares one command.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MultiDrawCommand {
    /// Region-minimum section coordinate.
    pub region: [i32; 3],
    /// Shared `box_facing_mask` bucket of every draw in `draws`.
    pub facing: u8,
    /// Indirect draws in region slot order.
    pub draws: Vec<DrawElementsCommand>,
}

/// Shared vertex/index arenas for one 8x4x8 slab of sections.
pub struct RenderRegion {
    /// Region-minimum section coordinate; multiples of (8, 4, 8).
    origin: [i32; 3],
    vertices: Arena<PackedVertex>,
    indices: Arena<u32>,
    slots: Box<[Option<SectionSlab>; REGION_SECTIONS]>,
    occupied: usize,
}

impl Default for RenderRegion {
    fn default() -> Self {
        Self::new([0; 3])
    }
}

impl RenderRegion {
    /// A region covering `origin .. origin + (8, 4, 8)` in section space.
    /// `origin` should be a multiple of the region extents, as produced by
    /// [`RenderRegions::origin_of`].
    pub fn new(origin: [i32; 3]) -> Self {
        Self {
            origin,
            vertices: Arena::default(),
            indices: Arena::default(),
            slots: Box::new([None; REGION_SECTIONS]),
            occupied: 0,
        }
    }

    pub fn origin(&self) -> [i32; 3] {
        self.origin
    }

    fn local(&self, coord: SectionCoord) -> Option<usize> {
        let x = coord.0.checked_sub(self.origin[0])?;
        let y = coord.1.checked_sub(self.origin[1])?;
        let z = coord.2.checked_sub(self.origin[2])?;
        if x < 0
            || x >= REGION_X as i32
            || z < 0
            || z >= REGION_Z as i32
            || y < 0
            || y >= REGION_Y as i32
        {
            return None;
        }
        Some(y as usize * REGION_X * REGION_Z + z as usize * REGION_X + x as usize)
    }

    pub fn contains(&self, coord: SectionCoord) -> bool {
        self.local(coord).is_some()
    }

    /// Number of slots holding an upload.
    pub fn sections(&self) -> usize {
        self.occupied
    }

    pub fn slab(&self, coord: SectionCoord) -> Option<SectionSlab> {
        self.slots[self.local(coord)?]
    }

    /// Writes `quads` into the shared arenas, releasing any previous slab for
    /// the section first so a same-size rebuild lands back on its old range.
    /// Returns false when `coord` lies outside the region.
    pub fn upload(&mut self, coord: SectionCoord, quads: &[Quad]) -> bool {
        let Some(index) = self.local(coord) else {
            return false;
        };
        let mut vertices = Vec::with_capacity(quads.len() * 4);
        let mut indices = Vec::with_capacity(quads.len() * 6);
        for (q, quad) in quads.iter().enumerate() {
            vertices.extend(pack_quad(quad));
            let base = q as u32 * 4;
            indices.extend(quad.indices().map(|corner| base + u32::from(corner)));
        }
        if let Some(old) = self.slots[index].take() {
            self.vertices
                .release(old.vertex_offset as usize, old.vertices as usize);
            self.indices
                .release(old.index_offset as usize, old.indices as usize);
            self.occupied -= 1;
        }
        let vertex_offset = self.vertices.upload(&vertices);
        let index_offset = self.indices.upload(&indices);
        self.slots[index] = Some(SectionSlab {
            vertex_offset,
            vertices: vertices.len() as u32,
            index_offset,
            indices: indices.len() as u32,
        });
        self.occupied += 1;
        true
    }

    /// Releases the section's slab back to the arenas. Returns false when the
    /// slot was empty or `coord` lies outside the region.
    pub fn remove(&mut self, coord: SectionCoord) -> bool {
        let Some(index) = self.local(coord) else {
            return false;
        };
        let Some(old) = self.slots[index].take() else {
            return false;
        };
        self.vertices
            .release(old.vertex_offset as usize, old.vertices as usize);
        self.indices
            .release(old.index_offset as usize, old.indices as usize);
        self.occupied -= 1;
        true
    }

    /// Arena and occupancy counters; `*_free` is reusable before growth.
    pub fn stats(&self) -> RegionStats {
        RegionStats {
            sections: self.occupied,
            vertex_words: self.vertices.buf.len(),
            vertex_free: self.vertices.free_len(),
            index_words: self.indices.buf.len(),
            index_free: self.indices.free_len(),
        }
    }

    /// Folds the region's non-empty slots into one [`MultiDrawCommand`] per
    /// direction bucket. `camera` is a world block position; a section's
    /// bucket is the `box_facing_mask` of its section box, so draws facing the
    /// camera alike submit together. Buckets and their draws are ordered
    /// deterministically.
    pub fn build_multidraw(&self, camera: [f32; 3]) -> Vec<MultiDrawCommand> {
        let mut buckets: BTreeMap<u8, Vec<DrawElementsCommand>> = BTreeMap::new();
        for (index, slot) in self.slots.iter().enumerate() {
            let Some(slab) = slot else { continue };
            if slab.indices == 0 {
                continue;
            }
            let x = index % REGION_X;
            let z = (index / REGION_X) % REGION_Z;
            let y = index / (REGION_X * REGION_Z);
            let min = [
                ((self.origin[0] + x as i32) * EDGE as i32) as f32,
                ((self.origin[1] + y as i32) * EDGE as i32) as f32,
                ((self.origin[2] + z as i32) * EDGE as i32) as f32,
            ];
            buckets
                .entry(box_facing_mask(min, camera))
                .or_default()
                .push(DrawElementsCommand {
                    index_count: slab.indices,
                    instance_count: 1,
                    first_index: slab.index_offset,
                    base_vertex: slab.vertex_offset as i32,
                    base_instance: 0,
                });
        }
        buckets
            .into_iter()
            .map(|(facing, draws)| MultiDrawCommand {
                region: self.origin,
                facing,
                draws,
            })
            .collect()
    }
}

/// A world/dimension set of [`RenderRegion`]s keyed by region-minimum
/// coordinate. Sections are routed to their containing region automatically.
#[derive(Default)]
pub struct RenderRegions {
    regions: HashMap<[i32; 3], RenderRegion>,
}

impl RenderRegions {
    /// The region-minimum coordinate containing `coord`: each axis floored to
    /// the region extents (8, 4, 8).
    pub fn origin_of(coord: SectionCoord) -> [i32; 3] {
        [
            coord.0.div_euclid(REGION_X as i32) * REGION_X as i32,
            coord.1.div_euclid(REGION_Y as i32) * REGION_Y as i32,
            coord.2.div_euclid(REGION_Z as i32) * REGION_Z as i32,
        ]
    }

    /// The region covering `coord`, if any section in it is loaded.
    pub fn region(&self, coord: SectionCoord) -> Option<&RenderRegion> {
        self.regions.get(&Self::origin_of(coord))
    }

    /// Uploads `quads` for `coord`, creating the containing region on first
    /// use. Re-uploading reuses the released slab when it still fits.
    pub fn upload(&mut self, coord: SectionCoord, quads: &[Quad]) {
        self.regions
            .entry(Self::origin_of(coord))
            .or_insert_with(|| RenderRegion::new(Self::origin_of(coord)))
            .upload(coord, quads);
    }

    /// Unloads `coord` and drops the region once its last slot is released.
    pub fn remove(&mut self, coord: SectionCoord) -> bool {
        let origin = Self::origin_of(coord);
        let Some(region) = self.regions.get_mut(&origin) else {
            return false;
        };
        if !region.remove(coord) {
            return false;
        }
        if region.occupied == 0 {
            self.regions.remove(&origin);
        }
        true
    }

    /// One [`MultiDrawCommand`] per (region, direction bucket) across all
    /// loaded regions, ordered by region origin then facing bucket.
    pub fn build_multidraw(&self, camera: [f32; 3]) -> Vec<MultiDrawCommand> {
        let mut commands: Vec<_> = self
            .regions
            .values()
            .flat_map(|region| region.build_multidraw(camera))
            .collect();
        commands.sort_by(|a, b| a.region.cmp(&b.region).then(a.facing.cmp(&b.facing)));
        commands
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Direction, Layer, Light};

    fn quad(x: f32) -> Quad {
        Quad {
            positions: [
                [x, 0.0, 0.0],
                [x + 1.0, 0.0, 0.0],
                [x + 1.0, 1.0, 0.0],
                [x, 1.0, 0.0],
            ],
            state_id: 1,
            tint: 0,
            flow: [0.0; 2],
            ao: [3; 4],
            block_light: [0; 4],
            sky_light: [15; 4],
            face_light: Light::new(0, 15),
            direction: Direction::Up,
            layer: Layer::Opaque,
            diagonal: 0,
            fluid_kind: crate::NO_FLUID,
            flags: 0,
            reserved: 0,
        }
    }

    #[test]
    fn unload_rebuild_reuses_the_released_slab() {
        let mut region = RenderRegion::new([0; 3]);
        let a = SectionCoord(0, 0, 0);
        let b = SectionCoord(1, 0, 0);
        assert!(region.upload(a, &[quad(0.0); 4]));
        assert!(region.upload(b, &[quad(0.0); 2]));
        let old_a = region.slab(a).unwrap();
        let old_b = region.slab(b).unwrap();
        assert_eq!((old_a.vertex_offset, old_a.index_offset), (0, 0));
        let grown = region.stats();
        assert_eq!(grown.vertex_words, 24);

        // Rebuilding in place releases before allocating, so `b` keeps its
        // exact slab instead of growing the arena.
        assert!(region.upload(b, &[quad(0.0); 2]));
        assert_eq!(region.slab(b).unwrap(), old_b);
        assert_eq!(region.stats().vertex_words, grown.vertex_words);

        // Unloading `a` frees its range; a same-size upload reuses it.
        assert!(region.remove(a));
        let c = SectionCoord(2, 0, 0);
        assert!(region.upload(c, &[quad(0.0); 4]));
        assert_eq!(region.slab(c).unwrap(), old_a);
        assert_eq!(region.stats().vertex_words, grown.vertex_words);

        // A smaller upload splits the freed range instead of growing.
        assert!(region.remove(c));
        let d = SectionCoord(3, 0, 0);
        assert!(region.upload(d, &[quad(0.0); 1]));
        assert_eq!(region.slab(d).unwrap().vertex_offset, 0);
        let e = SectionCoord(4, 0, 0);
        assert!(region.upload(e, &[quad(0.0); 2]));
        assert_eq!(region.slab(e).unwrap().vertex_offset, 4);
        let stats = region.stats();
        assert_eq!(stats.vertex_words, grown.vertex_words);
        assert_eq!(stats.vertex_free, 4);
    }

    #[test]
    fn multidraw_groups_by_region_and_facing_bucket() {
        let mut regions = RenderRegions::default();
        // One row of sections along X inside a single region.
        for x in 0..8 {
            regions.upload(SectionCoord(x, 0, 0), &[quad(0.0)]);
        }
        // A second region further east.
        regions.upload(SectionCoord(8, 0, 0), &[quad(0.0)]);
        // Camera west of everything: every section's West face is exposed.
        let camera = [-8.0, 8.0, 8.0];
        let commands = regions.build_multidraw(camera);
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0].region, [0, 0, 0]);
        assert_eq!(commands[1].region, [8, 0, 0]);
        assert_eq!(commands[0].draws.len(), 8);
        assert_eq!(commands[0].facing, Direction::West.bit());
        // Nine sections collapse into two submissions, one per region.
        let draws: usize = commands.iter().map(|c| c.draws.len()).sum();
        assert_eq!(draws, 9);
        for (i, draw) in commands[0].draws.iter().enumerate() {
            assert_eq!(draw.index_count, 6);
            assert_eq!(draw.first_index, i as u32 * 6);
            assert_eq!(draw.base_vertex, i as i32 * 4);
        }

        // A camera inside the first section splits that region into two
        // buckets: enclosed sections (facing 0) and West-facing ones.
        let inside = [8.0, 8.0, 8.0];
        let commands = regions.build_multidraw(inside);
        assert_eq!(commands.len(), 3);
        let mut facings = commands.iter().map(|c| (c.region, c.facing));
        assert_eq!(facings.next(), Some(([0, 0, 0], 0)));
        assert_eq!(facings.next(), Some(([0, 0, 0], Direction::West.bit())));
        assert_eq!(facings.next(), Some(([8, 0, 0], Direction::West.bit())));
    }

    #[test]
    fn arena_free_list_coalesces_across_neighbors() {
        let mut region = RenderRegion::new([0; 3]);
        let coords: Vec<_> = (0..4).map(|x| SectionCoord(x, 0, 0)).collect();
        for (i, &coord) in coords.iter().enumerate() {
            assert!(region.upload(coord, &[quad(i as f32); 2]));
        }
        // Release the middle two; adjacent holes must merge into one range
        // that a three-quad upload fills exactly.
        assert!(region.remove(coords[1]));
        assert!(region.remove(coords[2]));
        let hole = SectionCoord(4, 0, 0);
        assert!(region.upload(hole, &[quad(0.0); 3]));
        let slab = region.slab(hole).unwrap();
        assert_eq!(slab.vertex_offset, 8);
        let stats = region.stats();
        assert_eq!(stats.vertex_words, 32);
        // The 4-word tail of the merged hole remains free.
        assert_eq!(stats.vertex_free, 4);
    }
}
