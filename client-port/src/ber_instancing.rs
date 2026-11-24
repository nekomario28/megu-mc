// SPDX-License-Identifier: LGPL-3.0-or-later

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

// ---------------------------------------------------------------------------
// Vanilla block-entity geometry
// ---------------------------------------------------------------------------

use crate::vanilla_plus_3d::{ExtrudedMesh, ExtrudedQuad, MAX_UPGRADE_QUADS};
use std::collections::VecDeque;

/// Vanilla block-entity renderer families supported by [`build_ber_mesh`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VanillaBerKind {
    Chest,
    Sign,
    Banner,
    Beacon,
    Bed,
    Skull,
    ItemFrame,
    Campfire,
    BrewingStand,
    Lectern,
    DecoratedPot,
    ShulkerBox,
}

/// Direction in block coordinates, where Y points upward and south is +Z.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VanillaBerFacing {
    #[default]
    North,
    South,
    East,
    West,
    Up,
    Down,
}

/// Per-family geometry state. Yaw and sway are radians unless named `*_deg`.
/// All generated vertices remain in one block's `0..=1` local volume.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum VanillaBerState {
    Chest {
        lid_open: f32,
        double: bool,
        facing: VanillaBerFacing,
    },
    Sign {
        wall: bool,
        hanging: bool,
        glowing: bool,
        facing_yaw: f32,
    },
    Banner {
        wall: bool,
        sway_rad: f32,
        facing_yaw: f32,
    },
    Beacon {
        beam_height: f32,
        active: bool,
        rgb: [f32; 3],
    },
    Bed {
        is_head: bool,
        facing: VanillaBerFacing,
    },
    Skull {
        wall: bool,
        yaw_deg: f32,
    },
    ItemFrame {
        facing: VanillaBerFacing,
        glow: bool,
        rotation: u8,
    },
    Campfire {
        lit: bool,
        item_count: u8,
    },
    BrewingStand {
        bottle_mask: u8,
    },
    Lectern {
        has_book: bool,
        facing: VanillaBerFacing,
    },
    DecoratedPot {
        facing: VanillaBerFacing,
        wobble: f32,
    },
    ShulkerBox {
        open_progress: f32,
        facing: VanillaBerFacing,
    },
}

impl VanillaBerState {
    pub fn kind(self) -> VanillaBerKind {
        match self {
            Self::Chest { .. } => VanillaBerKind::Chest,
            Self::Sign { .. } => VanillaBerKind::Sign,
            Self::Banner { .. } => VanillaBerKind::Banner,
            Self::Beacon { .. } => VanillaBerKind::Beacon,
            Self::Bed { .. } => VanillaBerKind::Bed,
            Self::Skull { .. } => VanillaBerKind::Skull,
            Self::ItemFrame { .. } => VanillaBerKind::ItemFrame,
            Self::Campfire { .. } => VanillaBerKind::Campfire,
            Self::BrewingStand { .. } => VanillaBerKind::BrewingStand,
            Self::Lectern { .. } => VanillaBerKind::Lectern,
            Self::DecoratedPot { .. } => VanillaBerKind::DecoratedPot,
            Self::ShulkerBox { .. } => VanillaBerKind::ShulkerBox,
        }
    }
}

fn ber_unit(value: f32) -> f32 {
    if value.is_nan() {
        0.0
    } else {
        value.clamp(0.0, 1.0)
    }
}

fn ber_angle(angle: f32, limit: f32) -> f32 {
    if angle.is_finite() {
        angle.clamp(-limit, limit)
    } else {
        0.0
    }
}

fn ber_yaw(angle: f32) -> f32 {
    if angle.is_finite() {
        angle.rem_euclid(std::f32::consts::TAU)
    } else {
        0.0
    }
}

/// Emit one CCW face, dropping invalid and degenerate geometry.
fn ber_face(mesh: &mut ExtrudedMesh, points: [[f32; 3]; 4], tint_index: i32) {
    if mesh.quads.len() >= MAX_UPGRADE_QUADS
        || !points
            .iter()
            .flatten()
            .all(|v| v.is_finite() && (0.0..=1.0).contains(v))
    {
        return;
    }
    let a = [
        points[1][0] - points[0][0],
        points[1][1] - points[0][1],
        points[1][2] - points[0][2],
    ];
    let b = [
        points[2][0] - points[0][0],
        points[2][1] - points[0][1],
        points[2][2] - points[0][2],
    ];
    let cross = [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ];
    let len = (cross[0] * cross[0] + cross[1] * cross[1] + cross[2] * cross[2]).sqrt();
    if len <= 1e-8 {
        return;
    }
    mesh.quads.push(ExtrudedQuad {
        positions: points,
        normal: cross.map(|n| n / len),
        uv: [[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]],
        tint_index,
    });
}

/// Six outward-facing axis-aligned faces. Coordinates are local block units.
fn ber_box(mesh: &mut ExtrudedMesh, from: [f32; 3], to: [f32; 3], tint: i32) {
    let [x, y, z] = from;
    let [xx, yy, zz] = to;
    if x >= xx || y >= yy || z >= zz {
        return;
    }
    ber_face(
        mesh,
        [[x, y, zz], [xx, y, zz], [xx, yy, zz], [x, yy, zz]],
        tint,
    );
    ber_face(mesh, [[xx, y, z], [x, y, z], [x, yy, z], [xx, yy, z]], tint);
    ber_face(
        mesh,
        [[xx, y, zz], [xx, y, z], [xx, yy, z], [xx, yy, zz]],
        tint,
    );
    ber_face(mesh, [[x, y, z], [x, y, zz], [x, yy, zz], [x, yy, z]], tint);
    ber_face(
        mesh,
        [[x, yy, zz], [xx, yy, zz], [xx, yy, z], [x, yy, z]],
        tint,
    );
    ber_face(mesh, [[x, y, z], [xx, y, z], [xx, y, zz], [x, y, zz]], tint);
}

/// Right-handed rotations preserve both the face winding and the normals.
fn ber_rotate_y(mesh: &mut ExtrudedMesh, angle: f32) {
    let (s, c) = angle.sin_cos();
    mesh.affine(
        [[c, 0.0, s], [0.0, 1.0, 0.0], [-s, 0.0, c]],
        [0.5, 0.0, 0.5],
        [0.5, 0.0, 0.5],
    );
}

fn ber_rotate_x(mesh: &mut ExtrudedMesh, angle: f32, pivot: [f32; 3]) {
    let (s, c) = angle.sin_cos();
    mesh.affine([[1.0, 0.0, 0.0], [0.0, c, -s], [0.0, s, c]], pivot, pivot);
}

/// Turn south-facing geometry into the requested facing. Up/down are useful
/// for item frames and shulker boxes attached to ceilings or floors.
fn ber_orient(mesh: &mut ExtrudedMesh, facing: VanillaBerFacing) {
    match facing {
        VanillaBerFacing::South => {}
        VanillaBerFacing::North => ber_rotate_y(mesh, std::f32::consts::PI),
        VanillaBerFacing::East => ber_rotate_y(mesh, std::f32::consts::FRAC_PI_2),
        VanillaBerFacing::West => ber_rotate_y(mesh, -std::f32::consts::FRAC_PI_2),
        VanillaBerFacing::Up => ber_rotate_x(mesh, -std::f32::consts::FRAC_PI_2, [0.5; 3]),
        VanillaBerFacing::Down => ber_rotate_x(mesh, std::f32::consts::FRAC_PI_2, [0.5; 3]),
    }
}

/// Construct bounded, outward-CCW Vanilla 1.21.1 BER family geometry.
/// Material/texture selection is carried by `tint_index`; the beacon beam
/// stores its 24-bit RGB in that field. A beam taller than one block is
/// represented by its first local segment for instance-side repetition.
/// A mismatched `(kind, state)` pair returns an empty mesh.
pub fn build_ber_mesh(kind: VanillaBerKind, state: VanillaBerState) -> ExtrudedMesh {
    let mut mesh = ExtrudedMesh::new();
    if state.kind() != kind {
        return mesh;
    }

    match state {
        VanillaBerState::Chest {
            lid_open,
            double,
            facing,
        } => {
            let inset = if double { 0.025 } else { 0.07 };
            ber_box(
                &mut mesh,
                [inset, 0.05, 0.09],
                [1.0 - inset, 0.59, 0.91],
                -1,
            );
            let mut lid = ExtrudedMesh::new();
            ber_box(&mut lid, [inset, 0.59, 0.17], [1.0 - inset, 0.70, 0.81], -1);
            // A block-local articulation keeps the opening lid inside the cell.
            ber_rotate_x(&mut lid, -0.46 * ber_unit(lid_open), [0.5, 0.59, 0.17]);
            mesh.append(&lid);
            ber_box(&mut mesh, [0.46, 0.40, 0.91], [0.54, 0.57, 0.96], 1);
            if double {
                ber_box(&mut mesh, [0.492, 0.06, 0.911], [0.508, 0.58, 0.921], 1);
            }
            ber_orient(&mut mesh, facing);
        }
        VanillaBerState::Sign {
            wall,
            hanging,
            glowing,
            facing_yaw,
        } => {
            ber_box(
                &mut mesh,
                [0.24, 0.43, 0.46],
                [0.76, 0.71, 0.54],
                if glowing { 1 } else { -1 },
            );
            if hanging {
                for x in [0.33, 0.66] {
                    ber_box(
                        &mut mesh,
                        [x - 0.015, 0.71, 0.48],
                        [x + 0.015, 0.92, 0.52],
                        2,
                    );
                }
            } else if wall {
                ber_box(&mut mesh, [0.45, 0.50, 0.36], [0.55, 0.57, 0.46], 2);
            } else {
                ber_box(&mut mesh, [0.475, 0.05, 0.475], [0.525, 0.43, 0.525], 2);
            }
            ber_rotate_y(&mut mesh, ber_yaw(facing_yaw));
        }
        VanillaBerState::Banner {
            wall,
            sway_rad,
            facing_yaw,
        } => {
            if wall {
                ber_box(&mut mesh, [0.27, 0.85, 0.38], [0.73, 0.89, 0.53], 2);
            } else {
                ber_box(&mut mesh, [0.48, 0.11, 0.48], [0.52, 0.96, 0.52], 2);
                ber_box(&mut mesh, [0.28, 0.83, 0.47], [0.72, 0.87, 0.53], 2);
            }
            let mut cloth = ExtrudedMesh::new();
            ber_box(&mut cloth, [0.29, 0.21, 0.475], [0.71, 0.83, 0.525], -1);
            ber_rotate_x(&mut cloth, ber_angle(sway_rad, 0.12), [0.5, 0.83, 0.5]);
            mesh.append(&cloth);
            ber_rotate_y(&mut mesh, ber_yaw(facing_yaw));
        }
        VanillaBerState::Beacon {
            beam_height,
            active,
            rgb,
        } => {
            ber_box(&mut mesh, [0.08, 0.06, 0.08], [0.92, 0.19, 0.92], 2);
            ber_box(&mut mesh, [0.16, 0.19, 0.16], [0.84, 0.68, 0.84], -1);
            ber_box(&mut mesh, [0.28, 0.68, 0.28], [0.72, 0.72, 0.72], 2);
            let height = if beam_height.is_nan() {
                0.0
            } else {
                beam_height.clamp(0.0, 384.0)
            };
            if active && height > 0.0 {
                let tint = rgb.map(|c| (ber_unit(c) * 255.0).round() as i32);
                let packed = (tint[0] << 16) | (tint[1] << 8) | tint[2];
                ber_box(
                    &mut mesh,
                    [0.44, 0.72, 0.44],
                    [0.56, (0.72 + height).min(1.0), 0.56],
                    packed,
                );
            }
        }
        VanillaBerState::Bed { is_head, facing } => {
            ber_box(&mut mesh, [0.12, 0.13, 0.08], [0.88, 0.43, 0.92], -1);
            ber_box(&mut mesh, [0.13, 0.43, 0.09], [0.87, 0.53, 0.91], 1);
            if is_head {
                ber_box(&mut mesh, [0.22, 0.53, 0.14], [0.78, 0.60, 0.42], 2);
                ber_box(&mut mesh, [0.13, 0.11, 0.08], [0.87, 0.29, 0.14], 2);
            } else {
                ber_box(&mut mesh, [0.13, 0.11, 0.85], [0.87, 0.29, 0.91], 2);
            }
            ber_orient(&mut mesh, facing);
        }
        VanillaBerState::Skull { wall, yaw_deg } => {
            ber_box(&mut mesh, [0.27, 0.37, 0.27], [0.73, 0.78, 0.73], -1);
            // Recessed face markings and jaw make the direction visible.
            for x in [0.36, 0.59] {
                ber_box(&mut mesh, [x, 0.55, 0.73], [x + 0.05, 0.61, 0.745], 1);
            }
            ber_box(&mut mesh, [0.42, 0.39, 0.73], [0.58, 0.48, 0.745], 1);
            if wall {
                ber_box(&mut mesh, [0.44, 0.48, 0.14], [0.56, 0.61, 0.27], 2);
            } else {
                ber_box(&mut mesh, [0.41, 0.28, 0.41], [0.59, 0.37, 0.59], 2);
            }
            let radians = ber_yaw(yaw_deg.to_radians());
            ber_rotate_y(&mut mesh, radians);
        }
        VanillaBerState::ItemFrame {
            facing,
            glow,
            rotation,
        } => {
            ber_box(
                &mut mesh,
                [0.22, 0.22, 0.88],
                [0.78, 0.78, 0.91],
                if glow { 1 } else { -1 },
            );
            ber_box(&mut mesh, [0.18, 0.18, 0.87], [0.82, 0.25, 0.94], 2);
            ber_box(&mut mesh, [0.18, 0.75, 0.87], [0.82, 0.82, 0.94], 2);
            ber_box(&mut mesh, [0.18, 0.25, 0.87], [0.25, 0.75, 0.94], 2);
            ber_box(&mut mesh, [0.75, 0.25, 0.87], [0.82, 0.75, 0.94], 2);
            let mut needle = ExtrudedMesh::new();
            ber_box(&mut needle, [0.48, 0.49, 0.94], [0.52, 0.73, 0.955], 1);
            let angle = f32::from(rotation % 8) * std::f32::consts::FRAC_PI_4;
            let (s, c) = angle.sin_cos();
            needle.affine(
                [[c, -s, 0.0], [s, c, 0.0], [0.0, 0.0, 1.0]],
                [0.5, 0.5, 0.5],
                [0.5, 0.5, 0.5],
            );
            mesh.append(&needle);
            ber_orient(&mut mesh, facing);
        }
        VanillaBerState::Campfire { lit, item_count } => {
            for angle in [std::f32::consts::FRAC_PI_4, -std::f32::consts::FRAC_PI_4] {
                let mut log = ExtrudedMesh::new();
                ber_box(&mut log, [0.22, 0.08, 0.44], [0.78, 0.23, 0.56], 2);
                ber_rotate_y(&mut log, angle);
                mesh.append(&log);
            }
            if lit {
                for (height, width) in [(0.55, 0.10), (0.77, 0.045)] {
                    ber_box(
                        &mut mesh,
                        [0.5 - width, 0.23, 0.5 - width],
                        [0.5 + width, height, 0.5 + width],
                        1,
                    );
                }
            }
            for i in 0..usize::from(item_count.min(4)) {
                let x = 0.30 + (i % 2) as f32 * 0.30;
                let z = 0.30 + (i / 2) as f32 * 0.30;
                ber_box(&mut mesh, [x, 0.25, z], [x + 0.13, 0.29, z + 0.13], 3);
            }
        }
        VanillaBerState::BrewingStand { bottle_mask } => {
            ber_box(&mut mesh, [0.31, 0.04, 0.31], [0.69, 0.12, 0.69], 2);
            ber_box(&mut mesh, [0.46, 0.12, 0.46], [0.54, 0.87, 0.54], -1);
            for (i, (x, z)) in [(0.25_f32, 0.29_f32), (0.75, 0.29), (0.50, 0.76)]
                .iter()
                .copied()
                .enumerate()
            {
                let min = [x.min(0.5) - 0.02, 0.22, z.min(0.5) - 0.02];
                let max = [x.max(0.5) + 0.02, 0.29, z.max(0.5) + 0.02];
                ber_box(&mut mesh, min, max, 2);
                if bottle_mask & (1 << i) != 0 {
                    ber_box(
                        &mut mesh,
                        [x - 0.09, 0.10, z - 0.09],
                        [x + 0.09, 0.28, z + 0.09],
                        1,
                    );
                    ber_box(
                        &mut mesh,
                        [x - 0.035, 0.28, z - 0.035],
                        [x + 0.035, 0.37, z + 0.035],
                        1,
                    );
                }
            }
        }
        VanillaBerState::Lectern { has_book, facing } => {
            ber_box(&mut mesh, [0.22, 0.03, 0.22], [0.78, 0.14, 0.78], 2);
            ber_box(&mut mesh, [0.41, 0.14, 0.41], [0.59, 0.65, 0.59], -1);
            let mut desk = ExtrudedMesh::new();
            ber_box(&mut desk, [0.22, 0.64, 0.24], [0.78, 0.70, 0.76], 2);
            if has_book {
                ber_box(&mut desk, [0.29, 0.70, 0.30], [0.71, 0.73, 0.70], 1);
                ber_box(&mut desk, [0.49, 0.73, 0.31], [0.51, 0.75, 0.69], -1);
            }
            ber_rotate_x(&mut desk, -0.18, [0.5, 0.66, 0.5]);
            mesh.append(&desk);
            ber_orient(&mut mesh, facing);
        }
        VanillaBerState::DecoratedPot { facing, wobble } => {
            ber_box(&mut mesh, [0.29, 0.06, 0.29], [0.71, 0.15, 0.71], 2);
            ber_box(&mut mesh, [0.21, 0.15, 0.21], [0.79, 0.76, 0.79], -1);
            ber_box(&mut mesh, [0.27, 0.76, 0.27], [0.73, 0.85, 0.73], 2);
            ber_box(&mut mesh, [0.32, 0.85, 0.32], [0.68, 0.88, 0.68], 1);
            ber_rotate_x(&mut mesh, ber_angle(wobble, 0.1), [0.5, 0.1, 0.5]);
            ber_orient(&mut mesh, facing);
        }
        VanillaBerState::ShulkerBox {
            open_progress,
            facing,
        } => {
            ber_box(&mut mesh, [0.12, 0.09, 0.12], [0.88, 0.55, 0.88], -1);
            let lift = ber_unit(open_progress) * 0.16;
            ber_box(
                &mut mesh,
                [0.10, 0.50 + lift, 0.10],
                [0.90, 0.79 + lift, 0.90],
                1,
            );
            ber_orient(&mut mesh, facing);
        }
    }

    // Floating-point rotations can produce tiny excursions at exact 90°.
    // Clamp these round-off errors and update the normal from the CCW winding.
    for quad in &mut mesh.quads {
        for vertex in &mut quad.positions {
            for coord in vertex {
                *coord = coord.clamp(0.0, 1.0);
            }
        }
    }
    mesh
}

// ---------------------------------------------------------------------------
// Bounded, deterministic Vanilla particle simulation
// ---------------------------------------------------------------------------

/// Maximum number of live Vanilla particles, including in a single burst.
pub const MAX_VANILLA_PARTICLES: usize = 4096;

/// Particle sprite/shader families. Emission is deterministic and caller-owned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VanillaParticleKind {
    BlockBreak,
    BlockDig,
    Flame,
    Smoke,
    WaterSplash,
    Bubble,
    RainDrop,
    SnowFlake,
    LightningFlash,
    PotionSwirl,
    Crit,
    Explosion,
}

impl VanillaParticleKind {
    /// Linear drag in inverse seconds and upward-positive gravity in blocks/s².
    fn physics(self) -> (f64, f64) {
        match self {
            Self::BlockBreak | Self::BlockDig => (1.1, -9.8),
            Self::Flame => (1.8, 0.6),
            Self::Smoke => (0.8, 0.15),
            Self::WaterSplash => (0.7, -9.8),
            Self::Bubble => (1.0, 1.0),
            Self::RainDrop => (0.05, -16.0),
            Self::SnowFlake => (0.4, -1.6),
            Self::LightningFlash => (0.0, 0.0),
            Self::PotionSwirl => (1.2, -0.1),
            Self::Crit => (0.6, -6.0),
            Self::Explosion => (1.4, 0.0),
        }
    }

    /// Billboard half-size in blocks, prior to lifetime scaling.
    fn radius(self) -> f32 {
        match self {
            Self::BlockBreak | Self::BlockDig => 0.07,
            Self::Flame => 0.09,
            Self::Smoke => 0.14,
            Self::WaterSplash | Self::Bubble => 0.06,
            Self::RainDrop => 0.035,
            Self::SnowFlake => 0.065,
            Self::LightningFlash => 0.48,
            Self::PotionSwirl => 0.09,
            Self::Crit => 0.075,
            Self::Explosion => 0.35,
        }
    }
}

/// One live particle's state, exposed for direct GPU staging and inspection.
/// Original values are retained in f64 so integration never accumulates
/// position/velocity error across different frame partitions.
#[derive(Clone, Debug, PartialEq)]
pub struct VanillaParticle {
    pub kind: VanillaParticleKind,
    pub pos: [f32; 3],
    pub vel: [f32; 3],
    pub color: [f32; 4],
    pub age_secs: f32,
    pub max_age_secs: f32,
    origin: [f64; 3],
    initial_velocity: [f64; 3],
    elapsed: f64,
}

/// A camera-aligned CCW quad, ready for GPU vertex expansion or upload.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VanillaParticleQuad {
    pub kind: VanillaParticleKind,
    pub positions: [[f32; 3]; 4],
    pub uv: [[f32; 2]; 4],
    pub color: [f32; 4],
}

impl VanillaParticle {
    /// Age-scaled billboard half-size in blocks — the identical factor
    /// [`VanillaParticleWorld::billboard_quads`] multiplies the camera axes by.
    /// GPU-instanced paths pack this per particle instead of four expanded
    /// corners.
    pub fn billboard_half_size(&self) -> f32 {
        let remaining = (1.0 - self.age_secs / self.max_age_secs).clamp(0.0, 1.0);
        self.kind.radius() * (0.25 + 0.75 * remaining)
    }
}

/// FIFO particle pool. Identical calls produce identical meshes and states.
/// The oldest live particle is evicted when a spawn exceeds the fixed cap.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct VanillaParticleWorld {
    particles: VecDeque<VanillaParticle>,
}

impl VanillaParticleWorld {
    pub fn new() -> Self {
        Self {
            particles: VecDeque::with_capacity(MAX_VANILLA_PARTICLES),
        }
    }

    pub fn len(&self) -> usize {
        self.particles.len()
    }

    pub fn is_empty(&self) -> bool {
        self.particles.is_empty()
    }

    pub fn clear(&mut self) {
        self.particles.clear();
    }

    pub fn particles(&self) -> &VecDeque<VanillaParticle> {
        &self.particles
    }

    /// Add a particle. Invalid positions, velocities and lifetimes are rejected.
    /// Colors are clamped to [0,1], with non-finite channels replaced by zero.
    /// Returns whether the spawn was accepted.
    pub fn spawn(
        &mut self,
        kind: VanillaParticleKind,
        pos: [f32; 3],
        vel: [f32; 3],
        color: [f32; 4],
        max_age_secs: f32,
    ) -> bool {
        if !pos.iter().chain(vel.iter()).all(|v| v.is_finite())
            || !max_age_secs.is_finite()
            || max_age_secs <= 0.0
        {
            return false;
        }
        if self.particles.len() == MAX_VANILLA_PARTICLES {
            self.particles.pop_front();
        }
        self.particles.push_back(VanillaParticle {
            kind,
            pos,
            vel,
            color: color.map(ber_unit),
            age_secs: 0.0,
            max_age_secs,
            origin: pos.map(f64::from),
            initial_velocity: vel.map(f64::from),
            elapsed: 0.0,
        });
        true
    }

    /// Advance analytically under `dv/dt = -drag*v + gravity`, using the
    /// exact exponential for drag. Invalid, negative or zero dt is a no-op.
    /// Every update derives from the original spawn so 30 FPS and 120 FPS
    /// have the same trajectory for the same elapsed time (within float
    /// rounding). Expired and numerically overflowed particles are removed.
    pub fn step(&mut self, dt_secs: f32) {
        if !dt_secs.is_finite() || dt_secs <= 0.0 {
            return;
        }
        let dt = f64::from(dt_secs);
        self.particles.retain_mut(|particle| {
            particle.elapsed += dt;
            if particle.elapsed >= f64::from(particle.max_age_secs) {
                return false;
            }
            let t = particle.elapsed;
            let (drag, gravity) = particle.kind.physics();
            let (velocity_scale, distance_scale, gravity_displacement) = if drag > 0.0 {
                // exp_m1 is accurate for very small time steps.
                let one_minus_decay = -(-drag * t).exp_m1();
                let position_integral = one_minus_decay / drag;
                (
                    1.0 - one_minus_decay,
                    position_integral,
                    gravity / drag * (t - position_integral),
                )
            } else {
                (1.0, t, gravity * t * t * 0.5)
            };
            for axis in 0..3 {
                let acceleration = if axis == 1 { gravity } else { 0.0 };
                let velocity = if drag > 0.0 {
                    particle.initial_velocity[axis] * velocity_scale
                        + acceleration / drag * (1.0 - velocity_scale)
                } else {
                    particle.initial_velocity[axis] + acceleration * t
                };
                let position = particle.origin[axis]
                    + particle.initial_velocity[axis] * distance_scale
                    + if axis == 1 { gravity_displacement } else { 0.0 };
                particle.vel[axis] = velocity as f32;
                particle.pos[axis] = position as f32;
            }
            particle.age_secs = t as f32;
            particle
                .pos
                .iter()
                .chain(particle.vel.iter())
                .all(|v| v.is_finite())
        });
    }

    /// Orthonormalized billboard basis `(right, up)` — the exact axes
    /// [`Self::billboard_quads`] expands corners with. GPU-instanced
    /// particle paths scale these per particle instead of materializing
    /// the four world-space corners on the CPU.
    pub fn billboard_axes(
        camera_right: [f32; 3],
        camera_up: [f32; 3],
    ) -> ([f32; 3], [f32; 3]) {
        let right = particle_normalize(camera_right).unwrap_or([1.0, 0.0, 0.0]);
        let up_dot = particle_dot(right, camera_up);
        let orthogonal_up = [
            camera_up[0] - right[0] * up_dot,
            camera_up[1] - right[1] * up_dot,
            camera_up[2] - right[2] * up_dot,
        ];
        let fallback_up = if right[1].abs() < 0.9 {
            [0.0, 1.0, 0.0]
        } else {
            [0.0, 0.0, 1.0]
        };
        let up = particle_normalize(orthogonal_up).unwrap_or_else(|| {
            let projection = particle_dot(right, fallback_up);
            particle_normalize([
                fallback_up[0] - right[0] * projection,
                fallback_up[1] - right[1] * projection,
                fallback_up[2] - right[2] * projection,
            ])
            .unwrap_or([0.0, 1.0, 0.0])
        });
        (right, up)
    }

    /// Build screen-facing quads in oldest-to-newest spawn order. Degenerate
    /// camera axes use world-space fallbacks; non-finite quads are omitted.
    pub fn billboard_quads(
        &self,
        camera_right: [f32; 3],
        camera_up: [f32; 3],
    ) -> Vec<VanillaParticleQuad> {
        let (right, up) = Self::billboard_axes(camera_right, camera_up);
        let mut quads = Vec::with_capacity(self.particles.len());
        for particle in &self.particles {
            let size = particle.billboard_half_size();
            let corners = [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)];
            let positions = corners.map(|(x, y)| {
                [
                    particle.pos[0] + size * (x * right[0] + y * up[0]),
                    particle.pos[1] + size * (x * right[1] + y * up[1]),
                    particle.pos[2] + size * (x * right[2] + y * up[2]),
                ]
            });
            if positions.iter().flatten().all(|value| value.is_finite()) {
                quads.push(VanillaParticleQuad {
                    kind: particle.kind,
                    positions,
                    uv: [[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]],
                    color: particle.color,
                });
            }
        }
        quads
    }
}

fn particle_dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn particle_normalize(v: [f32; 3]) -> Option<[f32; 3]> {
    if !v.iter().all(|c| c.is_finite()) {
        return None;
    }
    let length = particle_dot(v, v).sqrt();
    if !length.is_finite() || length < 1e-8 {
        return None;
    }
    Some(v.map(|c| c / length))
}
