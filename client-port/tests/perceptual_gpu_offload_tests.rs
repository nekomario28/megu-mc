// SPDX-License-Identifier: LGPL-3.0-or-later
//! `perceptual_gpu_offload`: 60° frontal-cone priority ordering,
//! urgent-edit preemption, deterministic budgeted batching, distance-scaled
//! entity sync with >= 70% far bandwidth cut, shortest-angle interpolation,
//! and 16-byte GPU instance layout / pack-unpack parity.

use std::mem::{align_of, size_of};

use megu_client_mesh::perceptual_gpu_offload::{
    lerp_angle_deg, ConeBand, DistanceEntitySyncPlanner, EntityMotionInterpolator,
    EntityPoseSnapshot, EntitySyncClass, FrontalConePriorityQueue, GpuOffloadInstancePacker,
    GpuParticleInstance, GpuWeatherColumnInstance, OffloadTask, OffloadTaskKind,
    FRONTAL_CONE_MULTIPLIER, GPU_INSTANCE_BYTES, REAR_CONE_MULTIPLIER,
};

fn task(id: u64, kind: OffloadTaskKind, pos: [f32; 3], urgent: bool) -> OffloadTask {
    OffloadTask {
        job_id: id,
        kind,
        target_pos: pos,
        is_urgent_edit: urgent,
    }
}

fn snapshot(tick: u64, pos: [f32; 3], yaw: f32, pitch: f32) -> EntityPoseSnapshot {
    EntityPoseSnapshot {
        tick,
        pos,
        yaw_deg: yaw,
        pitch_deg: pitch,
    }
}

#[test]
fn frontal_cone_tasks_outrank_rear_tasks() {
    let queue = FrontalConePriorityQueue::new([0.0, 64.0, 0.0], [0.0, 0.0, 1.0]);
    let frontal = task(1, OffloadTaskKind::ChunkMesh, [0.0, 64.0, 32.0], false);
    let rear = task(2, OffloadTaskKind::ChunkMesh, [0.0, 64.0, -32.0], false);

    let scored_front = queue.score(&frontal);
    let scored_rear = queue.score(&rear);
    assert_eq!(scored_front.band, ConeBand::Frontal);
    assert_eq!(scored_rear.band, ConeBand::Rear);
    assert_eq!(
        scored_front.score,
        scored_rear.score * FRONTAL_CONE_MULTIPLIER / REAR_CONE_MULTIPLIER
    );

    let batch = queue.select_budgeted_batch(&[rear, frontal], 8);
    assert_eq!(batch.len(), 2);
    assert_eq!(batch[0].task.job_id, 1);
    assert_eq!(batch[1].task.job_id, 2);
}

#[test]
fn cone_bands_split_at_sixty_degrees_and_horizon() {
    let queue = FrontalConePriorityQueue::new([0.0, 0.0, 0.0], [0.0, 0.0, 1.0]);
    // 20° off-axis: inside the 60° cone.
    let inside = task(1, OffloadTaskKind::Particle, [3.64, 0.0, 10.0], false);
    // 45° off-axis: peripheral (in front of camera plane, outside cone).
    let side = task(2, OffloadTaskKind::Particle, [10.0, 0.0, 10.0], false);
    // Exactly behind.
    let rear = task(3, OffloadTaskKind::Particle, [0.0, 0.0, -10.0], false);
    assert_eq!(queue.score(&inside).band, ConeBand::Frontal);
    assert_eq!(queue.score(&side).band, ConeBand::Peripheral);
    assert_eq!(queue.score(&rear).band, ConeBand::Rear);
    assert!(queue.score(&side).score > queue.score(&rear).score);
    assert!(queue.score(&inside).score > queue.score(&side).score);
    // A task at the camera position is treated as frontal.
    let at_camera = task(4, OffloadTaskKind::Particle, [0.0, 0.0, 0.0], false);
    assert_eq!(queue.score(&at_camera).band, ConeBand::Frontal);
}

#[test]
fn urgent_edit_preempts_everything() {
    let queue = FrontalConePriorityQueue::new([0.0, 64.0, 0.0], [0.0, 0.0, 1.0]);
    let frontal_mesh = task(1, OffloadTaskKind::ChunkMesh, [0.0, 64.0, 16.0], false);
    let urgent_rear_edit = task(2, OffloadTaskKind::ChunkMesh, [0.0, 64.0, -2.0], true);
    let batch = queue.select_budgeted_batch(&[frontal_mesh, urgent_rear_edit], 8);
    assert!(batch[0].task.is_urgent_edit);
    assert_eq!(batch[0].task.job_id, 2);

    // Between two urgent edits the normal ordering still applies.
    let urgent_front = task(3, OffloadTaskKind::Particle, [0.0, 64.0, 4.0], true);
    let batch = queue.select_budgeted_batch(&[urgent_rear_edit, urgent_front], 8);
    assert_eq!(batch[0].task.job_id, 3);
    assert_eq!(batch[1].task.job_id, 2);
}

#[test]
fn budgeted_batch_is_bounded_and_deterministic() {
    let queue = FrontalConePriorityQueue::new([0.0, 0.0, 0.0], [1.0, 0.0, 0.0]);
    let candidates = [
        task(10, OffloadTaskKind::ChunkMesh, [8.0, 0.0, 0.0], false),
        task(11, OffloadTaskKind::ChunkMesh, [4.0, 0.0, 0.0], false),
        task(12, OffloadTaskKind::ChunkGen, [4.0, 0.0, 0.0], false),
        task(13, OffloadTaskKind::Particle, [4.0, 0.0, 0.0], false),
        task(14, OffloadTaskKind::Particle, [-4.0, 0.0, 0.0], false),
    ];
    let first = queue.select_budgeted_batch(&candidates, 2);
    let second = queue.select_budgeted_batch(&candidates, 2);
    assert_eq!(first, second);
    assert_eq!(first.len(), 2);
    // Both frontal chunk meshes beat everything; the nearer one wins.
    assert_eq!(first[0].task.job_id, 11);
    assert_eq!(first[1].task.job_id, 10);

    // Equal band, score, and distance: job_id is the stable tiebreak.
    let ties = [
        task(9, OffloadTaskKind::Particle, [4.0, 0.0, 0.0], false),
        task(7, OffloadTaskKind::Particle, [4.0, 0.0, 0.0], false),
        task(8, OffloadTaskKind::Particle, [4.0, 0.0, 0.0], false),
    ];
    let batch = queue.select_budgeted_batch(&ties, 8);
    let ids: Vec<u64> = batch.iter().map(|s| s.task.job_id).collect();
    assert_eq!(ids, [7, 8, 9]);
}

#[test]
fn sync_planner_scales_rate_by_distance_band() {
    let planner = DistanceEntitySyncPlanner::new();
    assert_eq!(planner.sync_period_ticks(0.0, EntitySyncClass::Passive), 1);
    assert_eq!(planner.sync_period_ticks(32.0, EntitySyncClass::Passive), 1);
    assert_eq!(planner.sync_period_ticks(32.5, EntitySyncClass::Passive), 2);
    assert_eq!(planner.sync_period_ticks(64.0, EntitySyncClass::Passive), 2);
    assert_eq!(planner.sync_period_ticks(64.5, EntitySyncClass::Passive), 4);
    assert_eq!(
        planner.sync_period_ticks(500.0, EntitySyncClass::Passive),
        4
    );
}

#[test]
fn critical_entities_never_rate_scale() {
    let planner = DistanceEntitySyncPlanner::new();
    for class in [
        EntitySyncClass::Falling,
        EntitySyncClass::Combat,
        EntitySyncClass::Player,
    ] {
        assert!(class.is_critical());
        assert_eq!(planner.sync_period_ticks(200.0, class), 1);
        assert!(planner.should_sync(7, 42, 200.0, class));
    }
    assert!(!EntitySyncClass::Passive.is_critical());
}

#[test]
fn far_entities_cut_bandwidth_at_least_70_percent() {
    let planner = DistanceEntitySyncPlanner::new();
    let ticks = 80;
    let near = planner.count_syncs(0, ticks, 7, 16.0, EntitySyncClass::Passive);
    let mid = planner.count_syncs(0, ticks, 7, 48.0, EntitySyncClass::Passive);
    let far = planner.count_syncs(0, ticks, 7, 100.0, EntitySyncClass::Passive);
    assert_eq!(near, ticks);
    assert_eq!(mid, ticks / 2);
    assert_eq!(far, ticks / 4);
    let reduction = 1.0 - far as f64 / near as f64;
    assert!(
        reduction >= 0.70,
        "far bandwidth reduction {reduction} < 0.70"
    );
    assert_eq!(reduction, 0.75);
}

#[test]
fn sync_phase_stagger_spreads_entities_across_period() {
    let planner = DistanceEntitySyncPlanner::new();
    // Four entities in the far band each occupy a different phase slot.
    let first_syncs: Vec<u64> = (0u32..4)
        .map(|eid| {
            (0u64..8)
                .find(|&t| planner.should_sync(t, eid, 100.0, EntitySyncClass::Passive))
                .unwrap()
        })
        .collect();
    let mut sorted = first_syncs.clone();
    sorted.sort_unstable();
    assert_eq!(sorted, [0, 1, 2, 3]);
    // Each entity then repeats exactly every 4 ticks.
    for eid in 0u32..4 {
        let hits: Vec<u64> = (0u64..16)
            .filter(|&t| planner.should_sync(t, eid, 100.0, EntitySyncClass::Passive))
            .collect();
        assert_eq!(hits.len(), 4);
        assert_eq!(hits[1] - hits[0], 4);
    }
}

#[test]
fn interpolator_tracks_linear_motion_within_0_02_blocks() {
    let mut interp = EntityMotionInterpolator::new();
    assert!(interp.sample(0.0).is_none());

    interp.push_snapshot(snapshot(0, [10.0, 64.0, -4.0], 45.0, 0.0));
    // Single snapshot: samples that pose exactly.
    let solo = interp.sample(2.0).unwrap();
    assert_eq!(solo.pos, [10.0, 64.0, -4.0]);

    interp.push_snapshot(snapshot(4, [14.0, 64.0, -8.0], 90.0, 45.0));
    let mid = interp.sample(2.0).unwrap();
    for (got, want) in mid.pos.iter().zip([12.0_f32, 64.0, -6.0]) {
        assert!(
            (got - want).abs() < 0.02,
            "pos error {} >= 0.02",
            (got - want).abs()
        );
    }
    assert!((mid.yaw_deg - 67.5).abs() < 0.02);
    assert!((mid.pitch_deg - 22.5).abs() < 0.02);

    // Sub-tick alpha keeps tracking the same line.
    let quarter = interp.sample(3.0).unwrap();
    assert!((quarter.pos[0] - 13.0).abs() < 0.02);
    assert!((quarter.pos[2] - -7.0).abs() < 0.02);

    // Clamped outside the window: no extrapolation.
    assert_eq!(interp.sample(-5.0).unwrap().pos, [10.0, 64.0, -4.0]);
    assert_eq!(interp.sample(40.0).unwrap().pos, [14.0, 64.0, -8.0]);
}

#[test]
fn yaw_interpolation_wraps_shortest_angle_across_180() {
    // 170° -> -170° is a 20° hop through ±180, not a 340° swing through 0.
    assert_eq!(lerp_angle_deg(170.0, -170.0, 0.0), 170.0);
    let mid = lerp_angle_deg(170.0, -170.0, 0.5);
    assert!((mid + 180.0).abs() < 1.0e-4 || (mid - 180.0).abs() < 1.0e-4);
    assert_eq!(lerp_angle_deg(170.0, -170.0, 1.0), -170.0);
    // The midpoint must pass through the seam, never near 0.
    assert!(mid.abs() > 170.0);

    let mut interp = EntityMotionInterpolator::new();
    interp.push_snapshot(snapshot(0, [0.0, 0.0, 0.0], 170.0, 0.0));
    interp.push_snapshot(snapshot(4, [0.0, 0.0, 0.0], -170.0, 0.0));
    let mid = interp.sample(2.0).unwrap();
    assert!(
        mid.yaw_deg.abs() > 170.0,
        "yaw took the long way: {}",
        mid.yaw_deg
    );
    let end = interp.sample(4.0).unwrap();
    assert!((end.yaw_deg - -170.0).abs() < 1.0e-4);
}

#[test]
fn gpu_instance_layout_is_16_bytes_align4() {
    assert_eq!(size_of::<GpuParticleInstance>(), GPU_INSTANCE_BYTES);
    assert_eq!(size_of::<GpuParticleInstance>(), 16);
    assert_eq!(align_of::<GpuParticleInstance>(), 4);
    assert_eq!(size_of::<GpuWeatherColumnInstance>(), GPU_INSTANCE_BYTES);
    assert_eq!(align_of::<GpuWeatherColumnInstance>(), 4);
    assert_eq!(GpuParticleInstance::SIZE_BYTES, 16);
    assert_eq!(GpuWeatherColumnInstance::SIZE_BYTES, 16);
}

#[test]
fn particle_instance_pack_unpack_parity() {
    let pos = [1.5, -2.25, 3.75];
    let vel = [0.5, -0.25, 1.0];
    let tint = [10, 20, 30, 40];
    let inst = GpuParticleInstance::pack(pos, vel, tint, 200, 77, 0b1010_0101);

    let got_pos = inst.position();
    for (got, want) in got_pos.iter().zip(pos) {
        assert!((got - want).abs() <= 1.0 / 256.0, "pos {got} != {want}");
    }
    let got_vel = inst.velocity();
    for (got, want) in got_vel.iter().zip(vel) {
        assert!((got - want).abs() <= 1.0 / 16.0, "vel {got} != {want}");
    }
    assert_eq!(inst.tint(), tint);
    assert_eq!(inst.fade_alpha(), 200);
    assert_eq!(inst.phase(), 77);
    assert_eq!(inst.flags(), 0b1010_0101);

    // Words are exactly the four struct fields.
    let words = inst.as_words();
    assert_eq!(words[0], inst.pos_xy);
    assert_eq!(words[3], inst.tint_rgba);

    // Out-of-range inputs clamp into the representable domain.
    let clamped = GpuParticleInstance::pack([500.0, -500.0, 0.0], [100.0, 0.0, 0.0], tint, 0, 0, 0);
    assert!((clamped.position()[0] - 127.996).abs() < 0.01);
    assert!((clamped.position()[1] - -128.0).abs() < 0.01);
    assert_eq!(clamped.velocity()[0], 7.9375);
}

#[test]
fn weather_column_pack_unpack_parity() {
    let tint = [180, 200, 255, 128];
    let inst = GpuWeatherColumnInstance::pack([120, -300], -60, 400, 255, 9, -3.5, 1, tint);
    assert_eq!(inst.column_xz(), [120, -300]);
    assert_eq!(inst.base_y(), -60);
    assert_eq!(inst.height(), 400);
    assert_eq!(inst.intensity(), 255);
    assert_eq!(inst.phase(), 9);
    assert_eq!(inst.fall_speed(), -3.5);
    assert_eq!(inst.kind(), 1);
    assert_eq!(inst.tint(), tint);
    let words = inst.as_words();
    assert_eq!(words[0], inst.column_xz);
    assert_eq!(words[3], inst.tint_rgba);
}

#[test]
fn packer_uploads_only_dirty_instance_slices() {
    let mut packer = GpuOffloadInstancePacker::new();
    assert!(packer.dirty_particles().is_empty());
    assert!(packer.dirty_weather_columns().is_empty());
    assert!(packer.dirty_particle_words().is_empty());

    let a = GpuParticleInstance::pack([1.0, 0.0, 0.0], [0.0; 3], [255; 4], 255, 0, 0);
    let b = GpuParticleInstance::pack([2.0, 0.0, 0.0], [0.0; 3], [255; 4], 255, 1, 0);
    let w = GpuWeatherColumnInstance::pack([0, 0], 0, 100, 200, 0, -2.0, 0, [255; 4]);
    assert_eq!(packer.push_particle(a), 0);
    assert_eq!(packer.push_particle(b), 1);
    assert_eq!(packer.push_weather_column(w), 0);
    assert_eq!(packer.dirty_particles(), &[a, b]);
    assert_eq!(packer.dirty_weather_columns(), &[w]);
    assert_eq!(packer.dirty_particle_words().len(), 8);
    assert_eq!(packer.dirty_weather_words().len(), 4);

    // After upload, only newly pushed instances are dirty.
    packer.clear_dirty();
    assert!(packer.dirty_particles().is_empty());
    let c = GpuParticleInstance::pack([3.0, 0.0, 0.0], [0.0; 3], [255; 4], 255, 2, 0);
    assert_eq!(packer.push_particle(c), 2);
    assert_eq!(packer.dirty_particles(), &[c]);
    assert_eq!(packer.particles(), &[a, b, c]);

    packer.reset();
    assert!(packer.particles().is_empty());
    assert!(packer.weather_columns().is_empty());
    assert!(packer.dirty_particles().is_empty());
}
