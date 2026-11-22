//! Bounded background worker pool for section geometry and Hi-Z builds.
//!
//! Two job kinds run off the render thread:
//!
//! * [`MeshJob::SectionGeometry`] — speculatively converts a queued
//!   [`PackedVertex`] stream into [`LiveSectionGeometry`] and its
//!   direction-bucket index ranges, so the upload-admission drain on the
//!   render thread can install a section without re-running the split.
//!   Results are matched back to pending uploads by `Arc` identity: a stale
//!   result (the pending entry was replaced by a newer mesh) is discarded
//!   and the install falls back to computing inline.
//! * [`MeshJob::HiZDepth`] — conservatively software-projects the opaque
//!   quads the current frame actually drew onto a coarse depth grid and
//!   builds a [`HiZPyramid`] the next frame's culling consults.
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

use megu_client_mesh::ffi::PackedVertex;
use megu_client_mesh::{DirectionBuckets, HiZPyramid, SectionCoord, DIRECTION_BUCKETS};

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
    /// Grouped packed quads (shared with the resident section record).
    pub packed: Arc<[PackedVertex]>,
    /// Direction-bucket quad ranges of `packed`, when the world published
    /// them. `None` splats the whole stream (matching the unbucketed draw
    /// fallback).
    pub buckets: Option<DirectionBuckets>,
    /// Buckets the frame's facing cull kept (bit per bucket); masked-out
    /// direction buckets are not drawn and therefore not occluders.
    pub draw_mask: u8,
}

/// A unit of off-thread work.
pub enum MeshJob {
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
    loop {
        let job = {
            let rx = job_rx.lock().unwrap_or_else(|e| e.into_inner());
            rx.recv()
        };
        let Ok(job) = job else {
            return;
        };
        if result_tx.send(execute(job)).is_err() {
            return;
        }
    }
}

fn execute(job: MeshJob) -> MeshResult {
    match job {
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
            sections,
        } => MeshResult::HiZDepth {
            view_proj,
            pyramid: build_hiz_pyramid(&view_proj, &sections),
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

/// Project every drawn opaque quad of `sections` onto a coarse
/// max-occluder depth grid and wrap it in a [`HiZPyramid`]. Conservative
/// by construction: a texel is only claimed when the projected quad covers
/// its center with margin, the claimed depth is the quad's *far* corner
/// (never nearer than the true surface), and quads that clip the near
/// plane are dropped rather than approximated.
fn build_hiz_pyramid(view_proj: &Mat4, sections: &[HiZSplatSection]) -> Option<HiZPyramid> {
    let (width, height) = (HIZ_PYRAMID_WIDTH as usize, HIZ_PYRAMID_HEIGHT as usize);
    let mut depth = vec![1.0f32; width * height];
    let rows = view_proj_row_major(view_proj);
    for section in sections {
        let quad_total = section.packed.len() / 4;
        match section.buckets {
            Some(buckets) => {
                for bucket in 0..DIRECTION_BUCKETS {
                    if section.draw_mask >> bucket & 1 == 0 {
                        continue;
                    }
                    splat_quad_range(
                        &mut depth,
                        width,
                        height,
                        &rows,
                        section,
                        buckets.offset[bucket] as usize,
                        (buckets.count[bucket] as usize).min(quad_total),
                    );
                }
            }
            None => splat_quad_range(&mut depth, width, height, &rows, section, 0, quad_total),
        }
    }
    HiZPyramid::from_depth_buffer(width, height, &depth).ok()
}

/// Rasterize the `quad_start..+quad_count` quads of `section.packed` into
/// `depth`, skipping translucent quads (they blend rather than occlude).
fn splat_quad_range(
    depth: &mut [f32],
    width: usize,
    height: usize,
    rows: &[[f32; 4]; 4],
    section: &HiZSplatSection,
    quad_start: usize,
    quad_count: usize,
) {
    let quad_end = (quad_start.saturating_add(quad_count)).min(section.packed.len() / 4);
    for quad_index in quad_start.min(quad_end)..quad_end {
        let quad = &section.packed[quad_index * 4..quad_index * 4 + 4];
        if packed_quad_translucent(quad[0].attributes) {
            continue;
        }
        splat_quad(depth, width, height, rows, section.origin, quad);
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
    quad: &[PackedVertex],
) {
    // Texel-space inset for the point-in-quad test: keeps texels whose
    // centers sit on the quad edge (ambiguous under rasterizer fill rules)
    // from claiming occluder coverage they may not actually have.
    const INSIDE_MARGIN: f32 = 0.02;
    let mut px = [0.0f32; 4];
    let mut py = [0.0f32; 4];
    let mut far_z = 0.0f32;
    for (corner, vertex) in quad.iter().enumerate() {
        // The same decode the terrain vertex shader runs.
        let lx = (vertex.position & 31) as f32;
        let lz = ((vertex.position >> 5) & 31) as f32;
        let ly = (vertex.position >> 10) as f32 / 1024.0;
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
