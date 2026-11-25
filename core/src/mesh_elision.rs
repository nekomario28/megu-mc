// SPDX-License-Identifier: LGPL-3.0-or-later
#![forbid(unsafe_code)]
//! Mesh-rebuild elision, content-addressed mesh sharing, and frame-paced GPU
//! upload scheduling for `16³` chunk sections.
//!
//! The occlusion path is deliberately fail-closed for modded rendering: a
//! mutation is elided only when both the old and new block are certified as
//! simple full opaque cubes, the cell is strictly interior, every face-adjacent
//! neighbor is a strict occluder, and no neighbor has a neighbor-dependent
//! visual model. Protruding models, connected textures, light-affecting blocks,
//! dynamic block-entity renderers, and uncertified blocks all take the normal
//! remesh path.

use std::collections::{BTreeMap, BinaryHeap};
use std::sync::Arc;

use crate::bloom::VoxelBitset4096;
use crate::coords::SectionKey;
use crate::dep_graph::{CutoffOutcome, DagError, IncrementalDag, NodeId};
use crate::hash::{crc32_ieee, crc32_ieee_update, fnv1a64, Fnv1a64};

/// Bitmask for the `-X` section face in [`RemeshDecision::SectionAndBorderNeighbors`].
pub const FACE_NEG_X: u8 = 1 << 0;
/// Bitmask for the `+X` section face in [`RemeshDecision::SectionAndBorderNeighbors`].
pub const FACE_POS_X: u8 = 1 << 1;
/// Bitmask for the `-Y` section face in [`RemeshDecision::SectionAndBorderNeighbors`].
pub const FACE_NEG_Y: u8 = 1 << 2;
/// Bitmask for the `+Y` section face in [`RemeshDecision::SectionAndBorderNeighbors`].
pub const FACE_POS_Y: u8 = 1 << 3;
/// Bitmask for the `-Z` section face in [`RemeshDecision::SectionAndBorderNeighbors`].
pub const FACE_NEG_Z: u8 = 1 << 4;
/// Bitmask for the `+Z` section face in [`RemeshDecision::SectionAndBorderNeighbors`].
pub const FACE_POS_Z: u8 = 1 << 5;
/// All six section-face bits.
pub const ALL_FACES: u8 =
    FACE_NEG_X | FACE_POS_X | FACE_NEG_Y | FACE_POS_Y | FACE_NEG_Z | FACE_POS_Z;

/// Certified rendering properties of one block state.
///
/// The bits are affirmative certifications, not guesses. In particular an
/// uncertified mod block must carry [`BlockRenderTraits::UNKNOWN_CONSERVATIVE`]
/// and is never eligible for remesh elision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockRenderTraits(pub u32);

impl Default for BlockRenderTraits {
    /// Uncertified blocks are conservative by default rather than accidentally elidable.
    fn default() -> Self {
        Self::UNKNOWN_CONSERVATIVE
    }
}

impl BlockRenderTraits {
    /// Fills exactly `[0,1]³`, occludes all six neighboring faces, and has no overhang.
    pub const FULL_OPAQUE_CUBE: Self = Self(1 << 0);
    /// The visual model or collision-relevant renderer extends outside `[0,1]³`.
    pub const PROTRUDING_BOUNDS: Self = Self(1 << 1);
    /// The visual mesh depends on neighboring blocks (CTM, fences, walls, stairs, redstone).
    pub const NEIGHBOR_DEPENDENT_VISUAL: Self = Self(1 << 2);
    /// The block emits light or changes light transmission.
    pub const LIGHT_AFFECTING: Self = Self(1 << 3);
    /// The block has a dynamic/animated/beam/particle block-entity renderer.
    pub const DYNAMIC_RENDERER: Self = Self(1 << 4);
    /// The block has not been certified; use the conservative path.
    pub const UNKNOWN_CONSERVATIVE: Self = Self(1 << 5);
    /// Explicit empty traits, suitable for air or another certified non-occluding simple block.
    pub const EMPTY: Self = Self(0);

    /// Raw bit representation.
    #[inline]
    #[must_use]
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Whether every bit in `other` is present.
    #[inline]
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Union of two trait sets.
    #[inline]
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Strict occluder contract: certified full opaque cube with no overhang,
    /// light effect, dynamic renderer, or unknown fallback. A neighbor-dependent
    /// full cube may still occlude geometrically, but it also blocks elision
    /// through the separate neighbor-sensitive test.
    #[inline]
    #[must_use]
    pub const fn is_strict_occluder(self) -> bool {
        let disqualified = Self::PROTRUDING_BOUNDS.0
            | Self::LIGHT_AFFECTING.0
            | Self::DYNAMIC_RENDERER.0
            | Self::UNKNOWN_CONSERVATIVE.0;
        self.0 & Self::FULL_OPAQUE_CUBE.0 != 0 && self.0 & disqualified == 0
    }

    /// Whether this exact block may participate in interior mutation elision.
    /// This is stricter than [`Self::is_strict_occluder`]: connected or
    /// otherwise neighbor-dependent visuals make a buried change visible.
    #[inline]
    #[must_use]
    pub const fn is_elidable_interior_block(self) -> bool {
        self.is_strict_occluder() && self.0 & Self::NEIGHBOR_DEPENDENT_VISUAL.0 == 0
    }

    /// Whether the block's renderer may cross a cell boundary or change independently of the mesh.
    #[inline]
    #[must_use]
    pub const fn is_protruding_or_dynamic(self) -> bool {
        self.0 & (Self::PROTRUDING_BOUNDS.0 | Self::DYNAMIC_RENDERER.0) != 0
    }
}

impl From<bool> for BlockRenderTraits {
    /// Compatibility mapping for callers that only track opaque occupancy:
    /// `true` is a certified ordinary full opaque cube and `false` is empty.
    /// Modded renderers should pass explicit [`BlockRenderTraits`] instead.
    fn from(opaque: bool) -> Self {
        if opaque {
            Self::FULL_OPAQUE_CUBE
        } else {
            Self::EMPTY
        }
    }
}

/// Remesh work required after one section-local block mutation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemeshDecision {
    /// The visible mesh is bit-identical; remesh computation is eliminated.
    Elide,
    /// Only the containing section needs remeshing.
    LocalSectionOnly,
    /// The containing section plus border-neighbor sections need remeshing.
    /// Bits use the `FACE_*` constants.
    SectionAndBorderNeighbors(u8),
}

#[inline]
fn voxel_index(x: u8, y: u8, z: u8) -> Option<u16> {
    if x >= 16 || y >= 16 || z >= 16 {
        return None;
    }
    Some((u16::from(y) << 8) | (u16::from(z) << 4) | u16::from(x))
}

/// Face bits for a mutation `radius` cells away from a section boundary.
/// Out-of-range input is conservatively treated as touching every face.
#[must_use]
pub fn border_mask_for_mutation(x: u8, y: u8, z: u8, radius: u8) -> u8 {
    if x >= 16 || y >= 16 || z >= 16 {
        return ALL_FACES;
    }
    let radius = radius.clamp(1, 16);
    let high_start = 16u8.saturating_sub(radius);
    let mut mask = 0u8;
    if x < radius {
        mask |= FACE_NEG_X;
    }
    if x >= high_start {
        mask |= FACE_POS_X;
    }
    if y < radius {
        mask |= FACE_NEG_Y;
    }
    if y >= high_start {
        mask |= FACE_POS_Y;
    }
    if z < radius {
        mask |= FACE_NEG_Z;
    }
    if z >= high_start {
        mask |= FACE_POS_Z;
    }
    mask
}

/// Exact per-section render-trait occupancy used by the occlusion classifier.
///
/// `strict_occluders` contains only certified non-protruding full opaque cubes.
/// `neighbor_sensitive` contains CTM/connected-model blocks. `protruding_or_dynamic`
/// contains blocks whose visual influence may escape the voxel cell. The mask is
/// pre-mutation state: call [`Self::classify_section_mutation`] before writing
/// the new traits into it.
#[derive(Clone)]
pub struct SectionOcclusionMask {
    strict_occluders: VoxelBitset4096,
    neighbor_sensitive: VoxelBitset4096,
    protruding_or_dynamic: VoxelBitset4096,
    check_26_neighbors: bool,
    protrusion_radius_cells: u8,
}

impl Default for SectionOcclusionMask {
    fn default() -> Self {
        Self::new()
    }
}

impl SectionOcclusionMask {
    /// Empty section mask using six-face CTM rejection and a one-cell protrusion radius.
    #[must_use]
    pub fn new() -> Self {
        Self {
            strict_occluders: VoxelBitset4096::new(),
            neighbor_sensitive: VoxelBitset4096::new(),
            protruding_or_dynamic: VoxelBitset4096::new(),
            check_26_neighbors: false,
            protrusion_radius_cells: 1,
        }
    }

    /// Compatibility setter for simple opaque occupancy. `opaque = true` certifies
    /// a vanilla-style full opaque cube; use [`Self::set_render_traits`] whenever
    /// modded render traits are known.
    pub fn set_opaque(&mut self, x: u8, y: u8, z: u8, opaque: bool) {
        self.set_render_traits(x, y, z, BlockRenderTraits::from(opaque));
    }

    /// Set the certified render traits of one section-local cell. Out-of-range
    /// coordinates are ignored.
    pub fn set_render_traits(&mut self, x: u8, y: u8, z: u8, traits: BlockRenderTraits) {
        let Some(index) = voxel_index(x, y, z) else {
            return;
        };
        if traits.is_strict_occluder() {
            self.strict_occluders.set(index);
        } else {
            self.strict_occluders.clear(index);
        }
        if traits.contains(BlockRenderTraits::NEIGHBOR_DEPENDENT_VISUAL) {
            self.neighbor_sensitive.set(index);
        } else {
            self.neighbor_sensitive.clear(index);
        }
        if traits.is_protruding_or_dynamic() {
            self.protruding_or_dynamic.set(index);
        } else {
            self.protruding_or_dynamic.clear(index);
        }
    }

    /// Whether a cell is a strict geometric occluder.
    #[must_use]
    pub fn is_strict_occluder_at(&self, x: u8, y: u8, z: u8) -> bool {
        voxel_index(x, y, z).is_some_and(|index| self.strict_occluders.get(index))
    }

    /// Whether a cell has a neighbor-dependent visual model.
    #[must_use]
    pub fn is_neighbor_sensitive_at(&self, x: u8, y: u8, z: u8) -> bool {
        voxel_index(x, y, z).is_some_and(|index| self.neighbor_sensitive.get(index))
    }

    /// Whether a cell may protrude or render dynamically.
    #[must_use]
    pub fn is_protruding_or_dynamic_at(&self, x: u8, y: u8, z: u8) -> bool {
        voxel_index(x, y, z).is_some_and(|index| self.protruding_or_dynamic.get(index))
    }

    /// Enable or disable the conservative 26-neighbor connected-texture check.
    /// The default six-face check already rejects ordinary CTM blocks; the
    /// 26-neighbor mode additionally covers corner-connected models.
    pub fn set_corner_ctm_checks_enabled(&mut self, enabled: bool) {
        self.check_26_neighbors = enabled;
    }

    /// Whether the conservative 26-neighbor connected-texture check is enabled.
    #[must_use]
    pub const fn corner_ctm_checks_enabled(&self) -> bool {
        self.check_26_neighbors
    }

    /// Set the conservative protrusion radius in whole cells. Values are clamped
    /// to `1..=16`; one cell is the correct minimum because any overhang may
    /// cross the immediate section boundary.
    pub fn set_protrusion_radius_cells(&mut self, radius: u8) {
        self.protrusion_radius_cells = radius.clamp(1, 16);
    }

    /// Configured protrusion radius in whole cells.
    #[must_use]
    pub const fn protrusion_radius_cells(&self) -> u8 {
        self.protrusion_radius_cells
    }

    /// Whether `(x,y,z)` is strictly interior and all six face-adjacent neighbors
    /// are strict occluders.
    #[must_use]
    pub fn is_interior_fully_occluded(&self, x: u8, y: u8, z: u8) -> bool {
        is_strictly_interior(x, y, z) && self.face_neighbors_are_strict_occluders(x, y, z)
    }

    /// Compatibility form of remesh elision for callers using opaque booleans.
    /// Boolean `true` means a certified ordinary full opaque cube. Callers with
    /// modded trait information must use [`Self::can_elide_remesh_traits`] or
    /// [`Self::classify_section_mutation`] so protrusions, CTM, light and dynamic
    /// renderers cannot be erased by the type conversion.
    #[must_use]
    pub fn can_elide_remesh(&self, x: u8, y: u8, z: u8, was_opaque: bool, is_opaque: bool) -> bool {
        self.can_elide_remesh_traits(
            x,
            y,
            z,
            BlockRenderTraits::from(was_opaque),
            BlockRenderTraits::from(is_opaque),
        )
    }

    /// Whether changing one cell from `old_traits` to `new_traits` is guaranteed
    /// to leave every visible face bit-identical. `self` must still describe the
    /// pre-mutation section.
    #[must_use]
    pub fn can_elide_remesh_traits(
        &self,
        x: u8,
        y: u8,
        z: u8,
        old_traits: BlockRenderTraits,
        new_traits: BlockRenderTraits,
    ) -> bool {
        if !old_traits.is_elidable_interior_block() || !new_traits.is_elidable_interior_block() {
            return false;
        }
        if !is_strictly_interior(x, y, z) || !self.face_neighbors_are_strict_occluders(x, y, z) {
            return false;
        }
        let Some(index) = voxel_index(x, y, z) else {
            return false;
        };
        // The pre-mutation cell itself must have been eligible too. This catches
        // callers that pass traits inconsistent with the mask they classified against.
        if !self.strict_occluders.get(index) || self.neighbor_sensitive.get(index) {
            return false;
        }
        !self.neighbor_sensitive_nearby(x, y, z)
    }

    /// Classify a mutation before writing `new_visual` into `prev`. `new_visual`
    /// accepts either explicit [`BlockRenderTraits`] or the compatibility boolean
    /// occupancy form.
    ///
    /// Elision is returned only when the complete fail-closed contract holds.
    /// Otherwise boundary bits are exact for ordinary blocks and expanded by the
    /// configured protrusion radius when either side may protrude or render dynamically.
    #[must_use]
    pub fn classify_section_mutation<V: Into<BlockRenderTraits>>(
        prev: &Self,
        x: u8,
        y: u8,
        z: u8,
        new_visual: V,
    ) -> RemeshDecision {
        let new_traits = new_visual.into();
        let Some(index) = voxel_index(x, y, z) else {
            return RemeshDecision::SectionAndBorderNeighbors(ALL_FACES);
        };
        let old_was_elidable =
            prev.strict_occluders.get(index) && !prev.neighbor_sensitive.get(index);
        let old_traits = if old_was_elidable {
            BlockRenderTraits::FULL_OPAQUE_CUBE
        } else {
            BlockRenderTraits::UNKNOWN_CONSERVATIVE
        };
        if prev.can_elide_remesh_traits(x, y, z, old_traits, new_traits) {
            return RemeshDecision::Elide;
        }
        let had_protruding_or_dynamic = prev.protruding_or_dynamic.get(index);
        let radius = if had_protruding_or_dynamic || new_traits.is_protruding_or_dynamic() {
            prev.protrusion_radius_cells
        } else {
            1
        };
        let mask = border_mask_for_mutation(x, y, z, radius);
        if mask == 0 {
            RemeshDecision::LocalSectionOnly
        } else {
            RemeshDecision::SectionAndBorderNeighbors(mask)
        }
    }

    fn face_neighbors_are_strict_occluders(&self, x: u8, y: u8, z: u8) -> bool {
        const OFFSETS: [(i8, i8, i8); 6] = [
            (-1, 0, 0),
            (1, 0, 0),
            (0, -1, 0),
            (0, 1, 0),
            (0, 0, -1),
            (0, 0, 1),
        ];
        OFFSETS.iter().all(|&(dx, dy, dz)| {
            let Some((nx, ny, nz)) = offset_cell(x, y, z, dx, dy, dz) else {
                return false;
            };
            self.is_strict_occluder_at(nx, ny, nz)
        })
    }

    fn neighbor_sensitive_nearby(&self, x: u8, y: u8, z: u8) -> bool {
        if self.check_26_neighbors {
            for dx in -1i8..=1 {
                for dy in -1i8..=1 {
                    for dz in -1i8..=1 {
                        if dx == 0 && dy == 0 && dz == 0 {
                            continue;
                        }
                        if let Some((nx, ny, nz)) = offset_cell(x, y, z, dx, dy, dz) {
                            if self.is_neighbor_sensitive_at(nx, ny, nz) {
                                return true;
                            }
                        }
                    }
                }
            }
            false
        } else {
            const OFFSETS: [(i8, i8, i8); 6] = [
                (-1, 0, 0),
                (1, 0, 0),
                (0, -1, 0),
                (0, 1, 0),
                (0, 0, -1),
                (0, 0, 1),
            ];
            OFFSETS.iter().any(|&(dx, dy, dz)| {
                offset_cell(x, y, z, dx, dy, dz)
                    .is_some_and(|(nx, ny, nz)| self.is_neighbor_sensitive_at(nx, ny, nz))
            })
        }
    }
}

#[inline]
fn is_strictly_interior(x: u8, y: u8, z: u8) -> bool {
    (1..=14).contains(&x) && (1..=14).contains(&y) && (1..=14).contains(&z)
}

#[inline]
fn offset_cell(x: u8, y: u8, z: u8, dx: i8, dy: i8, dz: i8) -> Option<(u8, u8, u8)> {
    let nx = i16::from(x) + i16::from(dx);
    let ny = i16::from(y) + i16::from(dy);
    let nz = i16::from(z) + i16::from(dz);
    if (0..16).contains(&nx) && (0..16).contains(&ny) && (0..16).contains(&nz) {
        Some((nx as u8, ny as u8, nz as u8))
    } else {
        None
    }
}

/// Apply a meshing decision to an incremental mesh DAG.
///
/// `mesh_node` is the containing section's mesh node. `face_mesh_nodes` uses the
/// bit order of `FACE_NEG_X`, `FACE_POS_X`, `FACE_NEG_Y`, `FACE_POS_Y`,
/// `FACE_NEG_Z`, `FACE_POS_Z`; each entry supplies the neighbor mesh node and its
/// newly computed mesh digest when that face is dirty. For [`RemeshDecision::Elide`]
/// the node is verified with its unchanged stored digest, exercising the DAG's
/// early-cutoff path without issuing downstream invalidation.
pub fn apply_decision_to_dag(
    dag: &mut IncrementalDag,
    mesh_node: NodeId,
    decision: RemeshDecision,
    new_mesh_digest: u64,
    face_mesh_nodes: [Option<(NodeId, u64)>; 6],
) -> Result<CutoffOutcome, DagError> {
    match decision {
        RemeshDecision::Elide => {
            let unchanged = dag.digest(mesh_node)?;
            Ok(dag.recompute_with_cutoff(mesh_node, unchanged))
        }
        RemeshDecision::LocalSectionOnly => {
            dag.mark_input_changed(mesh_node, new_mesh_digest)?;
            Ok(CutoffOutcome::Changed)
        }
        RemeshDecision::SectionAndBorderNeighbors(mask) => {
            dag.mark_input_changed(mesh_node, new_mesh_digest)?;
            for (bit, entry) in face_mesh_nodes.iter().enumerate() {
                if mask & (1u8 << bit) == 0 {
                    continue;
                }
                if let Some((node, digest)) = entry {
                    dag.mark_input_changed(*node, *digest)?;
                }
            }
            Ok(CutoffOutcome::Changed)
        }
    }
}

/// Stable two-part content fingerprint of a section's palette and packed indices.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SectionContentFingerprint(pub u64, pub u32);

/// Compute the canonical `(fnv1a64, crc32_ieee)` section-content fingerprint.
///
/// Lengths and little-endian scalar representations are part of the preimage so
/// concatenation ambiguity cannot collapse distinct layouts into one key.
#[must_use]
pub fn compute_section_fingerprint(
    palette_ids: &[u32],
    packed_indices: &[u8],
) -> SectionContentFingerprint {
    let palette_len = palette_ids.len() as u64;
    let index_len = packed_indices.len() as u64;
    let mut prefix = [0u8; 16];
    prefix[..8].copy_from_slice(&palette_len.to_le_bytes());
    prefix[8..].copy_from_slice(&index_len.to_le_bytes());

    let mut fnv = Fnv1a64::with_seed(fnv1a64(&prefix));
    let mut crc = crc32_ieee(&prefix);
    for &id in palette_ids {
        let bytes = id.to_le_bytes();
        fnv.update(&bytes);
        crc = crc32_ieee_update(crc, &bytes);
    }
    fnv.update(packed_indices);
    crc = crc32_ieee_update(crc, packed_indices);
    SectionContentFingerprint(fnv.finish(), crc)
}

/// Bounded LRU content-hash mesh cache. Identical section content shares one
/// [`Arc`] allocation across the world; distinct content fingerprints never alias.
pub struct ContentHashMeshCache<M> {
    entries: BTreeMap<SectionContentFingerprint, (Arc<M>, u64)>,
    recency: BTreeMap<(u64, SectionContentFingerprint), ()>,
    clock: u64,
    capacity: usize,
    hits: u64,
    misses: u64,
}

impl<M> ContentHashMeshCache<M> {
    /// Empty cache retaining at most `capacity` distinct meshes. Zero disables retention.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: BTreeMap::new(),
            recency: BTreeMap::new(),
            clock: 0,
            capacity,
            hits: 0,
            misses: 0,
        }
    }

    /// Return the shared mesh for `fp`, invoking `builder` only on a miss.
    /// The boolean is `true` on a cache hit.
    pub fn get_or_build(
        &mut self,
        fp: SectionContentFingerprint,
        builder: impl FnOnce() -> M,
    ) -> (Arc<M>, bool) {
        if let Some((mesh, generation)) = self.entries.get(&fp) {
            self.hits = self.hits.saturating_add(1);
            let mesh = Arc::clone(mesh);
            let generation = *generation;
            self.recency.remove(&(generation, fp));
            self.clock = self.clock.wrapping_add(1);
            let refreshed = self.clock;
            self.entries.insert(fp, (Arc::clone(&mesh), refreshed));
            self.recency.insert((refreshed, fp), ());
            return (mesh, true);
        }
        self.misses = self.misses.saturating_add(1);
        let mesh = Arc::new(builder());
        if self.capacity != 0 {
            while self.entries.len() >= self.capacity {
                let Some((recency_key, ())) = self.recency.iter().next() else {
                    break;
                };
                let recency_key = *recency_key;
                self.recency.remove(&recency_key);
                self.entries.remove(&recency_key.1);
            }
            self.clock = self.clock.wrapping_add(1);
            let generation = self.clock;
            self.entries.insert(fp, (Arc::clone(&mesh), generation));
            self.recency.insert((generation, fp), ());
        }
        (mesh, false)
    }

    /// Number of cache hits.
    #[must_use]
    pub const fn hits(&self) -> u64 {
        self.hits
    }

    /// Number of cache misses.
    #[must_use]
    pub const fn misses(&self) -> u64 {
        self.misses
    }

    /// `hits / (hits + misses)`, or zero before the first lookup.
    #[must_use]
    pub fn dedup_ratio(&self) -> f64 {
        let total = self.hits.saturating_add(self.misses);
        if total == 0 {
            0.0
        } else {
            self.hits as f64 / total as f64
        }
    }

    /// Number of retained distinct meshes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no mesh is retained.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct QueueOrder {
    priority_score: u32,
    sequence: u64,
    generation: u64,
    key: SectionKey,
}

impl Ord for QueueOrder {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.priority_score
            .cmp(&other.priority_score)
            .then_with(|| other.sequence.cmp(&self.sequence))
            .then_with(|| self.key.cmp(&other.key))
            .then_with(|| self.generation.cmp(&other.generation))
    }
}

impl PartialOrd for QueueOrder {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

struct PendingUpload<T> {
    byte_size: usize,
    generation: u64,
    payload: T,
}

/// Frame-pacing ring for GPU staging uploads.
///
/// Requeueing a [`SectionKey`] replaces its stale payload in place at the new
/// priority; obsolete heap entries are generation-checked and discarded without
/// ever draining duplicate uploads.
pub struct FramePacingUploadRing<T> {
    pending: BTreeMap<SectionKey, PendingUpload<T>>,
    heap: BinaryHeap<QueueOrder>,
    next_sequence: u64,
    pending_bytes: usize,
}

impl<T> Default for FramePacingUploadRing<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> FramePacingUploadRing<T> {
    /// Empty upload ring.
    #[must_use]
    pub fn new() -> Self {
        Self {
            pending: BTreeMap::new(),
            heap: BinaryHeap::new(),
            next_sequence: 0,
            pending_bytes: 0,
        }
    }

    /// Enqueue or replace one upload. Later pushes for the same key are the only
    /// live version, so stale queued uploads can never consume frame budget twice.
    pub fn push(&mut self, priority_score: u32, byte_size: usize, key: SectionKey, payload: T) {
        let generation = self
            .pending
            .get(&key)
            .map_or(0, |pending| pending.generation.wrapping_add(1));
        if let Some(old) = self.pending.remove(&key) {
            self.pending_bytes = self.pending_bytes.saturating_sub(old.byte_size);
        }
        self.pending.insert(
            key,
            PendingUpload {
                byte_size,
                generation,
                payload,
            },
        );
        self.pending_bytes = self.pending_bytes.saturating_add(byte_size);
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.wrapping_add(1);
        self.heap.push(QueueOrder {
            priority_score,
            sequence,
            generation,
            key,
        });
    }

    /// Drain highest-priority uploads into `out`, returning their total byte size.
    ///
    /// Uploads are considered in strict priority order. If the first live
    /// upload is larger than the whole budget it is still admitted alone,
    /// preventing permanent starvation of oversized chunk streams.
    pub fn drain_frame_budget(
        &mut self,
        max_bytes_per_frame: usize,
        out: &mut Vec<(SectionKey, T)>,
    ) -> usize {
        let mut drained_bytes = 0usize;
        let mut drained_items = 0usize;
        while let Some(order) = self.heap.pop() {
            let live = self
                .pending
                .get(&order.key)
                .is_some_and(|pending| pending.generation == order.generation);
            if !live {
                continue;
            }
            let Some(byte_size) = self
                .pending
                .get(&order.key)
                .map(|pending| pending.byte_size)
            else {
                continue;
            };
            let fits = drained_bytes.saturating_add(byte_size) <= max_bytes_per_frame;
            if !fits && drained_items != 0 {
                self.heap.push(order);
                break;
            }
            if let Some((key, pending)) = self.pending.remove_entry(&order.key) {
                self.pending_bytes = self.pending_bytes.saturating_sub(pending.byte_size);
                drained_bytes = drained_bytes.saturating_add(pending.byte_size);
                drained_items += 1;
                out.push((key, pending.payload));
            }
            if !fits || (byte_size != 0 && drained_bytes >= max_bytes_per_frame) {
                break;
            }
        }
        drained_bytes
    }

    /// Number of live deduplicated uploads.
    #[must_use]
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    /// Whether no upload is pending.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Total bytes of live deduplicated uploads.
    #[must_use]
    pub const fn pending_bytes(&self) -> usize {
        self.pending_bytes
    }

    /// Drop all pending uploads and stale queue entries.
    pub fn clear(&mut self) {
        self.pending.clear();
        self.heap.clear();
        self.pending_bytes = 0;
    }
}
