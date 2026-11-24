// SPDX-License-Identifier: LGPL-3.0-or-later

//! Camera-distance draw ordering with reusable scratch buffers.
//!
//! Two clean-room pieces, both pure `std` and wall-clock free:
//!
//! * [`DrawOrderScratch`] is a generic opaque/transparent draw-list builder.
//!   Opaque draws sort *front-to-back* so depth-tested rasterization writes
//!   the nearest fragments first and maximizes Early-Z rejection of the
//!   overdraw behind them; transparent draws sort *back-to-front* so
//!   src-over blending composites correctly. [`DrawOrderScratch::clear`]
//!   keeps every buffer's capacity, so a steady-state frame pushes and
//!   sorts with zero heap growth — [`DrawOrderScratch::growth_events`]
//!   counts the pushes that had to grow a buffer, which is `0` once the
//!   lists are warmed.
//! * [`MultidrawScratch`] is the [`crate::RenderRegions`] companion: it
//!   recycles the per-command `draws` buffers and every staging list across
//!   frames, so [`crate::RenderRegions::build_multidraw_into`] emits a
//!   camera-ordered command stream without per-frame `Vec` churn.
//!
//! Depth keys are monotone `u32`s derived from `f32` bit patterns (see
//! [`depth_key`]); sorting is stable, so equal-depth draws keep their
//! insertion order and identical inputs produce identical command streams.

use crate::region::{DrawElementsCommand, MultiDrawCommand};

/// Submission order for an ordered draw pass.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DrawOrder {
    /// Nearest first — opaque/cutout passes, maximizing Early-Z rejection.
    #[default]
    FrontToBack = 0,
    /// Farthest first — transparent passes, correct src-over compositing.
    BackToFront = 1,
}

/// Squared camera distance → monotone sort key. Non-finite or negative
/// inputs pin to `u32::MAX` (farthest), so corrupt depth inputs degrade to
/// "drawn last front-to-back / first back-to-front" instead of poisoning the
/// sort. For finite non-negative `f32`s the raw bit pattern is already
/// monotone, so the key is exact — no quantization error.
#[inline]
pub fn depth_key(distance_sq: f32) -> u32 {
    if !distance_sq.is_finite() || distance_sq < 0.0 {
        return u32::MAX;
    }
    distance_sq.to_bits()
}

/// Squared distance from `camera` to the nearest point of the `[min, max]`
/// AABB — `0` when the camera is inside the box. This is the right sort key
/// for section-sized draws: a camera-enclosing section sorts nearest
/// front-to-back, farthest back-to-front.
#[inline]
pub fn aabb_distance_sq(min: [f32; 3], max: [f32; 3], camera: [f32; 3]) -> f32 {
    let mut sum = 0.0f32;
    for axis in 0..3 {
        let d = (min[axis] - camera[axis])
            .max(camera[axis] - max[axis])
            .max(0.0);
        sum += d * d;
    }
    sum
}

/// One draw tagged with its computed [`depth_key`].
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct KeyedDraw<T> {
    /// Monotone sort key; see [`depth_key`].
    pub depth_key: u32,
    /// The draw payload.
    pub draw: T,
}

/// Number of `vec.push` calls that had to grow the vector — the exact upper
/// bound on reallocations for non-ZST elements. `0` means the scratch ran
/// entirely on warmed capacity.
#[inline]
fn push_counted<T>(vec: &mut Vec<T>, item: T, growth_events: &mut u64) {
    if std::mem::size_of::<T>() > 0 && vec.len() == vec.capacity() {
        *growth_events += 1;
    }
    vec.push(item);
}

/// Reusable opaque/transparent draw-list builder.
///
/// `push_opaque`/`push_transparent` record a draw with its squared camera
/// distance, [`finish`](Self::finish) stable-sorts both lists (opaque
/// nearest-first, transparent farthest-first), and [`clear`](Self::clear)
/// retains capacity for the next frame. Once the lists have grown to their
/// working size, a frame's pushes and sorts perform zero heap growth —
/// verified by [`growth_events`](Self::growth_events) staying flat.
#[derive(Clone, Debug)]
pub struct DrawOrderScratch<T> {
    opaque: Vec<KeyedDraw<T>>,
    transparent: Vec<KeyedDraw<T>>,
    growth_events: u64,
}

impl<T> Default for DrawOrderScratch<T> {
    fn default() -> Self {
        Self {
            opaque: Vec::new(),
            transparent: Vec::new(),
            growth_events: 0,
        }
    }
}

impl<T> DrawOrderScratch<T> {
    /// An empty scratch with no buffers allocated yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Drop every recorded draw while keeping both lists' capacity.
    pub fn clear(&mut self) {
        self.opaque.clear();
        self.transparent.clear();
    }

    /// Record an opaque/cutout draw `distance_sq` from the camera. Sorts
    /// nearest-first in [`finish`](Self::finish).
    pub fn push_opaque(&mut self, distance_sq: f32, draw: T) {
        let keyed = KeyedDraw {
            depth_key: depth_key(distance_sq),
            draw,
        };
        push_counted(&mut self.opaque, keyed, &mut self.growth_events);
    }

    /// Record a transparent draw `distance_sq` from the camera. Sorts
    /// farthest-first in [`finish`](Self::finish).
    pub fn push_transparent(&mut self, distance_sq: f32, draw: T) {
        let keyed = KeyedDraw {
            depth_key: depth_key(distance_sq),
            draw,
        };
        push_counted(&mut self.transparent, keyed, &mut self.growth_events);
    }

    /// Sort both recorded lists: opaque front-to-back (ascending depth key),
    /// transparent back-to-front (descending). Stable — equal keys keep
    /// insertion order, so output is fully deterministic.
    pub fn finish(&mut self) {
        self.opaque.sort_by_key(|k| k.depth_key);
        self.transparent.sort_by_key(|k| std::cmp::Reverse(k.depth_key));
    }

    /// Sorted opaque draws, nearest first. Valid after [`finish`](Self::finish).
    pub fn opaque(&self) -> &[KeyedDraw<T>] {
        &self.opaque
    }

    /// Sorted transparent draws, farthest first. Valid after
    /// [`finish`](Self::finish).
    pub fn transparent(&self) -> &[KeyedDraw<T>] {
        &self.transparent
    }

    /// Number of pushes that grew a buffer — the reallocation upper bound.
    /// Flat across frames once both lists hold their working set.
    pub fn growth_events(&self) -> u64 {
        self.growth_events
    }

    /// Retained capacity of the opaque list (for warmup assertions).
    pub fn opaque_capacity(&self) -> usize {
        self.opaque.capacity()
    }

    /// Retained capacity of the transparent list.
    pub fn transparent_capacity(&self) -> usize {
        self.transparent.capacity()
    }
}

/// Reusable buffers for [`crate::RenderRegions::build_multidraw_into`].
///
/// A frame cycles like this:
///
/// 1. [`begin`](Self::begin) — every emitted command's `draws` buffer moves
///    back into `draw_pool` (cleared), staging lists reset. Nothing frees.
/// 2. The region walk fills `buckets` (reused `KeyedDraw` vectors) and
///    checks `draw_pool` buffers out into `staging` commands.
/// 3. [`finish`](Self::finish) sorts `order` by each command's nearest draw
///    and permutes `staging` into `output`, which [`commands`](Self::commands)
///    exposes.
///
/// After warmup the only heap traffic is the `mem::take`/`mem::swap` of
/// already-allocated buffers; [`growth_events`](Self::growth_events) proves
/// it.
#[derive(Default)]
pub struct MultidrawScratch {
    /// Region origins, sorted lexicographically before the walk (stable
    /// tie-break for equal command depths).
    region_keys: Vec<[i32; 3]>,
    /// Per-region bucket assembly: `facing -> keyed draws`. The inner
    /// vectors persist across regions and frames.
    buckets: Vec<(u8, Vec<KeyedDraw<DrawElementsCommand>>)>,
    /// Assembled commands in (region-origin, facing) order before sorting.
    staging: Vec<MultiDrawCommand>,
    /// `(min draw depth_key, staging index)` per staged command.
    order: Vec<(u32, u32)>,
    /// The sorted result exposed by [`commands`](Self::commands).
    output: Vec<MultiDrawCommand>,
    /// Free `draws` buffers recycled from the previous frame's output.
    draw_pool: Vec<Vec<DrawElementsCommand>>,
    growth_events: u64,
}

impl MultidrawScratch {
    /// An empty scratch with no buffers allocated yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Reset for a new frame: `output` commands hand their `draws` buffers
    /// back to the pool, and every staging list clears while keeping its
    /// capacity. The pool also absorbs any `staging` leftovers.
    pub fn begin(&mut self) {
        for cmd in self.output.drain(..) {
            let mut draws = cmd.draws;
            draws.clear();
            push_counted(&mut self.draw_pool, draws, &mut self.growth_events);
        }
        for cmd in self.staging.drain(..) {
            let mut draws = cmd.draws;
            draws.clear();
            push_counted(&mut self.draw_pool, draws, &mut self.growth_events);
        }
        self.region_keys.clear();
        self.order.clear();
    }

    /// The sorted commands of the last [`finish`](Self::finish).
    pub fn commands(&self) -> &[MultiDrawCommand] {
        &self.output
    }

    /// Number of buffer-growing pushes since creation — the reallocation
    /// upper bound. Flat across frames once the pools are warm.
    pub fn growth_events(&self) -> u64 {
        self.growth_events
    }

    /// Record a region origin for the upcoming walk. Origins are sorted
    /// lexicographically by [`sort_region_keys`](Self::sort_region_keys) so
    /// equal command depths tie-break deterministically.
    pub(crate) fn push_region(&mut self, origin: [i32; 3]) {
        push_counted(&mut self.region_keys, origin, &mut self.growth_events);
    }

    /// Sort recorded region origins lexicographically for the walk.
    pub(crate) fn sort_region_keys(&mut self) {
        self.region_keys.sort_unstable();
    }

    /// Number of recorded region origins.
    pub(crate) fn region_count(&self) -> usize {
        self.region_keys.len()
    }

    /// The `index`-th sorted region origin.
    pub(crate) fn region_key(&self, index: usize) -> [i32; 3] {
        self.region_keys[index]
    }

    /// Reset the per-region bucket assembly for the next region.
    pub(crate) fn clear_buckets(&mut self) {
        for (_, draws) in &mut self.buckets {
            draws.clear();
        }
    }

    /// Record a keyed draw under `facing` in the bucket assembly, allocating
    /// a bucket vector on first use (kept for reuse afterwards).
    pub(crate) fn push_bucket_draw(
        &mut self,
        facing: u8,
        draw: KeyedDraw<DrawElementsCommand>,
    ) {
        let index = match self.buckets.iter().position(|&(f, _)| f == facing) {
            Some(index) => index,
            None => {
                push_counted(
                    &mut self.buckets,
                    (facing, Vec::new()),
                    &mut self.growth_events,
                );
                self.buckets.len() - 1
            }
        };
        push_counted(
            &mut self.buckets[index].1,
            draw,
            &mut self.growth_events,
        );
    }

    /// Drain the bucket assembly, sorted by facing, emitting one staged
    /// command per non-empty bucket. Each bucket's draws are sorted by
    /// `order` and the command is keyed by its nearest draw.
    pub(crate) fn drain_buckets(&mut self, region: [i32; 3], order: DrawOrder) {
        // Move the assembly out so `take_draw_buf` can borrow `self` while
        // the per-bucket vectors are drained; it is moved back afterwards
        // with all heap capacity intact.
        let mut buckets = std::mem::take(&mut self.buckets);
        buckets.sort_by_key(|&(facing, _)| facing);
        for (facing, keyed) in buckets.iter_mut() {
            if keyed.is_empty() {
                continue;
            }
            match order {
                DrawOrder::FrontToBack => keyed.sort_by_key(|k| k.depth_key),
                DrawOrder::BackToFront => {
                    keyed.sort_by_key(|k| std::cmp::Reverse(k.depth_key));
                }
            }
            let command_key = keyed
                .iter()
                .map(|k| k.depth_key)
                .min()
                .unwrap_or(u32::MAX);
            let mut draws = self.take_draw_buf();
            draws.extend(keyed.iter().map(|k| k.draw));
            let index = self.staging.len() as u32;
            push_counted(
                &mut self.staging,
                MultiDrawCommand {
                    region,
                    facing: *facing,
                    draws,
                },
                &mut self.growth_events,
            );
            push_counted(
                &mut self.order,
                (command_key, index),
                &mut self.growth_events,
            );
            keyed.clear();
        }
        self.buckets = buckets;
    }

    /// Check a recycled `draws` buffer out of the pool (fresh when empty).
    pub(crate) fn take_draw_buf(&mut self) -> Vec<DrawElementsCommand> {
        self.draw_pool.pop().unwrap_or_default()
    }

    /// Permute staged commands into `output` in `key_order`: front-to-back
    /// by each command's nearest draw depth (Early-Z), or back-to-front for
    /// transparent submissions. Ties keep (region, facing) emission order —
    /// the sort is stable.
    pub(crate) fn finish(&mut self, order: DrawOrder) {
        match order {
            DrawOrder::FrontToBack => self.order.sort_by_key(|&(key, _)| key),
            DrawOrder::BackToFront => {
                self.order.sort_by_key(|&(key, _)| std::cmp::Reverse(key));
            }
        }
        // Recycle any stale output (possible only when `finish` runs
        // without a preceding `begin`) so its buffers are not lost.
        for cmd in self.output.drain(..) {
            let mut draws = cmd.draws;
            draws.clear();
            push_counted(&mut self.draw_pool, draws, &mut self.growth_events);
        }
        for &(_, index) in &self.order {
            push_counted(
                &mut self.output,
                std::mem::take(&mut self.staging[index as usize]),
                &mut self.growth_events,
            );
        }
        // The hollowed `staging` commands keep `Vec::new()` draws buffers;
        // their pooled buffers moved into `output` with the commands.
        self.staging.clear();
    }
}
