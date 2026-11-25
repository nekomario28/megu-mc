// SPDX-License-Identifier: LGPL-3.0-or-later
#![forbid(unsafe_code)]
//! `megu_core::render_cs` — the five foundational graphics/GPU computer
//! science primitives both the Vanilla renderer and every modded
//! block/subworld pipeline inherit, keyed directly on
//! [`crate::bloom::VoxelBitset4096`] and [`crate::coords::SectionKey`].
//!
//! The suite is backend-agnostic: nothing here touches Vulkan, DX12 or
//! Metal, so the same decisions feed every `client-gpu` backend.
//!
//! * **Cave/portal section visibility** — [`SectionVisBits`] packs the
//!   6×6 "can a sight-line enter through face *a* and leave through face
//!   *b*" connectivity of one `16³` section into a single `u64` via
//!   [`compute_section_visibility`]'s 100 % stack-allocated flood fill,
//!   and [`bfs_visible_sections`] runs the Tommo/Sodium-style
//!   no-backtrack portal BFS across [`SectionKey`]s into a caller-owned
//!   scratch `Vec` (zero per-frame allocation).
//! * **Six-axis back-face bucket culling** — [`visible_face_mask`]
//!   exploits the fact that a section strictly east of the camera can
//!   have *no* visible `+X`-facing faces, and [`SixAxisBuckets`]
//!   partitions draw work into six normal-bucketed contiguous
//!   [`FaceRange`]s so the indirect-draw builder emits only the
//!   half-space that can be seen — plus a seventh
//!   [`DrawBucket::UnculledCustom`] bucket that is *never* culled.
//! * **Bit-parallel binary greedy mesher** — [`BinaryGreedyMesher`]
//!   packs each 16-voxel column into one padded `u32` word and extracts
//!   every exposed face along an axis in two bitwise ops
//!   (`col & !(col >> 1)` / `col & !(col << 1)`), then greedily merges
//!   coplanar runs into [`GreedyQuad`]s with 60–95 % fewer quads than
//!   naive per-voxel emission at bit-exact face coverage.
//! * **Quantized lazy translucent sorter** — [`TranslucentSortCache`]
//!   quantizes the camera to half-block cells and re-sorts only when the
//!   cell or quad revision changes, running an `O(N)` two-pass 8-bit LSB
//!   radix sort into reusable scratch.
//! * **Render-graph compiler** — [`RenderGraph::compile`] prunes dead
//!   passes, rejects cycles, emits a deterministic topological order,
//!   computes exact transient-resource lifetimes and runs linear-scan
//!   interval colouring to alias non-overlapping GPU textures/buffers
//!   into the minimum physical slots.
//!
//! # Mod/protrusion safety contract (fail-closed)
//!
//! Modded blocks whose visual models leave the `1×1×1` cube (Create-style
//! waterwheels, oversized statues, beacon beams) or use non-axis-aligned
//! geometry must *never* be clipped by math that assumes unit cubes:
//!
//! * [`SectionRenderMetadata`] carries `has_protruding_bounds` /
//!   `max_protrusion_blocks` / `has_non_axis_geometry`. Only strictly
//!   full-opaque non-protruding cubes may be marked in the `opaque`
//!   [`VoxelBitset4096`] passed to [`compute_section_visibility`] — a
//!   protruding or diagonal block is treated as *non-occluding* air for
//!   connectivity, so it can never hide a corridor behind it.
//! * [`visible_face_mask_protrusion`] expands the target section's bounds
//!   by `max_protrusion_blocks`, so an overhang reaching into the
//!   camera's half-space keeps its faces.
//! * [`bfs_visible_sections`] marks a protruding section visible whenever
//!   any section inside its expanded bounds is visible.
//! * [`SixAxisBuckets`]'s [`DrawBucket::UnculledCustom`] range is emitted
//!   unconditionally — diagonal, curved, animated or protruding model
//!   geometry is never bucket-culled.
//!
//! Pure `std`, zero dependencies, `unsafe`-free, deterministic.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::fmt;

use crate::bloom::VoxelBitset4096;
use crate::coords::{CoordMap, SectionKey};

/// Number of voxels in one `16³` chunk section.
pub const SECTION_VOLUME: usize = 4096;

/// Canonical section-local voxel index `(y << 8) | (z << 4) | x` — the
/// same layout [`crate::coords::BlockPos::local_index`] and
/// [`VoxelBitset4096`] share.
#[inline]
#[must_use]
pub const fn voxel_index(x: u8, y: u8, z: u8) -> u16 {
    ((y as u16) << 8) | ((z as u16) << 4) | (x as u16)
}

// ---------------------------------------------------------------------------
// 1. Cave / portal section visibility graph
// ---------------------------------------------------------------------------

/// The six cardinal faces of a cubic section, indexed so `dir as u8`
/// selects bit `dir` of every 6-bit face mask in this module.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum FaceDir {
    /// `+X` (east) face.
    PosX = 0,
    /// `-X` (west) face.
    NegX = 1,
    /// `+Y` (up) face.
    PosY = 2,
    /// `-Y` (down) face.
    NegY = 3,
    /// `+Z` (south) face.
    PosZ = 4,
    /// `-Z` (north) face.
    NegZ = 5,
}

impl FaceDir {
    /// All six directions, in index order.
    pub const ALL: [Self; 6] = [
        Self::PosX,
        Self::NegX,
        Self::PosY,
        Self::NegY,
        Self::PosZ,
        Self::NegZ,
    ];

    /// The face on the opposite side of the cube.
    #[inline]
    #[must_use]
    pub const fn opposite(self) -> Self {
        match self {
            Self::PosX => Self::NegX,
            Self::NegX => Self::PosX,
            Self::PosY => Self::NegY,
            Self::NegY => Self::PosY,
            Self::PosZ => Self::NegZ,
            Self::NegZ => Self::PosZ,
        }
    }

    /// Unit step crossing this face: `section + delta()` is the neighbour
    /// sharing the face.
    #[inline]
    #[must_use]
    pub const fn delta(self) -> (i32, i32, i32) {
        match self {
            Self::PosX => (1, 0, 0),
            Self::NegX => (-1, 0, 0),
            Self::PosY => (0, 1, 0),
            Self::NegY => (0, -1, 0),
            Self::PosZ => (0, 0, 1),
            Self::NegZ => (0, 0, -1),
        }
    }

    /// Bit selecting this face inside a 6-bit face mask.
    #[inline]
    #[must_use]
    pub const fn bit(self) -> u8 {
        1 << (self as u8)
    }

    /// `self as usize` — the 0..6 index used for bucket arrays.
    #[inline]
    #[must_use]
    pub const fn index(self) -> usize {
        self as usize
    }
}

/// Per-section portal connectivity: bit `a * 6 + b` is `1` iff a
/// sight-line can enter the section through face `a` and leave through
/// face `b` entirely inside non-opaque voxels.
///
/// Eight bytes per section — versus the multi-kilobyte occlusion meshes
/// per-chunk approaches need — and symmetric by construction, so
/// `can_see(a, b) == can_see(b, a)`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SectionVisBits(pub u64);

impl SectionVisBits {
    /// No face pair is connected (fully opaque section).
    pub const EMPTY: Self = Self(0);
    /// Every face pair is connected (fully air section).
    pub const ALL: Self = Self((1u64 << 36) - 1);

    /// Whether face `enter` reaches face `exit` through non-opaque voxels.
    #[inline]
    #[must_use]
    pub const fn can_see(self, enter: FaceDir, exit: FaceDir) -> bool {
        (self.0 >> ((enter as u64) * 6 + (exit as u64))) & 1 == 1
    }

    /// Whether any non-opaque voxel touches `face` — the degenerate
    /// `enter == exit` pair the camera-section seed uses to test
    /// "can sight leave through this face at all".
    #[inline]
    #[must_use]
    pub const fn touches_face(self, face: FaceDir) -> bool {
        self.can_see(face, face)
    }

    /// Mark faces `a`/`b` mutually reachable (sets both directions).
    #[inline]
    pub fn set(&mut self, a: FaceDir, b: FaceDir) {
        self.0 |= 1u64 << ((a as u64) * 6 + (b as u64));
        self.0 |= 1u64 << ((b as u64) * 6 + (a as u64));
    }
}

/// Compute the portal connectivity of one `16³` section.
///
/// `opaque` must contain **only strictly full-opaque, non-protruding
/// cubes**. Per the mod-safety contract, protruding or non-axis-aligned
/// mod geometry is never marked here: it is treated as non-occluding
/// air, which can only *loosen* connectivity — never wrongly hide a
/// corridor behind an overhanging model.
///
/// `O(1)` fast paths return [`SectionVisBits::EMPTY`] for a full-stone
/// section and [`SectionVisBits::ALL`] for an all-air section. Otherwise
/// a 3D flood fill labels every connected air component with the set of
/// boundary faces it touches and marks all mutually reachable face
/// pairs. The only working state is a second [`VoxelBitset4096`]
/// visited-set plus a fixed `[u16; 4096]` DFS stack — 100 %
/// stack-allocated, zero heap allocation.
#[must_use]
pub fn compute_section_visibility(opaque: &VoxelBitset4096) -> SectionVisBits {
    if opaque.is_empty() {
        return SectionVisBits::ALL;
    }
    if opaque.count_ones() == SECTION_VOLUME as u32 {
        return SectionVisBits::EMPTY;
    }

    let mut visited = VoxelBitset4096::new();
    let mut stack = [0u16; SECTION_VOLUME];
    let mut bits = SectionVisBits::EMPTY;

    for seed in 0..SECTION_VOLUME as u16 {
        if opaque.get(seed) || visited.get(seed) {
            continue;
        }
        // Flood one connected air component, collecting the boundary
        // faces it touches.
        let mut sp = 0usize;
        let mut face_mask = 0u8;
        visited.set(seed);
        stack[sp] = seed;
        sp += 1;
        while sp > 0 {
            sp -= 1;
            let idx = stack[sp];
            let x = i32::from(idx & 15);
            let y = i32::from(idx >> 8);
            let z = i32::from((idx >> 4) & 15);
            for dir in FaceDir::ALL {
                let (dx, dy, dz) = dir.delta();
                let (nx, ny, nz) = (x + dx, y + dy, z + dz);
                if !(0..16).contains(&nx) || !(0..16).contains(&ny) || !(0..16).contains(&nz) {
                    face_mask |= dir.bit();
                    continue;
                }
                let n = voxel_index(nx as u8, ny as u8, nz as u8);
                if !opaque.get(n) && !visited.get(n) {
                    visited.set(n);
                    stack[sp] = n;
                    sp += 1;
                }
            }
        }
        // Every pair of faces this component touches is mutually
        // reachable through it.
        for a in FaceDir::ALL {
            if face_mask & a.bit() == 0 {
                continue;
            }
            for b in FaceDir::ALL {
                if face_mask & b.bit() != 0 {
                    bits.set(a, b);
                }
            }
        }
    }
    bits
}

/// Lightweight per-section render descriptor consumed by the culling
/// and BFS passes. One instance per loaded render section; `Copy`, so a
/// `CoordMap<SectionKey, SectionRenderMetadata>` lookup is free to call
/// inside tight BFS loops.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SectionRenderMetadata {
    /// Portal connectivity from [`compute_section_visibility`].
    pub vis_bits: SectionVisBits,
    /// Any block in the section has a visual AABB exceeding `1×1×1`
    /// (protruding mod model, beam, multiblock part).
    pub has_protruding_bounds: bool,
    /// Farthest distance in whole blocks any contained model's visual
    /// AABB can extend past the section border. `0` for ordinary
    /// vanilla/modded unit-cube sections.
    pub max_protrusion_blocks: u8,
    /// Section contains non-axis-aligned/diagonal/curved/animated model
    /// geometry that cannot be classified into the six cardinal face
    /// buckets and must route to [`DrawBucket::UnculledCustom`].
    pub has_non_axis_geometry: bool,
}

impl SectionRenderMetadata {
    /// Plain section with the given portal connectivity and no
    /// protrusion or custom geometry.
    #[inline]
    #[must_use]
    pub const fn new(vis_bits: SectionVisBits) -> Self {
        Self {
            vis_bits,
            has_protruding_bounds: false,
            max_protrusion_blocks: 0,
            has_non_axis_geometry: false,
        }
    }

    /// Declare that models may protrude up to `blocks` whole blocks past
    /// the section border (a `0` clears the flag).
    #[inline]
    #[must_use]
    pub const fn with_protrusion(mut self, blocks: u8) -> Self {
        self.max_protrusion_blocks = blocks;
        self.has_protruding_bounds = blocks > 0;
        self
    }

    /// Declare non-axis-aligned geometry present (routes to the
    /// never-culled custom bucket).
    #[inline]
    #[must_use]
    pub const fn with_non_axis_geometry(mut self) -> Self {
        self.has_non_axis_geometry = true;
        self
    }

    /// Conservative protrusion distance used by the culling math: a
    /// flagged section with an unrecorded distance is still treated as
    /// protruding one block — fail-closed.
    #[inline]
    #[must_use]
    pub const fn effective_protrusion(self) -> u8 {
        if self.has_protruding_bounds && self.max_protrusion_blocks == 0 {
            1
        } else {
            self.max_protrusion_blocks
        }
    }
}

/// Mask bit marking the camera seed inside [`BfsScratch::entered`]
/// (faces occupy bits 0..6).
const BFS_CAMERA: u8 = 1 << 6;
/// Mask bit recording a section's protrusion scan already ran.
const BFS_SCANNED: u8 = 1 << 7;
/// The six entry-face bits of an `entered` mask.
const BFS_FACE_MASK: u8 = 0b11_1111;
/// Hard cap on the protrusion scan radius, bounding BFS fan-out; the
/// caller-selected `protrusion_scan_blocks` is clamped to `8 * 16` blocks
/// of section reach, which covers every realistic modded model.
const BFS_MAX_SCAN_RADIUS: i32 = 8;

/// Reusable BFS state for [`bfs_visible_sections`]. Allocate once per
/// render thread and pass by reference every frame: `clear` keeps the
/// allocations, so a steady-state frame performs zero heap traffic.
#[derive(Default)]
pub struct BfsScratch {
    /// FIFO of sections awaiting expansion.
    pub queue: VecDeque<SectionKey>,
    /// Per-section bitmask: entry faces (bits 0..6) plus the
    /// camera/protrusion-scanned flags (bits 6..8).
    pub entered: CoordMap<SectionKey, u8>,
}

/// Tommo/Sodium-style no-backtrack portal BFS across sections.
///
/// Starts at `camera` and floods outward across [`FaceDir`] neighbours.
/// A section expands through exit face `e` only when
///
/// 1. some face the section was entered through can see `e`
///    (`vis_bits.can_see(enter, e)`; for the camera seed, "entered" is
///    the camera point itself, so `touches_face(e)` suffices), **and**
/// 2. `e` is not itself an entry face — the BFS never backtracks
///    opposite to a direction already traversed, which stops the flood
///    from wrapping around behind-camera hills and sealed caves.
///
/// `meta` maps a [`SectionKey`] to its [`SectionRenderMetadata`];
/// sections with `None` (unloaded/untracked) are treated as invisible
/// and never propagate.
///
/// **Protrusion safety:** every time a section is first marked visible,
/// all neighbours within `protrusion_scan_blocks` of reach are probed;
/// a neighbour whose `max_protrusion_blocks` can bridge the gap is
/// marked visible too, so an oversized Create-style wheel sticking out
/// of a sealed cave doorway can never pop out. The ring-1 scan is always
/// performed (`protrusion_scan_blocks` ≥ 1..16 covers it); larger values
/// extend the reach by `ceil(blocks / 16)` sections.
///
/// Results are appended into caller-owned `out` (cleared first) and all
/// bookkeeping lives in `scratch` — zero allocation on reuse.
pub fn bfs_visible_sections(
    camera: SectionKey,
    meta: impl Fn(SectionKey) -> Option<SectionRenderMetadata>,
    protrusion_scan_blocks: u8,
    scratch: &mut BfsScratch,
    out: &mut Vec<SectionKey>,
) {
    scratch.queue.clear();
    scratch.entered.clear();
    out.clear();

    out.push(camera);
    if meta(camera).is_none() {
        return;
    }
    scratch.entered.insert(camera, BFS_CAMERA);
    scratch.queue.push_back(camera);

    // Ring-1 is always scanned; larger configured protrusions extend it.
    let radius = ((i32::from(protrusion_scan_blocks) + 15) / 16).clamp(1, BFS_MAX_SCAN_RADIUS);

    while let Some(sec) = scratch.queue.pop_front() {
        let mask = scratch.entered.get(&sec).copied().unwrap_or(0);
        let Some(m) = meta(sec) else {
            continue;
        };
        let entries = mask & BFS_FACE_MASK;
        for exit in FaceDir::ALL {
            // Rule 2: never backtrack out through an entry face.
            if entries & exit.bit() != 0 {
                continue;
            }
            // Rule 1: the camera point reaches any air-touching face;
            // otherwise some entry face must see the exit face.
            let mut open = mask & BFS_CAMERA != 0 && m.vis_bits.touches_face(exit);
            if !open {
                for enter in FaceDir::ALL {
                    if entries & enter.bit() != 0 && m.vis_bits.can_see(enter, exit) {
                        open = true;
                        break;
                    }
                }
            }
            if !open {
                continue;
            }
            let (dx, dy, dz) = exit.delta();
            let next = SectionKey {
                dim: sec.dim,
                cx: sec.cx + dx,
                sy: sec.sy + dy,
                cz: sec.cz + dz,
            };
            if meta(next).is_none() {
                continue;
            }
            bfs_reveal(next, exit.opposite().bit(), scratch, out);
        }
        if mask & BFS_SCANNED == 0 {
            if let Some(m) = scratch.entered.get_mut(&sec) {
                *m |= BFS_SCANNED;
            }
            bfs_scan_protrusions(sec, radius, &meta, scratch, out);
        }
    }
}

/// Record `face_bits` as additional entry faces of `sec`: first-time
/// reveals make it visible, every mask growth re-queues it so the new
/// portals propagate.
fn bfs_reveal(sec: SectionKey, face_bits: u8, scratch: &mut BfsScratch, out: &mut Vec<SectionKey>) {
    let entry = scratch.entered.entry(sec).or_insert(0);
    if *entry & face_bits == face_bits {
        return;
    }
    let first_visible = *entry & (BFS_FACE_MASK | BFS_CAMERA) == 0;
    *entry |= face_bits;
    if first_visible {
        out.push(sec);
    }
    scratch.queue.push_back(sec);
}

/// Fail-closed protrusion reveal: any neighbour whose declared
/// `max_protrusion_blocks` can geometrically bridge the gap to `sec`
/// (its expanded visual AABB overlaps `sec`) inherits `sec`'s
/// visibility — an overhanging model is drawn even when every portal
/// into its section is sealed.
fn bfs_scan_protrusions(
    sec: SectionKey,
    radius: i32,
    meta: &impl Fn(SectionKey) -> Option<SectionRenderMetadata>,
    scratch: &mut BfsScratch,
    out: &mut Vec<SectionKey>,
) {
    for dx in -radius..=radius {
        for dy in -radius..=radius {
            for dz in -radius..=radius {
                if dx == 0 && dy == 0 && dz == 0 {
                    continue;
                }
                let next = SectionKey {
                    dim: sec.dim,
                    cx: sec.cx + dx,
                    sy: sec.sy + dy,
                    cz: sec.cz + dz,
                };
                let Some(nm) = meta(next) else {
                    continue;
                };
                let protrusion = i32::from(nm.effective_protrusion());
                if protrusion <= 0 {
                    continue;
                }
                // Blocks the model must span on each axis: an offset of
                // |o| sections needs (|o|-1)*16+1 protruding blocks.
                let needed = [dx, dy, dz]
                    .into_iter()
                    .map(|o| if o == 0 { 0 } else { (o.abs() - 1) * 16 + 1 })
                    .max()
                    .unwrap_or(0);
                if protrusion < needed {
                    continue;
                }
                // The sight-line enters through the faces toward `sec`.
                let mut faces = 0u8;
                if dx > 0 {
                    faces |= FaceDir::NegX.bit();
                } else if dx < 0 {
                    faces |= FaceDir::PosX.bit();
                }
                if dy > 0 {
                    faces |= FaceDir::NegY.bit();
                } else if dy < 0 {
                    faces |= FaceDir::PosY.bit();
                }
                if dz > 0 {
                    faces |= FaceDir::NegZ.bit();
                } else if dz < 0 {
                    faces |= FaceDir::PosZ.bit();
                }
                bfs_reveal(next, faces, scratch, out);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 2. Six-axis normal-bucketed terrain back-face cluster culling
// ---------------------------------------------------------------------------

/// 6-bit mask of face directions of `target_sec` that can be
/// front-facing to a camera anywhere inside `cam_sec` (both in
/// chunk-section coordinates, `(cx, sy, cz)`).
///
/// If the target is strictly east of the camera section, every `+X`
/// face in it is mathematically back-facing — the bit is cleared, so
/// whole halves of the draw stream drop out with zero GPU work.
/// Same-section targets keep all six bits (`0b11_1111`).
#[inline]
#[must_use]
pub fn visible_face_mask(cam_sec: (i32, i32, i32), target_sec: (i32, i32, i32)) -> u8 {
    visible_face_mask_protrusion(cam_sec, target_sec, 0)
}

/// Protrusion-aware [`visible_face_mask`]: the target's bounds are
/// expanded by `protrusion_blocks` on every axis first, so a model
/// overhanging into the camera's half-space is never wrongly clipped.
/// `i64` block-space math makes overflow impossible even at extreme
/// modded coordinates.
#[must_use]
pub fn visible_face_mask_protrusion(
    cam_sec: (i32, i32, i32),
    target_sec: (i32, i32, i32),
    protrusion_blocks: u8,
) -> u8 {
    let p = i64::from(protrusion_blocks);
    let cam = [cam_sec.0, cam_sec.1, cam_sec.2];
    let tgt = [target_sec.0, target_sec.1, target_sec.2];
    let mut mask = 0b11_1111u8;
    for (axis, neg, pos) in [
        (0usize, FaceDir::NegX, FaceDir::PosX),
        (1usize, FaceDir::NegY, FaceDir::PosY),
        (2usize, FaceDir::NegZ, FaceDir::PosZ),
    ] {
        let cam_min = i64::from(cam[axis]) * 16;
        let cam_max = cam_min + 15;
        let tgt_min = i64::from(tgt[axis]) * 16 - p;
        let tgt_max = i64::from(tgt[axis]) * 16 + 15 + p;
        if cam_max < tgt_min {
            mask &= !pos.bit();
        }
        if cam_min > tgt_max {
            mask &= !neg.bit();
        }
    }
    mask
}

/// A contiguous `[offset, offset + count)` span inside a baked draw
/// buffer — exactly the unit an indirect-draw call needs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FaceRange {
    /// First element index.
    pub offset: u32,
    /// Number of elements.
    pub count: u32,
}

/// Draw-work classification: the six normal buckets plus
/// [`DrawBucket::UnculledCustom`], the fail-closed escape for
/// diagonal/curved/protruding/animated mod geometry that is never
/// half-space culled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DrawBucket {
    /// Cardinal face bucket for `FaceDir`-normal geometry.
    Face(FaceDir),
    /// Geometry that must always be drawn (non-axis or protruding
    /// models); emitted regardless of the visibility mask.
    UnculledCustom,
}

impl DrawBucket {
    /// Bucket array index (`0..6` faces, `6` custom).
    #[inline]
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::Face(dir) => dir.index(),
            Self::UnculledCustom => 6,
        }
    }
}

/// Total bucket count inside [`SixAxisBuckets`]: six face buckets plus
/// the never-culled custom bucket.
pub const BUCKET_COUNT: usize = 7;

/// Normal-bucketed draw-work partition with zero-alloc reuse.
///
/// Producers `push` classified items, `bake` packs them into one
/// contiguous `items` buffer with one [`FaceRange`] per bucket, and
/// [`SixAxisBuckets::emit_draw_ranges`] turns a
/// [`visible_face_mask`] into the minimal list of indirect-draw ranges:
/// culled half-space buckets emit *nothing* (0 vertex invocations) and
/// the [`DrawBucket::UnculledCustom`] range is always emitted.
pub struct SixAxisBuckets<T> {
    items: Vec<T>,
    ranges: [FaceRange; BUCKET_COUNT],
    staging: [Vec<T>; BUCKET_COUNT],
}

impl<T> SixAxisBuckets<T> {
    /// Empty bucket set.
    #[must_use]
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            ranges: [FaceRange::default(); BUCKET_COUNT],
            staging: Default::default(),
        }
    }

    /// Reset for a new frame, keeping every allocation.
    pub fn clear(&mut self) {
        self.items.clear();
        self.ranges = [FaceRange::default(); BUCKET_COUNT];
        for bucket in &mut self.staging {
            bucket.clear();
        }
    }

    /// Classify one item into `bucket` before [`Self::bake`].
    pub fn push(&mut self, bucket: DrawBucket, item: T) {
        self.staging[bucket.index()].push(item);
    }

    /// Pack all staged items into contiguous per-bucket spans.
    pub fn bake(&mut self) {
        self.items.clear();
        for (idx, staged) in self.staging.iter_mut().enumerate() {
            let offset = self.items.len() as u32;
            let count = staged.len() as u32;
            self.items.append(staged);
            self.ranges[idx] = FaceRange { offset, count };
        }
    }

    /// The baked contiguous draw buffer.
    #[inline]
    #[must_use]
    pub fn items(&self) -> &[T] {
        &self.items
    }

    /// Total baked item count.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether no items were baked.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Range of `dir`'s face bucket.
    #[inline]
    #[must_use]
    pub fn range(&self, dir: FaceDir) -> FaceRange {
        self.ranges[dir.index()]
    }

    /// Range of the never-culled custom bucket.
    #[inline]
    #[must_use]
    pub fn custom_range(&self) -> FaceRange {
        self.ranges[DrawBucket::UnculledCustom.index()]
    }

    /// Baked items of `bucket`.
    #[must_use]
    pub fn bucket_items(&self, bucket: DrawBucket) -> &[T] {
        let r = self.ranges[bucket.index()];
        &self.items[r.offset as usize..(r.offset + r.count) as usize]
    }

    /// Append the non-empty ranges the indirect-draw builder must emit
    /// for `face_mask` (from [`visible_face_mask`]): one per set face
    /// bit, plus the custom bucket unconditionally — that is the
    /// fail-closed contract that keeps mod geometry drawn from every
    /// camera angle.
    pub fn emit_draw_ranges(&self, face_mask: u8, out: &mut Vec<FaceRange>) {
        for dir in FaceDir::ALL {
            if face_mask & dir.bit() != 0 {
                let r = self.ranges[dir.index()];
                if r.count > 0 {
                    out.push(r);
                }
            }
        }
        let custom = self.custom_range();
        if custom.count > 0 {
            out.push(custom);
        }
    }
}

impl<T> Default for SixAxisBuckets<T> {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// 3. Bit-parallel binary greedy mesher
// ---------------------------------------------------------------------------

/// One greedily merged face run: `w × h` unit faces sharing `dir`,
/// `slice` (voxel coordinate along the face axis) and `material`.
///
/// In-plane axes follow the canonical voxel order: for `dir` on axis X
/// `u`/`v` are Y/Z, on axis Y they are X/Z, on axis Z they are X/Y.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GreedyQuad {
    /// Face normal direction.
    pub dir: FaceDir,
    /// Voxel coordinate along `dir`'s axis (`0..16`).
    pub slice: u8,
    /// Rectangle origin on the first in-plane axis.
    pub u: u8,
    /// Rectangle origin on the second in-plane axis.
    pub v: u8,
    /// Rectangle extent along `u` (`1..=16`).
    pub w: u8,
    /// Rectangle extent along `v` (`1..=16`).
    pub h: u8,
    /// Opaque material/AO key from the caller's `material_at`.
    pub material: u32,
}

/// In-plane coordinate mapping shared by the mesher and the
/// `border_opaque`/`material_at` callbacks: `(axis, along_axis, u, v)`
/// → voxel `(x, y, z)` in section-local coordinates.
fn mesher_coords(axis: usize, along: u8, u: u8, v: u8) -> (u8, u8, u8) {
    match axis {
        0 => (along, u, v),
        1 => (u, along, v),
        _ => (u, v, along),
    }
}

/// Bit-parallel greedy mesher for `16³` sections.
///
/// `occluders` must contain **only strictly full-opaque non-protruding
/// cubes** — the same contract as [`compute_section_visibility`].
/// Non-cube/protruding mod blocks are never fed in, so they neither emit
/// cardinal quads here (they belong to [`DrawBucket::UnculledCustom`])
/// nor hide faces of real cubes behind them.
///
/// `border_opaque(dir, u, v)` reports whether the voxel just outside the
/// section on face `dir` at in-plane `(u, v)` is an occluder — the
/// optional 1-bit per-column neighbour padding. Return `false` when the
/// neighbour is unknown (fail-closed: border faces are emitted rather
/// than wrongly culled).
///
/// `material_at(x, y, z)` partitions mergeable faces — identical keys
/// merge, different keys split quads.
///
/// Output appends into caller-owned `out` (cleared first) — zero
/// per-section heap allocation.
pub struct BinaryGreedyMesher;

impl BinaryGreedyMesher {
    /// Padded-column face bits: bit `j` (1..=16) is the face of voxel
    /// `j - 1`; bits 0 and 17 hold the neighbour border occupancy.
    const FACE_BITS: u32 = 0x0001_FFFE;

    /// Mesh `occluders`, appending merged quads to `out`.
    pub fn mesh(
        occluders: &VoxelBitset4096,
        border_opaque: impl Fn(FaceDir, u8, u8) -> bool,
        material_at: impl Fn(u8, u8, u8) -> u32,
        out: &mut Vec<GreedyQuad>,
    ) {
        out.clear();
        for (axis, pos_dir, neg_dir) in [
            (0usize, FaceDir::PosX, FaceDir::NegX),
            (1usize, FaceDir::PosY, FaceDir::NegY),
            (2usize, FaceDir::PosZ, FaceDir::NegZ),
        ] {
            // Face grids per direction: grid[slice][v] is a 16-bit row
            // of present faces along u.
            let mut pos_grid = [[0u32; 16]; 16];
            let mut neg_grid = [[0u32; 16]; 16];
            for v in 0..16u8 {
                for u in 0..16u8 {
                    // Pack the 16-voxel column along `axis` into one
                    // padded u32: bit j = voxel (j - 1), borders at
                    // bits 0 and 17.
                    let mut col = 0u32;
                    for i in 0..16u8 {
                        let (x, y, z) = mesher_coords(axis, i, u, v);
                        if occluders.get(voxel_index(x, y, z)) {
                            col |= 1 << (i + 1);
                        }
                    }
                    if border_opaque(neg_dir, u, v) {
                        col |= 1;
                    }
                    if border_opaque(pos_dir, u, v) {
                        col |= 1 << 17;
                    }
                    // Two bitwise ops per direction extract every
                    // exposed face in the column at once.
                    let mut pos_faces = col & !(col >> 1) & Self::FACE_BITS;
                    let mut neg_faces = col & !(col << 1) & Self::FACE_BITS;
                    while pos_faces != 0 {
                        let j = pos_faces.trailing_zeros();
                        pos_faces &= pos_faces - 1;
                        pos_grid[(j - 1) as usize][v as usize] |= 1 << u;
                    }
                    while neg_faces != 0 {
                        let j = neg_faces.trailing_zeros();
                        neg_faces &= neg_faces - 1;
                        neg_grid[(j - 1) as usize][v as usize] |= 1 << u;
                    }
                }
            }
            for (s, grid) in pos_grid.iter_mut().enumerate() {
                Self::merge_slice(axis, pos_dir, s as u8, grid, &material_at, out);
            }
            for (s, grid) in neg_grid.iter_mut().enumerate() {
                Self::merge_slice(axis, neg_dir, s as u8, grid, &material_at, out);
            }
        }
    }

    /// Greedy 2D rectangle merge of one `(dir, slice)` face grid:
    /// `rows[v]` holds the 16-bit face mask along `u`. Runs extend by
    /// `trailing_zeros`/`trailing_ones` and must share `material` —
    /// different keys force a quad boundary, so merging can never
    /// mislabel a face's material.
    fn merge_slice(
        axis: usize,
        dir: FaceDir,
        slice: u8,
        rows: &mut [u32; 16],
        material_at: &impl Fn(u8, u8, u8) -> u32,
        out: &mut Vec<GreedyQuad>,
    ) {
        for v0 in 0..16usize {
            let mut row = rows[v0];
            while row != 0 {
                let u0 = row.trailing_zeros();
                let (x, y, z) = mesher_coords(axis, slice, u0 as u8, v0 as u8);
                let material = material_at(x, y, z);
                let mut w = 1u32;
                while u0 + w < 16 && row & (1 << (u0 + w)) != 0 {
                    let (x, y, z) = mesher_coords(axis, slice, (u0 + w) as u8, v0 as u8);
                    if material_at(x, y, z) != material {
                        break;
                    }
                    w += 1;
                }
                let run_mask = ((1u32 << w) - 1) << u0;
                let mut h = 1u32;
                while v0 + (h as usize) < 16 {
                    let v1 = v0 + h as usize;
                    if rows[v1] & run_mask != run_mask {
                        break;
                    }
                    let mut uniform = true;
                    for du in 0..w {
                        let (x, y, z) =
                            mesher_coords(axis, slice, (u0 + du) as u8, v1 as u8);
                        if material_at(x, y, z) != material {
                            uniform = false;
                            break;
                        }
                    }
                    if !uniform {
                        break;
                    }
                    h += 1;
                }
                out.push(GreedyQuad {
                    dir,
                    slice,
                    u: u0 as u8,
                    v: v0 as u8,
                    w: w as u8,
                    h: h as u8,
                    material,
                });
                for dv in 0..h {
                    rows[v0 + dv as usize] &= !run_mask;
                }
                row &= !run_mask;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 4. Quantized lazy translucent radix sorter
// ---------------------------------------------------------------------------

/// A translucent quad's identity and world-space centre — the minimal
/// record the back-to-front pass needs.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TranslucentQuadKey {
    /// Index into the caller's translucent quad buffer.
    pub quad_idx: u32,
    /// World-space quad centre.
    pub center: [f32; 3],
}

/// Result of [`TranslucentSortCache::sort`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortOutcome {
    /// Camera stayed inside the same half-block cell and the quad
    /// revision is unchanged — the cached order is still exact.
    /// Zero work, zero allocations.
    Cached,
    /// A fresh radix pass ran; [`TranslucentSortCache::order`] holds the
    /// new back-to-front order.
    Resorted,
}

/// Lazy back-to-front sorter for translucent geometry.
///
/// Sorting translucent quads every frame is wasted work whenever the
/// camera hasn't meaningfully moved: the camera position is quantized
/// to half-block cells (`(cam * 2).floor()`), and while the cell and the
/// caller's `quads_rev` both hold, [`SortOutcome::Cached`] returns
/// without touching a single quad.
///
/// A real re-sort quantizes each quad's distance into a `u16` key in
/// 1/256-block units (clamped past 255.99 blocks — ties there are all
/// "far" anyway) and runs a stable two-pass 8-bit LSB radix sort
/// (`O(N)`, `[u32; 256]` histogram plus reusable scratch) that emits
/// descending — back-to-front — order.
pub struct TranslucentSortCache {
    last_cam_cell: (i32, i32, i32),
    last_rev: u64,
    primed: bool,
    order: Vec<u32>,
    scratch: Vec<u32>,
    keys: Vec<u32>,
    hist: [u32; 256],
}

impl TranslucentSortCache {
    /// Empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self {
            last_cam_cell: (0, 0, 0),
            last_rev: 0,
            primed: false,
            order: Vec::new(),
            scratch: Vec::new(),
            keys: Vec::new(),
            hist: [0; 256],
        }
    }

    /// The current back-to-front quad order (`quads` indices, far →
    /// near). Empty until the first [`Self::sort`].
    #[inline]
    #[must_use]
    pub fn order(&self) -> &[u32] {
        &self.order
    }

    /// Sort `quads` back-to-front for `cam`, or return
    /// [`SortOutcome::Cached`] unchanged when neither the half-block
    /// camera cell nor `quads_rev` moved.
    pub fn sort(
        &mut self,
        cam: [f32; 3],
        quads_rev: u64,
        quads: &[TranslucentQuadKey],
    ) -> SortOutcome {
        let cell = (
            (cam[0] * 2.0).floor() as i32,
            (cam[1] * 2.0).floor() as i32,
            (cam[2] * 2.0).floor() as i32,
        );
        if self.primed
            && self.last_cam_cell == cell
            && self.last_rev == quads_rev
            && self.order.len() == quads.len()
        {
            return SortOutcome::Cached;
        }

        // Descending-distance u16 keys in 1/256-block units: key 0 is
        // farthest, so an ascending LSB radix emits back-to-front.
        self.keys.clear();
        self.keys.reserve(quads.len());
        for q in quads {
            let dx = q.center[0] - cam[0];
            let dy = q.center[1] - cam[1];
            let dz = q.center[2] - cam[2];
            let d2 = dx * dx + dy * dy + dz * dz;
            let dist = if d2.is_finite() {
                (d2.sqrt() * 256.0).min(65535.0) as u32
            } else {
                0
            };
            self.keys.push(65535 - dist);
        }

        self.order.clear();
        self.order.extend(0..quads.len() as u32);
        self.scratch.clear();
        self.scratch.resize(quads.len(), 0);
        Self::radix_pass(&self.keys, &self.order, &mut self.scratch, &mut self.hist, 0);
        Self::radix_pass(&self.keys, &self.scratch, &mut self.order, &mut self.hist, 8);

        self.primed = true;
        self.last_cam_cell = cell;
        self.last_rev = quads_rev;
        SortOutcome::Resorted
    }

    /// One stable counting-sort pass over byte `shift` of `keys`,
    /// permuting the `src` index buffer into `dst`.
    fn radix_pass(
        keys: &[u32],
        src: &[u32],
        dst: &mut [u32],
        hist: &mut [u32; 256],
        shift: u32,
    ) {
        hist.fill(0);
        for &i in src {
            hist[((keys[i as usize] >> shift) & 0xff) as usize] += 1;
        }
        let mut sum = 0u32;
        for h in hist.iter_mut() {
            let count = *h;
            *h = sum;
            sum += count;
        }
        for &i in src {
            let bucket = ((keys[i as usize] >> shift) & 0xff) as usize;
            dst[hist[bucket] as usize] = i;
            hist[bucket] += 1;
        }
    }
}

impl Default for TranslucentSortCache {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// 5. Render-graph dead-pass culling & transient resource aliasing
// ---------------------------------------------------------------------------

/// Opaque render-pass handle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PassId(pub u32);

/// Opaque transient-resource handle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ResourceId(pub u32);

/// Declared size/class of one transient GPU texture or buffer.
/// Resources may only alias each other when their `kind_tag` matches —
/// a depth buffer never shares a slot with an HDR colour target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResourceDesc {
    /// Allocation size in bytes.
    pub bytes: u64,
    /// Caller-defined resource class (format/usage family); only equal
    /// tags alias.
    pub kind_tag: u32,
}

/// Failure of [`RenderGraph::compile`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderGraphError {
    /// A dependency cycle exists among the passes a root transitively
    /// needs — no valid execution order.
    Cycle,
    /// A pass references a [`ResourceId`] that was never registered.
    UnknownResource {
        /// Offending pass.
        pass: PassId,
        /// Unregistered resource handle.
        resource: ResourceId,
    },
}

impl fmt::Display for RenderGraphError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cycle => write!(f, "render graph contains a dependency cycle"),
            Self::UnknownResource { pass, resource } => write!(
                f,
                "pass {} references unregistered resource {}",
                pass.0, resource.0
            ),
        }
    }
}

impl std::error::Error for RenderGraphError {}

struct GraphPass {
    name: String,
    reads: Vec<ResourceId>,
    writes: Vec<ResourceId>,
    is_root: bool,
}

/// Declarative transient render graph: register resources and passes,
/// then [`RenderGraph::compile`] produces a dead-pass-culled,
/// cycle-checked, alias-coloured execution plan.
#[derive(Default)]
pub struct RenderGraph {
    resources: Vec<ResourceDesc>,
    passes: Vec<GraphPass>,
}

impl RenderGraph {
    /// Empty graph.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register one transient render target/buffer.
    pub fn add_transient_resource(&mut self, desc: ResourceDesc) -> ResourceId {
        let id = ResourceId(self.resources.len() as u32);
        self.resources.push(desc);
        id
    }

    /// Register one pass. `is_side_effect_root` marks swapchain-present
    /// and other passes whose work must survive dead-pass culling even
    /// when no other pass consumes their output.
    pub fn add_pass(
        &mut self,
        name: &str,
        reads: &[ResourceId],
        writes: &[ResourceId],
        is_side_effect_root: bool,
    ) -> PassId {
        let id = PassId(self.passes.len() as u32);
        self.passes.push(GraphPass {
            name: name.to_owned(),
            reads: reads.to_vec(),
            writes: writes.to_vec(),
            is_root: is_side_effect_root,
        });
        id
    }

    /// Number of registered passes.
    #[inline]
    #[must_use]
    pub fn pass_count(&self) -> usize {
        self.passes.len()
    }

    /// Number of registered transient resources.
    #[inline]
    #[must_use]
    pub fn resource_count(&self) -> usize {
        self.resources.len()
    }

    /// Debug name a pass was registered with.
    #[inline]
    #[must_use]
    pub fn pass_name(&self, id: PassId) -> &str {
        &self.passes[id.0 as usize].name
    }

    /// Compile the graph:
    ///
    /// 1. validate every referenced resource,
    /// 2. mark passes reachable *backward* from `is_side_effect_root`
    ///    passes through producer→consumer edges (dead passes become
    ///    [`CompiledRenderGraph::culled_passes`]),
    /// 3. topologically order the live subgraph (Kahn's algorithm with
    ///    a min-heap, so the order is deterministic), reporting
    ///    [`RenderGraphError::Cycle`] when the subgraph cannot close,
    /// 4. compute exact `[first, last]` execution-index lifetimes per
    ///    transient resource,
    /// 5. linear-scan interval-colour lifetimes into the minimum number
    ///    of physical slots (`kind_tag` must match), yielding
    ///    `aliased_slots`, `peak_unaliased_bytes`, `aliased_bytes` and
    ///    `vram_saved_bytes`.
    ///
    /// # Errors
    /// [`RenderGraphError::UnknownResource`] when a pass references an
    /// unregistered resource, [`RenderGraphError::Cycle`] when the
    /// needed subgraph has no topological order.
    pub fn compile(&self) -> Result<CompiledRenderGraph, RenderGraphError> {
        let pass_n = self.passes.len();
        let res_n = self.resources.len();
        for (pi, pass) in self.passes.iter().enumerate() {
            for &r in pass.reads.iter().chain(pass.writes.iter()) {
                if r.0 as usize >= res_n {
                    return Err(RenderGraphError::UnknownResource {
                        pass: PassId(pi as u32),
                        resource: r,
                    });
                }
            }
        }

        // Producer→consumer edges, deduplicated.
        let mut writers: Vec<Vec<u32>> = vec![Vec::new(); res_n];
        let mut readers: Vec<Vec<u32>> = vec![Vec::new(); res_n];
        for (pi, pass) in self.passes.iter().enumerate() {
            for &r in &pass.reads {
                readers[r.0 as usize].push(pi as u32);
            }
            for &r in &pass.writes {
                writers[r.0 as usize].push(pi as u32);
            }
        }
        let mut fwd: Vec<Vec<u32>> = vec![Vec::new(); pass_n];
        let mut rev: Vec<Vec<u32>> = vec![Vec::new(); pass_n];
        for ri in 0..res_n {
            for &w in &writers[ri] {
                for &r in &readers[ri] {
                    if w != r {
                        fwd[w as usize].push(r);
                        rev[r as usize].push(w);
                    }
                }
            }
        }
        for edges in fwd.iter_mut().chain(rev.iter_mut()) {
            edges.sort_unstable();
            edges.dedup();
        }

        // Live set: everything a root transitively depends on.
        let mut needed = vec![false; pass_n];
        let mut stack: Vec<u32> = Vec::new();
        for (pi, pass) in self.passes.iter().enumerate() {
            if pass.is_root {
                needed[pi] = true;
                stack.push(pi as u32);
            }
        }
        while let Some(p) = stack.pop() {
            for &a in &rev[p as usize] {
                if !needed[a as usize] {
                    needed[a as usize] = true;
                    stack.push(a);
                }
            }
        }

        // Deterministic Kahn order over the live subgraph.
        let mut indeg = vec![0u32; pass_n];
        for (a, edges) in fwd.iter().enumerate() {
            if !needed[a] {
                continue;
            }
            for &b in edges {
                if needed[b as usize] {
                    indeg[b as usize] += 1;
                }
            }
        }
        let mut ready: BinaryHeap<Reverse<u32>> = BinaryHeap::new();
        for (pi, &n) in needed.iter().enumerate() {
            if n && indeg[pi] == 0 {
                ready.push(Reverse(pi as u32));
            }
        }
        let mut order: Vec<PassId> = Vec::new();
        while let Some(Reverse(p)) = ready.pop() {
            order.push(PassId(p));
            for &b in &fwd[p as usize] {
                if needed[b as usize] {
                    indeg[b as usize] -= 1;
                    if indeg[b as usize] == 0 {
                        ready.push(Reverse(b));
                    }
                }
            }
        }
        if order.len() != needed.iter().filter(|&&n| n).count() {
            return Err(RenderGraphError::Cycle);
        }
        let culled_passes: Vec<PassId> = (0..pass_n)
            .filter(|&pi| !needed[pi])
            .map(|pi| PassId(pi as u32))
            .collect();

        // Exact lifetimes in execution order.
        let mut lifetimes: Vec<Option<(u32, u32)>> = vec![None; res_n];
        for (pos, &PassId(pi)) in order.iter().enumerate() {
            let pass = &self.passes[pi as usize];
            for &r in pass.reads.iter().chain(pass.writes.iter()) {
                let slot = &mut lifetimes[r.0 as usize];
                *slot = Some(match *slot {
                    Some((first, last)) => (first.min(pos as u32), last.max(pos as u32)),
                    None => (pos as u32, pos as u32),
                });
            }
        }

        // Linear-scan interval colouring: expire a slot the moment its
        // last use precedes the next interval's first use, reuse only
        // within a matching kind_tag.
        let mut intervals: Vec<(u32, u32, u32)> = Vec::new();
        for (ri, lt) in lifetimes.iter().enumerate() {
            if let Some((first, last)) = *lt {
                intervals.push((first, last, ri as u32));
            }
        }
        intervals.sort_unstable();
        let mut aliased_slots: Vec<Option<u32>> = vec![None; res_n];
        let mut slot_bytes: Vec<u64> = Vec::new();
        let mut slot_tag: Vec<u32> = Vec::new();
        let mut free: HashMap<u32, Vec<u32>> = HashMap::new();
        let mut active: BinaryHeap<Reverse<(u32, u32)>> = BinaryHeap::new();
        for (first, last, ri) in intervals {
            while let Some(&Reverse((end, slot))) = active.peek() {
                if end < first {
                    active.pop();
                    free.entry(slot_tag[slot as usize]).or_default().push(slot);
                } else {
                    break;
                }
            }
            let tag = self.resources[ri as usize].kind_tag;
            let slot = match free.get_mut(&tag).and_then(Vec::pop) {
                Some(s) => s,
                None => {
                    let s = slot_bytes.len() as u32;
                    slot_bytes.push(0);
                    slot_tag.push(tag);
                    s
                }
            };
            slot_bytes[slot as usize] =
                slot_bytes[slot as usize].max(self.resources[ri as usize].bytes);
            aliased_slots[ri as usize] = Some(slot);
            active.push(Reverse((last, slot)));
        }

        let peak_unaliased_bytes: u64 = lifetimes
            .iter()
            .enumerate()
            .filter(|(_, lt)| lt.is_some())
            .map(|(ri, _)| self.resources[ri].bytes)
            .sum();
        let aliased_bytes: u64 = slot_bytes.iter().sum();
        let vram_saved_bytes = peak_unaliased_bytes.saturating_sub(aliased_bytes);

        Ok(CompiledRenderGraph {
            order,
            culled_passes,
            lifetimes,
            aliased_slots,
            slot_bytes,
            peak_unaliased_bytes,
            aliased_bytes,
            vram_saved_bytes,
        })
    }
}

/// The compiled execution plan from [`RenderGraph::compile`].
#[derive(Clone, Debug)]
pub struct CompiledRenderGraph {
    /// Deterministic topological execution order of the live passes.
    pub order: Vec<PassId>,
    /// Dead passes unreachable from any root — safe to skip entirely.
    pub culled_passes: Vec<PassId>,
    /// Per-resource `[first, last]` execution-index lifetime; `None`
    /// when no live pass touches the resource.
    pub lifetimes: Vec<Option<(u32, u32)>>,
    /// Per-resource physical slot index; `None` for unused resources.
    pub aliased_slots: Vec<Option<u32>>,
    /// Byte size of each physical slot (max of aliased resources).
    pub slot_bytes: Vec<u64>,
    /// Unaliased baseline: total bytes if every live transient resource
    /// owned a dedicated allocation.
    pub peak_unaliased_bytes: u64,
    /// Physical bytes after slot aliasing.
    pub aliased_bytes: u64,
    /// `peak_unaliased_bytes - aliased_bytes`.
    pub vram_saved_bytes: u64,
}

impl CompiledRenderGraph {
    /// Execution-index lifetime of `resource`, when live.
    #[inline]
    #[must_use]
    pub fn lifetime(&self, resource: ResourceId) -> Option<(u32, u32)> {
        self.lifetimes.get(resource.0 as usize).copied().flatten()
    }

    /// Physical slot `resource` aliases into, when live.
    #[inline]
    #[must_use]
    pub fn slot(&self, resource: ResourceId) -> Option<u32> {
        self.aliased_slots
            .get(resource.0 as usize)
            .copied()
            .flatten()
    }
}
