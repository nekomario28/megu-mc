// 1.21.1 Entity.collide / collideBoundingBox / collideWithShapes / collectCandidateStepUpHeights
// Pinned jar SHA-1: 59353fb40c36d304f2035d51e7d6e6baa98dc05c
// Official mappings: 03f8985492bda0afc0898465341eb0acef35f570
//   net.minecraft.world.entity.Entity              -> bsr
//   net.minecraft.world.level.BlockCollisions      -> dca
//   net.minecraft.world.phys.shapes.Shapes         -> exs
//   net.minecraft.world.phys.shapes.VoxelShape     -> exv
//   net.minecraft.world.phys.AABB                  -> ewx
//   net.minecraft.util.Mth                         -> ayo

#[derive(Clone, Copy, Debug, PartialEq)]
struct CollideResult {
    delta: [f64; 3],
    horizontal_collision: bool,
    vertical_collision: bool,
    vertical_collision_below: bool,
    stepped_up: bool,
}

impl EntityAabb {
    fn from_min_max(min: [f64; 3], max: [f64; 3]) -> Self {
        Self {
            min_x: min[0],
            min_y: min[1],
            min_z: min[2],
            max_x: max[0],
            max_y: max[1],
            max_z: max[2],
        }
    }

    fn min(self) -> [f64; 3] {
        [self.min_x, self.min_y, self.min_z]
    }

    fn max(self) -> [f64; 3] {
        [self.max_x, self.max_y, self.max_z]
    }

    /// 1.21.1 `AABB.move(double, double, double)` (`ewx.d(DDD)`).
    fn offset(self, dx: f64, dy: f64, dz: f64) -> Self {
        Self {
            min_x: self.min_x + dx,
            min_y: self.min_y + dy,
            min_z: self.min_z + dz,
            max_x: self.max_x + dx,
            max_y: self.max_y + dy,
            max_z: self.max_z + dz,
        }
    }

    /// 1.21.1 `AABB.expandTowards(double, double, double)` (`ewx.b(DDD)`).
    fn expand_towards(self, dx: f64, dy: f64, dz: f64) -> Self {
        Self {
            min_x: if dx < 0.0 { self.min_x + dx } else { self.min_x },
            min_y: if dy < 0.0 { self.min_y + dy } else { self.min_y },
            min_z: if dz < 0.0 { self.min_z + dz } else { self.min_z },
            max_x: if dx > 0.0 { self.max_x + dx } else { self.max_x },
            max_y: if dy > 0.0 { self.max_y + dy } else { self.max_y },
            max_z: if dz > 0.0 { self.max_z + dz } else { self.max_z },
        }
    }
}

/// 1.21.1 `BlockCollisions` (`dca.<init>` and `dca.computeNext`) world block collision reader.
/// Scans `[floor(min - 1e-7) - 1 ..= floor(max + 1e-7) + 1]` on all three axes so blocks
/// with shapes extending outside `[0, 1]^3` (such as 1.5-block fences/walls/gates) are visited
/// even when `min[1]` sits above `by` (e.g. after step-up), then filters by `aabb_overlaps`.
fn collect_block_collision_boxes(min: [f64; 3], max: [f64; 3]) -> Vec<([f64; 3], [f64; 3])> {
    let Some(reg) = crate::block_registry::breg() else {
        return Vec::new();
    };
    let bx0 = (min[0] - 1e-7).floor() as i32 - 1;
    let bx1 = (max[0] + 1e-7).floor() as i32 + 1;
    let by0 = (min[1] - 1e-7).floor() as i32 - 1;
    let by1 = (max[1] + 1e-7).floor() as i32 + 1;
    let bz0 = (min[2] - 1e-7).floor() as i32 - 1;
    let bz1 = (max[2] + 1e-7).floor() as i32 + 1;
    let mut boxes = Vec::new();
    for by in by0..=by1 {
        for bz in bz0..=bz1 {
            for bx in bx0..=bx1 {
                let st = block_at(bx, by, bz);
                for (bmin, bmax) in reg.collision_boxes(st) {
                    let wmin = [
                        bx as f64 + bmin[0],
                        by as f64 + bmin[1],
                        bz as f64 + bmin[2],
                    ];
                    let wmax = [
                        bx as f64 + bmax[0],
                        by as f64 + bmax[1],
                        bz as f64 + bmax[2],
                    ];
                    if aabb_overlaps((min, max), (wmin, wmax)) {
                        boxes.push((wmin, wmax));
                    }
                }
            }
        }
    }
    boxes
}

/// 1.21.1 `Shapes.collide` (`exs.a(ji$a, ewx, Iterable, D)`) + `VoxelShape.collideX` (`exv.a(ja, ewx, D)`).
fn collide_axis(
    a_min: [f64; 3],
    a_max: [f64; 3],
    axis: usize,
    mut dist: f64,
    boxes: &[([f64; 3], [f64; 3])],
) -> f64 {
    if dist.abs() < 1e-7 {
        return 0.0;
    }
    let (ax1, ax2) = match axis {
        0 => (1, 2),
        1 => (0, 2),
        _ => (0, 1),
    };
    for &(b_min, b_max) in boxes {
        if dist.abs() < 1e-7 {
            return 0.0;
        }
        // VoxelShape.collideX perpendicular overlap bounds (`exv.a` bytecodes 104-208):
        // findIndex(ax, a_min + 1e-7) <= 0  <=>  a_min + 1e-7 < b_max
        // findIndex(ax, a_max - 1e-7) >= 0  <=>  a_max - 1e-7 >= b_min
        if !(a_min[ax1] + 1e-7 < b_max[ax1]
            && a_max[ax1] - 1e-7 >= b_min[ax1]
            && a_min[ax2] + 1e-7 < b_max[ax2]
            && a_max[ax2] - 1e-7 >= b_min[ax2])
        {
            continue;
        }
        if dist > 0.0 && a_max[axis] - 1e-7 < b_min[axis] {
            let e = b_min[axis] - a_max[axis];
            if e >= -1e-7 {
                dist = dist.min(e);
            }
        } else if dist < 0.0 && a_min[axis] + 1e-7 >= b_max[axis] {
            let e = b_max[axis] - a_min[axis];
            if e <= 1e-7 {
                dist = dist.max(e);
            }
        }
    }
    dist
}

/// 1.21.1 `Entity.collideWithShapes(Vec3, AABB, List<VoxelShape>)` (`bsr.a(exc, ewx, List)`).
/// Resolves Y first, then Z before X iff `|x| < |z|` (on tie `|x| == |z|`, X resolves first).
fn collide_with_shapes(
    delta: [f64; 3],
    mut aabb: EntityAabb,
    boxes: &[([f64; 3], [f64; 3])],
) -> [f64; 3] {
    if boxes.is_empty() {
        return delta;
    }
    let mut x = delta[0];
    let mut y = delta[1];
    let mut z = delta[2];
    if y != 0.0 {
        y = collide_axis(aabb.min(), aabb.max(), 1, y, boxes);
        if y != 0.0 {
            aabb = aabb.offset(0.0, y, 0.0);
        }
    }
    let z_first = x.abs() < z.abs();
    if z_first && z != 0.0 {
        z = collide_axis(aabb.min(), aabb.max(), 2, z, boxes);
        if z != 0.0 {
            aabb = aabb.offset(0.0, 0.0, z);
        }
    }
    if x != 0.0 {
        x = collide_axis(aabb.min(), aabb.max(), 0, x, boxes);
        if !z_first && x != 0.0 {
            aabb = aabb.offset(x, 0.0, 0.0);
        }
    }
    if !z_first && z != 0.0 {
        z = collide_axis(aabb.min(), aabb.max(), 2, z, boxes);
    }
    [x, y, z]
}

/// 1.21.1 `Entity.collectCandidateStepUpHeights(AABB, List<VoxelShape>, float, float)`
/// (`bsr.a(ewx, List, F, F)`).
fn collect_candidate_step_up_heights(
    aabb: EntityAabb,
    boxes: &[([f64; 3], [f64; 3])],
    max_up_step: f32,
    y_clipped: f32,
) -> Vec<f32> {
    let mut heights = Vec::with_capacity(4);
    for &(b_min, b_max) in boxes {
        for d in [b_min[1], b_max[1]] {
            let f = (d - aabb.min_y) as f32;
            if f < 0.0 || f == y_clipped {
                continue;
            }
            if f > max_up_step {
                break;
            }
            let f_norm = if f == 0.0 { 0.0 } else { f };
            if !heights.contains(&f_norm) {
                heights.push(f_norm);
            }
        }
    }
    heights.sort_by(f32::total_cmp);
    heights
}

/// 1.21.1 `Entity.collide(Vec3)` (`bsr.a(exc)`) + `Entity.move` collision flags (`bsr.a(bts, exc)`).
fn entity_collide(
    aabb: EntityAabb,
    delta: [f64; 3],
    on_ground: bool,
    max_up_step: f32,
    extra_boxes: &[([f64; 3], [f64; 3])],
    mut collect_world_boxes: impl FnMut(EntityAabb) -> Vec<([f64; 3], [f64; 3])>,
) -> CollideResult {
    let len_sq = delta[0] * delta[0] + delta[1] * delta[1] + delta[2] * delta[2];
    let vec3 = if len_sq == 0.0 {
        delta
    } else {
        let sweep = aabb.expand_towards(delta[0], delta[1], delta[2]);
        let mut shapes = collect_world_boxes(sweep);
        for &(emin, emax) in extra_boxes {
            if aabb_overlaps((sweep.min(), sweep.max()), (emin, emax)) {
                shapes.push((emin, emax));
            }
        }
        collide_with_shapes(delta, aabb, &shapes)
    };

    let x_changed = delta[0] != vec3[0];
    let y_changed = delta[1] != vec3[1];
    let z_changed = delta[2] != vec3[2];
    let initial_vertical_below = y_changed && delta[1] < 0.0;

    let mut final_delta = vec3;
    let mut stepped_up = false;

    if max_up_step > 0.0 && (initial_vertical_below || on_ground) && (x_changed || z_changed) {
        let aabb2 = if initial_vertical_below {
            aabb.offset(0.0, vec3[1], 0.0)
        } else {
            aabb
        };
        let mut aabb3 = aabb2.expand_towards(delta[0], f64::from(max_up_step), delta[2]);
        if !initial_vertical_below {
            aabb3 = aabb3.expand_towards(0.0, f64::from(-1.0e-5f32), 0.0);
        }
        let mut shapes2 = collect_world_boxes(aabb3);
        for &(emin, emax) in extra_boxes {
            if aabb_overlaps((aabb3.min(), aabb3.max()), (emin, emax)) {
                shapes2.push((emin, emax));
            }
        }
        let base_h_dist_sq = vec3[0] * vec3[0] + vec3[2] * vec3[2];
        let heights =
            collect_candidate_step_up_heights(aabb2, &shapes2, max_up_step, vec3[1] as f32);
        for h in heights {
            let cand = collide_with_shapes([delta[0], f64::from(h), delta[2]], aabb2, &shapes2);
            let cand_h_dist_sq = cand[0] * cand[0] + cand[2] * cand[2];
            if cand_h_dist_sq > base_h_dist_sq {
                let d = aabb.min_y - aabb2.min_y;
                final_delta = [cand[0], cand[1] - d, cand[2]];
                stepped_up = true;
                break;
            }
        }
    }

    // 1.21.1 Entity.move (`bsr.a` bytecodes 275-380):
    // horizontalCollision uses `!Mth.equal(delta, actual)` (`ayo.b(DD)Z`, eps = 1.0E-5F);
    // verticalCollision uses `delta.y != actual.y`.
    let eps = f64::from(1.0e-5f32);
    let h_x = (delta[0] - final_delta[0]).abs() >= eps;
    let h_z = (delta[2] - final_delta[2]).abs() >= eps;
    let vertical_collision = delta[1] != final_delta[1];
    let vertical_collision_below = vertical_collision && delta[1] < 0.0;

    CollideResult {
        delta: final_delta,
        horizontal_collision: h_x || h_z,
        vertical_collision,
        vertical_collision_below,
        stepped_up,
    }
}

fn entity_collide_world(
    aabb: EntityAabb,
    delta: [f64; 3],
    on_ground: bool,
    max_up_step: f32,
    extra_boxes: &[([f64; 3], [f64; 3])],
) -> CollideResult {
    entity_collide(aabb, delta, on_ground, max_up_step, extra_boxes, |sweep| {
        collect_block_collision_boxes(sweep.min(), sweep.max())
    })
}

fn step_entity_move(
    pos: [f64; 3],
    width: f32,
    height: f32,
    delta: [f64; 3],
    on_ground: bool,
    max_up_step: f32,
) -> ([f64; 3], CollideResult) {
    let aabb = EntityAabb::from_dimensions(pos[0], pos[1], pos[2], width, height);
    let res = entity_collide_world(aabb, delta, on_ground, max_up_step, &[]);
    (
        [
            pos[0] + res.delta[0],
            pos[1] + res.delta[1],
            pos[2] + res.delta[2],
        ],
        res,
    )
}

/// 1.21.1 `EntityType.<clinit>` (`bsx.javap`) bounding-box `(width, height)`.
fn mob_dimensions(etype: i64) -> (f32, f32) {
    match etype {
        3 => (0.5, 1.975),  // armor_stand
        6 => (0.5, 0.9),    // bat
        16 => (0.7, 0.5),   // cave_spider
        19 => (0.4, 0.7),   // chicken
        22 => (0.9, 1.4),   // cow
        23 => (0.6, 1.7),   // creeper
        77 => (0.9, 0.9),   // pig
        87 => (0.9, 1.3),   // sheep
        91 => (0.6, 1.99),  // skeleton
        100 => (1.4, 0.9),  // spider
        124 => (0.6, 1.95), // zombie
        _ => (0.6, 1.8),
    }
}

fn mob_has_support(etype: i64, x: f64, y: f64, z: f64) -> bool {
    let (w, h) = mob_dimensions(etype);
    let aabb = EntityAabb::from_dimensions(x, y, z, w, h);
    entity_collide_world(aabb, [0.0, -1e-4, 0.0], false, 0.0, &[]).vertical_collision_below
}

fn mob_collide_move(m: &Arc<Mob>, mp: &mut (f64, f64, f64, f32, f32), dx: f64, dz: f64) {
    let (w, h) = mob_dimensions(m.etype);
    let on_ground = mob_has_support(m.etype, mp.0, mp.1, mp.2);
    // 1.21.1 Attributes.STEP_HEIGHT (`buw.B`): default 0.6f.
    let (next_pos, res) =
        step_entity_move([mp.0, mp.1, mp.2], w, h, [dx, 0.0, dz], on_ground, 0.6);
    mp.0 = next_pos[0];
    mp.1 = next_pos[1];
    mp.2 = next_pos[2];
    if !res.stepped_up && !mob_has_support(m.etype, mp.0, mp.1, mp.2) {
        mob_snap_y(m, mp);
    }
}

#[cfg(test)]
mod entity_collision_tests {
    use super::*;

    struct TestWorldSnapshot {
        world: HashMap<DimChunkKey, HashMap<u32, u32>>,
    }

    impl TestWorldSnapshot {
        fn take() -> Self {
            Self {
                world: std::mem::take(&mut *world().lock().unwrap()),
            }
        }
    }

    impl std::ops::Drop for TestWorldSnapshot {
        fn drop(&mut self) {
            *world().lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.world);
        }
    }

    fn slice_boxes<'a>(
        all: &'a [([f64; 3], [f64; 3])],
        sweep: EntityAabb,
    ) -> Vec<([f64; 3], [f64; 3])> {
        let smin = sweep.min();
        let smax = sweep.max();
        all.iter()
            .copied()
            .filter(|&b| aabb_overlaps((smin, smax), b))
            .collect()
    }

    #[test]
    fn entity_collide_axis_priority_z_first_when_x_lt_z_and_x_first_on_tie() {
        let pillar = [([1.0, 0.0, 1.0], [2.0, 1.0, 2.0])];
        let aabb = EntityAabb::from_min_max([0.0, 0.0, 0.0], [1.0, 1.0, 1.0]);

        // |x| < |z| -> Z resolves first (0.5), then X is blocked at corner (0.0).
        let res_z = entity_collide(aabb, [0.2, 0.0, 0.5], true, 0.0, &[], |s| {
            slice_boxes(&pillar, s)
        });
        assert_eq!(res_z.delta, [0.0, 0.0, 0.5]);
        assert!(res_z.horizontal_collision);

        // |x| > |z| -> X resolves first (0.5), then Z is blocked at corner (0.0).
        let res_x = entity_collide(aabb, [0.5, 0.0, 0.2], true, 0.0, &[], |s| {
            slice_boxes(&pillar, s)
        });
        assert_eq!(res_x.delta, [0.5, 0.0, 0.0]);
        assert!(res_x.horizontal_collision);

        // |x| == |z| tie -> `abs(x) < abs(z)` is false, so X resolves first!
        let res_tie = entity_collide(aabb, [0.5, 0.0, 0.5], true, 0.0, &[], |s| {
            slice_boxes(&pillar, s)
        });
        assert_eq!(res_tie.delta, [0.5, 0.0, 0.0]);
        assert!(res_tie.horizontal_collision);

        // Negative direction mirror: corner at [-1, 0, -1]..[0, 1, 0].
        let neg_pillar = [([-1.0, 0.0, -1.0], [0.0, 1.0, 0.0])];
        let res_neg_z = entity_collide(aabb, [-0.2, 0.0, -0.5], true, 0.0, &[], |s| {
            slice_boxes(&neg_pillar, s)
        });
        assert_eq!(res_neg_z.delta, [0.0, 0.0, -0.5]);
        let res_neg_tie = entity_collide(aabb, [-0.5, 0.0, -0.5], true, 0.0, &[], |s| {
            slice_boxes(&neg_pillar, s)
        });
        assert_eq!(res_neg_tie.delta, [-0.5, 0.0, 0.0]);
    }

    #[test]
    fn entity_collide_step_up_over_slab_and_refuses_full_block_when_max_up_step_0_6() {
        crate::test_support::require_vanilla_data();
        let _guard = world_guard();
        let _snap = TestWorldSnapshot::take();
        let reg = crate::block_registry::breg().unwrap();
        let stone = name_to_state("stone").unwrap();
        let slab = reg
            .checked_state("minecraft:stone_slab", &[("type".into(), "bottom".into())])
            .unwrap();
        for x in -2..=4 {
            for z in -2..=2 {
                set_block(x, 63, z, stone);
                for y in 64..=68 {
                    set_block(x, y, z, 0);
                }
            }
        }
        set_block(1, 64, 0, slab);

        // Creeper (0.6 x 1.7) at x=0.5, y=64.0, z=0.5 moving +x=0.5 onto bottom slab (0.5 high).
        let (pos_slab, res_slab) =
            step_entity_move([0.5, 64.0, 0.5], 0.6, 1.7, [0.5, 0.0, 0.0], true, 0.6);
        assert!(res_slab.stepped_up);
        assert_eq!(res_slab.delta, [0.5, 0.5, 0.0]);
        assert!(!res_slab.horizontal_collision);
        assert!((pos_slab[1] - 64.5).abs() < 1e-9);

        // Exact max_up_step == 0.5 succeeds, 0.49 fails.
        let (_, res_exact) =
            step_entity_move([0.5, 64.0, 0.5], 0.6, 1.7, [0.5, 0.0, 0.0], true, 0.5);
        assert!(res_exact.stepped_up);
        let (_, res_short) =
            step_entity_move([0.5, 64.0, 0.5], 0.6, 1.7, [0.5, 0.0, 0.0], true, 0.49);
        assert!(!res_short.stepped_up);
        assert!(res_short.horizontal_collision);
        assert!((res_short.delta[0] - 0.2).abs() < 1e-6);

        // Airborne without downward collision does NOT step up.
        let (_, res_air) =
            step_entity_move([0.5, 64.0, 0.5], 0.6, 1.7, [0.5, 0.0, 0.0], false, 0.6);
        assert!(!res_air.stepped_up);
        assert!(res_air.horizontal_collision);

        // Falling onto floor (`vertical_collision_below`) triggers step-up and adds `vec3.y` (-0.05).
        let (pos_fall_step, res_fall_step) =
            step_entity_move([0.5, 64.05, 0.5], 0.6, 1.7, [0.5, -0.1, 0.0], false, 0.6);
        assert!(res_fall_step.stepped_up);
        assert!(res_fall_step.vertical_collision_below);
        assert!((pos_fall_step[0] - 1.0).abs() < 1e-9);
        assert!((pos_fall_step[1] - 64.5).abs() < 1e-9);

        // Full 1.0-block stone wall at (1, 64, 0) is refused when max_up_step == 0.6.
        set_block(1, 64, 0, stone);
        let (pos_wall, res_wall) =
            step_entity_move([0.5, 64.0, 0.5], 0.6, 1.7, [0.5, 0.0, 0.0], true, 0.6);
        assert!(!res_wall.stepped_up);
        assert!(res_wall.horizontal_collision);
        assert!((pos_wall[0] - 0.7).abs() < 1e-6, "pos_wall={pos_wall:?}");
        assert_eq!(pos_wall[1], 64.0);
    }

    #[test]
    fn entity_collide_step_up_blocked_by_low_ceiling() {
        crate::test_support::require_vanilla_data();
        let _guard = world_guard();
        let _snap = TestWorldSnapshot::take();
        let reg = crate::block_registry::breg().unwrap();
        let stone = name_to_state("stone").unwrap();
        let slab = reg
            .checked_state("minecraft:stone_slab", &[("type".into(), "bottom".into())])
            .unwrap();
        for x in -2..=4 {
            for z in -2..=2 {
                set_block(x, 63, z, stone);
                for y in 64..=69 {
                    set_block(x, y, z, 0);
                }
            }
        }
        set_block(1, 64, 0, slab);
        // Ceiling at y=66 leaves 66.0 - 64.5 = 1.5 headroom < creeper height 1.7.
        set_block(1, 66, 0, stone);

        let (pos_low, res_low) =
            step_entity_move([0.5, 64.0, 0.5], 0.6, 1.7, [0.5, 0.0, 0.0], true, 0.6);
        assert!(!res_low.stepped_up);
        assert!(res_low.horizontal_collision);
        assert_eq!(pos_low[1], 64.0);
        assert!((pos_low[0] - 0.7).abs() < 1e-6);

        // Moving ceiling to y=67 gives 2.5 headroom >= 1.7, so step-up succeeds.
        set_block(1, 66, 0, 0);
        set_block(1, 67, 0, stone);
        let (pos_high, res_high) =
            step_entity_move([0.5, 64.0, 0.5], 0.6, 1.7, [0.5, 0.0, 0.0], true, 0.6);
        assert!(res_high.stepped_up);
        assert!(!res_high.horizontal_collision);
        assert!((pos_high[0] - 1.0).abs() < 1e-9);
        assert!((pos_high[1] - 64.5).abs() < 1e-9);
    }

    #[test]
    fn entity_collide_fence_1_5_block_prevents_step_over_and_catches_window() {
        crate::test_support::require_vanilla_data();
        let _guard = world_guard();
        let _snap = TestWorldSnapshot::take();
        let reg = crate::block_registry::breg().unwrap();
        let stone = name_to_state("stone").unwrap();
        let oak_fence = name_to_state("oak_fence").unwrap();
        let slab = reg
            .checked_state("minecraft:stone_slab", &[("type".into(), "bottom".into())])
            .unwrap();
        for x in -2..=4 {
            for z in -2..=2 {
                set_block(x, 63, z, stone);
                for y in 64..=68 {
                    set_block(x, y, z, 0);
                }
            }
        }
        set_block(1, 64, 0, oak_fence);

        // Cow (0.9 x 1.4) at y=64.0 cannot step over 1.5-block oak_fence (max_up_step = 0.6).
        let (pos_ground, res_ground) =
            step_entity_move([0.5, 64.0, 0.5], 0.9, 1.4, [0.6, 0.0, 0.0], true, 0.6);
        assert!(!res_ground.stepped_up);
        assert!(res_ground.horizontal_collision);
        assert!(pos_ground[0] < 1.0, "cow must stop before fence: {pos_ground:?}");

        // BlockCollisions `by0 = floor(minY - 1e-7) - 1` window:
        // Entity standing on a slab at y=65.2 (above by=64, so floor(minY)=65) still collides
        // horizontally with the 1.5-block fence at by=64 (which extends up to y=65.5)!
        set_block(0, 64, 0, stone);
        set_block(0, 65, 0, slab); // top at 65.5, entity feet at 65.2..65.4
        let (pos_above, res_above) =
            step_entity_move([0.5, 65.2, 0.5], 0.6, 1.7, [0.6, 0.0, 0.0], true, 0.0);
        assert!(res_above.horizontal_collision);
        assert!(
            pos_above[0] <= 1.08,
            "fence at by=64 (top 65.5) must block entity at minY=65.2: {pos_above:?}"
        );
    }

    #[test]
    fn entity_collide_corner_multi_box_sliding_and_zero_1e7_tunneling() {
        // Multi-box corridor + corner: wall along X=[1..2], Z=[-1..1] and wall along Z=[1..2], X=[-1..0].
        let boxes = [
            ([1.0, 0.0, -1.0], [2.0, 2.0, 1.0]),
            ([-1.0, 0.0, 1.0], [0.0, 2.0, 2.0]),
        ];
        let aabb = EntityAabb::from_min_max([0.2, 0.0, 0.2], [0.8, 1.8, 0.8]);
        // Moving (+0.5, 0, +0.5): hits X=1.0 wall (clipped to 0.2) and slides full +0.5 along Z!
        let res_slide = entity_collide(aabb, [0.5, 0.0, 0.5], true, 0.0, &[], |s| {
            slice_boxes(&boxes, s)
        });
        assert!((res_slide.delta[0] - 0.2).abs() < 1e-12);
        assert!((res_slide.delta[2] - 0.5).abs() < 1e-12);
        assert!(res_slide.horizontal_collision);

        // Zero 1e-7 tunneling: high-speed move [5.0, 0, 0] stops flush at wall X=1.0.
        let wall = [([1.0, 0.0, 0.0], [2.0, 2.0, 1.0])];
        let res_fast = entity_collide(aabb, [5.0, 0.0, 0.0], true, 0.0, &[], |s| {
            slice_boxes(&wall, s)
        });
        assert!((res_fast.delta[0] - 0.2).abs() < 1e-12);
        assert!(aabb.max_x + res_fast.delta[0] <= 1.0 + 1e-12);

        // Sub-1e-7 movement with empty sweep returns delta untouched (`collideWithShapes` isEmpty),
        // whereas when a shape overlaps the sweep box, `Shapes.collide` snaps < 1e-7 to 0.0.
        let res_tiny_empty = entity_collide(aabb, [5e-8, 0.0, 0.0], true, 0.0, &[], |s| {
            slice_boxes(&wall, s)
        });
        assert_eq!(res_tiny_empty.delta[0], 5e-8);
        assert!(!res_tiny_empty.horizontal_collision);

        let near_wall = [([0.8 + 1e-8, 0.0, 0.0], [2.0, 2.0, 1.0])];
        let res_tiny = entity_collide(aabb, [5e-8, 0.0, 0.0], true, 0.0, &[], |s| {
            slice_boxes(&near_wall, s)
        });
        assert_eq!(res_tiny.delta[0], 0.0);
        // `Mth.equal(5e-8, 0.0)` is true (< 1e-5f), so horizontal_collision stays false.
        assert!(!res_tiny.horizontal_collision);

        // Perpendicular overlap <= 1e-7 does NOT block, > 1e-7 DOES block.
        let touch_wall = [([1.0, 0.0, 0.8 - 0.5e-7], [2.0, 2.0, 2.0])];
        let res_perp_clear = entity_collide(aabb, [0.5, 0.0, 0.0], true, 0.0, &[], |s| {
            slice_boxes(&touch_wall, s)
        });
        assert_eq!(res_perp_clear.delta[0], 0.5);
        let overlap_wall = [([1.0, 0.0, 0.8 - 2e-7], [2.0, 2.0, 2.0])];
        let res_perp_hit = entity_collide(aabb, [0.5, 0.0, 0.0], true, 0.0, &[], |s| {
            slice_boxes(&overlap_wall, s)
        });
        assert!((res_perp_hit.delta[0] - 0.2).abs() < 1e-12);

        // `vertical_collision` uses exact `!=` even for < 1e-5f movements.
        let floor = [([0.0, -1.0, 0.0], [1.0, 0.0, 1.0])];
        let res_v = entity_collide(aabb, [0.0, -5e-6, 0.0], false, 0.0, &[], |s| {
            slice_boxes(&floor, s)
        });
        assert_eq!(res_v.delta[1], 0.0);
        assert!(res_v.vertical_collision);
        assert!(res_v.vertical_collision_below);
    }

    #[test]
    fn entity_collide_mob_tick_integration_cow_creeper_sheep() {
        crate::test_support::require_vanilla_data();
        let _guard = world_guard();
        let _snap = TestWorldSnapshot::take();
        let reg = crate::block_registry::breg().unwrap();
        let stone = name_to_state("stone").unwrap();
        let slab = reg
            .checked_state("minecraft:stone_slab", &[("type".into(), "bottom".into())])
            .unwrap();
        let oak_fence = name_to_state("oak_fence").unwrap();
        for x in -4..=8 {
            for z in -4..=4 {
                set_block(x, 63, z, stone);
                for y in 64..=69 {
                    set_block(x, y, z, 0);
                }
            }
        }
        // Slab step at x=1, z=0; full wall at x=1, z=2; fence at x=3, z=0.
        set_block(1, 64, 0, slab);
        set_block(1, 64, 2, stone);
        set_block(3, 64, 0, oak_fence);

        let make_mob = |eid: i32, etype: i64, x: f64, y: f64, z: f64| {
            Arc::new(Mob {
                eid,
                uuid: [eid as u8; 16],
                etype,
                pos: Mutex::new((x, y, z, 0.0, 0.0)),
                hp: Mutex::new(20.0),
                last_hit: Mutex::new(Instant::now() - Duration::from_secs(10)),
                hurt_at: Mutex::new(Instant::now() - Duration::from_secs(10)),
                last_hurt_tick: Mutex::new(None),
                vel: Mutex::new([0.0; 3]),
                fuse: Mutex::new(0),
                love: Mutex::new(0),
                air: Mutex::new(server_ticks(AIR_SUPPLY_GT) as i32),
            })
        };

        // Creeper (0.6x1.7) steps onto slab [1..2] (x=1.5, y=64.5) in 4 steps of +0.25.
        let creeper = make_mob(9001, 23, 0.5, 64.0, 0.5);
        for _ in 0..4 {
            let mut mp = creeper.pos.lock().unwrap();
            mob_collide_move(&creeper, &mut mp, 0.25, 0.0);
        }
        let cp = *creeper.pos.lock().unwrap();
        assert!((cp.0 - 1.5).abs() < 1e-6 && (cp.1 - 64.5).abs() < 1e-6, "creeper must step onto slab (1.5, 64.5), got {cp:?}");

        // Continue moving east off the slab into oak_fence at x=3..4: fence stops creeper before x=3.0.
        for _ in 0..16 {
            let mut mp = creeper.pos.lock().unwrap();
            mob_collide_move(&creeper, &mut mp, 0.25, 0.0);
        }
        let cp_fence = *creeper.pos.lock().unwrap();
        assert!(
            cp_fence.0 <= 3.08 && (cp_fence.1 - 64.0).abs() < 1e-6,
            "creeper (half_w=0.3) must stop flush at 1.5-block fence post (x_min=3.375): {cp_fence:?}"
        );

        // Sheep (0.9x1.3) resting on slab at 64.5 stays at 64.5 across mob_snap_y (no false snap to 65.0).
        let sheep = make_mob(9002, 87, 1.5, 64.5, 0.5);
        {
            let mut mp = sheep.pos.lock().unwrap();
            mob_snap_y(&sheep, &mut mp);
            assert!((mp.1 - 64.5).abs() < 1e-6, "sheep on slab must stay at 64.5: {mp:?}");
        }

        // Cow (0.9x1.4) blocked by 1.0-tall stone wall at (1, 64, 2).
        let cow = make_mob(9003, 22, 0.3, 64.0, 2.5);
        for _ in 0..6 {
            let mut mp = cow.pos.lock().unwrap();
            mob_collide_move(&cow, &mut mp, 0.28, 0.0);
        }
        let cow_pos = *cow.pos.lock().unwrap();
        assert!(
            cow_pos.0 <= 0.56,
            "cow (half_w=0.45) must stop before 1.0-block wall at x=1.0: {cow_pos:?}"
        );
        assert_eq!(cow_pos.1, 64.0);
    }
}
