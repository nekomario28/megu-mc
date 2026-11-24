// SPDX-License-Identifier: LGPL-3.0-or-later
use megu_core::crowd_physics::*;

fn mob(id: i32, x: f64, y: f64, z: f64) -> CrowdEntity {
    CrowdEntity {
        id,
        x,
        y,
        z,
        aabb: Aabb {
            min_x: x - 0.3,
            min_y: y,
            min_z: z - 0.3,
            max_x: x + 0.3,
            max_y: y + 0.9,
            max_z: z + 0.3,
        },
        pushable: true,
        ..CrowdEntity::default()
    }
}

fn reference(entities: &[CrowdEntity]) -> Vec<[f64; 2]> {
    let mut out = vec![[0.0; 2]; entities.len()];
    for i in 0..entities.len() {
        for j in i + 1..entities.len() {
            let a = entities[i];
            let b = entities[j];
            if !a.pushable
                || !b.pushable
                || a.spectator
                || b.spectator
                || !a.aabb.intersects(b.aabb)
                || a.no_physics
                || b.no_physics
                || (a.same_vehicle_group != 0 && a.same_vehicle_group == b.same_vehicle_group)
            {
                continue;
            }
            let dx = b.x - a.x;
            let dz = b.z - a.z;
            let d = dx.abs().max(dz.abs());
            if d < ENTITY_PUSH_MIN_ABS_MAX {
                continue;
            }
            let d = d.sqrt();
            let dx = dx / d * (1.0 / d).min(1.0) * ENTITY_PUSH_STRENGTH;
            let dz = dz / d * (1.0 / d).min(1.0) * ENTITY_PUSH_STRENGTH;
            if !a.vehicle {
                out[i][0] -= dx;
                out[i][1] -= dz;
            }
            if !b.vehicle {
                out[j][0] += dx;
                out[j][1] += dz;
            }
        }
    }
    out
}

#[test]
fn exact_small_clusters_match_symmetric_reference_for_requested_sizes() {
    for n in [2, 8, 24, 25, 32] {
        let mut es: Vec<_> = (0..n)
            .map(|i| mob(i as i32, (i % 6) as f64 * 0.2, 0.0, (i / 6) as f64 * 0.2))
            .collect();
        if n > 4 {
            es[2].pushable = false;
            es[3].spectator = true;
            es[4].passenger = true;
            es[1].vehicle = true;
            es[0].no_physics = true;
            if n > 7 {
                es[5].same_vehicle_group = 17;
                es[6].same_vehicle_group = 17;
            }
        }
        let expected = reference(&es);
        let mut solver = DensePushSolver::new();
        solver.solve(&es, 0, &[]);
        assert_eq!(solver.mode, PushMode::Exact);
        for (i, o) in solver.outcomes().iter().enumerate() {
            assert_eq!([o.impulse_x, o.impulse_z], expected[i], "n={n}, i={i}");
            if !es[i].pushable || es[i].spectator {
                assert_eq!(o.pushable_neighbors, 0);
                assert_eq!(o.non_passenger_neighbors, 0);
                continue;
            }
            let neighbors: Vec<_> = es
                .iter()
                .enumerate()
                .filter(|(j, e)| {
                    *j != i && e.pushable && !e.spectator && es[i].aabb.intersects(e.aabb)
                })
                .map(|(_, e)| e)
                .collect();
            assert_eq!(o.pushable_neighbors as usize, neighbors.len());
            assert_eq!(
                o.non_passenger_neighbors as usize,
                neighbors.iter().filter(|e| !e.passenger).count()
            );
        }
    }
}

#[test]
fn cramming_census_honors_threshold_passengers_rng_roll_and_disabled_rule() {
    let mut s = DensePushSolver::new();
    let under: Vec<_> = (0..24).map(|i| mob(i, 0.5, 0.0, 0.5)).collect();
    s.solve(&under, 24, &vec![0; under.len()]);
    assert!(s.outcomes().iter().all(|o| o.cramming_damage == 0.0));
    let mut at_limit: Vec<_> = (0..25).map(|i| mob(i, 0.5, 0.0, 0.5)).collect();
    s.solve(&at_limit, 24, &vec![1; at_limit.len()]);
    assert!(s.outcomes().iter().all(|o| o.cramming_damage == 0.0));
    s.solve(&at_limit, 24, &vec![0; at_limit.len()]);
    assert!(s
        .outcomes()
        .iter()
        .all(|o| o.cramming_damage == ENTITY_CRAMMING_DAMAGE));
    at_limit[1].passenger = true;
    at_limit[2].passenger = true;
    s.solve(&at_limit, 24, &vec![0; at_limit.len()]);
    assert!(s.outcomes().iter().all(|o| o.cramming_damage == 0.0));
    s.solve(&at_limit, 0, &vec![0; at_limit.len()]);
    assert!(s.outcomes().iter().all(|o| o.cramming_damage == 0.0));
    assert_eq!(s.outcomes()[0].pushable_neighbors, 24);
}

#[test]
fn grid_is_three_dimensional_sap_deduplicated_and_handles_straddlers() {
    let mut es = vec![
        mob(1, 0.0, 0.0, 0.0),
        mob(2, 0.2, 3.0, 0.0),
        mob(3, 0.4, 0.0, 0.0),
    ];
    es[0].aabb.min_x = -0.2;
    es[0].aabb.max_x = 2.2;
    let mut grid = MicroCellGrid3D::new();
    grid.rebuild(&es);
    let pairs = grid.overlapping_pairs(&es);
    assert_eq!(pairs.len(), 1);
    assert_eq!(
        pairs,
        &[EntityPair {
            first: 0,
            second: 2
        }]
    );
    assert_ne!(es[0].cell(), es[1].cell());
    let mut solver = DensePushSolver::new();
    solver.solve(&es, 0, &[]);
    assert_eq!(solver.stats.exact_pairs, 1);
    let floor_pair = [mob(4, 0.0, 0.0, 0.0), mob(5, 0.0, 3.0, 0.0)];
    solver.solve(&floor_pair, 0, &[]);
    assert_eq!(solver.stats.exact_pairs, 0);
    assert_eq!(solver.outcomes()[0].pushable_neighbors, 0);
}

#[test]
fn dense_thousand_has_bounded_work_finite_impulses_and_cramming() {
    let es: Vec<_> = (0..1000)
        .map(|i| {
            mob(
                i,
                0.5 + (i % 10) as f64 * 0.001,
                0.5 + (i / 10 % 10) as f64 * 0.001,
                0.5 + (i / 100) as f64 * 0.001,
            )
        })
        .collect();
    let rolls = vec![0; es.len()];
    let mut s = DensePushSolver::new();
    s.bounded_neighbors = 16;
    s.solve(&es, 24, &rolls);
    assert_eq!(s.mode, PushMode::BoundedApproximation);
    assert!(s.stats.hyperdense_cells > 0);
    assert_eq!(s.stats.bounded_pairs, 1000 * 16);
    assert!(s.stats.exact_pairs < 1000 * 16);
    assert!(s.stats.crammed_entities > 0);
    assert!(s
        .outcomes()
        .iter()
        .all(|o| o.impulse_x.is_finite() && o.impulse_z.is_finite() && o.cramming_damage == 6.0));
}

#[test]
fn two_thousand_mobs_separate_into_eight_3d_groups() {
    let mut es = Vec::new();
    for g in 0..8 {
        let (x, y, z) = (
            (g % 4) as f64 * 4.0 + 1.0,
            if g / 4 == 0 { 1.0 } else { 7.0 },
            1.0,
        );
        for n in 0..250 {
            es.push(mob(
                (g * 250 + n) as i32,
                x + (n % 10) as f64 * 0.002,
                y + (n / 10 % 10) as f64 * 0.002,
                z + (n / 100) as f64 * 0.002,
            ));
        }
    }
    let mut grid = MicroCellGrid3D::new();
    grid.rebuild(&es);
    assert_eq!(grid.cells().len(), 8);
    let mut s = DensePushSolver::new();
    s.solve(&es, 24, &vec![1; es.len()]);
    assert_eq!(s.stats.hyperdense_cells, 8);
}

#[test]
fn sync_governor_suppresses_dense_jostle_and_never_suppresses_critical() {
    let mut g = CrowdSyncGovernor::new();
    for tick in 0..100 {
        for id in 0..500 {
            let d = g.decide(id, tick, 30, 0.001, CrowdSyncFlags::default());
            if d.forced {
                assert!(d.send)
            }
        }
    }
    assert_eq!(g.stats.sent, 4165);
    assert_eq!(g.stats.suppressed, 45_835);
    assert!(g.stats.suppressed as f64 / (g.stats.sent + g.stats.suppressed) as f64 >= 0.80);
    assert!(g.stats.forced_periodic > 0);
    for f in [
        CrowdSyncFlags {
            player: true,
            ..Default::default()
        },
        CrowdSyncFlags {
            hurt: true,
            ..Default::default()
        },
        CrowdSyncFlags {
            vy: 0.06,
            ..Default::default()
        },
        CrowdSyncFlags {
            breeding: true,
            ..Default::default()
        },
    ] {
        assert!(g.decide(1, 101, 100, 0.0, f).send)
    }
    assert!(
        g.decide(1, 101, 100, f64::NAN, CrowdSyncFlags::default())
            .send
    );
    assert!(g.stats.critical_bypass >= 4);
}

#[test]
fn sleep_stride_distribution_wakes_on_critical_and_floor_cache_tracks_revisions() {
    assert_eq!(CrowdSleepGovernor::stride(100, 500), 19);
    assert_eq!(CrowdSleepGovernor::stride(100, 200), 5);
    assert_eq!(CrowdSleepGovernor::stride(100, 100), 3);
    assert_eq!(CrowdSleepGovernor::stride(100, 30), 2);
    assert_eq!(CrowdSleepGovernor::stride(100, 10), 1);
    assert_eq!(
        (0..19)
            .filter(|tick| CrowdSleepGovernor::should_run_ai(
                5,
                *tick,
                100,
                500,
                CrowdSleepFlags::default()
            ))
            .count(),
        1
    );
    for flags in [
        CrowdSleepFlags {
            combat: true,
            ..Default::default()
        },
        CrowdSleepFlags {
            hurt: true,
            ..Default::default()
        },
        CrowdSleepFlags {
            water_or_lava: true,
            ..Default::default()
        },
        CrowdSleepFlags {
            breeding: true,
            ..Default::default()
        },
        CrowdSleepFlags {
            moved: true,
            ..Default::default()
        },
        CrowdSleepFlags {
            gravity: true,
            ..Default::default()
        },
        CrowdSleepFlags {
            cramming: true,
            ..Default::default()
        },
    ] {
        assert!(CrowdSleepGovernor::should_run_ai(1, 0, 100, 500, flags))
    }
    let mut c = FloorSupportCache::default();
    c.insert(7, (1, 2, 3), 10, true);
    assert_eq!(c.get(7, (1, 2, 3), 10), Some(true));
    assert_eq!(c.get(7, (1, 2, 3), 11), None);
    assert_eq!(c.get(7, (2, 2, 3), 10), None);
}

#[test]
fn render_budget_preserves_priority_caps_ordinary_cells_and_pose_quantizes() {
    let mut es: Vec<_> = (0..1000)
        .map(|i| RenderEntity {
            id: i,
            x: 0.2,
            y: 0.2,
            z: 0.2,
            camera_distance: (i % 40) as f64,
            ..RenderEntity::default()
        })
        .collect();
    es[0].player = true;
    es[1].boss = true;
    es[2].custom_named = true;
    es[3].hurt = true;
    es[4].baby_in_love = true;
    es[5].combat_target = true;
    let mut b = CrowdRenderBudget::default();
    let lod = b.apply(&es);
    for i in 0..6 {
        assert_eq!(lod[i], RenderLod::Full)
    }
    assert_eq!(lod.iter().filter(|x| **x == RenderLod::Full).count(), 18);
    let first = lod.to_vec();
    assert_eq!(b.apply(&es), first);
    let a = quantize_crowd_pose_key(0.2, 0.4, 0.1, 20.0, 100);
    let same = quantize_crowd_pose_key(0.21, 0.41, 0.11, 20.0, 100);
    assert_eq!(a, same);
    assert_eq!(a.quality, PoseQuality::CoarseStatic);
    let mut previous = None;
    let mut key_changes = 0;
    let mut raw_changes = 0;
    for tick in 0..1000 {
        let jitter = (tick % 7) as f32 * 0.005;
        let raw = 0.2 + jitter;
        if tick > 0 && raw != 0.2 + ((tick - 1) % 7) as f32 * 0.005 {
            raw_changes += 1;
        }
        let key = quantize_crowd_pose_key(raw, 0.4 + jitter, 0.1 + jitter, 20.0, 100);
        if previous.is_some_and(|p| p != key) {
            key_changes += 1;
        }
        previous = Some(key);
    }
    assert!((raw_changes - key_changes) as f64 / raw_changes as f64 >= 0.85);
    let mut global = CrowdRenderBudget {
        max_rigs_per_cell: 0,
        max_total_rigs: 10,
        lods: Vec::new(),
    };
    let many_cells: Vec<_> = (0..20)
        .map(|i| RenderEntity {
            id: i,
            x: i as f64 * 2.0,
            ..RenderEntity::default()
        })
        .collect();
    assert_eq!(
        global
            .apply(&many_cells)
            .iter()
            .filter(|lod| **lod == RenderLod::Full)
            .count(),
        10
    );
}
