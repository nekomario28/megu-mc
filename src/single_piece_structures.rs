// Native Minecraft 1.21.1 single-piece procedural box structures:
// - `minecraft:buried_treasure` (`BuriedTreasurePiece`)
// - `minecraft:swamp_hut` (`SwampHutPiece`)
// - `minecraft:desert_pyramid` (`DesertPyramidPiece` + `DesertPyramidStructure::after_place`)
// - `minecraft:jungle_pyramid` (`JungleTemplePiece`)

use crate::generation_math::{hash_block_pos, legacy_world_factory_seed, Lcg};
use crate::generation_payload::{ContainerKind, GenerationPayload};
use std::collections::BTreeMap;

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) type Bounds = [i32; 6];
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) type SinglePiecePos = (i32, i32, i32);

/// Direction$Plane.HORIZONTAL iteration order (`[NORTH, EAST, SOUTH, WEST]`).
#[cfg_attr(not(test), allow(dead_code))]
const HORIZONTAL_FACINGS: [Facing; 4] = [Facing::North, Facing::East, Facing::South, Facing::West];

/// Common RNG interface shared by `Xor` (`WorldgenRandom`) and `Lcg` (`LegacyRandomSource`).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) trait StructureRng {
    fn next_bound(&mut self, bound: i32) -> i32;
    fn next_i64(&mut self) -> i64;
    fn next_f32(&mut self) -> f32;
    fn next_bool(&mut self) -> bool;
    fn next_inbetween(&mut self, min: i32, max: i32) -> i32 {
        if max < min {
            min
        } else {
            self.next_bound(max - min + 1) + min
        }
    }
}

impl StructureRng for Xor {
    fn next_bound(&mut self, bound: i32) -> i32 {
        Xor::next_bound(self, bound)
    }
    fn next_i64(&mut self) -> i64 {
        Xor::next_i64(self)
    }
    fn next_f32(&mut self) -> f32 {
        Xor::next_f32(self)
    }
    fn next_bool(&mut self) -> bool {
        Xor::next_bool(self)
    }
}

impl StructureRng for Lcg {
    fn next_bound(&mut self, bound: i32) -> i32 {
        Lcg::next_bound(self, bound)
    }
    fn next_i64(&mut self) -> i64 {
        Lcg::next_i64(self)
    }
    fn next_f32(&mut self) -> f32 {
        Lcg::next_f32(self)
    }
    fn next_bool(&mut self) -> bool {
        Lcg::next_bool(self)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) enum LootKind {
    Chest,
    Dispenser,
    Brushable,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct SinglePieceLoot {
    pub(crate) pos: SinglePiecePos,
    pub(crate) kind: LootKind,
    pub(crate) table: &'static str,
    pub(crate) seed: i64,
}

#[derive(Clone, Debug, Default)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct PlacementResult {
    pub(crate) loots: Vec<SinglePieceLoot>,
}

#[cfg_attr(not(test), allow(dead_code))]
fn sp_contains(b: &Bounds, p: SinglePiecePos) -> bool {
    p.0 >= b[0] && p.0 <= b[3] && p.1 >= b[1] && p.1 <= b[4] && p.2 >= b[2] && p.2 <= b[5]
}

#[cfg_attr(not(test), allow(dead_code))]
fn move_bounds(b: &mut Bounds, dx: i32, dy: i32, dz: i32) {
    b[0] = b[0].wrapping_add(dx);
    b[1] = b[1].wrapping_add(dy);
    b[2] = b[2].wrapping_add(dz);
    b[3] = b[3].wrapping_add(dx);
    b[4] = b[4].wrapping_add(dy);
    b[5] = b[5].wrapping_add(dz);
}

#[cfg_attr(not(test), allow(dead_code))]
fn bounding_box_center(b: &Bounds) -> SinglePiecePos {
    let x_span = b[3] - b[0] + 1;
    let y_span = b[4] - b[1] + 1;
    let z_span = b[5] - b[2] + 1;
    (b[0] + x_span / 2, b[1] + y_span / 2, b[2] + z_span / 2)
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn make_bounding_box(
    x: i32,
    y: i32,
    z: i32,
    dir: Facing,
    width: i32,
    height: i32,
    depth: i32,
) -> Bounds {
    match dir {
        Facing::North | Facing::South => [
            x,
            y,
            z,
            x.wrapping_add(width).wrapping_sub(1),
            y.wrapping_add(height).wrapping_sub(1),
            z.wrapping_add(depth).wrapping_sub(1),
        ],
        Facing::West | Facing::East => [
            x,
            y,
            z,
            x.wrapping_add(depth).wrapping_sub(1),
            y.wrapping_add(height).wrapping_sub(1),
            z.wrapping_add(width).wrapping_sub(1),
        ],
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn random_horizontal_facing<R: StructureRng + ?Sized>(rng: &mut R) -> Facing {
    HORIZONTAL_FACINGS[rng.next_bound(4) as usize]
}

#[cfg_attr(not(test), allow(dead_code))]
fn facing_name(f: Facing) -> &'static str {
    match f {
        Facing::North => "north",
        Facing::South => "south",
        Facing::West => "west",
        Facing::East => "east",
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn facing_opposite(f: Facing) -> Facing {
    match f {
        Facing::North => Facing::South,
        Facing::South => Facing::North,
        Facing::West => Facing::East,
        Facing::East => Facing::West,
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn facing_cw(f: Facing) -> Facing {
    match f {
        Facing::North => Facing::East,
        Facing::East => Facing::South,
        Facing::South => Facing::West,
        Facing::West => Facing::North,
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn facing_step(f: Facing) -> (i32, i32) {
    match f {
        Facing::North => (0, -1),
        Facing::South => (0, 1),
        Facing::West => (-1, 0),
        Facing::East => (1, 0),
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn facing_data2d(f: Facing) -> usize {
    match f {
        Facing::South => 0,
        Facing::West => 1,
        Facing::North => 2,
        Facing::East => 3,
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn sp_state(reg: &BReg, name: &str, props: &[(&str, &str)]) -> u32 {
    if props.is_empty() {
        return reg.state_of(name, None);
    }
    let props = props
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect::<Vec<_>>();
    reg.checked_state(name, &props)
        .expect("single-piece native block state")
}

#[cfg_attr(not(test), allow(dead_code))]
fn block_pos_as_long((x, y, z): SinglePiecePos) -> i64 {
    ((i64::from(x) & 0x3FF_FFFF) << 38)
        | ((i64::from(z) & 0x3FF_FFFF) << 12)
        | (i64::from(y) & 0xFFF)
}

#[cfg_attr(not(test), allow(dead_code))]
fn world_pos(bounds: &Bounds, orient: Option<Facing>, (lx, ly, lz): SinglePiecePos) -> SinglePiecePos {
    let Some(o) = orient else {
        return (lx, ly, lz);
    };
    let (wx, wz) = match o {
        Facing::North => (bounds[0].wrapping_add(lx), bounds[5].wrapping_sub(lz)),
        Facing::South => (bounds[0].wrapping_add(lx), bounds[2].wrapping_add(lz)),
        Facing::West => (bounds[3].wrapping_sub(lz), bounds[2].wrapping_add(lx)),
        Facing::East => (bounds[0].wrapping_add(lz), bounds[2].wrapping_add(lx)),
    };
    (wx, bounds[1].wrapping_add(ly), wz)
}

#[cfg_attr(not(test), allow(dead_code))]
fn transform_state(orient: Option<Facing>, st: u32) -> u32 {
    let (m, r) = match orient {
        Some(Facing::South) => (Mirror::LeftRight, Rotation::None),
        Some(Facing::West) => (Mirror::LeftRight, Rotation::Clockwise90),
        Some(Facing::East) => (Mirror::None, Rotation::Clockwise90),
        _ => (Mirror::None, Rotation::None),
    };
    structure_transform::state(st, m, r).expect("single-piece state transform")
}

#[cfg_attr(not(test), allow(dead_code))]
struct PieceWriter<'a, 'g> {
    ctx: &'a mut FC<'g>,
    reg: &'static BReg,
    bounds: &'a mut Bounds,
    orient: Option<Facing>,
    clip: Bounds,
    loots: BTreeMap<SinglePiecePos, SinglePieceLoot>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl<'a, 'g> PieceWriter<'a, 'g> {
    fn world_pos(&self, local: SinglePiecePos) -> SinglePiecePos {
        world_pos(self.bounds, self.orient, local)
    }

    fn set_world_block(&mut self, pos: SinglePiecePos, st: u32) {
        self.ctx.set(pos.0, pos.1, pos.2, st);
        if self.loots.contains_key(&pos) {
            let name = self.reg.name_of(st);
            if name != "minecraft:chest"
                && name != "minecraft:dispenser"
                && name != "minecraft:suspicious_sand"
            {
                self.loots.remove(&pos);
            }
        }
    }

    fn place_block(&mut self, st: u32, lx: i32, ly: i32, lz: i32) {
        let wp = self.world_pos((lx, ly, lz));
        if !sp_contains(&self.clip, wp) {
            return;
        }
        let transformed = transform_state(self.orient, st);
        self.set_world_block(wp, transformed);
    }

    fn get_block(&mut self, lx: i32, ly: i32, lz: i32) -> u32 {
        let wp = self.world_pos((lx, ly, lz));
        if !sp_contains(&self.clip, wp) {
            0
        } else {
            self.ctx.get(wp.0, wp.1, wp.2)
        }
    }

    fn generate_air_box(&mut self, x0: i32, y0: i32, z0: i32, x1: i32, y1: i32, z1: i32) {
        let air = self.reg.state_of("minecraft:air", None);
        for ly in y0..=y1 {
            for lx in x0..=x1 {
                for lz in z0..=z1 {
                    self.place_block(air, lx, ly, lz);
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn generate_box(
        &mut self,
        x0: i32,
        y0: i32,
        z0: i32,
        x1: i32,
        y1: i32,
        z1: i32,
        edge: u32,
        fill: u32,
        skip_air: bool,
    ) {
        for ly in y0..=y1 {
            for lx in x0..=x1 {
                for lz in z0..=z1 {
                    if skip_air && self.reg.is_air(self.get_block(lx, ly, lz)) {
                        continue;
                    }
                    let is_edge =
                        ly == y0 || ly == y1 || lx == x0 || lx == x1 || lz == z0 || lz == z1;
                    self.place_block(if is_edge { edge } else { fill }, lx, ly, lz);
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn generate_box_selector<R: StructureRng + ?Sized, F>(
        &mut self,
        x0: i32,
        y0: i32,
        z0: i32,
        x1: i32,
        y1: i32,
        z1: i32,
        skip_air: bool,
        rng: &mut R,
        mut select: F,
    ) where
        F: FnMut(&mut R, i32, i32, i32, bool) -> u32,
    {
        for ly in y0..=y1 {
            for lx in x0..=x1 {
                for lz in z0..=z1 {
                    if skip_air && self.reg.is_air(self.get_block(lx, ly, lz)) {
                        continue;
                    }
                    let is_edge =
                        ly == y0 || ly == y1 || lx == x0 || lx == x1 || lz == z0 || lz == z1;
                    let st = select(rng, lx, ly, lz, is_edge);
                    self.place_block(st, lx, ly, lz);
                }
            }
        }
    }

    fn is_replaceable_by_structures(&self, st: u32) -> bool {
        if self.reg.is_air(st) || self.reg.is_liquid(st) {
            return true;
        }
        matches!(
            self.reg.name_of(st),
            "minecraft:glow_lichen" | "minecraft:seagrass" | "minecraft:tall_seagrass"
        )
    }

    fn fill_column_down(&mut self, st: u32, lx: i32, ly: i32, lz: i32) {
        let mut wp = self.world_pos((lx, ly, lz));
        if !sp_contains(&self.clip, wp) {
            return;
        }
        let min_y_plus_one = self.ctx.g.min_y + 1;
        while wp.1 > min_y_plus_one {
            let cur = self.ctx.get(wp.0, wp.1, wp.2);
            if !self.is_replaceable_by_structures(cur) {
                break;
            }
            self.set_world_block(wp, st);
            wp.1 -= 1;
        }
    }

    /// `StructurePiece.reorient(BlockGetter, BlockPos, BlockState)`
    fn orient_chest(&mut self, pos: SinglePiecePos) -> u32 {
        let mut solid_wall: Option<Facing> = None;
        for dir in HORIZONTAL_FACINGS {
            let (dx, dz) = facing_step(dir);
            let np = (pos.0 + dx, pos.1, pos.2 + dz);
            let nst = self.ctx.get(np.0, np.1, np.2);
            if self.reg.name_of(nst) == "minecraft:chest" {
                return sp_state(self.reg, "minecraft:chest", &[("facing", "north")]);
            }
            if self.reg.is_solid_render(nst) {
                if solid_wall.is_none() {
                    solid_wall = Some(dir);
                } else {
                    solid_wall = None;
                    break;
                }
            }
        }
        if let Some(wall) = solid_wall {
            return sp_state(
                self.reg,
                "minecraft:chest",
                &[("facing", facing_name(facing_opposite(wall)))],
            );
        }
        let mut out_dir = Facing::North;
        let step0 = facing_step(out_dir);
        if self
            .reg
            .is_solid_render(self.ctx.get(pos.0 + step0.0, pos.1, pos.2 + step0.1))
        {
            out_dir = facing_opposite(out_dir);
        }
        let step1 = facing_step(out_dir);
        if self
            .reg
            .is_solid_render(self.ctx.get(pos.0 + step1.0, pos.1, pos.2 + step1.1))
        {
            out_dir = facing_cw(out_dir);
        }
        let step2 = facing_step(out_dir);
        if self
            .reg
            .is_solid_render(self.ctx.get(pos.0 + step2.0, pos.1, pos.2 + step2.1))
        {
            out_dir = facing_opposite(out_dir);
        }
        sp_state(
            self.reg,
            "minecraft:chest",
            &[("facing", facing_name(out_dir))],
        )
    }

    fn create_chest_world<R: StructureRng + ?Sized>(
        &mut self,
        rng: &mut R,
        wp: SinglePiecePos,
        table: &'static str,
    ) -> bool {
        if !sp_contains(&self.clip, wp) {
            return false;
        }
        if self.reg.name_of(self.ctx.get(wp.0, wp.1, wp.2)) == "minecraft:chest" {
            return false;
        }
        let chest = self.orient_chest(wp);
        self.set_world_block(wp, chest);
        let seed = rng.next_i64();
        self.ctx.payloads.push((
            wp,
            GenerationPayload::LootContainer {
                kind: ContainerKind::Chest,
                loot_table: table.to_string(),
                loot_seed: Some(seed),
            },
        ));
        self.loots.insert(
            wp,
            SinglePieceLoot {
                pos: wp,
                kind: LootKind::Chest,
                table,
                seed,
            },
        );
        true
    }

    fn create_chest_local<R: StructureRng + ?Sized>(
        &mut self,
        rng: &mut R,
        lx: i32,
        ly: i32,
        lz: i32,
        table: &'static str,
    ) -> bool {
        let wp = self.world_pos((lx, ly, lz));
        self.create_chest_world(rng, wp, table)
    }

    fn create_dispenser<R: StructureRng + ?Sized>(
        &mut self,
        rng: &mut R,
        lx: i32,
        ly: i32,
        lz: i32,
        facing: Facing,
        table: &'static str,
    ) -> bool {
        let wp = self.world_pos((lx, ly, lz));
        if !sp_contains(&self.clip, wp) {
            return false;
        }
        if self.reg.name_of(self.ctx.get(wp.0, wp.1, wp.2)) == "minecraft:dispenser" {
            return false;
        }
        let disp = sp_state(
            self.reg,
            "minecraft:dispenser",
            &[("facing", facing_name(facing))],
        );
        self.place_block(disp, lx, ly, lz);
        let seed = rng.next_i64();
        // `RandomizableContainer.setLootTable`: the dispenser is a 9-slot block container, so its
        // table belongs in the store the hopper tick, `fire_dispenser` and the break-spill all
        // read by position, not just in the returned loot record.
        self.ctx.payloads.push((
            wp,
            GenerationPayload::LootContainer {
                kind: ContainerKind::BlockContainer(9),
                loot_table: table.to_string(),
                loot_seed: Some(seed),
            },
        ));
        self.loots.insert(
            wp,
            SinglePieceLoot {
                pos: wp,
                kind: LootKind::Dispenser,
                table,
                seed,
            },
        );
        true
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct ScatteredHeightState {
    pub(crate) h_pos: i32,
}

impl Default for ScatteredHeightState {
    fn default() -> Self {
        Self { h_pos: -1 }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn update_average_ground_height<H>(
    bounds: &mut Bounds,
    hs: &mut ScatteredHeightState,
    ctx: &mut FC<'_>,
    clip: &Bounds,
    y_offset: i32,
    hm: &mut H,
) -> bool
where
    H: FnMut(&mut FC<'_>, HeightmapKind, i32, i32) -> i32,
{
    if hs.h_pos >= 0 {
        return true;
    }
    let mut sum = 0i32;
    let mut count = 0i32;
    for wz in bounds[2]..=bounds[5] {
        for wx in bounds[0]..=bounds[3] {
            if sp_contains(clip, (wx, 64, wz)) {
                sum += hm(ctx, HeightmapKind::MotionBlockingNoLeaves, wx, wz);
                count += 1;
            }
        }
    }
    if count == 0 {
        return false;
    }
    hs.h_pos = sum / count;
    let dy = hs.h_pos - bounds[1] + y_offset;
    move_bounds(bounds, 0, dy, 0);
    true
}

#[cfg_attr(not(test), allow(dead_code))]
fn update_height_position_to_lowest_ground_height<H>(
    bounds: &mut Bounds,
    hs: &mut ScatteredHeightState,
    ctx: &mut FC<'_>,
    y_offset: i32,
    hm: &mut H,
) -> bool
where
    H: FnMut(&mut FC<'_>, HeightmapKind, i32, i32) -> i32,
{
    if hs.h_pos >= 0 {
        return true;
    }
    let mut min_y = ctx.g.min_y + ctx.g.height;
    let mut any = false;
    for wz in bounds[2]..=bounds[5] {
        for wx in bounds[0]..=bounds[3] {
            min_y = min_y.min(hm(ctx, HeightmapKind::MotionBlockingNoLeaves, wx, wz));
            any = true;
        }
    }
    if !any {
        return false;
    }
    hs.h_pos = min_y;
    let dy = hs.h_pos - bounds[1] + y_offset;
    move_bounds(bounds, 0, dy, 0);
    true
}

// ============================================================================
// 1. minecraft:buried_treasure (BuriedTreasurePieces$BuriedTreasurePiece)
// ============================================================================

#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct BuriedTreasurePiece {
    pub(crate) bounds: Bounds,
}

#[cfg_attr(not(test), allow(dead_code))]
impl BuriedTreasurePiece {
    pub(crate) fn new(chunk_x: i32, chunk_z: i32) -> Self {
        let x = (chunk_x << 4) + 9;
        let z = (chunk_z << 4) + 9;
        Self {
            bounds: [x, 90, z, x, 90, z],
        }
    }

    pub(crate) fn post_process<R: StructureRng + ?Sized>(
        &mut self,
        ctx: &mut FC<'_>,
        post_rng: &mut R,
        clip: Bounds,
    ) -> PlacementResult {
        self.post_process_with_heightmap(ctx, post_rng, clip, &mut |ctx, kind, x, z| {
            ctx.heightmap(kind, x, z)
        })
    }

    pub(crate) fn post_process_with_heightmap<R: StructureRng + ?Sized, H>(
        &mut self,
        ctx: &mut FC<'_>,
        post_rng: &mut R,
        clip: Bounds,
        hm: &mut H,
    ) -> PlacementResult
    where
        H: FnMut(&mut FC<'_>, HeightmapKind, i32, i32) -> i32,
    {
        let reg = breg().expect("block registry initialized");
        let wx = self.bounds[0];
        let wz = self.bounds[2];
        let start_y = hm(ctx, HeightmapKind::OceanFloorWg, wx, wz);

        let st_sandstone = reg.state_of("minecraft:sandstone", None);
        let st_stone = reg.state_of("minecraft:stone", None);
        let st_andesite = reg.state_of("minecraft:andesite", None);
        let st_granite = reg.state_of("minecraft:granite", None);
        let st_diorite = reg.state_of("minecraft:diorite", None);
        let st_sand = reg.state_of("minecraft:sand", None);
        let st_water = reg.state_of("minecraft:water", None);
        let st_lava = reg.state_of("minecraft:lava", None);

        let is_liquid_default = |st: u32| st == st_water || st == st_lava;

        let mut y = start_y;
        let min_y = ctx.g.min_y;
        let mut loots = BTreeMap::new();

        while y > min_y {
            let cur_state = ctx.get(wx, y, wz);
            let below_state = ctx.get(wx, y - 1, wz);
            if below_state == st_sandstone
                || below_state == st_stone
                || below_state == st_andesite
                || below_state == st_granite
                || below_state == st_diorite
            {
                let fill_state = if reg.is_air(cur_state) || is_liquid_default(cur_state) {
                    st_sand
                } else {
                    cur_state
                };

                // Direction.values() order: DOWN, UP, NORTH, SOUTH, WEST, EAST
                const DIRS_3D: [(i32, i32, i32, bool); 6] = [
                    (0, -1, 0, false),
                    (0, 1, 0, true),
                    (0, 0, -1, false),
                    (0, 0, 1, false),
                    (-1, 0, 0, false),
                    (1, 0, 0, false),
                ];
                for (dx, dy, dz, is_up) in DIRS_3D {
                    let np = (wx + dx, y + dy, wz + dz);
                    let nst = ctx.get(np.0, np.1, np.2);
                    if reg.is_air(nst) || is_liquid_default(nst) {
                        let below_np = ctx.get(np.0, np.1 - 1, np.2);
                        let chosen =
                            if (reg.is_air(below_np) || is_liquid_default(below_np)) && !is_up {
                                below_state
                            } else {
                                fill_state
                            };
                        ctx.set(np.0, np.1, np.2, chosen);
                    }
                }

                self.bounds = [wx, y, wz, wx, y, wz];
                let mut w = PieceWriter {
                    ctx,
                    reg,
                    bounds: &mut self.bounds,
                    orient: None,
                    clip,
                    loots,
                };
                w.create_chest_world(post_rng, (wx, y, wz), "minecraft:chests/buried_treasure");
                loots = w.loots;
                break;
            }
            y -= 1;
        }

        PlacementResult {
            loots: loots.into_values().collect(),
        }
    }
}

// ============================================================================
// 2. minecraft:swamp_hut (SwampHutPiece)
// ============================================================================

#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct SwampHutPiece {
    pub(crate) bounds: Bounds,
    pub(crate) orientation: Facing,
    pub(crate) height_state: ScatteredHeightState,
    pub(crate) spawned_witch: bool,
    pub(crate) spawned_cat: bool,
}

#[cfg_attr(not(test), allow(dead_code))]
impl SwampHutPiece {
    pub(crate) fn new<R: StructureRng + ?Sized>(rng: &mut R, x: i32, z: i32) -> Self {
        let orientation = random_horizontal_facing(rng);
        Self::with_orientation(x, z, orientation)
    }

    pub(crate) fn with_orientation(x: i32, z: i32, orientation: Facing) -> Self {
        Self {
            bounds: make_bounding_box(x, 64, z, orientation, 7, 7, 9),
            orientation,
            height_state: ScatteredHeightState::default(),
            spawned_witch: false,
            spawned_cat: false,
        }
    }

    pub(crate) fn post_process(
        &mut self,
        ctx: &mut FC<'_>,
        clip: Bounds,
    ) -> PlacementResult {
        self.post_process_with_heightmap(ctx, clip, &mut |ctx, kind, x, z| {
            ctx.heightmap(kind, x, z)
        })
    }

    pub(crate) fn post_process_with_heightmap<H>(
        &mut self,
        ctx: &mut FC<'_>,
        clip: Bounds,
        hm: &mut H,
    ) -> PlacementResult
    where
        H: FnMut(&mut FC<'_>, HeightmapKind, i32, i32) -> i32,
    {
        if !update_average_ground_height(
            &mut self.bounds,
            &mut self.height_state,
            ctx,
            &clip,
            0,
            hm,
        ) {
            return PlacementResult::default();
        }
        let reg = breg().expect("block registry initialized");
        let spruce_planks = reg.state_of("minecraft:spruce_planks", None);
        let oak_log = reg.state_of("minecraft:oak_log", None);
        let oak_fence = reg.state_of("minecraft:oak_fence", None);
        let air = reg.state_of("minecraft:air", None);
        let potted_red_mushroom = reg.state_of("minecraft:potted_red_mushroom", None);
        let crafting_table = reg.state_of("minecraft:crafting_table", None);
        let cauldron = reg.state_of("minecraft:cauldron", None);

        let st_n = sp_state(reg, "minecraft:spruce_stairs", &[("facing", "north")]);
        let st_e = sp_state(reg, "minecraft:spruce_stairs", &[("facing", "east")]);
        let st_w = sp_state(reg, "minecraft:spruce_stairs", &[("facing", "west")]);
        let st_s = sp_state(reg, "minecraft:spruce_stairs", &[("facing", "south")]);
        let st_n_or = sp_state(
            reg,
            "minecraft:spruce_stairs",
            &[("facing", "north"), ("shape", "outer_right")],
        );
        let st_n_ol = sp_state(
            reg,
            "minecraft:spruce_stairs",
            &[("facing", "north"), ("shape", "outer_left")],
        );
        let st_s_ol = sp_state(
            reg,
            "minecraft:spruce_stairs",
            &[("facing", "south"), ("shape", "outer_left")],
        );
        let st_s_or = sp_state(
            reg,
            "minecraft:spruce_stairs",
            &[("facing", "south"), ("shape", "outer_right")],
        );

        let mut w = PieceWriter {
            ctx,
            reg,
            bounds: &mut self.bounds,
            orient: Some(self.orientation),
            clip,
            loots: BTreeMap::new(),
        };

        w.generate_box(1, 1, 1, 5, 1, 7, spruce_planks, spruce_planks, false);
        w.generate_box(1, 4, 2, 5, 4, 7, spruce_planks, spruce_planks, false);
        w.generate_box(2, 1, 0, 4, 1, 0, spruce_planks, spruce_planks, false);
        w.generate_box(2, 2, 2, 3, 3, 2, spruce_planks, spruce_planks, false);
        w.generate_box(1, 2, 3, 1, 3, 6, spruce_planks, spruce_planks, false);
        w.generate_box(5, 2, 3, 5, 3, 6, spruce_planks, spruce_planks, false);
        w.generate_box(2, 2, 7, 4, 3, 7, spruce_planks, spruce_planks, false);

        w.generate_box(1, 0, 2, 1, 3, 2, oak_log, oak_log, false);
        w.generate_box(5, 0, 2, 5, 3, 2, oak_log, oak_log, false);
        w.generate_box(1, 0, 7, 1, 3, 7, oak_log, oak_log, false);
        w.generate_box(5, 0, 7, 5, 3, 7, oak_log, oak_log, false);

        w.place_block(oak_fence, 2, 3, 2);
        w.place_block(oak_fence, 3, 3, 7);
        w.place_block(air, 1, 3, 4);
        w.place_block(air, 5, 3, 4);
        w.place_block(air, 5, 3, 5);
        w.place_block(potted_red_mushroom, 1, 3, 5);
        w.place_block(crafting_table, 3, 2, 6);
        w.place_block(cauldron, 4, 2, 6);
        w.place_block(oak_fence, 1, 2, 1);
        w.place_block(oak_fence, 5, 2, 1);

        w.generate_box(0, 4, 1, 6, 4, 1, st_n, st_n, false);
        w.generate_box(0, 4, 2, 0, 4, 7, st_e, st_e, false);
        w.generate_box(6, 4, 2, 6, 4, 7, st_w, st_w, false);
        w.generate_box(0, 4, 8, 6, 4, 8, st_s, st_s, false);
        w.place_block(st_n_or, 0, 4, 1);
        w.place_block(st_n_ol, 6, 4, 1);
        w.place_block(st_s_ol, 0, 4, 8);
        w.place_block(st_s_or, 6, 4, 8);

        for z in (2..=7).step_by(5) {
            for x in (1..=5).step_by(4) {
                w.fill_column_down(oak_log, x, -1, z);
            }
        }

        if !self.spawned_witch {
            let wp = w.world_pos((2, 2, 5));
            if sp_contains(&w.clip, wp) {
                self.spawned_witch = true;
                w.ctx.push_entity(
                    "minecraft:witch",
                    [wp.0 as f64 + 0.5, wp.1 as f64, wp.2 as f64 + 0.5],
                    0.0,
                );
            }
        }
        if !self.spawned_cat {
            let wp = w.world_pos((2, 2, 5));
            if sp_contains(&w.clip, wp) {
                self.spawned_cat = true;
                w.ctx.push_entity(
                    "minecraft:cat",
                    [wp.0 as f64 + 0.5, wp.1 as f64, wp.2 as f64 + 0.5],
                    0.0,
                );
            }
        }

        PlacementResult {
            loots: w.loots.into_values().collect(),
        }
    }
}

// ============================================================================
// 3. minecraft:desert_pyramid (DesertPyramidPiece + DesertPyramidStructure)
// ============================================================================

#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct DesertPyramidPiece {
    pub(crate) bounds: Bounds,
    pub(crate) orientation: Facing,
    pub(crate) height_state: ScatteredHeightState,
    pub(crate) has_placed_chest: [bool; 4],
    pub(crate) potential_suspicious_sand_positions: Vec<SinglePiecePos>,
    pub(crate) random_collapsed_roof_pos: SinglePiecePos,
}

#[cfg_attr(not(test), allow(dead_code))]
impl DesertPyramidPiece {
    const WIDTH: i32 = 21;
    const DEPTH: i32 = 21;

    pub(crate) fn new<R: StructureRng + ?Sized>(rng: &mut R, x: i32, z: i32) -> Self {
        let orientation = random_horizontal_facing(rng);
        Self::with_orientation(x, z, orientation)
    }

    pub(crate) fn with_orientation(x: i32, z: i32, orientation: Facing) -> Self {
        Self {
            bounds: make_bounding_box(x, 64, z, orientation, Self::WIDTH, 15, Self::DEPTH),
            orientation,
            height_state: ScatteredHeightState::default(),
            has_placed_chest: [false; 4],
            potential_suspicious_sand_positions: Vec::new(),
            random_collapsed_roof_pos: (0, 0, 0),
        }
    }

    pub(crate) fn post_process<RP: StructureRng + ?Sized, RL: StructureRng + ?Sized>(
        &mut self,
        ctx: &mut FC<'_>,
        post_rng: &mut RP,
        level_rng: &mut RL,
        world_seed: i64,
        clip: Bounds,
    ) -> PlacementResult {
        self.post_process_with_heightmap(
            ctx,
            post_rng,
            level_rng,
            world_seed,
            clip,
            &mut |ctx, kind, x, z| ctx.heightmap(kind, x, z),
        )
    }

    pub(crate) fn post_process_with_heightmap<
        RP: StructureRng + ?Sized,
        RL: StructureRng + ?Sized,
        H,
    >(
        &mut self,
        ctx: &mut FC<'_>,
        post_rng: &mut RP,
        level_rng: &mut RL,
        world_seed: i64,
        clip: Bounds,
        hm: &mut H,
    ) -> PlacementResult
    where
        H: FnMut(&mut FC<'_>, HeightmapKind, i32, i32) -> i32,
    {
        let dy = -post_rng.next_bound(3);
        if !update_height_position_to_lowest_ground_height(
            &mut self.bounds,
            &mut self.height_state,
            ctx,
            dy,
            hm,
        ) {
            return PlacementResult::default();
        }
        let reg = breg().expect("block registry initialized");
        let sandstone = reg.state_of("minecraft:sandstone", None);
        let cut_sandstone = reg.state_of("minecraft:cut_sandstone", None);
        let chiseled_sandstone = reg.state_of("minecraft:chiseled_sandstone", None);
        let sandstone_slab = reg.state_of("minecraft:sandstone_slab", None);
        let orange_terracotta = reg.state_of("minecraft:orange_terracotta", None);
        let blue_terracotta = reg.state_of("minecraft:blue_terracotta", None);
        let stone_pressure_plate = reg.state_of("minecraft:stone_pressure_plate", None);
        let tnt = reg.state_of("minecraft:tnt", None);
        let sand = reg.state_of("minecraft:sand", None);
        let air = reg.state_of("minecraft:air", None);

        let st_n = sp_state(reg, "minecraft:sandstone_stairs", &[("facing", "north")]);
        let st_s = sp_state(reg, "minecraft:sandstone_stairs", &[("facing", "south")]);
        let st_e = sp_state(reg, "minecraft:sandstone_stairs", &[("facing", "east")]);
        let st_w = sp_state(reg, "minecraft:sandstone_stairs", &[("facing", "west")]);
        let st_ccw90 = structure_transform::state(
            reg.state_of("minecraft:sandstone_stairs", None),
            Mirror::None,
            Rotation::Counterclockwise90,
        )
        .expect("sandstone_stairs ccw90");

        let wi = Self::WIDTH;
        let de = Self::DEPTH;

        let mut w = PieceWriter {
            ctx,
            reg,
            bounds: &mut self.bounds,
            orient: Some(self.orientation),
            clip,
            loots: BTreeMap::new(),
        };

        // Main base and stepped pyramid
        w.generate_box(0, -4, 0, wi - 1, 0, de - 1, sandstone, sandstone, false);
        for i in 1..=9 {
            w.generate_box(
                i,
                i,
                i,
                wi - 1 - i,
                i,
                de - 1 - i,
                sandstone,
                sandstone,
                false,
            );
            w.generate_box(i + 1, i, i + 1, wi - 2 - i, i, de - 2 - i, air, air, false);
        }
        for lx in 0..wi {
            for lz in 0..de {
                w.fill_column_down(sandstone, lx, -5, lz);
            }
        }

        // Left & right front towers
        w.generate_box(0, 0, 0, 4, 9, 4, sandstone, air, false);
        w.generate_box(1, 10, 1, 3, 10, 3, sandstone, sandstone, false);
        w.place_block(st_n, 2, 10, 0);
        w.place_block(st_s, 2, 10, 4);
        w.place_block(st_e, 0, 10, 2);
        w.place_block(st_w, 4, 10, 2);

        w.generate_box(wi - 5, 0, 0, wi - 1, 9, 4, sandstone, air, false);
        w.generate_box(wi - 4, 10, 1, wi - 2, 10, 3, sandstone, sandstone, false);
        w.place_block(st_n, wi - 3, 10, 0);
        w.place_block(st_s, wi - 3, 10, 4);
        w.place_block(st_e, wi - 5, 10, 2);
        w.place_block(st_w, wi - 1, 10, 2);

        // Center entrance
        w.generate_box(8, 0, 0, 12, 4, 4, sandstone, air, false);
        w.generate_box(9, 1, 0, 11, 3, 4, air, air, false);
        w.place_block(cut_sandstone, 9, 1, 1);
        w.place_block(cut_sandstone, 9, 2, 1);
        w.place_block(cut_sandstone, 9, 3, 1);
        w.place_block(cut_sandstone, 10, 3, 1);
        w.place_block(cut_sandstone, 11, 3, 1);
        w.place_block(cut_sandstone, 11, 2, 1);
        w.place_block(cut_sandstone, 11, 1, 1);

        // Front side hallways & second floor ceiling
        w.generate_box(4, 1, 1, 8, 3, 3, sandstone, air, false);
        w.generate_box(4, 1, 2, 8, 2, 2, air, air, false);
        w.generate_box(12, 1, 1, 16, 3, 3, sandstone, air, false);
        w.generate_box(12, 1, 2, 16, 2, 2, air, air, false);

        w.generate_box(5, 4, 5, wi - 6, 4, de - 6, sandstone, sandstone, false);
        w.generate_box(9, 4, 9, 11, 4, 11, air, air, false);

        w.generate_box(8, 1, 8, 8, 3, 8, cut_sandstone, cut_sandstone, false);
        w.generate_box(12, 1, 8, 12, 3, 8, cut_sandstone, cut_sandstone, false);
        w.generate_box(8, 1, 12, 8, 3, 12, cut_sandstone, cut_sandstone, false);
        w.generate_box(12, 1, 12, 12, 3, 12, cut_sandstone, cut_sandstone, false);

        w.generate_box(1, 1, 5, 4, 4, 11, sandstone, sandstone, false);
        w.generate_box(wi - 5, 1, 5, wi - 2, 4, 11, sandstone, sandstone, false);

        w.generate_box(6, 7, 9, 6, 7, 11, sandstone, sandstone, false);
        w.generate_box(wi - 7, 7, 9, wi - 7, 7, 11, sandstone, sandstone, false);
        w.generate_box(5, 5, 9, 5, 7, 11, cut_sandstone, cut_sandstone, false);
        w.generate_box(wi - 6, 5, 9, wi - 6, 7, 11, cut_sandstone, cut_sandstone, false);
        w.place_block(air, 5, 5, 10);
        w.place_block(air, 5, 6, 10);
        w.place_block(air, 6, 6, 10);
        w.place_block(air, wi - 6, 5, 10);
        w.place_block(air, wi - 6, 6, 10);
        w.place_block(air, wi - 7, 6, 10);

        w.generate_box(2, 4, 4, 2, 6, 4, air, air, false);
        w.generate_box(wi - 3, 4, 4, wi - 3, 6, 4, air, air, false);
        w.place_block(st_n, 2, 4, 5);
        w.place_block(st_n, 2, 3, 4);
        w.place_block(st_n, wi - 3, 4, 5);
        w.place_block(st_n, wi - 3, 3, 4);
        w.generate_box(1, 1, 3, 2, 2, 3, sandstone, sandstone, false);
        w.generate_box(wi - 3, 1, 3, wi - 2, 2, 3, sandstone, sandstone, false);
        w.place_block(sandstone, 1, 1, 2);
        w.place_block(sandstone, wi - 2, 1, 2);
        w.place_block(sandstone_slab, 1, 2, 2);
        w.place_block(sandstone_slab, wi - 2, 2, 2);
        w.place_block(st_w, 2, 1, 2);
        w.place_block(st_e, wi - 3, 1, 2);

        w.generate_box(4, 3, 5, 4, 3, 17, sandstone, sandstone, false);
        w.generate_box(wi - 5, 3, 5, wi - 5, 3, 17, sandstone, sandstone, false);
        w.generate_box(3, 1, 5, 4, 2, 16, air, air, false);
        w.generate_box(wi - 6, 1, 5, wi - 5, 2, 16, air, air, false);
        for z in (5..=17).step_by(2) {
            w.place_block(cut_sandstone, 4, 1, z);
            w.place_block(chiseled_sandstone, 4, 2, z);
            w.place_block(cut_sandstone, wi - 5, 1, z);
            w.place_block(chiseled_sandstone, wi - 5, 2, z);
        }

        // Main chamber terracotta floor center
        w.place_block(orange_terracotta, 10, 0, 7);
        w.place_block(orange_terracotta, 10, 0, 8);
        w.place_block(orange_terracotta, 9, 0, 9);
        w.place_block(orange_terracotta, 11, 0, 9);
        w.place_block(orange_terracotta, 8, 0, 10);
        w.place_block(orange_terracotta, 12, 0, 10);
        w.place_block(orange_terracotta, 7, 0, 10);
        w.place_block(orange_terracotta, 13, 0, 10);
        w.place_block(orange_terracotta, 9, 0, 11);
        w.place_block(orange_terracotta, 11, 0, 11);
        w.place_block(orange_terracotta, 10, 0, 12);
        w.place_block(orange_terracotta, 10, 0, 13);
        w.place_block(blue_terracotta, 10, 0, 10);

        // Tower outer-side glyphs (x = 0 and x = wi - 1)
        for lx in [0, wi - 1] {
            w.place_block(cut_sandstone, lx, 2, 1);
            w.place_block(orange_terracotta, lx, 2, 2);
            w.place_block(cut_sandstone, lx, 2, 3);
            w.place_block(cut_sandstone, lx, 3, 1);
            w.place_block(orange_terracotta, lx, 3, 2);
            w.place_block(cut_sandstone, lx, 3, 3);
            w.place_block(orange_terracotta, lx, 4, 1);
            w.place_block(chiseled_sandstone, lx, 4, 2);
            w.place_block(orange_terracotta, lx, 4, 3);
            w.place_block(cut_sandstone, lx, 5, 1);
            w.place_block(orange_terracotta, lx, 5, 2);
            w.place_block(cut_sandstone, lx, 5, 3);
            w.place_block(orange_terracotta, lx, 6, 1);
            w.place_block(chiseled_sandstone, lx, 6, 2);
            w.place_block(orange_terracotta, lx, 6, 3);
            w.place_block(orange_terracotta, lx, 7, 1);
            w.place_block(orange_terracotta, lx, 7, 2);
            w.place_block(orange_terracotta, lx, 7, 3);
            w.place_block(cut_sandstone, lx, 8, 1);
            w.place_block(cut_sandstone, lx, 8, 2);
            w.place_block(cut_sandstone, lx, 8, 3);
        }

        // Tower front glyphs (x = 2 and x = wi - 3)
        for lx in [2, wi - 3] {
            w.place_block(cut_sandstone, lx - 1, 2, 0);
            w.place_block(orange_terracotta, lx, 2, 0);
            w.place_block(cut_sandstone, lx + 1, 2, 0);
            w.place_block(cut_sandstone, lx - 1, 3, 0);
            w.place_block(orange_terracotta, lx, 3, 0);
            w.place_block(cut_sandstone, lx + 1, 3, 0);
            w.place_block(orange_terracotta, lx - 1, 4, 0);
            w.place_block(chiseled_sandstone, lx, 4, 0);
            w.place_block(orange_terracotta, lx + 1, 4, 0);
            w.place_block(cut_sandstone, lx - 1, 5, 0);
            w.place_block(orange_terracotta, lx, 5, 0);
            w.place_block(cut_sandstone, lx + 1, 5, 0);
            w.place_block(orange_terracotta, lx - 1, 6, 0);
            w.place_block(chiseled_sandstone, lx, 6, 0);
            w.place_block(orange_terracotta, lx + 1, 6, 0);
            w.place_block(orange_terracotta, lx - 1, 7, 0);
            w.place_block(orange_terracotta, lx, 7, 0);
            w.place_block(orange_terracotta, lx + 1, 7, 0);
            w.place_block(cut_sandstone, lx - 1, 8, 0);
            w.place_block(cut_sandstone, lx, 8, 0);
            w.place_block(cut_sandstone, lx + 1, 8, 0);
        }

        // Front entrance upper trim
        w.generate_box(8, 4, 0, 12, 6, 0, cut_sandstone, cut_sandstone, false);
        w.place_block(air, 8, 6, 0);
        w.place_block(air, 12, 6, 0);
        w.place_block(orange_terracotta, 9, 5, 0);
        w.place_block(chiseled_sandstone, 10, 5, 0);
        w.place_block(orange_terracotta, 11, 5, 0);

        // Treasure pit
        w.generate_box(8, -14, 8, 12, -11, 12, cut_sandstone, cut_sandstone, false);
        w.generate_box(
            8,
            -10,
            8,
            12,
            -10,
            12,
            chiseled_sandstone,
            chiseled_sandstone,
            false,
        );
        w.generate_box(8, -9, 8, 12, -9, 12, cut_sandstone, cut_sandstone, false);
        w.generate_box(8, -8, 8, 12, -1, 12, sandstone, sandstone, false);
        w.generate_box(9, -11, 9, 11, -1, 11, air, air, false);
        w.place_block(stone_pressure_plate, 10, -11, 10);
        w.generate_box(9, -13, 9, 11, -13, 11, tnt, air, false);

        w.place_block(air, 8, -11, 10);
        w.place_block(air, 8, -10, 10);
        w.place_block(chiseled_sandstone, 7, -10, 10);
        w.place_block(cut_sandstone, 7, -11, 10);
        w.place_block(air, 12, -11, 10);
        w.place_block(air, 12, -10, 10);
        w.place_block(chiseled_sandstone, 13, -10, 10);
        w.place_block(cut_sandstone, 13, -11, 10);
        w.place_block(air, 10, -11, 8);
        w.place_block(air, 10, -10, 8);
        w.place_block(chiseled_sandstone, 10, -10, 7);
        w.place_block(cut_sandstone, 10, -11, 7);
        w.place_block(air, 10, -11, 12);
        w.place_block(air, 10, -10, 12);
        w.place_block(chiseled_sandstone, 10, -10, 13);
        w.place_block(cut_sandstone, 10, -11, 13);

        for dir in HORIZONTAL_FACINGS {
            let idx = facing_data2d(dir);
            if !self.has_placed_chest[idx] {
                let (sx, sz) = facing_step(dir);
                self.has_placed_chest[idx] = w.create_chest_local(
                    post_rng,
                    10 + sx * 2,
                    -11,
                    10 + sz * 2,
                    "minecraft:chests/desert_pyramid",
                );
            }
        }

        // Hidden archaeology cellar (`addCellar((16, -4, 13))`)
        let (x0, y0, z0) = (16, -4, 13);
        // addCellarStairs
        w.place_block(st_ccw90, 13, -1, 17);
        w.place_block(st_ccw90, 14, -2, 17);
        w.place_block(st_ccw90, 15, -3, 17);
        let flip = level_rng.next_bool();
        w.place_block(sand, x0 - 4, y0 + 4, z0 + 4);
        w.place_block(sand, x0 - 3, y0 + 4, z0 + 4);
        w.place_block(sand, x0 - 2, y0 + 4, z0 + 4);
        w.place_block(sand, x0 - 1, y0 + 4, z0 + 4);
        w.place_block(sand, x0, y0 + 4, z0 + 4);
        w.place_block(sand, x0 - 2, y0 + 3, z0 + 4);
        w.place_block(if flip { sand } else { sandstone }, x0 - 1, y0 + 3, z0 + 4);
        w.place_block(if !flip { sand } else { sandstone }, x0, y0 + 3, z0 + 4);
        w.place_block(sand, x0 - 1, y0 + 2, z0 + 4);
        w.place_block(sandstone, x0, y0 + 2, z0 + 4);
        w.place_block(sand, x0, y0 + 1, z0 + 4);

        // addCellarRoom
        w.generate_box(
            x0 - 3,
            y0 + 1,
            z0 - 3,
            x0 - 3,
            y0 + 1,
            z0 + 2,
            cut_sandstone,
            cut_sandstone,
            true,
        );
        w.generate_box(
            x0 + 3,
            y0 + 1,
            z0 - 3,
            x0 + 3,
            y0 + 1,
            z0 + 2,
            cut_sandstone,
            cut_sandstone,
            true,
        );
        w.generate_box(
            x0 - 3,
            y0 + 1,
            z0 - 3,
            x0 + 3,
            y0 + 1,
            z0 - 2,
            cut_sandstone,
            cut_sandstone,
            true,
        );
        w.generate_box(
            x0 - 3,
            y0 + 1,
            z0 + 3,
            x0 + 3,
            y0 + 1,
            z0 + 3,
            cut_sandstone,
            cut_sandstone,
            true,
        );

        w.generate_box(
            x0 - 3,
            y0 + 2,
            z0 - 3,
            x0 - 3,
            y0 + 2,
            z0 + 2,
            chiseled_sandstone,
            chiseled_sandstone,
            true,
        );
        w.generate_box(
            x0 + 3,
            y0 + 2,
            z0 - 3,
            x0 + 3,
            y0 + 2,
            z0 + 2,
            chiseled_sandstone,
            chiseled_sandstone,
            true,
        );
        w.generate_box(
            x0 - 3,
            y0 + 2,
            z0 - 3,
            x0 + 3,
            y0 + 2,
            z0 - 2,
            chiseled_sandstone,
            chiseled_sandstone,
            true,
        );
        w.generate_box(
            x0 - 3,
            y0 + 2,
            z0 + 3,
            x0 + 3,
            y0 + 2,
            z0 + 3,
            chiseled_sandstone,
            chiseled_sandstone,
            true,
        );

        w.generate_box(
            x0 - 3,
            -1,
            z0 - 3,
            x0 - 3,
            -1,
            z0 + 2,
            cut_sandstone,
            cut_sandstone,
            true,
        );
        w.generate_box(
            x0 + 3,
            -1,
            z0 - 3,
            x0 + 3,
            -1,
            z0 + 2,
            cut_sandstone,
            cut_sandstone,
            true,
        );
        w.generate_box(
            x0 - 3,
            -1,
            z0 - 3,
            x0 + 3,
            -1,
            z0 - 2,
            cut_sandstone,
            cut_sandstone,
            true,
        );
        w.generate_box(
            x0 - 3,
            -1,
            z0 + 3,
            x0 + 3,
            -1,
            z0 + 3,
            cut_sandstone,
            cut_sandstone,
            true,
        );

        for ly in (y0 + 1)..=(y0 + 3) {
            for lx in (x0 - 2)..=(x0 + 2) {
                for lz in (z0 - 2)..=(z0 + 2) {
                    self.potential_suspicious_sand_positions
                        .push(w.world_pos((lx, ly, lz)));
                }
            }
        }

        // placeCollapsedRoof(x0 - 2, y0 + 4, z0 - 2, x0 + 2, z0 + 2)
        let roof_x0 = x0 - 2;
        let roof_y = y0 + 4;
        let roof_z0 = z0 - 2;
        let roof_x1 = x0 + 2;
        let roof_z1 = z0 + 2;
        for lx in roof_x0..=roof_x1 {
            for lz in roof_z0..=roof_z1 {
                let st = if level_rng.next_f32() < 0.33f32 {
                    sandstone
                } else {
                    sand
                };
                w.place_block(st, lx, roof_y, lz);
            }
        }
        let roof_origin = w.world_pos((roof_x0, roof_y, roof_z0));
        let factory_seed = legacy_world_factory_seed(world_seed as u64);
        let mut roof_rng = Lcg::new(
            factory_seed ^ (hash_block_pos(roof_origin.0, roof_origin.1, roof_origin.2) as u64),
        );
        let rx = roof_rng.next_inbetween(roof_x0, roof_x1);
        let rz = roof_rng.next_inbetween(roof_z0, roof_z1);
        self.random_collapsed_roof_pos = w.world_pos((rx, roof_y, rz));

        // Cellar terracotta floor & wall niches
        w.place_block(blue_terracotta, x0, y0, z0);
        w.place_block(orange_terracotta, x0 + 1, y0, z0 - 1);
        w.place_block(orange_terracotta, x0 + 1, y0, z0 + 1);
        w.place_block(orange_terracotta, x0 - 1, y0, z0 - 1);
        w.place_block(orange_terracotta, x0 - 1, y0, z0 + 1);
        w.place_block(orange_terracotta, x0 + 2, y0, z0);
        w.place_block(orange_terracotta, x0 - 2, y0, z0);
        w.place_block(orange_terracotta, x0, y0, z0 + 2);
        w.place_block(orange_terracotta, x0, y0, z0 - 2);

        w.place_block(orange_terracotta, x0 + 3, y0, z0);
        self.potential_suspicious_sand_positions
            .push(w.world_pos((x0 + 3, y0 + 1, z0)));
        self.potential_suspicious_sand_positions
            .push(w.world_pos((x0 + 3, y0 + 2, z0)));
        w.place_block(cut_sandstone, x0 + 4, y0 + 1, z0);
        w.place_block(chiseled_sandstone, x0 + 4, y0 + 2, z0);

        w.place_block(orange_terracotta, x0 - 3, y0, z0);
        self.potential_suspicious_sand_positions
            .push(w.world_pos((x0 - 3, y0 + 1, z0)));
        self.potential_suspicious_sand_positions
            .push(w.world_pos((x0 - 3, y0 + 2, z0)));
        w.place_block(cut_sandstone, x0 - 4, y0 + 1, z0);
        w.place_block(chiseled_sandstone, x0 - 4, y0 + 2, z0);

        w.place_block(orange_terracotta, x0, y0, z0 + 3);
        self.potential_suspicious_sand_positions
            .push(w.world_pos((x0, y0 + 1, z0 + 3)));
        self.potential_suspicious_sand_positions
            .push(w.world_pos((x0, y0 + 2, z0 + 3)));

        w.place_block(orange_terracotta, x0, y0, z0 - 3);
        self.potential_suspicious_sand_positions
            .push(w.world_pos((x0, y0 + 1, z0 - 3)));
        self.potential_suspicious_sand_positions
            .push(w.world_pos((x0, y0 + 2, z0 - 3)));
        w.place_block(cut_sandstone, x0, y0 + 1, z0 - 4);
        w.place_block(chiseled_sandstone, x0, -2, z0 - 4);

        PlacementResult {
            loots: w.loots.into_values().collect(),
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct DesertPyramidStructure;

#[cfg_attr(not(test), allow(dead_code))]
impl DesertPyramidStructure {
    pub(crate) fn after_place(
        pieces: &[DesertPyramidPiece],
        ctx: &mut FC<'_>,
        world_seed: i64,
        clip: Bounds,
        result: &mut PlacementResult,
    ) {
        if pieces.is_empty() {
            return;
        }
        let reg = breg().expect("block registry initialized");
        let suspicious_sand = reg.state_of("minecraft:suspicious_sand", None);
        let sand = reg.state_of("minecraft:sand", None);

        let mut loots: BTreeMap<SinglePiecePos, SinglePieceLoot> = result
            .loots
            .drain(..)
            .map(|l| (l.pos, l))
            .collect();

        let place_suspicious_sand =
            |ctx: &mut FC<'_>, loots: &mut BTreeMap<SinglePiecePos, SinglePieceLoot>, pos: SinglePiecePos| {
                if sp_contains(&clip, pos) {
                    ctx.set(pos.0, pos.1, pos.2, suspicious_sand);
                    loots.insert(
                        pos,
                        SinglePieceLoot {
                            pos,
                            kind: LootKind::Brushable,
                            table: "minecraft:archaeology/desert_pyramid",
                            seed: block_pos_as_long(pos),
                        },
                    );
                }
            };

        // SortedArraySet<BlockPos> ordered by Vec3i::compareTo: (y, z, x)
        let mut unique_positions: Vec<SinglePiecePos> = Vec::new();
        for piece in pieces {
            unique_positions.extend_from_slice(&piece.potential_suspicious_sand_positions);
            place_suspicious_sand(ctx, &mut loots, piece.random_collapsed_roof_pos);
        }
        unique_positions.sort_by_key(|&(x, y, z)| (y, z, x));
        unique_positions.dedup();

        // PiecesContainer.calculateBoundingBox()
        let mut union_bb = pieces[0].bounds;
        for piece in &pieces[1..] {
            for i in 0..3 {
                union_bb[i] = union_bb[i].min(piece.bounds[i]);
                union_bb[i + 3] = union_bb[i + 3].max(piece.bounds[i + 3]);
            }
        }
        let center = bounding_box_center(&union_bb);
        let factory_seed = legacy_world_factory_seed(world_seed as u64);
        let mut rng = Lcg::new(
            factory_seed ^ (hash_block_pos(center.0, center.1, center.2) as u64),
        );

        // Util.shuffle(ObjectArrayList, RandomSource)
        for i in (2..=unique_positions.len()).rev() {
            let j = rng.next_bound(i as i32) as usize;
            unique_positions.swap(i - 1, j);
        }

        let mut count = unique_positions.len().min((rng.next_bound(8 - 5) + 5) as usize);
        for pos in unique_positions {
            if count > 0 {
                count -= 1;
                place_suspicious_sand(ctx, &mut loots, pos);
            } else if sp_contains(&clip, pos) {
                ctx.set(pos.0, pos.1, pos.2, sand);
                loots.remove(&pos);
            }
        }

        result.loots = loots.into_values().collect();
    }
}

// ============================================================================
// 4. minecraft:jungle_pyramid (JungleTemplePiece)
// ============================================================================

#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct JungleTemplePiece {
    pub(crate) bounds: Bounds,
    pub(crate) orientation: Facing,
    pub(crate) height_state: ScatteredHeightState,
    pub(crate) placed_main_chest: bool,
    pub(crate) placed_hidden_chest: bool,
    pub(crate) placed_trap1: bool,
    pub(crate) placed_trap2: bool,
}

#[cfg_attr(not(test), allow(dead_code))]
impl JungleTemplePiece {
    const WIDTH: i32 = 12;
    const DEPTH: i32 = 15;

    pub(crate) fn new<R: StructureRng + ?Sized>(rng: &mut R, x: i32, z: i32) -> Self {
        let orientation = random_horizontal_facing(rng);
        Self::with_orientation(x, z, orientation)
    }

    pub(crate) fn with_orientation(x: i32, z: i32, orientation: Facing) -> Self {
        Self {
            bounds: make_bounding_box(x, 64, z, orientation, Self::WIDTH, 10, Self::DEPTH),
            orientation,
            height_state: ScatteredHeightState::default(),
            placed_main_chest: false,
            placed_hidden_chest: false,
            placed_trap1: false,
            placed_trap2: false,
        }
    }

    pub(crate) fn post_process<R: StructureRng + ?Sized>(
        &mut self,
        ctx: &mut FC<'_>,
        post_rng: &mut R,
        clip: Bounds,
    ) -> PlacementResult {
        self.post_process_with_heightmap(ctx, post_rng, clip, &mut |ctx, kind, x, z| {
            ctx.heightmap(kind, x, z)
        })
    }

    pub(crate) fn post_process_with_heightmap<R: StructureRng + ?Sized, H>(
        &mut self,
        ctx: &mut FC<'_>,
        post_rng: &mut R,
        clip: Bounds,
        hm: &mut H,
    ) -> PlacementResult
    where
        H: FnMut(&mut FC<'_>, HeightmapKind, i32, i32) -> i32,
    {
        if !update_average_ground_height(
            &mut self.bounds,
            &mut self.height_state,
            ctx,
            &clip,
            0,
            hm,
        ) {
            return PlacementResult::default();
        }
        let reg = breg().expect("block registry initialized");
        let cobblestone = reg.state_of("minecraft:cobblestone", None);
        let mossy_cobblestone = reg.state_of("minecraft:mossy_cobblestone", None);
        let chiseled_stone_bricks = reg.state_of("minecraft:chiseled_stone_bricks", None);
        let air = reg.state_of("minecraft:air", None);

        let st_e = sp_state(reg, "minecraft:cobblestone_stairs", &[("facing", "east")]);
        let st_w = sp_state(reg, "minecraft:cobblestone_stairs", &[("facing", "west")]);
        let st_s = sp_state(reg, "minecraft:cobblestone_stairs", &[("facing", "south")]);
        let st_n = sp_state(reg, "minecraft:cobblestone_stairs", &[("facing", "north")]);

        let hook_e = sp_state(
            reg,
            "minecraft:tripwire_hook",
            &[("facing", "east"), ("attached", "true")],
        );
        let hook_w = sp_state(
            reg,
            "minecraft:tripwire_hook",
            &[("facing", "west"), ("attached", "true")],
        );
        let hook_n = sp_state(
            reg,
            "minecraft:tripwire_hook",
            &[("facing", "north"), ("attached", "true")],
        );
        let hook_s = sp_state(
            reg,
            "minecraft:tripwire_hook",
            &[("facing", "south"), ("attached", "true")],
        );

        let wire_ew = sp_state(
            reg,
            "minecraft:tripwire",
            &[("east", "true"), ("west", "true"), ("attached", "true")],
        );
        let wire_ns = sp_state(
            reg,
            "minecraft:tripwire",
            &[("north", "true"), ("south", "true"), ("attached", "true")],
        );

        let rs_ns = sp_state(
            reg,
            "minecraft:redstone_wire",
            &[("north", "side"), ("south", "side")],
        );
        let rs_nw = sp_state(
            reg,
            "minecraft:redstone_wire",
            &[("north", "side"), ("west", "side")],
        );
        let rs_ew = sp_state(
            reg,
            "minecraft:redstone_wire",
            &[("east", "side"), ("west", "side")],
        );
        let rs_ws = sp_state(
            reg,
            "minecraft:redstone_wire",
            &[("west", "side"), ("south", "side")],
        );
        let rs_n_sup = sp_state(
            reg,
            "minecraft:redstone_wire",
            &[("north", "side"), ("south", "up")],
        );
        let rs_all = sp_state(
            reg,
            "minecraft:redstone_wire",
            &[
                ("north", "side"),
                ("south", "side"),
                ("east", "side"),
                ("west", "side"),
            ],
        );

        let vine_s = sp_state(reg, "minecraft:vine", &[("south", "true")]);
        let vine_e = sp_state(reg, "minecraft:vine", &[("east", "true")]);
        let lever_nw = sp_state(
            reg,
            "minecraft:lever",
            &[("facing", "north"), ("face", "wall")],
        );
        let piston_up = sp_state(reg, "minecraft:sticky_piston", &[("facing", "up")]);
        let piston_w = sp_state(reg, "minecraft:sticky_piston", &[("facing", "west")]);
        let repeater_n = sp_state(reg, "minecraft:repeater", &[("facing", "north")]);

        let mut moss = |r: &mut R, _x: i32, _y: i32, _z: i32, _edge: bool| -> u32 {
            if r.next_f32() < 0.4f32 {
                cobblestone
            } else {
                mossy_cobblestone
            }
        };

        let mut w = PieceWriter {
            ctx,
            reg,
            bounds: &mut self.bounds,
            orient: Some(self.orientation),
            clip,
            loots: BTreeMap::new(),
        };

        // Main stone shell
        w.generate_box_selector(
            0,
            -4,
            0,
            Self::WIDTH - 1,
            0,
            Self::DEPTH - 1,
            false,
            post_rng,
            &mut moss,
        );
        w.generate_box_selector(2, 1, 2, 9, 2, 2, false, post_rng, &mut moss);
        w.generate_box_selector(2, 1, 12, 9, 2, 12, false, post_rng, &mut moss);
        w.generate_box_selector(2, 1, 3, 2, 2, 11, false, post_rng, &mut moss);
        w.generate_box_selector(9, 1, 3, 9, 2, 11, false, post_rng, &mut moss);
        w.generate_box_selector(1, 3, 1, 10, 6, 1, false, post_rng, &mut moss);
        w.generate_box_selector(1, 3, 13, 10, 6, 13, false, post_rng, &mut moss);
        w.generate_box_selector(1, 3, 2, 1, 6, 12, false, post_rng, &mut moss);
        w.generate_box_selector(10, 3, 2, 10, 6, 12, false, post_rng, &mut moss);
        w.generate_box_selector(2, 3, 2, 9, 3, 12, false, post_rng, &mut moss);
        w.generate_box_selector(2, 6, 2, 9, 6, 12, false, post_rng, &mut moss);
        w.generate_box_selector(3, 7, 3, 8, 7, 11, false, post_rng, &mut moss);
        w.generate_box_selector(4, 8, 4, 7, 8, 10, false, post_rng, &mut moss);

        // Interior air clearings
        w.generate_air_box(3, 1, 3, 8, 2, 11);
        w.generate_air_box(4, 3, 6, 7, 3, 9);
        w.generate_air_box(2, 4, 2, 9, 5, 12);
        w.generate_air_box(4, 6, 5, 7, 6, 9);
        w.generate_air_box(5, 7, 6, 6, 7, 8);
        w.generate_air_box(5, 1, 2, 6, 2, 2);
        w.generate_air_box(5, 2, 12, 6, 2, 12);
        w.generate_air_box(5, 5, 1, 6, 5, 1);
        w.generate_air_box(5, 5, 13, 6, 5, 13);

        w.place_block(air, 1, 5, 5);
        w.place_block(air, 10, 5, 5);
        w.place_block(air, 1, 5, 9);
        w.place_block(air, 10, 5, 9);

        // Exterior pilasters & roof blocks
        for z in (0..=14).step_by(14) {
            w.generate_box_selector(2, 4, z, 2, 5, z, false, post_rng, &mut moss);
            w.generate_box_selector(4, 4, z, 4, 5, z, false, post_rng, &mut moss);
            w.generate_box_selector(7, 4, z, 7, 5, z, false, post_rng, &mut moss);
            w.generate_box_selector(9, 4, z, 9, 5, z, false, post_rng, &mut moss);
        }
        w.generate_box_selector(5, 6, 0, 6, 6, 0, false, post_rng, &mut moss);
        for x in (0..=11).step_by(11) {
            for z in (2..=12).step_by(2) {
                w.generate_box_selector(x, 4, z, x, 5, z, false, post_rng, &mut moss);
            }
            w.generate_box_selector(x, 6, 5, x, 6, 5, false, post_rng, &mut moss);
            w.generate_box_selector(x, 6, 9, x, 6, 9, false, post_rng, &mut moss);
        }

        w.generate_box_selector(2, 7, 2, 2, 9, 2, false, post_rng, &mut moss);
        w.generate_box_selector(9, 7, 2, 9, 9, 2, false, post_rng, &mut moss);
        w.generate_box_selector(2, 7, 12, 2, 9, 12, false, post_rng, &mut moss);
        w.generate_box_selector(9, 7, 12, 9, 9, 12, false, post_rng, &mut moss);
        w.generate_box_selector(4, 9, 4, 4, 9, 4, false, post_rng, &mut moss);
        w.generate_box_selector(7, 9, 4, 7, 9, 4, false, post_rng, &mut moss);
        w.generate_box_selector(4, 9, 10, 4, 9, 10, false, post_rng, &mut moss);
        w.generate_box_selector(7, 9, 10, 7, 9, 10, false, post_rng, &mut moss);
        w.generate_box_selector(5, 9, 7, 6, 9, 7, false, post_rng, &mut moss);

        // Stairs
        w.place_block(st_n, 5, 9, 6);
        w.place_block(st_n, 6, 9, 6);
        w.place_block(st_s, 5, 9, 8);
        w.place_block(st_s, 6, 9, 8);
        w.place_block(st_n, 4, 0, 0);
        w.place_block(st_n, 5, 0, 0);
        w.place_block(st_n, 6, 0, 0);
        w.place_block(st_n, 7, 0, 0);
        w.place_block(st_n, 4, 1, 8);
        w.place_block(st_n, 4, 2, 9);
        w.place_block(st_n, 4, 3, 10);
        w.place_block(st_n, 7, 1, 8);
        w.place_block(st_n, 7, 2, 9);
        w.place_block(st_n, 7, 3, 10);

        w.generate_box_selector(4, 1, 9, 4, 1, 9, false, post_rng, &mut moss);
        w.generate_box_selector(7, 1, 9, 7, 1, 9, false, post_rng, &mut moss);
        w.generate_box_selector(4, 1, 10, 7, 2, 10, false, post_rng, &mut moss);
        w.generate_box_selector(5, 4, 5, 6, 4, 5, false, post_rng, &mut moss);

        w.place_block(st_e, 4, 4, 5);
        w.place_block(st_w, 7, 4, 5);

        for i in 0..4 {
            w.place_block(st_s, 5, 0 - i, 6 + i);
            w.place_block(st_s, 6, 0 - i, 6 + i);
            w.generate_air_box(5, 0 - i, 7 + i, 6, 0 - i, 9 + i);
        }

        // Basement hallways
        w.generate_air_box(1, -3, 12, 10, -1, 13);
        w.generate_air_box(1, -3, 1, 3, -1, 13);
        w.generate_air_box(1, -3, 1, 9, -1, 5);
        for z in (1..=13).step_by(2) {
            w.generate_box_selector(1, -3, z, 1, -2, z, false, post_rng, &mut moss);
        }
        for z in (2..=12).step_by(2) {
            w.generate_box_selector(1, -1, z, 3, -1, z, false, post_rng, &mut moss);
        }
        w.generate_box_selector(2, -2, 1, 5, -2, 1, false, post_rng, &mut moss);
        w.generate_box_selector(7, -2, 1, 9, -2, 1, false, post_rng, &mut moss);
        w.generate_box_selector(6, -3, 1, 6, -3, 1, false, post_rng, &mut moss);
        w.generate_box_selector(6, -1, 1, 6, -1, 1, false, post_rng, &mut moss);

        // Trap 1
        w.place_block(hook_e, 1, -3, 8);
        w.place_block(hook_w, 4, -3, 8);
        w.place_block(wire_ew, 2, -3, 8);
        w.place_block(wire_ew, 3, -3, 8);
        w.place_block(rs_ns, 5, -3, 7);
        w.place_block(rs_ns, 5, -3, 6);
        w.place_block(rs_ns, 5, -3, 5);
        w.place_block(rs_ns, 5, -3, 4);
        w.place_block(rs_ns, 5, -3, 3);
        w.place_block(rs_ns, 5, -3, 2);
        w.place_block(rs_nw, 5, -3, 1);
        w.place_block(rs_ew, 4, -3, 1);
        w.place_block(mossy_cobblestone, 3, -3, 1);
        if !self.placed_trap1 {
            self.placed_trap1 = w.create_dispenser(
                post_rng,
                3,
                -2,
                1,
                Facing::North,
                "minecraft:chests/jungle_temple_dispenser",
            );
        }
        w.place_block(vine_s, 3, -2, 2);

        // Trap 2 & main chest
        w.place_block(hook_n, 7, -3, 1);
        w.place_block(hook_s, 7, -3, 5);
        w.place_block(wire_ns, 7, -3, 2);
        w.place_block(wire_ns, 7, -3, 3);
        w.place_block(wire_ns, 7, -3, 4);
        w.place_block(rs_ew, 8, -3, 6);
        w.place_block(rs_ws, 9, -3, 6);
        w.place_block(rs_n_sup, 9, -3, 5);
        w.place_block(mossy_cobblestone, 9, -3, 4);
        w.place_block(rs_ns, 9, -2, 4);
        if !self.placed_trap2 {
            self.placed_trap2 = w.create_dispenser(
                post_rng,
                9,
                -2,
                3,
                Facing::West,
                "minecraft:chests/jungle_temple_dispenser",
            );
        }
        w.place_block(vine_e, 8, -1, 3);
        w.place_block(vine_e, 8, -2, 3);
        if !self.placed_main_chest {
            self.placed_main_chest =
                w.create_chest_local(post_rng, 8, -3, 3, "minecraft:chests/jungle_temple");
        }
        w.place_block(mossy_cobblestone, 9, -3, 2);
        w.place_block(mossy_cobblestone, 8, -3, 1);
        w.place_block(mossy_cobblestone, 4, -3, 5);
        w.place_block(mossy_cobblestone, 5, -2, 5);
        w.place_block(mossy_cobblestone, 5, -1, 5);
        w.place_block(mossy_cobblestone, 6, -3, 5);
        w.place_block(mossy_cobblestone, 7, -2, 5);
        w.place_block(mossy_cobblestone, 7, -1, 5);
        w.place_block(mossy_cobblestone, 8, -3, 5);

        w.generate_box_selector(9, -1, 1, 9, -1, 5, false, post_rng, &mut moss);

        // Hidden lever puzzle room
        w.generate_air_box(8, -3, 8, 10, -1, 10);
        w.place_block(chiseled_stone_bricks, 8, -2, 11);
        w.place_block(chiseled_stone_bricks, 9, -2, 11);
        w.place_block(chiseled_stone_bricks, 10, -2, 11);
        w.place_block(lever_nw, 8, -2, 12);
        w.place_block(lever_nw, 9, -2, 12);
        w.place_block(lever_nw, 10, -2, 12);
        w.generate_box_selector(8, -3, 8, 8, -3, 10, false, post_rng, &mut moss);
        w.generate_box_selector(10, -3, 8, 10, -3, 10, false, post_rng, &mut moss);
        w.place_block(mossy_cobblestone, 10, -2, 9);
        w.place_block(rs_ns, 8, -2, 9);
        w.place_block(rs_ns, 8, -2, 10);
        w.place_block(rs_all, 10, -1, 9);
        w.place_block(piston_up, 9, -2, 8);
        w.place_block(piston_w, 10, -2, 8);
        w.place_block(piston_w, 10, -1, 8);
        w.place_block(repeater_n, 10, -2, 10);
        if !self.placed_hidden_chest {
            self.placed_hidden_chest =
                w.create_chest_local(post_rng, 9, -3, 10, "minecraft:chests/jungle_temple");
        }

        PlacementResult {
            loots: w.loots.into_values().collect(),
        }
    }
}

#[cfg(test)]
mod single_piece_tests {
    use super::*;
    use crate::generation_math::sha256;

    #[derive(Clone, Debug)]
    struct SceneSpec {
        oc_floor: i32,
        ground_height: i32,
        floor_block: String,
        fill_block: String,
        neighbor_block: String,
        water: bool,
        sloped: bool,
    }

    impl SceneSpec {
        fn parse(spec: &str) -> Self {
            let mut s = Self {
                oc_floor: 58,
                ground_height: 64,
                floor_block: "minecraft:sandstone".to_string(),
                fill_block: "minecraft:sand".to_string(),
                neighbor_block: "minecraft:air".to_string(),
                water: false,
                sloped: false,
            };
            for part in spec.split(',') {
                if let Some((k, v)) = part.split_once('=') {
                    match k {
                        "oc" => s.oc_floor = v.parse().unwrap(),
                        "hm" => s.ground_height = v.parse().unwrap(),
                        "sub" => s.floor_block = v.to_string(),
                        "fill" => s.fill_block = v.to_string(),
                        "adj" => s.neighbor_block = v.to_string(),
                        "water" => s.water = v.parse().unwrap(),
                        "slope" => s.sloped = v.parse().unwrap(),
                        _ => {}
                    }
                }
            }
            s
        }

        fn height_at(&self, x: i32, z: i32) -> i32 {
            if !self.sloped {
                return self.ground_height;
            }
            let dx = x.rem_euclid(16);
            let dz = z.rem_euclid(16);
            self.ground_height + ((dx + dz) % 5) - 2
        }

        fn state_at(&self, reg: &BReg, x: i32, y: i32, z: i32, kind: &str) -> u32 {
            if kind == "buried_treasure" {
                if y < self.oc_floor - 3 {
                    return reg.state_of(&self.floor_block, None);
                }
                if y < self.oc_floor {
                    return reg.state_of(&self.fill_block, None);
                }
                if y == self.oc_floor {
                    if self.neighbor_block != "minecraft:air"
                        && ((x & 15) - 9).abs() + ((z & 15) - 9).abs() == 1
                    {
                        return reg.state_of(&self.neighbor_block, None);
                    }
                    return reg.state_of(&self.fill_block, None);
                }
                if y <= 62 && (self.fill_block == "minecraft:water" || self.water) {
                    return reg.state_of("minecraft:water", None);
                }
                return 0;
            }
            let h = self.height_at(x, z);
            if y < h - 3 {
                return reg.state_of("minecraft:stone", None);
            }
            if y < h {
                if self.water && y >= h - 2 {
                    return reg.state_of("minecraft:water", None);
                }
                return reg.state_of(&self.floor_block, None);
            }
            if self.water && y <= 62 {
                return reg.state_of("minecraft:water", None);
            }
            0
        }
    }

    fn parse_box(s: &str) -> Bounds {
        let nums: Vec<i32> = s.split(',').map(|p| p.parse().unwrap()).collect();
        [nums[0], nums[1], nums[2], nums[3], nums[4], nums[5]]
    }

    fn parse_facing(s: &str) -> Facing {
        match s {
            "north" => Facing::North,
            "south" => Facing::South,
            "west" => Facing::West,
            "east" => Facing::East,
            other => panic!("unexpected facing {other}"),
        }
    }

    fn format_loots(loots: &[SinglePieceLoot]) -> String {
        if loots.is_empty() {
            return "-".to_string();
        }
        let mut sorted = loots.to_vec();
        sorted.sort_by_key(|l| (l.pos.0, l.pos.1, l.pos.2, l.kind));
        sorted
            .into_iter()
            .map(|l| {
                let kind = match l.kind {
                    LootKind::Chest => "chest",
                    LootKind::Dispenser => "dispenser",
                    LootKind::Brushable => "brushable",
                };
                format!("{},{},{}|{kind}|{}|{}", l.pos.0, l.pos.1, l.pos.2, l.table, l.seed)
            })
            .collect::<Vec<_>>()
            .join(";")
    }

    fn sha256_of_writes(reg: &BReg, wr: &std::collections::HashMap<SinglePiecePos, u32>) -> (usize, String) {
        let mut keys: Vec<SinglePiecePos> = wr.keys().copied().collect();
        keys.sort_by_key(|&(x, y, z)| (x, y, z));
        let mut buf = Vec::new();
        for &(x, y, z) in &keys {
            let st = wr[&(x, y, z)];
            buf.extend_from_slice(
                format!("{x},{y},{z} {}\n", reg.canonical_state(st)).as_bytes(),
            );
        }
        let digest = sha256(&buf);
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        (keys.len(), hex)
    }

    #[test]
    fn single_piece_structures_match_all_1211_native_oracle_vectors() {
        let reg = breg().expect("block registry initialized");
        let data = include_str!("../tests/data/single_piece_structures_1211.tsv");
        let mut count = 0usize;

        let mut failures = 0usize;
        for line in data.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let f: Vec<&str> = line.split('\t').collect();
            assert_eq!(f.len(), 15, "bad oracle line: {line}");
            let kind = f[0];
            let case_id = f[1];
            let seed: i64 = f[2].parse().unwrap();
            let cx: i32 = f[3].parse().unwrap();
            let cz: i32 = f[4].parse().unwrap();
            let orient_str = f[5];
            let init_bb = parse_box(f[6]);
            let clip = parse_box(f[7]);
            let scene_str = f[8];
            let want_bb = parse_box(f[9]);
            let want_writes: usize = f[10].parse().unwrap();
            let want_sha = f[11];
            let want_rng_after: i64 = f[12].parse().unwrap();
            let want_level_rng_after: i64 = f[13].parse().unwrap();
            let want_loots = f[14];

            let scene = SceneSpec::parse(scene_str);
            let mut gen = crate::worldgen::Gen::load(seed as u64, crate::name_to_state).unwrap();
            let mut ctx = FC::new(&mut gen);
            ctx.test_no_terrain = true;
            ctx.test_default_air = true;

            // Populate terrain cells inside a generous margin around the piece & clip
            let x_lo = (clip[0].min(init_bb[0]) - 2).min(want_bb[0] - 2);
            let x_hi = (clip[3].max(init_bb[3]) + 2).max(want_bb[3] + 2);
            let z_lo = (clip[2].min(init_bb[2]) - 2).min(want_bb[2] - 2);
            let z_hi = (clip[5].max(init_bb[5]) + 2).max(want_bb[5] + 2);
            for x in x_lo..=x_hi {
                for z in z_lo..=z_hi {
                    for y in -64..=85 {
                        let st = scene.state_at(reg, x, y, z, kind);
                        if st != 0 {
                            ctx.test_base_cell((x, y, z), st);
                        }
                    }
                }
            }

            let mut post_rng = Xor::seed_worldgen((seed as u64) ^ 0x9E37_79B9_7F4A_7C15);
            let mut level_rng = Lcg::new((seed as u64) ^ 0x517C_C1B7_2722_0A95);

            let min_x = cx << 4;
            let min_z = cz << 4;

            let (final_bb, result) = match kind {
                "buried_treasure" => {
                    let mut p = BuriedTreasurePiece::new(cx, cz);
                    assert_eq!(p.bounds, init_bb, "{kind}/{case_id} init_bb");
                    let res = p.post_process_with_heightmap(
                        &mut ctx,
                        &mut post_rng,
                        clip,
                        &mut |_, _, _, _| scene.oc_floor,
                    );
                    (p.bounds, res)
                }
                "swamp_hut" => {
                    let orient = parse_facing(orient_str);
                    let mut p = SwampHutPiece::with_orientation(min_x, min_z, orient);
                    assert_eq!(p.bounds, init_bb, "{kind}/{case_id} init_bb");
                    let res = p.post_process_with_heightmap(
                        &mut ctx,
                        clip,
                        &mut |_, _, x, z| scene.height_at(x, z),
                    );
                    (p.bounds, res)
                }
                "desert_pyramid" => {
                    let orient = parse_facing(orient_str);
                    let mut p = DesertPyramidPiece::with_orientation(min_x, min_z, orient);
                    assert_eq!(p.bounds, init_bb, "{kind}/{case_id} init_bb");
                    let mut res = p.post_process_with_heightmap(
                        &mut ctx,
                        &mut post_rng,
                        &mut level_rng,
                        seed,
                        clip,
                        &mut |_, _, x, z| scene.height_at(x, z),
                    );
                    if case_id != "postprocess_only_slope" {
                        DesertPyramidStructure::after_place(
                            &[p.clone()],
                            &mut ctx,
                            seed,
                            clip,
                            &mut res,
                        );
                    }
                    (p.bounds, res)
                }
                "jungle_pyramid" => {
                    let orient = parse_facing(orient_str);
                    let mut p = JungleTemplePiece::with_orientation(min_x, min_z, orient);
                    assert_eq!(p.bounds, init_bb, "{kind}/{case_id} init_bb");
                    let res = p.post_process_with_heightmap(
                        &mut ctx,
                        &mut post_rng,
                        clip,
                        &mut |_, _, x, z| scene.height_at(x, z),
                    );
                    (p.bounds, res)
                }
                other => panic!("unknown kind {other}"),
            };

            let (got_writes, got_sha) = sha256_of_writes(reg, &ctx.wr);
            let got_loots = format_loots(&result.loots);
            let got_rng_after = post_rng.next_i64();
            let got_level_rng_after = level_rng.next_i64();

            let mut ok = true;
            if final_bb != want_bb {
                eprintln!("FAIL {kind}/{case_id} final_bb: got={final_bb:?} want={want_bb:?}");
                ok = false;
            }
            if got_writes != want_writes {
                eprintln!("FAIL {kind}/{case_id} write_count: got={got_writes} want={want_writes}");
                ok = false;
            }
            if got_sha != want_sha {
                eprintln!("FAIL {kind}/{case_id} sha256: got={got_sha} want={want_sha}");
                ok = false;
            }
            if got_rng_after != want_rng_after {
                eprintln!("FAIL {kind}/{case_id} rng_after: got={got_rng_after} want={want_rng_after}");
                ok = false;
            }
            if kind == "desert_pyramid" && got_level_rng_after != want_level_rng_after {
                eprintln!("FAIL {kind}/{case_id} level_rng_after: got={got_level_rng_after} want={want_level_rng_after}");
                ok = false;
            }
            if got_loots != want_loots {
                eprintln!("FAIL {kind}/{case_id} loots: got={got_loots} want={want_loots}");
                ok = false;
            }
            if !ok {
                failures += 1;
            }
            count += 1;
        }
        assert_eq!(count, 23);
        assert_eq!(failures, 0, "{failures} oracle case(s) failed");
    }

    #[test]
    fn random_orientation_constructors_draw_plane_horizontal() {
        let mut seen = [false; 4];
        for seed in 0u64..64 {
            let expected = HORIZONTAL_FACINGS[Xor::seed_worldgen(seed).next_bound(4) as usize];
            let s = SwampHutPiece::new(&mut Xor::seed_worldgen(seed), 32, -16);
            assert_eq!(s.orientation, expected);
            let d = DesertPyramidPiece::new(&mut Xor::seed_worldgen(seed), 0, 0);
            assert_eq!(d.orientation, expected);
            let j = JungleTemplePiece::new(&mut Xor::seed_worldgen(seed), 0, 0);
            assert_eq!(j.orientation, expected);
            seen[facing_data2d(expected)] = true;
        }
        assert!(seen.iter().all(|&v| v));
    }

    #[test]
    fn post_process_wrappers_use_live_context_heightmaps() {
        let reg = breg().expect("block registry initialized");
        let stone = reg.state_of("minecraft:stone", None);
        let sand = reg.state_of("minecraft:sand", None);
        let dirt = reg.state_of("minecraft:dirt", None);

        // BuriedTreasurePiece::post_process reads OceanFloorWg
        {
            let mut gen = crate::worldgen::Gen::load(101, crate::name_to_state).unwrap();
            let mut ctx = FC::new(&mut gen);
            ctx.test_no_terrain = true;
            ctx.test_default_air = true;
            for x in 7..=11 {
                for z in 7..=11 {
                    for y in 50..=54 {
                        ctx.test_base_cell((x, y, z), stone);
                    }
                    for y in 55..=57 {
                        ctx.test_base_cell((x, y, z), sand);
                    }
                }
            }
            ctx.test_frozen_height(HeightmapKind::OceanFloorWg, 9, 9, 58);
            let mut p = BuriedTreasurePiece::new(0, 0);
            let mut rng = Xor::seed_worldgen(101);
            let res = p.post_process(&mut ctx, &mut rng, [0, -64, 0, 15, 319, 15]);
            assert_eq!(res.loots.len(), 1);
            assert_eq!(res.loots[0].pos, (9, 55, 9));
        }

        // SwampHutPiece, DesertPyramidPiece, JungleTemplePiece post_process read MotionBlockingNoLeaves
        {
            let mut gen = crate::worldgen::Gen::load(202, crate::name_to_state).unwrap();
            let mut ctx = FC::new(&mut gen);
            ctx.test_no_terrain = true;
            ctx.test_default_air = true;
            for x in 0..=20 {
                for z in 0..=20 {
                    for y in 60..=63 {
                        ctx.test_base_cell((x, y, z), dirt);
                    }
                }
            }
            let mut s = SwampHutPiece::with_orientation(0, 0, Facing::North);
            s.post_process(&mut ctx, [0, -64, 0, 15, 319, 15]);
            assert_eq!(s.bounds[1], 64);
        }

        {
            let mut gen = crate::worldgen::Gen::load(303, crate::name_to_state).unwrap();
            let mut ctx = FC::new(&mut gen);
            ctx.test_no_terrain = true;
            ctx.test_default_air = true;
            for x in 0..=20 {
                for z in 0..=20 {
                    for y in 60..=64 {
                        ctx.test_base_cell((x, y, z), sand);
                    }
                }
            }
            let mut d = DesertPyramidPiece::with_orientation(0, 0, Facing::North);
            let mut post_rng = Xor::seed_worldgen(303);
            let mut level_rng = Lcg::new(303);
            let res = d.post_process(
                &mut ctx,
                &mut post_rng,
                &mut level_rng,
                303,
                [0, -64, 0, 31, 319, 31],
            );
            assert_eq!(res.loots.len(), 4);
            assert!((63..=65).contains(&d.bounds[1]));
        }

        {
            let mut gen = crate::worldgen::Gen::load(404, crate::name_to_state).unwrap();
            let mut ctx = FC::new(&mut gen);
            ctx.test_no_terrain = true;
            ctx.test_default_air = true;
            for x in 0..=14 {
                for z in 0..=14 {
                    for y in 60..=63 {
                        ctx.test_base_cell((x, y, z), dirt);
                    }
                }
            }
            let mut j = JungleTemplePiece::with_orientation(0, 0, Facing::North);
            let mut rng = Xor::seed_worldgen(404);
            let res = j.post_process(&mut ctx, &mut rng, [0, -64, 0, 31, 319, 31]);
            assert_eq!(res.loots.len(), 4);
            assert_eq!(j.bounds[1], 64);
        }
    }

    /// The container loot a single-piece piece writes is staged into the generation payload
    /// store, and the archaeology it also writes is not: this runtime has no brushing store, so
    /// the `LootKind::Brushable` record has nowhere to go. Putting that table in the item store
    /// would make a broken suspicious-sand block spill loot native never yields, which is why
    /// the record stays unstaged (the same named gap `feature.rs` carries for `desert_well`).
    #[test]
    fn single_piece_container_loot_is_staged_and_archaeology_is_not() {
        let reg = breg().expect("block registry initialized");
        let dirt = reg.state_of("minecraft:dirt", None);
        let sand = reg.state_of("minecraft:sand", None);

        // Jungle temple: two chests and two dispensers, each in the store that reads the block.
        {
            let mut gen = crate::worldgen::Gen::load(404, crate::name_to_state).unwrap();
            let mut ctx = FC::new(&mut gen);
            ctx.test_no_terrain = true;
            ctx.test_default_air = true;
            for x in 0..=14 {
                for z in 0..=14 {
                    for y in 60..=63 {
                        ctx.test_base_cell((x, y, z), dirt);
                    }
                }
            }
            let clip = [0, -64, 0, 31, 319, 31];
            let mut j = JungleTemplePiece::with_orientation(0, 0, Facing::North);
            let mut rng = Xor::seed_worldgen(404);
            let res = j.post_process(&mut ctx, &mut rng, clip);
            assert_eq!(res.loots.len(), 4, "two chests and two dispensers");
            let staged: Vec<(String, crate::generation_payload::ContainerKind)> = ctx
                .payloads
                .iter()
                .filter_map(|(_, p)| match p {
                    GenerationPayload::LootContainer { kind, loot_table, .. } => {
                        Some((loot_table.clone(), *kind))
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(
                staged
                    .iter()
                    .filter(|(t, k)| t == "minecraft:chests/jungle_temple"
                        && *k == ContainerKind::Chest)
                    .count(),
                2,
                "the temple's two chests are staged as chest loot"
            );
            assert_eq!(
                staged
                    .iter()
                    .filter(|(t, k)| t == "minecraft:chests/jungle_temple_dispenser"
                        && *k == ContainerKind::BlockContainer(9))
                    .count(),
                2,
                "the temple's two dispensers are staged as 9-slot block containers"
            );
        }

        // Desert pyramid: four chests staged, and no payload for any suspicious-sand cell.
        {
            let mut gen = crate::worldgen::Gen::load(303, crate::name_to_state).unwrap();
            let mut ctx = FC::new(&mut gen);
            ctx.test_no_terrain = true;
            ctx.test_default_air = true;
            for x in 0..=20 {
                for z in 0..=20 {
                    for y in 60..=64 {
                        ctx.test_base_cell((x, y, z), sand);
                    }
                }
            }
            let clip = [0, -64, 0, 31, 319, 31];
            let mut d = DesertPyramidPiece::with_orientation(0, 0, Facing::North);
            let mut post_rng = Xor::seed_worldgen(303);
            let mut level_rng = Lcg::new(303);
            let mut res = d.post_process(&mut ctx, &mut post_rng, &mut level_rng, 303, clip);
            DesertPyramidStructure::after_place(std::slice::from_ref(&d), &mut ctx, 303, clip, &mut res);

            let brushable: Vec<SinglePiecePos> = res
                .loots
                .iter()
                .filter(|l| l.kind == LootKind::Brushable)
                .map(|l| l.pos)
                .collect();
            assert!(
                !brushable.is_empty(),
                "the pyramid places suspicious sand with an archaeology table"
            );
            let chests = res.loots.iter().filter(|l| l.kind == LootKind::Chest).count();
            assert_eq!(chests, 4, "four pyramid chests");
            assert_eq!(ctx.payloads.len(), chests, "every payload is a chest");
            for (pos, payload) in &ctx.payloads {
                assert!(
                    !brushable.contains(pos),
                    "{pos:?} is suspicious sand: archaeology has no store, so it must not be staged"
                );
                assert!(matches!(
                    payload,
                    GenerationPayload::LootContainer {
                        kind: ContainerKind::Chest,
                        ..
                    }
                ));
            }
        }
    }

    #[test]
    fn swamp_hut_pushes_witch_and_cat_payloads_once() {
        let reg = breg().expect("block registry initialized");
        let dirt = reg.state_of("minecraft:dirt", None);
        let mut gen = crate::worldgen::Gen::load(202, crate::name_to_state).unwrap();
        let mut ctx = FC::new(&mut gen);
        ctx.test_no_terrain = true;
        ctx.test_default_air = true;
        for x in 0..=20 {
            for z in 0..=20 {
                for y in 60..=63 {
                    ctx.test_base_cell((x, y, z), dirt);
                }
            }
        }
        let mut hut = SwampHutPiece::with_orientation(0, 0, Facing::North);
        hut.post_process(&mut ctx, [0, -64, 0, 15, 319, 15]);
        assert!(hut.spawned_witch);
        assert!(hut.spawned_cat);

        let entities: Vec<(&str, [f64; 3], f32)> = ctx
            .payloads
            .iter()
            .filter_map(|(_, p)| match p {
                GenerationPayload::Entity { id, pos, yaw } => Some((id.as_str(), *pos, *yaw)),
                _ => None,
            })
            .collect();
        assert_eq!(entities.len(), 2);
        assert!(entities.contains(&("minecraft:witch", [2.5, 66.0, 3.5], 0.0)));
        assert!(entities.contains(&("minecraft:cat", [2.5, 66.0, 3.5], 0.0)));

        // Re-running over an overlapping clip must not duplicate witch or cat.
        hut.post_process(&mut ctx, [0, -64, 0, 15, 319, 15]);
        let entity_count = ctx
            .payloads
            .iter()
            .filter(|(_, p)| matches!(p, GenerationPayload::Entity { .. }))
            .count();
        assert_eq!(entity_count, 2);
    }
}
