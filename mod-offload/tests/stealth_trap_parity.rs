// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

use std::collections::HashMap;

use mod_offload::entity_spatial::{
    AiTickInput, ChunkSensorBroadPhase, CollisionResult, DroppedItem, EntityAiState, EntityKind,
    MobSensor, SensorEntity, SoaEntityPool, XpOrb, FLAG_NO_PHYSICS, FLAG_PASSENGER, FLAG_PUSHABLE,
    FLAG_SPECTATOR, FLAG_VEHICLE,
};
use mod_offload::{
    evaluate_mob_targets_uncached, merge_item_stacks, merge_xp_orbs, solve_collisions_spatial,
    tick_confined_ai,
};

/// Independently visits all N² pairs in the same deterministic pusher/candidate
/// order as Vanilla's indexed pushEntities step. No spatial helpers are reused.
fn reference_collision(pool: &SoaEntityPool, max_cramming: usize, rolls: &[u8]) -> CollisionResult {
    let n = pool.len();
    let mut expected = CollisionResult {
        push_deltas: vec![[0.0; 3]; n],
        cramming_damage: vec![false; n],
        candidate_counts: vec![0; n],
    };
    for i in 0..n {
        if matches!(
            pool.kinds[i],
            EntityKind::Other | EntityKind::PointOfInterest
        ) || pool.flags[i] & FLAG_SPECTATOR != 0
        {
            continue;
        }
        let mut nonpassengers = 0;
        for j in 0..n {
            if i == j || pool.flags[j] & FLAG_PUSHABLE == 0 || pool.flags[j] & FLAG_SPECTATOR != 0 {
                continue;
            }
            let mut intersect = true;
            for axis in 0..3 {
                let extent_i = if axis == 1 {
                    pool.half_extents[i][1]
                } else {
                    pool.half_extents[i][0]
                } as f64;
                let extent_j = if axis == 1 {
                    pool.half_extents[j][1]
                } else {
                    pool.half_extents[j][0]
                } as f64;
                intersect &= pool.pos[i][axis] - extent_i < pool.pos[j][axis] + extent_j
                    && pool.pos[i][axis] + extent_i > pool.pos[j][axis] - extent_j;
            }
            if !intersect {
                continue;
            }
            expected.candidate_counts[i] += 1;
            if pool.flags[j] & FLAG_PASSENGER == 0 {
                nonpassengers += 1;
            }
            if pool.flags[i] & FLAG_NO_PHYSICS != 0 || pool.flags[j] & FLAG_NO_PHYSICS != 0 {
                continue;
            }
            let dx = pool.pos[i][0] - pool.pos[j][0];
            let dz = pool.pos[i][2] - pool.pos[j][2];
            let mut d = dx.abs().max(dz.abs());
            if d < 0.009999999776482582 {
                continue;
            }
            d = d.sqrt();
            let mut px = dx / d;
            let mut pz = dz / d;
            let scale = (1.0 / d).min(1.0);
            px *= scale;
            pz *= scale;
            px *= 0.05000000074505806;
            pz *= 0.05000000074505806;
            if pool.flags[j] & FLAG_VEHICLE == 0 {
                expected.push_deltas[j][0] += -px;
                expected.push_deltas[j][2] += -pz;
            }
            if pool.flags[i] & (FLAG_PUSHABLE | FLAG_VEHICLE) == FLAG_PUSHABLE {
                expected.push_deltas[i][0] += px;
                expected.push_deltas[i][2] += pz;
            }
        }
        expected.cramming_damage[i] = max_cramming > 0
            && expected.candidate_counts[i] >= max_cramming
            && nonpassengers >= max_cramming
            && rolls[i] % 4 == 0;
    }
    expected
}

fn assert_collision_bits(actual: &CollisionResult, expected: &CollisionResult) {
    assert_eq!(actual.candidate_counts, expected.candidate_counts);
    assert_eq!(actual.cramming_damage, expected.cramming_damage);
    for (index, (a, b)) in actual
        .push_deltas
        .iter()
        .zip(&expected.push_deltas)
        .enumerate()
    {
        for axis in 0..3 {
            assert_eq!(
                a[axis].to_bits(),
                b[axis].to_bits(),
                "entity={index} axis={axis}"
            );
        }
    }
}

#[test]
fn spatial_250_entity_kill_chamber_matches_quadratic_bit_for_bit() {
    let mut pool = SoaEntityPool::default();
    for id in 0..250u32 {
        let flag = match id % 47 {
            0 => FLAG_PUSHABLE | FLAG_PASSENGER,
            1 => FLAG_PUSHABLE | FLAG_VEHICLE,
            2 => FLAG_PUSHABLE | FLAG_NO_PHYSICS,
            3 => FLAG_SPECTATOR,
            _ => FLAG_PUSHABLE,
        };
        pool.push(
            id,
            [
                (id % 16) as f64 * 0.051 - 0.7,
                (id % 3) as f64 * 0.015,
                (id / 16) as f64 * 0.049 - 0.6,
            ],
            [0.0; 3],
            [0.34, 0.9],
            if id % 23 == 0 {
                EntityKind::Player
            } else {
                EntityKind::Cow
            },
            flag,
        );
    }
    let rolls = (0..pool.len()).map(|i| (i % 4) as u8).collect::<Vec<_>>();
    let pool_before = pool.clone();
    for max_cramming in [0, 1, 24, 249, 250, 500] {
        let reference = reference_collision(&pool, max_cramming, &rolls);
        let spatial = solve_collisions_spatial(&pool, max_cramming, &rolls);
        assert_collision_bits(&spatial, &reference);
    }
    assert!(solve_collisions_spatial(&pool, 24, &rolls)
        .cramming_damage
        .iter()
        .any(|&v| v));
    assert_eq!(pool.ids, pool_before.ids);
    assert_eq!(pool.pos, pool_before.pos);
    assert_eq!(pool.vel, pool_before.vel);
}

#[test]
fn spatial_broad_phase_handles_cell_boundaries_large_entities_and_empty_pool() {
    let mut pool = SoaEntityPool::default();
    let rolls = vec![0; 54];
    for i in 0..54u32 {
        let x = if i % 2 == 0 { 1.99 } else { 2.01 };
        let y = if i % 3 == 0 { -2.01 } else { -1.99 };
        let width = if i == 0 { 8.0 } else { 0.2 };
        pool.push(
            i,
            [x + i as f64 * 0.013, y, -0.01],
            [0.0; 3],
            [width, 0.8],
            EntityKind::Villager,
            FLAG_PUSHABLE,
        );
    }
    assert_collision_bits(
        &solve_collisions_spatial(&pool, 24, &rolls),
        &reference_collision(&pool, 24, &rolls),
    );
    let empty = SoaEntityPool::default();
    assert_collision_bits(
        &solve_collisions_spatial(&empty, 24, &[]),
        &reference_collision(&empty, 24, &[]),
    );
}

#[test]
fn spatial_randomized_sparse_and_oversized_entities_match_reference() {
    for seed in 0..8u64 {
        let mut pool = SoaEntityPool::default();
        let mut state = seed + 1;
        for id in 0..128u32 {
            let mut coord = || {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                ((state >> 32) % 400) as f64 / 10.0 - 20.0
            };
            let half_extents = if id == 64 { [150.0, 50.0] } else { [0.3, 0.9] };
            pool.push(
                id,
                [coord(), coord(), coord()],
                [0.0; 3],
                half_extents,
                EntityKind::Zombie,
                if id % 13 == 0 {
                    FLAG_PUSHABLE | FLAG_PASSENGER
                } else {
                    FLAG_PUSHABLE
                },
            );
        }
        let rolls = (0..pool.len()).map(|i| (i % 4) as u8).collect::<Vec<_>>();
        assert_collision_bits(
            &solve_collisions_spatial(&pool, 24, &rolls),
            &reference_collision(&pool, 24, &rolls),
        );
    }
}

#[test]
fn thousand_item_stacks_and_xp_orbs_conserve_exact_totals() {
    let mut items: Vec<DroppedItem> = (0..1000u32)
        .map(|id| DroppedItem {
            id,
            pos: [
                ((id / 200) as f64 * 8.0) + ((id % 7) as f64 * 0.03),
                -0.05,
                -0.05,
            ],
            vel: if id % 4 == 0 {
                [0.1, -0.2, 0.0]
            } else {
                [0.0, -0.9, 0.0]
            },
            item_id: id % 5,
            component_hash: u64::from(id % 3),
            count: (id % 7) + 1,
            max_stack_size: 64,
        })
        .collect();
    let original_items = items.clone();
    let mut xp: Vec<XpOrb> = (0..1000u32)
        .map(|id| XpOrb {
            id,
            pos: [
                ((id / 200) as f64 * 8.0) + ((id % 13) as f64 * 0.03),
                0.0,
                0.0,
            ],
            vel: if id % 2 == 0 {
                [0.1, 0.0, 0.0]
            } else {
                [0.0, -0.9, 0.0]
            },
            value: u64::from(id % 17 + 1),
        })
        .collect();
    let original_xp = xp.clone();
    let mut totals_before = HashMap::<(u32, u64), u64>::new();
    for item in &items {
        *totals_before
            .entry((item.item_id, item.component_hash))
            .or_default() += u64::from(item.count);
    }
    let xp_before: u128 = xp.iter().map(|v| u128::from(v.value)).sum();

    let item_report = merge_item_stacks(&mut items);
    let xp_report = merge_xp_orbs(&mut xp);
    assert!(item_report.eliminated > 0);
    assert!(xp_report.eliminated > 0);
    assert!(item_report.transferred > 0 && xp_report.transferred > 0);
    let mut totals_after = HashMap::<(u32, u64), u64>::new();
    for item in &items {
        *totals_after
            .entry((item.item_id, item.component_hash))
            .or_default() += u64::from(item.count);
        assert!(item.count <= item.max_stack_size);
    }
    assert_eq!(totals_before, totals_after);
    assert_eq!(xp_before, xp.iter().map(|v| u128::from(v.value)).sum());

    // Neither surviving nor retired entities have their world physics rewritten.
    for (before, after) in original_items.iter().zip(&items) {
        assert_eq!(
            (
                before.id,
                before.pos,
                before.vel,
                before.item_id,
                before.component_hash
            ),
            (
                after.id,
                after.pos,
                after.vel,
                after.item_id,
                after.component_hash
            )
        );
    }
    for (before, after) in original_xp.iter().zip(&xp) {
        assert_eq!(
            (before.id, before.pos, before.vel),
            (after.id, after.pos, after.vel)
        );
    }
    assert_eq!(merge_item_stacks(&mut items).transferred, 0);
    assert_eq!(merge_xp_orbs(&mut xp).transferred, 0);
}

#[test]
fn stack_caps_component_identity_radius_and_stream_velocity_are_respected() {
    let template = DroppedItem {
        id: 0,
        pos: [0.0; 3],
        vel: [0.0, -0.9, 0.0],
        item_id: 4,
        component_hash: 1,
        count: 60,
        max_stack_size: 64,
    };
    let mut items = [
        template,
        DroppedItem {
            id: 1,
            count: 10,
            pos: [0.49, 0.0, 0.0],
            ..template
        },
        DroppedItem {
            id: 2,
            count: 3,
            pos: [-0.51, 0.0, 0.0],
            ..template
        },
        DroppedItem {
            id: 3,
            count: 3,
            component_hash: 2,
            ..template
        },
        DroppedItem {
            id: 4,
            count: 3,
            vel: [0.1, -0.9, 0.0],
            ..template
        },
    ];
    merge_item_stacks(&mut items);
    assert_eq!(
        [
            items[0].count,
            items[1].count,
            items[2].count,
            items[3].count,
            items[4].count
        ],
        [64, 6, 3, 3, 3]
    );

    let template = XpOrb {
        id: 0,
        pos: [0.0; 3],
        vel: [0.0, -0.9, 0.0],
        value: 4,
    };
    let mut xp = [
        template,
        XpOrb {
            id: 1,
            pos: [1.0, 0.0, 0.0],
            value: 7,
            ..template
        },
        XpOrb {
            id: 2,
            pos: [1.01, 0.0, 0.0],
            value: 11,
            ..template
        },
        XpOrb {
            id: 3,
            pos: [0.1, 0.0, 0.0],
            vel: [0.1, -0.9, 0.0],
            ..template
        },
    ];
    merge_xp_orbs(&mut xp);
    assert_eq!(
        [xp[0].value, xp[1].value, xp[2].value, xp[3].value],
        [11, 0, 11, 4]
    );
    let mut overflow = [
        XpOrb {
            value: u64::MAX,
            ..template
        },
        XpOrb {
            id: 1,
            value: 5,
            ..template
        },
    ];
    merge_xp_orbs(&mut overflow);
    assert_eq!([overflow[0].value, overflow[1].value], [u64::MAX, 5]);
}

#[test]
fn item_and_xp_merges_include_radius_boundary_but_exclude_outside() {
    let item = DroppedItem {
        id: 0,
        pos: [0.0; 3],
        vel: [0.0; 3],
        item_id: 1,
        component_hash: 5,
        count: 1,
        max_stack_size: 64,
    };
    let mut items = [
        item,
        DroppedItem {
            id: 1,
            pos: [0.5, 0.0, 0.0],
            ..item
        },
        DroppedItem {
            id: 2,
            pos: [1.00001, 0.0, 0.0],
            ..item
        },
    ];
    assert_eq!(merge_item_stacks(&mut items).eliminated, 1);
    assert_eq!([items[0].count, items[1].count, items[2].count], [2, 0, 1]);

    let orb = XpOrb {
        id: 0,
        pos: [0.0; 3],
        vel: [0.0; 3],
        value: 7,
    };
    let mut orbs = [
        orb,
        XpOrb {
            id: 1,
            pos: [1.0, 0.0, 0.0],
            value: 11,
            ..orb
        },
        XpOrb {
            id: 2,
            pos: [2.00001, 0.0, 0.0],
            value: 13,
            ..orb
        },
    ];
    assert_eq!(merge_xp_orbs(&mut orbs).eliminated, 1);
    assert_eq!([orbs[0].value, orbs[1].value, orbs[2].value], [18, 0, 13]);
}

fn assert_visual_equal_excluding_pathfinding(
    mut optimized: EntityAiState,
    original: EntityAiState,
) {
    optimized.pathfinding_calls = original.pathfinding_calls;
    assert_eq!(optimized, original);
}

#[test]
fn confined_animals_villager_visuals_and_lifecycle_match_with_sleep() {
    for kind in [
        EntityKind::Sheep,
        EntityKind::Cow,
        EntityKind::Pig,
        EntityKind::Chicken,
        EntityKind::Villager,
    ] {
        let mut off = EntityAiState::new(kind);
        let mut on = off;
        off.baby_age = -120;
        on.baby_age = -120;
        off.egg_timer = 3;
        on.egg_timer = 3;
        off.has_wool = false;
        on.has_wool = false;
        off.grass_eating_ticks = 13;
        on.grass_eating_ticks = 13;
        let first_yaw = on.head_yaw;
        for tick in 0..120 {
            let input = AiTickInput {
                pos: [0.0, 64.0, 0.0],
                nearest_player: Some([tick as f64 * 0.2 - 10.0, 65.0, 1.0]),
                enclosed: true,
                navigation_idle: true,
                grass_eaten: tick == 7,
                next_egg_interval: 9,
                ..AiTickInput::default()
            };
            let a = tick_confined_ai(&mut off, &input, false);
            let b = tick_confined_ai(&mut on, &input, true);
            assert!(a.pathfinding_ran);
            assert!(!b.pathfinding_ran);
            assert_eq!((a.egg_laid, a.wool_regrown), (b.egg_laid, b.wool_regrown));
            assert_visual_equal_excluding_pathfinding(on, off);
        }
        assert_ne!(first_yaw, on.head_yaw);
        assert_eq!(on.pathfinding_calls, 0);
        assert_eq!(off.pathfinding_calls, 120);
        assert_eq!(on.baby_age, 0);
        if kind == EntityKind::Sheep {
            assert!(on.has_wool);
        }
        if kind == EntityKind::Chicken {
            assert!(on.eggs_laid >= 10);
        }
    }
}

#[test]
fn wall_damage_food_and_predators_wake_animals_on_same_tick() {
    let base = AiTickInput {
        enclosed: true,
        navigation_idle: true,
        ..AiTickInput::default()
    };
    for wake in 0..8 {
        let mut input = base;
        match wake {
            0 => input.wall_removed = true,
            1 => input.damaged = true,
            2 => input.breeding_food_nearby = true,
            3 => input.predator_or_zombie_nearby = true,
            4 => input.lured = true,
            5 => input.falling = true,
            6 => input.attacking = true,
            _ => input.spawner_drop_chute = true,
        }
        let mut state = EntityAiState::new(EntityKind::Sheep);
        let result = tick_confined_ai(&mut state, &input, true);
        assert!(result.pathfinding_ran && result.woke);
        assert_eq!(state.pathfinding_calls, 1);
    }
}

#[test]
fn enderman_endermite_silverfish_and_spawner_chute_remain_fully_awake() {
    let kinds = [
        EntityKind::Enderman,
        EntityKind::Endermite,
        EntityKind::Silverfish,
        EntityKind::SpawnerMob,
        EntityKind::Zombie,
    ];
    for kind in kinds {
        let mut off = EntityAiState::new(kind);
        let mut on = off;
        let mut positions = [[0.0, 64.0, 0.0], [0.0, 64.0, 0.0]];
        for tick in 0..100 {
            let falling = kind == EntityKind::SpawnerMob;
            let input = AiTickInput {
                pos: positions[0],
                nearest_player: Some([2.0, 65.0, tick as f64 / 10.0]),
                enclosed: true,
                navigation_idle: true,
                lured: kind == EntityKind::Enderman || kind == EntityKind::Endermite,
                infested_block_alarm: kind == EntityKind::Silverfish && tick == 40,
                falling,
                spawner_drop_chute: falling,
                attacking: kind == EntityKind::Zombie,
                ..AiTickInput::default()
            };
            let first = tick_confined_ai(&mut off, &input, false);
            let second = tick_confined_ai(&mut on, &input, true);
            assert_eq!(first, second);
            if falling {
                // External water/fall physics advances identically in both paths.
                for pos in &mut positions {
                    pos[1] -= 0.13;
                    pos[0] += 0.01;
                }
            }
        }
        assert_eq!(off, on);
        assert_eq!(positions[0], positions[1]);
        assert_eq!(on.pathfinding_calls, 100);
    }
}

fn sensor_target(id: u32, kind: EntityKind, pos: [f64; 3]) -> SensorEntity {
    SensorEntity {
        id,
        kind,
        dimension: 0,
        pos,
        invisible: false,
        invisible_range_multiplier: 0.07,
        gold_armor: false,
    }
}

#[test]
fn chunk_sensor_cache_matches_full_scan_for_invisible_gold_and_edge_targets() {
    let mut targets = vec![
        SensorEntity {
            invisible: true,
            ..sensor_target(1, EntityKind::Player, [16.2, 64.0, 0.0])
        },
        SensorEntity {
            invisible: true,
            invisible_range_multiplier: 0.35,
            ..sensor_target(2, EntityKind::Player, [17.3, 64.0, 0.0])
        },
        SensorEntity {
            gold_armor: true,
            ..sensor_target(3, EntityKind::Player, [15.05, 64.0, 4.0])
        },
        sensor_target(4, EntityKind::Player, [15.5, 64.0, 4.0]),
        sensor_target(5, EntityKind::Endermite, [-0.1, 64.0, 0.0]),
        sensor_target(6, EntityKind::Villager, [-16.2, 64.0, 0.0]),
        sensor_target(7, EntityKind::PointOfInterest, [0.0, 64.0, 0.0]),
        sensor_target(8, EntityKind::Player, [80.0, 64.0, 0.0]),
        sensor_target(9, EntityKind::Player, [14.0, 64.0, 4.0]),
    ];
    targets[8].invisible = true;
    targets[8].invisible_range_multiplier = 0.07;
    let mobs = [
        MobSensor {
            id: 100,
            kind: EntityKind::Zombie,
            dimension: 0,
            pos: [15.8, 64.0, 0.0],
            follow_range: 8.0,
        },
        MobSensor {
            id: 101,
            kind: EntityKind::Piglin,
            dimension: 0,
            pos: [15.0, 64.0, 4.0],
            follow_range: 8.0,
        },
        MobSensor {
            id: 102,
            kind: EntityKind::Enderman,
            dimension: 0,
            pos: [0.1, 64.0, 0.0],
            follow_range: 8.0,
        },
        MobSensor {
            id: 103,
            kind: EntityKind::Zombie,
            dimension: 0,
            pos: [-15.8, 64.0, 0.0],
            follow_range: 8.0,
        },
        MobSensor {
            id: 104,
            kind: EntityKind::Zombie,
            dimension: 1,
            pos: [-15.8, 64.0, 0.0],
            follow_range: 8.0,
        },
    ];
    let cache = ChunkSensorBroadPhase::build(987, &targets);
    assert_eq!(cache.tick, 987);
    assert!(cache.chunk_population(0, 0, 0) >= 3);
    for mob in &mobs {
        assert_eq!(
            cache.evaluate_mob_targets(mob, |_, _| true),
            evaluate_mob_targets_uncached(mob, &targets, |_, _| true)
        );
    }
    assert_eq!(cache.evaluate_mob_targets(&mobs[0], |_, _| true), Some(1));
    assert_eq!(
        cache.evaluate_mob_targets(&mobs[1], |_, target| target.id != 4),
        None
    );
    assert_eq!(cache.evaluate_mob_targets(&mobs[1], |_, _| true), Some(4));
    assert_eq!(cache.evaluate_mob_targets(&mobs[2], |_, _| true), Some(5));
    assert_eq!(cache.evaluate_mob_targets(&mobs[3], |_, _| true), Some(6));
    assert_eq!(cache.evaluate_mob_targets(&mobs[4], |_, _| true), None);

    // Broad-phase is deliberately immutable across the tick. Rebuild is the
    // explicit world-tick boundary after live positions or equipment change.
    targets[0].pos = [100.0, 64.0, 0.0];
    let refreshed = ChunkSensorBroadPhase::build(988, &targets);
    assert_eq!(
        refreshed.evaluate_mob_targets(&mobs[0], |_, _| true),
        Some(2)
    );
}

#[test]
fn fifty_plus_mobs_per_chunk_cached_vs_uncached_with_los_and_negative_chunks() {
    let mut targets = Vec::new();
    for i in 0..340u32 {
        let mut target = sensor_target(
            i + 1000,
            match i % 5 {
                0 => EntityKind::Player,
                1 => EntityKind::Villager,
                2 => EntityKind::Endermite,
                3 => EntityKind::PointOfInterest,
                _ => EntityKind::Sheep,
            },
            [
                ((i * 37) % 95) as f64 - 50.0,
                64.0 + (i % 3) as f64,
                ((i * 29) % 91) as f64 - 50.0,
            ],
        );
        target.dimension = (i % 3 == 0) as i32;
        target.invisible = i % 11 == 0;
        target.invisible_range_multiplier = if i % 2 == 0 { 0.07 } else { 0.6 };
        target.gold_armor = i % 7 == 0;
        targets.push(target);
    }
    let cache = ChunkSensorBroadPhase::build(1024, &targets);
    for i in 0..150u32 {
        let mob = MobSensor {
            id: 4000 + i,
            kind: match i % 3 {
                0 => EntityKind::Piglin,
                1 => EntityKind::Zombie,
                _ => EntityKind::Enderman,
            },
            dimension: (i % 4 == 0) as i32,
            pos: [
                ((i * 31) % 41) as f64 - 24.0,
                64.0,
                ((i * 17) % 49) as f64 - 25.0,
            ],
            follow_range: if i % 17 == 0 { 280.0 } else { 12.0 },
        };
        let los = |_: &MobSensor, target: &SensorEntity| {
            target.id % 13 != 0 && (mob.id ^ target.id) % 19 != 0
        };
        assert_eq!(
            cache.evaluate_mob_targets(&mob, los),
            evaluate_mob_targets_uncached(&mob, &targets, los),
            "mob={i}"
        );
    }
}

#[test]
fn sensor_cache_enlarged_detection_range_and_shared_chunk_match_full_scan() {
    let mut targets = vec![
        SensorEntity {
            invisible: true,
            invisible_range_multiplier: 2.5,
            ..sensor_target(1, EntityKind::Player, [27.0, 64.0, 0.0])
        },
        sensor_target(2, EntityKind::Villager, [16.01, 64.0, 0.0]),
    ];
    // 64 mobs in one chunk reuse the same once-per-tick broad phase.
    let cache = ChunkSensorBroadPhase::build(900, &targets);
    for i in 0..64u32 {
        let mob = MobSensor {
            id: 100 + i,
            kind: EntityKind::Zombie,
            dimension: 0,
            pos: [15.0 - (i % 8) as f64 / 8.0, 64.0, (i / 8) as f64 / 10.0],
            follow_range: 6.0,
        };
        assert_eq!(
            cache.evaluate_mob_targets(&mob, |_, _| true),
            evaluate_mob_targets_uncached(&mob, &targets, |_, _| true),
        );
    }
    assert_eq!(cache.chunk_population(0, 1, 0), 2);
    targets[0].invisible_range_multiplier = 0.07;
    let cache = ChunkSensorBroadPhase::build(901, &targets);
    let mob = MobSensor {
        id: 200,
        kind: EntityKind::Zombie,
        dimension: 0,
        pos: [15.0, 64.0, 0.0],
        follow_range: 6.0,
    };
    assert_eq!(
        cache.evaluate_mob_targets(&mob, |_, _| true),
        evaluate_mob_targets_uncached(&mob, &targets, |_, _| true),
    );
}
