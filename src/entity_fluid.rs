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

const WATER_MOTION_SCALE: f64 = 0.014;
const LAVA_OVERWORLD_MOTION_SCALE: f64 = 0.007;
#[allow(dead_code)]
const LAVA_NETHER_MOTION_SCALE: f64 = 0.0023333333333333335;
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

    FluidPushResult {
        in_fluid: found,
        max_height,
        push,
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
        LAVA_OVERWORLD_MOTION_SCALE,
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
    aabb_in_water_with(aabb, &block_at)
}

fn aabb_in_lava(aabb: EntityAabb) -> bool {
    aabb_in_lava_with(aabb, &block_at)
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

fn player_eye_in_water(x: f64, y: f64, z: f64, sneaking: bool, fall_fly: bool) -> bool {
    let eh = player_eye_height(sneaking, fall_fly);
    eye_in_fluid_with(x, y, z, eh, EntityFluidKind::Water, &block_at)
}

fn player_drowning_submerged(x: f64, y: f64, z: f64, sneaking: bool, fall_fly: bool) -> bool {
    let eh = player_eye_height(sneaking, fall_fly);
    eye_in_fluid_with(x, y, z, eh, EntityFluidKind::Water, &block_at)
        && !eye_in_bubble_column_with(x, y, z, eh, &block_at)
}

fn player_in_lava(x: f64, y: f64, z: f64, sneaking: bool, fall_fly: bool) -> bool {
    aabb_in_lava(player_bounding_box(x, y, z, sneaking, fall_fly))
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
    eye_in_fluid_with(x, y, z, mob_eye_height(etype), EntityFluidKind::Water, &block_at)
}

fn mob_drowning_submerged(etype: i64, x: f64, y: f64, z: f64) -> bool {
    let eh = mob_eye_height(etype);
    eye_in_fluid_with(x, y, z, eh, EntityFluidKind::Water, &block_at)
        && !eye_in_bubble_column_with(x, y, z, eh, &block_at)
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
    let (w, h) = mob_dimensions(etype);
    let aabb = EntityAabb::from_dimensions(x, y, z, w, h);
    update_fluid_height_and_push_with(
        aabb,
        EntityFluidKind::Water,
        WATER_MOTION_SCALE,
        true,
        false,
        [0.0; 3],
        &block_at,
    )
}

#[allow(dead_code)]
fn mob_fluid_push_water(etype: i64, x: f64, y: f64, z: f64) -> [f64; 3] {
    mob_water_state(etype, x, y, z).push
}

#[cfg(test)]
mod entity_fluid_tests {
    use super::*;

    struct FluidTestWorldSnapshot {
        world: HashMap<(i32, i32), HashMap<u32, u32>>,
        mobs: Vec<Arc<Mob>>,
        region: Region,
    }

    impl FluidTestWorldSnapshot {
        fn take() -> Self {
            Self {
                world: std::mem::take(&mut *world().lock().unwrap()),
                mobs: std::mem::take(&mut *mobs().lock().unwrap()),
                region: std::mem::take(&mut *region().lock().unwrap()),
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
        assert!(!player_drowning_submerged(4.5, 60.0, 0.5, false, false)); // standing eye 61.62
        assert!(player_drowning_submerged(4.5, 60.0, 0.5, true, false)); // crouching eye 61.27
        set_block(4, 61, 0, bubble);
        assert!(player_eye_in_water(4.5, 60.0, 0.5, false, false));
        assert!(!player_drowning_submerged(4.5, 60.0, 0.5, false, false));
        set_block(4, 60, 0, 102); // flowing lava level 6
        assert!(player_in_lava(4.5, 60.0, 0.5, false, false));

        // Mob fall-damage cancellation in water (mob_snap_y).
        let cow_water = Arc::new(Mob {
            eid: 9501,
            uuid: [1u8; 16],
            etype: 22,
            pos: Mutex::new((6.5, 70.0, 0.5, 0.0, 0.0)),
            hp: Mutex::new(10.0),
            last_hit: Mutex::new(Instant::now() - Duration::from_secs(10)),
            hurt_at: Mutex::new(Instant::now() - Duration::from_secs(10)),
            fuse: Mutex::new(0),
            love: Mutex::new(0),
            air: Mutex::new(server_ticks(AIR_SUPPLY_GT) as i32),
        });
        set_block(6, 60, 0, 80); // 1-deep water over stone at y=59
        {
            let mut mp = cow_water.pos.lock().unwrap();
            mob_snap_y(&cow_water, &mut mp);
            assert!(
                (mp.1 - 60.0).abs() < 1e-6,
                "cow falling into water pool must descend to riverbed 60.0, got {mp:?}"
            );
        }
        assert_eq!(
            *cow_water.hp.lock().unwrap(),
            10.0,
            "landing in water must reset fall distance and deal 0 fall damage"
        );

        // Dry-land fall damage still works.
        let cow_dry = Arc::new(Mob {
            eid: 9502,
            uuid: [2u8; 16],
            etype: 22,
            pos: Mutex::new((7.5, 68.0, 0.5, 0.0, 0.0)),
            hp: Mutex::new(10.0),
            last_hit: Mutex::new(Instant::now() - Duration::from_secs(10)),
            hurt_at: Mutex::new(Instant::now() - Duration::from_secs(10)),
            fuse: Mutex::new(0),
            love: Mutex::new(0),
            air: Mutex::new(server_ticks(AIR_SUPPLY_GT) as i32),
        });
        {
            let mut mp = cow_dry.pos.lock().unwrap();
            mob_snap_y(&cow_dry, &mut mp);
            assert!((mp.1 - 60.0).abs() < 1e-6);
        }
        assert_eq!(*cow_dry.hp.lock().unwrap(), 5.0, "8-block dry fall deals 5 dmg");

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
            pos: Mutex::new((0.5, 65.0, 0.5, 0.0, 0.0, true)),
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
            held: Mutex::new(0),
            effs: Mutex::new(Vec::new()),
        });
        let old_clients = std::mem::take(&mut *clients().lock().unwrap());
        clients().lock().unwrap().push(dummy_client);
        let old_tick = TICK.load(Ordering::Relaxed);
        let old_world_time = WORLD_TIME.load(Ordering::Relaxed);
        TICK.store(server_ticks(10), Ordering::Relaxed);
        WORLD_TIME.store(6000, Ordering::Relaxed); // noon daylight

        // Submerged chicken (eyeHeight 0.644) in flowing water at (0,60,0) -> (0,61,0) drains air & gets pushed +X.
        set_block(0, 61, 0, 81);
        let chicken = Arc::new(Mob {
            eid: 9503,
            uuid: [3u8; 16],
            etype: 19,
            pos: Mutex::new((0.5, 60.0, 0.5, 0.0, 0.0)),
            hp: Mutex::new(4.0),
            last_hit: Mutex::new(Instant::now() - Duration::from_secs(10)),
            hurt_at: Mutex::new(Instant::now() - Duration::from_secs(10)),
            fuse: Mutex::new(0),
            love: Mutex::new(0),
            air: Mutex::new(10),
        });
        // Non-fire-immune pig in lava vs fire-immune blaze in lava.
        set_block(-3, 60, -3, 102); // flowing lava
        set_block(-3, 60, 3, 102);
        let pig_lava = Arc::new(Mob {
            eid: 9504,
            uuid: [4u8; 16],
            etype: 77,
            pos: Mutex::new((-2.5, 60.0, -2.5, 0.0, 0.0)),
            hp: Mutex::new(10.0),
            last_hit: Mutex::new(Instant::now() - Duration::from_secs(10)),
            hurt_at: Mutex::new(Instant::now() - Duration::from_secs(10)),
            fuse: Mutex::new(0),
            love: Mutex::new(0),
            air: Mutex::new(server_ticks(AIR_SUPPLY_GT) as i32),
        });
        let blaze_lava = Arc::new(Mob {
            eid: 9505,
            uuid: [5u8; 16],
            etype: 8,
            pos: Mutex::new((-2.5, 60.0, 3.5, 0.0, 0.0)),
            hp: Mutex::new(20.0),
            last_hit: Mutex::new(Instant::now() - Duration::from_secs(10)),
            hurt_at: Mutex::new(Instant::now() - Duration::from_secs(10)),
            fuse: Mutex::new(0),
            love: Mutex::new(0),
            air: Mutex::new(server_ticks(AIR_SUPPLY_GT) as i32),
        });
        // Zombie in daylight water clears burn fuse (124), whereas creeper in water keeps & advances its fuse (23).
        set_block(2, 60, 3, 80);
        set_block(0, 60, 2, 80);
        let zombie_water = Arc::new(Mob {
            eid: 9506,
            uuid: [6u8; 16],
            etype: 124,
            pos: Mutex::new((2.5, 60.0, 3.5, 0.0, 0.0)),
            hp: Mutex::new(20.0),
            last_hit: Mutex::new(Instant::now() - Duration::from_secs(10)),
            hurt_at: Mutex::new(Instant::now() - Duration::from_secs(10)),
            fuse: Mutex::new(5),
            love: Mutex::new(0),
            air: Mutex::new(server_ticks(AIR_SUPPLY_GT) as i32),
        });
        let creeper_water = Arc::new(Mob {
            eid: 9507,
            uuid: [7u8; 16],
            etype: 23,
            pos: Mutex::new((0.5, 60.0, 2.5, 0.0, 0.0)),
            hp: Mutex::new(20.0),
            last_hit: Mutex::new(Instant::now() - Duration::from_secs(10)),
            hurt_at: Mutex::new(Instant::now() - Duration::from_secs(10)),
            fuse: Mutex::new(7),
            love: Mutex::new(0),
            air: Mutex::new(server_ticks(AIR_SUPPLY_GT) as i32),
        });
        mobs().lock().unwrap().extend([
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
        *clients().lock().unwrap() = old_clients;

        assert_eq!(*chicken.air.lock().unwrap(), 9, "chicken air must drain in water");
        assert!(
            chicken.pos.lock().unwrap().0 > 0.5,
            "chicken must be pushed +X by water current: {:?}",
            *chicken.pos.lock().unwrap()
        );
        assert_eq!(
            *pig_lava.hp.lock().unwrap(),
            6.0,
            "pig in flowing lava must take 4.0 damage on 10-tick boundary"
        );
        assert_eq!(
            *blaze_lava.hp.lock().unwrap(),
            20.0,
            "fire-immune blaze must take 0 lava damage"
        );
        assert_eq!(
            *zombie_water.fuse.lock().unwrap(),
            0,
            "zombie daylight burn counter must clear in water"
        );
        assert_eq!(
            *creeper_water.fuse.lock().unwrap(),
            8,
            "creeper fuse must NOT be cleared by water contact (7 -> 8, not 0 -> 1)"
        );
    }
}
