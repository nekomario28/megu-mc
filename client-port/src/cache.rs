// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::ops::Range;
use std::sync::Arc;

use crate::cull::box_in_frustum;
use crate::packed::{depth_key, is_translucent, pack_quad, PackedVertex};
use crate::upload_budget::ChunkUploadBudget;
use crate::{
    edge_corner_index, mesh_section, Block, CompressedSection, Direction, DirectionBuckets,
    Light, MissingNeighbor, Quad, Sample, Section, SectionBlocks, SectionView, EDGE,
    EDGE_CORNER_CELLS, FACE_CELLS, SECTION_CELLS, DIRECTION_BUCKETS,
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

/// Canonical 3x3x3 neighborhood order for [`SectionMeshInput`]: slot 0 is the
/// center `[0, 0, 0]`, slots 1..=26 enumerate the 26 neighbor offsets in
/// y/z/x ascending order — the same set [`SectionCoord::neighbors`] yields.
const fn neighbor_offsets() -> [[i32; 3]; 27] {
    let mut offsets = [[0i32; 3]; 27];
    let mut slot = 1usize;
    let mut y = -1i32;
    while y <= 1 {
        let mut z = -1i32;
        while z <= 1 {
            let mut x = -1i32;
            while x <= 1 {
                if x != 0 || y != 0 || z != 0 {
                    offsets[slot] = [x, y, z];
                    slot += 1;
                }
                x += 1;
            }
            z += 1;
        }
        y += 1;
    }
    offsets
}

const NEIGHBOR_OFFSETS: [[i32; 3]; 27] = neighbor_offsets();

/// Inverse of [`NEIGHBOR_OFFSETS`]: `NEIGHBOR_SLOTS[dx + 1][dy + 1][dz + 1]`
/// is the cell slot holding the neighbor at offset `[dx, dy, dz]`.
const fn neighbor_slots() -> [[[usize; 3]; 3]; 3] {
    let mut slots = [[[0usize; 3]; 3]; 3];
    let mut slot = 0usize;
    while slot < NEIGHBOR_OFFSETS.len() {
        let offset = NEIGHBOR_OFFSETS[slot];
        slots[(offset[0] + 1) as usize][(offset[1] + 1) as usize][(offset[2] + 1) as usize] =
            slot;
        slot += 1;
    }
    slots
}

const NEIGHBOR_SLOTS: [[[usize; 3]; 3]; 3] = neighbor_slots();

/// Lock-free mesh-job input: an immutable snapshot of one section plus its 26
/// neighbors, captured as `Arc` clones only — no voxel data is copied. Safe to
/// send to any worker thread (`Send + Sync`) and to keep building from after
/// the [`MeshCache`] mutates: in-flight [`CompressedSection`] snapshots stay
/// alive through the `Arc`s and [`MeshCache::set_cell`] copy-on-writes.
///
/// Obtain one via [`MeshCache::snapshot_section_input`]; the private fields
/// keep the center/neighbor slot invariant intact.
pub struct SectionMeshInput {
    /// Section coordinate being meshed.
    pub coord: SectionCoord,
    /// Generation counter supplied by the caller; compare against the world's
    /// current revision on result receipt and discard stale meshes in O(1).
    pub revision: u64,
    missing: MissingNeighbor,
    /// `cells[0]` is the center section; `cells[i]` for `i >= 1` is the
    /// neighbor at [`NEIGHBOR_OFFSETS`][i], `None` when not loaded.
    cells: [Option<Arc<CompressedSection>>; 27],
}

impl SectionMeshInput {
    /// The neighbor snapshot at a `-1..=1` offset from [`Self::coord`], or the
    /// center itself for `[0, 0, 0]`.
    fn cell_at_offset(&self, offset: [i32; 3]) -> Option<&CompressedSection> {
        let slot = NEIGHBOR_SLOTS[(offset[0] + 1) as usize][(offset[1] + 1) as usize]
            [(offset[2] + 1) as usize];
        self.cells[slot].as_deref()
    }
}

/// Result of [`build_and_pack_section_mesh`]: the fully packed, upload-ready
/// mesh for one [`SectionMeshInput`]. `Send` so workers can hand it back
/// through a channel.
#[derive(Debug, PartialEq)]
pub struct BuiltSectionMesh {
    /// Section coordinate the mesh was built for.
    pub coord: SectionCoord,
    /// Copied from [`SectionMeshInput::revision`].
    pub revision: u64,
    /// 16-byte packed vertices of every non-translucent quad (opaque,
    /// cutout, non-water fluid), grouped into the seven direction buckets and
    /// packed four vertices per quad in bucket order. Quads whose greedy-mesh
    /// diagonal flag was set are stored rotated one slot
    /// (`[v0, v1, v2, v3] -> [v1, v2, v3, v0]`, bit 17 cleared) so consumers
    /// can always index with the canonical static `[0, 1, 2, 0, 2, 3]`
    /// pattern.
    pub opaque_packed: Vec<PackedVertex>,
    /// `(first_vertex, vertex_count)` span of each direction bucket inside
    /// [`Self::opaque_packed`], scaled by four vertices per quad — the
    /// packed-vertex analogue of [`DirectionBuckets::vertex_ranges`].
    pub opaque_bucket_ranges: [(u32, u32); DIRECTION_BUCKETS],
    /// Translucent and water quads in original mesh order, kept as [`Quad`]s
    /// so the camera-driven back-to-front re-sort still applies.
    pub translucent_quads: Vec<Quad>,
    /// `Direction::bit` mask of every face direction present in the meshed
    /// section (opaque and translucent), for whole-section culling.
    pub visibility_mask: u8,
}

/// Appends `quad`'s four packed vertices, normalizing the diagonal flip:
/// `diagonal != 0` quads rotate `[v0, v1, v2, v3] -> [v1, v2, v3, v0]` and
/// clear attribute bit 17, preserving the exact triangulation under the
/// canonical `[0, 1, 2, 0, 2, 3]` index pattern.
fn push_normalized_packed_quad(quad: &Quad, out: &mut Vec<PackedVertex>) {
    let mut verts = pack_quad(quad);
    if quad.diagonal != 0 {
        verts.rotate_left(1);
        for vertex in &mut verts {
            vertex.attributes &= !(1 << 17);
        }
    }
    out.extend_from_slice(&verts);
}

/// Pure, stateless section mesher: builds the same [`Quad`] stream as
/// [`MeshCache::build`], then performs the direction-bucket grouping, 16-byte
/// packing (with diagonal-flip normalization) and translucent split in one
/// pass. Holds no shared mutable state, so N workers can call it concurrently
/// on independent [`SectionMeshInput`] snapshots.
pub fn build_and_pack_section_mesh(input: &SectionMeshInput) -> BuiltSectionMesh {
    let coord = input.coord;
    let mut out = BuiltSectionMesh {
        coord,
        revision: input.revision,
        opaque_packed: Vec::new(),
        opaque_bucket_ranges: [(0, 0); DIRECTION_BUCKETS],
        translucent_quads: Vec::new(),
        visibility_mask: 0,
    };
    let Some(center) = input.cells[0].as_deref() else {
        return out;
    };
    // `Uniform` centers hold no dense array; materialize one for this pass so
    // `SectionView` keeps its flat-block layout, bit-identical to `build`.
    let expanded;
    let blocks: &[Block; SECTION_CELLS] = match &center.blocks {
        SectionBlocks::Dense(dense) => dense,
        SectionBlocks::Uniform(block) => {
            expanded = Box::new([*block; SECTION_CELLS]);
            &expanded
        }
    };
    let (borders, edges) =
        sample_neighbor_halo(coord, input.missing, |_, offset| input.cell_at_offset(offset));
    let view = SectionView {
        blocks,
        block_light: &center.block_light,
        sky_light: &center.sky_light,
        borders: borders.each_ref().map(Some),
        edges_and_corners: Some(&edges),
        missing: input.missing,
    };
    let quads = mesh_section(&view);

    let mut opaque_counts = [0u32; DIRECTION_BUCKETS];
    let mut opaque_total = 0usize;
    for quad in &quads {
        out.visibility_mask |= quad.direction.bit();
        if is_translucent(quad) {
            out.translucent_quads.push(*quad);
        } else {
            opaque_counts[DirectionBuckets::index_of(quad.direction)] += 1;
            opaque_total += 1;
        }
    }
    let buckets = DirectionBuckets::laid_out(opaque_counts, 0);
    out.opaque_bucket_ranges = buckets.vertex_ranges();
    out.opaque_packed = Vec::with_capacity(opaque_total * 4);
    for bucket in 0..DIRECTION_BUCKETS {
        for quad in quads.iter().filter(|quad| {
            !is_translucent(quad) && DirectionBuckets::index_of(quad.direction) == bucket
        }) {
            push_normalized_packed_quad(quad, &mut out.opaque_packed);
        }
    }
    out
}

/// Samples the 6 face borders and 12 edge + 8 corner halo cells around `coord`
/// from whichever neighbors `section_at` resolves, filling every unresolved
/// cell with the `missing` policy sample — the exact halo [`MeshCache::build`]
/// and [`build_and_pack_section_mesh`] share so both produce identical quads.
fn sample_neighbor_halo<'a>(
    coord: SectionCoord,
    missing: MissingNeighbor,
    mut section_at: impl FnMut(SectionCoord, [i32; 3]) -> Option<&'a CompressedSection>,
) -> ([[Sample; FACE_CELLS]; 6], [Sample; EDGE_CORNER_CELLS]) {
    let mut borders = [[missing.sample(); FACE_CELLS]; 6];
    let mut edges = [missing.sample(); EDGE_CORNER_CELLS];
    for (neighbor, offset) in coord.neighbors() {
        let Some(section) = section_at(neighbor, offset) else {
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
    (borders, edges)
}

/// Default camera displacement, in blocks, that re-triggers a translucent
/// sort. Below it the previous back-to-front order is kept.
pub const DEFAULT_RESORT_THRESHOLD: f32 = 1.0;

/// Player radius, in world blocks, that qualifies a dirty section for the
/// Priority-0 immediate rebuild lane
/// ([`MeshCache::rebuild_dirty_priority_zero`]). Edits this close to the
/// camera must land in the same frame so mining never shows an X-ray flicker.
pub const PRIORITY_ZERO_NEAR_RADIUS_BLOCKS: f32 = 16.0;

/// Section metadata kept on the CPU after
/// [`MeshCache::release_uploaded_opaque_cpu_mesh`] drops the opaque quad
/// payload. Enough for frustum/occlusion bookkeeping and debug HUDs while the
/// VRAM copy stays authoritative.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ReleasedMeshMeta {
    /// Total quads the uploaded mesh had before the opaque payload dropped.
    pub quad_count: usize,
    /// Total uploaded vertex count (`quad_count * 4`).
    pub vertex_count: usize,
    /// Total uploaded index count (`quad_count * 6`).
    pub index_count: usize,
    /// `Direction::bit` mask of face directions present in the mesh.
    pub direction_mask: u8,
    /// Minimum corner of the mesh's local AABB.
    pub aabb_min: [f32; 3],
    /// Maximum corner of the mesh's local AABB.
    pub aabb_max: [f32; 3],
    /// Translucent/water quads still held for camera-driven re-sorts.
    pub kept_translucent_quads: usize,
}

/// Owns section snapshots and meshes. Repeated changes are coalesced until
/// rebuilding. Use one cache per world or dimension.
///
/// Sections are stored as [`CompressedSection`]: fully uniform sections cost
/// zero block-array heap bytes. [`take_dirty_mesh_coords`](Self::take_dirty_mesh_coords)
/// reports exactly the sections rebuilt (or re-sorted) since the last call,
/// so consumers never scan the whole world to find GPU work.
pub struct MeshCache {
    /// `Arc` storage lets [`snapshot_section_input`](Self::snapshot_section_input)
    /// hand out lock-free [`SectionMeshInput`] jobs with zero voxel copies;
    /// [`set_cell`](Self::set_cell) copy-on-writes through `Arc::make_mut`.
    sections: HashMap<SectionCoord, Arc<CompressedSection>>,
    meshes: HashMap<SectionCoord, Vec<Quad>>,
    dirty: BTreeSet<SectionCoord>,
    /// FIFO queue of coordinates whose mesh was rebuilt or re-sorted and not
    /// yet collected through `take_dirty_mesh_coords`. `rebuilt_pending`
    /// deduplicates so each coordinate appears at most once.
    rebuilt: VecDeque<SectionCoord>,
    rebuilt_pending: HashSet<SectionCoord>,
    /// Metadata of sections whose opaque CPU quads were released after
    /// upload; cleared when the section is rebuilt, replaced or removed.
    released_meta: HashMap<SectionCoord, ReleasedMeshMeta>,
    missing: MissingNeighbor,
    /// Camera position of the last committed translucent sort.
    sort_origin: Option<[f32; 3]>,
    /// Camera displacement in blocks that forces the next sort.
    resort_threshold: f32,
    /// Admission allowance for sections passed to the render upload path.
    upload_budget: ChunkUploadBudget,
}

impl Default for MeshCache {
    fn default() -> Self {
        Self {
            sections: HashMap::new(),
            meshes: HashMap::new(),
            dirty: BTreeSet::new(),
            rebuilt: VecDeque::new(),
            rebuilt_pending: HashSet::new(),
            released_meta: HashMap::new(),
            missing: MissingNeighbor::default(),
            sort_origin: None,
            resort_threshold: DEFAULT_RESORT_THRESHOLD,
            upload_budget: ChunkUploadBudget::default(),
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
    ///
    /// The snapshot is stored as a [`CompressedSection`]: a fully uniform
    /// section collapses to zero heap block storage.
    pub fn insert(&mut self, coord: SectionCoord, section: Section) -> bool {
        self.insert_compressed(coord, CompressedSection::from(section))
    }

    /// [`insert`](Self::insert) for an already-compressed snapshot, skipping
    /// the dense-to-uniform collapse scan when `section` is
    /// [`SectionBlocks::Uniform`].
    pub fn insert_compressed(&mut self, coord: SectionCoord, section: CompressedSection) -> bool {
        let old = self.sections.get(&coord);
        if old.is_some_and(|old| **old == section) {
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
        self.sections.insert(coord, Arc::new(section));
        true
    }

    pub fn remove(&mut self, coord: SectionCoord) -> bool {
        let Some(old) = self.sections.remove(&coord) else {
            return false;
        };
        self.meshes.remove(&coord);
        self.dirty.remove(&coord);
        self.released_meta.remove(&coord);
        if self.rebuilt_pending.remove(&coord) {
            self.rebuilt.retain(|&queued| queued != coord);
        }
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
        if !Arc::make_mut(section).set(x, y, z, block, light) {
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
    ///
    /// After [`release_uploaded_opaque_cpu_mesh`](Self::release_uploaded_opaque_cpu_mesh)
    /// the slice holds only the translucent quads kept for re-sorting; the
    /// uploaded opaque geometry is authoritative in VRAM and
    /// [`released_mesh_meta`](Self::released_mesh_meta) keeps its counts.
    pub fn mesh(&self, coord: SectionCoord) -> Option<&[Quad]> {
        if self.dirty.contains(&coord) {
            return None;
        }
        self.meshes.get(&coord).map(Vec::as_slice)
    }

    /// The stored section snapshot, in [`CompressedSection`] form.
    pub fn section(&self, coord: SectionCoord) -> Option<&CompressedSection> {
        self.sections.get(&coord).map(|arc| &**arc)
    }

    /// Whether the section's block storage is the zero-heap uniform form.
    pub fn is_uniform_section(&self, coord: SectionCoord) -> Option<bool> {
        self.sections.get(&coord).map(|arc| arc.is_uniform())
    }

    /// Heap bytes held by the section's block storage (`0` when uniform).
    pub fn blocks_heap_bytes(&self, coord: SectionCoord) -> Option<usize> {
        self.sections.get(&coord).map(|arc| arc.heap_bytes())
    }

    pub fn pending(&self) -> usize {
        self.dirty.len()
    }

    /// Number of sections currently tracked (voxels held).
    pub fn sections_loaded(&self) -> usize {
        self.sections.len()
    }

    /// Enqueues a rebuilt/re-sorted coordinate for the next
    /// [`take_dirty_mesh_coords`](Self::take_dirty_mesh_coords), deduplicated.
    fn note_rebuilt(&mut self, coord: SectionCoord) {
        if self.rebuilt_pending.insert(coord) {
            self.rebuilt.push_back(coord);
        }
    }

    /// Drains the queue of coordinates whose mesh changed since the last
    /// call — every section `rebuild_dirty`, `rebuild_dirty_ordered`,
    /// `rebuild_dirty_priority_zero` or `resort_translucent` produced new GPU
    /// geometry for. Each coordinate is reported at most once no matter how
    /// often it was rebuilt, so this is O(rebuilt) instead of the
    /// O(all loaded sections) scan and vertex-slice compare it replaces.
    pub fn take_dirty_mesh_coords(&mut self) -> Vec<SectionCoord> {
        self.rebuilt_pending.clear();
        self.rebuilt.drain(..).collect()
    }

    /// Feed the windowed frame-time P95 measured by the render loop. This
    /// changes the upload count admitted on subsequent ordered rebuilds.
    pub fn report_frame_time_p95_us(&mut self, p95_us: f64) {
        self.upload_budget.adapt(p95_us);
    }

    /// Current adaptive chunk-mesh upload allowance.
    pub fn upload_budget(&self) -> &ChunkUploadBudget {
        &self.upload_budget
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
            self.released_meta.remove(&coord);
            self.note_rebuilt(coord);
            rebuilt.push(coord);
        }
        rebuilt
    }

    /// Rebuilds at most `limit` dirty sections, in-frustum and near-camera
    /// first, subject to the per-call mesh upload budget. Each invocation
    /// opens a new frame's upload allowance. Rejected sections remain dirty
    /// for the next invocation and retain their priority.
    /// `camera` is a world block position and `planes` the frustum planes
    /// in `box_in_frustum` form. Sections outside the frustum still rebuild,
    /// but only after every closer in-frustum section.
    pub fn rebuild_dirty_ordered(
        &mut self,
        limit: usize,
        camera: [f32; 3],
        planes: &[[f32; 4]],
    ) -> Vec<SectionCoord> {
        self.upload_budget.begin_frame();
        let normal_byte_cap = self.upload_budget.max_upload_bytes_per_frame;
        let normal_work_cap = self.upload_budget.max_upload_work_us_per_frame;
        let mut order: Vec<SectionCoord> = self.dirty.iter().copied().collect();
        order.sort_by(|a, b| {
            Self::queue_key(*a, camera, planes).cmp(&Self::queue_key(*b, camera, planes))
        });
        let mut rebuilt = Vec::new();
        for coord in order {
            if rebuilt.len() >= limit || self.upload_budget.remaining_uploads() == 0 {
                break;
            }
            let mesh = self.build(coord);
            // RenderRegion uploads four packed vertices and six u32 indices
            // for each quad. Budget the actual payload rather than the in-RAM
            // size of a Quad, with a conservative CPU transfer estimate.
            let bytes_per_quad = 4 * std::mem::size_of::<crate::packed::PackedVertex>()
                + 6 * std::mem::size_of::<u32>();
            let bytes = mesh.len().saturating_mul(bytes_per_quad);
            let cost_us = u64::try_from(bytes.div_ceil(2_048)).unwrap_or(u64::MAX);
            // A single oversized section must still make forward progress.
            // Admit it as the sole upload when its payload exceeds the usual
            // cap, charging the full payload to this frame.
            if self.upload_budget.used_uploads() == 0 {
                self.upload_budget.max_upload_bytes_per_frame =
                    self.upload_budget.max_upload_bytes_per_frame.max(bytes);
                self.upload_budget.max_upload_work_us_per_frame =
                    self.upload_budget.max_upload_work_us_per_frame.max(cost_us);
            }
            if !self.upload_budget.try_schedule(bytes, cost_us) {
                continue;
            }
            self.dirty.remove(&coord);
            self.meshes.insert(coord, mesh);
            self.released_meta.remove(&coord);
            self.note_rebuilt(coord);
            rebuilt.push(coord);
        }
        self.upload_budget.max_upload_bytes_per_frame = normal_byte_cap;
        self.upload_budget.max_upload_work_us_per_frame = normal_work_cap;
        rebuilt
    }

    /// Priority-0 rebuild lane: rebuilds up to `max_near` dirty sections
    /// whose boxes come within `near_radius_blocks` of `camera` (the player's
    /// block position), nearest first, and leaves every farther section dirty
    /// for the normal budgeted path. Call it before any distant chunk work
    /// each frame — a single near-section mesh is `~150 µs`, so blocks the
    /// player just broke or placed update in the same frame with no X-ray
    /// flicker. [`PRIORITY_ZERO_NEAR_RADIUS_BLOCKS`] is the recommended
    /// radius. Returns the rebuilt coordinates.
    pub fn rebuild_dirty_priority_zero(
        &mut self,
        camera: [f32; 3],
        near_radius_blocks: f32,
        max_near: usize,
    ) -> Vec<SectionCoord> {
        let radius2 = near_radius_blocks * near_radius_blocks;
        let mut near: Vec<(u64, SectionCoord)> = self
            .dirty
            .iter()
            .map(|&coord| (Self::camera_distance2(coord, camera), coord))
            .filter(|&(dist2, _)| dist2 <= radius2.max(0.0).to_bits() as u64)
            .collect();
        near.sort_unstable_by_key(|&(dist2, coord)| (dist2, coord));
        let mut rebuilt = Vec::new();
        for (_, coord) in near.into_iter().take(max_near) {
            self.dirty.remove(&coord);
            let mesh = self.build(coord);
            self.meshes.insert(coord, mesh);
            self.released_meta.remove(&coord);
            self.note_rebuilt(coord);
            rebuilt.push(coord);
        }
        rebuilt
    }

    /// Squared distance from `camera` to the nearest point of `coord`'s
    /// section box, as `f32` bits so it can order a sort.
    fn camera_distance2(coord: SectionCoord, camera: [f32; 3]) -> u64 {
        let origin = [coord.0, coord.1, coord.2];
        let mut dist2 = 0.0f32;
        for (axis, &p) in camera.iter().enumerate() {
            let lo = origin[axis] as f32 * EDGE as f32;
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
        dist2.max(0.0).to_bits() as u64
    }

    /// Drops every chunk column outside Chebyshev distance `radius_chunks`
    /// (typically `view_distance + 2`) of (`center_cx`, `center_cz`), removing
    /// both the voxels and the cached mesh via [`remove`](Self::remove) and
    /// marking the surviving boundary neighbors dirty exactly like an unload.
    /// Returns the number of evicted chunk columns.
    pub fn evict_chunks_outside_radius(
        &mut self,
        center_cx: i32,
        center_cz: i32,
        radius_chunks: i32,
    ) -> usize {
        let mut columns = BTreeSet::new();
        let radius = i64::from(radius_chunks);
        let evicted: Vec<SectionCoord> = self
            .sections
            .keys()
            .filter(|coord| {
                (i64::from(coord.0) - i64::from(center_cx)).abs() > radius
                    || (i64::from(coord.2) - i64::from(center_cz)).abs() > radius
            })
            .copied()
            .collect();
        for coord in evicted {
            columns.insert((coord.0, coord.2));
            self.remove(coord);
        }
        columns.len()
    }

    /// Drops the CPU copy of a section's opaque geometry after it has been
    /// uploaded to VRAM (plan §3.1 + §4 guard ②). The `Vec<Quad>` keeps only
    /// translucent/water quads — everything `is_translucent` re-sorts needs —
    /// while quad/vertex/index counts, the local AABB and the face-direction
    /// mask move into [`released_mesh_meta`](Self::released_mesh_meta). A no-op
    /// returning `0` for missing, dirty or already-released sections.
    /// Returns the freed CPU bytes.
    pub fn release_uploaded_opaque_cpu_mesh(&mut self, coord: SectionCoord) -> usize {
        if self.dirty.contains(&coord) {
            return 0;
        }
        let Some(mesh) = self.meshes.get_mut(&coord) else {
            return 0;
        };
        let mut meta = ReleasedMeshMeta {
            quad_count: mesh.len(),
            vertex_count: mesh.len() * 4,
            index_count: mesh.len() * 6,
            direction_mask: 0,
            aabb_min: [f32::INFINITY; 3],
            aabb_max: [f32::NEG_INFINITY; 3],
            kept_translucent_quads: 0,
        };
        for quad in mesh.iter() {
            meta.direction_mask |= quad.direction.bit();
            for corner in quad.positions {
                for (axis, &value) in corner.iter().enumerate() {
                    meta.aabb_min[axis] = meta.aabb_min[axis].min(value);
                    meta.aabb_max[axis] = meta.aabb_max[axis].max(value);
                }
            }
        }
        if mesh.is_empty() {
            meta.aabb_min = [0.0; 3];
            meta.aabb_max = [0.0; 3];
        }
        let before = mesh.len();
        mesh.retain(is_translucent);
        meta.kept_translucent_quads = mesh.len();
        let dropped = before - mesh.len();
        if dropped == 0 && self.released_meta.contains_key(&coord) {
            return 0;
        }
        mesh.shrink_to_fit();
        self.released_meta.insert(coord, meta);
        dropped * std::mem::size_of::<Quad>()
    }

    /// Metadata preserved by
    /// [`release_uploaded_opaque_cpu_mesh`](Self::release_uploaded_opaque_cpu_mesh),
    /// `None` while the full CPU mesh is still held.
    pub fn released_mesh_meta(&self, coord: SectionCoord) -> Option<&ReleasedMeshMeta> {
        self.released_meta.get(&coord)
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
        for &coord in &changed {
            self.note_rebuilt(coord);
        }
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

    /// Captures a lock-free mesh-job snapshot for `coord`: the center section
    /// plus its 26 neighbors as `Arc` clones (about 27 reference counts, zero
    /// voxel copies). Returns `None` when the center section is not loaded.
    /// Pair with [`build_and_pack_section_mesh`] on a worker thread; tag the
    /// result with `revision` and discard it if the world's revision moved on.
    pub fn snapshot_section_input(
        &self,
        coord: SectionCoord,
        revision: u64,
    ) -> Option<SectionMeshInput> {
        let center = self.sections.get(&coord)?;
        let mut cells = std::array::from_fn(|_| None);
        cells[0] = Some(Arc::clone(center));
        for (slot, offset) in NEIGHBOR_OFFSETS.iter().enumerate().skip(1) {
            if let Some(neighbor) = coord
                .offset(*offset)
                .and_then(|neighbor| self.sections.get(&neighbor))
            {
                cells[slot] = Some(Arc::clone(neighbor));
            }
        }
        Some(SectionMeshInput {
            coord,
            revision,
            missing: self.missing,
            cells,
        })
    }

    fn build(&self, coord: SectionCoord) -> Vec<Quad> {
        let stored = &self.sections[&coord];
        // `Uniform` sections hold no dense array; materialize one just for
        // this meshing pass so `SectionView` keeps its flat-block layout and
        // the mesher output is bit-identical to a dense snapshot.
        let expanded;
        let blocks: &[Block; SECTION_CELLS] = match &stored.blocks {
            SectionBlocks::Dense(dense) => dense,
            SectionBlocks::Uniform(block) => {
                expanded = Box::new([*block; SECTION_CELLS]);
                &expanded
            }
        };
        let (borders, edges) = sample_neighbor_halo(coord, self.missing, |neighbor, _| {
            self.sections.get(&neighbor).map(|arc| &**arc)
        });
        let view = SectionView {
            blocks,
            block_light: &stored.block_light,
            sky_light: &stored.sky_light,
            borders: borders.each_ref().map(Some),
            edges_and_corners: Some(&edges),
            missing: self.missing,
        };
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
    fn ordered_rebuild_retries_deferred_uploads_and_adapts_to_frame_p95() {
        let mut cache = MeshCache::default();
        for x in 0..17 {
            cache.insert(
                SectionCoord(x * 2, 0, 0),
                Section::filled(Block::AIR, Light::new(0, 15)),
            );
        }

        let rebuilt = cache.rebuild_dirty_ordered(17, [0.0; 3], &[]);
        assert_eq!(rebuilt.len(), 8);
        assert_eq!(cache.upload_budget().used_uploads(), 8);
        assert_eq!(cache.pending(), 9);
        assert!(rebuilt.iter().all(|&coord| cache.mesh(coord).is_some()));

        cache.report_frame_time_p95_us(30_000.0);
        assert_eq!(cache.upload_budget().uploads_per_frame(), 4);
        let rebuilt = cache.rebuild_dirty_ordered(17, [0.0; 3], &[]);
        assert_eq!(rebuilt.len(), 4);
        assert_eq!(cache.pending(), 5);

        cache.report_frame_time_p95_us(5_000.0);
        assert_eq!(cache.upload_budget().uploads_per_frame(), 5);
        let rebuilt = cache.rebuild_dirty_ordered(17, [0.0; 3], &[]);
        assert_eq!(rebuilt.len(), 5);
        assert_eq!(cache.pending(), 0);
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
