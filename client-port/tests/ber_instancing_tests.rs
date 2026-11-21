// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//
// Generic block-entity instancing proofs: a 2,000-unit factory wall must
// collapse to a handful of instanced draws, and every cull layer must be
// deterministic and allocation-free in steady state. Every entity below is
// described only by motion/interior/model keys — nothing in the pipeline
// knows or cares which block-entity type produced it.

use mc_spine_client_mesh::ber_instancing::*;

const ATLAS: BerAtlasKey = 0;
const SHELL_MODEL_A: BerModelKey = 10;
const SHELL_MODEL_B: BerModelKey = 11;
const ITEM_MODEL: BerModelKey = 20;

/// Six planes forming a huge box around the origin: everything placed near
/// the test coordinates is inside the frustum.
const HUGE_FRUSTUM: [[f32; 4]; 6] = [
    [1.0, 0.0, 0.0, 10_000.0],
    [-1.0, 0.0, 0.0, 10_000.0],
    [0.0, 1.0, 0.0, 10_000.0],
    [0.0, -1.0, 0.0, 10_000.0],
    [0.0, 0.0, 1.0, 10_000.0],
    [0.0, 0.0, -1.0, 10_000.0],
];

static ONE_ITEM: [BerInteriorItem; 1] = [BerInteriorItem {
    offset: [0.5, 0.4, 0.5],
    scale: 0.5,
    rot_uv_frame: [0.0, 0.0, 0.0, 0.0],
    model_id: ITEM_MODEL,
    atlas_id: ATLAS,
    packed_light: 0x00f0,
    tint_rgba: 0xffff_ffff,
}];

fn shell(model_id: BerModelKey) -> BerShellModel {
    BerShellModel {
        model_id,
        atlas_id: ATLAS,
        scale: 1.0,
        rot_uv_frame: [0.25, 0.0, 0.0, 3.0],
        packed_light: 0x00f0,
        tint_rgba: 0xffff_ffff,
    }
}

/// A block entity at `[0.5, 64.5, z + 0.5]`-ish distance `z + 0.5` from the
/// camera at the origin-half step below (centers land at `z + 0.5`).
fn entity(
    id: u64,
    z: f32,
    motion: BerMotionState,
    has_interior: bool,
    interior_visible: bool,
    items: &'static [BerInteriorItem],
    shell_model: BerModelKey,
) -> BerEntityInput<'static> {
    BerEntityInput {
        entity_id: id,
        pos: [0.0, 64.0, z],
        extents: [1.0; 3],
        motion_state: motion,
        has_dynamic_interior: has_interior,
        interior_visible,
        surrounded_opaque: false,
        shell: shell(shell_model),
        interior_items: items,
    }
}

fn view() -> BerFrameView<'static> {
    BerFrameView {
        camera_pos: [0.0, 64.5, 0.0],
        frustum_planes: &HUGE_FRUSTUM,
    }
}

/// The massive-factory proof: 1,200 sealed-interior units, 300 idle units,
/// and 500 live units across the three distance tiers must collapse from
/// ~2,500 naive draws into <= 3 instanced draws.
#[test]
fn ber_instancing_massive_2000_chute_factory_collapses_draw_calls_and_culls_interiors() {
    let mut inputs = Vec::with_capacity(2000);
    let mut next_id = 0u64;

    // 1,200 sealed units: live contents but no window — interior draws must
    // be suppressed 100%, and their static shells live in the chunk mesh.
    for _ in 0..1200 {
        inputs.push(entity(
            next_id,
            8.0,
            BerMotionState::Static,
            true,
            false,
            &ONE_ITEM,
            SHELL_MODEL_A,
        ));
        next_id += 1;
    }

    // 300 idle units: no motion, no live interior — promoted to the static
    // chunk mesh entirely.
    for _ in 0..300 {
        inputs.push(entity(
            next_id,
            8.0,
            BerMotionState::Static,
            false,
            false,
            &[],
            SHELL_MODEL_A,
        ));
        next_id += 1;
    }

    // 500 live units with moving shells and visible interior items:
    //   200 near  (z=10   -> dist 10.5  <= 24): shell + interior item
    //   200 mid   (z=40   -> dist 40.5  <= 64): shell only
    //   100 far   (z=100  -> dist 100.5 >  64): fully dropped
    for (z, count) in [(10.0f32, 200usize), (40.0, 200), (100.0, 100)] {
        for i in 0..count {
            let model = if i % 2 == 0 {
                SHELL_MODEL_A
            } else {
                SHELL_MODEL_B
            };
            inputs.push(entity(
                next_id,
                z,
                BerMotionState::Moving,
                true,
                true,
                &ONE_ITEM,
                model,
            ));
            next_id += 1;
        }
    }
    assert_eq!(inputs.len(), 2000);

    let mut batcher = BlockEntityInstanceBatcher::new(BerLodPolicy::new());
    let mut frame = BerFrameBatches::default();
    let stats = batcher.build_batches_into(&inputs, &view(), &mut frame);

    // Exact cull accounting.
    assert_eq!(stats.total_inputs, 2000);
    assert_eq!(stats.culled_static_idle, 300);
    assert_eq!(stats.culled_frustum_or_occluded, 0);
    assert_eq!(stats.culled_interior_opaque, 1200);
    // 100 far entities fully dropped + 200 mid-tier interior draws dropped.
    assert_eq!(stats.culled_distance_lod, 300);
    // Near: shell + item = 400. Mid: shell only = 200.
    assert_eq!(stats.visible_instances, 600);
    assert_eq!(stats.instanced_draw_calls, 3);
    assert_eq!(frame.batches.len(), 3);
    assert_eq!(frame.instances.len(), 600);

    // One instanced draw per unique (model, atlas): three model keys.
    let counts: Vec<(BerModelKey, u32)> = frame
        .batches
        .iter()
        .map(|b| (b.model_id, b.instance_count))
        .collect();
    assert_eq!(
        counts,
        vec![(SHELL_MODEL_A, 200), (SHELL_MODEL_B, 200), (ITEM_MODEL, 200)]
    );

    // Runs partition the packed stream contiguously and deterministically.
    let mut cursor = 0u32;
    for batch in &frame.batches {
        assert_eq!(batch.first_instance, cursor);
        cursor += batch.instance_count;
        let run = &frame.instances
            [batch.first_instance as usize..(batch.first_instance + batch.instance_count) as usize];
        assert_eq!(run.len(), batch.instance_count as usize);
    }
    assert_eq!(cursor as usize, frame.instances.len());

    // Every interior-item instance comes from the near band only.
    let item_batch = &frame.batches[2];
    let item_run = &frame.instances[item_batch.first_instance as usize
        ..(item_batch.first_instance + item_batch.instance_count) as usize];
    assert!(item_run.iter().all(|r| r.pos_scale[2] < 24.0));

    // Naive per-entity + per-item rendering vs instanced submission.
    let naive_draws = 2000u32 + 500; // every entity's shell + live interiors
    let naive_full = naive_draws + 1200; // counting sealed interiors too
    let reduction = naive_draws as f64 / stats.instanced_draw_calls as f64;
    let reduction_full = naive_full as f64 / stats.instanced_draw_calls as f64;
    println!(
        "2,000-unit factory: naive {naive_draws} draws ({naive_full} counting sealed \
         interiors) -> {} instanced draws = {reduction:.1}x ({reduction_full:.1}x) reduction; \
         culled: static={}, occluded={}, interior={}, distance={}",
        stats.instanced_draw_calls,
        stats.culled_static_idle,
        stats.culled_frustum_or_occluded,
        stats.culled_interior_opaque,
        stats.culled_distance_lod,
    );
    assert!(stats.instanced_draw_calls <= 3);
    assert!(reduction >= 800.0, "reduction {reduction}x");

    // A second identical build is byte-identical.
    let stats2 = batcher.build_batches_into(&inputs, &view(), &mut frame);
    assert_eq!(stats, stats2);
}

/// Repeated frame rebuilds over the same working set must trigger zero
/// buffer growth after warmup.
#[test]
fn ber_instancing_steady_state_frame_rebuild_has_zero_growth_events() {
    let mut inputs = Vec::new();
    for id in 0..400u64 {
        let z = match id % 4 {
            0 => 8.0,
            1 => 40.0,
            2 => 12.0,
            _ => 80.0,
        };
        inputs.push(entity(
            id,
            z,
            if id % 3 == 0 {
                BerMotionState::Moving
            } else {
                BerMotionState::Static
            },
            id % 5 == 0,
            id % 5 == 0,
            if id % 5 == 0 { &ONE_ITEM } else { &[] },
            if id % 2 == 0 {
                SHELL_MODEL_A
            } else {
                SHELL_MODEL_B
            },
        ));
    }

    let mut batcher = BlockEntityInstanceBatcher::new(BerLodPolicy::new());
    let mut frame = BerFrameBatches::default();
    let view = view();

    // Warmup: buffers reach working capacity.
    let first = batcher.build_batches_into(&inputs, &view, &mut frame);
    let warmed = batcher.growth_events();

    // Ten steady-state frames: no growth, identical stats.
    for frame_i in 0..10 {
        let stats = batcher.build_batches_into(&inputs, &view, &mut frame);
        assert_eq!(
            batcher.growth_events(),
            warmed,
            "frame {frame_i} reallocated"
        );
        assert_eq!(stats, first);
    }
    println!("steady state: growth_events={warmed} after warmup, flat across 10 frames");
}

/// Adaptive scale + low_spec preset shed distance smoothly; garbage inputs
/// fail closed.
#[test]
fn ber_instancing_low_spec_and_adaptive_scale_shed_distance_smoothly_and_scrub_nan() {
    // Policy scrubbing.
    let mut policy = BerLodPolicy::new();
    assert_eq!(policy.near_interior_dist(), 24.0);
    assert_eq!(policy.max_dynamic_dist(), 64.0);
    assert_eq!(policy.effective_near_interior_dist(), 24.0);
    assert_eq!(policy.effective_max_dynamic_dist(), 64.0);

    policy.set_lod_scale(0.5);
    assert_eq!(policy.effective_near_interior_dist(), 12.0);
    assert_eq!(policy.effective_max_dynamic_dist(), 32.0);

    for (bad, want) in [
        (f32::NAN, LOD_SCALE_MIN),
        (f32::NEG_INFINITY, LOD_SCALE_MIN),
        (-0.5, LOD_SCALE_MIN),
        (0.0, LOD_SCALE_MIN),
        (f32::INFINITY, LOD_SCALE_MAX),
        (0.7, 0.7),
    ] {
        policy.set_lod_scale(bad);
        assert_eq!(policy.lod_scale(), want, "lod_scale {bad}");
    }

    let low = BerLodPolicy::low_spec();
    assert_eq!(low.near_interior_dist(), 12.0);
    assert_eq!(low.max_dynamic_dist(), 32.0);
    let scrubbed = BerLodPolicy::with_distances(f32::NAN, -10.0);
    assert_eq!(scrubbed.near_interior_dist(), 0.0);
    assert_eq!(scrubbed.max_dynamic_dist(), 0.0);

    // Live unit at dist 18.5: interior visible at full scale, suppressed at
    // 0.5 scale and under low_spec.
    let inputs = [entity(
        0,
        18.0,
        BerMotionState::Moving,
        true,
        true,
        &ONE_ITEM,
        SHELL_MODEL_A,
    )];
    let view = view();
    let mut frame = BerFrameBatches::default();

    let mut full = BlockEntityInstanceBatcher::new(BerLodPolicy::new());
    let stats = full.build_batches_into(&inputs, &view, &mut frame);
    assert_eq!(stats.visible_instances, 2); // shell + item
    assert_eq!(stats.culled_distance_lod, 0);

    let mut half = BlockEntityInstanceBatcher::new(BerLodPolicy::new());
    half.set_lod_scale(0.5); // effective 12 / 32
    let stats = half.build_batches_into(&inputs, &view, &mut frame);
    assert_eq!(stats.visible_instances, 1); // shell only
    assert_eq!(stats.culled_distance_lod, 1); // interior suppressed

    let mut low = BlockEntityInstanceBatcher::new(BerLodPolicy::low_spec());
    let stats = low.build_batches_into(&inputs, &view, &mut frame);
    assert_eq!(stats.visible_instances, 1);
    assert_eq!(stats.culled_distance_lod, 1);

    // A unit at dist 40.5 survives default, is fully dropped by low_spec's
    // 32-block dynamic tier.
    let mid = [entity(
        1,
        40.0,
        BerMotionState::Moving,
        true,
        true,
        &ONE_ITEM,
        SHELL_MODEL_A,
    )];
    let stats = full.build_batches_into(&mid, &view, &mut frame);
    assert_eq!(stats.visible_instances, 1);
    let stats = low.build_batches_into(&mid, &view, &mut frame);
    assert_eq!(stats.visible_instances, 0);
    assert_eq!(stats.culled_distance_lod, 1);

    // Malformed inputs fail closed into the occluded bucket: NaN position,
    // -inf position, negative extents, NaN extents.
    let mut bad = entity(9, 8.0, BerMotionState::Moving, false, false, &[], SHELL_MODEL_A);
    bad.pos = [f32::NAN, 0.0, 0.0];
    let mut bad2 = entity(10, 8.0, BerMotionState::Moving, false, false, &[], SHELL_MODEL_A);
    bad2.pos = [0.0, f32::NEG_INFINITY, 0.0];
    let mut bad3 = entity(11, 8.0, BerMotionState::Moving, false, false, &[], SHELL_MODEL_A);
    bad3.extents = [1.0, -1.0, 1.0];
    let mut bad4 = entity(12, 8.0, BerMotionState::Moving, false, false, &[], SHELL_MODEL_A);
    bad4.extents = [f32::NAN; 3];
    let bads = [bad, bad2, bad3, bad4];
    let stats = full.build_batches_into(&bads, &view, &mut frame);
    assert_eq!(stats.visible_instances, 0);
    assert_eq!(stats.culled_frustum_or_occluded, 4);
    assert!(frame.instances.is_empty());
    assert!(frame.batches.is_empty());
}

/// Behind-camera and fully wall-enclosed entities emit zero draws,
/// deterministically.
#[test]
fn ber_instancing_frustum_and_surrounded_opaque_cull_deterministically() {
    // Inside half-space is -z >= 0 (z <= 0): the entity at z=10 is behind.
    let tight_planes: [[f32; 4]; 1] = [[0.0, 0.0, -1.0, 0.0]];
    let behind_view = BerFrameView {
        camera_pos: [0.0, 64.5, -100.0],
        frustum_planes: &tight_planes,
    };
    // Same entity inside the huge frustum but with every neighbor opaque.
    let open_view = view();

    let behind = entity(
        0,
        10.0,
        BerMotionState::Moving,
        true,
        true,
        &ONE_ITEM,
        SHELL_MODEL_A,
    );
    let mut enclosed = entity(
        1,
        10.0,
        BerMotionState::Moving,
        true,
        true,
        &ONE_ITEM,
        SHELL_MODEL_A,
    );
    enclosed.surrounded_opaque = true;
    let visible = entity(
        2,
        10.0,
        BerMotionState::Moving,
        true,
        true,
        &ONE_ITEM,
        SHELL_MODEL_B,
    );

    let mut batcher = BlockEntityInstanceBatcher::new(BerLodPolicy::new());
    let mut frame = BerFrameBatches::default();

    let behind_inputs = [behind.clone()];
    let stats = batcher.build_batches_into(&behind_inputs, &behind_view, &mut frame);
    assert_eq!(stats.visible_instances, 0);
    assert_eq!(stats.instanced_draw_calls, 0);
    assert_eq!(stats.culled_frustum_or_occluded, 1);

    // The enclosed entity drops even inside a permissive frustum; the open
    // one beside it survives.
    let mixed = [enclosed.clone(), visible.clone()];
    let stats = batcher.build_batches_into(&mixed, &open_view, &mut frame);
    assert_eq!(stats.culled_frustum_or_occluded, 1);
    assert_eq!(stats.visible_instances, 2);
    assert_eq!(stats.instanced_draw_calls, 2); // shell model + item model

    // Determinism: identical inputs yield identical streams.
    let mut frame2 = BerFrameBatches::default();
    let stats2 = batcher.build_batches_into(&mixed, &open_view, &mut frame2);
    assert_eq!(stats, stats2);
    assert_eq!(frame, frame2);
}
