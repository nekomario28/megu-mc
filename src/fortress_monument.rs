// Native Minecraft 1.21.1 multi-piece structures, on the existing generation context:
// - `minecraft:fortress` (`NetherFortressStructure` / `NetherFortressPieces`, pinned `ely`/`elx`)
// - `minecraft:monument` (`OceanMonumentStructure` / `OceanMonumentPieces`, pinned `emc`/`emb`)
//
// Both are "generate a piece list from the structure's random, then run each piece's
// `postProcess` per chunk". The shared scaffolding here is the part both inherit from
// `StructurePiece`: `getWorldX/Y/Z`, `placeBlock` (mirror/rotation from `setOrientation`),
// `generateBox` (Y then X then Z), `getBlock`, `fillColumnDown`, `createChest` and the
// `reorient` chest-facing derivation. Everything below that is the two piece trees.
//
// Three behaviours are easy to get wrong and are called out where they happen:
// - `generateBox` visits every cell in Y,X,Z order and writes even outside the clip, so the
//   visit order decides which writer wins a cell.
// - `StructurePiecesBuilder.moveInsideHeights` runs after generation and shifts every piece
//   by one `nextInt`, so generation and placement see different Y.
// - `createChest` draws its loot seed only when the chest is actually placed, and the turn
//   pieces latch `isNeedingChest` off, so the same piece visited by two chunks draws once.


// ---------- bounds ----------

/// `BoundingBox.orientBox(x, y, z, offX, offY, offZ, width, height, depth, direction)`.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn fm_orient_box(
    x: i32,
    y: i32,
    z: i32,
    ox: i32,
    oy: i32,
    oz: i32,
    width: i32,
    height: i32,
    depth: i32,
    dir: Facing,
) -> Bounds {
    match dir {
        Facing::North => [
            x + ox,
            y + oy,
            z - depth + 1 + oz,
            x + ox + width - 1,
            y + oy + height - 1,
            z + oz,
        ],
        Facing::South => [
            x + ox,
            y + oy,
            z + oz,
            x + ox + width - 1,
            y + oy + height - 1,
            z + oz + depth - 1,
        ],
        Facing::West => [
            x - depth + 1 + oz,
            y + oy,
            z + ox,
            x + oz,
            y + oy + height - 1,
            z + ox + width - 1,
        ],
        Facing::East => [
            x + oz,
            y + oy,
            z + ox,
            x + oz + depth - 1,
            y + oy + height - 1,
            z + ox + width - 1,
        ],
    }
}

fn fm_along_z(dir: Facing) -> bool {
    matches!(dir, Facing::North | Facing::South)
}

/// `StructurePiece.makeBoundingBox(x, y, z, direction, width, height, depth)`.
#[cfg_attr(not(test), allow(dead_code))]
fn fm_make_bounding_box(
    x: i32,
    y: i32,
    z: i32,
    dir: Facing,
    width: i32,
    height: i32,
    depth: i32,
) -> Bounds {
    if fm_along_z(dir) {
        [x, y, z, x + width - 1, y + height - 1, z + depth - 1]
    } else {
        [x, y, z, x + depth - 1, y + height - 1, z + width - 1]
    }
}

/// `BoundingBox.intersects(BoundingBox)`: inclusive on all three axes.
#[cfg_attr(not(test), allow(dead_code))]
fn fm_bb_intersects(a: &Bounds, b: &Bounds) -> bool {
    (0..3).all(|i| a[i + 3] >= b[i] && a[i] <= b[i + 3])
}

#[cfg_attr(not(test), allow(dead_code))]
fn fm_bb_move(b: &mut Bounds, dx: i32, dy: i32, dz: i32) {
    b[0] += dx;
    b[1] += dy;
    b[2] += dz;
    b[3] += dx;
    b[4] += dy;
    b[5] += dz;
}

#[cfg_attr(not(test), allow(dead_code))]
fn fm_bb_encapsulate(acc: &mut Bounds, b: &Bounds) {
    for i in 0..3 {
        acc[i] = acc[i].min(b[i]);
        acc[i + 3] = acc[i + 3].max(b[i + 3]);
    }
}

/// The RNG surface the piece trees need beyond `StructureRng`: `RandomSource.nextInt()`,
/// which is `java.util.Random.nextInt()` and not `nextInt(bound)`.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) trait FullIntRng {
    fn next_int(&mut self) -> i32;
}

impl FullIntRng for Xor {
    fn next_int(&mut self) -> i32 {
        Xor::next_i32(self)
    }
}

impl FullIntRng for Lcg {
    fn next_int(&mut self) -> i32 {
        Lcg::next_i32(self)
    }
}

// ---------- block states ----------

#[cfg_attr(not(test), allow(dead_code))]
fn fm_state(reg: &BReg, name: &str) -> u32 {
    reg.state_of(name, None)
}

/// `Blocks.NETHER_BRICK_FENCE.defaultBlockState().setValue(...)` for the named sides.
#[cfg_attr(not(test), allow(dead_code))]
fn fm_fence(reg: &BReg, north: bool, east: bool, south: bool, west: bool) -> u32 {
    let mut props: Vec<(String, String)> = Vec::new();
    for (name, on) in [
        ("north", north),
        ("east", east),
        ("south", south),
        ("west", west),
    ] {
        if on {
            props.push((name.to_string(), "true".to_string()));
        }
    }
    reg.checked_state("minecraft:nether_brick_fence", &props)
        .expect("nether brick fence state")
}

#[cfg_attr(not(test), allow(dead_code))]
fn fm_stairs(reg: &BReg, facing: Facing) -> u32 {
    reg.checked_state(
        "minecraft:nether_brick_stairs",
        &[("facing".to_string(), facing_name(facing).to_string())],
    )
    .expect("nether brick stairs state")
}

// ---------- the piece writer ----------

/// `StructurePiece`'s local-coordinate writer for one piece: world position mapping from
/// `setOrientation`, `placeBlock`'s clip-and-transform, `generateBox`'s iteration order and
/// `fillColumnDown`'s downward walk.
struct PieceWorld<'a, 'g> {
    ctx: &'a mut FC<'g>,
    reg: &'static BReg,
    clip: Bounds,
    bounds: Bounds,
    orient: Option<Facing>,
    /// The pieces' live container map. Vanilla's block-entity view drops an entry the
    /// moment a later writer replaces that cell with a non-container, so the recorded loot
    /// and the final block state can never disagree.
    loots: &'a mut std::collections::BTreeMap<(i32, i32, i32), SinglePieceLoot>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl<'a, 'g> PieceWorld<'a, 'g> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        ctx: &'a mut FC<'g>,
        reg: &'static BReg,
        clip: Bounds,
        bounds: Bounds,
        orient: Option<Facing>,
        loots: &'a mut std::collections::BTreeMap<(i32, i32, i32), SinglePieceLoot>,
    ) -> Self {
        PieceWorld {
            ctx,
            reg,
            clip,
            bounds,
            orient,
            loots,
        }
    }

    /// `StructurePiece.getWorldPos(x, y, z)`.
    fn world_pos(&self, x: i32, y: i32, z: i32) -> (i32, i32, i32) {
        world_pos(&self.bounds, self.orient, (x, y, z))
    }

    /// `StructurePiece.placeBlock`, minus the fluid tick schedule the port has no owner for.
    fn place(&mut self, st: u32, x: i32, y: i32, z: i32) {
        let p = self.world_pos(x, y, z);
        if !sp_contains(&self.clip, p) {
            return;
        }
        let st = transform_state(self.orient, st);
        if self.reg.needs_postprocess_shape(st) {
            self.ctx.mark_shape(p.0, p.1, p.2);
        }
        self.set_world(p, st);
    }

    fn set_world(&mut self, p: (i32, i32, i32), st: u32) {
        self.ctx.set(p.0, p.1, p.2, st);
        let name = self.reg.name_of(st);
        if !matches!(
            name,
            "minecraft:chest" | "minecraft:dispenser" | "minecraft:suspicious_sand"
        ) {
            self.loots.remove(&p);
        }
    }

    /// `StructurePiece.getBlock(level, x, y, z, box)`: outside the box is air, not a read.
    fn get(&mut self, x: i32, y: i32, z: i32) -> u32 {
        let p = self.world_pos(x, y, z);
        if !sp_contains(&self.clip, p) {
            0
        } else {
            self.ctx.get(p.0, p.1, p.2)
        }
    }

    /// `StructurePiece.generateBox(level, box, x0, y0, z0, x1, y1, z1, edge, fill, skipAir)`
    /// iterates Y, then X, then Z.
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
        for y in y0..=y1 {
            for x in x0..=x1 {
                for z in z0..=z1 {
                    if skip_air && self.reg.is_air(self.get(x, y, z)) {
                        continue;
                    }
                    let on_edge = y == y0 || y == y1 || x == x0 || x == x1 || z == z0 || z == z1;
                    self.place(if on_edge { edge } else { fill }, x, y, z);
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

    /// `StructurePiece.fillColumnDown`: the box gates the first cell only, and the walk keeps
    /// writing past the box while the cell is replaceable.
    fn fill_column_down(&mut self, st: u32, x: i32, y: i32, z: i32) {
        let (wx, mut wy, wz) = self.world_pos(x, y, z);
        if !sp_contains(&self.clip, (wx, wy, wz)) {
            return;
        }
        let floor = self.ctx.g.min_y + 1;
        while wy > floor {
            let cur = self.ctx.get(wx, wy, wz);
            if !self.is_replaceable_by_structures(cur) {
                break;
            }
            self.ctx.set(wx, wy, wz, st);
            wy -= 1;
        }
    }

    /// `StructurePiece.reorient`: the horizontal neighbours decide the chest's facing.
    fn orient_chest(&mut self, (x, y, z): (i32, i32, i32)) -> u32 {
        let mut solid: Option<Facing> = None;
        for dir in HORIZONTAL_FACINGS {
            let (dx, dz) = facing_step(dir);
            let nst = self.ctx.get(x + dx, y, z + dz);
            if self.reg.name_of(nst) == "minecraft:chest" {
                return sp_state(self.reg, "minecraft:chest", &[("facing", "north")]);
            }
            if self.reg.is_solid_render(nst) {
                if solid.is_some() {
                    solid = None;
                    break;
                }
                solid = Some(dir);
            }
        }
        if let Some(wall) = solid {
            return sp_state(
                self.reg,
                "minecraft:chest",
                &[("facing", facing_name(facing_opposite(wall)))],
            );
        }
        // The chest's default facing is NORTH, and the three checks below turn it only while
        // the neighbour in that direction is solid.
        let mut out = Facing::North;
        let mut s = facing_step(out);
        if self
            .reg
            .is_solid_render(self.ctx.get(x + s.0, y, z + s.1))
        {
            out = facing_opposite(out);
            s = facing_step(out);
        }
        if self
            .reg
            .is_solid_render(self.ctx.get(x + s.0, y, z + s.1))
        {
            out = facing_cw(out);
            s = facing_step(out);
        }
        if self
            .reg
            .is_solid_render(self.ctx.get(x + s.0, y, z + s.1))
        {
            out = facing_opposite(out);
        }
        sp_state(
            self.reg,
            "minecraft:chest",
            &[("facing", facing_name(out))],
        )
    }

    /// `StructurePiece.createChest`: the loot seed is drawn only when the chest is created.
    fn create_chest<R: StructureRng + ?Sized>(
        &mut self,
        rng: &mut R,
        x: i32,
        y: i32,
        z: i32,
        table: &'static str,
    ) -> bool {
        let p = self.world_pos(x, y, z);
        if !sp_contains(&self.clip, p) {
            return false;
        }
        if self.reg.name_of(self.ctx.get(p.0, p.1, p.2)) == "minecraft:chest" {
            return false;
        }
        let chest = self.orient_chest(p);
        self.set_world(p, chest);
        let seed = rng.next_i64();
        self.ctx.payloads.push((
            p,
            crate::generation_payload::GenerationPayload::LootContainer {
                kind: crate::generation_payload::ContainerKind::Chest,
                loot_table: table.to_string(),
                loot_seed: Some(seed),
            },
        ));
        self.loots.insert(
            p,
            SinglePieceLoot {
                pos: p,
                kind: LootKind::Chest,
                table,
                seed,
            },
        );
        true
    }
}

/// Java's `boolean` spelling in the piece tags.
#[cfg_attr(not(test), allow(dead_code))]
fn piece_bool(v: bool) -> &'static str {
    if v {
        "1"
    } else {
        "0"
    }
}

// ================= minecraft:fortress =================

#[cfg_attr(not(test), allow(dead_code))]
const FORTRESS_MAX_DEPTH: i32 = 30;
#[cfg_attr(not(test), allow(dead_code))]
const FORTRESS_LOWEST_Y: i32 = 10;
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const FORTRESS_MAGIC_START_Y: i32 = 64;
/// `BuiltInLootTables.NETHER_BRIDGE`.
#[cfg_attr(not(test), allow(dead_code))]
const FORTRESS_LOOT_TABLE: &str = "minecraft:chests/nether_bridge";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) enum FortressKind {
    Start,
    BridgeCrossing,
    BridgeEndFiller,
    BridgeStraight,
    CastleCorridorStairs,
    CastleCorridorTBalcony,
    CastleEntrance,
    CastleSmallCorridorCrossing,
    CastleSmallCorridorLeftTurn,
    CastleSmallCorridorRightTurn,
    CastleSmallCorridor,
    CastleStalkRoom,
    MonsterThrone,
    RoomCrossing,
    StairsRoom,
}

#[cfg_attr(not(test), allow(dead_code))]
impl FortressKind {
    /// The 1.21.1 `StructurePieceType` registry id this piece serialises as. The ids are the
    /// game's own (`NeBS` and friends lowercased), so the fixture's piece list is the native
    /// tag's `id` field rather than a name the port invented.
    pub(crate) fn registry_id(self) -> &'static str {
        match self {
            // `StartPiece` calls `BridgeCrossing(int, int, Direction)`, which hardcodes the
            // bridge-crossing type: the start piece serialises as a bridge crossing.
            Self::Start => "minecraft:nebcr",
            Self::BridgeCrossing => "minecraft:nebcr",
            Self::BridgeEndFiller => "minecraft:nebef",
            Self::BridgeStraight => "minecraft:nebs",
            Self::CastleCorridorStairs => "minecraft:neccs",
            Self::CastleCorridorTBalcony => "minecraft:nectb",
            Self::CastleEntrance => "minecraft:nece",
            Self::CastleSmallCorridorCrossing => "minecraft:nescsc",
            Self::CastleSmallCorridorLeftTurn => "minecraft:nesclt",
            Self::CastleSmallCorridorRightTurn => "minecraft:nescrt",
            Self::CastleSmallCorridor => "minecraft:nesc",
            Self::CastleStalkRoom => "minecraft:necsr",
            Self::MonsterThrone => "minecraft:nemt",
            Self::RoomCrossing => "minecraft:nerc",
            Self::StairsRoom => "minecraft:nesr",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
struct PieceWeight {
    kind: FortressKind,
    weight: i32,
    max_place_count: i32,
    allow_in_row: bool,
}

/// `BRIDGE_PIECE_WEIGHTS` then `CASTLE_PIECE_WEIGHTS`, flattened so one id names a weight.
/// Ids 0..=5 are the bridge list, 6..=12 the castle list, in their declared order.
#[cfg_attr(not(test), allow(dead_code))]
const FORTRESS_WEIGHTS: [PieceWeight; 13] = [
    PieceWeight {
        kind: FortressKind::BridgeStraight,
        weight: 30,
        max_place_count: 0,
        allow_in_row: true,
    },
    PieceWeight {
        kind: FortressKind::BridgeCrossing,
        weight: 10,
        max_place_count: 4,
        allow_in_row: false,
    },
    PieceWeight {
        kind: FortressKind::RoomCrossing,
        weight: 10,
        max_place_count: 4,
        allow_in_row: false,
    },
    PieceWeight {
        kind: FortressKind::StairsRoom,
        weight: 10,
        max_place_count: 3,
        allow_in_row: false,
    },
    PieceWeight {
        kind: FortressKind::MonsterThrone,
        weight: 5,
        max_place_count: 2,
        allow_in_row: false,
    },
    PieceWeight {
        kind: FortressKind::CastleEntrance,
        weight: 5,
        max_place_count: 1,
        allow_in_row: false,
    },
    PieceWeight {
        kind: FortressKind::CastleSmallCorridor,
        weight: 25,
        max_place_count: 0,
        allow_in_row: true,
    },
    PieceWeight {
        kind: FortressKind::CastleSmallCorridorCrossing,
        weight: 15,
        max_place_count: 5,
        allow_in_row: false,
    },
    PieceWeight {
        kind: FortressKind::CastleSmallCorridorRightTurn,
        weight: 5,
        max_place_count: 10,
        allow_in_row: false,
    },
    PieceWeight {
        kind: FortressKind::CastleSmallCorridorLeftTurn,
        weight: 5,
        max_place_count: 10,
        allow_in_row: false,
    },
    PieceWeight {
        kind: FortressKind::CastleCorridorStairs,
        weight: 10,
        max_place_count: 3,
        allow_in_row: true,
    },
    PieceWeight {
        kind: FortressKind::CastleCorridorTBalcony,
        weight: 7,
        max_place_count: 2,
        allow_in_row: false,
    },
    PieceWeight {
        kind: FortressKind::CastleStalkRoom,
        weight: 5,
        max_place_count: 2,
        allow_in_row: false,
    },
];

#[cfg_attr(not(test), allow(dead_code))]
const BRIDGE_WEIGHT_IDS: [u32; 6] = [0, 1, 2, 3, 4, 5];
#[cfg_attr(not(test), allow(dead_code))]
const CASTLE_WEIGHT_IDS: [u32; 7] = [6, 7, 8, 9, 10, 11, 12];

/// One generated `NetherBridgePiece`. Mutable fields are the pieces' own `postProcess` state:
/// the turn pieces consume `chest_pending` once and the throne consumes `spawner_placed`.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct FortressPiece {
    pub(crate) kind: FortressKind,
    pub(crate) depth: i32,
    pub(crate) bounds: Bounds,
    pub(crate) orient: Facing,
    pub(crate) self_seed: i32,
    pub(crate) chest_pending: bool,
    pub(crate) spawner_placed: bool,
}

#[cfg_attr(not(test), allow(dead_code))]
impl FortressPiece {
    pub(crate) fn canonical(&self) -> String {
        let bb = self
            .bounds
            .iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let mut out = format!(
            "{},{},{},{}",
            self.kind.registry_id(),
            facing_data2d(self.orient),
            self.depth,
            bb
        );
        // `addAdditionalSaveData` order: the filler writes Seed, the turn pieces write
        // Chest, the throne writes Mob.
        match self.kind {
            FortressKind::BridgeEndFiller => {
                out.push_str(&format!(",Seed={}", self.self_seed));
            }
            FortressKind::CastleSmallCorridorLeftTurn | FortressKind::CastleSmallCorridorRightTurn => {
                out.push_str(&format!(",Chest={}", piece_bool(self.chest_pending)));
            }
            FortressKind::MonsterThrone => {
                out.push_str(&format!(",Mob={}", piece_bool(self.spawner_placed)));
            }
            _ => {}
        }
        out
    }
}

#[derive(Clone, Debug, Default)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct FortressLayout {
    pub(crate) pieces: Vec<FortressPiece>,
    /// Encapsulated piece bounds after `StructurePiecesBuilder.moveInsideHeights`.
    pub(crate) bounds: Bounds,
}

struct FortressBuilder {
    pieces: Vec<FortressPiece>,
    pending: Vec<usize>,
    place_count: [i32; 13],
    available: [Vec<u32>; 2],
    previous: Option<u32>,
    start: Bounds,
}

#[cfg_attr(not(test), allow(dead_code))]
impl FortressBuilder {
    fn new(start: Bounds) -> Self {
        FortressBuilder {
            pieces: Vec::new(),
            pending: Vec::new(),
            place_count: [0; 13],
            available: [BRIDGE_WEIGHT_IDS.to_vec(), CASTLE_WEIGHT_IDS.to_vec()],
            previous: None,
            start,
        }
    }

    fn find_collision(&self, bb: &Bounds) -> bool {
        self.pieces.iter().any(|p| fm_bb_intersects(&p.bounds, bb))
    }

    fn do_place(&self, wid: usize) -> bool {
        let w = FORTRESS_WEIGHTS[wid];
        w.max_place_count == 0 || self.place_count[wid] < w.max_place_count
    }

    /// `NetherBridgePiece.updatePieceWeight`
    fn update_piece_weight(&self, which: usize) -> i32 {
        let mut any = false;
        let mut total = 0;
        for &wid in &self.available[which] {
            let w = FORTRESS_WEIGHTS[wid as usize];
            if w.max_place_count > 0 && self.place_count[wid as usize] < w.max_place_count {
                any = true;
            }
            total += w.weight;
        }
        if any {
            total
        } else {
            -1
        }
    }

    fn push(&mut self, piece: FortressPiece) -> usize {
        let id = self.pieces.len();
        self.pieces.push(piece);
        id
    }

    /// `NetherBridgePiece.generatePiece`: five attempts, each drawing one weight index.
    fn generate_piece<R: StructureRng + FullIntRng + ?Sized>(
        &mut self,
        which: usize,
        rng: &mut R,
        x: i32,
        y: i32,
        z: i32,
        dir: Facing,
        depth: i32,
    ) -> Option<usize> {
        let total = self.update_piece_weight(which);
        let possible = total > 0 && depth <= FORTRESS_MAX_DEPTH;
        let mut attempts = 0;
        while attempts < 5 && possible {
            attempts += 1;
            let mut r = rng.next_bound(total);
            let mut pos = 0;
            while pos < self.available[which].len() {
                let wid = self.available[which][pos];
                let w = FORTRESS_WEIGHTS[wid as usize];
                r -= w.weight;
                if r < 0 {
                    if !self.do_place(wid as usize)
                        || (self.previous == Some(wid) && !w.allow_in_row)
                    {
                        break;
                    }
                    if let Some(id) = self.create_piece(w.kind, rng, x, y, z, dir, depth) {
                        self.place_count[wid as usize] += 1;
                        self.previous = Some(wid);
                        if !self.do_place(wid as usize) {
                            self.available[which].remove(pos);
                        }
                        return Some(id);
                    }
                }
                pos += 1;
            }
        }
        self.create_bridge_end_filler(rng, x, y, z, dir, depth)
    }

    /// `NetherFortressPieces.createPiece`, which builds the box, rejects it, and only then
    /// runs the constructor (the constructors are where the draws happen).
    #[allow(clippy::too_many_arguments)]
    fn create_piece<R: StructureRng + ?Sized>(
        &mut self,
        kind: FortressKind,
        rng: &mut R,
        x: i32,
        y: i32,
        z: i32,
        dir: Facing,
        depth: i32,
    ) -> Option<usize> {
        let bb = match kind {
            FortressKind::BridgeStraight => fm_orient_box(x, y, z, -1, -3, 0, 5, 10, 19, dir),
            FortressKind::BridgeCrossing => fm_orient_box(x, y, z, -8, -3, 0, 19, 10, 19, dir),
            FortressKind::RoomCrossing => fm_orient_box(x, y, z, -2, 0, 0, 7, 9, 7, dir),
            FortressKind::StairsRoom => fm_orient_box(x, y, z, -2, 0, 0, 7, 11, 7, dir),
            FortressKind::MonsterThrone => fm_orient_box(x, y, z, -2, 0, 0, 7, 8, 9, dir),
            FortressKind::CastleEntrance => fm_orient_box(x, y, z, -5, -3, 0, 13, 14, 13, dir),
            FortressKind::CastleSmallCorridor => fm_orient_box(x, y, z, -1, 0, 0, 5, 7, 5, dir),
            FortressKind::CastleSmallCorridorCrossing => {
                fm_orient_box(x, y, z, -1, 0, 0, 5, 7, 5, dir)
            }
            FortressKind::CastleSmallCorridorRightTurn
            | FortressKind::CastleSmallCorridorLeftTurn => {
                fm_orient_box(x, y, z, -1, 0, 0, 5, 7, 5, dir)
            }
            FortressKind::CastleCorridorStairs => fm_orient_box(x, y, z, -1, -7, 0, 5, 14, 10, dir),
            FortressKind::CastleCorridorTBalcony => fm_orient_box(x, y, z, -3, 0, 0, 9, 7, 9, dir),
            FortressKind::CastleStalkRoom => fm_orient_box(x, y, z, -5, -3, 0, 13, 14, 13, dir),
            FortressKind::Start | FortressKind::BridgeEndFiller => return None,
        };
        if bb[1] <= FORTRESS_LOWEST_Y || self.find_collision(&bb) {
            return None;
        }
        // Constructor-time draws, in the order the pieces' constructors make them.
        let mut chest_pending = false;
        if matches!(
            kind,
            FortressKind::CastleSmallCorridorRightTurn | FortressKind::CastleSmallCorridorLeftTurn
        ) {
            chest_pending = rng.next_bound(3) == 0;
        }
        Some(self.push(FortressPiece {
            kind,
            depth,
            bounds: bb,
            orient: dir,
            self_seed: 0,
            chest_pending,
            spawner_placed: false,
        }))
    }

    /// `NetherBridgePiece.generatePiece`'s tail: the filler is a real piece here, so it goes
    /// into the piece list and (through `generateAndAddPiece`) into the pending queue.
    fn create_bridge_end_filler<R: StructureRng + FullIntRng + ?Sized>(
        &mut self,
        rng: &mut R,
        x: i32,
        y: i32,
        z: i32,
        dir: Facing,
        depth: i32,
    ) -> Option<usize> {
        let bb = fm_orient_box(x, y, z, -1, -3, 0, 5, 10, 8, dir);
        if bb[1] <= FORTRESS_LOWEST_Y || self.find_collision(&bb) {
            return None;
        }
        let self_seed = rng.next_int();
        Some(self.push(FortressPiece {
            kind: FortressKind::BridgeEndFiller,
            depth,
            bounds: bb,
            orient: dir,
            self_seed,
            chest_pending: false,
            spawner_placed: false,
        }))
    }

    /// The `generateAndAddPiece` branch for a target more than 112 blocks from the start.
    /// It still runs `BridgeEndFiller`'s constructor - and therefore still draws the piece's
    /// seed - but the result is discarded: it is never added to the builder.
    fn draw_discarded_bridge_end_filler<R: StructureRng + FullIntRng + ?Sized>(
        &mut self,
        rng: &mut R,
        x: i32,
        y: i32,
        z: i32,
        dir: Facing,
    ) {
        let bb = fm_orient_box(x, y, z, -1, -3, 0, 5, 10, 8, dir);
        if bb[1] <= FORTRESS_LOWEST_Y || self.find_collision(&bb) {
            return;
        }
        rng.next_int();
    }

    /// `NetherBridgePiece.generateAndAddPiece`
    #[allow(clippy::too_many_arguments)]
    fn generate_and_add<R: StructureRng + FullIntRng + ?Sized>(
        &mut self,
        rng: &mut R,
        x: i32,
        y: i32,
        z: i32,
        dir: Facing,
        depth: i32,
        castle: bool,
    ) -> Option<usize> {
        if (x - self.start[0]).abs() <= 112 && (z - self.start[2]).abs() <= 112 {
            // Only this branch queues the child; the far-away branch below builds and
            // discards a filler, so it never reaches the piece list.
            let which = usize::from(castle);
            let made = self.generate_piece(which, rng, x, y, z, dir, depth + 1);
            if let Some(id) = made {
                self.pending.push(id);
            }
            return made;
        }
        self.draw_discarded_bridge_end_filler(rng, x, y, z, dir);
        None
    }

    fn child_forward<R: StructureRng + FullIntRng + ?Sized>(
        &mut self,
        this: usize,
        rng: &mut R,
        xoff: i32,
        yoff: i32,
        castle: bool,
    ) -> Option<usize> {
        let b = self.pieces[this].bounds;
        let depth = self.pieces[this].depth;
        let (x, y, z, out) = match self.pieces[this].orient {
            Facing::North => (b[0] + xoff, b[1] + yoff, b[2] - 1, Facing::North),
            Facing::South => (b[0] + xoff, b[1] + yoff, b[5] + 1, Facing::South),
            Facing::West => (b[0] - 1, b[1] + yoff, b[2] + xoff, Facing::West),
            Facing::East => (b[3] + 1, b[1] + yoff, b[2] + xoff, Facing::East),
        };
        self.generate_and_add(rng, x, y, z, out, depth, castle)
    }

    fn child_left<R: StructureRng + FullIntRng + ?Sized>(
        &mut self,
        this: usize,
        rng: &mut R,
        xoff: i32,
        yoff: i32,
        castle: bool,
    ) -> Option<usize> {
        let b = self.pieces[this].bounds;
        let depth = self.pieces[this].depth;
        let (x, y, z, out) = match self.pieces[this].orient {
            Facing::North | Facing::South => (b[0] - 1, b[1] + xoff, b[2] + yoff, Facing::West),
            Facing::West | Facing::East => (b[0] + yoff, b[1] + xoff, b[2] - 1, Facing::North),
        };
        self.generate_and_add(rng, x, y, z, out, depth, castle)
    }

    fn child_right<R: StructureRng + FullIntRng + ?Sized>(
        &mut self,
        this: usize,
        rng: &mut R,
        xoff: i32,
        yoff: i32,
        castle: bool,
    ) -> Option<usize> {
        let b = self.pieces[this].bounds;
        let depth = self.pieces[this].depth;
        let (x, y, z, out) = match self.pieces[this].orient {
            Facing::North | Facing::South => (b[3] + 1, b[1] + xoff, b[2] + yoff, Facing::East),
            Facing::West | Facing::East => (b[0] + yoff, b[1] + xoff, b[5] + 1, Facing::South),
        };
        self.generate_and_add(rng, x, y, z, out, depth, castle)
    }

    /// `NetherBridgePiece.addChildren` per piece type, in the order the pieces declare them.
    fn add_children<R: StructureRng + FullIntRng + ?Sized>(&mut self, this: usize, rng: &mut R) {
        match self.pieces[this].kind {
            FortressKind::Start | FortressKind::BridgeCrossing => {
                self.child_forward(this, rng, 8, 3, false);
                self.child_left(this, rng, 3, 8, false);
                self.child_right(this, rng, 3, 8, false);
            }
            FortressKind::BridgeStraight => {
                self.child_forward(this, rng, 1, 3, false);
            }
            FortressKind::BridgeEndFiller => {}
            FortressKind::CastleCorridorStairs => {
                self.child_forward(this, rng, 1, 0, true);
            }
            FortressKind::CastleCorridorTBalcony => {
                let dir = self.pieces[this].orient;
                let i = if matches!(dir, Facing::West | Facing::North) {
                    5
                } else {
                    1
                };
                // Argument evaluation order: each flag is drawn immediately before the child
                // that uses it, so the left subtree's draws sit between the two `nextInt(8)`s.
                let left_castle = rng.next_bound(8) > 0;
                self.child_left(this, rng, 0, i, left_castle);
                let right_castle = rng.next_bound(8) > 0;
                self.child_right(this, rng, 0, i, right_castle);
            }
            FortressKind::CastleEntrance => {
                self.child_forward(this, rng, 5, 3, true);
            }
            FortressKind::CastleSmallCorridorCrossing => {
                self.child_forward(this, rng, 1, 0, true);
                self.child_left(this, rng, 0, 1, true);
                self.child_right(this, rng, 0, 1, true);
            }
            FortressKind::CastleSmallCorridorLeftTurn => {
                self.child_left(this, rng, 0, 1, true);
            }
            FortressKind::CastleSmallCorridorRightTurn => {
                self.child_right(this, rng, 0, 1, true);
            }
            FortressKind::CastleSmallCorridor => {
                self.child_forward(this, rng, 1, 0, true);
            }
            FortressKind::CastleStalkRoom => {
                self.child_forward(this, rng, 5, 3, true);
                self.child_forward(this, rng, 5, 11, true);
            }
            FortressKind::MonsterThrone => {}
            FortressKind::RoomCrossing => {
                self.child_forward(this, rng, 2, 0, false);
                self.child_left(this, rng, 0, 2, false);
                self.child_right(this, rng, 0, 2, false);
            }
            FortressKind::StairsRoom => {
                self.child_right(this, rng, 6, 2, false);
            }
        }
    }
}

/// `NetherFortressStructure.generatePieces` including the closing
/// `StructurePiecesBuilder.moveInsideHeights(random, 48, 70)`.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn generate_fortress_pieces<R: StructureRng + FullIntRng + ?Sized>(
    rng: &mut R,
    cx: i32,
    cz: i32,
) -> FortressLayout {
    // `new StartPiece(random, chunkPos.getBlockX(2), chunkPos.getBlockZ(2))`
    let dir = HORIZONTAL_FACINGS[rng.next_bound(4) as usize];
    let start_bounds = fm_make_bounding_box(
        cx * 16 + 2,
        FORTRESS_MAGIC_START_Y,
        cz * 16 + 2,
        dir,
        19,
        10,
        19,
    );
    let mut b = FortressBuilder::new(start_bounds);
    let start = b.push(FortressPiece {
        kind: FortressKind::Start,
        depth: 0,
        bounds: start_bounds,
        orient: dir,
        self_seed: 0,
        chest_pending: false,
        spawner_placed: false,
    });
    b.add_children(start, rng);

    while !b.pending.is_empty() {
        let i = rng.next_bound(b.pending.len() as i32) as usize;
        let child = b.pending.remove(i);
        b.add_children(child, rng);
    }

    // moveInsideHeights(random, 48, 70)
    let mut bb = b.pieces[0].bounds;
    for p in &b.pieces[1..] {
        fm_bb_encapsulate(&mut bb, &p.bounds);
    }
    let span = 70 - 48 + 1 - (bb[4] - bb[1] + 1);
    let top = if span > 1 { 48 + rng.next_bound(span) } else { 48 };
    let shift = top - bb[1];
    let mut bounds = bb;
    fm_bb_move(&mut bounds, 0, shift, 0);
    for p in &mut b.pieces {
        fm_bb_move(&mut p.bounds, 0, shift, 0);
    }
    FortressLayout {
        pieces: b.pieces,
        bounds,
    }
}

// ---------- fortress placement ----------

#[cfg_attr(not(test), allow(dead_code))]
struct FortressStates {
    bricks: u32,
    air: u32,
    fence_ns: u32,
    fence_we: u32,
    fence_ns_e: u32,
    fence_ns_w: u32,
    fence_e: u32,
    fence_w: u32,
    fence_ne: u32,
    fence_se: u32,
    fence_sw: u32,
    fence_nw: u32,
    stairs_s: u32,
    stairs_n: u32,
    stairs_e: u32,
    stairs_w: u32,
    lava: u32,
    soul_sand: u32,
    nether_wart: u32,
    spawner: u32,
    plain_fence: u32,
}

#[cfg_attr(not(test), allow(dead_code))]
impl FortressStates {
    fn new(reg: &BReg) -> Self {
        let f = |n: bool, e: bool, s: bool, w: bool| fm_fence(reg, n, e, s, w);
        FortressStates {
            bricks: fm_state(reg, "minecraft:nether_bricks"),
            air: fm_state(reg, "minecraft:air"),
            fence_ns: f(true, false, true, false),
            fence_we: f(false, true, false, true),
            fence_ns_e: f(true, true, true, false),
            fence_ns_w: f(true, false, true, true),
            fence_e: f(false, true, false, false),
            fence_w: f(false, false, false, true),
            fence_ne: f(true, true, false, false),
            fence_se: f(false, true, true, false),
            fence_sw: f(false, false, true, true),
            fence_nw: f(true, false, false, true),
            stairs_s: fm_stairs(reg, Facing::South),
            stairs_n: fm_stairs(reg, Facing::North),
            stairs_e: fm_stairs(reg, Facing::East),
            stairs_w: fm_stairs(reg, Facing::West),
            lava: fm_state(reg, "minecraft:lava"),
            soul_sand: fm_state(reg, "minecraft:soul_sand"),
            nether_wart: fm_state(reg, "minecraft:nether_wart"),
            spawner: fm_state(reg, "minecraft:spawner"),
            plain_fence: f(false, false, false, false),
        }
    }
}

/// One fortress piece's `postProcess` against one clip box.
#[cfg_attr(not(test), allow(dead_code))]
fn fortress_post_process<R: StructureRng + ?Sized>(
    w: &mut PieceWorld<'_, '_>,
    s: &FortressStates,
    piece: &mut FortressPiece,
    rng: &mut R,
    clip: Bounds,
) {
    match piece.kind {
        FortressKind::Start | FortressKind::BridgeCrossing => {
            let b = s.bricks;
            w.generate_box(7, 3, 0, 11, 4, 18, b, b, false);
            w.generate_box(0, 3, 7, 18, 4, 11, b, b, false);
            w.generate_box(8, 5, 0, 10, 7, 18, s.air, s.air, false);
            w.generate_box(0, 5, 8, 18, 7, 10, s.air, s.air, false);
            w.generate_box(7, 5, 0, 7, 5, 7, b, b, false);
            w.generate_box(7, 5, 11, 7, 5, 18, b, b, false);
            w.generate_box(11, 5, 0, 11, 5, 7, b, b, false);
            w.generate_box(11, 5, 11, 11, 5, 18, b, b, false);
            w.generate_box(0, 5, 7, 7, 5, 7, b, b, false);
            w.generate_box(11, 5, 7, 18, 5, 7, b, b, false);
            w.generate_box(0, 5, 11, 7, 5, 11, b, b, false);
            w.generate_box(11, 5, 11, 18, 5, 11, b, b, false);
            w.generate_box(7, 2, 0, 11, 2, 5, b, b, false);
            w.generate_box(7, 2, 13, 11, 2, 18, b, b, false);
            w.generate_box(7, 0, 0, 11, 1, 3, b, b, false);
            w.generate_box(7, 0, 15, 11, 1, 18, b, b, false);
            for i in 7..=11 {
                for j in 0..=2 {
                    w.fill_column_down(b, i, -1, j);
                    w.fill_column_down(b, i, -1, 18 - j);
                }
            }
            w.generate_box(0, 2, 7, 5, 2, 11, b, b, false);
            w.generate_box(13, 2, 7, 18, 2, 11, b, b, false);
            w.generate_box(0, 0, 7, 3, 1, 11, b, b, false);
            w.generate_box(15, 0, 7, 18, 1, 11, b, b, false);
            for i in 0..=2 {
                for j in 7..=11 {
                    w.fill_column_down(b, i, -1, j);
                    w.fill_column_down(b, 18 - i, -1, j);
                }
            }
        }
        FortressKind::BridgeEndFiller => {
            let mut r = Lcg::new(piece.self_seed as u64);
            let b = s.bricks;
            for i in 0..=4 {
                for j in 3..=4 {
                    let d = r.next_bound(8);
                    w.generate_box(i, j, 0, i, j, d, b, b, false);
                }
            }
            let mut d = r.next_bound(8);
            w.generate_box(0, 5, 0, 0, 5, d, b, b, false);
            d = r.next_bound(8);
            w.generate_box(4, 5, 0, 4, 5, d, b, b, false);
            for i in 0..=4 {
                let d = r.next_bound(5);
                w.generate_box(i, 2, 0, i, 2, d, b, b, false);
            }
            for i in 0..=4 {
                for j in 0..=1 {
                    let d = r.next_bound(3);
                    w.generate_box(i, j, 0, i, j, d, b, b, false);
                }
            }
        }
        FortressKind::BridgeStraight => {
            let b = s.bricks;
            w.generate_box(0, 3, 0, 4, 4, 18, b, b, false);
            w.generate_box(1, 5, 0, 3, 7, 18, s.air, s.air, false);
            w.generate_box(0, 5, 0, 0, 5, 18, b, b, false);
            w.generate_box(4, 5, 0, 4, 5, 18, b, b, false);
            w.generate_box(0, 2, 0, 4, 2, 5, b, b, false);
            w.generate_box(0, 2, 13, 4, 2, 18, b, b, false);
            w.generate_box(0, 0, 0, 4, 1, 3, b, b, false);
            w.generate_box(0, 0, 15, 4, 1, 18, b, b, false);
            for i in 0..=4 {
                for j in 0..=2 {
                    w.fill_column_down(b, i, -1, j);
                    w.fill_column_down(b, i, -1, 18 - j);
                }
            }
            let ns_e = s.fence_ns_e;
            let ns_w = s.fence_ns_w;
            w.generate_box(0, 1, 1, 0, 4, 1, ns_e, ns_e, false);
            w.generate_box(0, 3, 4, 0, 4, 4, ns_e, ns_e, false);
            w.generate_box(0, 3, 14, 0, 4, 14, ns_e, ns_e, false);
            w.generate_box(0, 1, 17, 0, 4, 17, ns_e, ns_e, false);
            w.generate_box(4, 1, 1, 4, 4, 1, ns_w, ns_w, false);
            w.generate_box(4, 3, 4, 4, 4, 4, ns_w, ns_w, false);
            w.generate_box(4, 3, 14, 4, 4, 14, ns_w, ns_w, false);
            w.generate_box(4, 1, 17, 4, 4, 17, ns_w, ns_w, false);
        }
        FortressKind::CastleCorridorStairs => {
            let stair = s.stairs_s;
            let fence = s.fence_ns;
            let b = s.bricks;
            for i in 0..=9 {
                let lo = (7 - i).max(1);
                let hi = (lo + 5).max(14 - i).min(13);
                w.generate_box(0, 0, i, 4, lo, i, b, b, false);
                w.generate_box(1, lo + 1, i, 3, hi - 1, i, s.air, s.air, false);
                if i <= 6 {
                    w.place(stair, 1, lo + 1, i);
                    w.place(stair, 2, lo + 1, i);
                    w.place(stair, 3, lo + 1, i);
                }
                w.generate_box(0, hi, i, 4, hi, i, b, b, false);
                w.generate_box(0, lo + 1, i, 0, hi - 1, i, b, b, false);
                w.generate_box(4, lo + 1, i, 4, hi - 1, i, b, b, false);
                if i & 1 == 0 {
                    w.generate_box(0, lo + 2, i, 0, lo + 3, i, fence, fence, false);
                    w.generate_box(4, lo + 2, i, 4, lo + 3, i, fence, fence, false);
                }
                for j in 0..=4 {
                    w.fill_column_down(b, j, -1, i);
                }
            }
        }
        FortressKind::CastleCorridorTBalcony => {
            let b = s.bricks;
            let ns = s.fence_ns;
            let we = s.fence_we;
            w.generate_box(0, 0, 0, 8, 1, 8, b, b, false);
            w.generate_box(0, 2, 0, 8, 5, 8, s.air, s.air, false);
            w.generate_box(0, 6, 0, 8, 6, 5, b, b, false);
            w.generate_box(0, 2, 0, 2, 5, 0, b, b, false);
            w.generate_box(6, 2, 0, 8, 5, 0, b, b, false);
            w.generate_box(1, 3, 0, 1, 4, 0, we, we, false);
            w.generate_box(7, 3, 0, 7, 4, 0, we, we, false);
            w.generate_box(0, 2, 4, 8, 2, 8, b, b, false);
            w.generate_box(1, 1, 4, 2, 2, 4, s.air, s.air, false);
            w.generate_box(6, 1, 4, 7, 2, 4, s.air, s.air, false);
            w.generate_box(1, 3, 8, 7, 3, 8, we, we, false);
            w.place(s.fence_se, 0, 3, 8);
            w.place(s.fence_sw, 8, 3, 8);
            w.generate_box(0, 3, 6, 0, 3, 7, ns, ns, false);
            w.generate_box(8, 3, 6, 8, 3, 7, ns, ns, false);
            w.generate_box(0, 3, 4, 0, 5, 5, b, b, false);
            w.generate_box(8, 3, 4, 8, 5, 5, b, b, false);
            w.generate_box(1, 3, 5, 2, 5, 5, b, b, false);
            w.generate_box(6, 3, 5, 7, 5, 5, b, b, false);
            w.generate_box(1, 4, 5, 1, 5, 5, we, we, false);
            w.generate_box(7, 4, 5, 7, 5, 5, we, we, false);
            for i in 0..=5 {
                for j in 0..=8 {
                    w.fill_column_down(b, j, -1, i);
                }
            }
        }
        FortressKind::CastleEntrance => {
            let b = s.bricks;
            let we = s.fence_we;
            let ns = s.fence_ns;
            w.generate_box(0, 3, 0, 12, 4, 12, b, b, false);
            w.generate_box(0, 5, 0, 12, 13, 12, s.air, s.air, false);
            w.generate_box(0, 5, 0, 1, 12, 12, b, b, false);
            w.generate_box(11, 5, 0, 12, 12, 12, b, b, false);
            w.generate_box(2, 5, 11, 4, 12, 12, b, b, false);
            w.generate_box(8, 5, 11, 10, 12, 12, b, b, false);
            w.generate_box(5, 9, 11, 7, 12, 12, b, b, false);
            w.generate_box(2, 5, 0, 4, 12, 1, b, b, false);
            w.generate_box(8, 5, 0, 10, 12, 1, b, b, false);
            w.generate_box(5, 9, 0, 7, 12, 1, b, b, false);
            w.generate_box(2, 11, 2, 10, 12, 10, b, b, false);
            w.generate_box(5, 8, 0, 7, 8, 0, s.plain_fence, s.plain_fence, false);
            let mut i = 1;
            while i <= 11 {
                w.generate_box(i, 10, 0, i, 11, 0, we, we, false);
                w.generate_box(i, 10, 12, i, 11, 12, we, we, false);
                w.generate_box(0, 10, i, 0, 11, i, ns, ns, false);
                w.generate_box(12, 10, i, 12, 11, i, ns, ns, false);
                w.place(b, i, 13, 0);
                w.place(b, i, 13, 12);
                w.place(b, 0, 13, i);
                w.place(b, 12, 13, i);
                if i != 11 {
                    w.place(we, i + 1, 13, 0);
                    w.place(we, i + 1, 13, 12);
                    w.place(ns, 0, 13, i + 1);
                    w.place(ns, 12, 13, i + 1);
                }
                i += 2;
            }
            w.place(s.fence_ne, 0, 13, 0);
            w.place(s.fence_se, 0, 13, 12);
            w.place(s.fence_sw, 12, 13, 12);
            w.place(s.fence_nw, 12, 13, 0);
            let mut k = 3;
            while k <= 9 {
                w.generate_box(1, 7, k, 1, 8, k, s.fence_ns_w, s.fence_ns_w, false);
                w.generate_box(11, 7, k, 11, 8, k, s.fence_ns_e, s.fence_ns_e, false);
                k += 2;
            }
            w.generate_box(4, 2, 0, 8, 2, 12, b, b, false);
            w.generate_box(0, 2, 4, 12, 2, 8, b, b, false);
            w.generate_box(4, 0, 0, 8, 1, 3, b, b, false);
            w.generate_box(4, 0, 9, 8, 1, 12, b, b, false);
            w.generate_box(0, 0, 4, 3, 1, 8, b, b, false);
            w.generate_box(9, 0, 4, 12, 1, 8, b, b, false);
            for i in 4..=8 {
                for j in 0..=2 {
                    w.fill_column_down(b, i, -1, j);
                    w.fill_column_down(b, i, -1, 12 - j);
                }
            }
            for i in 0..=2 {
                for j in 4..=8 {
                    w.fill_column_down(b, i, -1, j);
                    w.fill_column_down(b, 12 - i, -1, j);
                }
            }
            w.generate_box(5, 5, 5, 7, 5, 7, b, b, false);
            w.generate_box(6, 1, 6, 6, 4, 6, s.air, s.air, false);
            w.place(b, 6, 0, 6);
            w.place(s.lava, 6, 5, 6);
        }
        FortressKind::CastleSmallCorridorCrossing => {
            let b = s.bricks;
            w.generate_box(0, 0, 0, 4, 1, 4, b, b, false);
            w.generate_box(0, 2, 0, 4, 5, 4, s.air, s.air, false);
            w.generate_box(0, 2, 0, 0, 5, 0, b, b, false);
            w.generate_box(4, 2, 0, 4, 5, 0, b, b, false);
            w.generate_box(0, 2, 4, 0, 5, 4, b, b, false);
            w.generate_box(4, 2, 4, 4, 5, 4, b, b, false);
            w.generate_box(0, 6, 0, 4, 6, 4, b, b, false);
            for i in 0..=4 {
                for j in 0..=4 {
                    w.fill_column_down(b, i, -1, j);
                }
            }
        }
        FortressKind::CastleSmallCorridorLeftTurn => {
            let b = s.bricks;
            let we = s.fence_we;
            let ns = s.fence_ns;
            w.generate_box(0, 0, 0, 4, 1, 4, b, b, false);
            w.generate_box(0, 2, 0, 4, 5, 4, s.air, s.air, false);
            w.generate_box(4, 2, 0, 4, 5, 4, b, b, false);
            w.generate_box(4, 3, 1, 4, 4, 1, ns, ns, false);
            w.generate_box(4, 3, 3, 4, 4, 3, ns, ns, false);
            w.generate_box(0, 2, 0, 0, 5, 0, b, b, false);
            w.generate_box(0, 2, 4, 3, 5, 4, b, b, false);
            w.generate_box(1, 3, 4, 1, 4, 4, we, we, false);
            w.generate_box(3, 3, 4, 3, 4, 4, we, we, false);
            if piece.chest_pending && sp_contains(&clip, w.world_pos(3, 2, 3)) {
                piece.chest_pending = false;
                w.create_chest(rng, 3, 2, 3, FORTRESS_LOOT_TABLE);
            }
            w.generate_box(0, 6, 0, 4, 6, 4, b, b, false);
            for i in 0..=4 {
                for j in 0..=4 {
                    w.fill_column_down(b, i, -1, j);
                }
            }
        }
        FortressKind::CastleSmallCorridorRightTurn => {
            let b = s.bricks;
            let we = s.fence_we;
            let ns = s.fence_ns;
            w.generate_box(0, 0, 0, 4, 1, 4, b, b, false);
            w.generate_box(0, 2, 0, 4, 5, 4, s.air, s.air, false);
            w.generate_box(0, 2, 0, 0, 5, 4, b, b, false);
            w.generate_box(0, 3, 1, 0, 4, 1, ns, ns, false);
            w.generate_box(0, 3, 3, 0, 4, 3, ns, ns, false);
            w.generate_box(4, 2, 0, 4, 5, 0, b, b, false);
            w.generate_box(1, 2, 4, 4, 5, 4, b, b, false);
            w.generate_box(1, 3, 4, 1, 4, 4, we, we, false);
            w.generate_box(3, 3, 4, 3, 4, 4, we, we, false);
            if piece.chest_pending && sp_contains(&clip, w.world_pos(1, 2, 3)) {
                piece.chest_pending = false;
                w.create_chest(rng, 1, 2, 3, FORTRESS_LOOT_TABLE);
            }
            w.generate_box(0, 6, 0, 4, 6, 4, b, b, false);
            for i in 0..=4 {
                for j in 0..=4 {
                    w.fill_column_down(b, i, -1, j);
                }
            }
        }
        FortressKind::CastleSmallCorridor => {
            let b = s.bricks;
            let ns = s.fence_ns;
            w.generate_box(0, 0, 0, 4, 1, 4, b, b, false);
            w.generate_box(0, 2, 0, 4, 5, 4, s.air, s.air, false);
            w.generate_box(0, 2, 0, 0, 5, 4, b, b, false);
            w.generate_box(4, 2, 0, 4, 5, 4, b, b, false);
            w.generate_box(0, 3, 1, 0, 4, 1, ns, ns, false);
            w.generate_box(0, 3, 3, 0, 4, 3, ns, ns, false);
            w.generate_box(4, 3, 1, 4, 4, 1, ns, ns, false);
            w.generate_box(4, 3, 3, 4, 4, 3, ns, ns, false);
            w.generate_box(0, 6, 0, 4, 6, 4, b, b, false);
            for i in 0..=4 {
                for j in 0..=4 {
                    w.fill_column_down(b, i, -1, j);
                }
            }
        }
        FortressKind::CastleStalkRoom => fortress_stalk_room(w, s),
        FortressKind::MonsterThrone => fortress_monster_throne(w, s, piece),
        FortressKind::RoomCrossing => {
            let b = s.bricks;
            w.generate_box(0, 0, 0, 6, 1, 6, b, b, false);
            w.generate_box(0, 2, 0, 6, 7, 6, s.air, s.air, false);
            w.generate_box(0, 2, 0, 1, 6, 0, b, b, false);
            w.generate_box(0, 2, 6, 1, 6, 6, b, b, false);
            w.generate_box(5, 2, 0, 6, 6, 0, b, b, false);
            w.generate_box(5, 2, 6, 6, 6, 6, b, b, false);
            w.generate_box(0, 2, 0, 0, 6, 1, b, b, false);
            w.generate_box(0, 2, 5, 0, 6, 6, b, b, false);
            w.generate_box(6, 2, 0, 6, 6, 1, b, b, false);
            w.generate_box(6, 2, 5, 6, 6, 6, b, b, false);
            let we = s.fence_we;
            let ns = s.fence_ns;
            w.generate_box(2, 6, 0, 4, 6, 0, b, b, false);
            w.generate_box(2, 5, 0, 4, 5, 0, we, we, false);
            w.generate_box(2, 6, 6, 4, 6, 6, b, b, false);
            w.generate_box(2, 5, 6, 4, 5, 6, we, we, false);
            w.generate_box(0, 6, 2, 0, 6, 4, b, b, false);
            w.generate_box(0, 5, 2, 0, 5, 4, ns, ns, false);
            w.generate_box(6, 6, 2, 6, 6, 4, b, b, false);
            w.generate_box(6, 5, 2, 6, 5, 4, ns, ns, false);
            for i in 0..=6 {
                for j in 0..=6 {
                    w.fill_column_down(b, i, -1, j);
                }
            }
        }
        FortressKind::StairsRoom => {
            let b = s.bricks;
            w.generate_box(0, 0, 0, 6, 1, 6, b, b, false);
            w.generate_box(0, 2, 0, 6, 10, 6, s.air, s.air, false);
            w.generate_box(0, 2, 0, 1, 8, 0, b, b, false);
            w.generate_box(5, 2, 0, 6, 8, 0, b, b, false);
            w.generate_box(0, 2, 1, 0, 8, 6, b, b, false);
            w.generate_box(6, 2, 1, 6, 8, 6, b, b, false);
            w.generate_box(1, 2, 6, 5, 8, 6, b, b, false);
            let we = s.fence_we;
            let ns = s.fence_ns;
            w.generate_box(0, 3, 2, 0, 5, 4, ns, ns, false);
            w.generate_box(6, 3, 2, 6, 5, 2, ns, ns, false);
            w.generate_box(6, 3, 4, 6, 5, 4, ns, ns, false);
            w.place(b, 5, 2, 5);
            w.generate_box(4, 2, 5, 4, 3, 5, b, b, false);
            w.generate_box(3, 2, 5, 3, 4, 5, b, b, false);
            w.generate_box(2, 2, 5, 2, 5, 5, b, b, false);
            w.generate_box(1, 2, 5, 1, 6, 5, b, b, false);
            w.generate_box(1, 7, 1, 5, 7, 4, b, b, false);
            w.generate_box(6, 8, 2, 6, 8, 4, s.air, s.air, false);
            w.generate_box(2, 6, 0, 4, 8, 0, b, b, false);
            w.generate_box(2, 5, 0, 4, 5, 0, we, we, false);
            for i in 0..=6 {
                for j in 0..=6 {
                    w.fill_column_down(b, i, -1, j);
                }
            }
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn fortress_monster_throne(
    w: &mut PieceWorld<'_, '_>,
    s: &FortressStates,
    piece: &mut FortressPiece,
) {
    let b = s.bricks;
    let we = s.fence_we;
    let ns = s.fence_ns;
    w.generate_box(0, 2, 0, 6, 7, 7, s.air, s.air, false);
    w.generate_box(1, 0, 0, 5, 1, 7, b, b, false);
    w.generate_box(1, 2, 1, 5, 2, 7, b, b, false);
    w.generate_box(1, 3, 2, 5, 3, 7, b, b, false);
    w.generate_box(1, 4, 3, 5, 4, 7, b, b, false);
    w.generate_box(1, 2, 0, 1, 4, 2, b, b, false);
    w.generate_box(5, 2, 0, 5, 4, 2, b, b, false);
    w.generate_box(1, 5, 2, 1, 5, 3, b, b, false);
    w.generate_box(5, 5, 2, 5, 5, 3, b, b, false);
    w.generate_box(0, 5, 3, 0, 5, 8, b, b, false);
    w.generate_box(6, 5, 3, 6, 5, 8, b, b, false);
    w.generate_box(1, 5, 8, 5, 5, 8, b, b, false);
    w.place(s.fence_w, 1, 6, 3);
    w.place(s.fence_e, 5, 6, 3);
    w.place(s.fence_ne, 0, 6, 3);
    w.place(s.fence_nw, 6, 6, 3);
    w.generate_box(0, 6, 4, 0, 6, 7, ns, ns, false);
    w.generate_box(6, 6, 4, 6, 6, 7, ns, ns, false);
    w.place(s.fence_se, 0, 6, 8);
    w.place(s.fence_sw, 6, 6, 8);
    w.generate_box(1, 6, 8, 5, 6, 8, we, we, false);
    w.place(s.fence_e, 1, 7, 8);
    w.generate_box(2, 7, 8, 4, 7, 8, we, we, false);
    w.place(s.fence_w, 5, 7, 8);
    w.place(s.fence_e, 2, 8, 8);
    w.place(we, 3, 8, 8);
    w.place(s.fence_w, 4, 8, 8);
    if !piece.spawner_placed {
        let p = w.world_pos(3, 5, 5);
        if sp_contains(&w.clip, p) {
            piece.spawner_placed = true;
            w.set_world(p, s.spawner);
            w.ctx.payloads.push((
                p,
                crate::generation_payload::GenerationPayload::SpawnerEntity(
                    "minecraft:blaze".to_owned(),
                ),
            ));
        }
    }
    for i in 0..=6 {
        for j in 0..=6 {
            w.fill_column_down(b, i, -1, j);
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn fortress_stalk_room(w: &mut PieceWorld<'_, '_>, s: &FortressStates) {
    let b = s.bricks;
    let we = s.fence_we;
    let ns = s.fence_ns;
    let ns_w = s.fence_ns_w;
    let ns_e = s.fence_ns_e;
    w.generate_box(0, 3, 0, 12, 4, 12, b, b, false);
    w.generate_box(0, 5, 0, 12, 13, 12, s.air, s.air, false);
    w.generate_box(0, 5, 0, 1, 12, 12, b, b, false);
    w.generate_box(11, 5, 0, 12, 12, 12, b, b, false);
    w.generate_box(2, 5, 11, 4, 12, 12, b, b, false);
    w.generate_box(8, 5, 11, 10, 12, 12, b, b, false);
    w.generate_box(5, 9, 11, 7, 12, 12, b, b, false);
    w.generate_box(2, 5, 0, 4, 12, 1, b, b, false);
    w.generate_box(8, 5, 0, 10, 12, 1, b, b, false);
    w.generate_box(5, 9, 0, 7, 12, 1, b, b, false);
    w.generate_box(2, 11, 2, 10, 12, 10, b, b, false);
    let mut i = 1;
    while i <= 11 {
        w.generate_box(i, 10, 0, i, 11, 0, we, we, false);
        w.generate_box(i, 10, 12, i, 11, 12, we, we, false);
        w.generate_box(0, 10, i, 0, 11, i, ns, ns, false);
        w.generate_box(12, 10, i, 12, 11, i, ns, ns, false);
        w.place(b, i, 13, 0);
        w.place(b, i, 13, 12);
        w.place(b, 0, 13, i);
        w.place(b, 12, 13, i);
        if i != 11 {
            w.place(we, i + 1, 13, 0);
            w.place(we, i + 1, 13, 12);
            w.place(ns, 0, 13, i + 1);
            w.place(ns, 12, 13, i + 1);
        }
        i += 2;
    }
    w.place(s.fence_ne, 0, 13, 0);
    w.place(s.fence_se, 0, 13, 12);
    w.place(s.fence_sw, 12, 13, 12);
    w.place(s.fence_nw, 12, 13, 0);
    let mut k = 3;
    while k <= 9 {
        w.generate_box(1, 7, k, 1, 8, k, ns_w, ns_w, false);
        w.generate_box(11, 7, k, 11, 8, k, ns_e, ns_e, false);
        k += 2;
    }
    let stair_n = s.stairs_n;
    for i in 0..=6 {
        let z = i + 4;
        for x in 5..=7 {
            w.place(stair_n, x, 5 + i, z);
        }
        if (5..=8).contains(&z) {
            w.generate_box(5, 5, z, 7, i + 4, z, b, b, false);
        } else if (9..=10).contains(&z) {
            w.generate_box(5, 8, z, 7, i + 4, z, b, b, false);
        }
        if i >= 1 {
            w.generate_box(5, 6 + i, z, 7, 9 + i, z, s.air, s.air, false);
        }
    }
    for x in 5..=7 {
        w.place(stair_n, x, 12, 11);
    }
    w.generate_box(5, 6, 7, 5, 7, 7, ns_e, ns_e, false);
    w.generate_box(7, 6, 7, 7, 7, 7, ns_w, ns_w, false);
    w.generate_box(5, 13, 12, 7, 13, 12, s.air, s.air, false);
    w.generate_box(2, 5, 2, 3, 5, 3, b, b, false);
    w.generate_box(2, 5, 9, 3, 5, 10, b, b, false);
    w.generate_box(2, 5, 4, 2, 5, 8, b, b, false);
    w.generate_box(9, 5, 2, 10, 5, 3, b, b, false);
    w.generate_box(9, 5, 9, 10, 5, 10, b, b, false);
    w.generate_box(10, 5, 4, 10, 5, 8, b, b, false);
    let stair_e = s.stairs_e;
    let stair_w = s.stairs_w;
    w.place(stair_w, 4, 5, 2);
    w.place(stair_w, 4, 5, 3);
    w.place(stair_w, 4, 5, 9);
    w.place(stair_w, 4, 5, 10);
    w.place(stair_e, 8, 5, 2);
    w.place(stair_e, 8, 5, 3);
    w.place(stair_e, 8, 5, 9);
    w.place(stair_e, 8, 5, 10);
    w.generate_box(3, 4, 4, 4, 4, 8, s.soul_sand, s.soul_sand, false);
    w.generate_box(8, 4, 4, 9, 4, 8, s.soul_sand, s.soul_sand, false);
    w.generate_box(3, 5, 4, 4, 5, 8, s.nether_wart, s.nether_wart, false);
    w.generate_box(8, 5, 4, 9, 5, 8, s.nether_wart, s.nether_wart, false);
    w.generate_box(4, 2, 0, 8, 2, 12, b, b, false);
    w.generate_box(0, 2, 4, 12, 2, 8, b, b, false);
    w.generate_box(4, 0, 0, 8, 1, 3, b, b, false);
    w.generate_box(4, 0, 9, 8, 1, 12, b, b, false);
    w.generate_box(0, 0, 4, 3, 1, 8, b, b, false);
    w.generate_box(9, 0, 4, 12, 1, 8, b, b, false);
    for i in 4..=8 {
        for j in 0..=2 {
            w.fill_column_down(b, i, -1, j);
            w.fill_column_down(b, i, -1, 12 - j);
        }
    }
    for i in 0..=2 {
        for j in 4..=8 {
            w.fill_column_down(b, i, -1, j);
            w.fill_column_down(b, 12 - i, -1, j);
        }
    }
}

/// Live container map for the pieces of one structure, keyed by world position.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) type LootMap = std::collections::BTreeMap<(i32, i32, i32), SinglePieceLoot>;

/// Place one fortress chunk: every piece whose box meets `clip`, in generation order.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn place_fortress_chunk<R: StructureRng + ?Sized>(
    ctx: &mut FC<'_>,
    reg: &'static BReg,
    pieces: &mut [FortressPiece],
    rng: &mut R,
    clip: Bounds,
    loots: &mut LootMap,
) {
    let states = FortressStates::new(reg);
    for piece in pieces.iter_mut() {
        if !fm_bb_intersects(&piece.bounds, &clip) {
            continue;
        }
        let mut w = PieceWorld::new(ctx, reg, clip, piece.bounds, Some(piece.orient), loots);
        fortress_post_process(&mut w, &states, piece, rng, clip);
    }
}

#[cfg(test)]
mod fortress_monument_tests {
    use super::*;
    use crate::generation_math::sha256;

    /// The scripted world the oracle serves: horizontal bands of stone / water plus an
    /// optional single ice layer, everything else air.
    #[derive(Clone, Copy, Debug)]
    struct Scene {
        stone: Option<(i32, i32)>,
        water: Option<(i32, i32)>,
        ice: Option<i32>,
    }

    impl Scene {
        fn parse(spec: &str) -> Self {
            let mut s = Scene {
                stone: None,
                water: None,
                ice: None,
            };
            for part in spec.split(',') {
                if part.is_empty() {
                    continue;
                }
                let (key, value) = part.split_once('=').unwrap_or((part, ""));
                match key {
                    "stone" | "water" => {
                        let (lo, hi) = value.split_once(':').expect("band lo:hi");
                        let band = (lo.parse().unwrap(), hi.parse().unwrap());
                        if key == "stone" {
                            s.stone = Some(band);
                        } else {
                            s.water = Some(band);
                        }
                    }
                    "ice" => s.ice = Some(value.parse().unwrap()),
                    other => panic!("unknown scene part {other}"),
                }
            }
            s
        }

        fn state_at(&self, reg: &BReg, y: i32) -> u32 {
            if Some(y) == self.ice {
                return fm_state(reg, "minecraft:ice");
            }
            if let Some((lo, hi)) = self.stone {
                if y >= lo && y <= hi {
                    return fm_state(reg, "minecraft:stone");
                }
            }
            if let Some((lo, hi)) = self.water {
                if y >= lo && y <= hi {
                    return fm_state(reg, "minecraft:water");
                }
            }
            0
        }

        fn bands(&self) -> (i32, i32) {
            let mut lo = i32::MAX;
            let mut hi = i32::MIN;
            for band in [self.stone, self.water].into_iter().flatten() {
                lo = lo.min(band.0);
                hi = hi.max(band.1);
            }
            if let Some(y) = self.ice {
                lo = lo.min(y);
                hi = hi.max(y);
            }
            (lo, hi)
        }
    }

    fn parse_box(s: &str) -> Bounds {
        let nums: Vec<i32> = s.split(',').map(|p| p.trim().parse().unwrap()).collect();
        assert_eq!(nums.len(), 6, "bad box {s}");
        [nums[0], nums[1], nums[2], nums[3], nums[4], nums[5]]
    }

    fn parse_boxes(s: &str) -> Vec<Bounds> {
        s.split(';').map(parse_box).collect()
    }

    fn fmt_box(b: &Bounds) -> String {
        b.iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join(",")
    }

    /// Serve the scene inside the volume the clips can read.
    fn populate(ctx: &mut FC<'_>, reg: &BReg, scene: &Scene, clips: &[Bounds]) {
        let (band_lo, band_hi) = scene.bands();
        if band_lo > band_hi {
            return;
        }
        let mut lo = [i32::MAX; 3];
        let mut hi = [i32::MIN; 3];
        for c in clips {
            for i in 0..3 {
                lo[i] = lo[i].min(c[i]);
                hi[i] = hi[i].max(c[i + 3]);
            }
        }
        let y0 = band_lo.max(lo[1]);
        let y1 = band_hi.min(hi[1]);
        for y in y0..=y1 {
            for x in lo[0]..=hi[0] {
                for z in lo[2]..=hi[2] {
                    let st = scene.state_at(reg, y);
                    if st != 0 {
                        ctx.test_base_cell((x, y, z), st);
                    }
                }
            }
        }
    }

    fn sha_writes(reg: &BReg, wr: &std::collections::HashMap<(i32, i32, i32), u32>) -> String {
        let mut keys: Vec<(i32, i32, i32)> = wr.keys().copied().collect();
        keys.sort();
        let mut buf = Vec::new();
        for &(x, y, z) in &keys {
            buf.extend_from_slice(
                format!("{x},{y},{z} {}\n", reg.canonical_state(wr[&(x, y, z)])).as_bytes(),
            );
        }
        let digest = sha256(&buf);
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        format!("{}:{hex}", keys.len())
    }

    fn fmt_loots(loots: &LootMap) -> String {
        if loots.is_empty() {
            return "-".to_string();
        }
        loots
            .values()
            .map(|l| {
                format!(
                    "{},{},{}|chest|{}|{}",
                    l.pos.0, l.pos.1, l.pos.2, l.table, l.seed
                )
            })
            .collect::<Vec<_>>()
            .join(";")
    }

    #[test]
    fn fortress_and_monument_match_all_1211_native_oracle_vectors() {
        let reg = breg().expect("block registry initialized");
        let data = include_str!("../tests/data/fortress_monument_1211.tsv");
        let mut fortress_rows = 0usize;
        let mut monument_rows = 0usize;
        let mut failures = 0usize;

        for line in data.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let f: Vec<&str> = line.split('\t').collect();
            match f[0] {
                "fortress" => {
                    assert_eq!(f.len(), 15, "bad fortress row: {line}");
                    failures += run_fortress_row(&reg, &f);
                    fortress_rows += 1;
                }
                "monument" => {
                    assert_eq!(f.len(), 16, "bad monument row: {line}");
                    failures += run_monument_row(&reg, &f);
                    monument_rows += 1;
                }
                other => panic!("unknown structure kind {other}"),
            }
        }
        assert_eq!(fortress_rows, 7);
        assert_eq!(monument_rows, 7);
        assert_eq!(failures, 0, "{failures} oracle case(s) failed");
    }

    #[test]
    fn orient_box_matches_the_native_direction_table() {
        // `BoundingBox.orientBox(10, 20, 30, -1, -2, 3, 5, 7, 9, dir)`.
        let cases = [
            (Facing::North, [9, 18, 25, 13, 24, 33]),
            (Facing::South, [9, 18, 33, 13, 24, 41]),
            (Facing::West, [5, 18, 29, 13, 24, 33]),
            (Facing::East, [13, 18, 29, 21, 24, 33]),
        ];
        for (dir, want) in cases {
            assert_eq!(fm_orient_box(10, 20, 30, -1, -2, 3, 5, 7, 9, dir), want, "{dir:?}");
        }
        // `StructurePiece.makeBoundingBox` puts the depth along whichever axis the
        // direction runs on.
        assert_eq!(
            fm_make_bounding_box(0, 64, 0, Facing::North, 7, 7, 9),
            [0, 64, 0, 6, 70, 8]
        );
        assert_eq!(
            fm_make_bounding_box(0, 64, 0, Facing::West, 7, 7, 9),
            [0, 64, 0, 8, 70, 6]
        );
    }

    #[test]
    fn fortress_weight_table_matches_the_declared_pieces() {
        // `BRIDGE_PIECE_WEIGHTS` then `CASTLE_PIECE_WEIGHTS`, in declaration order.
        let want = [
            (FortressKind::BridgeStraight, 30, 0, true),
            (FortressKind::BridgeCrossing, 10, 4, false),
            (FortressKind::RoomCrossing, 10, 4, false),
            (FortressKind::StairsRoom, 10, 3, false),
            (FortressKind::MonsterThrone, 5, 2, false),
            (FortressKind::CastleEntrance, 5, 1, false),
            (FortressKind::CastleSmallCorridor, 25, 0, true),
            (FortressKind::CastleSmallCorridorCrossing, 15, 5, false),
            (FortressKind::CastleSmallCorridorRightTurn, 5, 10, false),
            (FortressKind::CastleSmallCorridorLeftTurn, 5, 10, false),
            (FortressKind::CastleCorridorStairs, 10, 3, true),
            (FortressKind::CastleCorridorTBalcony, 7, 2, false),
            (FortressKind::CastleStalkRoom, 5, 2, false),
        ];
        for (i, (kind, weight, max_place, allow_in_row)) in want.into_iter().enumerate() {
            let w = FORTRESS_WEIGHTS[i];
            assert_eq!(
                (w.kind, w.weight, w.max_place_count, w.allow_in_row),
                (kind, weight, max_place, allow_in_row),
                "weight {i}"
            );
        }
        assert_eq!(FORTRESS_WEIGHTS.len(), BRIDGE_WEIGHT_IDS.len() + CASTLE_WEIGHT_IDS.len());
    }

    #[test]
    fn monument_room_grid_decomposes_the_way_make_bounding_box_reads_it() {
        // `getRoomIndex(x, y, z) = y * 25 + z * 5 + x`, read back as index%5, index/5%5, index/25.
        for y in 0..3 {
            for z in 0..5 {
                for x in 0..5 {
                    assert_eq!(room_index(x, y, z), (y * 25 + z * 5 + x) as usize);
                }
            }
        }
        assert_eq!(room_index(2, 0, 0), 2);
        assert_eq!(room_index(2, 2, 0), 52);
        assert_eq!(room_index(0, 1, 0), 25);
        assert_eq!(room_index(4, 1, 0), 29);
        // A room's box is one grid cell of 8x4x8 per size unit, offset by its grid position.
        let a = monument_piece_box(room_index(1, 1, 2) as i32, Facing::North, 1, 1, 1);
        let b = monument_piece_box(room_index(2, 1, 2) as i32, Facing::North, 1, 1, 1);
        assert_eq!((b[0] - a[0], b[1] - a[1], b[2] - a[2]), (8, 0, 0));
        let c = monument_piece_box(room_index(1, 2, 2) as i32, Facing::North, 1, 1, 1);
        assert_eq!(c[1] - a[1], 4);
    }

    fn run_fortress_row(reg: &'static BReg, f: &[&str]) -> usize {
        let case = f[1];
        let seed: i64 = f[2].parse().unwrap();
        let cx: i32 = f[3].parse().unwrap();
        let cz: i32 = f[4].parse().unwrap();
        let orient = f[5];
        let clips = parse_boxes(f[6]);
        let scene_spec = f[7];
        let want_bounds = parse_box(f[8]);
        let want_pieces: usize = f[9].parse().unwrap();
        let want_piece_col = f[10];
        let want_writes = f[11];
        let want_rng_gen: i64 = f[12].parse().unwrap();
        let want_rng_place: i64 = f[13].parse().unwrap();
        let want_loots = f[14];

        let mut gen_rng = Xor::seed_worldgen((seed as u64) ^ 0x9E37_79B9_7F4A_7C15);
        let mut layout = generate_fortress_pieces(&mut gen_rng, cx, cz);
        let got_rng_gen = gen_rng.next_i64();

        let got_piece_col = layout
            .pieces
            .iter()
            .map(|p| p.canonical())
            .collect::<Vec<_>>()
            .join("|");

        let scene = Scene::parse(scene_spec);
        let mut gen = crate::worldgen::Gen::load(seed as u64, crate::name_to_state).unwrap();
        let mut ctx = FC::new(&mut gen);
        ctx.test_no_terrain = true;
        ctx.test_default_air = true;
        populate(&mut ctx, reg, &scene, &clips);

        let mut post_rng = Xor::seed_worldgen((seed as u64) ^ 0x2B99_2DDF_A232_49D6);
        let mut loots = LootMap::new();
        for clip in &clips {
            place_fortress_chunk(&mut ctx, reg, &mut layout.pieces, &mut post_rng, *clip, &mut loots);
        }
        let got_rng_place = post_rng.next_i64();
        let got_writes = sha_writes(reg, &ctx.wr);
        let got_loots = fmt_loots(&loots);

        let mut ok = true;
        let note = |what: &str, got: &str, want: &str, ok: &mut bool| {
            if got != want {
                eprintln!("FAIL {case} {what}: got={got} want={want}");
                *ok = false;
            }
        };
        note(
            "pieces",
            &format!("{}:{got_piece_col}", layout.pieces.len()),
            &format!("{want_pieces}:{want_piece_col}"),
            &mut ok,
        );
        note("bounds", &fmt_box(&layout.bounds), &fmt_box(&want_bounds), &mut ok);
        note("orient", facing_name(layout.pieces[0].orient), orient, &mut ok);
        // `moveInsideHeights(random, 48, 70)` shifts the lowest piece to
        // `48 + nextInt(23 - ySpan)` when that span leaves room, and to 48 otherwise.
        assert!(
            (48..=61).contains(&layout.bounds[1]),
            "{case}: moveInsideHeights must anchor the fortress at 48..=61, got {:?}",
            layout.bounds
        );
        note("rng_gen", &got_rng_gen.to_string(), &want_rng_gen.to_string(), &mut ok);
        note("writes", &got_writes, want_writes, &mut ok);
        note("rng_place", &got_rng_place.to_string(), &want_rng_place.to_string(), &mut ok);
        note("loots", &got_loots, want_loots, &mut ok);
        usize::from(!ok)
    }

    /// The monument's room graph reachable from the source room, `index,openMask,claimed`
    /// in index order - the same column the native side reads off its own `RoomDefinition`s.
    fn room_graph_column(layout: &MonumentLayout) -> String {
        let mut seen: std::collections::BTreeMap<i32, u32> = std::collections::BTreeMap::new();
        let mut queue: std::collections::VecDeque<u32> = std::collections::VecDeque::new();
        let root = layout.source_room;
        seen.insert(layout.rooms[root as usize].index, root);
        queue.push_back(root);
        while let Some(id) = queue.pop_front() {
            for d in 0..6 {
                if let Some(n) = layout.rooms[id as usize].connections[d] {
                    let idx = layout.rooms[n as usize].index;
                    if seen.insert(idx, n).is_none() {
                        queue.push_back(n);
                    }
                }
            }
        }
        seen.iter()
            .map(|(idx, id)| {
                let r = &layout.rooms[*id as usize];
                let mut mask = 0u32;
                for d in 0..6 {
                    if r.has_opening[d] {
                        mask |= 1 << d;
                    }
                }
                format!("{idx},{mask},{}", u8::from(r.claimed))
            })
            .collect::<Vec<_>>()
            .join("|")
    }

    fn run_monument_row(reg: &'static BReg, f: &[&str]) -> usize {
        let case = f[1];
        let seed: i64 = f[2].parse().unwrap();
        let cx: i32 = f[3].parse().unwrap();
        let cz: i32 = f[4].parse().unwrap();
        let orient = f[5];
        let sea_level: i32 = f[6].parse().unwrap();
        let clips = parse_boxes(f[7]);
        let scene_spec = f[8];
        let want_pieces: usize = f[9].parse().unwrap();
        let want_piece_col = f[10];
        let want_writes = f[11];
        let want_rng_gen: i64 = f[12].parse().unwrap();
        let want_rng_place: i64 = f[13].parse().unwrap();
        let want_elders = f[14];
        let want_graph = f[15];

        let mut gen_rng = Xor::seed_worldgen((seed as u64) ^ 0x9E37_79B9_7F4A_7C15);
        let mut layout = generate_monument_pieces(&mut gen_rng, cx, cz);
        let got_rng_gen = gen_rng.next_i64();

        let got_piece_col = layout
            .pieces
            .iter()
            .map(|p| p.canonical(&layout.rooms))
            .collect::<Vec<_>>()
            .join("|");

        let scene = Scene::parse(scene_spec);
        let mut gen = crate::worldgen::Gen::load(seed as u64, crate::name_to_state).unwrap();
        let mut ctx = FC::new(&mut gen);
        ctx.test_no_terrain = true;
        ctx.test_default_air = true;
        populate(&mut ctx, reg, &scene, &clips);

        let mut post_rng = Xor::seed_worldgen((seed as u64) ^ 0x2B99_2DDF_A232_49D6);
        let mut loots = LootMap::new();
        let mut elders: Vec<((i32, i32, i32), &'static str)> = Vec::new();
        for clip in &clips {
            place_monument_chunk(
                &mut ctx,
                reg,
                &mut layout,
                &mut post_rng,
                *clip,
                sea_level,
                &mut loots,
                &mut elders,
            );
        }
        let got_rng_place = post_rng.next_i64();
        let got_writes = sha_writes(reg, &ctx.wr);
        let mut elder_col: Vec<String> = elders
            .iter()
            .map(|(p, name)| format!("{name}@{},{},{}", p.0, p.1, p.2))
            .collect();
        elder_col.sort();
        let got_elders = if elder_col.is_empty() {
            "-".to_string()
        } else {
            elder_col.join(";")
        };

        let mut ok = true;
        let note = |what: &str, got: &str, want: &str, ok: &mut bool| {
            if got != want {
                eprintln!("FAIL {case} {what}: got={got} want={want}");
                *ok = false;
            }
        };
        note(
            "pieces",
            &format!("{}:{got_piece_col}", layout.pieces.len()),
            &format!("{want_pieces}:{want_piece_col}"),
            &mut ok,
        );
        note("orient", facing_name(layout.pieces[0].orient), orient, &mut ok);
        note("rng_gen", &got_rng_gen.to_string(), &want_rng_gen.to_string(), &mut ok);
        note("writes", &got_writes, want_writes, &mut ok);
        note("rng_place", &got_rng_place.to_string(), &want_rng_place.to_string(), &mut ok);
        note("elders", &got_elders, want_elders, &mut ok);
        note("graph", &room_graph_column(&layout), want_graph, &mut ok);
        assert!(
            layout.rooms[layout.core_room as usize].claimed,
            "{case}: the core room must be claimed"
        );
        usize::from(!ok)
    }
}

// ================= minecraft:monument =================

/// `Direction.values()` order: the 3D data value indexes `connections` and `hasOpening`.
#[cfg_attr(not(test), allow(dead_code))]
const DIR_DOWN: usize = 0;
#[cfg_attr(not(test), allow(dead_code))]
const DIR_UP: usize = 1;
#[cfg_attr(not(test), allow(dead_code))]
const DIR_NORTH: usize = 2;
#[cfg_attr(not(test), allow(dead_code))]
const DIR_SOUTH: usize = 3;
#[cfg_attr(not(test), allow(dead_code))]
const DIR_WEST: usize = 4;
#[cfg_attr(not(test), allow(dead_code))]
const DIR_EAST: usize = 5;

#[cfg_attr(not(test), allow(dead_code))]
const DIR_STEP: [(i32, i32, i32); 6] = [
    (0, -1, 0),
    (0, 1, 0),
    (0, 0, -1),
    (0, 0, 1),
    (-1, 0, 0),
    (1, 0, 0),
];

#[cfg_attr(not(test), allow(dead_code))]
fn dir_opposite(d: usize) -> usize {
    [DIR_UP, DIR_DOWN, DIR_SOUTH, DIR_NORTH, DIR_EAST, DIR_WEST][d]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) enum MonumentKind {
    Building,
    Core,
    DoubleX,
    DoubleXY,
    DoubleY,
    DoubleYZ,
    DoubleZ,
    Entry,
    Penthouse,
    Simple,
    SimpleTop,
    Wing,
}

#[cfg_attr(not(test), allow(dead_code))]
impl MonumentKind {
    /// The 1.21.1 `StructurePieceType` registry id, which is what the piece tag carries.
    pub(crate) fn registry_id(self) -> &'static str {
        match self {
            Self::Building => "minecraft:omb",
            Self::Core => "minecraft:omcr",
            Self::DoubleX => "minecraft:omdxr",
            Self::DoubleXY => "minecraft:omdxyr",
            Self::DoubleY => "minecraft:omdyr",
            Self::DoubleYZ => "minecraft:omdyzr",
            Self::DoubleZ => "minecraft:omdzr",
            Self::Entry => "minecraft:omentry",
            Self::Penthouse => "minecraft:ompenthouse",
            Self::Simple => "minecraft:omsimple",
            Self::SimpleTop => "minecraft:omsimplet",
            Self::Wing => "minecraft:omwr",
        }
    }
}

/// `OceanMonumentPieces.RoomDefinition`.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct RoomDefinition {
    pub(crate) index: i32,
    pub(crate) connections: [Option<u32>; 6],
    pub(crate) has_opening: [bool; 6],
    pub(crate) claimed: bool,
    pub(crate) is_source: bool,
    scan_index: i32,
}

#[cfg_attr(not(test), allow(dead_code))]
impl RoomDefinition {
    fn new(index: i32) -> Self {
        RoomDefinition {
            index,
            connections: [None; 6],
            has_opening: [false; 6],
            claimed: false,
            is_source: false,
            scan_index: -1,
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn room_index(x: i32, y: i32, z: i32) -> usize {
    (y * 25 + z * 5 + x) as usize
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct MonumentPiece {
    pub(crate) kind: MonumentKind,
    pub(crate) bounds: Bounds,
    pub(crate) orient: Facing,
    pub(crate) depth: i32,
    pub(crate) room: Option<u32>,
    /// `OceanMonumentSimpleRoom.mainDesign` / `OceanMonumentWingRoom.mainDesign`.
    pub(crate) main_design: i32,
}

#[cfg_attr(not(test), allow(dead_code))]
impl MonumentPiece {
    pub(crate) fn canonical(&self, rooms: &[RoomDefinition]) -> String {
        let bb = self
            .bounds
            .iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join(",");
        // The tag records `RoomDefinition.index`, not the slot the graph stores it in.
        let room = self
            .room
            .map(|id| rooms[id as usize].index)
            .unwrap_or(-1);
        format!(
            "{},{},{},{},{}",
            self.kind.registry_id(),
            facing_data2d(self.orient),
            self.depth,
            room,
            bb
        )
    }
}

#[derive(Clone, Debug, Default)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct MonumentLayout {
    /// Index 0 is the `MonumentBuilding`; the rest are its `childPieces` in creation order.
    pub(crate) pieces: Vec<MonumentPiece>,
    pub(crate) rooms: Vec<RoomDefinition>,
    pub(crate) source_room: u32,
    pub(crate) core_room: u32,
}

#[cfg_attr(not(test), allow(dead_code))]
struct MonumentGraph {
    rooms: Vec<RoomDefinition>,
    by_index: Vec<Option<u32>>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl MonumentGraph {
    fn room(&self, id: u32) -> &RoomDefinition {
        &self.rooms[id as usize]
    }

    fn room_mut(&mut self, id: u32) -> &mut RoomDefinition {
        &mut self.rooms[id as usize]
    }

    fn push(&mut self, index: i32) -> u32 {
        let id = self.rooms.len() as u32;
        self.rooms.push(RoomDefinition::new(index));
        id
    }

    /// `RoomDefinition.setConnection`: the stored link is bidirectional.
    fn set_connection(&mut self, a: u32, dir: usize, b: u32) {
        self.room_mut(a).connections[dir] = Some(b);
        self.room_mut(b).connections[dir_opposite(dir)] = Some(a);
    }

    fn update_openings(&mut self, id: u32) {
        for d in 0..6 {
            let open = self.room(id).connections[d].is_some();
            self.room_mut(id).has_opening[d] = open;
        }
    }

    /// `RoomDefinition.findSource`: can this room reach a source through open connections?
    fn find_source(&mut self, id: u32, scan: i32) -> bool {
        if self.room(id).is_source {
            return true;
        }
        self.room_mut(id).scan_index = scan;
        for d in 0..6 {
            let Some(next) = self.room(id).connections[d] else {
                continue;
            };
            if self.room(id).has_opening[d] && self.room(next).scan_index != scan {
                if self.find_source(next, scan) {
                    return true;
                }
            }
        }
        false
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn monument_piece_box(room: i32, dir: Facing, xs: i32, ys: i32, zs: i32) -> Bounds {
    let gx = room % 5;
    let gz = room / 5 % 5;
    let gy = room / 25;
    let mut bb = fm_make_bounding_box(0, 0, 0, dir, xs * 8, ys * 4, zs * 8);
    match dir {
        Facing::North => fm_bb_move(&mut bb, gx * 8, gy * 4, -(gz + zs) * 8 + 1),
        Facing::South => fm_bb_move(&mut bb, gx * 8, gy * 4, gz * 8),
        Facing::West => fm_bb_move(&mut bb, -(gz + zs) * 8 + 1, gy * 4, gx * 8),
        Facing::East => fm_bb_move(&mut bb, gz * 8, gy * 4, gx * 8),
    }
    bb
}

/// `Util.shuffle`: Fisher-Yates from the end, one `nextInt(i + 1)` per step.
#[cfg_attr(not(test), allow(dead_code))]
fn util_shuffle<T, R: StructureRng + ?Sized>(list: &mut [T], rng: &mut R) {
    for i in (1..list.len()).rev() {
        let j = rng.next_bound((i + 1) as i32) as usize;
        list.swap(i, j);
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn monument_generate_room_graph<R: StructureRng + ?Sized>(
    rng: &mut R,
) -> (MonumentGraph, u32, u32, Vec<u32>) {
    let mut g = MonumentGraph {
        rooms: Vec::new(),
        by_index: vec![None; 75],
    };
    for x in 0..5 {
        for z in 0..4 {
            let i = room_index(x, 0, z);
            g.by_index[i] = Some(g.push(i as i32));
        }
    }
    for x in 0..5 {
        for z in 0..4 {
            let i = room_index(x, 1, z);
            g.by_index[i] = Some(g.push(i as i32));
        }
    }
    for x in 1..4 {
        for z in 0..2 {
            let i = room_index(x, 2, z);
            g.by_index[i] = Some(g.push(i as i32));
        }
    }
    let source = g.by_index[room_index(2, 0, 0)].expect("source room");

    for x in 0..5 {
        for z in 0..5 {
            for y in 0..3 {
                let idx = room_index(x, y, z);
                let Some(room) = g.by_index[idx] else {
                    continue;
                };
                for d in 0..6 {
                    let (sx, sy, sz) = DIR_STEP[d];
                    let (nx, ny, nz) = (x + sx, y + sy, z + sz);
                    if !(0..5).contains(&nx) || !(0..5).contains(&nz) || !(0..3).contains(&ny) {
                        continue;
                    }
                    let Some(other) = g.by_index[room_index(nx, ny, nz)] else {
                        continue;
                    };
                    // A neighbour along Z is stored through its opposite: the room graph keeps
                    // one entry per pair, keyed by the axis the connection leaves on.
                    if nz == z {
                        g.set_connection(room, d, other);
                    } else {
                        g.set_connection(room, dir_opposite(d), other);
                    }
                }
            }
        }
    }

    let penthouse_room = g.push(1003);
    let left_wing = g.push(1001);
    let right_wing = g.push(1002);
    let top_connect = g.by_index[room_index(2, 2, 0)].expect("top connect room");
    let left_connect = g.by_index[room_index(0, 1, 0)].expect("left wing connect room");
    let right_connect = g.by_index[room_index(4, 1, 0)].expect("right wing connect room");
    g.set_connection(top_connect, DIR_UP, penthouse_room);
    g.set_connection(left_connect, DIR_SOUTH, left_wing);
    g.set_connection(right_connect, DIR_SOUTH, right_wing);
    g.room_mut(penthouse_room).claimed = true;
    g.room_mut(left_wing).claimed = true;
    g.room_mut(right_wing).claimed = true;
    g.room_mut(source).is_source = true;
    let core_room = g.by_index[room_index(rng.next_bound(4), 0, 2)].expect("core room");
    g.room_mut(core_room).claimed = true;

    // The core's neighbourhood is claimed so the fitters leave the entrance axis alone.
    let core_east = g.room(core_room).connections[DIR_EAST].expect("core east");
    let core_north = g.room(core_room).connections[DIR_NORTH].expect("core north");
    let core_up = g.room(core_room).connections[DIR_UP].expect("core up");
    let core_east_north = g.room(core_east).connections[DIR_NORTH].expect("core east/north");
    let core_east_up = g.room(core_east).connections[DIR_UP].expect("core east/up");
    let core_north_up = g.room(core_north).connections[DIR_UP].expect("core north/up");
    let core_east_north_up = g.room(core_east_north).connections[DIR_UP].expect("core east/north/up");
    for id in [
        core_east,
        core_north,
        core_east_north,
        core_up,
        core_east_up,
        core_north_up,
        core_east_north_up,
    ] {
        g.room_mut(id).claimed = true;
    }

    let mut list: Vec<u32> = Vec::new();
    for slot in 0..75 {
        if let Some(id) = g.by_index[slot] {
            g.update_openings(id);
            list.push(id);
        }
    }
    g.update_openings(penthouse_room);
    util_shuffle(&mut list, rng);

    let mut n = 1;
    for room in list.clone() {
        let mut found = 0;
        let mut tries = 0;
        while found < 2 && tries < 5 {
            tries += 1;
            let d = rng.next_bound(6) as usize;
            if !g.room(room).has_opening[d] {
                continue;
            }
            let other = g.room(room).connections[d].expect("open connection");
            let back = dir_opposite(d);
            g.room_mut(room).has_opening[d] = false;
            g.room_mut(other).has_opening[back] = false;
            // `room.findSource(n++) && other.findSource(n++)`: the first increment always
            // happens, the second only when the first probe succeeded.
            let first = {
                let probe = n;
                n += 1;
                g.find_source(room, probe)
            };
            let second = if first {
                let probe = n;
                n += 1;
                g.find_source(other, probe)
            } else {
                false
            };
            if first && second {
                found += 1;
            } else {
                g.room_mut(room).has_opening[d] = true;
                g.room_mut(other).has_opening[back] = true;
            }
        }
    }
    list.push(penthouse_room);
    list.push(left_wing);
    list.push(right_wing);
    // `MonumentBuilding`'s constructor claims the source room before the fitters run.
    g.room_mut(source).claimed = true;
    (g, source, core_room, list)
}

/// `MonumentBuilding`'s fitter list, in the order the constructor adds them.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MonumentFitter {
    DoubleXY,
    DoubleYZ,
    DoubleZ,
    DoubleX,
    DoubleY,
    SimpleTop,
    Simple,
}

#[cfg_attr(not(test), allow(dead_code))]
fn monument_fitter_fits(g: &MonumentGraph, f: MonumentFitter, room: u32) -> bool {
    let r = g.room(room);
    match f {
        MonumentFitter::DoubleX => {
            r.has_opening[DIR_EAST] && !g.room(r.connections[DIR_EAST].unwrap()).claimed
        }
        MonumentFitter::DoubleY => {
            r.has_opening[DIR_UP] && !g.room(r.connections[DIR_UP].unwrap()).claimed
        }
        MonumentFitter::DoubleZ => {
            r.has_opening[DIR_NORTH] && !g.room(r.connections[DIR_NORTH].unwrap()).claimed
        }
        MonumentFitter::DoubleXY => {
            if !(r.has_opening[DIR_EAST]
                && !g.room(r.connections[DIR_EAST].unwrap()).claimed
                && r.has_opening[DIR_UP]
                && !g.room(r.connections[DIR_UP].unwrap()).claimed)
            {
                return false;
            }
            let east = g.room(r.connections[DIR_EAST].unwrap());
            east.has_opening[DIR_UP] && !g.room(east.connections[DIR_UP].unwrap()).claimed
        }
        MonumentFitter::DoubleYZ => {
            if !(r.has_opening[DIR_NORTH]
                && !g.room(r.connections[DIR_NORTH].unwrap()).claimed
                && r.has_opening[DIR_UP]
                && !g.room(r.connections[DIR_UP].unwrap()).claimed)
            {
                return false;
            }
            let north = g.room(r.connections[DIR_NORTH].unwrap());
            north.has_opening[DIR_UP] && !g.room(north.connections[DIR_UP].unwrap()).claimed
        }
        MonumentFitter::SimpleTop => {
            !r.has_opening[DIR_WEST]
                && !r.has_opening[DIR_EAST]
                && !r.has_opening[DIR_NORTH]
                && !r.has_opening[DIR_SOUTH]
                && !r.has_opening[DIR_UP]
        }
        MonumentFitter::Simple => true,
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn monument_fitter_create<R: StructureRng + ?Sized>(
    g: &mut MonumentGraph,
    f: MonumentFitter,
    dir: Facing,
    room: u32,
    rng: &mut R,
) -> MonumentPiece {
    let room_index = g.room(room).index;
    match f {
        MonumentFitter::DoubleX => {
            let east = g.room(room).connections[DIR_EAST].unwrap();
            g.room_mut(room).claimed = true;
            g.room_mut(east).claimed = true;
            MonumentPiece {
                kind: MonumentKind::DoubleX,
                bounds: monument_piece_box(room_index, dir, 2, 1, 1),
                orient: dir,
                depth: 1,
                room: Some(room),
                main_design: 0,
            }
        }
        MonumentFitter::DoubleY => {
            let up = g.room(room).connections[DIR_UP].unwrap();
            g.room_mut(room).claimed = true;
            g.room_mut(up).claimed = true;
            MonumentPiece {
                kind: MonumentKind::DoubleY,
                bounds: monument_piece_box(room_index, dir, 1, 2, 1),
                orient: dir,
                depth: 1,
                room: Some(room),
                main_design: 0,
            }
        }
        MonumentFitter::DoubleZ => {
            let mut target = room;
            if !g.room(room).has_opening[DIR_NORTH]
                || g.room(g.room(room).connections[DIR_NORTH].unwrap()).claimed
            {
                target = g.room(room).connections[DIR_SOUTH].unwrap();
            }
            let north = g.room(target).connections[DIR_NORTH].unwrap();
            g.room_mut(target).claimed = true;
            g.room_mut(north).claimed = true;
            MonumentPiece {
                kind: MonumentKind::DoubleZ,
                bounds: monument_piece_box(g.room(target).index, dir, 1, 1, 2),
                orient: dir,
                depth: 1,
                room: Some(target),
                main_design: 0,
            }
        }
        MonumentFitter::DoubleXY => {
            let east = g.room(room).connections[DIR_EAST].unwrap();
            let up = g.room(room).connections[DIR_UP].unwrap();
            let east_up = g.room(east).connections[DIR_UP].unwrap();
            g.room_mut(room).claimed = true;
            g.room_mut(east).claimed = true;
            g.room_mut(up).claimed = true;
            g.room_mut(east_up).claimed = true;
            MonumentPiece {
                kind: MonumentKind::DoubleXY,
                bounds: monument_piece_box(room_index, dir, 2, 2, 1),
                orient: dir,
                depth: 1,
                room: Some(room),
                main_design: 0,
            }
        }
        MonumentFitter::DoubleYZ => {
            let north = g.room(room).connections[DIR_NORTH].unwrap();
            let up = g.room(room).connections[DIR_UP].unwrap();
            let north_up = g.room(north).connections[DIR_UP].unwrap();
            g.room_mut(room).claimed = true;
            g.room_mut(north).claimed = true;
            g.room_mut(up).claimed = true;
            g.room_mut(north_up).claimed = true;
            MonumentPiece {
                kind: MonumentKind::DoubleYZ,
                bounds: monument_piece_box(room_index, dir, 1, 2, 2),
                orient: dir,
                depth: 1,
                room: Some(room),
                main_design: 0,
            }
        }
        MonumentFitter::SimpleTop => {
            g.room_mut(room).claimed = true;
            MonumentPiece {
                kind: MonumentKind::SimpleTop,
                bounds: monument_piece_box(room_index, dir, 1, 1, 1),
                orient: dir,
                depth: 1,
                room: Some(room),
                main_design: 0,
            }
        }
        MonumentFitter::Simple => {
            let design = rng.next_bound(3);
            g.room_mut(room).claimed = true;
            MonumentPiece {
                kind: MonumentKind::Simple,
                bounds: monument_piece_box(room_index, dir, 1, 1, 1),
                orient: dir,
                depth: 1,
                room: Some(room),
                main_design: design,
            }
        }
    }
}

/// `OceanMonumentStructure.createTopPiece` + `MonumentBuilding`'s constructor.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn generate_monument_pieces<R: StructureRng + FullIntRng + ?Sized>(
    rng: &mut R,
    cx: i32,
    cz: i32,
) -> MonumentLayout {
    let dir = HORIZONTAL_FACINGS[rng.next_bound(4) as usize];
    let building_bounds = fm_make_bounding_box(cx * 16 - 29, 39, cz * 16 - 29, dir, 58, 23, 58);
    let building = MonumentPiece {
        kind: MonumentKind::Building,
        bounds: building_bounds,
        orient: dir,
        depth: 0,
        room: None,
        main_design: 0,
    };
    let (mut g, source_room, core_room, order_of_rooms) = monument_generate_room_graph(rng);

    let mut pieces: Vec<MonumentPiece> = Vec::new();
    pieces.push(MonumentPiece {
        kind: MonumentKind::Entry,
        bounds: monument_piece_box(g.room(source_room).index, dir, 1, 1, 1),
        orient: dir,
        depth: 1,
        room: Some(source_room),
        main_design: 0,
    });
    pieces.push(MonumentPiece {
        kind: MonumentKind::Core,
        bounds: monument_piece_box(g.room(core_room).index, dir, 2, 2, 2),
        orient: dir,
        depth: 1,
        room: Some(core_room),
        main_design: 0,
    });

    let order = [
        MonumentFitter::DoubleXY,
        MonumentFitter::DoubleYZ,
        MonumentFitter::DoubleZ,
        MonumentFitter::DoubleX,
        MonumentFitter::DoubleY,
        MonumentFitter::SimpleTop,
        MonumentFitter::Simple,
    ];
    // The fitters run in `Util.shuffle`d room order: claiming a neighbour changes what a
    // later room can fit, so this order is part of the result, not a convenience.
    for id in order_of_rooms {
        if g.room(id).claimed || g.room(id).index >= 75 {
            continue;
        }
        for &f in &order {
            if monument_fitter_fits(&g, f, id) {
                let piece = monument_fitter_create(&mut g, f, dir, id, rng);
                pieces.push(piece);
                break;
            }
        }
    }

    // `getWorldPos(9, 0, 22)` for the building, then every child's box is moved by it.
    let shift = world_pos(&building.bounds, Some(dir), (9, 0, 22));
    for p in &mut pieces {
        fm_bb_move(&mut p.bounds, shift.0, shift.1, shift.2);
    }

    let c1 = world_pos(&building.bounds, Some(dir), (1, 1, 1));
    let c2 = world_pos(&building.bounds, Some(dir), (23, 8, 21));
    let c3 = world_pos(&building.bounds, Some(dir), (34, 1, 1));
    let c4 = world_pos(&building.bounds, Some(dir), (56, 8, 21));
    let c5 = world_pos(&building.bounds, Some(dir), (22, 13, 22));
    let c6 = world_pos(&building.bounds, Some(dir), (35, 17, 35));
    let mut wing_a = [0; 6];
    let mut wing_b = [0; 6];
    let mut penthouse = [0; 6];
    for (i, (a, b)) in [(c1, c2), (c3, c4), (c5, c6)].into_iter().enumerate() {
        let bb = [
            a.0.min(b.0),
            a.1.min(b.1),
            a.2.min(b.2),
            a.0.max(b.0),
            a.1.max(b.1),
            a.2.max(b.2),
        ];
        match i {
            0 => wing_a = bb,
            1 => wing_b = bb,
            _ => penthouse = bb,
        }
    }
    let design = rng.next_int();
    pieces.push(MonumentPiece {
        kind: MonumentKind::Wing,
        bounds: wing_a,
        orient: dir,
        depth: 1,
        room: None,
        main_design: design & 1,
    });
    pieces.push(MonumentPiece {
        kind: MonumentKind::Wing,
        bounds: wing_b,
        orient: dir,
        depth: 1,
        room: None,
        main_design: (design + 1) & 1,
    });
    pieces.push(MonumentPiece {
        kind: MonumentKind::Penthouse,
        bounds: penthouse,
        orient: dir,
        depth: 1,
        room: None,
        main_design: 0,
    });

    let mut all = Vec::with_capacity(pieces.len() + 1);
    all.push(building);
    all.extend(pieces);
    MonumentLayout {
        pieces: all,
        rooms: g.rooms,
        source_room,
        core_room,
    }
}

// ---------- monument placement ----------

#[cfg_attr(not(test), allow(dead_code))]
struct MonumentStates {
    gray: u32,
    light: u32,
    black: u32,
    lamp: u32,
    water: u32,
    air: u32,
    gold: u32,
    sponge: u32,
    ice: [u32; 2],
}

#[cfg_attr(not(test), allow(dead_code))]
impl MonumentStates {
    fn new(reg: &BReg) -> Self {
        MonumentStates {
            gray: fm_state(reg, "minecraft:prismarine"),
            light: fm_state(reg, "minecraft:prismarine_bricks"),
            black: fm_state(reg, "minecraft:dark_prismarine"),
            lamp: fm_state(reg, "minecraft:sea_lantern"),
            water: fm_state(reg, "minecraft:water"),
            air: fm_state(reg, "minecraft:air"),
            gold: fm_state(reg, "minecraft:gold_block"),
            sponge: fm_state(reg, "minecraft:wet_sponge"),
            ice: [fm_state(reg, "minecraft:ice"), fm_state(reg, "minecraft:packed_ice")],
        }
    }

    /// `FILL_KEEP`: ice, packed ice, blue ice and water are left alone by the water box.
    fn fill_keep(&self, reg: &BReg, st: u32) -> bool {
        if st == self.water || st == self.ice[0] || st == self.ice[1] {
            return true;
        }
        matches!(
            reg.name_of(st),
            "minecraft:ice" | "minecraft:packed_ice" | "minecraft:blue_ice" | "minecraft:water"
        )
    }
}

#[cfg_attr(not(test), allow(dead_code))]
impl<'a, 'g> PieceWorld<'a, 'g> {
    /// `OceanMonumentPiece.generateWaterBox`: fill with water, but above sea level replace a
    /// non-water cell with air, and never touch `FILL_KEEP`.
    #[allow(clippy::too_many_arguments)]
    fn generate_water_box(
        &mut self,
        s: &MonumentStates,
        x0: i32,
        y0: i32,
        z0: i32,
        x1: i32,
        y1: i32,
        z1: i32,
        sea_level: i32,
    ) {
        for y in y0..=y1 {
            for x in x0..=x1 {
                for z in z0..=z1 {
                    let st = self.get(x, y, z);
                    if s.fill_keep(self.reg, st) {
                        continue;
                    }
                    let wy = self.world_pos(x, y, z).1;
                    if wy >= sea_level && st != s.water {
                        self.place(s.air, x, y, z);
                    } else {
                        self.place(s.water, x, y, z);
                    }
                }
            }
        }
    }

    /// `OceanMonumentPiece.generateBoxOnFillOnly`.
    #[allow(clippy::too_many_arguments)]
    fn generate_box_on_fill(
        &mut self,
        s: &MonumentStates,
        x0: i32,
        y0: i32,
        z0: i32,
        x1: i32,
        y1: i32,
        z1: i32,
        state: u32,
    ) {
        for y in y0..=y1 {
            for x in x0..=x1 {
                for z in z0..=z1 {
                    if self.get(x, y, z) == s.water {
                        self.place(state, x, y, z);
                    }
                }
            }
        }
    }

    /// `OceanMonumentPiece.generateDefaultFloor`.
    fn generate_default_floor(
        &mut self,
        s: &MonumentStates,
        x0: i32,
        z0: i32,
        has_opening: bool,
    ) {
        if has_opening {
            let g = s.gray;
            let l = s.light;
            self.generate_box(x0, 0, z0, x0 + 2, 0, z0 + 7, g, g, false);
            self.generate_box(x0 + 5, 0, z0, x0 + 7, 0, z0 + 7, g, g, false);
            self.generate_box(x0 + 3, 0, z0, x0 + 4, 0, z0 + 2, g, g, false);
            self.generate_box(x0 + 3, 0, z0 + 5, x0 + 4, 0, z0 + 7, g, g, false);
            self.generate_box(x0 + 3, 0, z0 + 2, x0 + 4, 0, z0 + 2, l, l, false);
            self.generate_box(x0 + 3, 0, z0 + 5, x0 + 4, 0, z0 + 5, l, l, false);
            self.generate_box(x0 + 2, 0, z0 + 3, x0 + 2, 0, z0 + 4, l, l, false);
            self.generate_box(x0 + 5, 0, z0 + 3, x0 + 5, 0, z0 + 4, l, l, false);
        } else {
            let g = s.gray;
            self.generate_box(x0, 0, z0, x0 + 7, 0, z0 + 7, g, g, false);
        }
    }

    /// `OceanMonumentPiece.chunkIntersects`: the world-space rectangle of a local one.
    fn chunk_intersects(&self, clip: &Bounds, x0: i32, z0: i32, x1: i32, z1: i32) -> bool {
        let a = self.world_pos(x0, 0, z0);
        let b = self.world_pos(x1, 0, z1);
        let (min_x, max_x) = (a.0.min(b.0), a.0.max(b.0));
        let (min_z, max_z) = (a.2.min(b.2), a.2.max(b.2));
        clip[3] >= min_x && clip[0] <= max_x && clip[5] >= min_z && clip[2] <= max_z
    }
}

/// `MonumentBuilding.postProcess`, in the order the building writes.
#[cfg_attr(not(test), allow(dead_code))]
fn monument_building_body(
    w: &mut PieceWorld<'_, '_>,
    s: &MonumentStates,
    clip: Bounds,
    sea_level: i32,
) {
    let top = sea_level.max(64) - w.bounds[1];
    w.generate_water_box(s, 0, 0, 0, 58, top, 58, sea_level);
    monument_generate_wing(w, s, clip, false, 0, sea_level);
    monument_generate_wing(w, s, clip, true, 33, sea_level);
    monument_generate_entrance_archs(w, s, clip);
    monument_generate_entrance_wall(w, s, clip, sea_level);
    monument_generate_roof_piece(w, s, clip, sea_level);
    monument_generate_lower_wall(w, s, clip, sea_level);
    monument_generate_middle_wall(w, s, clip, sea_level);
    monument_generate_upper_wall(w, s, clip, sea_level);

    for x0 in 0..7 {
        let mut z = 0;
        while z < 7 {
            if z == 0 && x0 == 3 {
                z = 6;
            }
            let x = x0 * 9;
            let zz = z * 9;
            for dx in 0..4 {
                for dz in 0..4 {
                    let light = s.light;
                    w.place(light, x + dx, 0, zz + dz);
                    w.fill_column_down(light, x + dx, -1, zz + dz);
                }
            }
            if x0 != 0 && x0 != 6 {
                z += 6;
            } else {
                z += 1;
            }
        }
    }

    for k in 0..5 {
        w.generate_water_box(s, -1 - k, 0 + k * 2, -1 - k, -1 - k, 23, 58 + k, sea_level);
        w.generate_water_box(s, 58 + k, 0 + k * 2, -1 - k, 58 + k, 23, 58 + k, sea_level);
        w.generate_water_box(s, 0 - k, 0 + k * 2, -1 - k, 57 + k, 23, -1 - k, sea_level);
        w.generate_water_box(s, 0 - k, 0 + k * 2, 58 + k, 57 + k, 23, 58 + k, sea_level);
    }
}

/// The RNG surface the monument's room pieces need (`nextBoolean`), kept separate so the
/// building body can be generic over the caller's generator without threading a type.
#[cfg_attr(not(test), allow(dead_code))]
trait MonumentRng {
    fn next_bool(&mut self) -> bool;
    fn next_bound(&mut self, bound: i32) -> i32;
}

impl<R: StructureRng + ?Sized> MonumentRng for R {
    fn next_bool(&mut self) -> bool {
        StructureRng::next_bool(self)
    }
    fn next_bound(&mut self, bound: i32) -> i32 {
        StructureRng::next_bound(self, bound)
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn monument_generate_wing(
    w: &mut PieceWorld<'_, '_>,
    s: &MonumentStates,
    clip: Bounds,
    top: bool,
    off: i32,
    sea_level: i32,
) {
    if !w.chunk_intersects(&clip, off, 0, off + 23, 20) {
        return;
    }
    let g = s.gray;
    let l = s.light;
    w.generate_box(off, 0, 0, off + 24, 0, 20, g, g, false);
    w.generate_water_box(s, off, 1, 0, off + 24, 10, 20, sea_level);
    for k in 0..4 {
        w.generate_box(off + k, k + 1, k, off + k, k + 1, 20, l, l, false);
        w.generate_box(off + k + 7, k + 5, k + 7, off + k + 7, k + 5, 20, l, l, false);
        w.generate_box(
            off + 17 - k,
            k + 5,
            k + 7,
            off + 17 - k,
            k + 5,
            20,
            l,
            l,
            false,
        );
        w.generate_box(
            off + 24 - k,
            k + 1,
            k,
            off + 24 - k,
            k + 1,
            20,
            l,
            l,
            false,
        );
        w.generate_box(off + k + 1, k + 1, k, off + 23 - k, k + 1, k, l, l, false);
        w.generate_box(
            off + k + 8,
            k + 5,
            k + 7,
            off + 16 - k,
            k + 5,
            k + 7,
            l,
            l,
            false,
        );
    }
    w.generate_box(off + 4, 4, 4, off + 6, 4, 20, g, g, false);
    w.generate_box(off + 7, 4, 4, off + 17, 4, 6, g, g, false);
    w.generate_box(off + 18, 4, 4, off + 20, 4, 20, g, g, false);
    w.generate_box(off + 11, 8, 11, off + 13, 8, 20, g, g, false);
    w.place(l, off + 12, 9, 12);
    w.place(l, off + 12, 9, 15);
    w.place(l, off + 12, 9, 18);
    let a = off + if top { 19 } else { 5 };
    let b = off + if top { 5 } else { 19 };
    let mut y = 20;
    while y >= 5 {
        w.place(l, a, 5, y);
        y -= 3;
    }
    let mut y = 19;
    while y >= 7 {
        w.place(l, b, 5, y);
        y -= 3;
    }
    for k in 0..4 {
        let x = if top {
            off + 24 - (17 - k * 3)
        } else {
            off + 17 - k * 3
        };
        w.place(l, x, 5, 5);
    }
    w.place(l, b, 5, 5);
    w.generate_box(off + 11, 1, 12, off + 13, 7, 12, g, g, false);
    w.generate_box(off + 12, 1, 11, off + 12, 7, 13, g, g, false);
}

#[cfg_attr(not(test), allow(dead_code))]
fn monument_generate_entrance_archs(w: &mut PieceWorld<'_, '_>, s: &MonumentStates, clip: Bounds) {
    if !w.chunk_intersects(&clip, 22, 5, 35, 17) {
        return;
    }
    let g = s.gray;
    let l = s.light;
    let m = s.lamp;
    w.generate_water_box(s, 25, 0, 0, 32, 8, 20, 63);
    for k in 0..4 {
        let z = 5 + k * 4;
        w.generate_box(24, 2, z, 24, 4, z, l, l, false);
        w.generate_box(22, 4, z, 23, 4, z, l, l, false);
        w.place(l, 25, 5, z);
        w.place(l, 26, 6, z);
        w.place(m, 26, 5, z);
        w.generate_box(33, 2, z, 33, 4, z, l, l, false);
        w.generate_box(34, 4, z, 35, 4, z, l, l, false);
        w.place(l, 32, 5, z);
        w.place(l, 31, 6, z);
        w.place(m, 31, 5, z);
        w.generate_box(27, 6, z, 30, 6, z, g, g, false);
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn monument_generate_entrance_wall(
    w: &mut PieceWorld<'_, '_>,
    s: &MonumentStates,
    clip: Bounds,
    sea_level: i32,
) {
    if !w.chunk_intersects(&clip, 15, 20, 42, 21) {
        return;
    }
    let g = s.gray;
    let l = s.light;
    let k = s.black;
    w.generate_box(15, 0, 21, 42, 0, 21, g, g, false);
    w.generate_water_box(s, 26, 1, 21, 31, 3, 21, sea_level);
    w.generate_box(21, 12, 21, 36, 12, 21, g, g, false);
    w.generate_box(17, 11, 21, 40, 11, 21, g, g, false);
    w.generate_box(16, 10, 21, 41, 10, 21, g, g, false);
    w.generate_box(15, 7, 21, 42, 9, 21, g, g, false);
    w.generate_box(16, 6, 21, 41, 6, 21, g, g, false);
    w.generate_box(17, 5, 21, 40, 5, 21, g, g, false);
    w.generate_box(21, 4, 21, 36, 4, 21, g, g, false);
    w.generate_box(22, 3, 21, 26, 3, 21, g, g, false);
    w.generate_box(31, 3, 21, 35, 3, 21, g, g, false);
    w.generate_box(23, 2, 21, 25, 2, 21, g, g, false);
    w.generate_box(32, 2, 21, 34, 2, 21, g, g, false);
    w.generate_box(28, 4, 20, 29, 4, 21, l, l, false);
    w.place(l, 27, 3, 21);
    w.place(l, 30, 3, 21);
    w.place(l, 26, 2, 21);
    w.place(l, 31, 2, 21);
    w.place(l, 25, 1, 21);
    w.place(l, 32, 1, 21);
    for i in 0..7 {
        w.place(k, 28 - i, 6 + i, 21);
        w.place(k, 29 + i, 6 + i, 21);
    }
    for i in 0..4 {
        w.place(k, 28 - i, 9 + i, 21);
        w.place(k, 29 + i, 9 + i, 21);
    }
    w.place(k, 28, 12, 21);
    w.place(k, 29, 12, 21);
    for i in 0..3 {
        w.place(k, 22 - i * 2, 8, 21);
        w.place(k, 22 - i * 2, 9, 21);
        w.place(k, 35 + i * 2, 8, 21);
        w.place(k, 35 + i * 2, 9, 21);
    }
    w.generate_water_box(s, 15, 13, 21, 42, 15, 21, sea_level);
    w.generate_water_box(s, 15, 1, 21, 15, 6, 21, sea_level);
    w.generate_water_box(s, 16, 1, 21, 16, 5, 21, sea_level);
    w.generate_water_box(s, 17, 1, 21, 20, 4, 21, sea_level);
    w.generate_water_box(s, 21, 1, 21, 21, 3, 21, sea_level);
    w.generate_water_box(s, 22, 1, 21, 22, 2, 21, sea_level);
    w.generate_water_box(s, 23, 1, 21, 24, 1, 21, sea_level);
    w.generate_water_box(s, 42, 1, 21, 42, 6, 21, sea_level);
    w.generate_water_box(s, 41, 1, 21, 41, 5, 21, sea_level);
    w.generate_water_box(s, 37, 1, 21, 40, 4, 21, sea_level);
    w.generate_water_box(s, 36, 1, 21, 36, 3, 21, sea_level);
    w.generate_water_box(s, 33, 1, 21, 34, 1, 21, sea_level);
    w.generate_water_box(s, 35, 1, 21, 35, 2, 21, sea_level);
}

#[cfg_attr(not(test), allow(dead_code))]
fn monument_generate_roof_piece(
    w: &mut PieceWorld<'_, '_>,
    s: &MonumentStates,
    clip: Bounds,
    sea_level: i32,
) {
    if !w.chunk_intersects(&clip, 21, 21, 36, 36) {
        return;
    }
    let g = s.gray;
    let l = s.light;
    let m = s.lamp;
    w.generate_box(21, 0, 22, 36, 0, 36, g, g, false);
    w.generate_water_box(s, 21, 1, 22, 36, 23, 36, sea_level);
    for k in 0..4 {
        w.generate_box(21 + k, 13 + k, 21 + k, 36 - k, 13 + k, 21 + k, l, l, false);
        w.generate_box(21 + k, 13 + k, 36 - k, 36 - k, 13 + k, 36 - k, l, l, false);
        w.generate_box(21 + k, 13 + k, 22 + k, 21 + k, 13 + k, 35 - k, l, l, false);
        w.generate_box(36 - k, 13 + k, 22 + k, 36 - k, 13 + k, 35 - k, l, l, false);
    }
    w.generate_box(25, 16, 25, 32, 16, 32, g, g, false);
    w.generate_box(25, 17, 25, 25, 19, 25, l, l, false);
    w.generate_box(32, 17, 25, 32, 19, 25, l, l, false);
    w.generate_box(25, 17, 32, 25, 19, 32, l, l, false);
    w.generate_box(32, 17, 32, 32, 19, 32, l, l, false);
    w.place(l, 26, 20, 26);
    w.place(l, 27, 21, 27);
    w.place(m, 27, 20, 27);
    w.place(l, 26, 20, 31);
    w.place(l, 27, 21, 30);
    w.place(m, 27, 20, 30);
    w.place(l, 31, 20, 31);
    w.place(l, 30, 21, 30);
    w.place(m, 30, 20, 30);
    w.place(l, 31, 20, 26);
    w.place(l, 30, 21, 27);
    w.place(m, 30, 20, 27);
    w.generate_box(28, 21, 27, 29, 21, 27, g, g, false);
    w.generate_box(27, 21, 28, 27, 21, 29, g, g, false);
    w.generate_box(28, 21, 30, 29, 21, 30, g, g, false);
    w.generate_box(30, 21, 28, 30, 21, 29, g, g, false);
}

#[cfg_attr(not(test), allow(dead_code))]
fn monument_generate_lower_wall(
    w: &mut PieceWorld<'_, '_>,
    s: &MonumentStates,
    clip: Bounds,
    sea_level: i32,
) {
    let g = s.gray;
    let l = s.light;
    let d = s.light;
    if w.chunk_intersects(&clip, 0, 21, 6, 58) {
        w.generate_box(0, 0, 21, 6, 0, 57, g, g, false);
        w.generate_water_box(s, 0, 1, 21, 6, 7, 57, sea_level);
        w.generate_box(4, 4, 21, 6, 4, 53, g, g, false);
        for k in 0..4 {
            w.generate_box(k, k + 1, 21, k, k + 1, 57 - k, l, l, false);
        }
        let mut z = 23;
        while z < 53 {
            w.place(d, 5, 5, z);
            z += 3;
        }
        w.place(d, 5, 5, 52);
        for k in 0..4 {
            w.generate_box(k, k + 1, 21, k, k + 1, 57 - k, l, l, false);
        }
        w.generate_box(4, 1, 52, 6, 3, 52, g, g, false);
        w.generate_box(5, 1, 51, 5, 3, 53, g, g, false);
    }
    if w.chunk_intersects(&clip, 51, 21, 58, 58) {
        w.generate_box(51, 0, 21, 57, 0, 57, g, g, false);
        w.generate_water_box(s, 51, 1, 21, 57, 7, 57, sea_level);
        w.generate_box(51, 4, 21, 53, 4, 53, g, g, false);
        for k in 0..4 {
            w.generate_box(57 - k, k + 1, 21, 57 - k, k + 1, 57 - k, l, l, false);
        }
        let mut z = 23;
        while z < 53 {
            w.place(d, 52, 5, z);
            z += 3;
        }
        w.place(d, 52, 5, 52);
        w.generate_box(51, 1, 52, 53, 3, 52, g, g, false);
        w.generate_box(52, 1, 51, 52, 3, 53, g, g, false);
    }
    if w.chunk_intersects(&clip, 0, 51, 57, 57) {
        w.generate_box(7, 0, 51, 50, 0, 57, g, g, false);
        w.generate_water_box(s, 7, 1, 51, 50, 10, 57, sea_level);
        for k in 0..4 {
            w.generate_box(k + 1, k + 1, 57 - k, 56 - k, k + 1, 57 - k, l, l, false);
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn monument_generate_middle_wall(
    w: &mut PieceWorld<'_, '_>,
    s: &MonumentStates,
    clip: Bounds,
    sea_level: i32,
) {
    let g = s.gray;
    let l = s.light;
    let d = s.light;
    if w.chunk_intersects(&clip, 7, 21, 13, 50) {
        w.generate_box(7, 0, 21, 13, 0, 50, g, g, false);
        w.generate_water_box(s, 7, 1, 21, 13, 10, 50, sea_level);
        w.generate_box(11, 8, 21, 13, 8, 53, g, g, false);
        for k in 0..4 {
            w.generate_box(k + 7, k + 5, 21, k + 7, k + 5, 54, l, l, false);
        }
        let mut z = 21;
        while z <= 45 {
            w.place(d, 12, 9, z);
            z += 3;
        }
    }
    if w.chunk_intersects(&clip, 44, 21, 50, 54) {
        w.generate_box(44, 0, 21, 50, 0, 50, g, g, false);
        w.generate_water_box(s, 44, 1, 21, 50, 10, 50, sea_level);
        w.generate_box(44, 8, 21, 46, 8, 53, g, g, false);
        for k in 0..4 {
            w.generate_box(50 - k, k + 5, 21, 50 - k, k + 5, 54, l, l, false);
        }
        let mut z = 21;
        while z <= 45 {
            w.place(d, 45, 9, z);
            z += 3;
        }
    }
    if w.chunk_intersects(&clip, 8, 44, 49, 54) {
        w.generate_box(14, 0, 44, 43, 0, 50, g, g, false);
        w.generate_water_box(s, 14, 1, 44, 43, 10, 50, sea_level);
        let mut x = 12;
        while x <= 45 {
            w.place(d, x, 9, 45);
            w.place(d, x, 9, 52);
            if matches!(x, 12 | 18 | 24 | 33 | 39 | 45) {
                w.place(d, x, 9, 47);
                w.place(d, x, 9, 50);
                w.place(d, x, 10, 45);
                w.place(d, x, 10, 46);
                w.place(d, x, 10, 51);
                w.place(d, x, 10, 52);
                w.place(d, x, 11, 47);
                w.place(d, x, 11, 50);
                w.place(d, x, 12, 48);
                w.place(d, x, 12, 49);
            }
            x += 3;
        }
        for k in 0..3 {
            w.generate_box(8 + k, 5 + k, 54, 49 - k, 5 + k, 54, g, g, false);
        }
        w.generate_box(11, 8, 54, 46, 8, 54, l, l, false);
        w.generate_box(14, 8, 44, 43, 8, 53, g, g, false);
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn monument_generate_upper_wall(
    w: &mut PieceWorld<'_, '_>,
    s: &MonumentStates,
    clip: Bounds,
    sea_level: i32,
) {
    let g = s.gray;
    let l = s.light;
    let d = s.light;
    if w.chunk_intersects(&clip, 14, 21, 20, 43) {
        w.generate_box(14, 0, 21, 20, 0, 43, g, g, false);
        w.generate_water_box(s, 14, 1, 22, 20, 14, 43, sea_level);
        w.generate_box(18, 12, 22, 20, 12, 39, g, g, false);
        w.generate_box(18, 12, 21, 20, 12, 21, l, l, false);
        for k in 0..4 {
            w.generate_box(k + 14, k + 9, 21, k + 14, k + 9, 43 - k, l, l, false);
        }
        let mut z = 23;
        while z <= 39 {
            w.place(d, 19, 13, z);
            z += 3;
        }
    }
    if w.chunk_intersects(&clip, 37, 21, 43, 43) {
        w.generate_box(37, 0, 21, 43, 0, 43, g, g, false);
        w.generate_water_box(s, 37, 1, 22, 43, 14, 43, sea_level);
        w.generate_box(37, 12, 22, 39, 12, 39, g, g, false);
        w.generate_box(37, 12, 21, 39, 12, 21, l, l, false);
        for k in 0..4 {
            w.generate_box(43 - k, k + 9, 21, 43 - k, k + 9, 43 - k, l, l, false);
        }
        let mut z = 23;
        while z <= 39 {
            w.place(d, 38, 13, z);
            z += 3;
        }
    }
    if w.chunk_intersects(&clip, 15, 37, 42, 43) {
        w.generate_box(21, 0, 37, 36, 0, 43, g, g, false);
        w.generate_water_box(s, 21, 1, 37, 36, 14, 43, sea_level);
        w.generate_box(21, 12, 37, 36, 12, 39, g, g, false);
        for k in 0..4 {
            w.generate_box(15 + k, k + 9, 43 - k, 42 - k, k + 9, 43 - k, l, l, false);
        }
        let mut x = 21;
        while x <= 36 {
            w.place(d, x, 13, 38);
            x += 3;
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn monument_piece_post_process(
    w: &mut PieceWorld<'_, '_>,
    s: &MonumentStates,
    piece: &MonumentPiece,
    rooms: &[RoomDefinition],
    rng: &mut dyn MonumentRng,
    clip: Bounds,
    sea_level: i32,
    elders: &mut Vec<((i32, i32, i32), &'static str)>,
) {
    let g = s.gray;
    let l = s.light;
    let k = s.black;
    let m = s.lamp;
    // The wing rooms and the penthouse carry no `roomDefinition`; they never read it, so a
    // placeholder with `index = -1` stands in rather than making every arm optional.
    let room = match piece.room {
        Some(id) => rooms[id as usize].clone(),
        None => RoomDefinition::new(-1),
    };
    let conn = |d: usize| rooms[room.connections[d].expect("room connection") as usize].clone();
    match piece.kind {
        MonumentKind::Building => unreachable!("the building body is written separately"),
        MonumentKind::Core => {
            w.generate_box_on_fill(s, 1, 8, 0, 14, 8, 14, g);
            w.generate_box(0, 7, 0, 0, 7, 15, l, l, false);
            w.generate_box(15, 7, 0, 15, 7, 15, l, l, false);
            w.generate_box(1, 7, 0, 15, 7, 0, l, l, false);
            w.generate_box(1, 7, 15, 14, 7, 15, l, l, false);
            for i in 1..=6 {
                let bs = if i == 2 || i == 6 { g } else { l };
                let mut x = 0;
                while x <= 15 {
                    w.generate_box(x, i, 0, x, i, 1, bs, bs, false);
                    w.generate_box(x, i, 6, x, i, 9, bs, bs, false);
                    w.generate_box(x, i, 14, x, i, 15, bs, bs, false);
                    x += 15;
                }
                w.generate_box(1, i, 0, 1, i, 0, bs, bs, false);
                w.generate_box(6, i, 0, 9, i, 0, bs, bs, false);
                w.generate_box(14, i, 0, 14, i, 0, bs, bs, false);
                w.generate_box(1, i, 15, 14, i, 15, bs, bs, false);
            }
            w.generate_box(6, 3, 6, 9, 6, 9, k, k, false);
            w.generate_box(7, 4, 7, 8, 5, 8, s.gold, s.gold, false);
            for i in [3, 6] {
                for j in [6, 9] {
                    w.place(m, j, i, 6);
                    w.place(m, j, i, 9);
                }
            }
            for (a, b, c, d, e, f) in [
                (5, 1, 6, 5, 2, 6),
                (5, 1, 9, 5, 2, 9),
                (10, 1, 6, 10, 2, 6),
                (10, 1, 9, 10, 2, 9),
                (6, 1, 5, 6, 2, 5),
                (9, 1, 5, 9, 2, 5),
                (6, 1, 10, 6, 2, 10),
                (9, 1, 10, 9, 2, 10),
                (5, 2, 5, 5, 6, 5),
                (5, 2, 10, 5, 6, 10),
                (10, 2, 5, 10, 6, 5),
                (10, 2, 10, 10, 6, 10),
                (5, 7, 1, 5, 7, 6),
                (10, 7, 1, 10, 7, 6),
                (5, 7, 9, 5, 7, 14),
                (10, 7, 9, 10, 7, 14),
                (1, 7, 5, 6, 7, 5),
                (1, 7, 10, 6, 7, 10),
                (9, 7, 5, 14, 7, 5),
                (9, 7, 10, 14, 7, 10),
                (2, 1, 2, 2, 1, 3),
                (3, 1, 2, 3, 1, 2),
                (13, 1, 2, 13, 1, 3),
                (12, 1, 2, 12, 1, 2),
                (2, 1, 12, 2, 1, 13),
                (3, 1, 13, 3, 1, 13),
                (13, 1, 12, 13, 1, 13),
                (12, 1, 13, 12, 1, 13),
            ] {
                w.generate_box(a, b, c, d, e, f, l, l, false);
            }
        }
        MonumentKind::DoubleX => {
            let east = conn(DIR_EAST);
            if room.index / 25 > 0 {
                w.generate_default_floor(s, 8, 0, east.has_opening[DIR_DOWN]);
                w.generate_default_floor(s, 0, 0, room.has_opening[DIR_DOWN]);
            }
            if room.connections[DIR_UP].is_none() {
                w.generate_box_on_fill(s, 1, 4, 1, 7, 4, 6, g);
            }
            if east.connections[DIR_UP].is_none() {
                w.generate_box_on_fill(s, 8, 4, 1, 14, 4, 6, g);
            }
            w.generate_box(0, 3, 0, 0, 3, 7, l, l, false);
            w.generate_box(15, 3, 0, 15, 3, 7, l, l, false);
            w.generate_box(1, 3, 0, 15, 3, 0, l, l, false);
            w.generate_box(1, 3, 7, 14, 3, 7, l, l, false);
            w.generate_box(0, 2, 0, 0, 2, 7, g, g, false);
            w.generate_box(15, 2, 0, 15, 2, 7, g, g, false);
            w.generate_box(1, 2, 0, 15, 2, 0, g, g, false);
            w.generate_box(1, 2, 7, 14, 2, 7, g, g, false);
            w.generate_box(0, 1, 0, 0, 1, 7, l, l, false);
            w.generate_box(15, 1, 0, 15, 1, 7, l, l, false);
            w.generate_box(1, 1, 0, 15, 1, 0, l, l, false);
            w.generate_box(1, 1, 7, 14, 1, 7, l, l, false);
            w.generate_box(5, 1, 0, 10, 1, 4, l, l, false);
            w.generate_box(6, 2, 0, 9, 2, 3, g, g, false);
            w.generate_box(5, 3, 0, 10, 3, 4, l, l, false);
            w.place(m, 6, 2, 3);
            w.place(m, 9, 2, 3);
            if room.has_opening[DIR_SOUTH] {
                w.generate_water_box(s, 3, 1, 0, 4, 2, 0, sea_level);
            }
            if room.has_opening[DIR_NORTH] {
                w.generate_water_box(s, 3, 1, 7, 4, 2, 7, sea_level);
            }
            if room.has_opening[DIR_WEST] {
                w.generate_water_box(s, 0, 1, 3, 0, 2, 4, sea_level);
            }
            if east.has_opening[DIR_SOUTH] {
                w.generate_water_box(s, 11, 1, 0, 12, 2, 0, sea_level);
            }
            if east.has_opening[DIR_NORTH] {
                w.generate_water_box(s, 11, 1, 7, 12, 2, 7, sea_level);
            }
            if east.has_opening[DIR_EAST] {
                w.generate_water_box(s, 15, 1, 3, 15, 2, 4, sea_level);
            }
        }
        MonumentKind::DoubleXY => {
            let east = conn(DIR_EAST);
            let up = conn(DIR_UP);
            let east_up = rooms[east.connections[DIR_UP].expect("east up") as usize].clone();
            if room.index / 25 > 0 {
                w.generate_default_floor(s, 8, 0, east.has_opening[DIR_DOWN]);
                w.generate_default_floor(s, 0, 0, room.has_opening[DIR_DOWN]);
            }
            if up.connections[DIR_UP].is_none() {
                w.generate_box_on_fill(s, 1, 8, 1, 7, 8, 6, g);
            }
            if east_up.connections[DIR_UP].is_none() {
                w.generate_box_on_fill(s, 8, 8, 1, 14, 8, 6, g);
            }
            for i in 1..=7 {
                let bs = if i == 2 || i == 6 { g } else { l };
                w.generate_box(0, i, 0, 0, i, 7, bs, bs, false);
                w.generate_box(15, i, 0, 15, i, 7, bs, bs, false);
                w.generate_box(1, i, 0, 15, i, 0, bs, bs, false);
                w.generate_box(1, i, 7, 14, i, 7, bs, bs, false);
            }
            for (a, b, c, d, e, f) in [
                (2, 1, 3, 2, 7, 4),
                (3, 1, 2, 4, 7, 2),
                (3, 1, 5, 4, 7, 5),
                (13, 1, 3, 13, 7, 4),
                (11, 1, 2, 12, 7, 2),
                (11, 1, 5, 12, 7, 5),
                (5, 1, 3, 5, 3, 4),
                (10, 1, 3, 10, 3, 4),
                (5, 7, 2, 10, 7, 5),
                (5, 5, 2, 5, 7, 2),
                (10, 5, 2, 10, 7, 2),
                (5, 5, 5, 5, 7, 5),
                (10, 5, 5, 10, 7, 5),
            ] {
                w.generate_box(a, b, c, d, e, f, l, l, false);
            }
            w.place(l, 6, 6, 2);
            w.place(l, 9, 6, 2);
            w.place(l, 6, 6, 5);
            w.place(l, 9, 6, 5);
            w.generate_box(5, 4, 3, 6, 4, 4, l, l, false);
            w.generate_box(9, 4, 3, 10, 4, 4, l, l, false);
            w.place(m, 5, 4, 2);
            w.place(m, 5, 4, 5);
            w.place(m, 10, 4, 2);
            w.place(m, 10, 4, 5);
            // The lower half's four openings, then the upper half's, each side in the order
            // the piece declares them.
            if room.has_opening[DIR_SOUTH] {
                w.generate_water_box(s, 3, 1, 0, 4, 2, 0, sea_level);
            }
            if room.has_opening[DIR_NORTH] {
                w.generate_water_box(s, 3, 1, 7, 4, 2, 7, sea_level);
            }
            if room.has_opening[DIR_WEST] {
                w.generate_water_box(s, 0, 1, 3, 0, 2, 4, sea_level);
            }
            if east.has_opening[DIR_SOUTH] {
                w.generate_water_box(s, 11, 1, 0, 12, 2, 0, sea_level);
            }
            if east.has_opening[DIR_NORTH] {
                w.generate_water_box(s, 11, 1, 7, 12, 2, 7, sea_level);
            }
            if east.has_opening[DIR_EAST] {
                w.generate_water_box(s, 15, 1, 3, 15, 2, 4, sea_level);
            }
            if up.has_opening[DIR_SOUTH] {
                w.generate_water_box(s, 3, 5, 0, 4, 6, 0, sea_level);
            }
            if up.has_opening[DIR_NORTH] {
                w.generate_water_box(s, 3, 5, 7, 4, 6, 7, sea_level);
            }
            if up.has_opening[DIR_WEST] {
                w.generate_water_box(s, 0, 5, 3, 0, 6, 4, sea_level);
            }
            if east_up.has_opening[DIR_SOUTH] {
                w.generate_water_box(s, 11, 5, 0, 12, 6, 0, sea_level);
            }
            if east_up.has_opening[DIR_NORTH] {
                w.generate_water_box(s, 11, 5, 7, 12, 6, 7, sea_level);
            }
            if east_up.has_opening[DIR_EAST] {
                w.generate_water_box(s, 15, 5, 3, 15, 6, 4, sea_level);
            }
        }
        MonumentKind::DoubleY => {
            if room.index / 25 > 0 {
                w.generate_default_floor(s, 0, 0, room.has_opening[DIR_DOWN]);
            }
            let up = conn(DIR_UP);
            if up.connections[DIR_UP].is_none() {
                w.generate_box_on_fill(s, 1, 8, 1, 6, 8, 6, g);
            }
            for (a, b, c, d, e, f) in [
                (0, 4, 0, 0, 4, 7),
                (7, 4, 0, 7, 4, 7),
                (1, 4, 0, 6, 4, 0),
                (1, 4, 7, 6, 4, 7),
                (2, 4, 1, 2, 4, 2),
                (1, 4, 2, 1, 4, 2),
                (5, 4, 1, 5, 4, 2),
                (6, 4, 2, 6, 4, 2),
                (2, 4, 5, 2, 4, 6),
                (1, 4, 5, 1, 4, 5),
                (5, 4, 5, 5, 4, 6),
                (6, 4, 5, 6, 4, 5),
            ] {
                w.generate_box(a, b, c, d, e, f, l, l, false);
            }
            let mut cur = room.clone();
            for i in [1, 5] {
                let z = 0;
                if cur.has_opening[DIR_SOUTH] {
                    w.generate_box(2, i, z, 2, i + 2, z, l, l, false);
                    w.generate_box(5, i, z, 5, i + 2, z, l, l, false);
                    w.generate_box(3, i + 2, z, 4, i + 2, z, l, l, false);
                } else {
                    w.generate_box(0, i, z, 7, i + 2, z, l, l, false);
                    w.generate_box(0, i + 1, z, 7, i + 1, z, g, g, false);
                }
                let zz = 7;
                if cur.has_opening[DIR_NORTH] {
                    w.generate_box(2, i, zz, 2, i + 2, zz, l, l, false);
                    w.generate_box(5, i, zz, 5, i + 2, zz, l, l, false);
                    w.generate_box(3, i + 2, zz, 4, i + 2, zz, l, l, false);
                } else {
                    w.generate_box(0, i, zz, 7, i + 2, zz, l, l, false);
                    w.generate_box(0, i + 1, zz, 7, i + 1, zz, g, g, false);
                }
                let x = 0;
                if cur.has_opening[DIR_WEST] {
                    w.generate_box(x, i, 2, x, i + 2, 2, l, l, false);
                    w.generate_box(x, i, 5, x, i + 2, 5, l, l, false);
                    w.generate_box(x, i + 2, 3, x, i + 2, 4, l, l, false);
                } else {
                    w.generate_box(x, i, 0, x, i + 2, 7, l, l, false);
                    w.generate_box(x, i + 1, 0, x, i + 1, 7, g, g, false);
                }
                let xx = 7;
                if cur.has_opening[DIR_EAST] {
                    w.generate_box(xx, i, 2, xx, i + 2, 2, l, l, false);
                    w.generate_box(xx, i, 5, xx, i + 2, 5, l, l, false);
                    w.generate_box(xx, i + 2, 3, xx, i + 2, 4, l, l, false);
                } else {
                    w.generate_box(xx, i, 0, xx, i + 2, 7, l, l, false);
                    w.generate_box(xx, i + 1, 0, xx, i + 1, 7, g, g, false);
                }
                cur = up.clone();
            }
        }
        MonumentKind::DoubleYZ => {
            let north = conn(DIR_NORTH);
            let up_north = rooms[north.connections[DIR_UP].expect("north up") as usize].clone();
            let up = conn(DIR_UP);
            if room.index / 25 > 0 {
                w.generate_default_floor(s, 0, 8, north.has_opening[DIR_DOWN]);
                w.generate_default_floor(s, 0, 0, room.has_opening[DIR_DOWN]);
            }
            if up.connections[DIR_UP].is_none() {
                w.generate_box_on_fill(s, 1, 8, 1, 6, 8, 7, g);
            }
            if up_north.connections[DIR_UP].is_none() {
                w.generate_box_on_fill(s, 1, 8, 8, 6, 8, 14, g);
            }
            for i in 1..=7 {
                let bs = if i == 2 || i == 6 { g } else { l };
                w.generate_box(0, i, 0, 0, i, 15, bs, bs, false);
                w.generate_box(7, i, 0, 7, i, 15, bs, bs, false);
                w.generate_box(1, i, 0, 6, i, 0, bs, bs, false);
                w.generate_box(1, i, 15, 6, i, 15, bs, bs, false);
            }
            for i in 1..=7 {
                let bs = if i == 2 || i == 6 { m } else { k };
                w.generate_box(3, i, 7, 4, i, 8, bs, bs, false);
            }
            if room.has_opening[DIR_SOUTH] {
                w.generate_water_box(s, 3, 1, 0, 4, 2, 0, sea_level);
            }
            if room.has_opening[DIR_EAST] {
                w.generate_water_box(s, 7, 1, 3, 7, 2, 4, sea_level);
            }
            if room.has_opening[DIR_WEST] {
                w.generate_water_box(s, 0, 1, 3, 0, 2, 4, sea_level);
            }
            if north.has_opening[DIR_NORTH] {
                w.generate_water_box(s, 3, 1, 15, 4, 2, 15, sea_level);
            }
            if north.has_opening[DIR_WEST] {
                w.generate_water_box(s, 0, 1, 11, 0, 2, 12, sea_level);
            }
            if north.has_opening[DIR_EAST] {
                w.generate_water_box(s, 7, 1, 11, 7, 2, 12, sea_level);
            }
            if up.has_opening[DIR_SOUTH] {
                w.generate_water_box(s, 3, 5, 0, 4, 6, 0, sea_level);
            }
            if up.has_opening[DIR_EAST] {
                w.generate_water_box(s, 7, 5, 3, 7, 6, 4, sea_level);
                w.generate_box(5, 4, 2, 6, 4, 5, l, l, false);
                w.generate_box(6, 1, 2, 6, 3, 2, l, l, false);
                w.generate_box(6, 1, 5, 6, 3, 5, l, l, false);
            }
            if up.has_opening[DIR_WEST] {
                w.generate_water_box(s, 0, 5, 3, 0, 6, 4, sea_level);
                w.generate_box(1, 4, 2, 2, 4, 5, l, l, false);
                w.generate_box(1, 1, 2, 1, 3, 2, l, l, false);
                w.generate_box(1, 1, 5, 1, 3, 5, l, l, false);
            }
            if up_north.has_opening[DIR_NORTH] {
                w.generate_water_box(s, 3, 5, 15, 4, 6, 15, sea_level);
            }
            if up_north.has_opening[DIR_WEST] {
                w.generate_water_box(s, 0, 5, 11, 0, 6, 12, sea_level);
                w.generate_box(1, 4, 10, 2, 4, 13, l, l, false);
                w.generate_box(1, 1, 10, 1, 3, 10, l, l, false);
                w.generate_box(1, 1, 13, 1, 3, 13, l, l, false);
            }
            if up_north.has_opening[DIR_EAST] {
                w.generate_water_box(s, 7, 5, 11, 7, 6, 12, sea_level);
                w.generate_box(5, 4, 10, 6, 4, 13, l, l, false);
                w.generate_box(6, 1, 10, 6, 3, 10, l, l, false);
                w.generate_box(6, 1, 13, 6, 3, 13, l, l, false);
            }
        }
        MonumentKind::DoubleZ => {
            let north = conn(DIR_NORTH);
            if room.index / 25 > 0 {
                w.generate_default_floor(s, 0, 8, north.has_opening[DIR_DOWN]);
                w.generate_default_floor(s, 0, 0, room.has_opening[DIR_DOWN]);
            }
            if room.connections[DIR_UP].is_none() {
                w.generate_box_on_fill(s, 1, 4, 1, 6, 4, 7, g);
            }
            if north.connections[DIR_UP].is_none() {
                w.generate_box_on_fill(s, 1, 4, 8, 6, 4, 14, g);
            }
            for (a, b, c, d, e, f, st) in [
                (0, 3, 0, 0, 3, 15, l),
                (7, 3, 0, 7, 3, 15, l),
                (1, 3, 0, 7, 3, 0, l),
                (1, 3, 15, 6, 3, 15, l),
                (0, 2, 0, 0, 2, 15, g),
                (7, 2, 0, 7, 2, 15, g),
                (1, 2, 0, 7, 2, 0, g),
                (1, 2, 15, 6, 2, 15, g),
                (0, 1, 0, 0, 1, 15, l),
                (7, 1, 0, 7, 1, 15, l),
                (1, 1, 0, 7, 1, 0, l),
                (1, 1, 15, 6, 1, 15, l),
                (1, 1, 1, 1, 1, 2, l),
                (6, 1, 1, 6, 1, 2, l),
                (1, 3, 1, 1, 3, 2, l),
                (6, 3, 1, 6, 3, 2, l),
                (1, 1, 13, 1, 1, 14, l),
                (6, 1, 13, 6, 1, 14, l),
                (1, 3, 13, 1, 3, 14, l),
                (6, 3, 13, 6, 3, 14, l),
                (2, 1, 6, 2, 3, 6, l),
                (5, 1, 6, 5, 3, 6, l),
                (2, 1, 9, 2, 3, 9, l),
                (5, 1, 9, 5, 3, 9, l),
                (3, 2, 6, 4, 2, 6, l),
                (3, 2, 9, 4, 2, 9, l),
                (2, 2, 7, 2, 2, 8, l),
                (5, 2, 7, 5, 2, 8, l),
            ] {
                w.generate_box(a, b, c, d, e, f, st, st, false);
            }
            w.place(m, 2, 2, 5);
            w.place(m, 5, 2, 5);
            w.place(m, 2, 2, 10);
            w.place(m, 5, 2, 10);
            w.place(l, 2, 3, 5);
            w.place(l, 5, 3, 5);
            w.place(l, 2, 3, 10);
            w.place(l, 5, 3, 10);
            if room.has_opening[DIR_SOUTH] {
                w.generate_water_box(s, 3, 1, 0, 4, 2, 0, sea_level);
            }
            if room.has_opening[DIR_EAST] {
                w.generate_water_box(s, 7, 1, 3, 7, 2, 4, sea_level);
            }
            if room.has_opening[DIR_WEST] {
                w.generate_water_box(s, 0, 1, 3, 0, 2, 4, sea_level);
            }
            if north.has_opening[DIR_NORTH] {
                w.generate_water_box(s, 3, 1, 15, 4, 2, 15, sea_level);
            }
            if north.has_opening[DIR_WEST] {
                w.generate_water_box(s, 0, 1, 11, 0, 2, 12, sea_level);
            }
            if north.has_opening[DIR_EAST] {
                w.generate_water_box(s, 7, 1, 11, 7, 2, 12, sea_level);
            }
        }
        MonumentKind::Entry => {
            for (a, b, c, d, e, f) in [
                (0, 3, 0, 2, 3, 7),
                (5, 3, 0, 7, 3, 7),
                (0, 2, 0, 1, 2, 7),
                (6, 2, 0, 7, 2, 7),
                (0, 1, 0, 0, 1, 7),
                (7, 1, 0, 7, 1, 7),
                (0, 1, 7, 7, 3, 7),
                (1, 1, 0, 2, 3, 0),
                (5, 1, 0, 6, 3, 0),
            ] {
                w.generate_box(a, b, c, d, e, f, l, l, false);
            }
            if room.has_opening[DIR_NORTH] {
                w.generate_water_box(s, 3, 1, 7, 4, 2, 7, sea_level);
            }
            if room.has_opening[DIR_WEST] {
                w.generate_water_box(s, 0, 1, 3, 1, 2, 4, sea_level);
            }
            if room.has_opening[DIR_EAST] {
                w.generate_water_box(s, 6, 1, 3, 7, 2, 4, sea_level);
            }
        }
        MonumentKind::Penthouse => {
            for (a, b, c, d, e, f, st) in [
                (2, -1, 2, 11, -1, 11, l),
                (0, -1, 0, 1, -1, 11, g),
                (12, -1, 0, 13, -1, 11, g),
                (2, -1, 0, 11, -1, 1, g),
                (2, -1, 12, 11, -1, 13, g),
                (0, 0, 0, 0, 0, 13, l),
                (13, 0, 0, 13, 0, 13, l),
                (1, 0, 0, 12, 0, 0, l),
                (1, 0, 13, 12, 0, 13, l),
            ] {
                w.generate_box(a, b, c, d, e, f, st, st, false);
            }
            let mut z = 2;
            while z <= 11 {
                w.place(m, 0, 0, z);
                w.place(m, 13, 0, z);
                w.place(m, z, 0, 0);
                z += 3;
            }
            for (a, b, c, d, e, f, st) in [
                (2, 0, 3, 4, 0, 9, l),
                (9, 0, 3, 11, 0, 9, l),
                (4, 0, 9, 9, 0, 11, l),
                (3, 0, 3, 3, 0, 7, k),
                (10, 0, 3, 10, 0, 7, k),
                (6, 0, 10, 7, 0, 10, k),
            ] {
                w.generate_box(a, b, c, d, e, f, st, st, false);
            }
            w.place(l, 5, 0, 8);
            w.place(l, 8, 0, 8);
            w.place(l, 10, 0, 10);
            w.place(l, 3, 0, 10);
            let mut x = 3;
            for _ in 0..2 {
                let mut zz = 2;
                while zz <= 8 {
                    w.generate_box(x, 0, zz, x, 2, zz, l, l, false);
                    zz += 3;
                }
                x = 10;
            }
            w.generate_box(5, 0, 10, 5, 2, 10, l, l, false);
            w.generate_box(8, 0, 10, 8, 2, 10, l, l, false);
            w.generate_box(6, -1, 7, 7, -1, 8, k, k, false);
            w.generate_water_box(s, 6, -1, 3, 7, -1, 4, sea_level);
            monument_spawn_elder(w, clip, 6, 1, 6, elders);
        }
        MonumentKind::Simple => {
            if room.index / 25 > 0 {
                w.generate_default_floor(s, 0, 0, room.has_opening[DIR_DOWN]);
            }
            if room.connections[DIR_UP].is_none() {
                w.generate_box_on_fill(s, 1, 4, 1, 6, 4, 6, g);
            }
            // `boolean` short-circuit: `nextBoolean` is drawn only for a non-zero design.
            let mut decorated = false;
            if piece.main_design != 0 {
                decorated = rng.next_bool()
                    && !room.has_opening[DIR_DOWN]
                    && !room.has_opening[DIR_UP]
                    && room.has_opening.iter().filter(|v| **v).count() > 1;
            }
            match piece.main_design {
                0 => {
                    for (a, b, c, d, e, f, st) in [
                        (0, 1, 0, 2, 1, 2, l),
                        (0, 3, 0, 2, 3, 2, l),
                        (0, 2, 0, 0, 2, 2, g),
                        (1, 2, 0, 2, 2, 0, g),
                        (5, 1, 0, 7, 1, 2, l),
                        (5, 3, 0, 7, 3, 2, l),
                        (7, 2, 0, 7, 2, 2, g),
                        (5, 2, 0, 6, 2, 0, g),
                        (0, 1, 5, 2, 1, 7, l),
                        (0, 3, 5, 2, 3, 7, l),
                        (0, 2, 5, 0, 2, 7, g),
                        (1, 2, 7, 2, 2, 7, g),
                        (5, 1, 5, 7, 1, 7, l),
                        (5, 3, 5, 7, 3, 7, l),
                        (7, 2, 5, 7, 2, 7, g),
                        (5, 2, 7, 6, 2, 7, g),
                    ] {
                        w.generate_box(a, b, c, d, e, f, st, st, false);
                    }
                    w.place(m, 1, 2, 1);
                    w.place(m, 6, 2, 1);
                    w.place(m, 1, 2, 6);
                    w.place(m, 6, 2, 6);
                    if room.has_opening[DIR_SOUTH] {
                        w.generate_box(3, 3, 0, 4, 3, 0, l, l, false);
                    } else {
                        w.generate_box(3, 3, 0, 4, 3, 1, l, l, false);
                        w.generate_box(3, 2, 0, 4, 2, 0, g, g, false);
                        w.generate_box(3, 1, 0, 4, 1, 1, l, l, false);
                    }
                    if room.has_opening[DIR_NORTH] {
                        w.generate_box(3, 3, 7, 4, 3, 7, l, l, false);
                    } else {
                        w.generate_box(3, 3, 6, 4, 3, 7, l, l, false);
                        w.generate_box(3, 2, 7, 4, 2, 7, g, g, false);
                        w.generate_box(3, 1, 6, 4, 1, 7, l, l, false);
                    }
                    if room.has_opening[DIR_WEST] {
                        w.generate_box(0, 3, 3, 0, 3, 4, l, l, false);
                    } else {
                        w.generate_box(0, 3, 3, 1, 3, 4, l, l, false);
                        w.generate_box(0, 2, 3, 0, 2, 4, g, g, false);
                        w.generate_box(0, 1, 3, 1, 1, 4, l, l, false);
                    }
                    if room.has_opening[DIR_EAST] {
                        w.generate_box(7, 3, 3, 7, 3, 4, l, l, false);
                    } else {
                        w.generate_box(6, 3, 3, 7, 3, 4, l, l, false);
                        w.generate_box(7, 2, 3, 7, 2, 4, g, g, false);
                        w.generate_box(6, 1, 3, 7, 1, 4, l, l, false);
                    }
                }
                1 => {
                    for (a, b, c, d, e, f, st) in [
                        (2, 1, 2, 2, 3, 2, l),
                        (2, 1, 5, 2, 3, 5, l),
                        (5, 1, 5, 5, 3, 5, l),
                        (5, 1, 2, 5, 3, 2, l),
                        (0, 1, 0, 1, 3, 0, l),
                        (0, 1, 1, 0, 3, 1, l),
                        (0, 1, 7, 1, 3, 7, l),
                        (0, 1, 6, 0, 3, 6, l),
                        (6, 1, 7, 7, 3, 7, l),
                        (7, 1, 6, 7, 3, 6, l),
                        (6, 1, 0, 7, 3, 0, l),
                        (7, 1, 1, 7, 3, 1, l),
                    ] {
                        w.generate_box(a, b, c, d, e, f, st, st, false);
                    }
                    w.place(m, 2, 2, 2);
                    w.place(m, 2, 2, 5);
                    w.place(m, 5, 2, 5);
                    w.place(m, 5, 2, 2);
                    for (a, b, c) in [
                        (1, 2, 0),
                        (0, 2, 1),
                        (1, 2, 7),
                        (0, 2, 6),
                        (6, 2, 7),
                        (7, 2, 6),
                        (6, 2, 0),
                        (7, 2, 1),
                    ] {
                        w.place(g, a, b, c);
                    }
                    if !room.has_opening[DIR_SOUTH] {
                        w.generate_box(1, 3, 0, 6, 3, 0, l, l, false);
                        w.generate_box(1, 2, 0, 6, 2, 0, g, g, false);
                        w.generate_box(1, 1, 0, 6, 1, 0, l, l, false);
                    }
                    if !room.has_opening[DIR_NORTH] {
                        w.generate_box(1, 3, 7, 6, 3, 7, l, l, false);
                        w.generate_box(1, 2, 7, 6, 2, 7, g, g, false);
                        w.generate_box(1, 1, 7, 6, 1, 7, l, l, false);
                    }
                    if !room.has_opening[DIR_WEST] {
                        w.generate_box(0, 3, 1, 0, 3, 6, l, l, false);
                        w.generate_box(0, 2, 1, 0, 2, 6, g, g, false);
                        w.generate_box(0, 1, 1, 0, 1, 6, l, l, false);
                    }
                    if !room.has_opening[DIR_EAST] {
                        w.generate_box(7, 3, 1, 7, 3, 6, l, l, false);
                        w.generate_box(7, 2, 1, 7, 2, 6, g, g, false);
                        w.generate_box(7, 1, 1, 7, 1, 6, l, l, false);
                    }
                }
                _ => {
                    for (a, b, c, d, e, f, st) in [
                        (0, 1, 0, 0, 1, 7, l),
                        (7, 1, 0, 7, 1, 7, l),
                        (1, 1, 0, 6, 1, 0, l),
                        (1, 1, 7, 6, 1, 7, l),
                        (0, 2, 0, 0, 2, 7, k),
                        (7, 2, 0, 7, 2, 7, k),
                        (1, 2, 0, 6, 2, 0, k),
                        (1, 2, 7, 6, 2, 7, k),
                        (0, 3, 0, 0, 3, 7, l),
                        (7, 3, 0, 7, 3, 7, l),
                        (1, 3, 0, 6, 3, 0, l),
                        (1, 3, 7, 6, 3, 7, l),
                        (0, 1, 3, 0, 2, 4, k),
                        (7, 1, 3, 7, 2, 4, k),
                        (3, 1, 0, 4, 2, 0, k),
                        (3, 1, 7, 4, 2, 7, k),
                    ] {
                        w.generate_box(a, b, c, d, e, f, st, st, false);
                    }
                    if room.has_opening[DIR_SOUTH] {
                        w.generate_water_box(s, 3, 1, 0, 4, 2, 0, sea_level);
                    }
                    if room.has_opening[DIR_NORTH] {
                        w.generate_water_box(s, 3, 1, 7, 4, 2, 7, sea_level);
                    }
                    if room.has_opening[DIR_WEST] {
                        w.generate_water_box(s, 0, 1, 3, 0, 2, 4, sea_level);
                    }
                    if room.has_opening[DIR_EAST] {
                        w.generate_water_box(s, 7, 1, 3, 7, 2, 4, sea_level);
                    }
                }
            }
            if decorated {
                w.generate_box(3, 1, 3, 4, 1, 4, l, l, false);
                w.generate_box(3, 2, 3, 4, 2, 4, g, g, false);
                w.generate_box(3, 3, 3, 4, 3, 4, l, l, false);
            }
        }
        MonumentKind::SimpleTop => {
            if room.index / 25 > 0 {
                w.generate_default_floor(s, 0, 0, room.has_opening[DIR_DOWN]);
            }
            if room.connections[DIR_UP].is_none() {
                w.generate_box_on_fill(s, 1, 4, 1, 6, 4, 6, g);
            }
            for x in 1..=6 {
                for z in 1..=6 {
                    if rng.next_bound(3) != 0 {
                        let y = 2 + i32::from(rng.next_bound(4) != 0);
                        w.generate_box(x, y, z, x, 3, z, s.sponge, s.sponge, false);
                    }
                }
            }
            for (a, b, c, d, e, f, st) in [
                (0, 1, 0, 0, 1, 7, l),
                (7, 1, 0, 7, 1, 7, l),
                (1, 1, 0, 6, 1, 0, l),
                (1, 1, 7, 6, 1, 7, l),
                (0, 2, 0, 0, 2, 7, k),
                (7, 2, 0, 7, 2, 7, k),
                (1, 2, 0, 6, 2, 0, k),
                (1, 2, 7, 6, 2, 7, k),
                (0, 3, 0, 0, 3, 7, l),
                (7, 3, 0, 7, 3, 7, l),
                (1, 3, 0, 6, 3, 0, l),
                (1, 3, 7, 6, 3, 7, l),
                (0, 1, 3, 0, 2, 4, k),
                (7, 1, 3, 7, 2, 4, k),
                (3, 1, 0, 4, 2, 0, k),
                (3, 1, 7, 4, 2, 7, k),
            ] {
                w.generate_box(a, b, c, d, e, f, st, st, false);
            }
            if room.has_opening[DIR_SOUTH] {
                w.generate_water_box(s, 3, 1, 0, 4, 2, 0, sea_level);
            }
        }
        MonumentKind::Wing => {
            if piece.main_design == 0 {
                for k in 0..4 {
                    w.generate_box(10 - k, 3 - k, 20 - k, 12 + k, 3 - k, 20, l, l, false);
                }
                for (a, b, c, d, e, f, st) in [
                    (7, 0, 6, 15, 0, 16, l),
                    (6, 0, 6, 6, 3, 20, l),
                    (16, 0, 6, 16, 3, 20, l),
                    (7, 1, 7, 7, 1, 20, l),
                    (15, 1, 7, 15, 1, 20, l),
                    (7, 1, 6, 9, 3, 6, l),
                    (13, 1, 6, 15, 3, 6, l),
                    (8, 1, 7, 9, 1, 7, l),
                    (13, 1, 7, 14, 1, 7, l),
                    (9, 0, 5, 13, 0, 5, l),
                    (10, 0, 7, 12, 0, 7, k),
                    (8, 0, 10, 8, 0, 12, k),
                    (14, 0, 10, 14, 0, 12, k),
                    (6, 0, 21, 7, 4, 21, l),
                    (15, 0, 21, 16, 4, 21, l),
                ] {
                    w.generate_box(a, b, c, d, e, f, st, st, false);
                }
                let mut z = 18;
                while z >= 7 {
                    w.place(m, 6, 3, z);
                    w.place(m, 16, 3, z);
                    z -= 3;
                }
                for (x, y, z) in [
                    (10, 0, 10),
                    (12, 0, 10),
                    (10, 0, 12),
                    (12, 0, 12),
                    (8, 3, 6),
                    (14, 3, 6),
                ] {
                    w.place(m, x, y, z);
                }
                for (x, y, z, st) in [
                    (4, 2, 4, l),
                    (4, 1, 4, m),
                    (4, 0, 4, l),
                    (18, 2, 4, l),
                    (18, 1, 4, m),
                    (18, 0, 4, l),
                    (4, 2, 18, l),
                    (4, 1, 18, m),
                    (4, 0, 18, l),
                    (18, 2, 18, l),
                    (18, 1, 18, m),
                    (18, 0, 18, l),
                    (9, 7, 20, l),
                    (13, 7, 20, l),
                ] {
                    w.place(st, x, y, z);
                }
                monument_spawn_elder(w, clip, 11, 2, 16, elders);
            } else {
                for (a, b, c, d, e, f, st) in [
                    (9, 3, 18, 13, 3, 20, l),
                    (9, 0, 18, 9, 2, 18, l),
                    (13, 0, 18, 13, 2, 18, l),
                    (7, 3, 7, 15, 3, 14, l),
                ] {
                    w.generate_box(a, b, c, d, e, f, st, st, false);
                }
                let mut x = 9;
                for _ in 0..2 {
                    w.place(l, x, 6, 20);
                    w.place(m, x, 5, 20);
                    w.place(l, x, 4, 20);
                    x = 13;
                }
                let mut x = 10;
                for _ in 0..2 {
                    w.generate_box(x, 0, 10, x, 6, 10, l, l, false);
                    w.generate_box(x, 0, 12, x, 6, 12, l, l, false);
                    w.place(m, x, 0, 10);
                    w.place(m, x, 0, 12);
                    w.place(m, x, 4, 10);
                    w.place(m, x, 4, 12);
                    x = 12;
                }
                let mut x = 8;
                for _ in 0..2 {
                    w.generate_box(x, 0, 7, x, 2, 7, l, l, false);
                    w.generate_box(x, 0, 14, x, 2, 14, l, l, false);
                    x = 14;
                }
                w.generate_box(8, 3, 8, 8, 3, 13, k, k, false);
                w.generate_box(14, 3, 8, 14, 3, 13, k, k, false);
                monument_spawn_elder(w, clip, 11, 5, 13, elders);
            }
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn monument_spawn_elder(
    w: &mut PieceWorld<'_, '_>,
    clip: Bounds,
    x: i32,
    y: i32,
    z: i32,
    elders: &mut Vec<((i32, i32, i32), &'static str)>,
) {
    let p = w.world_pos(x, y, z);
    if sp_contains(&clip, p) {
        elders.push((p, "minecraft:elder_guardian"));
    }
}

/// Place one monument chunk. The building writes its own body, then every child whose box
/// meets the clip, exactly as `MonumentBuilding.postProcess` ends.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn place_monument_chunk<R: StructureRng + FullIntRng>(
    ctx: &mut FC<'_>,
    reg: &'static BReg,
    layout: &mut MonumentLayout,
    rng: &mut R,
    clip: Bounds,
    sea_level: i32,
    loots: &mut LootMap,
    elders: &mut Vec<((i32, i32, i32), &'static str)>,
) {
    let states = MonumentStates::new(reg);
    let MonumentLayout {
        pieces, rooms, ..
    } = layout;
    if pieces.is_empty() || !fm_bb_intersects(&pieces[0].bounds, &clip) {
        return;
    }
    {
        let building = pieces[0].clone();
        let mut w = PieceWorld::new(
            ctx,
            reg,
            clip,
            building.bounds,
            Some(building.orient),
            loots,
        );
        monument_building_body(&mut w, &states, clip, sea_level);
    }
    for i in 1..pieces.len() {
        if !fm_bb_intersects(&pieces[i].bounds, &clip) {
            continue;
        }
        let piece = pieces[i].clone();
        let mut w = PieceWorld::new(ctx, reg, clip, piece.bounds, Some(piece.orient), loots);
        monument_piece_post_process(&mut w, &states, &piece, rooms, rng, clip, sea_level, elders);
    }
}
