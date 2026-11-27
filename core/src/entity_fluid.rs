// SPDX-License-Identifier: LGPL-3.0-or-later
#![forbid(unsafe_code)]
//! The single source of truth for the 1.21.1 fluid-height, flow-vector,
//! fluid-push and solid-face math, so the server tick
//! (`src/entity_fluid.rs`) and the client prediction
//! (`client-kernel/src/physics.rs`) cannot drift apart.
//!
//! Pinned jar SHA-1: `59353fb40c36d304f2035d51e7d6e6baa98dc05c`
//! Official mappings: `03f8985492bda0afc0898465341eb0acef35f570`
//!
//! ```text
//! net.minecraft.world.level.material.FlowingFluid   -> epc
//! net.minecraft.world.level.material.Fluid          -> epd
//! net.minecraft.world.level.material.FluidState     -> epe
//! net.minecraft.world.level.material.EmptyFluid     -> epb
//! net.minecraft.world.level.material.LavaFluid      -> eph
//! net.minecraft.world.level.material.WaterFluid     -> epk
//! net.minecraft.world.level.block.LiquidBlock       -> dko
//! net.minecraft.world.level.block.IceBlock          -> djw
//! net.minecraft.world.entity.Entity                 -> bsr
//! net.minecraft.world.phys.AABB                     -> ewx
//! net.minecraft.world.phys.Vec3                     -> exc
//! ```
//!
//! The registry never enters this crate. Both consumers project a block into a
//! [`FluidCell`] and hand the math a `Fn(i32, i32, i32) -> FluidCell` lookup:
//! the server fills the cell from its `BReg` state table, the kernel from the
//! block-fact table it already syncs for lighting.
//!
//! Float exactness is the contract, not a nicety. Vanilla keeps the fluid
//! height in Java `float` (`getAmount() / 9.0F`), widens it to `double` at the
//! AABB comparison, and samples the flow sum in `float` before widening it
//! into the accumulated `Vec3`. Every constant below is the exact value the
//! JVM loads, and the accumulation order is the (x, y, z) visit order of
//! `Entity.updateFluidHeightAndDoFluidPushing`'s boxed loop, because a
//! reordered sum is a different double.

// `Entity.bn()` (`bsr.bn()`, offsets 33–42) pushes every fluid except lava at `0.014`.
pub const WATER_MOTION_SCALE: f64 = 0.014;
// `Entity.bm()` (`bsr.bm()`, offsets 9–35): `dimensionType().ultraWarm() ? 0.007 : 0.0023333333333333335`.
// The pinned bytecode puts the *larger* push in the ultra-warm (Nether) dimension; the
// previous constants here were named the other way round, which is only observable once the
// dimension is wired (Slice 4d), so the names follow the bytecode now.
pub const LAVA_ULTRA_WARM_MOTION_SCALE: f64 = 0.007;
pub const LAVA_TEMPERATE_MOTION_SCALE: f64 = 0.0023333333333333335;
pub const FLUID_PUSH_MIN_SPEED: f64 = 0.0045000000000000005;
pub const FLUID_VELOCITY_THRESHOLD: f64 = 0.003;

/// The two 1.21.1 fluid types, `FluidTags.WATER` and `FluidTags.LAVA`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntityFluidKind {
    Water,
    Lava,
}

/// The subset of `FluidState` the height rule reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EntityFluidState {
    /// `FluidState.getType()`.
    pub kind: EntityFluidKind,
    /// `FluidState.getAmount()`: `1..=8`. A source and a falling column both
    /// report `8`.
    pub amount: u8,
    /// `FluidState.isSource() == false && isFull()`: the level-8 column that
    /// only a falling fluid reports, which makes `getFlow` look for a side wall.
    pub falling: bool,
}

/// One block cell's facts, projected from the caller's block registry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FluidCell {
    /// `BlockGetter.getFluidState(pos).getType()`; `None` for an empty fluid.
    pub kind: Option<EntityFluidKind>,
    /// `FluidState.getAmount()` when `kind` is set, `0` otherwise.
    pub amount: u8,
    /// `FluidState.isFalling()` — the level-8 columns.
    pub falling: bool,
    /// `BlockState.blocksMotion()`.
    pub blocks_motion: bool,
    /// `minecraft:ice` or `minecraft:frosted_ice`, the pair
    /// `FlowingFluid.isSolidFace` refuses to call a solid face.
    pub is_ice: bool,
    /// The six `Support::Full` face bits, indexed by `Direction` ordinal
    /// (DOWN 0, UP 1, NORTH 2, SOUTH 3, WEST 4, EAST 5).
    pub sturdy_faces: u8,
}

/// A world-space box, in the field layout the server's `EntityAabb` and the
/// kernel's `Aabb` both convert from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FluidAabb {
    pub min_x: f64,
    pub min_y: f64,
    pub min_z: f64,
    pub max_x: f64,
    pub max_y: f64,
    pub max_z: f64,
}

impl FluidAabb {
    /// 1.21.1 `AABB.deflate(double, double, double)` (`ewx.f(DDD)`).
    fn deflate_xyz(self, dx: f64, dy: f64, dz: f64) -> Self {
        Self {
            min_x: self.min_x + dx,
            min_y: self.min_y + dy,
            min_z: self.min_z + dz,
            max_x: self.max_x - dx,
            max_y: self.max_y - dy,
            max_z: self.max_z - dz,
        }
    }

    /// 1.21.1 `AABB.deflate(double)` (`ewx.h(D)`).
    fn deflate(self, d: f64) -> Self {
        self.deflate_xyz(d, d, d)
    }
}

/// The half-open integer bounds one deflated sweep visits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FluidSweepWindow {
    pub x0: i32,
    pub x1: i32,
    pub y0: i32,
    pub y1: i32,
    pub z0: i32,
    pub z1: i32,
}

/// `FluidPushResult`: the `Entity` fluid counters one
/// `updateFluidHeightAndDoFluidPushing` pass leaves behind.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FluidPushResult {
    /// `fluidHeight > 0.0` for the queried tag: water's `in_fluid` is this,
    /// lava's is `max_height > 0.0`.
    pub in_fluid: bool,
    /// `getFluidHeight(tag)`: the tallest `(pos.y + height) - min_y` inside the
    /// deflated box.
    pub max_height: f64,
    /// The `deltaMovement` the current adds.
    pub push: [f64; 3],
}

/// `deflate(0.001)`'s `floor`/`ceil` window, in the (x, y, z) order the boxed
/// loop visits. The caller owns the deflate so a sweep can reuse the deflated
/// box for the height comparisons.
pub fn fluid_sweep_window(deflated: FluidAabb) -> FluidSweepWindow {
    FluidSweepWindow {
        x0: deflated.min_x.floor() as i32,
        x1: deflated.max_x.ceil() as i32,
        y0: deflated.min_y.floor() as i32,
        y1: deflated.max_y.ceil() as i32,
        z0: deflated.min_z.floor() as i32,
        z1: deflated.max_z.ceil() as i32,
    }
}

/// The `FluidState` a projected cell reports, `None` for an empty fluid.
pub fn fluid_state_of(cell: FluidCell) -> Option<EntityFluidState> {
    cell.kind.map(|kind| EntityFluidState {
        kind,
        amount: cell.amount,
        falling: cell.falling,
    })
}

/// 1.21.1 `FlowingFluid.getOwnHeight(FluidState)` (`epc.a(Lepe;)F`, offsets 0–9).
pub fn fluid_own_height(fs: EntityFluidState) -> f32 {
    (fs.amount as f32) / 9.0f32
}

/// `Entity.bn()` (water `0.014`) and `Entity.bm()` (lava `ultraWarm() ? 0.007 :
/// 0.0023333333333333335`) motion scales.
pub fn fluid_push_scale(kind: EntityFluidKind, ultra_warm: bool) -> f64 {
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

/// 1.21.1 `FlowingFluid.hasSameAbove(FluidState, BlockGetter, BlockPos)` (`epc.c(Lepe;Ldcc;Ljd;)Z`, offsets 0–20).
pub fn fluid_has_same_above_with(
    kind: EntityFluidKind,
    x: i32,
    y: i32,
    z: i32,
    get_block: &impl Fn(i32, i32, i32) -> FluidCell,
) -> bool {
    fluid_state_of(get_block(x, y + 1, z)).is_some_and(|above| above.kind == kind)
}

/// 1.21.1 `FlowingFluid.getHeight(FluidState, BlockGetter, BlockPos)` (`epc.a(Lepe;Ldcc;Ljd;)F`, offsets 0–15).
pub fn fluid_height_with(
    x: i32,
    y: i32,
    z: i32,
    get_block: &impl Fn(i32, i32, i32) -> FluidCell,
) -> Option<(EntityFluidKind, f32)> {
    let fs = fluid_state_of(get_block(x, y, z))?;
    let h = if fluid_has_same_above_with(fs.kind, x, y, z, get_block) {
        1.0f32
    } else {
        fluid_own_height(fs)
    };
    Some((fs.kind, h))
}

/// 1.21.1 `Vec3.normalize()` (`exc.d()`, offsets 0–70): returns zero vector when length < 1e-4.
pub fn fluid_vec3_normalize(v: [f64; 3]) -> [f64; 3] {
    let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    if len < 1.0e-4 {
        [0.0, 0.0, 0.0]
    } else {
        [v[0] / len, v[1] / len, v[2] / len]
    }
}

/// `Vec3.length()` (`exc.f()`).
pub fn fluid_vec3_len(v: [f64; 3]) -> f64 {
    (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt()
}

/// The registry-free half of `FlowingFluid.isSolidFace`: a cell that is not ice
/// and covers the queried face. A consumer that already carries the face mask
/// answers the same question from its own [`FluidCell`].
pub fn fluid_cell_solid_face(cell: FluidCell, dir_idx: u8) -> bool {
    dir_idx < 6 && !cell.is_ice && (cell.sturdy_faces & (1 << dir_idx)) != 0
}

/// 1.21.1 `FlowingFluid.isSolidFace(BlockGetter, BlockPos, Direction)` (`epc.a(Ldcc;Ljd;Lji;)Z`, offsets 0–62).
pub fn fluid_is_solid_face(
    x: i32,
    y: i32,
    z: i32,
    dir_idx: u8,
    self_kind: EntityFluidKind,
    get_block: &impl Fn(i32, i32, i32) -> FluidCell,
) -> bool {
    let cell = get_block(x, y, z);
    if fluid_state_of(cell).is_some_and(|nfs| nfs.kind == self_kind) {
        return false;
    }
    if dir_idx == 1 {
        return true;
    }
    fluid_cell_solid_face(cell, dir_idx)
}

/// 1.21.1 `FlowingFluid.getFlow(BlockGetter, BlockPos, FluidState)` (`epc.a(Ldcc;Ljd;Lepe;)Lexc;`, offsets 0–340).
pub fn fluid_flow_with(
    x: i32,
    y: i32,
    z: i32,
    fs: EntityFluidState,
    get_block: &impl Fn(i32, i32, i32) -> FluidCell,
) -> [f64; 3] {
    // Direction.Plane.HORIZONTAL: (dir_idx, step_x, step_z) for NORTH(2), EAST(5), SOUTH(3), WEST(4).
    const HORIZ: [(u8, i32, i32); 4] = [(2, 0, -1), (5, 1, 0), (3, 0, 1), (4, -1, 0)];
    let own_h = fluid_own_height(fs);
    let mut flow_x = 0.0f64;
    let mut flow_z = 0.0f64;

    for &(_dir_idx, dx, dz) in &HORIZ {
        let (nx, nz) = (x + dx, z + dz);
        let neighbor = get_block(nx, y, nz);
        let neighbor_fs = fluid_state_of(neighbor);
        // FlowingFluid.affectsFlow (`epc.g`): empty or same fluid type.
        if let Some(nfs) = neighbor_fs {
            if nfs.kind != fs.kind {
                continue;
            }
        }
        let mut f = neighbor_fs.map_or(0.0f32, fluid_own_height);
        let mut g = 0.0f32;
        if f == 0.0f32 {
            if !neighbor.blocks_motion {
                let below_fs = fluid_state_of(get_block(nx, y - 1, nz));
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
pub fn fluid_push_finalize(
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

/// The single accumulation body of `Entity.updateFluidHeightAndDoFluidPushing`,
/// shared by the boxed AABB sweep and its pre-collected replay of one deflated
/// pass. The `cells` order is therefore load-bearing: it is the boxed loop's
/// (x, y, z) visit order, and the flow sum is a different double otherwise.
#[allow(clippy::too_many_arguments)]
fn fluid_accumulate<I>(
    cells: I,
    deflated: FluidAabb,
    kind: EntityFluidKind,
    motion_scale: f64,
    is_pushed_by_fluid: bool,
    is_player: bool,
    vel: [f64; 3],
    get_block: &impl Fn(i32, i32, i32) -> FluidCell,
) -> FluidPushResult
where
    I: Iterator<Item = (i32, i32, i32, EntityFluidState)>,
{
    let mut max_height = 0.0f64;
    let mut found = false;
    let mut acc = [0.0f64; 3];
    let mut count = 0i32;
    for (bx, by, bz, fs) in cells {
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

/// Accumulate `in_fluid`/`max_height` (and, when `is_pushed_by_fluid`, the flow
/// sum) over the `(bx, by, bz, fs)` cells of one deflated-AABB sweep whose kind
/// matches `kind`. `cells` must be in the sweep's (x, y, z) visit order so the
/// flow accumulation is ordered exactly as the boxed loop did it.
#[allow(clippy::too_many_arguments)]
pub fn fluid_result_from_cells(
    cells: &[(i32, i32, i32, EntityFluidState)],
    deflated: FluidAabb,
    kind: EntityFluidKind,
    motion_scale: f64,
    is_pushed_by_fluid: bool,
    is_player: bool,
    vel: [f64; 3],
    get_block: &impl Fn(i32, i32, i32) -> FluidCell,
) -> FluidPushResult {
    fluid_accumulate(
        cells.iter().copied(),
        deflated,
        kind,
        motion_scale,
        is_pushed_by_fluid,
        is_player,
        vel,
        get_block,
    )
}

/// 1.21.1 `Entity.updateFluidHeightAndDoFluidPushing(TagKey<Fluid>, double)` (`bsr.a(Lawu;D)Z`, offsets 0–431).
pub fn update_fluid_height_and_push_with(
    aabb: FluidAabb,
    kind: EntityFluidKind,
    motion_scale: f64,
    is_pushed_by_fluid: bool,
    is_player: bool,
    vel: [f64; 3],
    get_block: &impl Fn(i32, i32, i32) -> FluidCell,
) -> FluidPushResult {
    let deflated = aabb.deflate(0.001);
    let window = fluid_sweep_window(deflated);
    // `for (MutableBlockPos pos : BlockPos.betweenClosed(aabb))` visits
    // x-major, then y, then z.
    let sweep = (window.x0..window.x1).flat_map(|bx| {
        (window.y0..window.y1)
            .flat_map(move |by| (window.z0..window.z1).map(move |bz| (bx, by, bz)))
    });
    let cells = sweep.filter_map(|(bx, by, bz)| {
        fluid_state_of(get_block(bx, by, bz)).map(|fs| (bx, by, bz, fs))
    });
    fluid_accumulate(
        cells,
        deflated,
        kind,
        motion_scale,
        is_pushed_by_fluid,
        is_player,
        vel,
        get_block,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const STONE_FACES: u8 = 0x3f;

    fn air() -> FluidCell {
        FluidCell::default()
    }

    fn water(amount: u8) -> FluidCell {
        FluidCell {
            kind: Some(EntityFluidKind::Water),
            amount,
            ..FluidCell::default()
        }
    }

    fn lava(amount: u8) -> FluidCell {
        FluidCell {
            kind: Some(EntityFluidKind::Lava),
            amount,
            ..FluidCell::default()
        }
    }

    fn falling_water() -> FluidCell {
        FluidCell {
            kind: Some(EntityFluidKind::Water),
            amount: 8,
            falling: true,
            ..FluidCell::default()
        }
    }

    fn stone() -> FluidCell {
        FluidCell {
            blocks_motion: true,
            sturdy_faces: STONE_FACES,
            ..FluidCell::default()
        }
    }

    fn ice() -> FluidCell {
        FluidCell {
            blocks_motion: true,
            is_ice: true,
            sturdy_faces: STONE_FACES,
            ..FluidCell::default()
        }
    }

    fn glass_pane() -> FluidCell {
        // Blocks motion, but its shapes cover no whole face: `Support::Full` false.
        FluidCell {
            blocks_motion: true,
            ..FluidCell::default()
        }
    }

    #[test]
    fn fluid_own_height_is_amount_over_nine() {
        for amount in 1u8..=8 {
            let fs = EntityFluidState {
                kind: EntityFluidKind::Water,
                amount,
                falling: false,
            };
            assert_eq!(fluid_own_height(fs), amount as f32 / 9.0f32);
        }
        // A source and a falling column both report 8/9.
        let falling = EntityFluidState {
            kind: EntityFluidKind::Lava,
            amount: 8,
            falling: true,
        };
        assert_eq!(fluid_own_height(falling), 8.0f32 / 9.0f32);
    }

    #[test]
    fn fluid_push_scale_selects_lava_by_dimension_and_water_never() {
        assert_eq!(fluid_push_scale(EntityFluidKind::Water, false), 0.014);
        assert_eq!(fluid_push_scale(EntityFluidKind::Water, true), 0.014);
        // `bsr.bm()` puts the larger push in the ultra-warm dimension.
        assert_eq!(
            fluid_push_scale(EntityFluidKind::Lava, true),
            LAVA_ULTRA_WARM_MOTION_SCALE
        );
        assert_eq!(
            fluid_push_scale(EntityFluidKind::Lava, false),
            LAVA_TEMPERATE_MOTION_SCALE
        );
        assert!(
            fluid_push_scale(EntityFluidKind::Lava, true)
                > fluid_push_scale(EntityFluidKind::Lava, false)
        );
    }

    #[test]
    fn fluid_vec3_normalize_zeroes_below_1e_4_and_keeps_direction_above() {
        assert_eq!(fluid_vec3_normalize([0.0, 0.0, 0.0]), [0.0, 0.0, 0.0]);
        // 0.00009 < 1e-4 is the pinned dead zone.
        assert_eq!(fluid_vec3_normalize([0.00009, 0.0, 0.0]), [0.0, 0.0, 0.0]);
        let unit = fluid_vec3_normalize([3.0, 4.0, 0.0]);
        assert_eq!(unit, [0.6, 0.8, 0.0]);
        assert!((fluid_vec3_len(unit) - 1.0).abs() < 1e-15);
        let falling = fluid_vec3_normalize([0.0, -6.0, 0.0]);
        assert_eq!(falling, [0.0, -1.0, 0.0]);
    }

    #[test]
    fn fluid_height_reads_own_height_and_same_fluid_above() {
        let source = |x: i32, y: i32, z: i32| match (x, y, z) {
            (0, 64, 0) => water(8),
            _ => air(),
        };
        assert_eq!(
            fluid_height_with(0, 64, 0, &source),
            Some((EntityFluidKind::Water, 8.0f32 / 9.0f32))
        );
        assert_eq!(fluid_height_with(0, 65, 0, &source), None);

        let column = |x: i32, y: i32, z: i32| match (x, y, z) {
            (0, 64, 0) => water(8),
            (0, 65, 0) => water(4),
            _ => air(),
        };
        // `hasSameAbove` reports the full block, not the cell's own height.
        assert_eq!(
            fluid_height_with(0, 64, 0, &column),
            Some((EntityFluidKind::Water, 1.0f32))
        );
        // The upper cell of a column is 4/9.
        assert_eq!(
            fluid_height_with(0, 65, 0, &column),
            Some((EntityFluidKind::Water, 4.0f32 / 9.0f32))
        );

        // Cross-fluid and solid above are not `hasSameAbove`.
        let cross = |x: i32, y: i32, z: i32| match (x, y, z) {
            (0, 64, 0) => water(8),
            (0, 65, 0) => lava(8),
            _ => air(),
        };
        assert_eq!(
            fluid_height_with(0, 64, 0, &cross),
            Some((EntityFluidKind::Water, 8.0f32 / 9.0f32))
        );
        let capped = |x: i32, y: i32, z: i32| match (x, y, z) {
            (0, 64, 0) => water(8),
            (0, 65, 0) => stone(),
            _ => air(),
        };
        assert_eq!(
            fluid_height_with(0, 64, 0, &capped),
            Some((EntityFluidKind::Water, 8.0f32 / 9.0f32))
        );
    }

    #[test]
    fn fluid_is_solid_face_ignores_same_fluid_and_ice_but_never_upward() {
        // Same fluid in the queried cell is not a solid face in any direction.
        let same = |_x: i32, _y: i32, _z: i32| water(8);
        for dir in 0u8..6 {
            assert!(
                !fluid_is_solid_face(0, 64, 0, dir, EntityFluidKind::Water, &same),
                "same fluid must not block direction {dir}"
            );
        }
        // UP is a solid face even over air: that is what makes a falling
        // column look for a wall.
        let empty = |_x: i32, _y: i32, _z: i32| air();
        assert!(fluid_is_solid_face(
            0,
            64,
            0,
            1,
            EntityFluidKind::Water,
            &empty
        ));
        // Air side faces are not.
        for dir in [0u8, 2, 3, 4, 5] {
            assert!(!fluid_is_solid_face(
                0,
                64,
                0,
                dir,
                EntityFluidKind::Water,
                &empty
            ));
        }
        // Ice is explicitly not a face, in every direction but UP.
        let frozen = |_x: i32, _y: i32, _z: i32| ice();
        for dir in [0u8, 2, 3, 4, 5] {
            assert!(
                !fluid_is_solid_face(0, 64, 0, dir, EntityFluidKind::Water, &frozen),
                "ice must not block direction {dir}"
            );
        }
        assert!(fluid_is_solid_face(
            0,
            64,
            0,
            1,
            EntityFluidKind::Water,
            &frozen
        ));
        // A full stone block blocks every horizontal face, and a shape-only
        // obstacle (a pane, a fence) blocks none.
        let solid = |_x: i32, _y: i32, _z: i32| stone();
        let panes = |_x: i32, _y: i32, _z: i32| glass_pane();
        for dir in [0u8, 2, 3, 4, 5] {
            assert!(fluid_is_solid_face(
                0,
                64,
                0,
                dir,
                EntityFluidKind::Water,
                &solid
            ));
            assert!(!fluid_is_solid_face(
                0,
                64,
                0,
                dir,
                EntityFluidKind::Water,
                &panes
            ));
        }
        // The residual predicate answers the same question from a cell.
        assert!(fluid_cell_solid_face(stone(), 3));
        assert!(!fluid_cell_solid_face(glass_pane(), 3));
        assert!(!fluid_cell_solid_face(ice(), 3));
        // An out-of-range Direction ordinal has no face bit to read.
        assert!(!fluid_cell_solid_face(stone(), 6));
    }

    #[test]
    fn fluid_flow_slope_stepdown_and_falling_wall() {
        let source = EntityFluidState {
            kind: EntityFluidKind::Water,
            amount: 8,
            falling: false,
        };
        // Source at (0,64,0), one lower level east at (1,64,0), stone on the
        // other three sides: the flow points +x only.
        let slope = |x: i32, y: i32, z: i32| match (x, y, z) {
            (0, 64, 0) => water(8),
            (1, 64, 0) => water(7),
            (-1, 64, 0) | (0, 64, -1) | (0, 64, 1) => stone(),
            _ => air(),
        };
        let flow = fluid_flow_with(0, 64, 0, source, &slope);
        assert!(flow[0] > 0.0, "flow must point east: {flow:?}");
        assert_eq!(flow[1], 0.0);
        assert_eq!(flow[2], 0.0);
        assert!(
            (flow[0] - 1.0).abs() < 1e-6,
            "normalized east flow: {flow:?}"
        );

        // A step down east of the source adds the `own - (f - 8/9)` bonus.
        let stepdown = |x: i32, y: i32, z: i32| match (x, y, z) {
            (0, 64, 0) => water(8),
            (1, 64, 0) => air(),
            (1, 63, 0) => water(8),
            _ => air(),
        };
        let flow = fluid_flow_with(0, 64, 0, source, &stepdown);
        assert!(flow[0] > 0.0 && flow[1] == 0.0, "step-down flow: {flow:?}");

        // A falling column next to a wall gets the -6.0 y frame, then
        // normalizes to exactly (0, -1, 0).
        let falling = EntityFluidState {
            kind: EntityFluidKind::Water,
            amount: 8,
            falling: true,
        };
        let wall = |x: i32, y: i32, z: i32| match (x, y, z) {
            (0, 64, 0) => falling_water(),
            (1, 64, 0) => stone(),
            _ => air(),
        };
        assert_eq!(fluid_flow_with(0, 64, 0, falling, &wall), [0.0, -1.0, 0.0]);
        // Ice next to a falling column is not a wall, so the y frame stays 0.
        let ice_wall = |x: i32, y: i32, z: i32| match (x, y, z) {
            (0, 64, 0) => falling_water(),
            (1, 64, 0) => ice(),
            _ => air(),
        };
        assert_eq!(
            fluid_flow_with(0, 64, 0, falling, &ice_wall),
            [0.0, 0.0, 0.0]
        );
        // A full stone block one cell above the ice wall is still solid, so the
        // falling frame returns.
        let ice_under_stone = |x: i32, y: i32, z: i32| match (x, y, z) {
            (0, 64, 0) => falling_water(),
            (1, 64, 0) => ice(),
            (1, 65, 0) => stone(),
            _ => air(),
        };
        assert_eq!(
            fluid_flow_with(0, 64, 0, falling, &ice_under_stone),
            [0.0, -1.0, 0.0]
        );
    }

    #[test]
    fn fluid_push_finalize_averages_normalizes_and_applies_the_min_speed_frame() {
        // A mob normalizes the averaged flow before the motion scale.
        let push = fluid_push_finalize([2.0, 0.0, 0.0], 2, false, WATER_MOTION_SCALE, [0.0; 3]);
        assert_eq!(push, [WATER_MOTION_SCALE, 0.0, 0.0]);
        // A player keeps the raw average, so a 2-cell average halves it.
        let push = fluid_push_finalize([2.0, 0.0, 0.0], 2, true, WATER_MOTION_SCALE, [0.0; 3]);
        assert!((push[0] - WATER_MOTION_SCALE).abs() < 1e-15);
        assert_eq!(push[0], 0.014);
        // A zero accumulation has no direction to push in.
        assert_eq!(
            fluid_push_finalize([0.0; 3], 3, false, WATER_MOTION_SCALE, [0.0; 3]),
            [0.0; 3]
        );
        // Below the velocity threshold a too-slow push is lifted to the
        // pinned minimum in the same direction.
        let weak = fluid_push_finalize([1.0, 0.0, 0.0], 1, true, 0.001, [0.0, 0.0, 0.0]);
        assert_eq!(weak, [FLUID_PUSH_MIN_SPEED, 0.0, 0.0]);
        // Above it the raw scaled push survives untouched.
        let moving = fluid_push_finalize([1.0, 0.0, 0.0], 1, true, 0.001, [0.01, 0.0, 0.0]);
        assert_eq!(moving, [0.001, 0.0, 0.0]);
    }

    #[test]
    fn update_fluid_height_and_push_reads_only_the_deflated_box() {
        // A water source filling the cell the box sits in.
        let world = |x: i32, y: i32, z: i32| match (x, y, z) {
            (0, 64, 0) => water(8),
            _ => air(),
        };
        let box_ = FluidAabb {
            min_x: 0.25,
            min_y: 64.25,
            min_z: 0.25,
            max_x: 0.75,
            max_y: 65.0,
            max_z: 0.75,
        };
        // The height is computed in `float` and only then widened, which is the
        // pinned ordering: `(float) pos.y + getOwnHeight(...)`.
        let expected = f64::from(64.0f32 + 8.0f32 / 9.0f32) - (64.25 + 0.001);
        let result = update_fluid_height_and_push_with(
            box_,
            EntityFluidKind::Water,
            WATER_MOTION_SCALE,
            false,
            false,
            [0.0; 3],
            &world,
        );
        assert!(result.in_fluid);
        assert_eq!(result.max_height, expected);
        assert_eq!(result.push, [0.0; 3]);

        // A box whose min_x is exactly 1.0 deflates to 0.999: the cell it no
        // longer reaches is not water.
        let flush = FluidAabb {
            min_x: 1.0,
            min_y: 64.25,
            min_z: 0.25,
            max_x: 1.75,
            max_y: 65.0,
            max_z: 0.75,
        };
        let result = update_fluid_height_and_push_with(
            flush,
            EntityFluidKind::Water,
            WATER_MOTION_SCALE,
            false,
            false,
            [0.0; 3],
            &world,
        );
        assert!(!result.in_fluid, "deflate(0.001) must exclude a flush box");
        assert_eq!(result.max_height, 0.0);

        // Lava never answers water's sweep.
        let result = update_fluid_height_and_push_with(
            box_,
            EntityFluidKind::Lava,
            LAVA_ULTRA_WARM_MOTION_SCALE,
            false,
            false,
            [0.0; 3],
            &world,
        );
        assert!(!result.in_fluid && result.max_height == 0.0);
    }

    #[test]
    fn update_fluid_height_and_push_reports_the_flow_push() {
        // Source at (0,64,0) with a lower level east, stone on the other sides.
        let world = |x: i32, y: i32, z: i32| match (x, y, z) {
            (0, 64, 0) => water(8),
            (1, 64, 0) => water(7),
            (-1, 64, 0) | (0, 64, -1) | (0, 64, 1) => stone(),
            _ => air(),
        };
        let box_ = FluidAabb {
            min_x: 0.25,
            min_y: 64.25,
            min_z: 0.25,
            max_x: 0.75,
            max_y: 65.0,
            max_z: 0.75,
        };
        let mob = update_fluid_height_and_push_with(
            box_,
            EntityFluidKind::Water,
            WATER_MOTION_SCALE,
            true,
            false,
            [0.0; 3],
            &world,
        );
        assert!(mob.in_fluid);
        // A mob normalizes the flow, so the push magnitude is the full scale.
        assert!((mob.push[0] - WATER_MOTION_SCALE).abs() < 1e-9, "{mob:?}");
        assert_eq!(mob.push[1], 0.0);
        assert_eq!(mob.push[2], 0.0);
        // A stationary player in a weak sub-0.4 flow is lifted to the minimum
        // speed instead of the raw scaled push.
        let player = update_fluid_height_and_push_with(
            box_,
            EntityFluidKind::Water,
            WATER_MOTION_SCALE,
            true,
            true,
            [0.0; 3],
            &world,
        );
        assert!(player.push[0] > 0.0);
        assert!(
            player.push[0] >= FLUID_PUSH_MIN_SPEED - 1e-12,
            "min-speed frame: {player:?}"
        );
        // Pushing disabled leaves the height but clears the push.
        let still = update_fluid_height_and_push_with(
            box_,
            EntityFluidKind::Water,
            WATER_MOTION_SCALE,
            false,
            false,
            [0.0; 3],
            &world,
        );
        assert_eq!(still.max_height, mob.max_height);
        assert_eq!(still.push, [0.0; 3]);
    }

    #[test]
    fn lava_height_reads_max_height_not_in_fluid() {
        // `Entity.isInLava` is `getFluidHeight(LAVA) > 0.0`, which is why the
        // lava pass reports a height without water's `in_fluid` question.
        let world = |x: i32, y: i32, z: i32| match (x, y, z) {
            (0, 64, 0) => lava(8),
            _ => air(),
        };
        let box_ = FluidAabb {
            min_x: 0.25,
            min_y: 64.25,
            min_z: 0.25,
            max_x: 0.75,
            max_y: 65.0,
            max_z: 0.75,
        };
        for ultra_warm in [false, true] {
            let result = update_fluid_height_and_push_with(
                box_,
                EntityFluidKind::Lava,
                fluid_push_scale(EntityFluidKind::Lava, ultra_warm),
                false,
                false,
                [0.0; 3],
                &world,
            );
            assert!(result.in_fluid, "ultra_warm {ultra_warm}");
            assert!(result.max_height > 0.0);
        }
        // A single lava cell has no horizontal fall to flow along, so the
        // dimension scale cannot be observed until a flow exists.
        let flowing = |x: i32, y: i32, z: i32| match (x, y, z) {
            (0, 64, 0) => lava(8),
            (1, 64, 0) => lava(7),
            _ => air(),
        };
        let nether = update_fluid_height_and_push_with(
            box_,
            EntityFluidKind::Lava,
            fluid_push_scale(EntityFluidKind::Lava, true),
            true,
            false,
            [0.0; 3],
            &flowing,
        );
        let overworld = update_fluid_height_and_push_with(
            box_,
            EntityFluidKind::Lava,
            fluid_push_scale(EntityFluidKind::Lava, false),
            true,
            false,
            [0.0; 3],
            &flowing,
        );
        assert!(nether.push[0] > 0.0 && overworld.push[0] > 0.0);
        assert!(
            nether.push[0] > overworld.push[0],
            "ultra-warm lava pushes harder"
        );
    }

    #[test]
    fn fluid_sweep_window_matches_floor_ceil_of_the_deflated_box() {
        let aabb = FluidAabb {
            min_x: 0.0,
            min_y: 64.0,
            min_z: 0.0,
            max_x: 0.6,
            max_y: 65.8,
            max_z: 0.6,
        };
        let deflated_box = aabb.deflate(0.001);
        assert_eq!(deflated_box.min_x, 0.001);
        assert_eq!(
            fluid_sweep_window(deflated_box),
            FluidSweepWindow {
                x0: 0,
                x1: 1,
                y0: 64,
                y1: 66,
                z0: 0,
                z1: 1,
            }
        );
    }

    #[test]
    fn fluid_result_from_cells_matches_the_boxed_sweep() {
        let world = |x: i32, y: i32, z: i32| match (x, y, z) {
            (0, 64, 0) => water(8),
            (1, 64, 0) => water(7),
            (-1, 64, 0) | (0, 64, -1) | (0, 64, 1) => stone(),
            (0, 63, 0) | (1, 63, 0) => stone(),
            _ => air(),
        };
        let aabb = FluidAabb {
            min_x: 0.0,
            min_y: 64.0,
            min_z: 0.0,
            max_x: 0.6,
            max_y: 65.8,
            max_z: 0.6,
        };
        let deflated_box = aabb.deflate(0.001);
        let window = fluid_sweep_window(deflated_box);
        let mut cells = Vec::new();
        for bx in window.x0..window.x1 {
            for by in window.y0..window.y1 {
                for bz in window.z0..window.z1 {
                    if let Some(fs) = fluid_state_of(world(bx, by, bz)) {
                        cells.push((bx, by, bz, fs));
                    }
                }
            }
        }
        let vel = [0.001, 0.0, 0.002];
        let replay = fluid_result_from_cells(
            &cells,
            deflated_box,
            EntityFluidKind::Water,
            WATER_MOTION_SCALE,
            true,
            false,
            vel,
            &world,
        );
        let boxed = update_fluid_height_and_push_with(
            aabb,
            EntityFluidKind::Water,
            WATER_MOTION_SCALE,
            true,
            false,
            vel,
            &world,
        );
        assert_eq!(replay, boxed);
        assert!(boxed.in_fluid);
        assert!(boxed.push[0] > 0.0);
    }
}
