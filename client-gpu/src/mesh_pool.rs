//! Bounded background worker pool for section geometry and Hi-Z builds.
//!
//! Three job kinds run off the render thread:
//!
//! * [`MeshJob::SectionMesh`] — full section meshing: greedily meshes a
//!   [`SectionMeshInput`] snapshot (center section plus its 26 neighbors,
//!   `Arc`-cloned so the snapshot is immutable) into a packed
//!   [`BuiltSectionMesh`]. [`SectionMeshLane`] tracks the per-coord
//!   submission revision on the render thread — a result whose section
//!   re-dirtied after the snapshot is dropped instead of installed.
//! * [`MeshJob::SectionGeometry`] — speculatively converts a queued
//!   [`PackedVertex`] stream into [`LiveSectionGeometry`] and its
//!   direction-bucket index ranges, so the upload-admission drain on the
//!   render thread can install a section without re-running the split.
//!   Results are matched back to pending uploads by `Arc` identity: a stale
//!   result (the pending entry was replaced by a newer mesh) is discarded
//!   and the install falls back to computing inline.
//! * [`MeshJob::HiZDepth`] — conservatively software-projects the opaque
//!   quads the current frame actually drew onto a coarse depth grid and
//!   builds a [`HiZPyramid`] the next frame's culling consults. The worker
//!   first extracts the only vertex data the splat consumes — each drawn
//!   opaque quad's four packed `position` words — into a pooled compact
//!   stream, so the full [`PackedVertex`] buffers are released before
//!   rasterization instead of being pinned through it.
//!
//! The pool is a bounded `mpsc::sync_channel` work queue shared by a small
//! fixed set of threads. Submission is non-blocking (`try_send`): a full
//! queue drops the speculative job and the render thread falls back to the
//! synchronous path, so backpressure can never stall a frame. Shutdown is a
//! plain channel close plus `join` in [`Drop`]; workers contain no
//! `unwrap`/panicking calls.

use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use std::collections::HashMap;

use megu_client_mesh::ffi::PackedVertex;
use megu_client_mesh::{
    build_and_pack_section_mesh, BuiltSectionMesh, DirectionBuckets, HiZPyramid, SectionCoord,
    SectionMeshInput, DIRECTION_BUCKETS,
};

use crate::renderer::{section_geometry, view_proj_row_major, LiveSectionGeometry};
use crate::Mat4;

/// Coarse depth-grid resolution the live Hi-Z pyramid is built from. Small
/// enough that one splat job costs well under a millisecond per hundred
/// sections, large enough to capture wall-scale occluders.
pub const HIZ_PYRAMID_WIDTH: u32 = 160;
pub const HIZ_PYRAMID_HEIGHT: u32 = 90;

/// Maximum queued jobs. Upload bursts submit one geometry job per dirty
/// section; a full queue drops extras and the inline fallback covers them.
const JOB_QUEUE_BOUND: usize = 96;

/// Hard cap on worker threads regardless of the strategy hint.
const MAX_WORKERS: usize = 8;

/// One section's drawn opaque quads as a Hi-Z occluder source.
pub struct HiZSplatSection {
    /// World-space minimum corner (`coord * 16`).
    pub origin: [f32; 3],
    /// Packed `position` word of every opaque quad vertex, extracted at
    /// install — the only vertex data the splat worker ever reads, so
    /// the section's full CPU mesh copy is released post-upload. The
    /// worker drops this handle once its quads are extracted, so a
    /// queued job pins the buffer only until a worker picks it up.
    pub positions: Arc<[u32]>,
    /// `(first_quad, quad_count)` per direction bucket inside
    /// `positions`, when the world published buckets. `None` splats the
    /// whole stream (matching the unbucketed draw fallback).
    pub bucket_ranges: Option<[(u32, u32); DIRECTION_BUCKETS]>,
    /// Buckets the frame's facing cull kept (bit per bucket); masked-out
    /// direction buckets are not drawn and therefore not occluders.
    pub draw_mask: u8,
}

/// A unit of off-thread work.
pub enum MeshJob {
    /// Mesh one dirty section's voxel snapshot into a packed, upload-ready
    /// [`BuiltSectionMesh`]. The input's `revision` is the
    /// [`SectionMeshLane`] epoch recorded at submission; the drain drops
    /// results whose epoch moved on.
    SectionMesh { input: SectionMeshInput },
    /// Convert a queued packed mesh into upload-ready geometry.
    SectionGeometry {
        coord: SectionCoord,
        packed: Arc<[PackedVertex]>,
        order: Arc<[u32]>,
        buckets: Option<DirectionBuckets>,
    },
    /// Build the next Hi-Z pyramid from this frame's drawn opaque quads.
    HiZDepth {
        /// Column-major camera view-projection the splat uses; stored with
        /// the resulting pyramid so it is only applied to matching frames.
        view_proj: Mat4,
        sections: Vec<HiZSplatSection>,
    },
}

/// A completed off-thread result.
pub enum MeshResult {
    /// The packed mesh one `SectionMesh` job produced.
    SectionMesh { built: BuiltSectionMesh },
    /// `section_geometry` + bucket ranges for one queued section.
    SectionGeometry {
        coord: SectionCoord,
        /// The input mesh, echoed back so the drain can `Arc::ptr_eq`-match
        /// it against the pending upload that requested it.
        packed: Arc<[PackedVertex]>,
        geometry: LiveSectionGeometry,
        /// Per-bucket `(first, count)` ranges into
        /// `geometry.opaque_indices`; `None` when the mesh was unbucketed.
        opaque_bucket_ranges: Option<[(u32, u32); DIRECTION_BUCKETS]>,
    },
    /// A pyramid built from a finished frame's drawn occluders.
    HiZDepth {
        view_proj: Mat4,
        pyramid: Option<HiZPyramid>,
        /// The job's section list, drained empty during quad extraction
        /// but returned with its capacity intact — the renderer recycles
        /// it as the next frame's splat scratch instead of allocating.
        sections: Vec<HiZSplatSection>,
    },
}

/// The shared work queue. Clone-free: every worker holds the same
/// `Arc<Mutex<Receiver>>` and contends on it only to pull the next job.
pub struct MeshJobPool {
    job_tx: Option<SyncSender<MeshJob>>,
    result_rx: Receiver<MeshResult>,
    workers: Vec<JoinHandle<()>>,
    threads: usize,
}

impl MeshJobPool {
    /// Spawn `threads` workers (clamped to `1..=MAX_WORKERS`). Thread
    /// spawn failure is non-fatal: submissions report failure so callers
    /// keep their synchronous fallback.
    pub fn new(threads: u32) -> Self {
        let threads = (threads.max(1) as usize).min(MAX_WORKERS);
        let (job_tx, job_rx) = sync_channel::<MeshJob>(JOB_QUEUE_BOUND);
        let (result_tx, result_rx) = sync_channel::<MeshResult>(JOB_QUEUE_BOUND * 2);
        let job_rx = Arc::new(Mutex::new(job_rx));
        let workers = (0..threads)
            .filter_map(|index| {
                let rx = Arc::clone(&job_rx);
                let tx = result_tx.clone();
                std::thread::Builder::new()
                    .name(format!("mesh-job-{index}"))
                    .spawn(move || worker_loop(rx, tx))
                    .ok()
            })
            .collect();
        Self {
            job_tx: Some(job_tx),
            result_rx,
            workers,
            threads,
        }
    }

    /// Non-blocking job submission. Returns false when the queue is full
    /// or no worker is running — the caller falls back to inline work.
    pub fn try_submit(&self, job: MeshJob) -> bool {
        if self.workers.is_empty() {
            return false;
        }
        self.job_tx
            .as_ref()
            .is_some_and(|tx| tx.try_send(job).is_ok())
    }

    /// Invoke `f` on every finished result without blocking.
    pub fn drain(&self, mut f: impl FnMut(MeshResult)) {
        while let Ok(result) = self.result_rx.try_recv() {
            f(result);
        }
    }

    /// Rebuild the pool when the worker count changes (compute-strategy
    /// updates). In-flight jobs complete; queued jobs are dropped and their
    /// sections install through the inline fallback instead.
    pub fn set_threads(&mut self, threads: u32) {
        let threads = (threads.max(1) as usize).min(MAX_WORKERS);
        if threads != self.threads || self.workers.is_empty() {
            *self = Self::new(threads as u32);
        }
    }
}

impl Drop for MeshJobPool {
    fn drop(&mut self) {
        // Closing the job channel ends every worker's blocking `recv`
        // loop once queued work drains; results the receiver dropped are
        // simply abandoned.
        drop(self.job_tx.take());
        for handle in self.workers.drain(..) {
            let _ = handle.join();
        }
    }
}

fn worker_loop(job_rx: Arc<Mutex<Receiver<MeshJob>>>, result_tx: SyncSender<MeshResult>) {
    // Retained Hi-Z scratch: the depth grid and compact occluder stream
    // stay allocated across jobs, so steady-state frames never allocate
    // on the worker.
    let mut hiz_scratch = HiZScratch::default();
    loop {
        let job = {
            let rx = job_rx.lock().unwrap_or_else(|e| e.into_inner());
            rx.recv()
        };
        let Ok(job) = job else {
            return;
        };
        if result_tx.send(execute(job, &mut hiz_scratch)).is_err() {
            return;
        }
    }
}

fn execute(job: MeshJob, hiz_scratch: &mut HiZScratch) -> MeshResult {
    match job {
        MeshJob::SectionMesh { input } => MeshResult::SectionMesh {
            built: build_and_pack_section_mesh(&input),
        },
        MeshJob::SectionGeometry {
            coord,
            packed,
            order,
            buckets,
        } => {
            let origin = [
                coord.0 as f32 * 16.0,
                coord.1 as f32 * 16.0,
                coord.2 as f32 * 16.0,
            ];
            let geometry = section_geometry(&packed, &order, origin);
            let opaque_bucket_ranges = buckets.map(|b| compute_opaque_bucket_ranges(&packed, b));
            MeshResult::SectionGeometry {
                coord,
                packed,
                geometry,
                opaque_bucket_ranges,
            }
        }
        MeshJob::HiZDepth {
            view_proj,
            mut sections,
        } => MeshResult::HiZDepth {
            view_proj,
            pyramid: build_hiz_pyramid(&view_proj, &mut sections, hiz_scratch),
            // `extract_splat_quads` drained the list — the emptied buffer
            // rides back so the render thread reuses its capacity.
            sections,
        },
    }
}

/// `true` when a packed quad belongs to the translucent stream — the same
/// attribute decode [`section_geometry`] uses (layer 2, or fluid layer
/// with the water kind).
pub fn packed_quad_translucent(attributes: u32) -> bool {
    let layer = (attributes >> 13) & 3;
    let fluid = (attributes >> 15) & 3;
    layer == 2 || (layer == 3 && fluid == 1)
}

/// Map each direction bucket's quad range (in the grouped `packed` stream)
/// onto its `(first_index, index_count)` span inside the opaque index list
/// [`section_geometry`] produces: opaque quads appear in packed order and
/// contribute six indices each.
pub fn compute_opaque_bucket_ranges(
    packed: &[PackedVertex],
    buckets: DirectionBuckets,
) -> [(u32, u32); DIRECTION_BUCKETS] {
    let mut ranges = [(0u32, 0u32); DIRECTION_BUCKETS];
    let mut opaque_indices = 0u32;
    for (bucket, range) in ranges.iter_mut().enumerate() {
        let first = opaque_indices;
        let start = buckets.offset[bucket] as usize;
        let count = buckets.count[bucket] as usize;
        let quad_end = (start.saturating_add(count)).min(packed.len() / 4);
        for quad_index in start.min(quad_end)..quad_end {
            if !packed_quad_translucent(packed[quad_index * 4].attributes) {
                opaque_indices += 6;
            }
        }
        *range = (first, opaque_indices - first);
    }
    ranges
}

/// Render-thread bookkeeping for [`MeshJob::SectionMesh`] fan-out. Every
/// dirty mark bumps the section's *content epoch*; a submission records the
/// epoch it snapshotted as the job's revision. A completed result is
/// accepted only while it is still the in-flight submission *and* no newer
/// dirty mark landed meanwhile — one map lookup, no channel state needed.
///
/// Epoch `0` is the never-touched value: [`Self::mark_dirty`] bumps to `1`
/// on first contact, so an unsubmitted coord never collides with a real
/// revision.
#[derive(Default)]
pub struct SectionMeshLane {
    /// Per-coord content epoch: the number of dirty marks (block edits,
    /// chunk inserts/unloads, light updates, neighbor invalidations —
    /// every call site shares the same mark helpers) a section received.
    /// Wrapping would need `u64::MAX` edits to one section — unreachable.
    epochs: HashMap<SectionCoord, u64>,
    /// `coord -> epoch` of every submitted-but-not-yet-drained job.
    /// Two jobs for the same coord are never simultaneously in flight: a
    /// resubmission bumps the in-flight revision and the earlier result
    /// drops as stale.
    in_flight: HashMap<SectionCoord, u64>,
}

impl SectionMeshLane {
    /// Record one content change for `coord` (the section may or may not
    /// end up in the submission set — epoch bumps are cheaper than
    /// checking eligibility first).
    pub fn mark_dirty(&mut self, coord: SectionCoord) {
        let epoch = self.epochs.entry(coord).or_insert(0);
        *epoch = epoch.wrapping_add(1);
    }

    /// Drop tracking for a section that left the loaded set (unload,
    /// eviction, dimension reset). Any still-in-flight job completes and
    /// gets dropped by [`Self::accept`] — its epoch can no longer match.
    pub fn remove(&mut self, coord: SectionCoord) {
        self.epochs.remove(&coord);
        self.in_flight.remove(&coord);
    }

    /// Clear all tracking (dimension reset / full resync).
    pub fn clear(&mut self) {
        self.epochs.clear();
        self.in_flight.clear();
    }

    /// `true` when no `SectionMesh` job for `coord` is currently in
    /// flight — the caller may snapshot and submit it.
    pub fn can_submit(&self, coord: SectionCoord) -> bool {
        !self.in_flight.contains_key(&coord)
    }

    /// The content epoch last recorded for `coord` (`0` = never marked).
    /// External payload producers stamp this on their output so the drain
    /// can drop entries a later dirty mark superseded.
    pub fn epoch_of(&self, coord: SectionCoord) -> u64 {
        self.epochs.get(&coord).copied().unwrap_or(0)
    }

    /// The revision a new submission for `coord` would carry — the value
    /// [`Self::note_submitted`] records. Read it before snapshotting so
    /// the snapshot's revision and the in-flight marker cannot diverge.
    pub fn pending_revision(&self, coord: SectionCoord) -> u64 {
        self.epoch_of(coord) + 1
    }

    /// Record a submission and return the revision the snapshot must
    /// carry: the epoch current at submission time. `+1` keeps revision
    /// `0` reserved for the never-marked state.
    pub fn note_submitted(&mut self, coord: SectionCoord) -> u64 {
        let revision = self.epochs.get(&coord).copied().unwrap_or(0) + 1;
        self.in_flight.insert(coord, revision);
        revision
    }

    /// Result gate: `true` only when `built` is still the in-flight
    /// submission for its coord *and* the content epoch did not advance
    /// after its snapshot was taken — i.e. no later edit invalidated the
    /// meshed data. Accepted results consume their in-flight slot; stale
    /// ones keep the newer in-flight marker (or leave the slot clear for
    /// a resubmission the caller decides to make).
    pub fn accept(&mut self, built: &BuiltSectionMesh) -> bool {
        if self.in_flight.get(&built.coord) != Some(&built.revision) {
            return false;
        }
        let epoch = self.epochs.get(&built.coord).copied().unwrap_or(0) + 1;
        if epoch != built.revision {
            // The section re-dirtied after the snapshot: this result is
            // stale — free the slot so the new content gets resubmitted.
            self.in_flight.remove(&built.coord);
            return false;
        }
        self.in_flight.remove(&built.coord);
        true
    }
}

/// The Hi-Z splat worker only reads each opaque quad's packed `position`
/// word — extracting it at install lets the full packed mesh leave the
/// CPU right after upload. Translucent quads never occlude and are
/// dropped here. With `DirectionBuckets` the stream is emitted
/// bucket-major so every bucket's `(first_quad, quad_count)` span is
/// contiguous and disjoint — a masked bucket's quads can never leak into
/// a drawn neighbour's span (the interleaved translucent quads would
/// have compacted the ranges into each other). Quads outside every
/// bucket range are dropped exactly as the old per-bucket iteration
/// skipped them. `None` keeps the original packed order.
pub fn build_hiz_stream(
    packed: &[PackedVertex],
    buckets: Option<DirectionBuckets>,
) -> (Vec<u32>, Option<[(u32, u32); DIRECTION_BUCKETS]>) {
    let quad_total = packed.len() / 4;
    let mut positions = Vec::with_capacity(quad_total * 4);
    let Some(b) = buckets else {
        for quad in packed.as_chunks::<4>().0.iter() {
            if packed_quad_translucent(quad[0].attributes) {
                continue;
            }
            for vertex in quad {
                positions.push(vertex.position);
            }
        }
        return (positions, None);
    };
    let mut ranges = [(0u32, 0u32); DIRECTION_BUCKETS];
    for (bucket, range) in ranges.iter_mut().enumerate() {
        let first = (positions.len() / 4) as u32;
        let begin = b.offset[bucket] as usize;
        let end = begin
            .saturating_add(b.count[bucket] as usize)
            .min(quad_total);
        for quad_index in begin.min(end)..end {
            let quad = &packed[quad_index * 4..quad_index * 4 + 4];
            if packed_quad_translucent(quad[0].attributes) {
                continue;
            }
            for vertex in quad {
                positions.push(vertex.position);
            }
        }
        *range = (first, (positions.len() / 4 - first as usize) as u32);
    }
    (positions, Some(ranges))
}

/// One extracted occluder quad: the four packed `position` words and
/// nothing else. Every other `PackedVertex` field (and every translucent
/// quad) is discarded at extraction, so the raster pass streams 16 bytes
/// per quad instead of the 64-byte vertex stride.
struct SplatQuad {
    positions: [u32; 4],
}

/// `(origin, quads range)` of one contributing section — the raster pass
/// walks [`HiZScratch::quads`] span by span, mirroring the old
/// per-section iteration order.
struct SplatSpan {
    origin: [f32; 3],
    start: usize,
    end: usize,
}

/// Per-worker retained buffers for [`MeshJob::HiZDepth`] jobs. The depth
/// grid and the compact occluder stream persist across jobs, so a
/// steady-state frame allocates nothing on the worker — previously every
/// job allocated a fresh `vec![1.0; width * height]` grid.
#[derive(Default)]
struct HiZScratch {
    /// `HIZ_PYRAMID_WIDTH * HIZ_PYRAMID_HEIGHT` max-occluder texels,
    /// refilled to `1.0` (farthest = no occluder) before each splat.
    depth: Vec<f32>,
    /// Compact drawn-opaque quad stream the raster pass consumes.
    quads: Vec<SplatQuad>,
    /// One entry per section that contributed at least one quad.
    spans: Vec<SplatSpan>,
}

/// Project every drawn opaque quad of `sections` onto a coarse
/// max-occluder depth grid and wrap it in a [`HiZPyramid`]. Conservative
/// by construction: a texel is only claimed when the projected quad covers
/// its center with margin, the claimed depth is the quad's *far* corner
/// (never nearer than the true surface), and quads that clip the near
/// plane are dropped rather than approximated.
fn build_hiz_pyramid(
    view_proj: &Mat4,
    sections: &mut Vec<HiZSplatSection>,
    scratch: &mut HiZScratch,
) -> Option<HiZPyramid> {
    let (width, height) = (HIZ_PYRAMID_WIDTH as usize, HIZ_PYRAMID_HEIGHT as usize);
    extract_splat_quads(sections, scratch);
    scratch.depth.clear();
    scratch.depth.resize(width * height, 1.0);
    let rows = view_proj_row_major(view_proj);
    for span in &scratch.spans {
        for quad in &scratch.quads[span.start..span.end] {
            splat_quad(&mut scratch.depth, width, height, &rows, span.origin, quad);
        }
    }
    HiZPyramid::from_depth_buffer(width, height, &scratch.depth).ok()
}

/// Extract every splat-eligible quad of `sections` into `scratch.quads`:
/// bucket ranges filtered by `draw_mask` — exactly the set the inline
/// splat used to walk (the positions-only stream already excludes
/// translucent quads, which never occlude). Draining the section list
/// drops each `positions` handle as soon as its quads are read, so a
/// `HiZDepth` job pins the buffer only until extraction finishes, not
/// through rasterization and the channel round-trip back — and the
/// emptied `Vec` keeps its capacity for the renderer's next job.
fn extract_splat_quads(sections: &mut Vec<HiZSplatSection>, scratch: &mut HiZScratch) {
    scratch.quads.clear();
    scratch.spans.clear();
    for section in sections.drain(..) {
        let first = scratch.quads.len();
        let quad_total = section.positions.len() / 4;
        match section.bucket_ranges {
            Some(ranges) => {
                for (bucket, &(first, count)) in ranges.iter().enumerate() {
                    if section.draw_mask >> bucket & 1 == 0 {
                        continue;
                    }
                    extract_quad_range(
                        &section.positions,
                        first as usize,
                        count as usize,
                        &mut scratch.quads,
                    );
                }
            }
            None => {
                extract_quad_range(&section.positions, 0, quad_total, &mut scratch.quads)
            }
        }
        if scratch.quads.len() > first {
            scratch.spans.push(SplatSpan {
                origin: section.origin,
                start: first,
                end: scratch.quads.len(),
            });
        }
    }
}

/// Copy the quads of `positions[quad_start..+quad_count]` into `out`,
/// with the same range clamp the inline splat used.
fn extract_quad_range(
    positions: &[u32],
    quad_start: usize,
    quad_count: usize,
    out: &mut Vec<SplatQuad>,
) {
    let quad_end = (quad_start.saturating_add(quad_count)).min(positions.len() / 4);
    for quad_index in quad_start.min(quad_end)..quad_end {
        let quad = &positions[quad_index * 4..quad_index * 4 + 4];
        out.push(SplatQuad {
            positions: [quad[0], quad[1], quad[2], quad[3]],
        });
    }
}

/// Screen-space z-splat of one opaque quad: project the four corners with
/// the row-major view-projection, then raise `depth` at every texel whose
/// center is strictly inside the projected quad to the quad's farthest
/// corner depth.
fn splat_quad(
    depth: &mut [f32],
    width: usize,
    height: usize,
    rows: &[[f32; 4]; 4],
    origin: [f32; 3],
    quad: &SplatQuad,
) {
    // Texel-space inset for the point-in-quad test: keeps texels whose
    // centers sit on the quad edge (ambiguous under rasterizer fill rules)
    // from claiming occluder coverage they may not actually have.
    const INSIDE_MARGIN: f32 = 0.02;
    let mut px = [0.0f32; 4];
    let mut py = [0.0f32; 4];
    let mut far_z = 0.0f32;
    for (corner, &position) in quad.positions.iter().enumerate() {
        // The same decode the terrain vertex shader runs.
        let lx = (position & 31) as f32;
        let lz = ((position >> 5) & 31) as f32;
        let ly = (position >> 10) as f32 / 1024.0;
        let world = [origin[0] + lx, origin[1] + ly, origin[2] + lz];
        let clip = [
            rows[0][0] * world[0] + rows[0][1] * world[1] + rows[0][2] * world[2] + rows[0][3],
            rows[1][0] * world[0] + rows[1][1] * world[1] + rows[1][2] * world[2] + rows[1][3],
            rows[2][0] * world[0] + rows[2][1] * world[1] + rows[2][2] * world[2] + rows[2][3],
            rows[3][0] * world[0] + rows[3][1] * world[1] + rows[3][2] * world[2] + rows[3][3],
        ];
        let w = clip[3];
        // A corner on or behind the near plane would smear the quad across
        // the whole screen; skipping it under-covers, which is safe.
        if !w.is_finite() || w <= 0.0 {
            return;
        }
        let ndc = [clip[0] / w, clip[1] / w, clip[2] / w];
        if !ndc.iter().all(|v| v.is_finite()) {
            return;
        }
        px[corner] = (ndc[0] * 0.5 + 0.5) * width as f32;
        py[corner] = (ndc[1] * 0.5 + 0.5) * height as f32;
        far_z = far_z.max(ndc[2]);
    }
    if !(0.0..=1.0).contains(&far_z) {
        return;
    }
    let min_x = px.iter().fold(f32::INFINITY, |a, &b| a.min(b));
    let max_x = px.iter().fold(f32::NEG_INFINITY, |a, &b| a.max(b));
    let min_y = py.iter().fold(f32::INFINITY, |a, &b| a.min(b));
    let max_y = py.iter().fold(f32::NEG_INFINITY, |a, &b| a.max(b));
    let x0 = (min_x.floor() as i32).clamp(0, width as i32);
    let x1 = (max_x.ceil() as i32).clamp(0, width as i32);
    let y0 = (min_y.floor() as i32).clamp(0, height as i32);
    let y1 = (max_y.ceil() as i32).clamp(0, height as i32);
    for ty in y0..y1 {
        let cy = ty as f32 + 0.5;
        for tx in x0..x1 {
            let cx = tx as f32 + 0.5;
            // Inside the convex projected quad iff the center is more than
            // INSIDE_MARGIN texels inside every edge, on either winding.
            let mut all_pos = true;
            let mut all_neg = true;
            for edge in 0..4 {
                let (ax, ay) = (px[edge], py[edge]);
                let (bx, by) = (px[(edge + 1) % 4], py[(edge + 1) % 4]);
                let (ex, ey) = (bx - ax, by - ay);
                let len = (ex * ex + ey * ey).sqrt().max(1.0e-6);
                let dist = (ex * (cy - ay) - ey * (cx - ax)) / len;
                all_pos &= dist > INSIDE_MARGIN;
                all_neg &= dist < -INSIDE_MARGIN;
            }
            if all_pos || all_neg {
                // Nearest occluder wins the texel; `far_z` is already the
                // quad's farthest corner, so the stored depth can only be
                // farther than the true surface — never nearer.
                let texel = &mut depth[ty as usize * width + tx as usize];
                *texel = texel.min(far_z);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One scene section in the pre-refactor input shape — raw packed
    /// quads + `DirectionBuckets` — so the golden reference below still
    /// exercises the old extraction semantics (bucket ranges over the
    /// full packed stream with the translucent skip inside).
    struct SceneEntry {
        origin: [f32; 3],
        quads: Vec<[PackedVertex; 4]>,
        buckets: Option<DirectionBuckets>,
        draw_mask: u8,
    }

    /// The pre-refactor splat, copied verbatim as the golden reference:
    /// if the compact extraction changes which quads are drawn or what
    /// depth they claim, this grid diverges.
    fn legacy_depth_grid(view_proj: &Mat4, entries: &[SceneEntry]) -> Vec<f32> {
        let (width, height) = (HIZ_PYRAMID_WIDTH as usize, HIZ_PYRAMID_HEIGHT as usize);
        let mut depth = vec![1.0f32; width * height];
        let rows = view_proj_row_major(view_proj);
        for entry in entries {
            let quad_total = entry.quads.len();
            match entry.buckets {
                Some(buckets) => {
                    for bucket in 0..DIRECTION_BUCKETS {
                        if entry.draw_mask >> bucket & 1 == 0 {
                            continue;
                        }
                        legacy_splat_quad_range(
                            &mut depth,
                            width,
                            height,
                            &rows,
                            entry,
                            buckets.offset[bucket] as usize,
                            (buckets.count[bucket] as usize).min(quad_total),
                        );
                    }
                }
                None => legacy_splat_quad_range(
                    &mut depth, width, height, &rows, entry, 0, quad_total,
                ),
            }
        }
        depth
    }

    fn legacy_splat_quad_range(
        depth: &mut [f32],
        width: usize,
        height: usize,
        rows: &[[f32; 4]; 4],
        entry: &SceneEntry,
        quad_start: usize,
        quad_count: usize,
    ) {
        let quad_end = (quad_start.saturating_add(quad_count)).min(entry.quads.len());
        for quad_index in quad_start.min(quad_end)..quad_end {
            let quad = &entry.quads[quad_index];
            if packed_quad_translucent(quad[0].attributes) {
                continue;
            }
            legacy_splat_quad(depth, width, height, rows, entry.origin, quad);
        }
    }

    fn legacy_splat_quad(
        depth: &mut [f32],
        width: usize,
        height: usize,
        rows: &[[f32; 4]; 4],
        origin: [f32; 3],
        quad: &[PackedVertex],
    ) {
        const INSIDE_MARGIN: f32 = 0.02;
        let mut px = [0.0f32; 4];
        let mut py = [0.0f32; 4];
        let mut far_z = 0.0f32;
        for (corner, vertex) in quad.iter().enumerate() {
            let lx = (vertex.position & 31) as f32;
            let lz = ((vertex.position >> 5) & 31) as f32;
            let ly = (vertex.position >> 10) as f32 / 1024.0;
            let world = [origin[0] + lx, origin[1] + ly, origin[2] + lz];
            let clip = [
                rows[0][0] * world[0] + rows[0][1] * world[1] + rows[0][2] * world[2]
                    + rows[0][3],
                rows[1][0] * world[0] + rows[1][1] * world[1] + rows[1][2] * world[2]
                    + rows[1][3],
                rows[2][0] * world[0] + rows[2][1] * world[1] + rows[2][2] * world[2]
                    + rows[2][3],
                rows[3][0] * world[0] + rows[3][1] * world[1] + rows[3][2] * world[2]
                    + rows[3][3],
            ];
            let w = clip[3];
            if !w.is_finite() || w <= 0.0 {
                return;
            }
            let ndc = [clip[0] / w, clip[1] / w, clip[2] / w];
            if !ndc.iter().all(|v| v.is_finite()) {
                return;
            }
            px[corner] = (ndc[0] * 0.5 + 0.5) * width as f32;
            py[corner] = (ndc[1] * 0.5 + 0.5) * height as f32;
            far_z = far_z.max(ndc[2]);
        }
        if !(0.0..=1.0).contains(&far_z) {
            return;
        }
        let min_x = px.iter().fold(f32::INFINITY, |a, &b| a.min(b));
        let max_x = px.iter().fold(f32::NEG_INFINITY, |a, &b| a.max(b));
        let min_y = py.iter().fold(f32::INFINITY, |a, &b| a.min(b));
        let max_y = py.iter().fold(f32::NEG_INFINITY, |a, &b| a.max(b));
        let x0 = (min_x.floor() as i32).clamp(0, width as i32);
        let x1 = (max_x.ceil() as i32).clamp(0, width as i32);
        let y0 = (min_y.floor() as i32).clamp(0, height as i32);
        let y1 = (max_y.ceil() as i32).clamp(0, height as i32);
        for ty in y0..y1 {
            let cy = ty as f32 + 0.5;
            for tx in x0..x1 {
                let cx = tx as f32 + 0.5;
                let mut all_pos = true;
                let mut all_neg = true;
                for edge in 0..4 {
                    let (ax, ay) = (px[edge], py[edge]);
                    let (bx, by) = (px[(edge + 1) % 4], py[(edge + 1) % 4]);
                    let (ex, ey) = (bx - ax, by - ay);
                    let len = (ex * ex + ey * ey).sqrt().max(1.0e-6);
                    let dist = (ex * (cy - ay) - ey * (cx - ax)) / len;
                    all_pos &= dist > INSIDE_MARGIN;
                    all_neg &= dist < -INSIDE_MARGIN;
                }
                if all_pos || all_neg {
                    let texel = &mut depth[ty as usize * width + tx as usize];
                    *texel = texel.min(far_z);
                }
            }
        }
    }

    /// xorshift64 — deterministic scene generation without extra deps.
    struct Rng(u64);

    impl Rng {
        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next_u64() % n as u64) as usize
        }

        fn range_f32(&mut self, lo: f32, hi: f32) -> f32 {
            let unit = (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32;
            lo + (hi - lo) * unit
        }
    }

    /// `clip = [scale*x, scale*y, z - near, z]` — perspective looking
    /// down +z from the origin, wgpu-style `z_ndc` in `[0, 1)` for
    /// `z > near`.
    fn test_view_proj(scale: f32, near: f32) -> Mat4 {
        let mut m: Mat4 = [[0.0; 4]; 4];
        m[0][0] = scale;
        m[1][1] = scale;
        m[2][2] = 1.0;
        m[3][2] = -near;
        m[2][3] = 1.0;
        m
    }

    /// Pack a vertex the way the mesher does: integer `x`/`z`, `y` in
    /// 1/1024 fixed point.
    fn vertex(x: f32, y: f32, z: f32, attributes: u32) -> PackedVertex {
        PackedVertex {
            position: (x as u32) | ((z as u32) << 5) | (((y * 1024.0) as u32) << 10),
            attributes,
            ..PackedVertex::default()
        }
    }

    const OPAQUE: u32 = 0;
    const TRANSLUCENT: u32 = 2 << 13;
    const WATER: u32 = (3 << 13) | (1 << 15);

    /// Flat quad in the local x/y plane (the -z/+z face shape).
    fn xy_quad(x0: f32, y0: f32, x1: f32, y1: f32, z: f32, attr: u32) -> [PackedVertex; 4] {
        [
            vertex(x0, y0, z, attr),
            vertex(x1, y0, z, attr),
            vertex(x1, y1, z, attr),
            vertex(x0, y1, z, attr),
        ]
    }

    /// Flat quad in the local z/y plane (the -x/+x face shape).
    fn zy_quad(z0: f32, y0: f32, z1: f32, y1: f32, x: f32, attr: u32) -> [PackedVertex; 4] {
        [
            vertex(x, y0, z0, attr),
            vertex(x, y0, z1, attr),
            vertex(x, y1, z1, attr),
            vertex(x, y1, z0, attr),
        ]
    }

    /// Flat quad in the local x/z plane (the -y/+y face shape).
    fn xz_quad(x0: f32, z0: f32, x1: f32, z1: f32, y: f32, attr: u32) -> [PackedVertex; 4] {
        [
            vertex(x0, y, z0, attr),
            vertex(x1, y, z0, attr),
            vertex(x1, y, z1, attr),
            vertex(x0, y, z1, attr),
        ]
    }

    fn make_section(
        origin: [f32; 3],
        quads: &[[PackedVertex; 4]],
        buckets: Option<DirectionBuckets>,
        draw_mask: u8,
    ) -> HiZSplatSection {
        let packed: Vec<PackedVertex> = quads.iter().flatten().copied().collect();
        let (positions, bucket_ranges) = build_hiz_stream(&packed, buckets);
        HiZSplatSection {
            origin,
            positions: positions.into(),
            bucket_ranges,
            draw_mask,
        }
    }

    fn clone_sections(sections: &[HiZSplatSection]) -> Vec<HiZSplatSection> {
        sections
            .iter()
            .map(|s| HiZSplatSection {
                origin: s.origin,
                positions: Arc::clone(&s.positions),
                bucket_ranges: s.bucket_ranges,
                draw_mask: s.draw_mask,
            })
            .collect()
    }

    /// Contiguous bucket ranges covering `quad_count` quads, with
    /// `counts` taken verbatim (so callers can forge out-of-range spans).
    fn buckets_with_counts(counts: [u32; DIRECTION_BUCKETS]) -> DirectionBuckets {
        let mut buckets = DirectionBuckets {
            offset: [0; DIRECTION_BUCKETS],
            count: counts,
        };
        let mut next = 0u32;
        for (offset, &count) in buckets.offset.iter_mut().zip(&counts) {
            *offset = next;
            next = next.saturating_add(count);
        }
        buckets
    }

    fn random_buckets(rng: &mut Rng, quad_count: usize) -> DirectionBuckets {
        let mut counts = [0u32; DIRECTION_BUCKETS];
        let mut remaining = quad_count as u32;
        for count in counts.iter_mut().take(DIRECTION_BUCKETS - 1) {
            let take = rng.below(remaining as usize + 1) as u32;
            *count = take;
            remaining -= take;
        }
        counts[DIRECTION_BUCKETS - 1] = remaining;
        buckets_with_counts(counts)
    }

    /// A mixed scene exercising every splat branch: bucketed/unbucketed
    /// sections, masked buckets, translucent + water quads, empty
    /// sections, out-of-range bucket spans, and quads crossing the near
    /// plane / sitting behind the camera. Returned in the legacy input
    /// shape; the compact sections derive from the same quads.
    fn random_scene(rng: &mut Rng) -> Vec<SceneEntry> {
        let mut entries = Vec::new();
        for _ in 0..10 {
            let origin = [
                (rng.below(5) as f32 - 2.0) * 32.0,
                (rng.below(4) as f32 - 2.0) * 16.0,
                4.0 + rng.below(5) as f32 * 16.0,
            ];
            let quad_count = 4 + rng.below(80);
            let mut quads = Vec::with_capacity(quad_count);
            for _ in 0..quad_count {
                let attr = match rng.below(12) {
                    0 => TRANSLUCENT,
                    1 => WATER,
                    _ => OPAQUE,
                };
                // Mostly 1-block face quads; every eighth quad spans the
                // whole section so the grid definitely claims texels.
                let quad = if rng.below(8) == 0 {
                    match rng.below(3) {
                        0 => xy_quad(0.0, 0.0, 16.0, 16.0, rng.below(16) as f32, attr),
                        1 => zy_quad(0.0, 0.0, 16.0, 16.0, rng.below(16) as f32, attr),
                        _ => xz_quad(0.0, 0.0, 16.0, 16.0, rng.below(64) as f32 / 4.0, attr),
                    }
                } else {
                    let (x0, z0, y) = (
                        rng.below(15) as f32,
                        rng.below(15) as f32,
                        rng.below(64) as f32 / 4.0,
                    );
                    match rng.below(3) {
                        0 => xy_quad(x0, y, x0 + 1.0, (y + 1.0).min(16.0), z0, attr),
                        1 => zy_quad(z0, y, z0 + 1.0, (y + 1.0).min(16.0), x0, attr),
                        _ => xz_quad(x0, z0, x0 + 1.0, z0 + 1.0, y, attr),
                    }
                };
                quads.push(quad);
            }
            let (buckets, draw_mask) = match rng.below(4) {
                0 => (None, 0xff),
                1 => (Some(random_buckets(rng, quad_count)), 0xff),
                _ => (
                    Some(random_buckets(rng, quad_count)),
                    (1 + rng.below(127)) as u8,
                ),
            };
            entries.push(SceneEntry { origin, quads, buckets, draw_mask });
        }
        // Empty section: contributes no span on either path.
        entries.push(SceneEntry {
            origin: [0.0, 0.0, 32.0],
            quads: Vec::new(),
            buckets: None,
            draw_mask: 0xff,
        });
        // Out-of-range bucket offsets/counts must clamp identically.
        entries.push(SceneEntry {
            origin: [0.0, 0.0, 24.0],
            quads: vec![xy_quad(0.0, 0.0, 16.0, 16.0, 8.0, OPAQUE)],
            buckets: Some(buckets_with_counts([0, 0, u32::MAX, 3, 0, 1, 0])),
            draw_mask: 0xff,
        });
        // Quads crossing the near plane and fully behind the camera:
        // dropped corner-for-corner by both paths.
        entries.push(SceneEntry {
            origin: [0.0, 0.0, -8.0],
            quads: vec![
                xy_quad(0.0, 0.0, 16.0, 16.0, 8.0, OPAQUE),
                xy_quad(0.0, 0.0, 16.0, 16.0, 4.0, OPAQUE),
                xy_quad(0.0, 0.0, 16.0, 16.0, 0.0, OPAQUE),
            ],
            buckets: None,
            draw_mask: 0xff,
        });
        entries.push(SceneEntry {
            origin: [-16.0, -16.0, -40.0],
            quads: vec![xy_quad(0.0, 0.0, 16.0, 16.0, 8.0, OPAQUE)],
            buckets: None,
            draw_mask: 0xff,
        });
        entries
    }

    /// Bit-identical depth grid + every mip + identical cull verdicts on
    /// an AABB battery: the observable culling output (the visible set)
    /// is unchanged by compact extraction.
    #[test]
    fn compact_splat_matches_legacy_grid_and_verdicts_bit_exact() {
        let mut rng = Rng(0x0048_5A5F_5241_4D45);
        let view_projs = [
            test_view_proj(1.0, 0.5),
            test_view_proj(0.25, 1.0),
            test_view_proj(2.5, 0.05),
        ];
        for (case, vp) in view_projs.iter().enumerate() {
            let scene = random_scene(&mut rng);
            let legacy = legacy_depth_grid(vp, &scene);
            let mut sections: Vec<HiZSplatSection> = scene
                .iter()
                .map(|e| make_section(e.origin, &e.quads, e.buckets, e.draw_mask))
                .collect();
            let mut scratch = HiZScratch::default();
            let pyramid = build_hiz_pyramid(vp, &mut sections, &mut scratch)
                .expect("pyramid builds");
            assert_eq!(
                scratch.depth, legacy,
                "case {case}: compact splat grid diverged from legacy"
            );
            let legacy_pyramid = HiZPyramid::from_depth_buffer(
                HIZ_PYRAMID_WIDTH as usize,
                HIZ_PYRAMID_HEIGHT as usize,
                &legacy,
            )
            .expect("legacy pyramid");
            assert_eq!(legacy_pyramid.mip_count(), pyramid.mip_count());
            for level in 0..legacy_pyramid.mip_count() {
                assert_eq!(
                    legacy_pyramid.mip(level),
                    pyramid.mip(level),
                    "case {case} mip {level} diverged"
                );
            }
            let rows = view_proj_row_major(vp);
            for _ in 0..512 {
                let min = [
                    rng.range_f32(-96.0, 96.0),
                    rng.range_f32(-48.0, 48.0),
                    rng.range_f32(-32.0, 220.0),
                ];
                let max = [
                    min[0] + rng.range_f32(1.0, 40.0),
                    min[1] + rng.range_f32(1.0, 40.0),
                    min[2] + rng.range_f32(1.0, 40.0),
                ];
                assert_eq!(
                    legacy_pyramid.cull_world_aabb(&rows, min, max),
                    pyramid.cull_world_aabb(&rows, min, max),
                    "case {case}: cull verdict differs at {min:?}..{max:?}"
                );
            }
        }
    }

    /// Known-answer contract: a full-screen occluder culls sections
    /// strictly behind it and nothing else — the visible set the
    /// renderer consumes.
    #[test]
    fn occluder_wall_produces_expected_visible_set() {
        let vp = test_view_proj(1.0, 0.5);
        // Full-section face quad: origin -8 + local [0,16] → world
        // [-8,8] in x/y, local z 1 + origin z 4 → world z = 5.
        let wall = make_section(
            [-8.0, -8.0, 4.0],
            &[xy_quad(0.0, 0.0, 16.0, 16.0, 1.0, OPAQUE)],
            None,
            0xff,
        );
        let mut scratch = HiZScratch::default();
        let pyramid = build_hiz_pyramid(&vp, &mut vec![wall], &mut scratch).expect("pyramid");
        // The wall covers every texel at ndc z = (5 - 0.5) / 5.
        let expected = (5.0f32 - 0.5) / 5.0;
        assert!(
            scratch.depth.iter().all(|&d| d == expected),
            "every texel claims the wall depth {expected}"
        );
        let rows = view_proj_row_major(&vp);
        assert!(pyramid.cull_world_aabb(&rows, [-2.0, -2.0, 40.0], [2.0, 2.0, 44.0]));
        // Nearer than the wall: never culled.
        assert!(!pyramid.cull_world_aabb(&rows, [-2.0, -2.0, 1.0], [2.0, 2.0, 3.0]));
        // Behind the camera: never culled.
        assert!(!pyramid.cull_world_aabb(&rows, [-2.0, -2.0, -30.0], [2.0, 2.0, -20.0]));
    }

    /// Masked-out buckets and translucent quads must contribute nothing:
    /// the grid stays `1.0`, so no AABB can ever report occluded.
    #[test]
    fn masked_buckets_and_translucent_quads_never_splat() {
        let vp = test_view_proj(1.0, 0.5);
        let quads = [
            xy_quad(0.0, 0.0, 16.0, 16.0, 8.0, OPAQUE),       // bucket 0 (masked)
            xy_quad(0.0, 0.0, 16.0, 16.0, 9.0, TRANSLUCENT),  // translucent
            xy_quad(0.0, 0.0, 16.0, 16.0, 10.0, WATER),       // water
        ];
        let mut sections = vec![make_section(
            [-8.0, -8.0, 8.0],
            &quads,
            Some(buckets_with_counts([1, 0, 1, 1, 0, 0, 0])),
            // Only buckets 1 and 4 draw — both empty of opaque quads.
            (1 << 1) | (1 << 4),
        )];
        let mut scratch = HiZScratch::default();
        let pyramid = build_hiz_pyramid(&vp, &mut sections, &mut scratch).expect("pyramid");
        assert!(scratch.depth.iter().all(|&d| d == 1.0));
        let rows = view_proj_row_major(&vp);
        assert!(!pyramid.cull_world_aabb(&rows, [-2.0, -2.0, 60.0], [2.0, 2.0, 64.0]));
    }

    /// The pooled scratch keeps its capacity across jobs and the second
    /// build still produces the correct grid — pooling never replays
    /// stale state.
    #[test]
    fn pooled_scratch_survives_jobs_without_realloc() {
        let mut rng = Rng(0xBEEF_CAFE);
        let vp = test_view_proj(1.0, 0.5);
        let big: Vec<HiZSplatSection> = random_scene(&mut rng)
            .iter()
            .map(|e| make_section(e.origin, &e.quads, e.buckets, e.draw_mask))
            .collect();
        let small_entry = SceneEntry {
            origin: [-8.0, -8.0, 4.0],
            quads: vec![xy_quad(0.0, 0.0, 16.0, 16.0, 1.0, OPAQUE)],
            buckets: None,
            draw_mask: 0xff,
        };
        let small = vec![make_section(
            small_entry.origin,
            &small_entry.quads,
            small_entry.buckets,
            small_entry.draw_mask,
        )];
        let mut scratch = HiZScratch::default();
        let _ = build_hiz_pyramid(&vp, &mut clone_sections(&big), &mut scratch);
        let (depth_cap, quads_cap, spans_cap) = (
            scratch.depth.capacity(),
            scratch.quads.capacity(),
            scratch.spans.capacity(),
        );
        assert_eq!(
            scratch.depth.len(),
            (HIZ_PYRAMID_WIDTH * HIZ_PYRAMID_HEIGHT) as usize
        );
        assert!(quads_cap > 0, "first job populated the compact stream");
        let pyramid =
            build_hiz_pyramid(&vp, &mut clone_sections(&small), &mut scratch).expect("pyramid");
        assert_eq!(scratch.depth.capacity(), depth_cap);
        assert_eq!(scratch.quads.capacity(), quads_cap);
        assert_eq!(scratch.spans.capacity(), spans_cap);
        // Correct, not a stale replay: the small scene's grid matches the
        // legacy reference computed independently.
        let legacy = legacy_depth_grid(&vp, std::slice::from_ref(&small_entry));
        assert_eq!(scratch.depth, legacy);
        let rows = view_proj_row_major(&vp);
        assert!(pyramid.cull_world_aabb(&rows, [-2.0, -2.0, 40.0], [2.0, 2.0, 44.0]));
    }

    fn next_result(pool: &MeshJobPool) -> MeshResult {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            let mut out = None;
            pool.drain(|result| out = Some(result));
            if let Some(result) = out {
                return result;
            }
            assert!(std::time::Instant::now() < deadline, "pool result timed out");
            // Sleep, not yield: under the full test harness many tests
            // wait on their own pools at once — busy-yielding waiters can
            // starve the single worker thread past the deadline.
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// End-to-end through a real worker: the job builds the same
    /// occlusion verdict the inline path would, and the worker releases
    /// the occluder-stream `Arc` before the result lands — nothing keeps
    /// `positions` alive past extraction.
    #[test]
    fn hiz_depth_job_releases_vertex_buffers_and_matches_verdict() {
        let pool = MeshJobPool::new(2);
        let vp = test_view_proj(1.0, 0.5);
        let section = make_section(
            [-8.0, -8.0, 4.0],
            &[xy_quad(0.0, 0.0, 16.0, 16.0, 1.0, OPAQUE)],
            None,
            0xff,
        );
        let weak = Arc::downgrade(&section.positions);
        assert!(pool.try_submit(MeshJob::HiZDepth {
            view_proj: vp,
            sections: vec![section],
        }));
        let MeshResult::HiZDepth {
            view_proj,
            pyramid,
            sections,
        } = next_result(&pool) else {
            panic!("expected HiZDepth result");
        };
        assert_eq!(view_proj, vp);
        assert!(
            sections.is_empty() && sections.capacity() >= 1,
            "the result returns the drained section list for scratch reuse"
        );
        let pyramid = pyramid.expect("pyramid built");
        let rows = view_proj_row_major(&vp);
        assert!(pyramid.cull_world_aabb(&rows, [-2.0, -2.0, 40.0], [2.0, 2.0, 44.0]));
        assert!(
            weak.upgrade().is_none(),
            "the finished job must not retain the vertex buffer"
        );
    }

    /// Two consecutive Hi-Z jobs on one pool: the second reuses the
    /// worker's scratch and still produces an independent, correct
    /// pyramid.
    #[test]
    fn consecutive_hiz_jobs_stay_independent() {
        let pool = MeshJobPool::new(1);
        let vp = test_view_proj(1.0, 0.5);
        let occluder = |z_offset: f32| {
            vec![make_section(
                [-8.0, -8.0, z_offset],
                &[xy_quad(0.0, 0.0, 16.0, 16.0, 1.0, OPAQUE)],
                None,
                0xff,
            )]
        };
        assert!(pool.try_submit(MeshJob::HiZDepth {
            view_proj: vp,
            sections: occluder(4.0),
        }));
        assert!(pool.try_submit(MeshJob::HiZDepth {
            view_proj: vp,
            sections: occluder(20.0),
        }));
        let rows = view_proj_row_major(&vp);
        // Drain until both results arrive — `next_result` keeps only the
        // last queued result per drain, so two back-to-back calls could
        // swallow the first pyramid when the worker outruns the test.
        let mut pyramids = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while pyramids.len() < 2 {
            pool.drain(|result| {
                let MeshResult::HiZDepth { pyramid, .. } = result else {
                    panic!("expected HiZDepth result");
                };
                pyramids.push(pyramid);
            });
            assert!(
                std::time::Instant::now() < deadline,
                "pool result timed out"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let mut pyramids = pyramids.into_iter();
        let near = pyramids.next().unwrap().expect("first pyramid");
        let far = pyramids.next().unwrap().expect("second pyramid");
        // Wall at z=5 claims 0.9 depth; wall at z=21 claims ~0.976.
        assert!(near.cull_world_aabb(&rows, [-2.0, -2.0, 40.0], [2.0, 2.0, 44.0]));
        assert!(far.cull_world_aabb(&rows, [-2.0, -2.0, 40.0], [2.0, 2.0, 44.0]));
        // A z=8 AABB sits behind the z=5 wall (nearest ndc 0.9375 > 0.9 →
        // culled) but in front of the z=21 wall (0.9375 < 0.976 → kept).
        // Opposite verdicts prove the second job did not replay the first.
        assert!(near.cull_world_aabb(&rows, [-1.0, -1.0, 8.0], [1.0, 1.0, 10.0]));
        assert!(!far.cull_world_aabb(&rows, [-1.0, -1.0, 8.0], [1.0, 1.0, 10.0]));
    }

    /// The `SectionGeometry` arm is untouched but shares `execute` —
    /// smoke-test its round-trip through the same pool.
    #[test]
    fn section_geometry_job_still_round_trips() {
        let pool = MeshJobPool::new(1);
        let packed: Arc<[PackedVertex]> = xy_quad(0.0, 0.0, 16.0, 16.0, 1.0, OPAQUE)
            .into_iter()
            .collect();
        let order: Arc<[u32]> = vec![0u32].into();
        let coord = SectionCoord(1, 2, 3);
        assert!(pool.try_submit(MeshJob::SectionGeometry {
            coord,
            packed: Arc::clone(&packed),
            order,
            buckets: Some(buckets_with_counts([1, 0, 0, 0, 0, 0, 0])),
        }));
        let MeshResult::SectionGeometry {
            coord: done,
            packed: echoed,
            opaque_bucket_ranges,
            ..
        } = next_result(&pool)
        else {
            panic!("expected SectionGeometry result");
        };
        assert_eq!(done, coord);
        assert!(Arc::ptr_eq(&echoed, &packed));
        let ranges = opaque_bucket_ranges.expect("bucketed mesh reports ranges");
        assert_eq!(ranges[0], (0, 6), "one opaque quad → six indices");
        assert_eq!(&ranges[1..], &[(6, 0); DIRECTION_BUCKETS - 1][..]);
    }

    #[test]
    fn translucent_decode_matches_layer_and_fluid_bits() {
        for layer in 0..4u32 {
            for fluid in 0..4u32 {
                let attr = (layer << 13) | (fluid << 15);
                assert_eq!(
                    packed_quad_translucent(attr),
                    layer == 2 || (layer == 3 && fluid == 1),
                    "layer {layer} fluid {fluid}"
                );
            }
        }
    }

    #[test]
    fn bucket_ranges_count_opaque_only() {
        // Bucket 0: 1 opaque + 1 translucent; bucket 2: 2 opaque.
        let quads = [
            xy_quad(0.0, 0.0, 1.0, 1.0, 8.0, OPAQUE),
            xy_quad(0.0, 0.0, 1.0, 1.0, 8.0, TRANSLUCENT),
            xy_quad(0.0, 0.0, 1.0, 1.0, 8.0, OPAQUE),
            xy_quad(0.0, 0.0, 1.0, 1.0, 8.0, WATER),
        ];
        let packed: Vec<PackedVertex> = quads.iter().flatten().copied().collect();
        let ranges = compute_opaque_bucket_ranges(
            &packed,
            buckets_with_counts([2, 0, 2, 0, 0, 0, 0]),
        );
        assert_eq!(
            ranges,
            [(0, 6), (6, 0), (6, 6), (12, 0), (12, 0), (12, 0), (12, 0)]
        );
    }
}
