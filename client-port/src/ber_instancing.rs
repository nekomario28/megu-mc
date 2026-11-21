// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Generic block-entity renderer draw-call elimination: the per-frame
//! batcher that turns "every block entity submits its own draw" into "one
//! instanced draw per unique (model, atlas) pair".
//!
//! The input is deliberately shape-agnostic — a chute, belt, cogwheel,
//! pipe, chest, sign, banner, item frame, or monitor all arrive as the
//! same [`BerEntityInput`]: a world AABB, a [`BerMotionState`], two
//! interior flags, an optional dynamic shell descriptor, and a slice of
//! interior items. No block-entity type name appears anywhere in the
//! pipeline, so every vanilla and modded block entity gets the same four
//! cull layers for free:
//!
//! * **Layer 1 — static-idle promotion.** An entity that is not
//!   [`BerEntityInput::is_dynamically_active`] (static motion, no live
//!   interior contents) emits zero dynamic draws: its shell is already
//!   part of the static chunk mesh.
//! * **Layer 2 — interior occlusion + frustum/wall culling.** A fully
//!   opaque-surrounded or out-of-frustum entity drops entirely; an
//!   entity whose interior is sealed (`interior_visible == false`)
//!   keeps its shell but emits zero interior-item draws.
//! * **Layer 3 — distance-tiered LOD.** [`BerLodPolicy`] draws animated
//!   interior items only inside `near_interior_dist * lod_scale` and
//!   drops the whole dynamic entity past `max_dynamic_dist * lod_scale`,
//!   where the static chunk mesh already covers it. `lod_scale` eases
//!   between [`LOD_SCALE_MIN`] and [`LOD_SCALE_MAX`] so a low-spec iGPU
//!   or a frame-P95 governor can shed distance smoothly;
//!   [`BerLodPolicy::low_spec`] ships tighter base distances for
//!   integrated GPUs.
//! * **Layer 4 — GPU hardware instancing.** Survivors pack into 40-byte
//!   [`BerInstanceData`] records grouped by `(model_id, atlas_id)` into
//!   one [`BerInstanceBatch`] per unique pair — `instanced_draw_calls`
//!   equals the number of distinct models, not the instance count.
//!
//! [`BlockEntityInstanceBatcher::build_batches_into`] writes through
//! reusable staging/output buffers; after warmup it performs zero heap
//! growth, which [`BlockEntityInstanceBatcher::growth_events`] proves.
#![forbid(unsafe_code)]

/// Interior-item draw distance (blocks) for the default LOD policy.
pub const DEFAULT_NEAR_INTERIOR_DIST: f32 = 24.0;
/// Whole-entity dynamic draw distance (blocks) for the default policy.
pub const DEFAULT_MAX_DYNAMIC_DIST: f32 = 64.0;
/// Interior-item draw distance (blocks) for [`BerLodPolicy::low_spec`].
pub const LOW_SPEC_NEAR_INTERIOR_DIST: f32 = 12.0;
/// Whole-entity dynamic draw distance (blocks) for
/// [`BerLodPolicy::low_spec`].
pub const LOW_SPEC_MAX_DYNAMIC_DIST: f32 = 32.0;
/// Tightest adaptive LOD scale (25% of base distances).
pub const LOD_SCALE_MIN: f32 = 0.25;
/// Full-detail adaptive LOD scale.
pub const LOD_SCALE_MAX: f32 = 1.0;
/// Bytes per [`BerInstanceData`] GPU record.
pub const BER_INSTANCE_GPU_BYTES: u32 = std::mem::size_of::<BerInstanceData>() as u32;

/// Stable id of one shared block-entity mesh template in the merged
/// geometry buffer.
pub type BerModelKey = u32;
/// Stable id of the texture atlas a model samples.
pub type BerAtlasKey = u32;

/// Whether the block entity's own geometry animates this frame.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BerMotionState {
    /// Nothing moves: the shell is drawn once inside the static chunk
    /// mesh and costs no dynamic draw.
    #[default]
    Static = 0,
    /// Some part animates (lid, belt, rotor, wobble, drum), so a dynamic
    /// shell instance is emitted while the entity survives culling.
    Moving = 1,
}

/// Dense GPU instance record: 40 bytes, `align(4)`, laid out for a
/// `std430` SSBO / interleaved instancing stream.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BerInstanceData {
    /// xyz = instance origin in render-world blocks, w = uniform scale.
    pub pos_scale: [f32; 4],
    /// xyz = rotation triplet in radians, w = animation/atlas frame
    /// index.
    pub rot_uv_frame: [f32; 4],
    /// Packed light: block-light nibble | sky-light nibble << 4.
    pub packed_light: u32,
    /// RGBA tint, one byte per channel.
    pub tint_rgba: u32,
}

/// The dynamic shell draw for one [`BerMotionState::Moving`] entity:
/// which `(model, atlas)` to instance and the per-instance transform
/// payload.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BerShellModel {
    /// Shared mesh template this shell instances.
    pub model_id: BerModelKey,
    /// Atlas the model samples.
    pub atlas_id: BerAtlasKey,
    /// Uniform shell scale.
    pub scale: f32,
    /// Rotation triplet + animation frame, packed like
    /// [`BerInstanceData::rot_uv_frame`].
    pub rot_uv_frame: [f32; 4],
    /// Packed light at the entity.
    pub packed_light: u32,
    /// RGBA tint.
    pub tint_rgba: u32,
}

/// One animated item inside a block entity's interior (the thing a
/// windowed chute shows sliding through). Items are emitted relative to
/// the entity origin and instance under their own `(model, atlas)` key.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BerInteriorItem {
    /// Position offset from the entity's `pos` corner, in blocks.
    pub offset: [f32; 3],
    /// Uniform item scale.
    pub scale: f32,
    /// Rotation triplet + animation frame.
    pub rot_uv_frame: [f32; 4],
    /// Shared mesh template this item instances.
    pub model_id: BerModelKey,
    /// Atlas the model samples.
    pub atlas_id: BerAtlasKey,
    /// Packed light at the item.
    pub packed_light: u32,
    /// RGBA tint.
    pub tint_rgba: u32,
}

/// One block entity submitted for culling and instancing this frame.
/// `pos`/`extents` describe the world AABB used for the frustum,
/// occlusion, and distance tests.
#[derive(Clone, Debug)]
pub struct BerEntityInput<'a> {
    /// Unique, stable id — the deterministic tiebreak inside one
    /// `(model, atlas)` run.
    pub entity_id: u64,
    /// World-space min corner of the entity's AABB.
    pub pos: [f32; 3],
    /// AABB extents from `pos` (a full block is `[1.0; 3]`). Must be
    /// finite and non-negative.
    pub extents: [f32; 3],
    /// Whether the entity's own geometry animates.
    pub motion_state: BerMotionState,
    /// Whether interior contents animate (items in transit, fluid
    /// level, progress gauge). Only meaningful alongside a non-empty
    /// `interior_items` slice.
    pub has_dynamic_interior: bool,
    /// Whether the interior is open/windowed so its contents can be
    /// seen. `false` = sealed/opaque interior: all interior-item draws
    /// are skipped.
    pub interior_visible: bool,
    /// All six neighbor blocks fully occlude toward this cell — nothing
    /// of the entity can be seen, so the whole entity drops.
    pub surrounded_opaque: bool,
    /// Dynamic shell descriptor, emitted when `motion_state` is
    /// [`BerMotionState::Moving`].
    pub shell: BerShellModel,
    /// Interior items candidate for drawing when `interior_visible` and
    /// inside the near-interior distance tier.
    pub interior_items: &'a [BerInteriorItem],
}

impl BerEntityInput<'_> {
    /// Layer 1 test: `true` when this entity can emit any dynamic draw
    /// — either its shell animates or it carries live interior
    /// contents. A `false` entity renders entirely through the static
    /// chunk mesh at zero dynamic cost.
    pub fn is_dynamically_active(&self) -> bool {
        self.motion_state != BerMotionState::Static
            || (self.has_dynamic_interior && !self.interior_items.is_empty())
    }

    /// Whether `pos` is finite and `extents` is finite and non-negative.
    /// Inputs failing this fail closed: they are dropped before any
    /// draw is emitted.
    fn is_valid(&self) -> bool {
        self.pos.iter().all(|v| v.is_finite())
            && self.extents.iter().all(|v| v.is_finite() && *v >= 0.0)
    }

    /// AABB center used for the distance-tier tests.
    fn center(&self) -> [f32; 3] {
        [
            self.pos[0] + self.extents[0] * 0.5,
            self.pos[1] + self.extents[1] * 0.5,
            self.pos[2] + self.extents[2] * 0.5,
        ]
    }

    /// Squared distance from the AABB center to `camera`. Non-finite
    /// input yields `f32::INFINITY`, i.e. maximum distance — a bad
    /// camera fails closed by shedding every draw.
    fn dist_sq_to(&self, camera: [f32; 3]) -> f32 {
        if !camera.iter().all(|v| v.is_finite()) {
            return f32::INFINITY;
        }
        let c = self.center();
        let d = [c[0] - camera[0], c[1] - camera[1], c[2] - camera[2]];
        d[0] * d[0] + d[1] * d[1] + d[2] * d[2]
    }
}

/// Per-frame camera state for the cull layers: eye position and the
/// frustum planes in `dot(n, p) + d >= 0` inside-half-space form.
#[derive(Clone, Copy, Debug)]
pub struct BerFrameView<'a> {
    /// Camera eye position in render-world blocks.
    pub camera_pos: [f32; 3],
    /// Frustum planes `[nx, ny, nz, d]`; an AABB fully outside any one
    /// plane is culled. An empty slice disables frustum culling.
    pub frustum_planes: &'a [[f32; 4]],
}

/// Distance-tiered LOD policy: base interior/dynamic distances plus the
/// adaptive `lod_scale` an external frame-time governor eases between
/// [`LOD_SCALE_MIN`] and [`LOD_SCALE_MAX`]. All setters scrub
/// non-finite and negative inputs fail-closed — a garbage distance
/// becomes `0.0` (draws shed), and a `NaN` scale drops to
/// [`LOD_SCALE_MIN`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BerLodPolicy {
    near_interior_dist: f32,
    max_dynamic_dist: f32,
    lod_scale: f32,
}

impl Default for BerLodPolicy {
    fn default() -> Self {
        Self::new()
    }
}

impl BerLodPolicy {
    /// Full-quality policy: [`DEFAULT_NEAR_INTERIOR_DIST`] /
    /// [`DEFAULT_MAX_DYNAMIC_DIST`] at scale 1.0.
    pub const fn new() -> Self {
        Self {
            near_interior_dist: DEFAULT_NEAR_INTERIOR_DIST,
            max_dynamic_dist: DEFAULT_MAX_DYNAMIC_DIST,
            lod_scale: LOD_SCALE_MAX,
        }
    }

    /// Integrated-GPU preset (Intel HD/UHD class): tighter interior and
    /// dynamic distances so a factory wall costs a fraction of the
    /// default tier budget.
    pub const fn low_spec() -> Self {
        Self {
            near_interior_dist: LOW_SPEC_NEAR_INTERIOR_DIST,
            max_dynamic_dist: LOW_SPEC_MAX_DYNAMIC_DIST,
            lod_scale: LOD_SCALE_MAX,
        }
    }

    /// Policy with explicit base distances; non-finite or negative
    /// values scrub to `0.0` (fail closed).
    pub fn with_distances(near_interior_dist: f32, max_dynamic_dist: f32) -> Self {
        Self {
            near_interior_dist: scrub_distance(near_interior_dist),
            max_dynamic_dist: scrub_distance(max_dynamic_dist),
            lod_scale: LOD_SCALE_MAX,
        }
    }

    /// Base interior-item draw distance (before `lod_scale`).
    pub fn near_interior_dist(&self) -> f32 {
        self.near_interior_dist
    }

    /// Base whole-entity dynamic draw distance (before `lod_scale`).
    pub fn max_dynamic_dist(&self) -> f32 {
        self.max_dynamic_dist
    }

    /// Current adaptive scale in `0.25..=1.0`.
    pub fn lod_scale(&self) -> f32 {
        self.lod_scale
    }

    /// Set the adaptive scale. Values clamp into
    /// [`LOD_SCALE_MIN`]..=[`LOD_SCALE_MAX`]; `NaN` fails closed to the
    /// tightest scale, `+inf` clamps to [`LOD_SCALE_MAX`].
    pub fn set_lod_scale(&mut self, scale: f32) {
        self.lod_scale = if scale.is_nan() {
            LOD_SCALE_MIN
        } else {
            scale.clamp(LOD_SCALE_MIN, LOD_SCALE_MAX)
        };
    }

    /// Effective interior-item draw distance this frame.
    pub fn effective_near_interior_dist(&self) -> f32 {
        self.near_interior_dist * self.lod_scale
    }

    /// Effective whole-entity dynamic draw distance this frame.
    pub fn effective_max_dynamic_dist(&self) -> f32 {
        self.max_dynamic_dist * self.lod_scale
    }
}

/// One instanced draw call: every instance of `(model_id, atlas_id)`
/// lives contiguously in [`BerFrameBatches::instances`] at
/// `first_instance .. first_instance + instance_count`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BerInstanceBatch {
    /// Shared mesh template this batch draws.
    pub model_id: BerModelKey,
    /// Atlas bound for this batch.
    pub atlas_id: BerAtlasKey,
    /// Offset of this batch's run in the packed instance stream.
    pub first_instance: u32,
    /// Instances in the run.
    pub instance_count: u32,
}

/// Reusable per-frame output: the packed instance stream and one batch
/// descriptor per unique `(model, atlas)` pair — one instanced draw
/// call each. Pass the same `BerFrameBatches` to every
/// [`BlockEntityInstanceBatcher::build_batches_into`] to keep the
/// steady state allocation-free.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BerFrameBatches {
    /// Instance records grouped by `(model_id, atlas_id)`, ordered
    /// deterministically by `(model, atlas, entity_id, emission seq)`.
    pub instances: Vec<BerInstanceData>,
    /// One instanced draw per unique `(model_id, atlas_id)`, in
    /// ascending key order.
    pub batches: Vec<BerInstanceBatch>,
}

/// Exact per-frame cull accounting. `total_inputs` entities split into
/// the cull buckets plus the survivors; `culled_interior_opaque` and
/// the interior part of `culled_distance_lod` describe partial culls of
/// entities that still emit a shell, so those two overlap the visible
/// set rather than partitioning it.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BerCullStats {
    /// Entities submitted this frame.
    pub total_inputs: u32,
    /// Entities dropped by layer 1 (no motion, no live interior): zero
    /// dynamic draws, covered by the static chunk mesh.
    pub culled_static_idle: u32,
    /// Entities dropped by layer 2: malformed, fully opaque-surrounded,
    /// or outside the frustum.
    pub culled_frustum_or_occluded: u32,
    /// Entities whose dynamic interior was suppressed by layer 2
    /// because `interior_visible == false` (shell may still draw).
    pub culled_interior_opaque: u32,
    /// Layer 3 suppressions: entities fully dropped past
    /// `max_dynamic_dist * lod_scale` plus entities whose interior items
    /// were dropped past `near_interior_dist * lod_scale`.
    pub culled_distance_lod: u32,
    /// [`BerInstanceData`] records emitted into the packed stream.
    pub visible_instances: u32,
    /// Instanced draw calls emitted — one per unique
    /// `(model_id, atlas_id)` pair.
    pub instanced_draw_calls: u32,
}

/// Fail-closed distance scrub: finite non-negative passes through,
/// anything else becomes `0.0` (draw nothing beyond the origin).
fn scrub_distance(v: f32) -> f32 {
    if v.is_finite() && v >= 0.0 {
        v
    } else {
        0.0
    }
}

/// Fail-closed float scrub for per-instance payloads: non-finite
/// becomes `0.0`.
fn scrub_finite(v: f32) -> f32 {
    if v.is_finite() {
        v
    } else {
        0.0
    }
}

/// Fail-closed scale scrub: non-finite or negative becomes `0.0`.
fn scrub_scale(v: f32) -> f32 {
    if v.is_finite() && v >= 0.0 {
        v
    } else {
        0.0
    }
}

/// Positive-vertex frustum test for an arbitrary `[min, max]` AABB
/// against `dot(n, p) + d >= 0` inside-half-space planes. A `NaN`
/// corner makes every test fail — outside — so malformed geometry
/// culls rather than draws.
#[inline]
fn aabb_in_frustum(min: [f32; 3], max: [f32; 3], planes: &[[f32; 4]]) -> bool {
    planes.iter().all(|&[nx, ny, nz, d]| {
        let p = [
            if nx >= 0.0 { max[0] } else { min[0] },
            if ny >= 0.0 { max[1] } else { min[1] },
            if nz >= 0.0 { max[2] } else { min[2] },
        ];
        nx * p[0] + ny * p[1] + nz * p[2] + d >= 0.0
    })
}

/// Sort key for one emitted instance: batch grouping by
/// `(model, atlas)`, then stable `entity_id`/`seq` order so two builds
/// over identical input are byte-identical.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct BerEmitKey {
    model_id: BerModelKey,
    atlas_id: BerAtlasKey,
    entity_id: u64,
    seq: u32,
}

#[derive(Clone, Copy, Debug)]
struct BerEmit {
    key: BerEmitKey,
    data: BerInstanceData,
}

/// Push to `buf`, counting a growth event when the buffer has to
/// reallocate. Steady state keeps capacity, so the counter stays flat.
fn push_counted<T>(buf: &mut Vec<T>, item: T, growth_events: &mut u64) {
    if buf.len() == buf.capacity() {
        *growth_events += 1;
    }
    buf.push(item);
}

/// Per-frame generic block-entity batcher. Holds the [`BerLodPolicy`]
/// plus reusable scratch so [`build_batches_into`](Self::build_batches_into)
/// performs zero heap growth after warmup — verified through
/// [`growth_events`](Self::growth_events).
#[derive(Default)]
pub struct BlockEntityInstanceBatcher {
    policy: BerLodPolicy,
    /// Total buffer reallocs observed across all builds; must stay flat
    /// once working capacity has been reached.
    growth_events: u64,
    /// Reusable emission scratch: surviving instances + sort keys.
    emit_scratch: Vec<BerEmit>,
}

impl BlockEntityInstanceBatcher {
    pub fn new(policy: BerLodPolicy) -> Self {
        Self {
            policy,
            ..Self::default()
        }
    }

    /// The active LOD policy.
    pub fn policy(&self) -> BerLodPolicy {
        self.policy
    }

    /// Replace the LOD policy.
    pub fn set_policy(&mut self, policy: BerLodPolicy) {
        self.policy = policy;
    }

    /// Ease the adaptive scale (frame-P95 governor hook); see
    /// [`BerLodPolicy::set_lod_scale`].
    pub fn set_lod_scale(&mut self, scale: f32) {
        self.policy.set_lod_scale(scale);
    }

    /// Buffer reallocations observed since construction — flat after
    /// warmup, so a rising value is the zero-realloc regression signal.
    pub fn growth_events(&self) -> u64 {
        self.growth_events
    }

    /// Cull `inputs` through the four layers and pack the survivors
    /// into `out`'s instanced batches. Deterministic: instances sort by
    /// `(model_id, atlas_id, entity_id, seq)`, so identical input yields
    /// byte-identical output. `out` is cleared, not reallocated, so
    /// steady-state frames trigger zero growth events.
    pub fn build_batches_into(
        &mut self,
        inputs: &[BerEntityInput<'_>],
        view: &BerFrameView<'_>,
        out: &mut BerFrameBatches,
    ) -> BerCullStats {
        let policy = self.policy;
        let max_dyn_sq = {
            let d = policy.effective_max_dynamic_dist();
            d * d
        };
        let near_int_sq = {
            let d = policy.effective_near_interior_dist();
            d * d
        };

        let mut stats = BerCullStats {
            total_inputs: inputs.len() as u32,
            ..BerCullStats::default()
        };
        let growth = &mut self.growth_events;
        let scratch = &mut self.emit_scratch;
        scratch.clear();

        for input in inputs {
            // Layer 1 — static-idle promotion.
            if !input.is_dynamically_active() {
                stats.culled_static_idle += 1;
                continue;
            }
            // Layer 2 — validity, wall occlusion, frustum.
            if !input.is_valid() || input.surrounded_opaque {
                stats.culled_frustum_or_occluded += 1;
                continue;
            }
            let aabb_max = [
                input.pos[0] + input.extents[0],
                input.pos[1] + input.extents[1],
                input.pos[2] + input.extents[2],
            ];
            if !aabb_in_frustum(input.pos, aabb_max, view.frustum_planes) {
                stats.culled_frustum_or_occluded += 1;
                continue;
            }
            // Layer 3 — distance tiers.
            let dist_sq = input.dist_sq_to(view.camera_pos);
            if dist_sq > max_dyn_sq {
                stats.culled_distance_lod += 1;
                continue;
            }

            let mut seq = 0u32;
            if input.motion_state != BerMotionState::Static {
                let shell = &input.shell;
                push_counted(
                    scratch,
                    BerEmit {
                        key: BerEmitKey {
                            model_id: shell.model_id,
                            atlas_id: shell.atlas_id,
                            entity_id: input.entity_id,
                            seq,
                        },
                        data: BerInstanceData {
                            pos_scale: [
                                input.pos[0],
                                input.pos[1],
                                input.pos[2],
                                scrub_scale(shell.scale),
                            ],
                            rot_uv_frame: shell.rot_uv_frame.map(scrub_finite),
                            packed_light: shell.packed_light,
                            tint_rgba: shell.tint_rgba,
                        },
                    },
                    growth,
                );
                seq += 1;
            }

            if input.has_dynamic_interior && !input.interior_items.is_empty() {
                if !input.interior_visible {
                    stats.culled_interior_opaque += 1;
                } else if dist_sq > near_int_sq {
                    stats.culled_distance_lod += 1;
                } else {
                    for item in input.interior_items {
                        if !item.offset.iter().all(|v| v.is_finite()) {
                            continue;
                        }
                        push_counted(
                            scratch,
                            BerEmit {
                                key: BerEmitKey {
                                    model_id: item.model_id,
                                    atlas_id: item.atlas_id,
                                    entity_id: input.entity_id,
                                    seq,
                                },
                                data: BerInstanceData {
                                    pos_scale: [
                                        input.pos[0] + item.offset[0],
                                        input.pos[1] + item.offset[1],
                                        input.pos[2] + item.offset[2],
                                        scrub_scale(item.scale),
                                    ],
                                    rot_uv_frame: item.rot_uv_frame.map(scrub_finite),
                                    packed_light: item.packed_light,
                                    tint_rgba: item.tint_rgba,
                                },
                            },
                            growth,
                        );
                        seq += 1;
                    }
                }
            }
        }

        scratch.sort_unstable_by_key(|emit| emit.key);

        out.instances.clear();
        out.batches.clear();
        let mut run_start = 0usize;
        while run_start < scratch.len() {
            let key = scratch[run_start].key;
            let mut run_end = run_start + 1;
            while run_end < scratch.len()
                && scratch[run_end].key.model_id == key.model_id
                && scratch[run_end].key.atlas_id == key.atlas_id
            {
                run_end += 1;
            }
            let first_instance = out.instances.len() as u32;
            for emit in &scratch[run_start..run_end] {
                push_counted(&mut out.instances, emit.data, growth);
            }
            push_counted(
                &mut out.batches,
                BerInstanceBatch {
                    model_id: key.model_id,
                    atlas_id: key.atlas_id,
                    first_instance,
                    instance_count: (run_end - run_start) as u32,
                },
                growth,
            );
            run_start = run_end;
        }

        stats.visible_instances = out.instances.len() as u32;
        stats.instanced_draw_calls = out.batches.len() as u32;
        stats
    }
}

#[cfg(test)]
mod layout_tests {
    use super::*;
    use std::mem::{offset_of, size_of};

    #[test]
    fn instance_record_is_40_bytes_gpu_aligned() {
        assert_eq!(size_of::<BerInstanceData>(), 40);
        assert_eq!(BER_INSTANCE_GPU_BYTES, 40);
        assert_eq!(offset_of!(BerInstanceData, pos_scale), 0);
        assert_eq!(offset_of!(BerInstanceData, rot_uv_frame), 16);
        assert_eq!(offset_of!(BerInstanceData, packed_light), 32);
        assert_eq!(offset_of!(BerInstanceData, tint_rgba), 36);
    }
}
