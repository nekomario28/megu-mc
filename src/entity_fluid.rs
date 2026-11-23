// 1.21.1 Fluid Height, Eye Immersion, Fluid Current Push & Lava Contact
// Pinned jar SHA-1: 59353fb40c36d304f2035d51e7d6e6baa98dc05c
// Official mappings: 03f8985492bda0afc0898465341eb0acef35f570
//   net.minecraft.world.level.material.FlowingFluid   -> epc
//   net.minecraft.world.level.material.Fluid          -> epd
//   net.minecraft.world.level.material.FluidState     -> epe
//   net.minecraft.world.level.material.EmptyFluid     -> epb
//   net.minecraft.world.level.material.LavaFluid      -> eph
//   net.minecraft.world.level.material.WaterFluid     -> epk
//   net.minecraft.world.level.block.LiquidBlock       -> dko
//   net.minecraft.world.level.block.IceBlock          -> djw
//   net.minecraft.world.entity.Entity                 -> bsr
//   net.minecraft.world.entity.LivingEntity           -> btn
//   net.minecraft.world.entity.player.Player          -> cmx
//   net.minecraft.world.entity.EntityType             -> bsx
//   net.minecraft.world.entity.EntityDimensions       -> bsu
//   net.minecraft.world.phys.AABB                     -> ewx
//   net.minecraft.world.phys.Vec3                     -> exc

// `Entity.bn()` (`bsr.bn()`, offsets 33–42) pushes every fluid except lava at `0.014`.
const WATER_MOTION_SCALE: f64 = 0.014;
// `Entity.bm()` (`bsr.bm()`, offsets 9–35): `dimensionType().ultraWarm() ? 0.007 : 0.0023333333333333335`.
// The pinned bytecode puts the *larger* push in the ultra-warm (Nether) dimension; the
// previous constants here were named the other way round, which is only observable once the
// dimension is wired (Slice 4d), so the names follow the bytecode now.
const LAVA_ULTRA_WARM_MOTION_SCALE: f64 = 0.007;
const LAVA_TEMPERATE_MOTION_SCALE: f64 = 0.0023333333333333335;
const FLUID_PUSH_MIN_SPEED: f64 = 0.0045000000000000005;
const FLUID_VELOCITY_THRESHOLD: f64 = 0.003;

impl EntityAabb {
    /// 1.21.1 `AABB.deflate(double, double, double)` (`ewx.f(DDD)`).
    fn deflate_xyz(self, dx: f64, dy: f64, dz: f64) -> Self {
        self.inflate(-dx, -dy, -dz)
    }

    /// 1.21.1 `AABB.deflate(double)` (`ewx.h(D)`).
    fn deflate(self, d: f64) -> Self {
        self.deflate_xyz(d, d, d)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EntityFluidKind {
    Water,
    Lava,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct EntityFluidState {
    kind: EntityFluidKind,
    amount: u8,
    falling: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct FluidPushResult {
    in_fluid: bool,
    max_height: f64,
    push: [f64; 3],
}

/// Maps a block state ID to its 1.21.1 `FluidState` (`LiquidBlock.getFluidState` `dko.b_`,
/// `SimpleWaterloggedBlock`, and always-waterlogged aquatic blocks via `BReg::has_water`).
fn entity_fluid_state_of(st: u32) -> Option<EntityFluidState> {
    if let Some(f) = fluid_from_block(st) {
        let kind = match f.kind {
            flowing_fluid::Kind::Water => EntityFluidKind::Water,
            flowing_fluid::Kind::Lava => EntityFluidKind::Lava,
        };
        let (amount, falling) = if f.level == 0 {
            (8, false)
        } else if f.level < 8 {
            (8 - f.level, false)
        } else {
            (8, true)
        };
        return Some(EntityFluidState {
            kind,
            amount,
            falling,
        });
    }
    if let Some(reg) = crate::block_registry::breg() {
        if reg.has_water(st) {
            return Some(EntityFluidState {
                kind: EntityFluidKind::Water,
                amount: 8,
                falling: false,
            });
        }
    }
    None
}

/// 1.21.1 `FlowingFluid.getOwnHeight(FluidState)` (`epc.a(Lepe;)F`, offsets 0–9).
fn fluid_own_height(fs: EntityFluidState) -> f32 {
    (fs.amount as f32) / 9.0f32
}

/// 1.21.1 `FlowingFluid.hasSameAbove(FluidState, BlockGetter, BlockPos)` (`epc.c(Lepe;Ldcc;Ljd;)Z`, offsets 0–20).
fn fluid_has_same_above_with(
    kind: EntityFluidKind,
    x: i32,
    y: i32,
    z: i32,
    get_block: &impl Fn(i32, i32, i32) -> u32,
) -> bool {
    entity_fluid_state_of(get_block(x, y + 1, z)).is_some_and(|above| above.kind == kind)
}

/// 1.21.1 `FlowingFluid.getHeight(FluidState, BlockGetter, BlockPos)` (`epc.a(Lepe;Ldcc;Ljd;)F`, offsets 0–15).
fn fluid_height_with(
    x: i32,
    y: i32,
    z: i32,
    get_block: &impl Fn(i32, i32, i32) -> u32,
) -> Option<(EntityFluidKind, f32)> {
    let fs = entity_fluid_state_of(get_block(x, y, z))?;
    let h = if fluid_has_same_above_with(fs.kind, x, y, z, get_block) {
        1.0f32
    } else {
        fluid_own_height(fs)
    };
    Some((fs.kind, h))
}

/// 1.21.1 `Vec3.normalize()` (`exc.d()`, offsets 0–70): returns zero vector when length < 1e-4.
fn fluid_vec3_normalize(v: [f64; 3]) -> [f64; 3] {
    let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    if len < 1.0e-4 {
        [0.0, 0.0, 0.0]
    } else {
        [v[0] / len, v[1] / len, v[2] / len]
    }
}

fn fluid_vec3_len(v: [f64; 3]) -> f64 {
    (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt()
}

fn fluid_blocks_motion(st: u32) -> bool {
    if let Some(reg) = crate::block_registry::breg() {
        reg.blocks_motion(st)
    } else {
        st != 0 && entity_fluid_state_of(st).is_none()
    }
}

/// 1.21.1 `FlowingFluid.isSolidFace(BlockGetter, BlockPos, Direction)` (`epc.a(Ldcc;Ljd;Lji;)Z`, offsets 0–62).
fn fluid_is_solid_face(
    x: i32,
    y: i32,
    z: i32,
    dir_idx: u8,
    self_kind: EntityFluidKind,
    get_block: &impl Fn(i32, i32, i32) -> u32,
) -> bool {
    let st = get_block(x, y, z);
    if entity_fluid_state_of(st).is_some_and(|nfs| nfs.kind == self_kind) {
        return false;
    }
    if dir_idx == 1 {
        return true;
    }
    if let Some(reg) = crate::block_registry::breg() {
        if matches!(reg.name_of(st), "minecraft:ice" | "minecraft:frosted_ice") {
            return false;
        }
        reg.is_face_sturdy(st, dir_idx, crate::block_registry::Support::Full)
    } else {
        st != 0
    }
}

/// 1.21.1 `FlowingFluid.getFlow(BlockGetter, BlockPos, FluidState)` (`epc.a(Ldcc;Ljd;Lepe;)Lexc;`, offsets 0–340).
fn fluid_flow_with(
    x: i32,
    y: i32,
    z: i32,
    fs: EntityFluidState,
    get_block: &impl Fn(i32, i32, i32) -> u32,
) -> [f64; 3] {
    // Direction.Plane.HORIZONTAL: (dir_idx, step_x, step_z) for NORTH(2), EAST(5), SOUTH(3), WEST(4).
    const HORIZ: [(u8, i32, i32); 4] = [(2, 0, -1), (5, 1, 0), (3, 0, 1), (4, -1, 0)];
    let own_h = fluid_own_height(fs);
    let mut flow_x = 0.0f64;
    let mut flow_z = 0.0f64;

    for &(_dir_idx, dx, dz) in &HORIZ {
        let (nx, nz) = (x + dx, z + dz);
        let neighbor_st = get_block(nx, y, nz);
        let neighbor_fs = entity_fluid_state_of(neighbor_st);
        // FlowingFluid.affectsFlow (`epc.g`): empty or same fluid type.
        if let Some(nfs) = neighbor_fs {
            if nfs.kind != fs.kind {
                continue;
            }
        }
        let mut f = neighbor_fs.map_or(0.0f32, fluid_own_height);
        let mut g = 0.0f32;
        if f == 0.0f32 {
            if !fluid_blocks_motion(neighbor_st) {
                let below_fs = entity_fluid_state_of(get_block(nx, y - 1, nz));
                let below_affects = below_fs.is_none_or(|bfs| bfs.kind == fs.kind);
                if below_affects {
                    f = below_fs.map_or(0.0f32, fluid_own_height);
                    if f > 0.0f32 {
                        g = own_h - (f - 0.8888889f32);
                    }
                }
            }
        } else if f > 0.0f32 {
            g = own_h - f;
        }
        if g != 0.0f32 {
            flow_x += f64::from((dx as f32) * g);
            flow_z += f64::from((dz as f32) * g);
        }
    }

    let mut vec = [flow_x, 0.0, flow_z];
    if fs.falling {
        for &(dir_idx, dx, dz) in &HORIZ {
            let (nx, nz) = (x + dx, z + dz);
            if fluid_is_solid_face(nx, y, nz, dir_idx, fs.kind, get_block)
                || fluid_is_solid_face(nx, y + 1, nz, dir_idx, fs.kind, get_block)
            {
                let n = fluid_vec3_normalize(vec);
                vec = [n[0], n[1] - 6.0, n[2]];
                break;
            }
        }
    }
    fluid_vec3_normalize(vec)
}

/// `bsr.a(Lawu;D)Z` offsets 305–431: average the accumulated flows, normalize
/// for non-players, scale by `d`, then apply the stationary min-speed frame.
fn fluid_push_finalize(
    acc: [f64; 3],
    count: i32,
    is_player: bool,
    motion_scale: f64,
    vel: [f64; 3],
) -> [f64; 3] {
    let mut push = [0.0f64; 3];
    if fluid_vec3_len(acc) > 0.0 {
        let mut v = acc;
        if count > 0 {
            let inv = 1.0 / f64::from(count);
            v = [v[0] * inv, v[1] * inv, v[2] * inv];
        }
        if !is_player {
            v = fluid_vec3_normalize(v);
        }
        v = [
            v[0] * motion_scale,
            v[1] * motion_scale,
            v[2] * motion_scale,
        ];
        if vel[0].abs() < FLUID_VELOCITY_THRESHOLD
            && vel[2].abs() < FLUID_VELOCITY_THRESHOLD
            && fluid_vec3_len(v) < FLUID_PUSH_MIN_SPEED
        {
            let vn = fluid_vec3_normalize(v);
            v = [
                vn[0] * FLUID_PUSH_MIN_SPEED,
                vn[1] * FLUID_PUSH_MIN_SPEED,
                vn[2] * FLUID_PUSH_MIN_SPEED,
            ];
        }
        push = v;
    }
    push
}

/// Accumulate `in_fluid`/`max_height` (and, when `is_pushed_by_fluid`, the flow
/// sum) over the `(bx, by, bz, fs)` cells of one deflated-AABB sweep whose kind
/// matches `kind`. `cells` must be in the sweep's (x, y, z) visit order so the
/// flow accumulation is ordered exactly as the boxed loop did it.
#[allow(clippy::too_many_arguments)]
fn fluid_result_from_cells(
    cells: &[(i32, i32, i32, EntityFluidState)],
    deflated: EntityAabb,
    kind: EntityFluidKind,
    motion_scale: f64,
    is_pushed_by_fluid: bool,
    is_player: bool,
    vel: [f64; 3],
    get_block: &impl Fn(i32, i32, i32) -> u32,
) -> FluidPushResult {
    let mut max_height = 0.0f64;
    let mut found = false;
    let mut acc = [0.0f64; 3];
    let mut count = 0i32;
    for &(bx, by, bz, fs) in cells {
        if fs.kind != kind {
            continue;
        }
        let h = if fluid_has_same_above_with(fs.kind, bx, by, bz, get_block) {
            1.0f32
        } else {
            fluid_own_height(fs)
        };
        let e = f64::from(by as f32 + h);
        if e >= deflated.min_y {
            found = true;
            max_height = max_height.max(e - deflated.min_y);
            if is_pushed_by_fluid {
                let mut flow = fluid_flow_with(bx, by, bz, fs, get_block);
                if max_height < 0.4 {
                    flow = [
                        flow[0] * max_height,
                        flow[1] * max_height,
                        flow[2] * max_height,
                    ];
                }
                acc[0] += flow[0];
                acc[1] += flow[1];
                acc[2] += flow[2];
                count += 1;
            }
        }
    }
    FluidPushResult {
        in_fluid: found,
        max_height,
        push: fluid_push_finalize(acc, count, is_player, motion_scale, vel),
    }
}

/// 1.21.1 `Entity.updateFluidHeightAndDoFluidPushing(TagKey<Fluid>, double)` (`bsr.a(Lawu;D)Z`, offsets 0–431).
fn update_fluid_height_and_push_with(
    aabb: EntityAabb,
    kind: EntityFluidKind,
    motion_scale: f64,
    is_pushed_by_fluid: bool,
    is_player: bool,
    vel: [f64; 3],
    get_block: &impl Fn(i32, i32, i32) -> u32,
) -> FluidPushResult {
    let deflated = aabb.deflate(0.001);
    let x0 = deflated.min_x.floor() as i32;
    let x1 = deflated.max_x.ceil() as i32;
    let y0 = deflated.min_y.floor() as i32;
    let y1 = deflated.max_y.ceil() as i32;
    let z0 = deflated.min_z.floor() as i32;
    let z1 = deflated.max_z.ceil() as i32;

    let mut max_height = 0.0f64;
    let mut found = false;
    let mut acc = [0.0f64; 3];
    let mut count = 0i32;

    for bx in x0..x1 {
        for by in y0..y1 {
            for bz in z0..z1 {
                let st = get_block(bx, by, bz);
                let Some(fs) = entity_fluid_state_of(st) else {
                    continue;
                };
                if fs.kind != kind {
                    continue;
                }
                let h = if fluid_has_same_above_with(fs.kind, bx, by, bz, get_block) {
                    1.0f32
                } else {
                    fluid_own_height(fs)
                };
                let e = f64::from(by as f32 + h);
                if e >= deflated.min_y {
                    found = true;
                    max_height = max_height.max(e - deflated.min_y);
                    if is_pushed_by_fluid {
                        let mut flow = fluid_flow_with(bx, by, bz, fs, get_block);
                        if max_height < 0.4 {
                            flow = [
                                flow[0] * max_height,
                                flow[1] * max_height,
                                flow[2] * max_height,
                            ];
                        }
                        acc[0] += flow[0];
                        acc[1] += flow[1];
                        acc[2] += flow[2];
                        count += 1;
                    }
                }
            }
        }
    }

    FluidPushResult {
        in_fluid: found,
        max_height,
        push: fluid_push_finalize(acc, count, is_player, motion_scale, vel),
    }
}

/// 1.21.1 `Entity.updateFluidOnEyes()` (`bsr.H()`, offsets 20–155):
/// `double d = getEyeY(); BlockPos pos = BlockPos.containing(x, d, z); (float)pos.y + fluidHeight > d`.
fn eye_in_fluid_with(
    x: f64,
    y: f64,
    z: f64,
    eye_height: f32,
    kind: EntityFluidKind,
    get_block: &impl Fn(i32, i32, i32) -> u32,
) -> bool {
    let eye_y = y + f64::from(eye_height);
    let (bx, by, bz) = (x.floor() as i32, eye_y.floor() as i32, z.floor() as i32);
    let Some((fk, h)) = fluid_height_with(bx, by, bz, get_block) else {
        return false;
    };
    fk == kind && f64::from(by as f32 + h) > eye_y
}

fn eye_in_bubble_column_with(
    x: f64,
    y: f64,
    z: f64,
    eye_height: f32,
    get_block: &impl Fn(i32, i32, i32) -> u32,
) -> bool {
    let eye_y = y + f64::from(eye_height);
    let (bx, by, bz) = (x.floor() as i32, eye_y.floor() as i32, z.floor() as i32);
    let st = get_block(bx, by, bz);
    crate::block_registry::breg().is_some_and(|reg| reg.name_of(st) == "minecraft:bubble_column")
}

/// 1.21.1 `Entity.isInLava()` (`bsr.bt()`, offsets 0–29):
/// `!firstTick && fluidHeight.getDouble(FluidTags.LAVA) > 0.0`, populated with `deflate(0.001)`.
fn aabb_in_lava_with(
    aabb: EntityAabb,
    get_block: &impl Fn(i32, i32, i32) -> u32,
) -> bool {
    update_fluid_height_and_push_with(
        aabb,
        EntityFluidKind::Lava,
        fluid_push_scale(EntityFluidKind::Lava, level_ultra_warm()),
        false,
        false,
        [0.0; 3],
        get_block,
    )
    .max_height
        > 0.0
}

fn aabb_in_water_with(
    aabb: EntityAabb,
    get_block: &impl Fn(i32, i32, i32) -> u32,
) -> bool {
    update_fluid_height_and_push_with(
        aabb,
        EntityFluidKind::Water,
        WATER_MOTION_SCALE,
        false,
        false,
        [0.0; 3],
        get_block,
    )
    .in_fluid
}

fn aabb_in_water(aabb: EntityAabb) -> bool {
    aabb_in_water_with(aabb, &block_at_cached)
}

fn aabb_in_lava(aabb: EntityAabb) -> bool {
    aabb_in_lava_with(aabb, &block_at_cached)
}

/// 1.21.1 `Player.<clinit>` (`cmx`) pose eye heights:
/// standing `1.62f`, crouching `1.27f`, fall-flying `0.4f`.
fn player_eye_height(sneaking: bool, fall_fly: bool) -> f32 {
    if fall_fly {
        0.4
    } else if sneaking {
        1.27
    } else {
        1.62
    }
}

fn player_bounding_box(x: f64, y: f64, z: f64, sneaking: bool, fall_fly: bool) -> EntityAabb {
    let height = if fall_fly {
        0.6
    } else if sneaking {
        1.5
    } else {
        1.8
    };
    EntityAabb::from_dimensions(x, y, z, 0.6, height)
}

#[cfg(test)]
#[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
fn player_eye_in_water(x: f64, y: f64, z: f64, sneaking: bool, fall_fly: bool) -> bool {
    let eh = player_eye_height(sneaking, fall_fly);
    eye_in_fluid_with(x, y, z, eh, EntityFluidKind::Water, &block_at_cached)
}

fn player_drowning_submerged(
    dim: DimId, x: f64, y: f64, z: f64, sneaking: bool, fall_fly: bool,
) -> bool {
    let eh = player_eye_height(sneaking, fall_fly);
    let get_block = |bx, by, bz| block_at_cached_dim(dim, bx, by, bz);
    eye_in_fluid_with(x, y, z, eh, EntityFluidKind::Water, &get_block)
        && !eye_in_bubble_column_with(x, y, z, eh, &get_block)
}

fn player_in_lava(dim: DimId, x: f64, y: f64, z: f64, sneaking: bool, fall_fly: bool) -> bool {
    aabb_in_lava_with(player_bounding_box(x, y, z, sneaking, fall_fly), &|bx, by, bz| {
        block_at_cached_dim(dim, bx, by, bz)
    })
}

/// 1.21.1 `EntityType.<clinit>` (`bsx.javap`) & `EntityDimensions.defaultEyeHeight` (`height * 0.85f`).
fn mob_eye_height(etype: i64) -> f32 {
    match etype {
        15 => 0.35,   // cat
        16 => 0.45,   // cave_spider
        19 => 0.644,  // chicken
        22 => 1.3,    // cow
        27 | 54 | 91 | 102 | 124 | 126 => 1.74, // drowned, husk, skeleton, stray, zombie, zombie_villager
        33 => 2.55,   // enderman
        48 | 101 => 0.4, // glow_squid, squid
        87 => 1.235,  // sheep
        100 => 0.65,  // spider
        113 | 118 => 1.62, // villager, witch
        122 => 0.68,  // wolf
        _ => mob_dimensions(etype).1 * 0.85,
    }
}

/// 1.21.1 `EntityType$Builder.fireImmune()` (`bsx$a.c`).
fn mob_fire_immune(etype: i64) -> bool {
    matches!(
        etype,
        8 | 31 | 45 | 67 | 88 | 103 | 112 | 116 | 119 | 120 | 123 | 127
    )
}

#[allow(dead_code)]
fn mob_eye_in_water(etype: i64, x: f64, y: f64, z: f64) -> bool {
    eye_in_fluid_with(x, y, z, mob_eye_height(etype), EntityFluidKind::Water, &block_at_cached)
}

fn mob_drowning_submerged(etype: i64, x: f64, y: f64, z: f64) -> bool {
    let eh = mob_eye_height(etype);
    eye_in_fluid_with(x, y, z, eh, EntityFluidKind::Water, &block_at_cached)
        && !eye_in_bubble_column_with(x, y, z, eh, &block_at_cached)
}

fn mob_in_water(etype: i64, x: f64, y: f64, z: f64) -> bool {
    let (w, h) = mob_dimensions(etype);
    aabb_in_water(EntityAabb::from_dimensions(x, y, z, w, h))
}

fn mob_in_lava(etype: i64, x: f64, y: f64, z: f64) -> bool {
    let (w, h) = mob_dimensions(etype);
    aabb_in_lava(EntityAabb::from_dimensions(x, y, z, w, h))
}

fn mob_water_state(etype: i64, x: f64, y: f64, z: f64) -> FluidPushResult {
    mob_water_state_vel(etype, [x, y, z], [0.0; 3])
}

#[allow(dead_code)]
fn mob_fluid_push_water(etype: i64, x: f64, y: f64, z: f64) -> [f64; 3] {
    mob_water_state(etype, x, y, z).push
}

// ---------- Slice 4d: `LivingEntity.travel` fluid branches ----------
//
// 1.21.1 `LivingEntity.travel(Vec3)` (`btn.a(Lexc;)V`, `Code:` offsets 0–1251),
// `LivingEntity.getFluidFallingAdjustedMovement(double, boolean, Vec3)`
// (`btn.a(DZLexc;)Lexc;`, offsets 0–100) and `Entity.getInputVector(Vec3, float, float)`
// (`bsr.b(Lexc;FF)Lexc;`, offsets 0–104). Every constant below is read out of the pinned
// jar's bytecode; no source is copied.

/// `Entity.ba()` gravity and the `MobEffects.SLOW_FALLING` override for a descending entity
/// (`btn.a` offsets 8–53).
const LIVING_GRAVITY: f64 = 0.08;
const SLOW_FALLING_GRAVITY: f64 = 0.01;
/// `LivingEntity.getWaterSlowDown()` (`btn.fh()`, constant `0.8f`), `isSprinting()` (`0.9f`)
/// and `MobEffects.DOLPHINS_GRACE` (`0.96f`, `bsb.D`).
const WATER_SLOW_DOWN: f32 = 0.8;
const WATER_SPRINT_SLOW_DOWN: f32 = 0.9;
const DOLPHINS_GRACE_SLOW_DOWN: f32 = 0.96;
/// `Attributes.WATER_MOVEMENT_EFFICIENCY` (`buw.E`) blends the slow-down toward `0.54600006f`.
const WATER_MOVEMENT_EFFICIENCY_SLOW_DOWN: f32 = 0.54600006;
/// `Vec3.multiply(f, 0.8F, f)` (`btn.a` offsets 255–269): the vertical factor is the
/// `float 0.8f` widened to double, not the `f` slow-down.
const WATER_VERTICAL_DRAG: f64 = 0.800000011920929;
/// `Vec3.multiply(0.5, 0.8F, 0.5)` / `Vec3.scale(0.5)` in the lava branch (offsets 416–469).
const LAVA_DRAG: f64 = 0.5;
/// `moveRelative(0.02F, movementInput)` in both fluid branches (offsets 195, 382).
const FLUID_MOVE_RELATIVE: f32 = 0.02;
/// `btn.a` offsets 219–253: a horizontal collision while climbing replaces `y` with `0.2d`.
const FLUID_CLIMBABLE_LIFT: f64 = 0.2;
/// `isFree` probe height `+ 0.6f - getY() + e` and escape lift `0.3f` (offsets 292–347).
const FLUID_FREE_PROBE: f64 = 0.6000000238418579;
const FLUID_FREE_LIFT: f64 = 0.30000001192092896;
/// `getFluidFallingAdjustedMovement`: `d / 16.0`, probe `0.005d`, terminal `0.003d`.
const FLUID_FALLING_DIVISOR: f64 = 16.0;
const FLUID_FALLING_PROBE: f64 = 0.005;
const FLUID_FALLING_TERMINAL: f64 = 0.003;
/// Lava gravity term `-d / 4.0` (offsets 472–494).
const LAVA_GRAVITY_DIVISOR: f64 = 4.0;
/// `Mth.sin`/`Mth.cos` argument scale `0.017453292f` (`getInputVector`).
const DEG_TO_RAD: f32 = 0.017453292;
/// `getInputVector` treats an input shorter than `1.0E-7` as zero and normalizes above `1.0`.
const INPUT_VECTOR_EPSILON: f64 = 1.0e-7;

/// 1.21.1 `DimensionType.ultraWarm()` (`dwy.i()`), read by `Entity.bm()` to pick the lava
/// push scale. The thread that hosts this server owns the Overworld
/// (`dimension_type/overworld.json`, `ultra_warm=false`); keeping it as an owner value is what
/// makes the Nether scale reachable instead of a second copy of the number.
static LEVEL_ULTRA_WARM: AtomicBool = AtomicBool::new(false);

fn level_ultra_warm() -> bool {
    LEVEL_ULTRA_WARM.load(Ordering::Relaxed)
}

#[cfg(test)]
#[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
fn set_level_ultra_warm(v: bool) {
    LEVEL_ULTRA_WARM.store(v, Ordering::Relaxed);
}

/// `Entity.bn()` (water `0.014`) and `Entity.bm()` (lava `ultraWarm() ? 0.007 :
/// 0.0023333333333333335`) motion scales.
fn fluid_push_scale(kind: EntityFluidKind, ultra_warm: bool) -> f64 {
    match kind {
        EntityFluidKind::Water => WATER_MOTION_SCALE,
        EntityFluidKind::Lava => {
            if ultra_warm {
                LAVA_ULTRA_WARM_MOTION_SCALE
            } else {
                LAVA_TEMPERATE_MOTION_SCALE
            }
        }
    }
}

/// `Entity.getFluidJumpThreshold()` (`bsr.di()`, offsets 0–19): `eyeHeight < 0.4F ? 0.0 : 0.4`.
fn fluid_jump_threshold(eye_height: f32) -> f64 {
    if eye_height < 0.4 {
        0.0
    } else {
        0.4
    }
}

/// `btn.a` offsets 8–53: `d = getGravity()`, lowered to `0.01` while descending under
/// `MobEffects.SLOW_FALLING`.
fn living_gravity(falling: bool, slow_falling: bool) -> f64 {
    if falling && slow_falling {
        SLOW_FALLING_GRAVITY
    } else {
        LIVING_GRAVITY
    }
}

/// 1.21.1 `LivingEntity.getFluidFallingAdjustedMovement(double, boolean, Vec3)`
/// (`btn.a(DZLexc;)Lexc;`, offsets 0–100): no adjustment for zero gravity or a sprinting
/// entity; otherwise `y - d / 16`, with the `-0.003` terminal frame that makes a sinking
/// entity hover instead of accelerating without bound.
fn fluid_falling_adjusted_movement(
    gravity: f64,
    falling: bool,
    vel: [f64; 3],
    sprinting: bool,
) -> [f64; 3] {
    if gravity == 0.0 || sprinting {
        return vel;
    }
    let adjusted = vel[1] - gravity / FLUID_FALLING_DIVISOR;
    let y = if falling
        && (vel[1] - FLUID_FALLING_PROBE).abs() >= FLUID_FALLING_TERMINAL
        && adjusted.abs() < FLUID_FALLING_TERMINAL
    {
        -FLUID_FALLING_TERMINAL
    } else {
        adjusted
    };
    [vel[0], y, vel[2]]
}

/// 1.21.1 `Entity.getInputVector(Vec3, float, float)` (`bsr.b(Lexc;FF)Lexc;`, offsets 0–104):
/// zero below `1.0E-7`, normalized above unit length, scaled by `amount` and rotated by yaw.
fn fluid_input_vector(input: [f64; 3], amount: f32, yaw_deg: f32) -> [f64; 3] {
    let len_sq = input[0] * input[0] + input[1] * input[1] + input[2] * input[2];
    if len_sq < INPUT_VECTOR_EPSILON {
        return [0.0, 0.0, 0.0];
    }
    let base = if len_sq > 1.0 {
        fluid_vec3_normalize(input)
    } else {
        input
    };
    let amt = f64::from(amount);
    let scaled = [base[0] * amt, base[1] * amt, base[2] * amt];
    let rad = yaw_deg * DEG_TO_RAD;
    let sin = f64::from(crate::generation_math::mth_sin(rad));
    let cos = f64::from(crate::generation_math::mth_cos(rad));
    [
        scaled[0] * cos - scaled[2] * sin,
        scaled[1],
        scaled[2] * cos + scaled[0] * sin,
    ]
}

/// 1.21.1 `Entity.isFree(double, double, double)` (`bsr.g(DDD)Z`): `Level.noCollision` for the
/// entity's AABB at that position. Only the block half is modelled — the entity half needs the
/// level's entity-AABB query, and this server's mob lacks one (the mob push grid excludes the
/// querying mob rather than answering obstruction).
fn entity_is_free_blocks(aabb: EntityAabb) -> bool {
    !any_block_collision_box(aabb.min(), aabb.max())
}

/// The `LivingEntity.travel` inputs the fluid branches read, separated from the mob so the step
/// stays pure.
#[derive(Clone, Copy, Debug, PartialEq)]
struct LivingTravelContext {
    /// `d` from `btn.a` offsets 8–53.
    gravity: f64,
    /// `bl` — `getDeltaMovement().y <= 0.0` sampled at branch entry.
    falling: bool,
    sprinting: bool,
    on_ground: bool,
    on_climbable: bool,
    /// `Attributes.WATER_MOVEMENT_EFFICIENCY` (`buw.E`); `0.0` for every vanilla mob.
    water_movement_efficiency: f32,
    dolphins_grace: bool,
    /// `LivingEntity.speed` (field `cg`, `btn.fj()`), only read by the efficiency term.
    speed: f32,
    /// `getFluidHeight(tag)` — the `max_height` of `updateFluidHeightAndDoFluidPushing`.
    fluid_height: f64,
    /// `getFluidJumpThreshold()`.
    jump_threshold: f64,
}

impl Default for LivingTravelContext {
    fn default() -> Self {
        Self {
            gravity: LIVING_GRAVITY,
            falling: true,
            sprinting: false,
            on_ground: true,
            on_climbable: false,
            water_movement_efficiency: 0.0,
            dolphins_grace: false,
            speed: 0.0,
            fluid_height: 0.0,
            jump_threshold: 0.4,
        }
    }
}

/// The result of one fluid branch: the entity's `deltaMovement` after it and the position delta
/// its `Entity.move(MoverType.SELF, …)` call applied.
#[derive(Clone, Copy, Debug, PartialEq)]
struct FluidTravelOutcome {
    vel: [f64; 3],
    moved: [f64; 3],
}

/// One `LivingEntity.travel` fluid step (`btn.a(Lexc;)V` offsets 67–561). `move_fn` is
/// `Entity.move(MoverType.SELF, delta)` returning the applied delta and `horizontalCollision`;
/// `is_free` is `Entity.isFree(Vec3)`.
fn travel_fluid_step(
    kind: EntityFluidKind,
    vel: [f64; 3],
    input: [f64; 3],
    yaw_deg: f32,
    ctx: LivingTravelContext,
    move_fn: &mut dyn FnMut([f64; 3]) -> ([f64; 3], bool),
    is_free: &dyn Fn([f64; 3]) -> bool,
) -> FluidTravelOutcome {
    let (moved, mut v, horizontal_collision) = match kind {
        EntityFluidKind::Water => {
            // `f` = getWaterSlowDown()/sprint/dolphins, `g` = moveRelative amount, `h` = efficiency.
            let mut f = if ctx.sprinting {
                WATER_SPRINT_SLOW_DOWN
            } else {
                WATER_SLOW_DOWN
            };
            let mut g = FLUID_MOVE_RELATIVE;
            let mut h = ctx.water_movement_efficiency;
            if !ctx.on_ground {
                h *= 0.5;
            }
            if h > 0.0 {
                f += (WATER_MOVEMENT_EFFICIENCY_SLOW_DOWN - f) * h;
                g += (ctx.speed - g) * h;
            }
            if ctx.dolphins_grace {
                f = DOLPHINS_GRACE_SLOW_DOWN;
            }
            let rel = fluid_input_vector(input, g, yaw_deg);
            let asked = [vel[0] + rel[0], vel[1] + rel[1], vel[2] + rel[2]];
            let (moved, horizontal) = move_fn(asked);
            let mut out = if horizontal && ctx.on_climbable {
                [asked[0], FLUID_CLIMBABLE_LIFT, asked[2]]
            } else {
                asked
            };
            out = [
                out[0] * f64::from(f),
                out[1] * WATER_VERTICAL_DRAG,
                out[2] * f64::from(f),
            ];
            out = fluid_falling_adjusted_movement(ctx.gravity, ctx.falling, out, ctx.sprinting);
            (moved, out, horizontal)
        }
        EntityFluidKind::Lava => {
            let rel = fluid_input_vector(input, FLUID_MOVE_RELATIVE, yaw_deg);
            let asked = [vel[0] + rel[0], vel[1] + rel[1], vel[2] + rel[2]];
            let (moved, horizontal) = move_fn(asked);
            let mut out = if ctx.fluid_height <= ctx.jump_threshold {
                let dragged = [
                    asked[0] * LAVA_DRAG,
                    asked[1] * WATER_VERTICAL_DRAG,
                    asked[2] * LAVA_DRAG,
                ];
                fluid_falling_adjusted_movement(ctx.gravity, ctx.falling, dragged, ctx.sprinting)
            } else {
                [
                    asked[0] * LAVA_DRAG,
                    asked[1] * LAVA_DRAG,
                    asked[2] * LAVA_DRAG,
                ]
            };
            // `if (d != 0.0) deltaMovement = deltaMovement.add(0.0, -d / 4.0, 0.0)`
            if ctx.gravity != 0.0 {
                out[1] -= ctx.gravity / LAVA_GRAVITY_DIVISOR;
            }
            (moved, out, horizontal)
        }
    };

    // `if (horizontalCollision && isFree(v.x, v.y + 0.6 - getY() + e, v.z)) v = (v.x, 0.3, v.z)`:
    // `e` is `getY()` sampled before the move, so the probe reduces to `v.y + 0.6 - moved.y`.
    if horizontal_collision {
        let probe = [v[0], v[1] + FLUID_FREE_PROBE - moved[1], v[2]];
        if is_free(probe) {
            v = [v[0], FLUID_FREE_LIFT, v[2]];
        }
    }
    FluidTravelOutcome { vel: v, moved }
}

/// `updateFluidHeightAndDoFluidPushing` for a mob at `pos` with its own `deltaMovement` in
/// flight, which is what vanilla samples for the `0.0045000000000000005` minimum-speed guard.
fn mob_water_state_vel(etype: i64, pos: [f64; 3], vel: [f64; 3]) -> FluidPushResult {
    let (w, h) = mob_dimensions(etype);
    let aabb = EntityAabb::from_dimensions(pos[0], pos[1], pos[2], w, h);
    update_fluid_height_and_push_with(
        aabb,
        EntityFluidKind::Water,
        fluid_push_scale(EntityFluidKind::Water, level_ultra_warm()),
        true,
        false,
        vel,
        &block_at_cached,
    )
}

/// The standalone lava sweep, kept as the scalar reference the combined
/// `fluid_sweep_states` is verified against in tests.
#[allow(dead_code)]
fn mob_lava_state_vel(etype: i64, pos: [f64; 3], vel: [f64; 3]) -> FluidPushResult {
    let (w, h) = mob_dimensions(etype);
    let aabb = EntityAabb::from_dimensions(pos[0], pos[1], pos[2], w, h);
    update_fluid_height_and_push_with(
        aabb,
        EntityFluidKind::Lava,
        fluid_push_scale(EntityFluidKind::Lava, level_ultra_warm()),
        true,
        false,
        vel,
        &block_at_cached,
    )
}

/// Retained-capacity bound for the per-tick fluid sweep cell list. A mob AABB
/// covers single-digit block cells; the bound only guards a pathological AABB
/// from pinning a peak allocation in the thread-local buffer.
const FLUID_SWEEP_KEEP: usize =
    256 * 1024 / std::mem::size_of::<(i32, i32, i32, EntityFluidState)>();

thread_local! {
    static FLUID_SWEEP_CELLS: std::cell::RefCell<Vec<(i32, i32, i32, EntityFluidState)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Borrow the per-thread fluid sweep cell list. `block_at` never reaches back
/// into it, but a reentrant caller still falls back to a fresh `Vec`.
fn with_fluid_sweep_cells<R>(
    f: impl FnOnce(&mut Vec<(i32, i32, i32, EntityFluidState)>) -> R,
) -> R {
    FLUID_SWEEP_CELLS.with(|cell| match cell.try_borrow_mut() {
        Ok(mut cells) => {
            let result = f(&mut cells);
            if cells.capacity() > FLUID_SWEEP_KEEP {
                cells.shrink_to(FLUID_SWEEP_KEEP);
            }
            result
        }
        Err(_) => f(&mut Vec::new()),
    })
}

/// Record every `(bx, by, bz, fluid state)` inside `aabb`'s deflated sweep
/// window into `out`, preserving the (x, y, z) visit order so replaying the
/// list accumulates pushes exactly as the boxed loop did. The deflate is
/// spelled through `deflate_xyz` — the pinned `aabb.deflate(0.001)` call site
/// in `update_fluid_height_and_push_with` stays unique — and the widened
/// arguments are the same three `0.001`s.
fn fluid_sweep_cells(
    aabb: EntityAabb,
    get_block: &impl Fn(i32, i32, i32) -> u32,
    out: &mut Vec<(i32, i32, i32, EntityFluidState)>,
) -> EntityAabb {
    out.clear();
    let deflated = aabb.deflate_xyz(0.001, 0.001, 0.001);
    let x0 = deflated.min_x.floor() as i32;
    let x1 = deflated.max_x.ceil() as i32;
    let y0 = deflated.min_y.floor() as i32;
    let y1 = deflated.max_y.ceil() as i32;
    let z0 = deflated.min_z.floor() as i32;
    let z1 = deflated.max_z.ceil() as i32;
    for bx in x0..x1 {
        for by in y0..y1 {
            for bz in z0..z1 {
                if let Some(fs) = entity_fluid_state_of(get_block(bx, by, bz)) {
                    out.push((bx, by, bz, fs));
                }
            }
        }
    }
    deflated
}

/// One deflated AABB pass answering `mob_water_state_vel` and
/// `mob_lava_state_vel` together: a dry sweep skips every per-kind check after
/// a single scan, and a lava-only mob no longer pays a second full block
/// rescan. Water's result is identical to the standalone call; lava's `push`
/// is materialized only when water is dry, matching what the caller consumed
/// before (a mob already in water never read the lava push).
fn fluid_sweep_states(
    aabb: EntityAabb,
    vel: [f64; 3],
    get_block: &impl Fn(i32, i32, i32) -> u32,
) -> (FluidPushResult, FluidPushResult) {
    with_fluid_sweep_cells(|cells| {
        let deflated = fluid_sweep_cells(aabb, get_block, cells);
        let water = fluid_result_from_cells(
            cells,
            deflated,
            EntityFluidKind::Water,
            fluid_push_scale(EntityFluidKind::Water, level_ultra_warm()),
            true,
            false,
            vel,
            get_block,
        );
        let lava = fluid_result_from_cells(
            cells,
            deflated,
            EntityFluidKind::Lava,
            fluid_push_scale(EntityFluidKind::Lava, level_ultra_warm()),
            !water.in_fluid,
            false,
            vel,
            get_block,
        );
        (water, lava)
    })
}

fn mob_fluid_states_vel(etype: i64, pos: [f64; 3], vel: [f64; 3]) -> (FluidPushResult, FluidPushResult) {
    let (w, h) = mob_dimensions(etype);
    let aabb = EntityAabb::from_dimensions(pos[0], pos[1], pos[2], w, h);
    fluid_sweep_states(aabb, vel, &block_at_cached)
}

/// One `LivingEntity.travel` fluid tick for a mob.
///
/// Returns the position delta the branch applied, or `None` when the mob is in neither fluid — in
/// which case the stored `deltaMovement` is cleared, which is the non-fluid branch's friction.
/// The fluid current push is added to the velocity first, because `Entity.baseTick` runs
/// `updateInWaterStateAndDoFluidPushing` (and its `addDeltaMovement`) before `travel`.
fn mob_fluid_travel_tick(
    m: &Arc<Mob>,
    mp: &mut (f64, f64, f64, f32, f32),
) -> Option<FluidTravelOutcome> {
    let (w, h) = mob_dimensions(m.etype);
    let pos = [mp.0, mp.1, mp.2];
    let mut vel = *m.vel.lock().unwrap_or_else(|e| e.into_inner());
    let (water, lava) = mob_fluid_states_vel(m.etype, pos, vel);
    let (kind, state) = if water.in_fluid {
        (EntityFluidKind::Water, water)
    } else if lava.max_height > 0.0 {
        (EntityFluidKind::Lava, lava)
    } else {
        if vel != [0.0; 3] {
            *m.vel.lock().unwrap_or_else(|e| e.into_inner()) = [0.0; 3];
        }
        return None;
    };
    vel = [
        vel[0] + state.push[0],
        vel[1] + state.push[1],
        vel[2] + state.push[2],
    ];
    let on_ground = mob_has_support(m.etype, pos[0], pos[1], pos[2]);
    let ctx = LivingTravelContext {
        // `MobEffects.SLOW_FALLING` cannot be raised here: this server keeps effects on `Client`
        // (`effs`), not on `Mob`, so the second argument is the vanilla default.
        gravity: living_gravity(vel[1] <= 0.0, false),
        falling: vel[1] <= 0.0,
        on_ground,
        fluid_height: state.max_height,
        jump_threshold: fluid_jump_threshold(mob_eye_height(m.etype)),
        ..LivingTravelContext::default()
    };
    let mut move_fn = |delta: [f64; 3]| {
        let aabb = EntityAabb::from_dimensions(pos[0], pos[1], pos[2], w, h);
        // `Attributes.STEP_HEIGHT` (`buw.B`) default `0.6f`; `Entity.move` runs `collide`.
        let res = entity_collide_world(aabb, delta, on_ground, 0.6, &[]);
        (res.delta, res.horizontal_collision)
    };
    let is_free = |p: [f64; 3]| {
        entity_is_free_blocks(EntityAabb::from_dimensions(p[0], p[1], p[2], w, h))
    };
    let out = travel_fluid_step(kind, vel, [0.0; 3], mp.3, ctx, &mut move_fn, &is_free);
    mp.0 += out.moved[0];
    mp.1 += out.moved[1];
    mp.2 += out.moved[2];
    *m.vel.lock().unwrap_or_else(|e| e.into_inner()) = out.vel;
    Some(out)
}

#[cfg(all(test, megu_vanilla_data))]
mod entity_fluid_tests {
    use super::*;

    struct FluidTestWorldSnapshot {
        world: HashMap<DimChunkKey, HashMap<u32, u32>>,
        mobs: Vec<Arc<Mob>>,
        region: Region,
    }

    impl FluidTestWorldSnapshot {
        fn take() -> Self {
            Self {
                world: std::mem::take(&mut *world().lock().unwrap_or_else(|e| e.into_inner())),
                mobs: std::mem::take(&mut *mobs().lock().unwrap_or_else(|e| e.into_inner())),
                region: std::mem::take(&mut *region().lock().unwrap_or_else(|e| e.into_inner())),
            }
        }
    }

    impl std::ops::Drop for FluidTestWorldSnapshot {
        fn drop(&mut self) {
            *region().lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.region);
            *mobs().lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.mobs);
            *world().lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.world);
        }
    }

    #[test]
    fn entity_fluid_height_and_has_same_above_water_lava_waterlogged_and_kelp() {
        crate::test_support::require_vanilla_data();
        let reg = crate::block_registry::breg().unwrap();
        let stone = name_to_state("stone").unwrap();

        // LiquidBlock levels 0..=15 for water (80..=95) and lava (96..=111).
        for lvl in 0u32..=15 {
            let w = 80 + lvl;
            let l = 96 + lvl;
            let expected_num = if lvl == 0 || lvl >= 8 { 8 } else { 8 - lvl };
            let expected_h = expected_num as f32 / 9.0f32;
            let world_w = |x: i32, y: i32, z: i32| if (x, y, z) == (0, 64, 0) { w } else { 0 };
            let world_l = |x: i32, y: i32, z: i32| if (x, y, z) == (0, 64, 0) { l } else { 0 };
            assert_eq!(
                fluid_height_with(0, 64, 0, &world_w),
                Some((EntityFluidKind::Water, expected_h)),
                "water level {lvl}"
            );
            assert_eq!(
                fluid_height_with(0, 64, 0, &world_l),
                Some((EntityFluidKind::Lava, expected_h)),
                "lava level {lvl}"
            );
            // hasSameAbove -> 1.0f when same fluid is at y+1.
            let world_ww = |x: i32, y: i32, z: i32| {
                if (x, y, z) == (0, 64, 0) {
                    w
                } else if (x, y, z) == (0, 65, 0) {
                    81
                } else {
                    0
                }
            };
            let world_ll = |x: i32, y: i32, z: i32| {
                if (x, y, z) == (0, 64, 0) {
                    l
                } else if (x, y, z) == (0, 65, 0) {
                    97
                } else {
                    0
                }
            };
            assert_eq!(
                fluid_height_with(0, 64, 0, &world_ww),
                Some((EntityFluidKind::Water, 1.0f32)),
                "water hasSameAbove at level {lvl}"
            );
            assert_eq!(
                fluid_height_with(0, 64, 0, &world_ll),
                Some((EntityFluidKind::Lava, 1.0f32)),
                "lava hasSameAbove at level {lvl}"
            );
        }

        // Cross-fluid or solid above does NOT trigger hasSameAbove.
        let world_cross = |x: i32, y: i32, z: i32| {
            if (x, y, z) == (0, 64, 0) {
                80
            } else if (x, y, z) == (0, 65, 0) {
                96
            } else {
                0
            }
        };
        assert_eq!(
            fluid_height_with(0, 64, 0, &world_cross),
            Some((EntityFluidKind::Water, 8.0f32 / 9.0f32))
        );
        let world_solid_above = |x: i32, y: i32, z: i32| {
            if (x, y, z) == (0, 64, 0) {
                80
            } else if (x, y, z) == (0, 65, 0) {
                stone
            } else {
                0
            }
        };
        assert_eq!(
            fluid_height_with(0, 64, 0, &world_solid_above),
            Some((EntityFluidKind::Water, 8.0f32 / 9.0f32))
        );

        // Waterlogged slab + always-waterlogged plants (kelp, kelp_plant, seagrass, tall_seagrass, bubble_column).
        let wl_slab = reg
            .checked_state(
                "minecraft:oak_slab",
                &[
                    ("type".into(), "bottom".into()),
                    ("waterlogged".into(), "true".into()),
                ],
            )
            .unwrap();
        let dry_slab = reg
            .checked_state(
                "minecraft:oak_slab",
                &[
                    ("type".into(), "bottom".into()),
                    ("waterlogged".into(), "false".into()),
                ],
            )
            .unwrap();
        assert_eq!(
            fluid_height_with(0, 64, 0, &|x, y, z| if (x, y, z) == (0, 64, 0) {
                dry_slab
            } else {
                0
            }),
            None
        );
        for name in [
            "kelp",
            "kelp_plant",
            "seagrass",
            "tall_seagrass",
            "bubble_column",
        ] {
            let st = name_to_state(name).unwrap();
            assert_eq!(
                fluid_height_with(0, 64, 0, &|x, y, z| if (x, y, z) == (0, 64, 0) {
                    st
                } else {
                    0
                }),
                Some((EntityFluidKind::Water, 8.0f32 / 9.0f32)),
                "{name} must report water source height"
            );
        }
        // Waterlogged slab below water reports 1.0f via hasSameAbove.
        assert_eq!(
            fluid_height_with(0, 64, 0, &|x, y, z| {
                if (x, y, z) == (0, 64, 0) {
                    wl_slab
                } else if (x, y, z) == (0, 65, 0) {
                    80
                } else {
                    0
                }
            }),
            Some((EntityFluidKind::Water, 1.0f32))
        );
    }

    #[test]
    fn entity_fluid_get_flow_horizontal_slope_stepdown_and_falling() {
        crate::test_support::require_vanilla_data();
        let stone = name_to_state("stone").unwrap();
        let ice = name_to_state("ice").unwrap();

        // Source (8/9) at (0,64,0) flowing east into level 1 (7/9) at (1,64,0), flanked by stone walls.
        let flow_east = fluid_flow_with(
            0,
            64,
            0,
            entity_fluid_state_of(80).unwrap(),
            &|x, y, z| match (x, y, z) {
                (0, 64, 0) => 80,
                (1, 64, 0) => 81,
                (-1, 64, 0) | (0, 64, -1) | (0, 64, 1) => stone,
                _ => 0,
            },
        );
        assert!(
            (flow_east[0] - 1.0).abs() < 1e-6 && flow_east[1] == 0.0 && flow_east[2].abs() < 1e-6,
            "expected +X unit flow, got {flow_east:?}"
        );

        // Step-down into empty air at (1,64,0) above water at (1,63,0): flows +X.
        let flow_stepdown = fluid_flow_with(
            0,
            64,
            0,
            entity_fluid_state_of(81).unwrap(),
            &|x, y, z| match (x, y, z) {
                (0, 64, 0) => 81,
                (1, 63, 0) => 88, // falling water below empty neighbor
                (-1, 64, 0) | (0, 64, -1) | (0, 64, 1) => stone,
                _ => 0,
            },
        );
        assert!(
            (flow_stepdown[0] - 1.0).abs() < 1e-6 && flow_stepdown[1] == 0.0,
            "step-down should flow toward +X dropoff: {flow_stepdown:?}"
        );

        // Falling water (level 8) next to solid stone face picks up downward -6.0 component before normalize.
        let flow_fall_wall = fluid_flow_with(
            0,
            64,
            0,
            entity_fluid_state_of(88).unwrap(),
            &|x, y, z| match (x, y, z) {
                (0, 64, 0) => 88,
                (1, 64, 0) => stone,
                _ => 0,
            },
        );
        assert!(
            (flow_fall_wall[1] + 1.0).abs() < 1e-6,
            "falling water against stone wall should normalize to -Y: {flow_fall_wall:?}"
        );

        // Falling water next to IceBlock (`djw`) does NOT treat ice as solid face.
        let flow_fall_ice = fluid_flow_with(
            0,
            64,
            0,
            entity_fluid_state_of(88).unwrap(),
            &|x, y, z| match (x, y, z) {
                (0, 64, 0) => 88,
                (1, 64, 0) => ice,
                _ => 0,
            },
        );
        assert_eq!(
            flow_fall_ice,
            [0.0, 0.0, 0.0],
            "falling water next to IceBlock must not add -6.0 downward pull"
        );
    }

    #[test]
    fn entity_fluid_update_height_and_push_motion_scale_and_player_vs_mob() {
        crate::test_support::require_vanilla_data();
        let stone = name_to_state("stone").unwrap();

        // AABB flush at x = 1.0 next to water at (1, 64, 0): deflate(0.001) makes max_x = 0.999 -> NOT in water.
        let flush_box = EntityAabb::from_min_max([0.0, 64.0, 0.0], [1.0, 65.0, 1.0]);
        let world_edge = |x: i32, y: i32, z: i32| if (x, y, z) == (1, 64, 0) { 80 } else { 0 };
        let res_flush = update_fluid_height_and_push_with(
            flush_box,
            EntityFluidKind::Water,
            WATER_MOTION_SCALE,
            true,
            false,
            [0.0; 3],
            &world_edge,
        );
        assert!(!res_flush.in_fluid);
        assert_eq!(res_flush.max_height, 0.0);

        // Crossing past 1.001 into x = 1.002 -> deflated max_x = 1.001 -> ceil = 2 -> IN water.
        let grazing_box = EntityAabb::from_min_max([0.0, 64.0, 0.0], [1.002, 65.0, 1.0]);
        let res_graze = update_fluid_height_and_push_with(
            grazing_box,
            EntityFluidKind::Water,
            WATER_MOTION_SCALE,
            false,
            false,
            [0.0; 3],
            &world_edge,
        );
        assert!(res_graze.in_fluid);
        assert!((res_graze.max_height - (f64::from(64.0f32 + 8.0f32 / 9.0f32) - 64.001)).abs() < 1e-6);

        // Shallow sub-0.4 depth in flowing water: mob normalizes direction, so push magnitude >= 0.014 (WATER_MOTION_SCALE).
        // Meanwhile player does NOT normalize averaged vector, so shallow flow < 0.0045 gets boosted to FLUID_PUSH_MIN_SPEED when stationary.
        let shallow_box = EntityAabb::from_min_max([0.2, 64.75, 0.2], [0.8, 66.55, 0.8]);
        let world_flow = |x: i32, y: i32, z: i32| match (x, y, z) {
            (0, 64, 0) => 80,
            (1, 64, 0) => 81,
            (-1, 64, 0) | (0, 64, -1) | (0, 64, 1) => stone,
            _ => 0,
        };
        let push_mob = update_fluid_height_and_push_with(
            shallow_box,
            EntityFluidKind::Water,
            WATER_MOTION_SCALE,
            true,
            false,
            [0.0; 3],
            &world_flow,
        );
        assert!(push_mob.in_fluid);
        assert!((push_mob.push[0] - WATER_MOTION_SCALE).abs() < 1e-9);

        let push_player_stationary = update_fluid_height_and_push_with(
            shallow_box,
            EntityFluidKind::Water,
            WATER_MOTION_SCALE,
            true,
            true,
            [0.0; 3],
            &world_flow,
        );
        assert!(
            (push_player_stationary.push[0] - FLUID_PUSH_MIN_SPEED).abs() < 1e-12,
            "stationary player in weak sub-0.4 flow must clamp up to FLUID_PUSH_MIN_SPEED: {push_player_stationary:?}"
        );

        let push_player_moving = update_fluid_height_and_push_with(
            shallow_box,
            EntityFluidKind::Water,
            WATER_MOTION_SCALE,
            true,
            true,
            [0.1, 0.0, 0.0],
            &world_flow,
        );
        let expected_unboosted = (64.0f32 + 8.0 / 9.0) as f64 - 64.751;
        assert!(
            (push_player_moving.push[0] - expected_unboosted * WATER_MOTION_SCALE).abs() < 1e-9,
            "moving player must not receive stationary min-speed boost: {push_player_moving:?}"
        );
    }

    #[test]
    fn entity_fluid_eye_immersion_player_poses_and_mobs() {
        crate::test_support::require_vanilla_data();
        let reg = crate::block_registry::breg().unwrap();
        let wl_slab = reg
            .checked_state(
                "minecraft:oak_slab",
                &[
                    ("type".into(), "bottom".into()),
                    ("waterlogged".into(), "true".into()),
                ],
            )
            .unwrap();
        let bubble = name_to_state("bubble_column").unwrap();

        // Standing player at y=63.0 -> eyeY = 64.62f.
        // Water level 0 at y=64 has surface 64 + 8/9 = 64.8889 > 64.62 -> eye in water.
        let w_src = |x: i32, y: i32, z: i32| if (x, y, z) == (0, 64, 0) { 80 } else { 0 };
        assert!(eye_in_fluid_with(
            0.5,
            63.0,
            0.5,
            player_eye_height(false, false),
            EntityFluidKind::Water,
            &w_src
        ));

        // Water level 3 at y=64 has amount=5 -> surface 64 + 5/9 = 64.5556.
        // Standing (eyeY=64.62) is ABOVE surface (false); crouching (eyeY=64.27) is BELOW surface (true)!
        let w_lvl3 = |x: i32, y: i32, z: i32| if (x, y, z) == (0, 64, 0) { 83 } else { 0 };
        assert!(!eye_in_fluid_with(
            0.5,
            63.0,
            0.5,
            player_eye_height(false, false),
            EntityFluidKind::Water,
            &w_lvl3
        ));
        assert!(eye_in_fluid_with(
            0.5,
            63.0,
            0.5,
            player_eye_height(true, false),
            EntityFluidKind::Water,
            &w_lvl3
        ));

        // Fall-flying eyeHeight = 0.4f at y=64.0 (eyeY=64.4) is immersed in level 3 (64.5556 > 64.4), dry in level 6 (64.2222 < 64.4).
        let w_lvl6 = |x: i32, y: i32, z: i32| if (x, y, z) == (0, 64, 0) { 86 } else { 0 };
        assert!(eye_in_fluid_with(
            0.5,
            64.0,
            0.5,
            player_eye_height(false, true),
            EntityFluidKind::Water,
            &w_lvl3
        ));
        assert!(!eye_in_fluid_with(
            0.5,
            64.0,
            0.5,
            player_eye_height(false, true),
            EntityFluidKind::Water,
            &w_lvl6
        ));

        // Waterlogged slab at eye level triggers eye_in_fluid; bubble_column triggers eye_in_fluid but NOT drowning.
        assert!(eye_in_fluid_with(
            0.5,
            63.0,
            0.5,
            player_eye_height(false, false),
            EntityFluidKind::Water,
            &|x, y, z| if (x, y, z) == (0, 64, 0) { wl_slab } else { 0 }
        ));
        assert!(eye_in_fluid_with(
            0.5,
            63.0,
            0.5,
            player_eye_height(false, false),
            EntityFluidKind::Water,
            &|x, y, z| if (x, y, z) == (0, 64, 0) { bubble } else { 0 }
        ));
        assert!(eye_in_bubble_column_with(
            0.5,
            63.0,
            0.5,
            player_eye_height(false, false),
            &|x, y, z| if (x, y, z) == (0, 64, 0) { bubble } else { 0 }
        ));

        // Mob eye heights: cow (1.3f) vs chicken (0.644f) at y=63.5 in water at y=64..=65.
        // Cow eyeY = 64.8 > 64.5556 (dry in lvl 3), chicken eyeY = 64.144 < 64.5556 (submerged in lvl 3).
        assert!(!eye_in_fluid_with(
            0.5,
            63.5,
            0.5,
            mob_eye_height(22),
            EntityFluidKind::Water,
            &w_lvl3
        ));
        assert!(eye_in_fluid_with(
            0.5,
            63.5,
            0.5,
            mob_eye_height(19),
            EntityFluidKind::Water,
            &w_lvl3
        ));
        assert!((mob_eye_height(16) - 0.45).abs() < 1e-6); // cave_spider
        assert!((mob_eye_height(87) - 1.235).abs() < 1e-6); // sheep
        assert!((mob_eye_height(91) - 1.74).abs() < 1e-6); // skeleton
        assert!((mob_eye_height(124) - 1.74).abs() < 1e-6); // zombie
    }

    #[test]
    fn entity_fluid_is_in_lava_deflate_0_001_boundary_and_flowing_lava() {
        crate::test_support::require_vanilla_data();
        let w_lava_src = |x: i32, y: i32, z: i32| if (x, y, z) == (1, 64, 0) { 96 } else { 0 };
        let w_lava_flow = |x: i32, y: i32, z: i32| if (x, y, z) == (0, 64, 0) { 103 } else { 0 }; // level 7 -> 1/9

        // Flush at x=1.0: deflate(0.001).max_x = 0.999 -> NOT in lava.
        let player_flush = player_bounding_box(0.7, 64.0, 0.5, false, false);
        assert!(!aabb_in_lava_with(player_flush, &w_lava_src));

        // Grazing past 1.001 (center x=0.702 -> max_x=1.002 -> deflated max_x=1.001): IN lava.
        let player_graze = player_bounding_box(0.702, 64.0, 0.5, false, false);
        assert!(aabb_in_lava_with(player_graze, &w_lava_src));

        // Standing at y=64.0 above lava at y=63 (top 63 + 8/9 = 63.8889 < 64.001): NOT in lava.
        let w_lava_below = |x: i32, y: i32, z: i32| if (x, y, z) == (0, 63, 0) { 96 } else { 0 };
        assert!(!aabb_in_lava_with(
            player_bounding_box(0.5, 64.0, 0.5, false, false),
            &w_lava_below
        ));

        // Flowing lava level 7 (height 1/9 = 0.1111 > 0.001) at feet: IN lava.
        assert!(aabb_in_lava_with(
            player_bounding_box(0.5, 64.0, 0.5, false, false),
            &w_lava_flow
        ));

        // Exact tie e == deflated.min_y gives max_height == 0.0, so `fluidHeight > 0.0` is false.
        let exact_tie_y = f64::from(64.0f32 + 1.0f32 / 9.0f32) - 0.001;
        let tie_box = EntityAabb::from_min_max([0.2, exact_tie_y, 0.2], [0.8, exact_tie_y + 1.8, 0.8]);
        assert!(!aabb_in_lava_with(tie_box, &w_lava_flow));
    }

    #[test]
    fn entity_fluid_integration_client_cart_and_mob_tick() {
        crate::test_support::require_vanilla_data();
        let _guard = world_guard();
        let _snap = FluidTestWorldSnapshot::take();
        let reg = crate::block_registry::breg().unwrap();
        let stone = name_to_state("stone").unwrap();
        let wl_rail = reg
            .checked_state("minecraft:rail", &[("waterlogged".into(), "true".into())])
            .unwrap();
        let bubble = name_to_state("bubble_column").unwrap();

        for x in -4..=8 {
            for z in -4..=4 {
                set_block(x, 59, z, stone);
                for y in 60..=72 {
                    set_block(x, y, z, 0);
                }
            }
        }

        // Cart in waterlogged rail and flowing water.
        set_block(0, 60, 0, wl_rail);
        assert!(cart_in_water([0.5, 60.0625, 0.5]));
        set_block(2, 60, 0, 81); // flowing water level 1 (height 7/9 = 0.7778)
        assert!(cart_in_water([2.5, 60.0, 0.5]));
        // Cart perched above flowing water height (0.7778 < 0.78 + 0.001) is dry.
        assert!(!cart_in_water([2.5, 60.78, 0.5]));

        // Client pose drowning & bubble column & flowing lava.
        set_block(4, 61, 0, 83); // level 3 water -> surf 61.5556
        assert!(!player_drowning_submerged(DimId::Overworld, 4.5, 60.0, 0.5, false, false)); // standing eye 61.62
        assert!(player_drowning_submerged(DimId::Overworld, 4.5, 60.0, 0.5, true, false)); // crouching eye 61.27
        set_block(4, 61, 0, bubble);
        assert!(player_eye_in_water(4.5, 60.0, 0.5, false, false));
        assert!(!player_drowning_submerged(DimId::Overworld, 4.5, 60.0, 0.5, false, false));
        set_block(4, 60, 0, 102); // flowing lava level 6
        assert!(player_in_lava(DimId::Overworld, 4.5, 60.0, 0.5, false, false));

        // Mob fall-damage cancellation in water (mob_snap_y).
        let cow_water = Arc::new(Mob {
            dim: DimId::Overworld,
            eid: 9501,
            uuid: [1u8; 16],
            etype: 22,
            pos: Mutex::new((6.5, 70.0, 0.5, 0.0, 0.0)),
            hp: Mutex::new(10.0),
            last_hit: Mutex::new(Instant::now() - Duration::from_secs(10)),
            hurt_at: Mutex::new(Instant::now() - Duration::from_secs(10)),
            last_hurt_tick: Mutex::new(None),
            vel: Mutex::new([0.0; 3]),
            fuse: Mutex::new(0),
            fire_ticks: Mutex::new(0),
            love: Mutex::new(0),
            air: Mutex::new(server_ticks(AIR_SUPPLY_GT) as i32),
        });
        set_block(6, 60, 0, 80); // 1-deep water over stone at y=59
        {
            let mut mp = cow_water.pos.lock().unwrap_or_else(|e| e.into_inner());
            mob_snap_y(&cow_water, &mut mp);
            assert!(
                (mp.1 - 60.0).abs() < 1e-6,
                "cow falling into water pool must descend to riverbed 60.0, got {mp:?}"
            );
        }
        assert_eq!(
            *cow_water.hp.lock().unwrap_or_else(|e| e.into_inner()),
            10.0,
            "landing in water must reset fall distance and deal 0 fall damage"
        );

        // Dry-land fall damage still works.
        let cow_dry = Arc::new(Mob {
            dim: DimId::Overworld,
            eid: 9502,
            uuid: [2u8; 16],
            etype: 22,
            pos: Mutex::new((7.5, 68.0, 0.5, 0.0, 0.0)),
            hp: Mutex::new(10.0),
            last_hit: Mutex::new(Instant::now() - Duration::from_secs(10)),
            hurt_at: Mutex::new(Instant::now() - Duration::from_secs(10)),
            last_hurt_tick: Mutex::new(None),
            vel: Mutex::new([0.0; 3]),
            fuse: Mutex::new(0),
            fire_ticks: Mutex::new(0),
            love: Mutex::new(0),
            air: Mutex::new(server_ticks(AIR_SUPPLY_GT) as i32),
        });
        {
            let mut mp = cow_dry.pos.lock().unwrap_or_else(|e| e.into_inner());
            mob_snap_y(&cow_dry, &mut mp);
            assert!((mp.1 - 60.0).abs() < 1e-6);
        }
        assert_eq!(*cow_dry.hp.lock().unwrap_or_else(|e| e.into_inner()), 5.0, "8-block dry fall deals 5 dmg");

        // Mob water current push + drowning + lava burn in mob_tick.
        // Build a 1x1 channel along +X from x=-2..=2 at y=60 over stone y=59.
        for x in -2..=2 {
            set_block(x, 60, -1, stone);
            set_block(x, 60, 1, stone);
            set_block(x, 60, 0, 80 + (x + 2) as u32); // 80, 81, 82, 83, 84
        }
        set_block(-3, 60, 0, stone);
        assert!(
            mob_fluid_push_water(22, -0.5, 60.0, 0.5)[0] > 0.0,
            "flowing water channel must push cow +X"
        );
        // Set up dummy client in hot chunk + run mob_tick for drowning, push, and lava burn.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stream = TcpStream::connect(addr).unwrap();
        let _accepted = listener.accept().unwrap();
        let dummy_client = Arc::new(Client {
            eid: 9999,
            uuid: [9u8; 16],
            name: "fluid_tester".to_string(),
            player_attack_damage_id: None,
            pos: Mutex::new((0.5, 65.0, 0.5, 0.0, 0.0, true)),
            teleport: Mutex::new(Default::default()),
            hp: Mutex::new(20.0),
            sock: Mutex::new(stream),
            inv: Mutex::new(HashMap::new()),
            cursor: Mutex::new((0, 0, Vec::new())),
            drag: Mutex::new((0, Vec::new())),
            open_chest: Mutex::new(None),
            chest_sync: Mutex::new(None),
            open_furnace: Mutex::new(None),
            craft: Mutex::new(HashMap::new()),
            craft9: Mutex::new(HashMap::new()),
            open_table: Mutex::new(false),
            open_merchant: Mutex::new(false),
            trade_sel: Mutex::new(0),
            merchant_in: Mutex::new([(0, 0, Vec::new()), (0, 0, Vec::new())]),
            bow_draw: Mutex::new(None),
            dig: Mutex::new(None),
            eat_start: Mutex::new(None),
            blocking: Mutex::new(false),
            sprinting: Mutex::new(false),
            sneaking: Mutex::new(false),
            last_melee: Mutex::new(Instant::now() - Duration::from_secs(10)),
            fall_fly: Mutex::new(false),
            invulnerable: Mutex::new(0),
            last_hurt: Mutex::new(0.0),
            shield_cd: Mutex::new(Instant::now() - Duration::from_secs(10)),
            absorb: Mutex::new(0.0),
            pearl_cd: Mutex::new(Instant::now() - Duration::from_secs(10)),
            xp: Mutex::new(0),
            skin: Mutex::new(None),
            seen: Mutex::new(HashSet::new()),
            chunks: Mutex::new(HashSet::from([(-1, -1), (-1, 0), (0, -1), (0, 0)])),
            chunk_ack: Mutex::new(ChunkFeedback::new(1)),
            gen_sources: Mutex::new(GenSources::default()),
            gm: Mutex::new(1),
            fall_from: Mutex::new(65.0),
            food: Mutex::new(20),
            air: Mutex::new(server_ticks(AIR_SUPPLY_GT) as i32),
            armor_pts: Mutex::new(0),
            sat: Mutex::new(5.0),
            walk_acc: Mutex::new(0.0),
            lastp: Mutex::new((0.5, 65.0, 0.5)),
            spawn_pt: Mutex::new(None),
            dim: Mutex::new(DimId::Overworld),
            spawn_dim: Mutex::new(DimId::Overworld),
            view: Mutex::new(2),
            in_portal: Mutex::new(false),
            portal_at: Mutex::new(None),
            held: Mutex::new(0),
            effs: Mutex::new(Vec::new()),
            lod_v1_enabled: AtomicBool::new(false),
            underground_revealed: AtomicBool::new(false),
            sleeping: Mutex::new(None),
            inv_state_id: AtomicI32::new(0),
            use_on_resend: Mutex::new(None),
        });
        let old_clients = std::mem::take(&mut *clients().lock().unwrap_or_else(|e| e.into_inner()));
        clients().lock().unwrap_or_else(|e| e.into_inner()).push(dummy_client);
        let old_tick = TICK.load(Ordering::Relaxed);
        let old_world_time = WORLD_TIME.load(Ordering::Relaxed);
        TICK.store(server_ticks(10), Ordering::Relaxed);
        WORLD_TIME.store(6000, Ordering::Relaxed); // noon daylight

        // Submerged chicken (eyeHeight 0.644) in flowing water at (0,60,0) -> (0,61,0) drains air & gets pushed +X.
        set_block(0, 61, 0, 81);
        let chicken = Arc::new(Mob {
            dim: DimId::Overworld,
            eid: 9503,
            uuid: [3u8; 16],
            etype: 19,
            pos: Mutex::new((0.5, 60.0, 0.5, 0.0, 0.0)),
            hp: Mutex::new(4.0),
            last_hit: Mutex::new(Instant::now() - Duration::from_secs(10)),
            hurt_at: Mutex::new(Instant::now() - Duration::from_secs(10)),
            last_hurt_tick: Mutex::new(None),
            vel: Mutex::new([0.0; 3]),
            fuse: Mutex::new(0),
            fire_ticks: Mutex::new(0),
            love: Mutex::new(0),
            air: Mutex::new(10),
        });
        // Non-fire-immune pig in lava vs fire-immune blaze in lava.
        set_block(-3, 60, -3, 102); // flowing lava
        set_block(-3, 60, 3, 102);
        let pig_lava = Arc::new(Mob {
            dim: DimId::Overworld,
            eid: 9504,
            uuid: [4u8; 16],
            etype: 77,
            pos: Mutex::new((-2.5, 60.0, -2.5, 0.0, 0.0)),
            hp: Mutex::new(10.0),
            last_hit: Mutex::new(Instant::now() - Duration::from_secs(10)),
            hurt_at: Mutex::new(Instant::now() - Duration::from_secs(10)),
            last_hurt_tick: Mutex::new(None),
            vel: Mutex::new([0.0; 3]),
            fuse: Mutex::new(0),
            fire_ticks: Mutex::new(0),
            love: Mutex::new(0),
            air: Mutex::new(server_ticks(AIR_SUPPLY_GT) as i32),
        });
        let blaze_lava = Arc::new(Mob {
            dim: DimId::Overworld,
            eid: 9505,
            uuid: [5u8; 16],
            etype: 8,
            pos: Mutex::new((-2.5, 60.0, 3.5, 0.0, 0.0)),
            hp: Mutex::new(20.0),
            last_hit: Mutex::new(Instant::now() - Duration::from_secs(10)),
            hurt_at: Mutex::new(Instant::now() - Duration::from_secs(10)),
            last_hurt_tick: Mutex::new(None),
            vel: Mutex::new([0.0; 3]),
            fuse: Mutex::new(0),
            fire_ticks: Mutex::new(0),
            love: Mutex::new(0),
            air: Mutex::new(server_ticks(AIR_SUPPLY_GT) as i32),
        });
        // Zombie in daylight water clears its fire ticks (124), whereas creeper in water keeps & advances its fuse (23).
        set_block(2, 60, 3, 80);
        set_block(0, 60, 2, 80);
        let zombie_water = Arc::new(Mob {
            dim: DimId::Overworld,
            eid: 9506,
            uuid: [6u8; 16],
            etype: 124,
            pos: Mutex::new((2.5, 60.0, 3.5, 0.0, 0.0)),
            hp: Mutex::new(20.0),
            last_hit: Mutex::new(Instant::now() - Duration::from_secs(10)),
            hurt_at: Mutex::new(Instant::now() - Duration::from_secs(10)),
            last_hurt_tick: Mutex::new(None),
            vel: Mutex::new([0.0; 3]),
            fuse: Mutex::new(0),
            fire_ticks: Mutex::new(5),
            love: Mutex::new(0),
            air: Mutex::new(server_ticks(AIR_SUPPLY_GT) as i32),
        });
        let creeper_water = Arc::new(Mob {
            dim: DimId::Overworld,
            eid: 9507,
            uuid: [7u8; 16],
            etype: 23,
            pos: Mutex::new((0.5, 60.0, 2.5, 0.0, 0.0)),
            hp: Mutex::new(20.0),
            last_hit: Mutex::new(Instant::now() - Duration::from_secs(10)),
            hurt_at: Mutex::new(Instant::now() - Duration::from_secs(10)),
            last_hurt_tick: Mutex::new(None),
            vel: Mutex::new([0.0; 3]),
            fuse: Mutex::new(7),
            fire_ticks: Mutex::new(0),
            love: Mutex::new(0),
            air: Mutex::new(server_ticks(AIR_SUPPLY_GT) as i32),
        });
        mobs().lock().unwrap_or_else(|e| e.into_inner()).extend([
            chicken.clone(),
            pig_lava.clone(),
            blaze_lava.clone(),
            zombie_water.clone(),
            creeper_water.clone(),
        ]);

        refresh_hot();
        mob_tick();

        WORLD_TIME.store(old_world_time, Ordering::Relaxed);
        TICK.store(old_tick, Ordering::Relaxed);
        *clients().lock().unwrap_or_else(|e| e.into_inner()) = old_clients;

        assert_eq!(*chicken.air.lock().unwrap_or_else(|e| e.into_inner()), 9, "chicken air must drain in water");
        assert!(
            chicken.pos.lock().unwrap_or_else(|e| e.into_inner()).0 > 0.5,
            "chicken must be pushed +X by water current: {:?}",
            *chicken.pos.lock().unwrap_or_else(|e| e.into_inner())
        );
        assert_eq!(
            *pig_lava.hp.lock().unwrap_or_else(|e| e.into_inner()),
            6.0,
            "pig in flowing lava must take 4.0 damage on 10-tick boundary"
        );
        assert_eq!(
            *blaze_lava.hp.lock().unwrap_or_else(|e| e.into_inner()),
            20.0,
            "fire-immune blaze must take 0 lava damage"
        );
        assert_eq!(
            *zombie_water.fire_ticks.lock().unwrap_or_else(|e| e.into_inner()),
            0,
            "zombie daylight burn counter must clear in water"
        );
        assert_eq!(
            *creeper_water.fuse.lock().unwrap_or_else(|e| e.into_inner()),
            8,
            "creeper fuse must NOT be cleared by water contact (7 -> 8, not 0 -> 1)"
        );
    }

    #[test]
    fn xpoll_entity_scratch_fluid_sweep_matches_scalar_sweeps_with_zero_growth() {
        let _guard = crate::world_guard();
        crate::test_support::require_vanilla_data();
        // Synthetic pool: a water source ringed by flowing levels, a lava
        // pocket behind stone, a solid floor. State ids are the synthetic
        // fluid table (80..=95 water, 96..=111 lava) the file's other tests use.
        let stone = name_to_state("stone").unwrap();
        let world = move |x: i32, y: i32, z: i32| -> u32 {
            match (x, y, z) {
                (0, 64, 0) => 80,
                (1, 64, 0) | (-1, 64, 0) | (0, 64, 1) | (0, 64, -1) => 81,
                (2, 64, 0) => 82,
                (3, 63, 0) => 96,
                (4, 63, 0) => 98,
                (_, y, _) if y <= 63 => stone,
                _ => 0,
            }
        };
        let water_scale = fluid_push_scale(EntityFluidKind::Water, level_ultra_warm());
        let lava_scale = fluid_push_scale(EntityFluidKind::Lava, level_ultra_warm());
        let aabb = EntityAabb::from_min_max([0.0, 64.0, 0.0], [0.6, 65.8, 0.6]);
        let vel = [0.001, 0.0, 0.002];

        // Warm every code path once so lazy registry init lands outside the
        // timed loops.
        let _ = fluid_sweep_states(aabb, vel, &world);
        let _ = update_fluid_height_and_push_with(
            aabb, EntityFluidKind::Water, water_scale, true, false, vel, &world,
        );
        let _ = update_fluid_height_and_push_with(
            aabb, EntityFluidKind::Lava, lava_scale, true, false, vel, &world,
        );

        let mut expected = Vec::with_capacity(500);
        let fresh_start = Instant::now();
        for i in 0..500usize {
            let probe = aabb.offset((i % 5) as f64 * 0.5, 0.0, -((i % 3) as f64) * 0.5);
            let water = update_fluid_height_and_push_with(
                probe, EntityFluidKind::Water, water_scale, true, false, vel, &world,
            );
            let lava = update_fluid_height_and_push_with(
                probe,
                EntityFluidKind::Lava,
                lava_scale,
                !water.in_fluid,
                false,
                vel,
                &world,
            );
            expected.push((water, lava));
        }
        let fresh_us = fresh_start.elapsed().as_micros();

        let pooled_start = Instant::now();
        for (i, expected) in expected.iter().enumerate().take(500) {
            let probe = aabb.offset((i % 5) as f64 * 0.5, 0.0, -((i % 3) as f64) * 0.5);
            let actual = fluid_sweep_states(probe, vel, &world);
            assert_eq!(*expected, actual, "sweep #{i} must be bit-identical");
        }
        let pooled_us = pooled_start.elapsed().as_micros();
        eprintln!(
            "xpoll_entity_scratch_fluid: fresh {fresh_us}us vs pooled {pooled_us}us over 500 pairs"
        );

        // The pooled cell list holds steady capacity after warmup.
        let before = FLUID_SWEEP_CELLS.with(|cell| cell.borrow().capacity());
        for i in 0..500usize {
            let probe = aabb.offset((i % 5) as f64 * 0.5, 0.0, -((i % 3) as f64) * 0.5);
            let _ = fluid_sweep_states(probe, vel, &world);
        }
        let after = FLUID_SWEEP_CELLS.with(|cell| cell.borrow().capacity());
        assert_eq!(before, after, "fluid sweep cells must not grow after warmup");
        assert!(before > 0, "warmup must have populated the sweep cell list");

        // Dry-cell short-circuit: an all-air sweep never calls back into
        // get_block beyond the scan itself, and matches the scalar result.
        let calls = std::cell::Cell::new(0usize);
        let counting = |x: i32, y: i32, z: i32| {
            calls.set(calls.get() + 1);
            world(x, y, z)
        };
        let dry = aabb.offset(64.0, 8.0, 64.0);
        let (water_dry, lava_dry) = fluid_sweep_states(dry, vel, &counting);
        let scan_calls = calls.get();
        let dry_window = dry.deflate(0.001);
        let expected_scan = ((dry_window.max_x.ceil() as i32) - (dry_window.min_x.floor() as i32))
            * ((dry_window.max_y.ceil() as i32) - (dry_window.min_y.floor() as i32))
            * ((dry_window.max_z.ceil() as i32) - (dry_window.min_z.floor() as i32));
        assert_eq!(
            scan_calls,
            expected_scan as usize,
            "a dry sweep must skip per-kind flow/height lookups entirely"
        );
        assert_eq!(
            (water_dry, lava_dry),
            (
                update_fluid_height_and_push_with(
                    dry, EntityFluidKind::Water, water_scale, true, false, vel, &world,
                ),
                update_fluid_height_and_push_with(
                    dry, EntityFluidKind::Lava, lava_scale, true, false, vel, &world,
                ),
            ),
            "dry-cell result must be bit-identical to the scalar sweeps"
        );
    }
}
