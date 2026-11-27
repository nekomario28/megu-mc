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

use crate::vanilla_plus_3d::{cuboid_face_uvs, ExtrudedMesh, ExtrudedQuad, MAX_UPGRADE_QUADS};
use crate::Direction;
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

/// Emit one CCW face, dropping non-finite and degenerate geometry. BERs may
/// extend outside the unit cube while animating, as vanilla models do.
fn ber_face(mesh: &mut ExtrudedMesh, points: [[f32; 3]; 4], uv: [[f32; 2]; 4], tint_index: i32) {
    if mesh.quads.len() >= MAX_UPGRADE_QUADS || !points.iter().flatten().all(|v| v.is_finite()) {
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
        uv,
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
    let uvs = cuboid_face_uvs([xx - x, yy - y, zz - z]);
    let faces = [
        (
            Direction::South,
            [[x, y, zz], [xx, y, zz], [xx, yy, zz], [x, yy, zz]],
        ),
        (
            Direction::North,
            [[xx, y, z], [x, y, z], [x, yy, z], [xx, yy, z]],
        ),
        (
            Direction::East,
            [[xx, y, zz], [xx, y, z], [xx, yy, z], [xx, yy, zz]],
        ),
        (
            Direction::West,
            [[x, y, z], [x, y, zz], [x, yy, zz], [x, yy, z]],
        ),
        (
            Direction::Up,
            [[x, yy, zz], [xx, yy, zz], [xx, yy, z], [x, yy, z]],
        ),
        (
            Direction::Down,
            [[x, y, z], [xx, y, z], [xx, y, zz], [x, y, zz]],
        ),
    ];
    for (direction, points) in faces {
        ber_face(mesh, points, uvs[direction as usize], tint);
    }
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

/// A shulker lid opens along local +Y, unlike the south-facing BERs.
fn ber_orient_shulker(mesh: &mut ExtrudedMesh, facing: VanillaBerFacing) {
    match facing {
        VanillaBerFacing::Up => {}
        VanillaBerFacing::Down => ber_rotate_x(mesh, std::f32::consts::PI, [0.5; 3]),
        _ => {
            ber_rotate_x(mesh, std::f32::consts::FRAC_PI_2, [0.5; 3]);
            ber_orient(mesh, facing);
        }
    }
}

/// Construct bounded, outward-CCW Vanilla 1.21.1 BER family geometry.
/// Material/texture selection is carried by `tint_index`; the beacon beam
/// stores its 24-bit RGB in that field. A beam taller than one block is
/// represented by its first local segment for instance-side repetition.
/// A mismatched `(kind, state)` pair returns an empty mesh.
fn build_legacy_ber_mesh(kind: VanillaBerKind, state: VanillaBerState) -> ExtrudedMesh {
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
            ber_orient_shulker(&mut mesh, facing);
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
// Complete Vanilla 1.21.1 BER catalogue (extension-compatible API)
// ---------------------------------------------------------------------------

/// Extended Vanilla 1.21.1 BER renderer families. The original 12-family
/// [`VanillaBerKind`] remains unchanged for mesh/upload ABI compatibility;
/// this catalogue adds the renderer types that cannot be represented there
/// without breaking downstream exhaustive matches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VanillaBerExtendedKind {
    Chest,
    ShulkerBox,
    Bed,
    Sign,
    Banner,
    Bell,
    EnchantingTable,
    Beacon,
    Conduit,
    Campfire,
    Lectern,
    DecoratedPot,
    BrushableSand,
    BrushableGravel,
    TrialSpawner,
    Vault,
    MovingPiston,
    Skull,
    ItemFrame,
    BrewingStand,
}

pub const VANILLA_EXTENDED_BER_KINDS: [VanillaBerExtendedKind; 20] = [
    VanillaBerExtendedKind::Chest,
    VanillaBerExtendedKind::ShulkerBox,
    VanillaBerExtendedKind::Bed,
    VanillaBerExtendedKind::Sign,
    VanillaBerExtendedKind::Banner,
    VanillaBerExtendedKind::Bell,
    VanillaBerExtendedKind::EnchantingTable,
    VanillaBerExtendedKind::Beacon,
    VanillaBerExtendedKind::Conduit,
    VanillaBerExtendedKind::Campfire,
    VanillaBerExtendedKind::Lectern,
    VanillaBerExtendedKind::DecoratedPot,
    VanillaBerExtendedKind::BrushableSand,
    VanillaBerExtendedKind::BrushableGravel,
    VanillaBerExtendedKind::TrialSpawner,
    VanillaBerExtendedKind::Vault,
    VanillaBerExtendedKind::MovingPiston,
    VanillaBerExtendedKind::Skull,
    VanillaBerExtendedKind::ItemFrame,
    VanillaBerExtendedKind::BrewingStand,
];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BerChestKind {
    #[default]
    Normal,
    Trapped,
    Ender,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BerDyeColor {
    White,
    Orange,
    Magenta,
    LightBlue,
    Yellow,
    Lime,
    Pink,
    Gray,
    LightGray,
    Cyan,
    Purple,
    Blue,
    Brown,
    Green,
    #[default]
    Red,
    Black,
}

impl BerDyeColor {
    pub const ALL: [Self; 16] = [
        Self::White, Self::Orange, Self::Magenta, Self::LightBlue,
        Self::Yellow, Self::Lime, Self::Pink, Self::Gray,
        Self::LightGray, Self::Cyan, Self::Purple, Self::Blue,
        Self::Brown, Self::Green, Self::Red, Self::Black,
    ];
    pub const fn tint_index(self) -> i32 {
        100 + self as i32
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BerBellMount {
    #[default]
    Floor,
    Wall,
    Ceiling,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BerSkullKind {
    Player,
    #[default]
    Skeleton,
    WitherSkeleton,
    Zombie,
    Creeper,
    Piglin,
    EnderDragon,
}

/// Full-fidelity state for the extended BER catalogue. Angles are radians
/// except chest `lid_open`, which is the vanilla zero-to-one hinge progress
/// rendered over the full 0..90 degree range.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum VanillaBerExtendedState {
    Chest { lid_open: f32, double: bool, facing: VanillaBerFacing, chest: BerChestKind },
    ShulkerBox { open_progress: f32, facing: VanillaBerFacing, dye: BerDyeColor },
    Bed { is_head: bool, facing: VanillaBerFacing, dye: BerDyeColor },
    Sign { wall: bool, hanging: bool, glowing: bool, facing_yaw: f32 },
    Banner { wall: bool, sway_rad: f32, facing_yaw: f32, dye: BerDyeColor },
    Bell { mount: BerBellMount, facing: VanillaBerFacing, swing_rad: f32 },
    EnchantingTable { facing_yaw: f32, book_yaw_rad: f32, page_open: f32 },
    Beacon { beam_height: f32, active: bool, rgb: [f32; 3], beam_yaw: f32 },
    Conduit { cage_yaw_rad: f32, eye_open: f32 },
    Campfire { lit: bool, item_count: u8, soul: bool },
    Lectern { has_book: bool, facing: VanillaBerFacing },
    DecoratedPot { facing: VanillaBerFacing, wobble: f32, sherds: [u8; 4] },
    BrushableSand { dusted: u8, item_id: u16, facing: VanillaBerFacing },
    BrushableGravel { dusted: u8, item_id: u16, facing: VanillaBerFacing },
    TrialSpawner { spin_rad: f32, active: bool, ominous: bool, mob_scale: f32 },
    Vault { spin_rad: f32, active: bool, ominous: bool },
    MovingPiston { facing: VanillaBerFacing, sticky: bool, extension_progress: f32 },
    Skull { skull: BerSkullKind, wall: bool, yaw_deg: f32, jaw_open: f32 },
    ItemFrame { facing: VanillaBerFacing, glow: bool, rotation: u8 },
    BrewingStand { bottle_mask: u8 },
}

impl VanillaBerExtendedState {
    pub fn kind(self) -> VanillaBerExtendedKind {
        match self {
            Self::Chest { .. } => VanillaBerExtendedKind::Chest,
            Self::ShulkerBox { .. } => VanillaBerExtendedKind::ShulkerBox,
            Self::Bed { .. } => VanillaBerExtendedKind::Bed,
            Self::Sign { .. } => VanillaBerExtendedKind::Sign,
            Self::Banner { .. } => VanillaBerExtendedKind::Banner,
            Self::Bell { .. } => VanillaBerExtendedKind::Bell,
            Self::EnchantingTable { .. } => VanillaBerExtendedKind::EnchantingTable,
            Self::Beacon { .. } => VanillaBerExtendedKind::Beacon,
            Self::Conduit { .. } => VanillaBerExtendedKind::Conduit,
            Self::Campfire { .. } => VanillaBerExtendedKind::Campfire,
            Self::Lectern { .. } => VanillaBerExtendedKind::Lectern,
            Self::DecoratedPot { .. } => VanillaBerExtendedKind::DecoratedPot,
            Self::BrushableSand { .. } => VanillaBerExtendedKind::BrushableSand,
            Self::BrushableGravel { .. } => VanillaBerExtendedKind::BrushableGravel,
            Self::TrialSpawner { .. } => VanillaBerExtendedKind::TrialSpawner,
            Self::Vault { .. } => VanillaBerExtendedKind::Vault,
            Self::MovingPiston { .. } => VanillaBerExtendedKind::MovingPiston,
            Self::Skull { .. } => VanillaBerExtendedKind::Skull,
            Self::ItemFrame { .. } => VanillaBerExtendedKind::ItemFrame,
            Self::BrewingStand { .. } => VanillaBerExtendedKind::BrewingStand,
        }
    }
}

impl From<VanillaBerKind> for VanillaBerExtendedKind {
    fn from(kind: VanillaBerKind) -> Self {
        match kind {
            VanillaBerKind::Chest => Self::Chest,
            VanillaBerKind::Sign => Self::Sign,
            VanillaBerKind::Banner => Self::Banner,
            VanillaBerKind::Beacon => Self::Beacon,
            VanillaBerKind::Bed => Self::Bed,
            VanillaBerKind::Skull => Self::Skull,
            VanillaBerKind::ItemFrame => Self::ItemFrame,
            VanillaBerKind::Campfire => Self::Campfire,
            VanillaBerKind::BrewingStand => Self::BrewingStand,
            VanillaBerKind::Lectern => Self::Lectern,
            VanillaBerKind::DecoratedPot => Self::DecoratedPot,
            VanillaBerKind::ShulkerBox => Self::ShulkerBox,
        }
    }
}

impl From<VanillaBerState> for VanillaBerExtendedState {
    fn from(state: VanillaBerState) -> Self {
        match state {
            VanillaBerState::Chest { lid_open, double, facing } => Self::Chest { lid_open, double, facing, chest: BerChestKind::Normal },
            VanillaBerState::Sign { wall, hanging, glowing, facing_yaw } => Self::Sign { wall, hanging, glowing, facing_yaw },
            VanillaBerState::Banner { wall, sway_rad, facing_yaw } => Self::Banner { wall, sway_rad, facing_yaw, dye: BerDyeColor::Red },
            VanillaBerState::Beacon { beam_height, active, rgb } => Self::Beacon { beam_height, active, rgb, beam_yaw: 0.0 },
            VanillaBerState::Bed { is_head, facing } => Self::Bed { is_head, facing, dye: BerDyeColor::Red },
            VanillaBerState::Skull { wall, yaw_deg } => Self::Skull { skull: BerSkullKind::Skeleton, wall, yaw_deg, jaw_open: 0.0 },
            VanillaBerState::ItemFrame { facing, glow, rotation } => Self::ItemFrame { facing, glow, rotation },
            VanillaBerState::Campfire { lit, item_count } => Self::Campfire { lit, item_count, soul: false },
            VanillaBerState::BrewingStand { bottle_mask } => Self::BrewingStand { bottle_mask },
            VanillaBerState::Lectern { has_book, facing } => Self::Lectern { has_book, facing },
            VanillaBerState::DecoratedPot { facing, wobble } => Self::DecoratedPot { facing, wobble, sherds: [0; 4] },
            VanillaBerState::ShulkerBox { open_progress, facing } => Self::ShulkerBox { open_progress, facing, dye: BerDyeColor::Purple },
        }
    }
}

/// Type-erased input accepted by the compatibility-preserving generic
/// [`build_ber_mesh`]. Legacy pairs continue through the legacy mesh builder;
/// extended pairs use the full 1.21.1 catalogue.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum VanillaBerKindInput {
    Legacy(VanillaBerKind),
    Extended(VanillaBerExtendedKind),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum VanillaBerStateInput {
    Legacy(VanillaBerState),
    Extended(VanillaBerExtendedState),
}

pub trait IntoVanillaBerKindInput {
    fn into_ber_kind_input(self) -> VanillaBerKindInput;
}
impl IntoVanillaBerKindInput for VanillaBerKind {
    fn into_ber_kind_input(self) -> VanillaBerKindInput { VanillaBerKindInput::Legacy(self) }
}
impl IntoVanillaBerKindInput for VanillaBerExtendedKind {
    fn into_ber_kind_input(self) -> VanillaBerKindInput { VanillaBerKindInput::Extended(self) }
}
pub trait IntoVanillaBerStateInput {
    fn into_ber_state_input(self) -> VanillaBerStateInput;
}
impl IntoVanillaBerStateInput for VanillaBerState {
    fn into_ber_state_input(self) -> VanillaBerStateInput { VanillaBerStateInput::Legacy(self) }
}
impl IntoVanillaBerStateInput for VanillaBerExtendedState {
    fn into_ber_state_input(self) -> VanillaBerStateInput { VanillaBerStateInput::Extended(self) }
}

/// Backwards-compatible BER mesh constructor. Existing calls using
/// [`VanillaBerKind`]/[`VanillaBerState`] compile and keep bit-identical
/// legacy geometry; new calls may pass [`VanillaBerExtendedKind`] and
/// [`VanillaBerExtendedState`] to reach the complete 1.21.1 catalogue.
pub fn build_ber_mesh<K, S>(kind: K, state: S) -> ExtrudedMesh
where
    K: IntoVanillaBerKindInput,
    S: IntoVanillaBerStateInput,
{
    match (kind.into_ber_kind_input(), state.into_ber_state_input()) {
        (VanillaBerKindInput::Legacy(k), VanillaBerStateInput::Legacy(s)) => build_legacy_ber_mesh(k, s),
        (VanillaBerKindInput::Extended(k), VanillaBerStateInput::Extended(s)) => build_extended_ber_mesh(k, s),
        _ => ExtrudedMesh::new(),
    }
}

fn ber_rotate_z(mesh: &mut ExtrudedMesh, angle: f32, pivot: [f32; 3]) {
    let (s, c) = angle.sin_cos();
    mesh.affine([[c, -s, 0.0], [s, c, 0.0], [0.0, 0.0, 1.0]], pivot, pivot);
}

fn ber_part_y(mesh: &mut ExtrudedMesh, from: [f32; 3], to: [f32; 3], tint: i32, yaw: f32) {
    let mut part = ExtrudedMesh::new();
    ber_box(&mut part, from, to, tint);
    ber_rotate_y(&mut part, yaw);
    mesh.append(&part);
}

fn ber_part_x(mesh: &mut ExtrudedMesh, from: [f32; 3], to: [f32; 3], tint: i32, angle: f32, pivot: [f32; 3]) {
    let mut part = ExtrudedMesh::new();
    ber_box(&mut part, from, to, tint);
    ber_rotate_x(&mut part, angle, pivot);
    mesh.append(&part);
}


/// Build one full Vanilla 1.21.1 BER mesh in bounded, outward-CCW,
/// block-local coordinates; animated parts may extend outside the unit cube
/// exactly as vanilla BER models do. Tint indices encode logical materials: dye colors
/// are `100 + dye`, sherd slots `140 + side * 16 + sherd`, chest materials
/// 10/11/12, emissive cores use small positive ids.
pub fn build_extended_ber_mesh(kind: VanillaBerExtendedKind, state: VanillaBerExtendedState) -> ExtrudedMesh {
    let mut mesh = ExtrudedMesh::new();
    if state.kind() != kind {
        return mesh;
    }
    match state {
        VanillaBerExtendedState::Chest { lid_open, double, facing, chest } => {
            let material = match chest { BerChestKind::Normal => 10, BerChestKind::Trapped => 11, BerChestKind::Ender => 12 };
            let inset = if double { 0.025 } else { 0.07 };
            let right = if double { 1.975 } else { 1.0 - inset };
            let center = if double { 1.0 } else { 0.5 };
            ber_box(&mut mesh, [inset, 0.05, 0.09], [right, 0.58, 0.91], material);
            let mut lid = ExtrudedMesh::new();
            ber_box(&mut lid, [inset, 0.58, 0.18], [right, 0.69, 0.78], material);
            ber_rotate_x(&mut lid, -ber_unit(lid_open) * std::f32::consts::FRAC_PI_2, [center, 0.58, 0.18]);
            mesh.append(&lid);
            let latch = match chest { BerChestKind::Trapped => 13, BerChestKind::Ender => 14, BerChestKind::Normal => 1 };
            ber_box(&mut mesh, [center - 0.04, 0.39, 0.91], [center + 0.04, 0.57, 0.96], latch);
            if double { ber_box(&mut mesh, [0.992, 0.06, 0.911], [1.008, 0.57, 0.921], latch); }
            ber_orient(&mut mesh, facing);
        }
        VanillaBerExtendedState::ShulkerBox { open_progress, facing, dye } => {
            let open = ber_unit(open_progress);
            let tint = dye.tint_index();
            ber_box(&mut mesh, [0.12, 0.08, 0.12], [0.88, 0.54, 0.88], tint);
            ber_box(&mut mesh, [0.18, 0.54, 0.18], [0.82, 0.58, 0.82], tint + 32);
            let mut lid = ExtrudedMesh::new();
            let lift = open * 0.14;
            ber_box(&mut lid, [0.10, 0.54 + lift, 0.10], [0.90, 0.76 + lift, 0.90], tint);
            ber_box(&mut lid, [0.46, 0.76 + lift, 0.18], [0.54, 0.80 + lift, 0.34], tint + 48);
            ber_rotate_y(&mut lid, open * 3.0 * std::f32::consts::FRAC_PI_2);
            mesh.append(&lid);
            ber_orient_shulker(&mut mesh, facing);
        }
        VanillaBerExtendedState::Bed { is_head, facing, dye } => {
            ber_box(&mut mesh, [0.10, 0.12, 0.06], [0.90, 0.38, 0.94], 20);
            ber_box(&mut mesh, [0.13, 0.38, 0.09], [0.87, 0.52, 0.91], dye.tint_index());
            for x in [0.16, 0.78] { ber_box(&mut mesh, [x, 0.04, if is_head { 0.10 } else { 0.82 }], [x + 0.06, 0.13, if is_head { 0.16 } else { 0.88 }], 21); }
            if is_head {
                ber_box(&mut mesh, [0.23, 0.52, 0.13], [0.77, 0.61, 0.42], 22);
                ber_box(&mut mesh, [0.12, 0.18, 0.06], [0.88, 0.36, 0.14], 21);
            }
            ber_orient(&mut mesh, facing);
        }
        VanillaBerExtendedState::Sign { wall, hanging, glowing, facing_yaw } => {
            let text = if glowing { 31 } else { 30 };
            ber_box(&mut mesh, [0.22, 0.42, 0.455], [0.78, 0.72, 0.545], text);
            if hanging {
                for x in [0.32, 0.66] {
                    ber_part_z_chain(&mut mesh, x, 0.71, 0.92);
                }
            } else if wall {
                ber_box(&mut mesh, [0.44, 0.49, 0.34], [0.56, 0.58, 0.455], 32);
            } else {
                ber_box(&mut mesh, [0.47, 0.05, 0.47], [0.53, 0.42, 0.53], 32);
                ber_box(&mut mesh, [0.38, 0.03, 0.38], [0.62, 0.07, 0.62], 32);
            }
            ber_rotate_y(&mut mesh, ber_yaw(facing_yaw));
        }
        VanillaBerExtendedState::Banner { wall, sway_rad, facing_yaw, dye } => {
            if wall {
                ber_box(&mut mesh, [0.25, 0.84, 0.36], [0.75, 0.89, 0.54], 33);
            } else {
                ber_box(&mut mesh, [0.48, 0.08, 0.48], [0.52, 0.96, 0.52], 33);
                ber_box(&mut mesh, [0.25, 0.82, 0.47], [0.75, 0.87, 0.53], 33);
            }
            let mut cloth = ExtrudedMesh::new();
            ber_box(&mut cloth, [0.28, 0.20, 0.478], [0.72, 0.82, 0.522], dye.tint_index());
            ber_rotate_x(&mut cloth, ber_angle(sway_rad, 0.18), [0.5, 0.82, 0.5]);
            mesh.append(&cloth);
            ber_rotate_y(&mut mesh, ber_yaw(facing_yaw));
        }
        VanillaBerExtendedState::Bell { mount, facing, swing_rad } => {
            match mount {
                BerBellMount::Floor => {
                    ber_box(&mut mesh, [0.16, 0.05, 0.43], [0.84, 0.13, 0.57], 34);
                    ber_box(&mut mesh, [0.20, 0.13, 0.46], [0.28, 0.76, 0.54], 34);
                    ber_box(&mut mesh, [0.72, 0.13, 0.46], [0.80, 0.76, 0.54], 34);
                    ber_box(&mut mesh, [0.18, 0.72, 0.42], [0.82, 0.80, 0.58], 34);
                }
                BerBellMount::Wall => ber_box(&mut mesh, [0.28, 0.68, 0.16], [0.72, 0.80, 0.55], 34),
                BerBellMount::Ceiling => ber_box(&mut mesh, [0.24, 0.82, 0.42], [0.76, 0.94, 0.58], 34),
            }
            let mut bell = ExtrudedMesh::new();
            ber_box(&mut bell, [0.33, 0.34, 0.34], [0.67, 0.68, 0.66], 35);
            ber_box(&mut bell, [0.39, 0.24, 0.39], [0.61, 0.36, 0.61], 35);
            ber_box(&mut bell, [0.47, 0.18, 0.47], [0.53, 0.27, 0.53], 36);
            ber_rotate_z(&mut bell, ber_angle(swing_rad, 0.55), [0.5, 0.72, 0.5]);
            mesh.append(&bell);
            ber_orient(&mut mesh, facing);
        }
        VanillaBerExtendedState::EnchantingTable { facing_yaw, book_yaw_rad, page_open } => {
            ber_box(&mut mesh, [0.10, 0.05, 0.10], [0.90, 0.20, 0.90], 40);
            ber_box(&mut mesh, [0.17, 0.20, 0.17], [0.83, 0.66, 0.83], 41);
            ber_box(&mut mesh, [0.20, 0.66, 0.20], [0.80, 0.72, 0.80], 40);
            let mut book = ExtrudedMesh::new();
            let open = ber_unit(page_open);
            ber_box(&mut book, [0.28, 0.76, 0.38], [0.49, 0.80, 0.66], 42);
            ber_box(&mut book, [0.51, 0.76, 0.38], [0.72, 0.80, 0.66], 42);
            ber_part_x(&mut book, [0.30, 0.80, 0.40], [0.48, 0.825, 0.64], 43, -0.35 * open, [0.49, 0.80, 0.52]);
            ber_part_x(&mut book, [0.52, 0.80, 0.40], [0.70, 0.825, 0.64], 43, 0.35 * open, [0.51, 0.80, 0.52]);
            ber_rotate_y(&mut book, ber_yaw(book_yaw_rad));
            mesh.append(&book);
            ber_rotate_y(&mut mesh, ber_yaw(facing_yaw));
        }
        VanillaBerExtendedState::Beacon { beam_height, active, rgb, beam_yaw } => {
            ber_box(&mut mesh, [0.07, 0.04, 0.07], [0.93, 0.18, 0.93], 44);
            ber_box(&mut mesh, [0.15, 0.18, 0.15], [0.85, 0.67, 0.85], 45);
            ber_box(&mut mesh, [0.24, 0.67, 0.24], [0.76, 0.73, 0.76], 44);
            ber_box(&mut mesh, [0.36, 0.29, 0.36], [0.64, 0.57, 0.64], 46);
            let height = if beam_height.is_nan() { 0.0 } else { beam_height.clamp(0.0, 384.0) };
            if active && height > 0.0 {
                let c = rgb.map(|v| (ber_unit(v) * 255.0).round() as i32);
                let tint = (c[0] << 16) | (c[1] << 8) | c[2];
                let mut beam = ExtrudedMesh::new();
                ber_box(&mut beam, [0.45, 0.73, 0.39], [0.55, 0.73 + height, 0.61], tint);
                ber_box(&mut beam, [0.39, 0.73, 0.45], [0.61, 0.73 + height, 0.55], tint);
                ber_rotate_y(&mut beam, ber_yaw(beam_yaw));
                mesh.append(&beam);
            }
        }
        VanillaBerExtendedState::Conduit { cage_yaw_rad, eye_open } => {
            ber_box(&mut mesh, [0.36, 0.36, 0.36], [0.64, 0.64, 0.64], 47);
            let eye = ber_unit(eye_open);
            ber_box(&mut mesh, [0.43, 0.43 + 0.04 * eye, 0.43], [0.57, 0.57 + 0.04 * eye, 0.57], 48);
            let yaw = ber_yaw(cage_yaw_rad);
            for offset in [0.0, std::f32::consts::FRAC_PI_2] {
                ber_part_y(&mut mesh, [0.20, 0.47, 0.47], [0.80, 0.53, 0.53], 49, yaw + offset);
                ber_part_y(&mut mesh, [0.47, 0.20, 0.47], [0.53, 0.80, 0.53], 49, yaw + offset);
                ber_part_y(&mut mesh, [0.47, 0.47, 0.20], [0.53, 0.53, 0.80], 49, yaw + offset);
            }
        }
        VanillaBerExtendedState::Campfire { lit, item_count, soul } => {
            for angle in [std::f32::consts::FRAC_PI_4, -std::f32::consts::FRAC_PI_4] {
                ber_part_y(&mut mesh, [0.20, 0.08, 0.43], [0.80, 0.23, 0.57], 50, angle);
            }
            ber_box(&mut mesh, [0.23, 0.23, 0.23], [0.77, 0.29, 0.77], if soul { 52 } else { 51 });
            if lit {
                for (height, width) in [(0.53, 0.10), (0.76, 0.045)] {
                    ber_box(&mut mesh, [0.5 - width, 0.29, 0.5 - width], [0.5 + width, height, 0.5 + width], if soul { 54 } else { 53 });
                }
            }
            for i in 0..usize::from(item_count.min(4)) {
                let x = 0.28 + (i % 2) as f32 * 0.31;
                let z = 0.28 + (i / 2) as f32 * 0.31;
                ber_box(&mut mesh, [x, 0.31, z], [x + 0.13, 0.35, z + 0.13], 55 + i as i32);
            }
        }
        VanillaBerExtendedState::Lectern { has_book, facing } => {
            ber_box(&mut mesh, [0.22, 0.03, 0.22], [0.78, 0.14, 0.78], 60);
            ber_box(&mut mesh, [0.41, 0.14, 0.41], [0.59, 0.64, 0.59], 61);
            let mut desk = ExtrudedMesh::new();
            ber_box(&mut desk, [0.20, 0.63, 0.23], [0.80, 0.70, 0.77], 60);
            if has_book {
                ber_box(&mut desk, [0.28, 0.70, 0.30], [0.49, 0.735, 0.70], 62);
                ber_box(&mut desk, [0.51, 0.70, 0.30], [0.72, 0.735, 0.70], 62);
                ber_box(&mut desk, [0.49, 0.70, 0.30], [0.51, 0.75, 0.70], 63);
            }
            ber_rotate_x(&mut desk, -0.18, [0.5, 0.65, 0.5]);
            mesh.append(&desk);
            ber_orient(&mut mesh, facing);
        }
        VanillaBerExtendedState::DecoratedPot { facing, wobble, sherds } => {
            ber_box(&mut mesh, [0.30, 0.05, 0.30], [0.70, 0.15, 0.70], 64);
            ber_box(&mut mesh, [0.22, 0.15, 0.22], [0.78, 0.75, 0.78], 65);
            for (side, sherd) in sherds.iter().copied().enumerate() {
                let tint = 140 + side as i32 * 16 + i32::from(sherd.min(15));
                match side {
                    0 => ber_box(&mut mesh, [0.24, 0.24, 0.775], [0.76, 0.66, 0.795], tint),
                    1 => ber_box(&mut mesh, [0.24, 0.24, 0.205], [0.76, 0.66, 0.225], tint),
                    2 => ber_box(&mut mesh, [0.775, 0.24, 0.24], [0.795, 0.66, 0.76], tint),
                    _ => ber_box(&mut mesh, [0.205, 0.24, 0.24], [0.225, 0.66, 0.76], tint),
                }
            }
            ber_box(&mut mesh, [0.28, 0.75, 0.28], [0.72, 0.85, 0.72], 64);
            ber_box(&mut mesh, [0.32, 0.85, 0.32], [0.68, 0.89, 0.68], 66);
            ber_rotate_x(&mut mesh, ber_angle(wobble, 0.1), [0.5, 0.1, 0.5]);
            ber_orient(&mut mesh, facing);
        }
        VanillaBerExtendedState::BrushableSand { dusted, item_id, facing } | VanillaBerExtendedState::BrushableGravel { dusted, item_id, facing } => {
            let gravel = matches!(state, VanillaBerExtendedState::BrushableGravel { .. });
            let base = if gravel { 70 } else { 71 };
            ber_box(&mut mesh, [0.02, 0.02, 0.02], [0.98, 0.98, 0.98], base);
            let stage = dusted.min(3);
            let protrude = f32::from(stage) / 3.0;
            if stage > 0 {
                let item_tint = 72 + i32::from(item_id.min(15));
                ber_box(&mut mesh, [0.42, 0.96, 0.42], [0.58, 0.99 + 0.20 * protrude, 0.58], item_tint);
                for layer in 0..stage {
                    let y = 0.88 - f32::from(layer) * 0.08;
                    ber_box(&mut mesh, [0.30, y, 0.30], [0.70, y + 0.035, 0.70], base + 2);
                }
            }
            ber_orient(&mut mesh, facing);
        }
        VanillaBerExtendedState::TrialSpawner { spin_rad, active, ominous, mob_scale } => {
            let glow = if ominous { 82 } else { 81 };
            ber_box(&mut mesh, [0.16, 0.08, 0.16], [0.84, 0.16, 0.84], 80);
            for x in [0.18, 0.76] {
                for z in [0.18, 0.76] {
                    ber_box(&mut mesh, [x, 0.16, z], [x + 0.06, 0.84, z + 0.06], 80);
                }
            }
            for y in [0.20, 0.76] {
                ber_box(&mut mesh, [0.18, y, 0.18], [0.84, y + 0.05, 0.24], 80);
                ber_box(&mut mesh, [0.18, y, 0.76], [0.84, y + 0.05, 0.82], 80);
                ber_box(&mut mesh, [0.18, y, 0.18], [0.24, y + 0.05, 0.82], 80);
                ber_box(&mut mesh, [0.76, y, 0.18], [0.82, y + 0.05, 0.82], 80);
            }
            if active {
                let scale = ber_unit(mob_scale).max(0.15) * 0.28;
                ber_part_y(&mut mesh, [0.5 - scale, 0.34, 0.5 - scale], [0.5 + scale, 0.34 + scale * 1.5, 0.5 + scale * 0.58], glow, ber_yaw(spin_rad));
            }
        }
        VanillaBerExtendedState::Vault { spin_rad, active, ominous } => {
            ber_box(&mut mesh, [0.12, 0.06, 0.12], [0.88, 0.86, 0.88], 84);
            ber_box(&mut mesh, [0.20, 0.14, 0.86], [0.80, 0.72, 0.91], 85);
            ber_box(&mut mesh, [0.42, 0.36, 0.91], [0.58, 0.55, 0.96], if ominous { 87 } else { 86 });
            if active {
                ber_part_y(&mut mesh, [0.35, 0.33, 0.41], [0.65, 0.57, 0.59], if ominous { 87 } else { 86 }, ber_yaw(spin_rad));
            }
        }
        VanillaBerExtendedState::MovingPiston { facing, sticky, extension_progress } => {
            let ext = ber_unit(extension_progress);
            ber_box(&mut mesh, [0.10, 0.10, 0.10], [0.90, 0.90, 0.24], 90);
            ber_box(&mut mesh, [0.34, 0.34, 0.24], [0.66, 0.66, 0.34 + 0.55 * ext], 91);
            let head_z = 0.34 + 0.55 * ext;
            ber_box(&mut mesh, [0.08, 0.08, head_z], [0.92, 0.92, head_z + 0.16], if sticky { 93 } else { 92 });
            ber_orient(&mut mesh, facing);
        }
        VanillaBerExtendedState::Skull { skull, wall, yaw_deg, jaw_open } => {
            let base = match skull {
                BerSkullKind::Player => 94,
                BerSkullKind::Skeleton => 95,
                BerSkullKind::WitherSkeleton => 96,
                BerSkullKind::Zombie => 97,
                BerSkullKind::Creeper => 98,
                BerSkullKind::Piglin => 99,
                BerSkullKind::EnderDragon => 100,
            };
            ber_box(&mut mesh, [0.27, 0.37, 0.27], [0.73, 0.78, 0.73], base);
            for x in [0.35, 0.59] { ber_box(&mut mesh, [x, 0.55, 0.73], [x + 0.06, 0.62, 0.745], 101); }
            ber_box(&mut mesh, [0.42, 0.40, 0.73], [0.58, 0.49, 0.745], 101);
            if matches!(skull, BerSkullKind::EnderDragon) {
                let jaw = ber_unit(jaw_open) * 0.16;
                ber_part_x(&mut mesh, [0.28, 0.30 - jaw, 0.48], [0.72, 0.38 - jaw, 0.82], base, 0.25 * ber_unit(jaw_open), [0.5, 0.38, 0.48]);
                ber_box(&mut mesh, [0.20, 0.66, 0.36], [0.31, 0.78, 0.48], base);
                ber_box(&mut mesh, [0.69, 0.66, 0.36], [0.80, 0.78, 0.48], base);
            } else if matches!(skull, BerSkullKind::Piglin) {
                ber_box(&mut mesh, [0.18, 0.60, 0.34], [0.28, 0.75, 0.55], base);
                ber_box(&mut mesh, [0.72, 0.60, 0.34], [0.82, 0.75, 0.55], base);
            }
            if wall { ber_box(&mut mesh, [0.44, 0.48, 0.14], [0.56, 0.61, 0.27], 102); } else { ber_box(&mut mesh, [0.41, 0.28, 0.41], [0.59, 0.37, 0.59], 102); }
            ber_rotate_y(&mut mesh, ber_yaw(yaw_deg.to_radians()));
        }
        VanillaBerExtendedState::ItemFrame { facing, glow, rotation } => {
            let tint = if glow { 111 } else { 110 };
            ber_box(&mut mesh, [0.22, 0.22, 0.88], [0.78, 0.78, 0.91], tint);
            ber_box(&mut mesh, [0.18, 0.18, 0.87], [0.82, 0.25, 0.94], 112);
            ber_box(&mut mesh, [0.18, 0.75, 0.87], [0.82, 0.82, 0.94], 112);
            ber_box(&mut mesh, [0.18, 0.25, 0.87], [0.25, 0.75, 0.94], 112);
            ber_box(&mut mesh, [0.75, 0.25, 0.87], [0.82, 0.75, 0.94], 112);
            let mut needle = ExtrudedMesh::new();
            ber_box(&mut needle, [0.48, 0.49, 0.94], [0.52, 0.73, 0.955], 113);
            let angle = f32::from(rotation % 8) * std::f32::consts::FRAC_PI_4;
            let (sn, cs) = angle.sin_cos();
            needle.affine([[cs, -sn, 0.0], [sn, cs, 0.0], [0.0, 0.0, 1.0]], [0.5, 0.5, 0.5], [0.5, 0.5, 0.5]);
            mesh.append(&needle);
            ber_orient(&mut mesh, facing);
        }
        VanillaBerExtendedState::BrewingStand { bottle_mask } => {
            ber_box(&mut mesh, [0.31, 0.04, 0.31], [0.69, 0.12, 0.69], 120);
            ber_box(&mut mesh, [0.46, 0.12, 0.46], [0.54, 0.87, 0.54], 121);
            for (i, (x, z)) in [(0.25_f32, 0.29_f32), (0.75, 0.29), (0.50, 0.76)].iter().copied().enumerate() {
                ber_box(&mut mesh, [x.min(0.5) - 0.02, 0.22, z.min(0.5) - 0.02], [x.max(0.5) + 0.02, 0.29, z.max(0.5) + 0.02], 120);
                if bottle_mask & (1 << i) != 0 {
                    ber_box(&mut mesh, [x - 0.09, 0.10, z - 0.09], [x + 0.09, 0.28, z + 0.09], 122);
                    ber_box(&mut mesh, [x - 0.035, 0.28, z - 0.035], [x + 0.035, 0.37, z + 0.035], 123);
                }
            }
        }
    }

    mesh
}

fn ber_part_z_chain(mesh: &mut ExtrudedMesh, x: f32, y0: f32, y1: f32) {
    ber_box(mesh, [x - 0.012, y0, 0.47], [x + 0.012, (y0 + y1) * 0.5, 0.53], 32);
    ber_box(mesh, [x + 0.025 - 0.012, (y0 + y1) * 0.5, 0.47], [x + 0.025 + 0.012, y1, 0.53], 32);
}


/// Classify a Java-edition block/entity registry id into the extended BER
/// catalogue. Accepts optional `minecraft:` prefixes and recognizes all color
/// and wood variants by their stable vanilla suffixes.
pub fn classify_vanilla_ber_extended(registry_id: &str) -> Option<VanillaBerExtendedKind> {
    use VanillaBerExtendedKind as K;
    let id = registry_id.strip_prefix("minecraft:").unwrap_or(registry_id);
    if matches!(id, "chest" | "trapped_chest" | "ender_chest") {
        return Some(K::Chest);
    }
    if id == "shulker_box" || id.ends_with("_shulker_box") {
        return Some(K::ShulkerBox);
    }
    if id.ends_with("_bed") && !id.ends_with("bedrock") {
        return Some(K::Bed);
    }
    if id.ends_with("_hanging_sign") || id.ends_with("_wall_hanging_sign") {
        return Some(K::Sign);
    }
    if id.ends_with("_sign") || id.ends_with("_wall_sign") {
        return Some(K::Sign);
    }
    if id.ends_with("_banner") || id.ends_with("_wall_banner") {
        return Some(K::Banner);
    }
    if id.ends_with("_head") || id.ends_with("_wall_head") || id.ends_with("_skull") || id.ends_with("_wall_skull") {
        return Some(K::Skull);
    }
    Some(match id {
        "bell" => K::Bell,
        "enchanting_table" => K::EnchantingTable,
        "beacon" => K::Beacon,
        "conduit" => K::Conduit,
        "campfire" | "soul_campfire" => K::Campfire,
        "lectern" => K::Lectern,
        "decorated_pot" => K::DecoratedPot,
        "suspicious_sand" => K::BrushableSand,
        "suspicious_gravel" => K::BrushableGravel,
        "trial_spawner" => K::TrialSpawner,
        "vault" => K::Vault,
        "piston_head" | "moving_piston" => K::MovingPiston,
        "item_frame" | "glow_item_frame" => K::ItemFrame,
        "brewing_stand" => K::BrewingStand,
        _ => return None,
    })
}

/// Neutral default state for each extended kind. World/blockstate decoders
/// overwrite animation fields; this helper is primarily useful for tests,
/// debug views and registry validation.
pub fn default_extended_ber_state(kind: VanillaBerExtendedKind) -> VanillaBerExtendedState {
    use VanillaBerExtendedState as S;
    match kind {
        VanillaBerExtendedKind::Chest => S::Chest { lid_open: 0.0, double: false, facing: VanillaBerFacing::North, chest: BerChestKind::Normal },
        VanillaBerExtendedKind::ShulkerBox => S::ShulkerBox { open_progress: 0.0, facing: VanillaBerFacing::Up, dye: BerDyeColor::Purple },
        VanillaBerExtendedKind::Bed => S::Bed { is_head: true, facing: VanillaBerFacing::North, dye: BerDyeColor::Red },
        VanillaBerExtendedKind::Sign => S::Sign { wall: false, hanging: false, glowing: false, facing_yaw: 0.0 },
        VanillaBerExtendedKind::Banner => S::Banner { wall: false, sway_rad: 0.0, facing_yaw: 0.0, dye: BerDyeColor::White },
        VanillaBerExtendedKind::Bell => S::Bell { mount: BerBellMount::Floor, facing: VanillaBerFacing::North, swing_rad: 0.0 },
        VanillaBerExtendedKind::EnchantingTable => S::EnchantingTable { facing_yaw: 0.0, book_yaw_rad: 0.0, page_open: 0.5 },
        VanillaBerExtendedKind::Beacon => S::Beacon { beam_height: 0.0, active: false, rgb: [1.0; 3], beam_yaw: 0.0 },
        VanillaBerExtendedKind::Conduit => S::Conduit { cage_yaw_rad: 0.0, eye_open: 1.0 },
        VanillaBerExtendedKind::Campfire => S::Campfire { lit: true, item_count: 0, soul: false },
        VanillaBerExtendedKind::Lectern => S::Lectern { has_book: false, facing: VanillaBerFacing::North },
        VanillaBerExtendedKind::DecoratedPot => S::DecoratedPot { facing: VanillaBerFacing::North, wobble: 0.0, sherds: [0; 4] },
        VanillaBerExtendedKind::BrushableSand => S::BrushableSand { dusted: 0, item_id: 0, facing: VanillaBerFacing::Up },
        VanillaBerExtendedKind::BrushableGravel => S::BrushableGravel { dusted: 0, item_id: 0, facing: VanillaBerFacing::Up },
        VanillaBerExtendedKind::TrialSpawner => S::TrialSpawner { spin_rad: 0.0, active: false, ominous: false, mob_scale: 0.5 },
        VanillaBerExtendedKind::Vault => S::Vault { spin_rad: 0.0, active: false, ominous: false },
        VanillaBerExtendedKind::MovingPiston => S::MovingPiston { facing: VanillaBerFacing::North, sticky: false, extension_progress: 0.0 },
        VanillaBerExtendedKind::Skull => S::Skull { skull: BerSkullKind::Skeleton, wall: false, yaw_deg: 0.0, jaw_open: 0.0 },
        VanillaBerExtendedKind::ItemFrame => S::ItemFrame { facing: VanillaBerFacing::North, glow: false, rotation: 0 },
        VanillaBerExtendedKind::BrewingStand => S::BrewingStand { bottle_mask: 0 },
    }
}

// ---------------------------------------------------------------------------
// Bounded, deterministic Vanilla particle simulation
// ---------------------------------------------------------------------------

/// Maximum number of live Vanilla particles, including in a single burst.
pub const MAX_VANILLA_PARTICLES: usize = 4096;
/// Number of particle-type entries in the protocol-767 (Java 1.21.1) registry.
/// The registry is often described through id 106 because `raid_omen` and
/// `trial_omen` were late 1.21 additions; on the wire 1.21.1 assigns them
/// ids 107 and 108, so the complete protocol-767 range is `0..=108`.
pub const PROTOCOL767_PARTICLE_COUNT: usize = 109;
/// Highest protocol-767 particle registry id (`trial_omen`).
pub const PROTOCOL767_MAX_PARTICLE_ID: u8 = 108;

/// Vanilla particle motion family. This is a semantic rendering hint used by
/// the deterministic integrator; `Linear` remains the exact exponential drag
/// model the original bounded particle pool used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VanillaParticleMotion {
    /// Exponential drag plus constant vertical acceleration.
    Linear,
    /// No translational acceleration; used by flashes, decals and emitters.
    Static,
    /// Rise-positive particles such as bubbles, flames, smoke and souls.
    Buoyant,
    /// Swirling attractor motion used by portal, enchant and effect motes.
    Swirl,
    /// Horizontal flutter superimposed on a slow fall (leaves, ash, snow).
    Flutter,
    /// Expanding shockwave ring / radial burst.
    Shockwave,
    /// Emitter anchor with a subtle deterministic pulse.
    Emitter,
}

/// Size-over-life family for one vanilla particle sprite.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VanillaParticleSizeCurve {
    Constant,
    /// Vanilla's common slow shrink, never reaching a degenerate zero quad.
    Shrink,
    /// Expanding ring/plume curve.
    Grow,
    /// Quick attack followed by a softer decay.
    Pop,
    /// Bright large flash collapsing rapidly.
    Flash,
}

impl VanillaParticleSizeCurve {
    /// Multiplier at normalized age `t` (`0..=1`). The lower bound keeps a
    /// final-frame billboard non-degenerate while retaining vanilla fade-out.
    pub fn scale(self, t: f32) -> f32 {
        let t = ber_unit(t);
        match self {
            Self::Constant => 1.0,
            Self::Shrink => 0.25 + 0.75 * (1.0 - t),
            Self::Grow => 0.25 + 1.15 * (1.0 - (1.0 - t) * (1.0 - t)),
            Self::Pop => {
                if t < 0.18 {
                    0.35 + 4.0 * t
                } else {
                    (1.15 - 0.9 * (t - 0.18) / 0.82).max(0.25)
                }
            }
            Self::Flash => (1.35 - 1.15 * t.sqrt()).max(0.20),
        }
    }
}

/// Payload layout required by a protocol particle type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VanillaParticlePayloadKind {
    None,
    /// `block`, `block_marker`, `falling_dust` and `dust_pillar` carry a
    /// vanilla block-state id.
    BlockState,
    /// `item` carries the item id (the item model supplies the sprite).
    ItemStack,
    /// `dust`: RGB plus scale.
    Dust,
    /// `dust_color_transition`: source RGB, target RGB plus scale.
    DustColorTransition,
    /// `vibration`: source, destination and travel time.
    Vibration,
    /// `entity_effect`: RGBA color bytes as floats.
    EntityEffect,
    /// `sculk_charge`: one float roll/rotation angle.
    SculkCharge,
    /// `shriek`: one VarInt delay in ticks.
    Shriek,
}

/// Static visual/physical description of one protocol-767 particle type.
/// Values are block units and seconds (lifetimes are additionally exposed in
/// vanilla ticks). `sprite_atlas_hint` is intentionally a stable logical
/// sprite key, not a baked Mojang asset.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VanillaParticleSpec {
    pub protocol_id: u8,
    pub registry_name: &'static str,
    pub min_lifetime_ticks: u16,
    pub max_lifetime_ticks: u16,
    pub base_radius: f32,
    pub size_curve: VanillaParticleSizeCurve,
    pub tint_rgba: [f32; 4],
    pub fullbright: bool,
    pub sprite_atlas_hint: &'static str,
    pub payload_kind: VanillaParticlePayloadKind,
    pub motion: VanillaParticleMotion,
    /// Exponential linear drag coefficient in inverse seconds.
    pub drag_per_sec: f64,
    /// Upward-positive vertical acceleration in blocks/s².
    pub gravity_per_sec2: f64,
}

impl VanillaParticleSpec {
    fn base(kind: VanillaParticleKind) -> Self {
        Self {
            protocol_id: kind.protocol_id(),
            registry_name: kind.registry_name(),
            min_lifetime_ticks: 20,
            max_lifetime_ticks: 40,
            base_radius: 0.07,
            size_curve: VanillaParticleSizeCurve::Shrink,
            tint_rgba: [1.0; 4],
            fullbright: false,
            sprite_atlas_hint: kind.registry_name(),
            payload_kind: VanillaParticlePayloadKind::None,
            motion: VanillaParticleMotion::Linear,
            drag_per_sec: 0.8,
            gravity_per_sec2: -8.0,
        }
    }

    /// Authentic 1.21.1 visual family, lifetime range, color tendency,
    /// fullbright flag, payload shape and physics class for `kind`.
    pub fn for_kind(kind: VanillaParticleKind) -> Self {
        use VanillaParticleKind as P;
        use VanillaParticleMotion as M;
        use VanillaParticlePayloadKind as Payload;
        use VanillaParticleSizeCurve as Curve;
        let mut s = Self::base(kind);
        match kind.canonical() {
            P::AngryVillager | P::HappyVillager | P::Composter | P::Heart => {
                s.min_lifetime_ticks = 20;
                s.max_lifetime_ticks = 50;
                s.base_radius = 0.09;
                s.gravity_per_sec2 = -1.5;
                s.tint_rgba = if matches!(kind.canonical(), P::AngryVillager) {
                    [0.45, 0.45, 0.45, 1.0]
                } else if matches!(kind.canonical(), P::Heart) {
                    [1.0, 0.25, 0.35, 1.0]
                } else {
                    [0.55, 1.0, 0.35, 1.0]
                };
            }
            P::Block | P::BlockMarker | P::FallingDust | P::DustPillar => {
                s.min_lifetime_ticks = 20;
                s.max_lifetime_ticks = 60;
                s.base_radius = 0.075;
                s.payload_kind = Payload::BlockState;
                s.drag_per_sec = 1.1;
                s.gravity_per_sec2 = -9.8;
                if matches!(kind.canonical(), P::DustPillar) {
                    s.motion = M::Shockwave;
                    s.size_curve = Curve::Grow;
                    s.base_radius = 0.16;
                }
            }
            P::Bubble | P::BubblePop | P::BubbleColumnUp => {
                s.min_lifetime_ticks = 16;
                s.max_lifetime_ticks = 55;
                s.base_radius = 0.055;
                s.motion = M::Buoyant;
                s.drag_per_sec = 1.0;
                s.gravity_per_sec2 = 1.0;
                s.tint_rgba = [0.35, 0.65, 1.0, 0.85];
            }
            P::Cloud | P::Poof | P::WhiteSmoke | P::Sneeze | P::Spit => {
                s.min_lifetime_ticks = 12;
                s.max_lifetime_ticks = 45;
                s.base_radius = 0.13;
                s.drag_per_sec = 1.2;
                s.gravity_per_sec2 = 0.0;
                s.tint_rgba = [0.92, 0.92, 0.92, 0.85];
            }
            P::Crit | P::EnchantedHit | P::DamageIndicator | P::SweepAttack => {
                s.min_lifetime_ticks = 10;
                s.max_lifetime_ticks = 24;
                s.base_radius = 0.085;
                s.size_curve = Curve::Pop;
                s.drag_per_sec = 0.6;
                s.gravity_per_sec2 = -6.0;
                s.fullbright = matches!(kind.canonical(), P::EnchantedHit | P::SweepAttack);
                s.tint_rgba = if matches!(kind.canonical(), P::DamageIndicator) {
                    [1.0, 0.25, 0.1, 1.0]
                } else {
                    [1.0, 0.95, 0.45, 1.0]
                };
                if matches!(kind.canonical(), P::SweepAttack) {
                    s.motion = M::Static;
                    s.base_radius = 0.28;
                }
            }
            P::DragonBreath | P::Witch | P::Effect | P::InstantEffect | P::EntityEffect => {
                s.min_lifetime_ticks = 20;
                s.max_lifetime_ticks = 70;
                s.base_radius = 0.09;
                s.motion = M::Swirl;
                s.drag_per_sec = 1.2;
                s.gravity_per_sec2 = -0.1;
                s.tint_rgba = if matches!(kind.canonical(), P::DragonBreath) {
                    [0.72, 0.25, 0.9, 0.9]
                } else {
                    [0.65, 0.25, 0.9, 0.95]
                };
                if matches!(kind.canonical(), P::EntityEffect) {
                    s.payload_kind = Payload::EntityEffect;
                }
            }
            P::DrippingLava
            | P::FallingLava
            | P::LandingLava
            | P::DrippingDripstoneLava
            | P::FallingDripstoneLava
            | P::Lava => {
                s.min_lifetime_ticks = 15;
                s.max_lifetime_ticks = 60;
                s.base_radius = 0.065;
                s.drag_per_sec = 0.25;
                s.gravity_per_sec2 = -10.5;
                s.tint_rgba = [1.0, 0.28, 0.02, 1.0];
                s.fullbright = true;
            }
            P::DrippingWater
            | P::FallingWater
            | P::DrippingDripstoneWater
            | P::FallingDripstoneWater
            | P::Splash
            | P::Fishing
            | P::Rain
            | P::Underwater
            | P::CurrentDown => {
                s.min_lifetime_ticks = 10;
                s.max_lifetime_ticks = 50;
                s.base_radius = 0.055;
                s.drag_per_sec = if matches!(kind.canonical(), P::Rain) { 0.05 } else { 0.7 };
                s.gravity_per_sec2 = if matches!(kind.canonical(), P::Rain) {
                    -16.0
                } else {
                    -9.8
                };
                s.tint_rgba = [0.25, 0.55, 1.0, 0.9];
            }
            P::Dust => {
                s.min_lifetime_ticks = 20;
                s.max_lifetime_ticks = 100;
                s.base_radius = 0.065;
                s.size_curve = Curve::Constant;
                s.payload_kind = Payload::Dust;
                s.drag_per_sec = 0.6;
                s.gravity_per_sec2 = -0.5;
            }
            P::DustColorTransition => {
                s.min_lifetime_ticks = 30;
                s.max_lifetime_ticks = 100;
                s.base_radius = 0.07;
                s.size_curve = Curve::Constant;
                s.payload_kind = Payload::DustColorTransition;
                s.motion = M::Swirl;
                s.drag_per_sec = 0.7;
                s.gravity_per_sec2 = -0.4;
                s.tint_rgba = [0.55, 0.9, 1.0, 1.0];
            }
            P::ElderGuardian => {
                s.min_lifetime_ticks = 40;
                s.max_lifetime_ticks = 60;
                s.base_radius = 0.55;
                s.size_curve = Curve::Flash;
                s.motion = M::Static;
                s.drag_per_sec = 0.0;
                s.gravity_per_sec2 = 0.0;
                s.fullbright = true;
            }
            P::Enchant | P::Nautilus => {
                s.min_lifetime_ticks = 30;
                s.max_lifetime_ticks = 90;
                s.base_radius = 0.07;
                s.motion = M::Swirl;
                s.drag_per_sec = 1.1;
                s.gravity_per_sec2 = 0.0;
                s.tint_rgba = [0.45, 0.75, 1.0, 0.95];
            }
            P::EndRod | P::Firework | P::Flash | P::Glow | P::ElectricSpark => {
                s.min_lifetime_ticks = 8;
                s.max_lifetime_ticks = 40;
                s.base_radius = 0.075;
                s.drag_per_sec = 0.8;
                s.gravity_per_sec2 = if matches!(kind.canonical(), P::EndRod) { 0.3 } else { -1.0 };
                s.fullbright = true;
                s.tint_rgba = if matches!(kind.canonical(), P::Glow) {
                    [0.45, 1.0, 0.65, 1.0]
                } else if matches!(kind.canonical(), P::ElectricSpark) {
                    [0.65, 0.9, 1.0, 1.0]
                } else {
                    [1.0, 0.9, 0.55, 1.0]
                };
                if matches!(kind.canonical(), P::Flash) {
                    s.motion = M::Static;
                    s.size_curve = Curve::Flash;
                    s.base_radius = 0.50;
                    s.drag_per_sec = 0.0;
                    s.gravity_per_sec2 = 0.0;
                }
            }
            P::ExplosionEmitter | P::GustEmitterLarge | P::GustEmitterSmall => {
                s.min_lifetime_ticks = 30;
                s.max_lifetime_ticks = 80;
                s.base_radius = 0.24;
                s.size_curve = Curve::Grow;
                s.motion = M::Emitter;
                s.drag_per_sec = 1.5;
                s.gravity_per_sec2 = 0.0;
                s.tint_rgba = [0.85, 0.85, 0.85, 0.75];
            }
            P::Explosion | P::Gust | P::SmallGust | P::SonicBoom | P::DustPlume => {
                s.min_lifetime_ticks = 4;
                s.max_lifetime_ticks = 24;
                s.base_radius = match kind.canonical() {
                    P::SmallGust => 0.14,
                    P::Gust => 0.24,
                    P::SonicBoom => 0.28,
                    P::DustPlume => 0.18,
                    _ => 0.35,
                };
                s.size_curve = Curve::Grow;
                s.motion = M::Shockwave;
                s.drag_per_sec = 1.4;
                s.gravity_per_sec2 = 0.0;
                s.fullbright = matches!(kind.canonical(), P::SonicBoom);
                s.tint_rgba = if matches!(kind.canonical(), P::SonicBoom) {
                    [0.25, 0.9, 1.0, 1.0]
                } else if matches!(kind.canonical(), P::DustPlume) {
                    [0.65, 0.55, 0.42, 0.85]
                } else {
                    [0.92, 0.92, 0.88, 0.95]
                };
            }
            P::Flame | P::SmallFlame | P::SoulFireFlame => {
                s.min_lifetime_ticks = 10;
                s.max_lifetime_ticks = 25;
                s.base_radius = if matches!(kind.canonical(), P::SmallFlame) { 0.055 } else { 0.09 };
                s.motion = M::Buoyant;
                s.drag_per_sec = 1.8;
                s.gravity_per_sec2 = 0.6;
                s.fullbright = true;
                s.tint_rgba = if matches!(kind.canonical(), P::SoulFireFlame) {
                    [0.15, 0.85, 1.0, 1.0]
                } else {
                    [1.0, 0.42, 0.05, 1.0]
                };
            }
            P::Infested => {
                s.min_lifetime_ticks = 30;
                s.max_lifetime_ticks = 70;
                s.base_radius = 0.075;
                s.tint_rgba = [0.55, 0.65, 0.45, 1.0];
            }
            P::CherryLeaves => {
                s.min_lifetime_ticks = 40;
                s.max_lifetime_ticks = 120;
                s.base_radius = 0.065;
                s.motion = M::Flutter;
                s.drag_per_sec = 0.4;
                s.gravity_per_sec2 = -1.2;
                s.tint_rgba = [0.97, 0.55, 0.75, 0.95];
            }
            P::SculkSoul | P::Soul => {
                s.min_lifetime_ticks = 20;
                s.max_lifetime_ticks = 70;
                s.base_radius = 0.085;
                s.motion = M::Buoyant;
                s.drag_per_sec = 0.9;
                s.gravity_per_sec2 = 1.1;
                s.fullbright = matches!(kind.canonical(), P::SculkSoul);
                s.tint_rgba = if matches!(kind.canonical(), P::SculkSoul) {
                    [0.1, 0.75, 0.85, 0.95]
                } else {
                    [0.35, 0.45, 0.65, 0.9]
                };
            }
            P::SculkCharge | P::SculkChargePop | P::Shriek => {
                s.min_lifetime_ticks = 12;
                s.max_lifetime_ticks = 40;
                s.base_radius = 0.13;
                s.size_curve = if matches!(kind.canonical(), P::SculkChargePop | P::Shriek) {
                    Curve::Pop
                } else {
                    Curve::Constant
                };
                s.motion = if matches!(kind.canonical(), P::Shriek) { M::Static } else { M::Buoyant };
                s.drag_per_sec = 0.9;
                s.gravity_per_sec2 = 0.3;
                s.fullbright = true;
                s.tint_rgba = [0.1, 0.85, 0.9, 1.0];
                if matches!(kind.canonical(), P::SculkCharge) {
                    s.payload_kind = Payload::SculkCharge;
                } else if matches!(kind.canonical(), P::Shriek) {
                    s.payload_kind = Payload::Shriek;
                }
            }
            P::Item | P::ItemSlime | P::ItemCobweb | P::ItemSnowball => {
                s.min_lifetime_ticks = 15;
                s.max_lifetime_ticks = 45;
                s.base_radius = 0.065;
                s.drag_per_sec = 1.1;
                s.gravity_per_sec2 = -9.8;
                if matches!(kind.canonical(), P::Item) {
                    s.payload_kind = Payload::ItemStack;
                }
            }
            P::Vibration => {
                s.min_lifetime_ticks = 20;
                s.max_lifetime_ticks = 100;
                s.base_radius = 0.08;
                s.motion = M::Swirl;
                s.payload_kind = Payload::Vibration;
                s.drag_per_sec = 0.0;
                s.gravity_per_sec2 = 0.0;
                s.fullbright = true;
                s.tint_rgba = [0.2, 0.9, 1.0, 1.0];
            }
            P::LargeSmoke | P::Smoke | P::CampfireCosySmoke | P::CampfireSignalSmoke => {
                s.min_lifetime_ticks = 45;
                s.max_lifetime_ticks = if matches!(kind.canonical(), P::CampfireSignalSmoke) {
                    180
                } else {
                    120
                };
                s.base_radius = if matches!(kind.canonical(), P::LargeSmoke) { 0.18 } else { 0.14 };
                s.size_curve = Curve::Grow;
                s.motion = M::Buoyant;
                s.drag_per_sec = 0.8;
                s.gravity_per_sec2 = 0.15;
                s.tint_rgba = [0.35, 0.35, 0.35, 0.8];
            }
            P::Mycelium | P::Note | P::Portal | P::ReversePortal | P::TotemOfUndying => {
                s.min_lifetime_ticks = 25;
                s.max_lifetime_ticks = 90;
                s.base_radius = 0.075;
                s.motion = M::Swirl;
                s.drag_per_sec = 1.0;
                s.gravity_per_sec2 = 0.0;
                s.fullbright = matches!(kind.canonical(), P::TotemOfUndying);
                s.tint_rgba = match kind.canonical() {
                    P::Note => [0.2, 1.0, 0.35, 1.0],
                    P::Mycelium => [0.65, 0.45, 0.75, 0.9],
                    P::TotemOfUndying => [1.0, 0.85, 0.2, 1.0],
                    _ => [0.55, 0.2, 0.9, 0.95],
                };
            }
            P::SquidInk | P::GlowSquidInk => {
                s.min_lifetime_ticks = 25;
                s.max_lifetime_ticks = 80;
                s.base_radius = 0.12;
                s.motion = M::Buoyant;
                s.drag_per_sec = 0.7;
                s.gravity_per_sec2 = 0.15;
                s.fullbright = matches!(kind.canonical(), P::GlowSquidInk);
                s.tint_rgba = if matches!(kind.canonical(), P::GlowSquidInk) {
                    [0.15, 0.85, 0.8, 0.95]
                } else {
                    [0.03, 0.04, 0.08, 0.95]
                };
            }
            P::DrippingHoney
            | P::FallingHoney
            | P::LandingHoney
            | P::FallingNectar
            | P::FallingSporeBlossom
            | P::SporeBlossomAir => {
                s.min_lifetime_ticks = 20;
                s.max_lifetime_ticks = 100;
                s.base_radius = 0.06;
                s.motion = if matches!(kind.canonical(), P::FallingSporeBlossom | P::SporeBlossomAir) {
                    M::Flutter
                } else {
                    M::Linear
                };
                s.drag_per_sec = 0.25;
                s.gravity_per_sec2 = -5.5;
                s.tint_rgba = if matches!(kind.canonical(), P::FallingSporeBlossom | P::SporeBlossomAir) {
                    [0.55, 0.9, 0.45, 0.95]
                } else {
                    [1.0, 0.65, 0.1, 1.0]
                };
            }
            P::Ash
            | P::CrimsonSpore
            | P::WarpedSpore
            | P::WhiteAsh
            | P::Snowflake => {
                s.min_lifetime_ticks = 40;
                s.max_lifetime_ticks = 140;
                s.base_radius = if matches!(kind.canonical(), P::Snowflake) { 0.065 } else { 0.055 };
                s.motion = M::Flutter;
                s.drag_per_sec = 0.4;
                s.gravity_per_sec2 = if matches!(kind.canonical(), P::Snowflake) { -1.6 } else { -0.7 };
                s.tint_rgba = match kind.canonical() {
                    P::CrimsonSpore => [0.75, 0.15, 0.2, 0.9],
                    P::WarpedSpore => [0.1, 0.65, 0.65, 0.9],
                    P::Snowflake | P::WhiteAsh => [0.92, 0.95, 1.0, 0.95],
                    _ => [0.45, 0.43, 0.40, 0.9],
                };
            }
            P::DrippingObsidianTear | P::FallingObsidianTear | P::LandingObsidianTear => {
                s.min_lifetime_ticks = 20;
                s.max_lifetime_ticks = 80;
                s.base_radius = 0.06;
                s.drag_per_sec = 0.25;
                s.gravity_per_sec2 = -8.0;
                s.tint_rgba = [0.35, 0.1, 0.55, 1.0];
            }
            P::WaxOn | P::WaxOff | P::Scrape => {
                s.min_lifetime_ticks = 8;
                s.max_lifetime_ticks = 25;
                s.base_radius = 0.055;
                s.drag_per_sec = 1.0;
                s.gravity_per_sec2 = -3.0;
                s.fullbright = matches!(kind.canonical(), P::WaxOn);
                s.tint_rgba = if matches!(kind.canonical(), P::Scrape) {
                    [0.75, 0.45, 0.18, 1.0]
                } else {
                    [1.0, 0.85, 0.35, 1.0]
                };
            }
            P::EggCrack => {
                s.min_lifetime_ticks = 15;
                s.max_lifetime_ticks = 35;
                s.base_radius = 0.07;
                s.drag_per_sec = 1.1;
                s.gravity_per_sec2 = -9.8;
                s.tint_rgba = [0.95, 0.9, 0.75, 1.0];
            }
            P::Dolphin => {
                s.min_lifetime_ticks = 20;
                s.max_lifetime_ticks = 70;
                s.base_radius = 0.07;
                s.motion = M::Buoyant;
                s.drag_per_sec = 0.8;
                s.gravity_per_sec2 = 0.5;
                s.tint_rgba = [0.25, 0.75, 0.55, 0.95];
            }
            P::TrialSpawnerDetectedPlayer
            | P::TrialSpawnerDetectedPlayerOminous
            | P::VaultConnection
            | P::OminousSpawning
            | P::RaidOmen
            | P::TrialOmen => {
                let ominous = matches!(
                    kind.canonical(),
                    P::TrialSpawnerDetectedPlayerOminous | P::OminousSpawning | P::RaidOmen | P::TrialOmen
                );
                s.min_lifetime_ticks = 15;
                s.max_lifetime_ticks = 60;
                s.base_radius = 0.11;
                s.size_curve = if matches!(kind.canonical(), P::VaultConnection) {
                    Curve::Constant
                } else {
                    Curve::Grow
                };
                s.motion = if matches!(kind.canonical(), P::VaultConnection) {
                    M::Swirl
                } else {
                    M::Shockwave
                };
                s.drag_per_sec = 0.9;
                s.gravity_per_sec2 = 0.0;
                s.fullbright = true;
                s.tint_rgba = if ominous {
                    [0.15, 0.55, 1.0, 1.0]
                } else {
                    [1.0, 0.45, 0.08, 1.0]
                };
            }
            // Legacy compatibility variants canonicalize before reaching this
            // match, so they are intentionally unreachable here.
            P::BlockBreak
            | P::BlockDig
            | P::WaterSplash
            | P::RainDrop
            | P::SnowFlake
            | P::LightningFlash
            | P::PotionSwirl => unreachable!("legacy particle kinds canonicalize"),
        }
        s
    }
}

/// Particle sprite/shader families. Canonical variants are in exact
/// protocol-767 registry order. The seven trailing legacy variants preserve
/// the original public API and canonicalize onto the protocol entries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VanillaParticleKind {
    AngryVillager,
    Block,
    BlockMarker,
    Bubble,
    Cloud,
    Crit,
    DamageIndicator,
    DragonBreath,
    DrippingLava,
    FallingLava,
    LandingLava,
    DrippingWater,
    FallingWater,
    Dust,
    DustColorTransition,
    Effect,
    ElderGuardian,
    EnchantedHit,
    Enchant,
    EndRod,
    EntityEffect,
    ExplosionEmitter,
    Explosion,
    Gust,
    SmallGust,
    GustEmitterLarge,
    GustEmitterSmall,
    SonicBoom,
    FallingDust,
    Firework,
    Fishing,
    Flame,
    Infested,
    CherryLeaves,
    SculkSoul,
    SculkCharge,
    SculkChargePop,
    SoulFireFlame,
    Soul,
    Flash,
    HappyVillager,
    Composter,
    Heart,
    InstantEffect,
    Item,
    Vibration,
    ItemSlime,
    ItemCobweb,
    ItemSnowball,
    LargeSmoke,
    Lava,
    Mycelium,
    Note,
    Poof,
    Portal,
    Rain,
    Smoke,
    WhiteSmoke,
    Sneeze,
    Spit,
    SquidInk,
    SweepAttack,
    TotemOfUndying,
    Underwater,
    Splash,
    Witch,
    BubblePop,
    CurrentDown,
    BubbleColumnUp,
    Nautilus,
    Dolphin,
    CampfireCosySmoke,
    CampfireSignalSmoke,
    DrippingHoney,
    FallingHoney,
    LandingHoney,
    FallingNectar,
    FallingSporeBlossom,
    Ash,
    CrimsonSpore,
    WarpedSpore,
    SporeBlossomAir,
    DrippingObsidianTear,
    FallingObsidianTear,
    LandingObsidianTear,
    ReversePortal,
    WhiteAsh,
    SmallFlame,
    Snowflake,
    DrippingDripstoneLava,
    FallingDripstoneLava,
    DrippingDripstoneWater,
    FallingDripstoneWater,
    GlowSquidInk,
    Glow,
    WaxOn,
    WaxOff,
    ElectricSpark,
    Scrape,
    Shriek,
    EggCrack,
    DustPlume,
    TrialSpawnerDetectedPlayer,
    TrialSpawnerDetectedPlayerOminous,
    VaultConnection,
    DustPillar,
    OminousSpawning,
    RaidOmen,
    TrialOmen,
    // Backwards-compatible logical families from the original renderer.
    BlockBreak,
    BlockDig,
    WaterSplash,
    RainDrop,
    SnowFlake,
    LightningFlash,
    PotionSwirl,
}

/// Exact protocol-767 particle kinds indexed by registry id.
pub const PROTOCOL767_PARTICLE_KINDS: [VanillaParticleKind; PROTOCOL767_PARTICLE_COUNT] = [
    VanillaParticleKind::AngryVillager,
    VanillaParticleKind::Block,
    VanillaParticleKind::BlockMarker,
    VanillaParticleKind::Bubble,
    VanillaParticleKind::Cloud,
    VanillaParticleKind::Crit,
    VanillaParticleKind::DamageIndicator,
    VanillaParticleKind::DragonBreath,
    VanillaParticleKind::DrippingLava,
    VanillaParticleKind::FallingLava,
    VanillaParticleKind::LandingLava,
    VanillaParticleKind::DrippingWater,
    VanillaParticleKind::FallingWater,
    VanillaParticleKind::Dust,
    VanillaParticleKind::DustColorTransition,
    VanillaParticleKind::Effect,
    VanillaParticleKind::ElderGuardian,
    VanillaParticleKind::EnchantedHit,
    VanillaParticleKind::Enchant,
    VanillaParticleKind::EndRod,
    VanillaParticleKind::EntityEffect,
    VanillaParticleKind::ExplosionEmitter,
    VanillaParticleKind::Explosion,
    VanillaParticleKind::Gust,
    VanillaParticleKind::SmallGust,
    VanillaParticleKind::GustEmitterLarge,
    VanillaParticleKind::GustEmitterSmall,
    VanillaParticleKind::SonicBoom,
    VanillaParticleKind::FallingDust,
    VanillaParticleKind::Firework,
    VanillaParticleKind::Fishing,
    VanillaParticleKind::Flame,
    VanillaParticleKind::Infested,
    VanillaParticleKind::CherryLeaves,
    VanillaParticleKind::SculkSoul,
    VanillaParticleKind::SculkCharge,
    VanillaParticleKind::SculkChargePop,
    VanillaParticleKind::SoulFireFlame,
    VanillaParticleKind::Soul,
    VanillaParticleKind::Flash,
    VanillaParticleKind::HappyVillager,
    VanillaParticleKind::Composter,
    VanillaParticleKind::Heart,
    VanillaParticleKind::InstantEffect,
    VanillaParticleKind::Item,
    VanillaParticleKind::Vibration,
    VanillaParticleKind::ItemSlime,
    VanillaParticleKind::ItemCobweb,
    VanillaParticleKind::ItemSnowball,
    VanillaParticleKind::LargeSmoke,
    VanillaParticleKind::Lava,
    VanillaParticleKind::Mycelium,
    VanillaParticleKind::Note,
    VanillaParticleKind::Poof,
    VanillaParticleKind::Portal,
    VanillaParticleKind::Rain,
    VanillaParticleKind::Smoke,
    VanillaParticleKind::WhiteSmoke,
    VanillaParticleKind::Sneeze,
    VanillaParticleKind::Spit,
    VanillaParticleKind::SquidInk,
    VanillaParticleKind::SweepAttack,
    VanillaParticleKind::TotemOfUndying,
    VanillaParticleKind::Underwater,
    VanillaParticleKind::Splash,
    VanillaParticleKind::Witch,
    VanillaParticleKind::BubblePop,
    VanillaParticleKind::CurrentDown,
    VanillaParticleKind::BubbleColumnUp,
    VanillaParticleKind::Nautilus,
    VanillaParticleKind::Dolphin,
    VanillaParticleKind::CampfireCosySmoke,
    VanillaParticleKind::CampfireSignalSmoke,
    VanillaParticleKind::DrippingHoney,
    VanillaParticleKind::FallingHoney,
    VanillaParticleKind::LandingHoney,
    VanillaParticleKind::FallingNectar,
    VanillaParticleKind::FallingSporeBlossom,
    VanillaParticleKind::Ash,
    VanillaParticleKind::CrimsonSpore,
    VanillaParticleKind::WarpedSpore,
    VanillaParticleKind::SporeBlossomAir,
    VanillaParticleKind::DrippingObsidianTear,
    VanillaParticleKind::FallingObsidianTear,
    VanillaParticleKind::LandingObsidianTear,
    VanillaParticleKind::ReversePortal,
    VanillaParticleKind::WhiteAsh,
    VanillaParticleKind::SmallFlame,
    VanillaParticleKind::Snowflake,
    VanillaParticleKind::DrippingDripstoneLava,
    VanillaParticleKind::FallingDripstoneLava,
    VanillaParticleKind::DrippingDripstoneWater,
    VanillaParticleKind::FallingDripstoneWater,
    VanillaParticleKind::GlowSquidInk,
    VanillaParticleKind::Glow,
    VanillaParticleKind::WaxOn,
    VanillaParticleKind::WaxOff,
    VanillaParticleKind::ElectricSpark,
    VanillaParticleKind::Scrape,
    VanillaParticleKind::Shriek,
    VanillaParticleKind::EggCrack,
    VanillaParticleKind::DustPlume,
    VanillaParticleKind::TrialSpawnerDetectedPlayer,
    VanillaParticleKind::TrialSpawnerDetectedPlayerOminous,
    VanillaParticleKind::VaultConnection,
    VanillaParticleKind::DustPillar,
    VanillaParticleKind::OminousSpawning,
    VanillaParticleKind::RaidOmen,
    VanillaParticleKind::TrialOmen,
];

impl VanillaParticleKind {
    /// Canonical protocol variant for this kind. Legacy logical families map
    /// to their closest exact vanilla particle entry.
    pub const fn canonical(self) -> Self {
        match self {
            Self::BlockBreak | Self::BlockDig => Self::Block,
            Self::WaterSplash => Self::Splash,
            Self::RainDrop => Self::Rain,
            Self::SnowFlake => Self::Snowflake,
            Self::LightningFlash => Self::Flash,
            Self::PotionSwirl => Self::Effect,
            _ => self,
        }
    }

    /// Resolve one protocol-767 registry id (`0..=108`).
    pub fn from_protocol_id(protocol_id: u8) -> Option<Self> {
        PROTOCOL767_PARTICLE_KINDS.get(usize::from(protocol_id)).copied()
    }

    /// Resolve a vanilla registry name. Two common 1.21 documentation aliases
    /// (`trial_spawner_detection*`) are accepted in addition to the exact wire
    /// names (`trial_spawner_detected_player*`).
    pub fn from_registry_name(name: &str) -> Option<Self> {
        let name = name.strip_prefix("minecraft:").unwrap_or(name);
        Some(match name {
            "angry_villager" => Self::AngryVillager,
            "block" => Self::Block,
            "block_marker" => Self::BlockMarker,
            "bubble" => Self::Bubble,
            "cloud" => Self::Cloud,
            "crit" => Self::Crit,
            "damage_indicator" => Self::DamageIndicator,
            "dragon_breath" => Self::DragonBreath,
            "dripping_lava" => Self::DrippingLava,
            "falling_lava" => Self::FallingLava,
            "landing_lava" => Self::LandingLava,
            "dripping_water" => Self::DrippingWater,
            "falling_water" => Self::FallingWater,
            "dust" => Self::Dust,
            "dust_color_transition" => Self::DustColorTransition,
            "effect" => Self::Effect,
            "elder_guardian" => Self::ElderGuardian,
            "enchanted_hit" => Self::EnchantedHit,
            "enchant" => Self::Enchant,
            "end_rod" => Self::EndRod,
            "entity_effect" => Self::EntityEffect,
            "explosion_emitter" => Self::ExplosionEmitter,
            "explosion" => Self::Explosion,
            "gust" => Self::Gust,
            "small_gust" => Self::SmallGust,
            "gust_emitter_large" => Self::GustEmitterLarge,
            "gust_emitter_small" => Self::GustEmitterSmall,
            "sonic_boom" => Self::SonicBoom,
            "falling_dust" => Self::FallingDust,
            "firework" => Self::Firework,
            "fishing" => Self::Fishing,
            "flame" => Self::Flame,
            "infested" => Self::Infested,
            "cherry_leaves" => Self::CherryLeaves,
            "sculk_soul" => Self::SculkSoul,
            "sculk_charge" => Self::SculkCharge,
            "sculk_charge_pop" => Self::SculkChargePop,
            "soul_fire_flame" => Self::SoulFireFlame,
            "soul" => Self::Soul,
            "flash" => Self::Flash,
            "happy_villager" => Self::HappyVillager,
            "composter" => Self::Composter,
            "heart" => Self::Heart,
            "instant_effect" => Self::InstantEffect,
            "item" => Self::Item,
            "vibration" => Self::Vibration,
            "item_slime" => Self::ItemSlime,
            "item_cobweb" => Self::ItemCobweb,
            "item_snowball" => Self::ItemSnowball,
            "large_smoke" => Self::LargeSmoke,
            "lava" => Self::Lava,
            "mycelium" => Self::Mycelium,
            "note" => Self::Note,
            "poof" => Self::Poof,
            "portal" => Self::Portal,
            "rain" => Self::Rain,
            "smoke" => Self::Smoke,
            "white_smoke" => Self::WhiteSmoke,
            "sneeze" => Self::Sneeze,
            "spit" => Self::Spit,
            "squid_ink" => Self::SquidInk,
            "sweep_attack" => Self::SweepAttack,
            "totem_of_undying" => Self::TotemOfUndying,
            "underwater" => Self::Underwater,
            "splash" => Self::Splash,
            "witch" => Self::Witch,
            "bubble_pop" => Self::BubblePop,
            "current_down" => Self::CurrentDown,
            "bubble_column_up" => Self::BubbleColumnUp,
            "nautilus" => Self::Nautilus,
            "dolphin" => Self::Dolphin,
            "campfire_cosy_smoke" => Self::CampfireCosySmoke,
            "campfire_signal_smoke" => Self::CampfireSignalSmoke,
            "dripping_honey" => Self::DrippingHoney,
            "falling_honey" => Self::FallingHoney,
            "landing_honey" => Self::LandingHoney,
            "falling_nectar" => Self::FallingNectar,
            "falling_spore_blossom" => Self::FallingSporeBlossom,
            "ash" => Self::Ash,
            "crimson_spore" => Self::CrimsonSpore,
            "warped_spore" => Self::WarpedSpore,
            "spore_blossom_air" => Self::SporeBlossomAir,
            "dripping_obsidian_tear" => Self::DrippingObsidianTear,
            "falling_obsidian_tear" => Self::FallingObsidianTear,
            "landing_obsidian_tear" => Self::LandingObsidianTear,
            "reverse_portal" => Self::ReversePortal,
            "white_ash" => Self::WhiteAsh,
            "small_flame" => Self::SmallFlame,
            "snowflake" => Self::Snowflake,
            "dripping_dripstone_lava" => Self::DrippingDripstoneLava,
            "falling_dripstone_lava" => Self::FallingDripstoneLava,
            "dripping_dripstone_water" => Self::DrippingDripstoneWater,
            "falling_dripstone_water" => Self::FallingDripstoneWater,
            "glow_squid_ink" => Self::GlowSquidInk,
            "glow" => Self::Glow,
            "wax_on" => Self::WaxOn,
            "wax_off" => Self::WaxOff,
            "electric_spark" => Self::ElectricSpark,
            "scrape" => Self::Scrape,
            "shriek" => Self::Shriek,
            "egg_crack" => Self::EggCrack,
            "dust_plume" => Self::DustPlume,
            "trial_spawner_detected_player" | "trial_spawner_detection" => {
                Self::TrialSpawnerDetectedPlayer
            }
            "trial_spawner_detected_player_ominous" | "trial_spawner_detection_ominous" => {
                Self::TrialSpawnerDetectedPlayerOminous
            }
            "vault_connection" => Self::VaultConnection,
            "dust_pillar" => Self::DustPillar,
            "ominous_spawning" => Self::OminousSpawning,
            "raid_omen" => Self::RaidOmen,
            "trial_omen" => Self::TrialOmen,
            _ => return None,
        })
    }

    /// Exact unnamespaced vanilla registry name of the canonical kind.
    pub fn registry_name(self) -> &'static str {
        match self.canonical() {
            Self::AngryVillager => "angry_villager",
            Self::Block => "block",
            Self::BlockMarker => "block_marker",
            Self::Bubble => "bubble",
            Self::Cloud => "cloud",
            Self::Crit => "crit",
            Self::DamageIndicator => "damage_indicator",
            Self::DragonBreath => "dragon_breath",
            Self::DrippingLava => "dripping_lava",
            Self::FallingLava => "falling_lava",
            Self::LandingLava => "landing_lava",
            Self::DrippingWater => "dripping_water",
            Self::FallingWater => "falling_water",
            Self::Dust => "dust",
            Self::DustColorTransition => "dust_color_transition",
            Self::Effect => "effect",
            Self::ElderGuardian => "elder_guardian",
            Self::EnchantedHit => "enchanted_hit",
            Self::Enchant => "enchant",
            Self::EndRod => "end_rod",
            Self::EntityEffect => "entity_effect",
            Self::ExplosionEmitter => "explosion_emitter",
            Self::Explosion => "explosion",
            Self::Gust => "gust",
            Self::SmallGust => "small_gust",
            Self::GustEmitterLarge => "gust_emitter_large",
            Self::GustEmitterSmall => "gust_emitter_small",
            Self::SonicBoom => "sonic_boom",
            Self::FallingDust => "falling_dust",
            Self::Firework => "firework",
            Self::Fishing => "fishing",
            Self::Flame => "flame",
            Self::Infested => "infested",
            Self::CherryLeaves => "cherry_leaves",
            Self::SculkSoul => "sculk_soul",
            Self::SculkCharge => "sculk_charge",
            Self::SculkChargePop => "sculk_charge_pop",
            Self::SoulFireFlame => "soul_fire_flame",
            Self::Soul => "soul",
            Self::Flash => "flash",
            Self::HappyVillager => "happy_villager",
            Self::Composter => "composter",
            Self::Heart => "heart",
            Self::InstantEffect => "instant_effect",
            Self::Item => "item",
            Self::Vibration => "vibration",
            Self::ItemSlime => "item_slime",
            Self::ItemCobweb => "item_cobweb",
            Self::ItemSnowball => "item_snowball",
            Self::LargeSmoke => "large_smoke",
            Self::Lava => "lava",
            Self::Mycelium => "mycelium",
            Self::Note => "note",
            Self::Poof => "poof",
            Self::Portal => "portal",
            Self::Rain => "rain",
            Self::Smoke => "smoke",
            Self::WhiteSmoke => "white_smoke",
            Self::Sneeze => "sneeze",
            Self::Spit => "spit",
            Self::SquidInk => "squid_ink",
            Self::SweepAttack => "sweep_attack",
            Self::TotemOfUndying => "totem_of_undying",
            Self::Underwater => "underwater",
            Self::Splash => "splash",
            Self::Witch => "witch",
            Self::BubblePop => "bubble_pop",
            Self::CurrentDown => "current_down",
            Self::BubbleColumnUp => "bubble_column_up",
            Self::Nautilus => "nautilus",
            Self::Dolphin => "dolphin",
            Self::CampfireCosySmoke => "campfire_cosy_smoke",
            Self::CampfireSignalSmoke => "campfire_signal_smoke",
            Self::DrippingHoney => "dripping_honey",
            Self::FallingHoney => "falling_honey",
            Self::LandingHoney => "landing_honey",
            Self::FallingNectar => "falling_nectar",
            Self::FallingSporeBlossom => "falling_spore_blossom",
            Self::Ash => "ash",
            Self::CrimsonSpore => "crimson_spore",
            Self::WarpedSpore => "warped_spore",
            Self::SporeBlossomAir => "spore_blossom_air",
            Self::DrippingObsidianTear => "dripping_obsidian_tear",
            Self::FallingObsidianTear => "falling_obsidian_tear",
            Self::LandingObsidianTear => "landing_obsidian_tear",
            Self::ReversePortal => "reverse_portal",
            Self::WhiteAsh => "white_ash",
            Self::SmallFlame => "small_flame",
            Self::Snowflake => "snowflake",
            Self::DrippingDripstoneLava => "dripping_dripstone_lava",
            Self::FallingDripstoneLava => "falling_dripstone_lava",
            Self::DrippingDripstoneWater => "dripping_dripstone_water",
            Self::FallingDripstoneWater => "falling_dripstone_water",
            Self::GlowSquidInk => "glow_squid_ink",
            Self::Glow => "glow",
            Self::WaxOn => "wax_on",
            Self::WaxOff => "wax_off",
            Self::ElectricSpark => "electric_spark",
            Self::Scrape => "scrape",
            Self::Shriek => "shriek",
            Self::EggCrack => "egg_crack",
            Self::DustPlume => "dust_plume",
            Self::TrialSpawnerDetectedPlayer => "trial_spawner_detected_player",
            Self::TrialSpawnerDetectedPlayerOminous => "trial_spawner_detected_player_ominous",
            Self::VaultConnection => "vault_connection",
            Self::DustPillar => "dust_pillar",
            Self::OminousSpawning => "ominous_spawning",
            Self::RaidOmen => "raid_omen",
            Self::TrialOmen => "trial_omen",
            Self::BlockBreak
            | Self::BlockDig
            | Self::WaterSplash
            | Self::RainDrop
            | Self::SnowFlake
            | Self::LightningFlash
            | Self::PotionSwirl => unreachable!("legacy particle kinds canonicalize"),
        }
    }

    /// Protocol-767 registry id. Legacy logical kinds report the id of their
    /// canonical vanilla equivalent.
    pub fn protocol_id(self) -> u8 {
        let canonical = self.canonical();
        PROTOCOL767_PARTICLE_KINDS
            .iter()
            .position(|kind| *kind == canonical)
            .expect("every canonical particle kind is in the protocol table") as u8
    }

    /// Static visual and physical specification for this particle.
    pub fn spec(self) -> VanillaParticleSpec {
        VanillaParticleSpec::for_kind(self)
    }

    /// Linear drag in inverse seconds and upward-positive gravity in blocks/s².
    fn physics(self) -> (f64, f64) {
        let spec = self.spec();
        (spec.drag_per_sec, spec.gravity_per_sec2)
    }

    /// Billboard half-size in blocks at age zero, prior to lifetime scaling.
    fn radius(self) -> f32 {
        self.spec().base_radius
    }
}

/// Optional typed payload carried by data-driven vanilla particles.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum VanillaParticlePayload {
    None,
    BlockState(u32),
    ItemStack(u32),
    Dust { rgb: [f32; 3], scale: f32 },
    DustColorTransition { from_rgb: [f32; 3], to_rgb: [f32; 3], scale: f32 },
    Vibration { origin: [f32; 3], destination: [f32; 3], travel_ticks: u16 },
    EntityEffect { rgba: [f32; 4] },
    SculkCharge { roll: f32 },
    Shriek { delay_ticks: u32 },
}

impl VanillaParticlePayload {
    fn kind(self) -> VanillaParticlePayloadKind {
        match self {
            Self::None => VanillaParticlePayloadKind::None,
            Self::BlockState(_) => VanillaParticlePayloadKind::BlockState,
            Self::ItemStack(_) => VanillaParticlePayloadKind::ItemStack,
            Self::Dust { .. } => VanillaParticlePayloadKind::Dust,
            Self::DustColorTransition { .. } => VanillaParticlePayloadKind::DustColorTransition,
            Self::Vibration { .. } => VanillaParticlePayloadKind::Vibration,
            Self::EntityEffect { .. } => VanillaParticlePayloadKind::EntityEffect,
            Self::SculkCharge { .. } => VanillaParticlePayloadKind::SculkCharge,
            Self::Shriek { .. } => VanillaParticlePayloadKind::Shriek,
        }
    }

    fn validate(self) -> bool {
        match self {
            Self::None | Self::BlockState(_) | Self::ItemStack(_) => true,
            Self::Dust { rgb, scale } => {
                rgb.iter().all(|v| v.is_finite()) && scale.is_finite() && scale > 0.0
            }
            Self::DustColorTransition { from_rgb, to_rgb, scale } => {
                from_rgb.iter().chain(to_rgb.iter()).all(|v| v.is_finite())
                    && scale.is_finite()
                    && scale > 0.0
            }
            Self::Vibration { origin, destination, travel_ticks } => {
                travel_ticks > 0
                    && origin.iter().chain(destination.iter()).all(|v| v.is_finite())
            }
            Self::EntityEffect { rgba } => rgba.iter().all(|v| v.is_finite()),
            Self::SculkCharge { roll } => roll.is_finite(),
            Self::Shriek { delay_ticks } => delay_ticks <= 0x7fff_ffff,
        }
    }
}

fn default_particle_payload(
    kind: VanillaParticleKind,
    color: [f32; 4],
    pos: [f32; 3],
) -> VanillaParticlePayload {
    match kind.spec().payload_kind {
        VanillaParticlePayloadKind::None => VanillaParticlePayload::None,
        VanillaParticlePayloadKind::ItemStack => VanillaParticlePayload::ItemStack(0),
        VanillaParticlePayloadKind::BlockState => VanillaParticlePayload::BlockState(0),
        VanillaParticlePayloadKind::Dust => VanillaParticlePayload::Dust {
            rgb: [color[0], color[1], color[2]],
            scale: 1.0,
        },
        VanillaParticlePayloadKind::DustColorTransition => VanillaParticlePayload::DustColorTransition {
            from_rgb: [0.2, 0.9, 1.0],
            to_rgb: [0.8, 0.2, 1.0],
            scale: 1.0,
        },
        VanillaParticlePayloadKind::Vibration => VanillaParticlePayload::Vibration {
            origin: pos,
            destination: pos,
            travel_ticks: 20,
        },
        VanillaParticlePayloadKind::EntityEffect => {
            VanillaParticlePayload::EntityEffect { rgba: color }
        }
        VanillaParticlePayloadKind::SculkCharge => VanillaParticlePayload::SculkCharge { roll: 0.0 },
        VanillaParticlePayloadKind::Shriek => VanillaParticlePayload::Shriek { delay_ticks: 0 },
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
    pub payload: VanillaParticlePayload,
    origin: [f64; 3],
    initial_velocity: [f64; 3],
    elapsed: f64,
    size_scale: f32,
    motion_phase: f32,
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
    fn start_delay_secs(&self) -> f64 {
        match self.payload {
            VanillaParticlePayload::Shriek { delay_ticks } => f64::from(delay_ticks) / 20.0,
            _ => 0.0,
        }
    }

    /// In-plane rotation for the CPU and instanced billboard paths.
    pub fn billboard_roll_rad(&self) -> f32 {
        match self.payload {
            VanillaParticlePayload::SculkCharge { roll } => roll,
            _ => 0.0,
        }
    }

    /// Lifetime progress used by animated sprites, tint and scale.
    pub fn normalized_age(&self) -> f32 {
        if self.max_age_secs > 0.0 {
            ber_unit(self.age_secs / self.max_age_secs)
        } else {
            1.0
        }
    }

    /// Age-scaled billboard half-size in blocks — the identical factor
    /// [`VanillaParticleWorld::billboard_quads`] multiplies the camera axes by.
    /// GPU-instanced paths pack this per particle instead of four expanded
    /// corners.
    pub fn billboard_half_size(&self) -> f32 {
        if self.elapsed < self.start_delay_secs() {
            return 0.0;
        }
        self.kind.radius()
            * self.size_scale
            * self.kind.spec().size_curve.scale(self.normalized_age())
    }

    /// Color at the current age, including `dust_color_transition`'s vanilla
    /// source-to-target interpolation.
    pub fn color_at_age(&self) -> [f32; 4] {
        let t = self.normalized_age();
        match self.payload {
            VanillaParticlePayload::DustColorTransition {
                from_rgb, to_rgb, ..
            } => [
                from_rgb[0] + (to_rgb[0] - from_rgb[0]) * t,
                from_rgb[1] + (to_rgb[1] - from_rgb[1]) * t,
                from_rgb[2] + (to_rgb[2] - from_rgb[2]) * t,
                self.color[3],
            ],
            VanillaParticlePayload::Dust { rgb, .. } => [rgb[0], rgb[1], rgb[2], self.color[3]],
            VanillaParticlePayload::EntityEffect { rgba } => rgba,
            _ => self.color,
        }
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
        self.spawn_with_payload(kind, pos, vel, color, max_age_secs, VanillaParticlePayload::None)
    }

    /// Add a particle with an explicit protocol payload. A `None` payload for
    /// a data-driven type is replaced by the type's safe default so the legacy
    /// spawn API remains source-compatible.
    pub fn spawn_with_payload(
        &mut self,
        kind: VanillaParticleKind,
        pos: [f32; 3],
        vel: [f32; 3],
        color: [f32; 4],
        max_age_secs: f32,
        payload: VanillaParticlePayload,
    ) -> bool {
        if !pos.iter().chain(vel.iter()).all(|v| v.is_finite())
            || !max_age_secs.is_finite()
            || max_age_secs <= 0.0
            || !payload.validate()
        {
            return false;
        }
        let color = color.map(ber_unit);
        let required = kind.spec().payload_kind;
        let payload = if payload.kind() == required {
            payload
        } else if matches!(payload, VanillaParticlePayload::None) {
            default_particle_payload(kind, color, pos)
        } else {
            return false;
        };
        let (pos, vel, max_age_secs) = match payload {
            VanillaParticlePayload::Vibration {
                origin,
                destination,
                travel_ticks,
            } => {
                let seconds = f32::from(travel_ticks) / 20.0;
                let vel = std::array::from_fn(|axis| {
                    ((f64::from(destination[axis]) - f64::from(origin[axis])) / f64::from(seconds))
                        as f32
                });
                if !vel.iter().all(|value| value.is_finite()) {
                    return false;
                }
                (origin, vel, seconds)
            }
            _ => (pos, vel, max_age_secs),
        };
        let size_scale = match payload {
            VanillaParticlePayload::Dust { scale, .. }
            | VanillaParticlePayload::DustColorTransition { scale, .. } => scale.clamp(0.01, 4.0),
            _ => 1.0,
        };
        if self.particles.len() == MAX_VANILLA_PARTICLES {
            self.particles.pop_front();
        }
        let motion_seed = pos[0].to_bits() as u64
            ^ ((pos[1].to_bits() as u64) << 21)
            ^ ((pos[2].to_bits() as u64) << 42)
            ^ u64::from(kind.protocol_id());
        self.particles.push_back(VanillaParticle {
            kind,
            pos,
            vel,
            color,
            age_secs: 0.0,
            max_age_secs,
            payload,
            origin: pos.map(f64::from),
            initial_velocity: vel.map(f64::from),
            elapsed: 0.0,
            size_scale,
            motion_phase: particle_hash01(motion_seed) * std::f32::consts::TAU,
        });
        true
    }

    /// Spawn a protocol particle with its authentic vanilla lifetime sampled
    /// deterministically from `seed` inside the kind's 1.21.1 lifetime range.
    pub fn spawn_protocol(
        &mut self,
        protocol_id: u8,
        pos: [f32; 3],
        vel: [f32; 3],
        seed: u64,
        payload: VanillaParticlePayload,
    ) -> bool {
        let Some(kind) = VanillaParticleKind::from_protocol_id(protocol_id) else {
            return false;
        };
        let spec = kind.spec();
        let spread = spec.max_lifetime_ticks.saturating_sub(spec.min_lifetime_ticks);
        let jitter = if spread == 0 {
            0
        } else {
            (particle_hash01(seed ^ u64::from(protocol_id)) * f32::from(spread)).round() as u16
        };
        let lifetime = f32::from(spec.min_lifetime_ticks + jitter) / 20.0;
        let color = match payload {
            VanillaParticlePayload::Dust { rgb, .. } => [rgb[0], rgb[1], rgb[2], 1.0],
            VanillaParticlePayload::DustColorTransition { from_rgb, .. } => {
                [from_rgb[0], from_rgb[1], from_rgb[2], 1.0]
            }
            VanillaParticlePayload::EntityEffect { rgba } => rgba,
            _ => spec.tint_rgba,
        };
        self.spawn_with_payload(kind, pos, vel, color, lifetime, payload)
    }

    /// Advance analytically under `dv/dt = -drag*v + gravity`, using the
    /// exact exponential for drag. Invalid, negative or zero dt is a no-op.
    /// Swirl/flutter/shockwave overlays are likewise closed-form functions of
    /// elapsed time, so 30 FPS and 120 FPS have the same trajectory for the
    /// same elapsed time (within float rounding). Expired and numerically
    /// overflowed particles are removed.
    pub fn step(&mut self, dt_secs: f32) {
        if !dt_secs.is_finite() || dt_secs <= 0.0 {
            return;
        }
        let dt = f64::from(dt_secs);
        self.particles.retain_mut(|particle| {
            particle.elapsed += dt;
            let t = particle.elapsed - particle.start_delay_secs();
            if t < 0.0 {
                return true;
            }
            let duration = f64::from(particle.max_age_secs);
            if t >= duration {
                return false;
            }
            particle.age_secs = t as f32;
            if let VanillaParticlePayload::Vibration { destination, .. } = particle.payload {
                let progress = t / duration;
                particle.pos = std::array::from_fn(|axis| {
                    let from = particle.origin[axis];
                    let to = f64::from(destination[axis]);
                    (from + (to - from) * progress) as f32
                });
                return true;
            }
            let (drag, gravity) = particle.kind.physics();
            let (velocity_scale, distance_scale, gravity_displacement) = if drag > 0.0 {
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
            apply_vanilla_motion_overlay(particle, t as f32);
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
        self.billboard_quads_with_uvs(camera_right, camera_up, |_, _| [0.0, 0.0, 1.0, 1.0])
    }

    /// Expand quads with each particle's age-selected atlas rectangle in
    /// `[u_min, v_min, u_max, v_max]` order.
    pub fn billboard_quads_with_uvs(
        &self,
        camera_right: [f32; 3],
        camera_up: [f32; 3],
        uv_rect: impl Fn(u16, f32) -> [f32; 4],
    ) -> Vec<VanillaParticleQuad> {
        let mut quads = Vec::with_capacity(self.particles.len());
        self.append_billboard_quads_with_uvs(camera_right, camera_up, uv_rect, &mut quads);
        quads
    }

    /// Append textured billboards to a caller's reusable staging buffer.
    pub fn append_billboard_quads_with_uvs(
        &self,
        camera_right: [f32; 3],
        camera_up: [f32; 3],
        uv_rect: impl Fn(u16, f32) -> [f32; 4],
        quads: &mut Vec<VanillaParticleQuad>,
    ) {
        let (right, up) = Self::billboard_axes(camera_right, camera_up);
        for particle in &self.particles {
            let size = particle.billboard_half_size();
            if size <= 0.0 {
                continue;
            }
            let (sin, cos) = particle.billboard_roll_rad().sin_cos();
            let corners = [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)];
            let positions = corners.map(|(x, y)| {
                let u = x * cos - y * sin;
                let v = x * sin + y * cos;
                [
                    particle.pos[0] + size * (u * right[0] + v * up[0]),
                    particle.pos[1] + size * (u * right[1] + v * up[1]),
                    particle.pos[2] + size * (u * right[2] + v * up[2]),
                ]
            });
            if positions.iter().flatten().all(|value| value.is_finite()) {
                let [u0, v0, u1, v1] = uv_rect(
                    particle.kind.protocol_id().into(),
                    particle.normalized_age(),
                );
                quads.push(VanillaParticleQuad {
                    kind: particle.kind,
                    positions,
                    uv: [[u0, v1], [u1, v1], [u1, v0], [u0, v0]],
                    color: particle.color_at_age(),
                });
            }
        }
    }
}

fn apply_vanilla_motion_overlay(particle: &mut VanillaParticle, t: f32) {
    let spec = particle.kind.spec();
    let phase = particle.motion_phase;
    match spec.motion {
        VanillaParticleMotion::Linear | VanillaParticleMotion::Buoyant => {}
        VanillaParticleMotion::Static => {
            particle.pos = particle.origin.map(|v| v as f32);
            particle.vel = [0.0; 3];
        }
        VanillaParticleMotion::Flutter => {
            let omega = 2.1 + 1.1 * (phase / std::f32::consts::TAU);
            let amplitude = 0.045 + spec.base_radius * 0.35;
            let x = amplitude * (omega * t + phase).sin();
            let z = amplitude * (omega * t * 0.83 + phase * 1.37).cos();
            particle.pos[0] += x;
            particle.pos[2] += z;
            particle.vel[0] += amplitude * omega * (omega * t + phase).cos();
            particle.vel[2] -= amplitude * omega * 0.83 * (omega * t * 0.83 + phase * 1.37).sin();
        }
        VanillaParticleMotion::Swirl => {
            let omega = 3.2 + 1.4 * (phase / std::f32::consts::TAU);
            let radius = spec.base_radius * (0.35 + 0.65 * (1.0 + (t * 0.7).sin()) * 0.5);
            let angle = omega * t + phase;
            let x = radius * angle.cos();
            let z = radius * angle.sin();
            particle.pos[0] += x;
            particle.pos[2] += z;
            particle.vel[0] += -radius * omega * angle.sin();
            particle.vel[2] += radius * omega * angle.cos();
        }
        VanillaParticleMotion::Shockwave => {
            let expansion = 1.0 + 0.85 * t;
            let vel = particle.initial_velocity.map(|v| v as f32);
            for (axis, &v) in vel.iter().enumerate() {
                particle.pos[axis] = particle.origin[axis] as f32 + v * t * expansion;
                particle.vel[axis] = v * (1.0 + 1.7 * t);
            }
        }
        VanillaParticleMotion::Emitter => {
            let bob = 0.025 * (t * 2.4 + phase).sin();
            particle.pos = particle.origin.map(|v| v as f32);
            particle.pos[1] += bob;
            particle.vel = [0.0, 0.06 * (t * 2.4 + phase).cos(), 0.0];
        }
    }
}

fn particle_hash01(mut x: u64) -> f32 {
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^= x >> 31;
    ((x >> 40) as f32) / 16_777_215.0
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
