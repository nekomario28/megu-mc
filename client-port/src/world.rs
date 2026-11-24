// SPDX-License-Identifier: LGPL-3.0-or-later

//! Client-side world state: compressed voxel storage plus the mesh cache.
//!
//! [`WorldState`] is the embedding-friendly facade over [`MeshCache`]. It
//! stores section voxels as [`CompressedSection`] (uniform sections cost
//! zero block-array heap bytes), rebuilds meshes through the same
//! dirty-queue machinery, and exposes the ultra-low-latency additions:
//! the O(1) [`take_dirty_mesh_coords`](WorldState::take_dirty_mesh_coords)
//! GPU-work queue, the Priority-0 near-player lane
//! [`rebuild_dirty_priority_zero`](WorldState::rebuild_dirty_priority_zero),
//! VRAM-uploaded opaque CPU-mesh release
//! [`release_uploaded_opaque_cpu_mesh`](WorldState::release_uploaded_opaque_cpu_mesh),
//! and the distance governor
//! [`evict_chunks_outside_radius`](WorldState::evict_chunks_outside_radius).

use crate::{
    Block, CompressedSection, Light, MeshCache, MissingNeighbor, Quad, ReleasedMeshMeta, Section,
    SectionCoord,
};

/// Loaded-world voxel + mesh state for one dimension.
///
/// All mutation funnels through the inner [`MeshCache`], so neighbor
/// invalidation, translucent re-sorting, the rebuilt-coordinate queue and
/// distance eviction behave exactly as they do on the cache itself.
#[derive(Default)]
pub struct WorldState {
    mesh_cache: MeshCache,
}

impl WorldState {
    /// An empty world using `MissingNeighbor::Air` at unloaded borders.
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty world with an explicit missing-neighbor policy.
    pub fn with_missing(missing: MissingNeighbor) -> Self {
        Self {
            mesh_cache: MeshCache::new(missing),
        }
    }

    /// The inner cache, read-only.
    pub fn mesh_cache(&self) -> &MeshCache {
        &self.mesh_cache
    }

    /// The inner cache, for callers driving cache-specific options
    /// (resort threshold, missing-neighbor policy).
    pub fn mesh_cache_mut(&mut self) -> &mut MeshCache {
        &mut self.mesh_cache
    }

    /// Inserts or replaces a section snapshot (stored compressed when
    /// uniform). Returns whether the snapshot changed.
    pub fn insert_section(&mut self, coord: SectionCoord, section: Section) -> bool {
        self.mesh_cache.insert(coord, section)
    }

    /// [`insert_section`](Self::insert_section) for an already-compressed
    /// snapshot.
    pub fn insert_compressed(&mut self, coord: SectionCoord, section: CompressedSection) -> bool {
        self.mesh_cache.insert_compressed(coord, section)
    }

    /// Unloads a section's voxels and cached mesh.
    pub fn remove_section(&mut self, coord: SectionCoord) -> bool {
        self.mesh_cache.remove(coord)
    }

    /// Block edit in section-local coordinates; promotes the section's
    /// storage on the first non-uniform write. Returns whether it changed.
    pub fn set_cell(
        &mut self,
        coord: SectionCoord,
        local: [usize; 3],
        block: Block,
        light: Light,
    ) -> bool {
        self.mesh_cache.set_cell(coord, local, block, light)
    }

    /// Reads a section-local cell, `None` when the section is not loaded.
    pub fn get_cell(&self, coord: SectionCoord, [x, y, z]: [usize; 3]) -> Option<crate::Sample> {
        self.mesh_cache.section(coord).map(|s| s.get(x, y, z))
    }

    /// The stored section snapshot, in [`CompressedSection`] form.
    pub fn section(&self, coord: SectionCoord) -> Option<&CompressedSection> {
        self.mesh_cache.section(coord)
    }

    /// Whether the section's block storage is the zero-heap uniform form.
    pub fn is_uniform_section(&self, coord: SectionCoord) -> Option<bool> {
        self.mesh_cache.is_uniform_section(coord)
    }

    /// Heap bytes held by the section's block storage (`0` when uniform).
    pub fn blocks_heap_bytes(&self, coord: SectionCoord) -> Option<usize> {
        self.mesh_cache.blocks_heap_bytes(coord)
    }

    /// The clean mesh for `coord` (translucent quads only after an opaque
    /// CPU-mesh release), `None` when missing or dirty.
    pub fn mesh(&self, coord: SectionCoord) -> Option<&[Quad]> {
        self.mesh_cache.mesh(coord)
    }

    /// Dirty sections awaiting a mesh rebuild.
    pub fn pending(&self) -> usize {
        self.mesh_cache.pending()
    }

    /// Number of loaded sections (voxels held).
    pub fn sections_loaded(&self) -> usize {
        self.mesh_cache.sections_loaded()
    }

    /// Rebuilds at most `limit` dirty sections in coordinate order.
    pub fn rebuild_dirty(&mut self, limit: usize) -> Vec<SectionCoord> {
        self.mesh_cache.rebuild_dirty(limit)
    }

    /// Rebuilds at most `limit` dirty sections, in-frustum and near-camera
    /// first. See [`MeshCache::rebuild_dirty_ordered`].
    pub fn rebuild_dirty_ordered(
        &mut self,
        limit: usize,
        camera: [f32; 3],
        planes: &[[f32; 4]],
    ) -> Vec<SectionCoord> {
        self.mesh_cache.rebuild_dirty_ordered(limit, camera, planes)
    }

    /// Priority-0 lane: immediately rebuilds dirty sections within
    /// `near_radius_blocks` of the player (`camera` in world blocks),
    /// before any distant chunk work. See
    /// [`MeshCache::rebuild_dirty_priority_zero`].
    pub fn rebuild_dirty_priority_zero(
        &mut self,
        camera_pos: [f32; 3],
        near_radius_blocks: f32,
        max_near: usize,
    ) -> Vec<SectionCoord> {
        self.mesh_cache
            .rebuild_dirty_priority_zero(camera_pos, near_radius_blocks, max_near)
    }

    /// Drains the deduplicated queue of sections whose mesh changed since
    /// the last call — the O(1) replacement for scanning every loaded
    /// section and byte-comparing vertex slices. See
    /// [`MeshCache::take_dirty_mesh_coords`].
    pub fn take_dirty_mesh_coords(&mut self) -> Vec<SectionCoord> {
        self.mesh_cache.take_dirty_mesh_coords()
    }

    /// Drops the uploaded section's opaque CPU quad payload, keeping
    /// translucent re-sort data and metadata. Returns freed bytes. See
    /// [`MeshCache::release_uploaded_opaque_cpu_mesh`].
    pub fn release_uploaded_opaque_cpu_mesh(&mut self, coord: SectionCoord) -> usize {
        self.mesh_cache.release_uploaded_opaque_cpu_mesh(coord)
    }

    /// Metadata kept for a released section, `None` while the full CPU mesh
    /// is still held.
    pub fn released_mesh_meta(&self, coord: SectionCoord) -> Option<&ReleasedMeshMeta> {
        self.mesh_cache.released_mesh_meta(coord)
    }

    /// Drops every chunk column outside Chebyshev distance `radius_chunks`
    /// (typically `view_distance + 2`) of the center column — voxels and
    /// cached meshes alike. Returns the evicted column count. See
    /// [`MeshCache::evict_chunks_outside_radius`].
    pub fn evict_chunks_outside_radius(
        &mut self,
        center_cx: i32,
        center_cz: i32,
        radius_chunks: i32,
    ) -> usize {
        self.mesh_cache
            .evict_chunks_outside_radius(center_cx, center_cz, radius_chunks)
    }

    /// Incremental translucent re-sort; see
    /// [`MeshCache::resort_translucent`].
    pub fn resort_translucent(&mut self, camera: [f32; 3]) -> Vec<SectionCoord> {
        self.mesh_cache.resort_translucent(camera)
    }

    /// Whether a translucent re-sort would run for `camera`.
    pub fn translucent_resort_due(&self, camera: [f32; 3]) -> bool {
        self.mesh_cache.translucent_resort_due(camera)
    }
}
