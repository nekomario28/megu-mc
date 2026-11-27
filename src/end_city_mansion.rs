// Native Minecraft 1.21.1 `minecraft:end_city` and `minecraft:mansion`.
//
// This file is included at the end of `structure_template.rs`, so it shares that
// module's template/processor vocabulary (`Template`, `Settings`, `ExtProc`,
// `Processor`, `place_in_world`, `filter_blocks`, `template_bb`, `bb_*`, `Pos`,
// `BB`, `Lcg`, `Xor`, ...).
//
// Both structures are `TemplateStructurePiece` assemblies, but their layout
// algorithms are different kinds of program:
//
//   * `end_city` is a recursive section generator with a rejection pass: every
//     candidate section is generated into a scratch list, given one shared
//     `genDepth` draw, and kept only if it does not collide with an already
//     accepted piece at a different depth (`EndCityPieces.recursiveChildren`).
//   * `mansion` is a grid algorithm: an 11x11 `SimpleGrid` is carved into
//     corridors and rooms, a second grid is derived for the third floor, and a
//     placer walks the grids emitting one template piece per wall/floor/roof
//     cell. The grid RNG stream is shared with the piece placer, so the order of
//     the draws in `identifyRooms` is part of the contract.
//
// Every contract below was read from the pinned 1.21.1 server jar
// (`tools/bytecode_probe.sh`) and is pinned by
// `tests/data/end_city_mansion_1211.tsv`, which
// `tools/end_city_mansion_oracle.py` regenerates from the native classes.
//
// What the fixture asserts, per structure:
//
//   * `end_city` - everything: the admission decision and stub, the whole
//     recursive piece tree (box, rotation, `genDepth`), and the multi-chunk
//     block placement with its chest loot seeds, shulkers and item frames.
//   * `mansion` - admission, stub, both layout grids cell by cell, the whole
//     557-619 piece tree, and full multi-chunk block placement (including chest
//     loot seeds, mob spawners, `WallBlock`/`FenceBlock`/`ChestBlock` shape
//     updates, and the `afterPlace` foundation). For `"Group of Allays"`, vanilla
//     draws `1..=3` from `level.getRandom()` (shared with `Mob.finalizeSpawn`),
//     so each group's position and `1..=3` range are verified while avoiding
//     spurious draws from `postRng`.
//
// Two native details are easy to get wrong and are called out where they bite:
//
//   * `getLowestYIn5by5BoxOffset7Blocks` reads four *base heights* (the
//     generator's own column answer, not the live heightmap) at
//     `chunk*16+7` and takes the **minimum**; both structures refuse below y=60.
//   * `TemplateStructurePiece.postProcess` runs the template with the *chunk's*
//     bounding box as both the write clip and the `filterBlocks` box, so a
//     multi-chunk structure writes each cell exactly once and its data markers
//     (chests, shulkers, elytra, mansion mobs) are handled per chunk.

// ---------------------------------------------------------------- directions

/// Native `Direction` in declaration order; the ordinals are what `from2DDataValue`
/// and `getClockWise` are defined over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Dir {
    Down,
    Up,
    North,
    South,
    West,
    East,
}

/// `Direction.Plane.HORIZONTAL`: `[NORTH, EAST, SOUTH, WEST]`, the order every
/// horizontal iteration in the mansion code uses.
pub(crate) const HORIZONTAL: [Dir; 4] = [Dir::North, Dir::East, Dir::South, Dir::West];

/// `Direction.from2DDataValue`: `[SOUTH, WEST, NORTH, EAST]`.
pub(crate) const BY_2D_DATA: [Dir; 4] = [Dir::South, Dir::West, Dir::North, Dir::East];

impl Dir {
    pub(crate) fn step_x(self) -> i32 {
        match self {
            Self::West => -1,
            Self::East => 1,
            _ => 0,
        }
    }
    pub(crate) fn step_z(self) -> i32 {
        match self {
            Self::North => -1,
            Self::South => 1,
            _ => 0,
        }
    }
    pub(crate) fn opposite(self) -> Self {
        match self {
            Self::Down => Self::Up,
            Self::Up => Self::Down,
            Self::North => Self::South,
            Self::South => Self::North,
            Self::West => Self::East,
            Self::East => Self::West,
        }
    }
    /// `Direction.getClockWise`: in the horizontal plane `N -> E -> S -> W -> N`.
    pub(crate) fn clock_wise(self) -> Self {
        match self {
            Self::North => Self::East,
            Self::East => Self::South,
            Self::South => Self::West,
            Self::West => Self::North,
            other => other,
        }
    }
    pub(crate) fn counter_clock_wise(self) -> Self {
        self.clock_wise().clock_wise().clock_wise()
    }
    /// `Direction.getSerializedName` — the value a `facing` property carries.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Down => "down",
            Self::Up => "up",
            Self::North => "north",
            Self::South => "south",
            Self::West => "west",
            Self::East => "east",
        }
    }
    /// `Direction.get3DDataValue`: the number `HangingEntity`'s `Facing` save byte
    /// and the `spawn_entity` object data carry. 1.21.1 builds `BY_3D_DATA` by
    /// sorting the enum on this field and the field is the declaration ordinal, so
    /// it is the ordinal (`DOWN` 0 … `EAST` 5) — *not* the older -1/2/0/1/3 table
    /// the 2D values imply.
    pub(crate) fn data_3d(self) -> u8 {
        self as u8
    }
}

/// `Direction.from3DDataValue`: `BY_3D_DATA` is the declaration order.
pub(crate) fn direction_from_3d_data_value(value: u8) -> Option<Dir> {
    Some(match value {
        0 => Dir::Down,
        1 => Dir::Up,
        2 => Dir::North,
        3 => Dir::South,
        4 => Dir::West,
        5 => Dir::East,
        _ => return None,
    })
}

/// `Rotation.getRotated(Rotation)`: index addition modulo four
/// (`NONE, CLOCKWISE_90, CLOCKWISE_180, COUNTERCLOCKWISE_90`).
pub(crate) fn rotate_rotation(this: Rotation, other: Rotation) -> Rotation {
    Rotation::from_index((this as u8 + other as u8) % 4).expect("rotation index")
}

/// `Rotation.rotate(Direction)`: vertical directions are invariant.
pub(crate) fn rotate_direction(rotation: Rotation, direction: Dir) -> Dir {
    match rotation {
        Rotation::None => direction,
        Rotation::Clockwise90 => direction.clock_wise(),
        Rotation::Clockwise180 => direction.opposite(),
        Rotation::Counterclockwise90 => direction.counter_clock_wise(),
    }
}

/// `BlockPos.rotate(Rotation)`.
pub(crate) fn rotate_pos(p: Pos, rotation: Rotation) -> Pos {
    match rotation {
        Rotation::None => p,
        Rotation::Clockwise90 => (p.2.wrapping_neg(), p.1, p.0),
        Rotation::Clockwise180 => (p.0.wrapping_neg(), p.1, p.2.wrapping_neg()),
        Rotation::Counterclockwise90 => (p.2, p.1, p.0.wrapping_neg()),
    }
}

/// `StructureTemplate.getZeroPositionWithTransform(pos, mirror, rotation, pivot)`,
/// i.e. `StructureTemplate.transform(...)` — the same body `block()` carries.
pub(crate) fn zero_pos(p: Pos, mirror: Mirror, rotation: Rotation, pivot: Pos) -> Pos {
    transform_pos(p, mirror, rotation, pivot)
}

// ---------------------------------------------------------------- template pieces

/// One `TemplateStructurePiece` produced by either structure.
///
/// `pos` is `TemplateStructurePiece.templatePosition`; `bb` is
/// `StructureTemplate.getBoundingBox(placeSettings, pos)` computed at
/// construction (native recomputes it from the piece's own template inside
/// `postProcess`, and that recomputation is a no-op for these settings because
/// the clip is not part of `getBoundingBox`).
#[derive(Clone, Debug)]
pub(crate) struct TemplatePiece {
    /// `minecraft:<structure>/<template>` resource id.
    pub(crate) template: String,
    pub(crate) pos: Pos,
    pub(crate) bb: BB,
    pub(crate) rotation: Rotation,
    pub(crate) mirror: Mirror,
    /// `BlockIgnoreProcessor.STRUCTURE_AND_AIR` when true, else `STRUCTURE_BLOCK`.
    ///
    /// `EndCityPieces` passes `overwrite` straight into this choice, and the
    /// base floors/roofs are the only pieces that write air.
    pub(crate) ignore_air: bool,
    /// `StructurePiece.genDepth`; `recursiveChildren` compares it.
    pub(crate) gen_depth: i32,
}

impl TemplatePiece {
    fn load(
        templates: &Templates,
        reg: &BReg,
        template: String,
        pos: Pos,
        rotation: Rotation,
        mirror: Mirror,
        ignore_air: bool,
    ) -> Result<Self, &'static str> {
        let loaded = templates.get(&template, reg)?;
        Ok(Self {
            template,
            pos,
            bb: template_bb(&loaded, pos, mirror, rotation, (0, 0, 0)),
            rotation,
            mirror,
            ignore_air,
            gen_depth: 0,
        })
    }

    /// `StructurePlaceSettings` for this piece: `setIgnoreEntities(true)`,
    /// the rotation/mirror, and the one `BlockIgnoreProcessor`.
    pub(crate) fn settings(&self) -> Settings {
        let mut settings = Settings::new(self.mirror, self.rotation, (0, 0, 0));
        settings.ignore_entities = true;
        settings.processors = vec![ExtProc::Native(Processor::BlockIgnore(vec![
            "minecraft:structure_block".to_owned(),
        ]))];
        if self.ignore_air {
            if let ExtProc::Native(Processor::BlockIgnore(names)) = &mut settings.processors[0] {
                names.push("minecraft:air".to_owned());
            }
        }
        settings
    }
}

fn rotated_offset(pos: Pos, rotation: Rotation, offset: Pos) -> Pos {
    add_pos(pos, rotate_pos(offset, rotation))
}

/// `(minX, minZ)` of a chunk, native `ChunkPos.getMinBlockX/Z`.
fn chunk_origin(cx: i32, cz: i32) -> (i32, i32) {
    (cx << 4, cz << 4)
}

/// `Structure.getLowestYIn5by5BoxOffset7Blocks`: the four
/// `getFirstOccupiedHeight` answers of the 5x5 box anchored at
/// `chunk*16 + 7`, minimum.
///
/// `ChunkGenerator.getFirstOccupiedHeight` is `getBaseHeight - 1` (the
/// generator's base height is the first *free* y), and native reads it through
/// that method, not through `getBaseHeight` directly.
///
/// `dx`/`dz` are `+5` except for the quarter turns that flip an axis. The
/// corners are read from the generator (`getBaseHeight`), not from the live
/// heightmap, so this consumes no RNG and observes no writes.
pub(crate) fn lowest_y_in_5by5_box_offset_7<W: TemplateWorld + ?Sized>(
    world: &mut W,
    cx: i32,
    cz: i32,
    rotation: Rotation,
) -> Pos {
    let (mut dx, mut dz) = (5, 5);
    match rotation {
        Rotation::Clockwise90 => dx = -5,
        Rotation::Clockwise180 => {
            dx = -5;
            dz = -5;
        }
        Rotation::Counterclockwise90 => dz = -5,
        Rotation::None => {}
    }
    let (min_x, min_z) = chunk_origin(cx, cz);
    let x = min_x + 7;
    let z = min_z + 7;
    let mut lowest = i32::MAX;
    for (cx_off, cz_off) in [(0, 0), (0, dz), (dx, 0), (dx, dz)] {
        let h = world.base_height(HeightmapKind::WorldSurfaceWg, x + cx_off, z + cz_off) - 1;
        lowest = lowest.min(h);
    }
    (x, lowest, z)
}

/// The lazy piece consumer native keeps behind `GenerationStub`'s `Either.left`:
/// the pieces are built only when the builder runs, after the caller's
/// intermediate `rngState()` draw.
pub(crate) type EndCityMansionPieceBuilder =
    Box<dyn FnOnce(&mut dyn TemplateWorld, &Templates, &mut Lcg, &BReg) -> Vec<TemplatePiece>>;

// ---------------------------------------------------------------- end city

/// `EndCityPieces.SectionGenerator` — the four section shapes.
enum EndCitySection {
    HouseTower(Pos),
    Tower,
    TowerBridge,
    FatTower,
}

/// `EndCityPieces.TOWER_BRIDGES`: `[NONE -> (1,-1,0), CLOCKWISE_90 -> (6,-1,1),
/// COUNTERCLOCKWISE_90 -> (0,-1,5), CLOCKWISE_180 -> (5,-1,6)]`.
pub(crate) const END_CITY_TOWER_BRIDGES: [(Rotation, Pos); 4] = [
    (Rotation::None, (1, -1, 0)),
    (Rotation::Clockwise90, (6, -1, 1)),
    (Rotation::Counterclockwise90, (0, -1, 5)),
    (Rotation::Clockwise180, (5, -1, 6)),
];

/// `EndCityPieces.FAT_TOWER_BRIDGES`: `[NONE -> (4,-1,0), CLOCKWISE_90 -> (12,-1,4),
/// COUNTERCLOCKWISE_90 -> (0,-1,8), CLOCKWISE_180 -> (8,-1,12)]`.
pub(crate) const END_CITY_FAT_TOWER_BRIDGES: [(Rotation, Pos); 4] = [
    (Rotation::None, (4, -1, 0)),
    (Rotation::Clockwise90, (12, -1, 4)),
    (Rotation::Counterclockwise90, (0, -1, 8)),
    (Rotation::Clockwise180, (8, -1, 12)),
];

/// `EndCityPieces.MAX_GEN_DEPTH`.
pub(crate) const END_CITY_MAX_GEN_DEPTH: i32 = 8;

fn end_city_piece(
    templates: &Templates,
    reg: &BReg,
    name: &str,
    pos: Pos,
    rotation: Rotation,
    overwrite: bool,
) -> Result<TemplatePiece, &'static str> {
    TemplatePiece::load(
        templates,
        reg,
        format!("minecraft:end_city/{name}"),
        pos,
        rotation,
        Mirror::None,
        !overwrite, // `STRUCTURE_AND_AIR` also drops air cells.
    )
}

/// `BoundingBox.intersects`.
///
/// The shared `bb_intersects` compares transposed indices (`a.maxZ` against
/// `b.minY` and so on). That is harmless for the template structures, whose
/// chunk boxes span the whole build height so the transposed comparisons hold
/// anyway, but an end city reaches z = 150 while its y stays under 130, so the
/// transposed test answers "no overlap" for a chunk the structure is inside.
/// This file keeps the pair-wise form.
pub(crate) fn bb_overlaps(a: &BB, b: &BB) -> bool {
    a[3] >= b[0] && a[0] <= b[3] && a[4] >= b[1] && a[1] <= b[4] && a[5] >= b[2] && a[2] <= b[5]
}

/// `StructurePiece.findCollisionPiece`.
fn find_collision_piece<'a>(list: &'a [TemplatePiece], bb: &BB) -> Option<&'a TemplatePiece> {
    list.iter().find(|piece| bb_overlaps(&piece.bb, bb))
}

struct EndCityGenerator<'a> {
    templates: &'a Templates,
    reg: &'a BReg,
    rng: &'a mut Lcg,
    ship_created: bool,
}

impl EndCityGenerator<'_> {
    fn attach(
        &self,
        list: &mut Vec<TemplatePiece>,
        parent: &TemplatePiece,
        offset: Pos,
        name: &str,
        overwrite: bool,
    ) -> Result<TemplatePiece, &'static str> {
        self.attach_rotated(list, parent, offset, name, Rotation::None, overwrite)
    }

    /// `EndCityPieces.addPiece`: the child origin is the parent origin plus
    /// the offset in the parent's frame, even when the child turns.
    fn attach_rotated(
        &self,
        list: &mut Vec<TemplatePiece>,
        parent: &TemplatePiece,
        offset: Pos,
        name: &str,
        turn: Rotation,
        overwrite: bool,
    ) -> Result<TemplatePiece, &'static str> {
        let delta = zero_pos(offset, parent.mirror, parent.rotation, (0, 0, 0));
        let pos = add_pos(parent.pos, delta);
        let rotation = rotate_rotation(parent.rotation, turn);
        let piece = end_city_piece(self.templates, self.reg, name, pos, rotation, overwrite)?;
        list.push(piece.clone());
        Ok(piece)
    }

    /// Generate into a scratch list, give the batch one `nextInt()` depth,
    /// and reject it if it collides with an accepted piece of another depth.
    /// The RNG and ship flag are not rolled back on rejection.
    fn recursive_children(
        &mut self,
        section: EndCitySection,
        depth: i32,
        parent: &TemplatePiece,
        list: &mut Vec<TemplatePiece>,
    ) -> bool {
        if depth > END_CITY_MAX_GEN_DEPTH {
            return false;
        }
        let mut scratch = Vec::new();
        let result = match section {
            EndCitySection::HouseTower(offset) => {
                self.house_tower(depth, parent, offset, &mut scratch)
            }
            EndCitySection::Tower => self.tower(depth, parent, &mut scratch),
            EndCitySection::TowerBridge => self.tower_bridge(depth, parent, &mut scratch),
            EndCitySection::FatTower => self.fat_tower(depth, parent, &mut scratch),
        };
        if result.is_err() {
            return false;
        }
        let stamp = self.rng.next_i32();
        for candidate in &mut scratch {
            candidate.gen_depth = stamp;
            if find_collision_piece(list, &candidate.bb)
                .is_some_and(|existing| existing.gen_depth != parent.gen_depth)
            {
                return false;
            }
        }
        list.append(&mut scratch);
        true
    }

    fn house_tower(
        &mut self,
        depth: i32,
        parent: &TemplatePiece,
        offset: Pos,
        list: &mut Vec<TemplatePiece>,
    ) -> Result<(), &'static str> {
        let mut current = self.attach(list, parent, offset, "base_floor", true)?;
        let choice = self.rng.next_bound(3);
        if choice == 0 {
            self.attach(list, &current, (-1, 4, -1), "base_roof", true)?;
            return Ok(());
        }
        current = self.attach(list, &current, (-1, 0, -1), "second_floor_2", false)?;
        let (roof, overwrite) = if choice == 1 {
            ("second_roof", false)
        } else {
            current = self.attach(list, &current, (-1, 4, -1), "third_floor_2", false)?;
            ("third_roof", true)
        };
        let roof = self.attach(list, &current, (-1, 8, -1), roof, overwrite)?;
        self.recursive_children(EndCitySection::Tower, depth + 1, &roof, list);
        Ok(())
    }

    fn tower(
        &mut self,
        depth: i32,
        parent: &TemplatePiece,
        list: &mut Vec<TemplatePiece>,
    ) -> Result<(), &'static str> {
        let offset = (3 + self.rng.next_bound(2), -3, 3 + self.rng.next_bound(2));
        let base = self.attach(list, parent, offset, "tower_base", true)?;
        let mut current = self.attach(list, &base, (0, 7, 0), "tower_piece", true)?;
        let mut bridge = (self.rng.next_bound(3) == 0).then(|| current.clone());
        let count = 1 + self.rng.next_bound(3);
        for i in 0..count {
            current = self.attach(list, &current, (0, 4, 0), "tower_piece", true)?;
            if i < count - 1 && self.rng.next_bool() {
                bridge = Some(current.clone());
            }
        }
        if let Some(bridge) = bridge {
            self.tower_bridges(depth, &bridge, list, &END_CITY_TOWER_BRIDGES)?;
        } else if depth != 7 {
            return self.require_child(EndCitySection::FatTower, depth + 1, &current, list);
        }
        self.attach(list, &current, (-1, 4, -1), "tower_top", true)?;
        Ok(())
    }

    fn tower_bridges(
        &mut self,
        depth: i32,
        parent: &TemplatePiece,
        list: &mut Vec<TemplatePiece>,
        bridges: &[(Rotation, Pos)],
    ) -> Result<(), &'static str> {
        for &(turn, offset) in bridges {
            if self.rng.next_bool() {
                let end = self.attach_rotated(list, parent, offset, "bridge_end", turn, true)?;
                self.recursive_children(EndCitySection::TowerBridge, depth + 1, &end, list);
            }
        }
        Ok(())
    }

    fn tower_bridge(
        &mut self,
        depth: i32,
        parent: &TemplatePiece,
        list: &mut Vec<TemplatePiece>,
    ) -> Result<(), &'static str> {
        let length = self.rng.next_bound(4) + 1;
        let mut current = self.attach(list, parent, (0, 0, -4), "bridge_piece", true)?;
        current.gen_depth = -1;
        let mut rise = 0;
        for _ in 0..length {
            let (name, run, next_rise) = if self.rng.next_bool() {
                ("bridge_piece", -4, 0)
            } else if self.rng.next_bool() {
                ("bridge_steep_stairs", -4, 4)
            } else {
                ("bridge_gentle_stairs", -8, 4)
            };
            current = self.attach(list, &current, (0, rise, run), name, true)?;
            rise = next_rise;
        }
        if !self.ship_created && self.rng.next_bound(10 - depth) == 0 {
            self.add_ship(&current, rise, list)?;
        } else {
            let landing = EndCitySection::HouseTower((-3, rise + 1, -11));
            self.require_child(landing, depth + 1, &current, list)?;
        }
        self.attach_rotated(
            list,
            &current,
            (4, rise, 0),
            "bridge_end",
            Rotation::Clockwise180,
            true,
        )?;
        // The end piece is already stored; update the stored copy.
        if let Some(end) = list.last_mut() {
            end.gen_depth = -1;
        }
        Ok(())
    }

    fn add_ship(
        &mut self,
        parent: &TemplatePiece,
        rise: i32,
        list: &mut Vec<TemplatePiece>,
    ) -> Result<(), &'static str> {
        let offset = (
            -8 + self.rng.next_bound(8),
            rise,
            -70 + self.rng.next_bound(10),
        );
        self.attach(list, parent, offset, "ship", true)?;
        self.ship_created = true;
        Ok(())
    }

    fn fat_tower(
        &mut self,
        depth: i32,
        parent: &TemplatePiece,
        list: &mut Vec<TemplatePiece>,
    ) -> Result<(), &'static str> {
        let base = self.attach(list, parent, (-3, 4, -3), "fat_tower_base", true)?;
        let mut current = self.attach(list, &base, (0, 4, 0), "fat_tower_middle", true)?;
        for _ in 0..2 {
            if self.rng.next_bound(3) == 0 {
                break;
            }
            current = self.attach(list, &current, (0, 8, 0), "fat_tower_middle", true)?;
            self.tower_bridges(depth, &current, list, &END_CITY_FAT_TOWER_BRIDGES)?;
        }
        self.attach(list, &current, (-2, 8, -2), "fat_tower_top", true)?;
        Ok(())
    }

    fn require_child(
        &mut self,
        section: EndCitySection,
        depth: i32,
        parent: &TemplatePiece,
        list: &mut Vec<TemplatePiece>,
    ) -> Result<(), &'static str> {
        self.recursive_children(section, depth, parent, list)
            .then_some(())
            .ok_or("end city child section rejected")
    }
}

/// `EndCityPieces.startHouseTower`: the four `init()` calls reset the shared
/// `shipCreated` flag, then the fixed base/second/third stack is laid down and
/// the tower recursion starts.
pub(crate) fn start_house_tower(
    templates: &Templates,
    reg: &BReg,
    pos: Pos,
    rotation: Rotation,
    list: &mut Vec<TemplatePiece>,
    rng: &mut Lcg,
) -> Result<(), &'static str> {
    let mut generator = EndCityGenerator {
        templates,
        reg,
        rng,
        ship_created: false,
    };
    let mut current = end_city_piece(templates, reg, "base_floor", pos, rotation, true)?;
    list.push(current.clone());
    for (offset, name, overwrite) in [
        ((-1, 0, -1), "second_floor_1", false),
        ((-1, 4, -1), "third_floor_1", false),
        ((-1, 8, -1), "third_roof", true),
    ] {
        current = generator.attach(list, &current, offset, name, overwrite)?;
    }
    generator.recursive_children(EndCitySection::Tower, 1, &current, list);
    Ok(())
}

/// `EndCityStructure.findGenerationPoint`.
///
/// One `Rotation.getRandom` draw, then the four base-height reads. The pieces
/// are built lazily so the caller's intermediate RNG probe lands between the
/// two, exactly as native's `GenerationStub` consumer does.
pub(crate) fn find_end_city_pieces(
    world: &mut dyn TemplateWorld,
    cx: i32,
    cz: i32,
    rng: &mut Lcg,
) -> Option<(Pos, EndCityMansionPieceBuilder)> {
    let rotation = random_rotation(&mut |bound| rng.next_bound(bound));
    let pos = lowest_y_in_5by5_box_offset_7(world, cx, cz, rotation);
    if pos.1 < 60 {
        return None;
    }
    let builder: EndCityMansionPieceBuilder = Box::new(move |_world, templates, rng, reg| {
        let mut pieces = Vec::new();
        let _ = start_house_tower(templates, reg, pos, rotation, &mut pieces, rng);
        pieces
    });
    Some((pos, builder))
}

// ---------------------------------------------------------------- mansion grid

/// `WoodlandMansionPieces.SimpleGrid`: an `int[width][height]` with an
/// out-of-bounds default, which is what makes the mansion layout's edge probes
/// cheap (`get(x, y)` off the edge is the "outside" value).
#[derive(Clone, Debug)]
pub(crate) struct SimpleGrid {
    grid: Vec<Vec<i32>>,
    pub(crate) width: i32,
    pub(crate) height: i32,
    value_if_outside: i32,
}

impl SimpleGrid {
    pub(crate) fn new(width: i32, height: i32, value_if_outside: i32) -> Self {
        SimpleGrid {
            grid: vec![vec![0; height as usize]; width as usize],
            width,
            height,
            value_if_outside,
        }
    }
    pub(crate) fn set(&mut self, x: i32, y: i32, value: i32) {
        if x >= 0 && x < self.width && y >= 0 && y < self.height {
            self.grid[x as usize][y as usize] = value;
        }
    }
    pub(crate) fn set_box(&mut self, x1: i32, y1: i32, x2: i32, y2: i32, value: i32) {
        for y in y1..=y2 {
            for x in x1..=x2 {
                self.set(x, y, value);
            }
        }
    }
    pub(crate) fn get(&self, x: i32, y: i32) -> i32 {
        if x >= 0 && x < self.width && y >= 0 && y < self.height {
            self.grid[x as usize][y as usize]
        } else {
            self.value_if_outside
        }
    }
    /// `SimpleGrid.setif`: only overwrite an exact match.
    pub(crate) fn set_if(&mut self, x: i32, y: i32, expected: i32, value: i32) {
        if self.get(x, y) == expected {
            self.set(x, y, value);
        }
    }
    pub(crate) fn edges_to(&self, x: i32, y: i32, value: i32) -> bool {
        self.get(x - 1, y) == value
            || self.get(x + 1, y) == value
            || self.get(x, y + 1) == value
            || self.get(x, y - 1) == value
    }

    fn cells(&self) -> impl Iterator<Item = (i32, i32)> {
        let width = self.width;
        (0..self.height).flat_map(move |y| (0..width).map(move |x| (x, y)))
    }

    fn block_all(&mut self) {
        self.set_box(0, 0, self.width - 1, self.height - 1, GRID_BLOCKED);
    }

    fn room_id(&self, x: i32, y: i32) -> i32 {
        self.get(x, y) & ROOM_ID_MASK
    }

    /// The first horizontal neighbour with the same room id.
    fn room_direction(&self, cell: (i32, i32), id: i32) -> Option<Dir> {
        HORIZONTAL.into_iter().find(|&side| {
            let (x, y) = grid_step(cell, side);
            self.room_id(x, y) == id
        })
    }
}

fn grid_step((x, y): (i32, i32), side: Dir) -> (i32, i32) {
    (x + side.step_x(), y + side.step_z())
}

/// `MansionGrid` cell values.
pub(crate) const GRID_CLEAR: i32 = 0;
pub(crate) const GRID_CORRIDOR: i32 = 1;
pub(crate) const GRID_ROOM: i32 = 2;
pub(crate) const GRID_START_ROOM: i32 = 3;
pub(crate) const GRID_BLOCKED: i32 = 5;
/// Room-record bits, all in the low 32 bits of a `floorRooms` cell.
pub(crate) const ROOM_1X1: i32 = 65536;
pub(crate) const ROOM_1X2: i32 = 131072;
pub(crate) const ROOM_2X2: i32 = 262144;
pub(crate) const ROOM_ORIGIN_FLAG: i32 = 1048576;
pub(crate) const ROOM_DOOR_FLAG: i32 = 2097152;
pub(crate) const ROOM_STAIRS_FLAG: i32 = 4194304;
pub(crate) const ROOM_CORRIDOR_FLAG: i32 = 8388608;
pub(crate) const ROOM_TYPE_MASK: i32 = 983040;
pub(crate) const ROOM_ID_MASK: i32 = 65535;

/// `WoodlandMansionPieces.MansionGrid`.
pub(crate) struct MansionGrid {
    pub(crate) base_grid: SimpleGrid,
    pub(crate) third_floor_grid: SimpleGrid,
    pub(crate) floor_rooms: [SimpleGrid; 3],
    pub(crate) entrance_x: i32,
    pub(crate) entrance_y: i32,
}

/// `MansionGrid.isHouse`.
pub(crate) fn grid_is_house(grid: &SimpleGrid, x: i32, y: i32) -> bool {
    let value = grid.get(x, y);
    value == 1 || value == 2 || value == 3 || value == 4
}

impl MansionGrid {
    pub(crate) fn build(rng: &mut Lcg) -> Self {
        let entrance_x = 7;
        let entrance_y = 4;
        let base_grid = mansion_base_grid(rng, entrance_x, entrance_y);
        let new_grid = || SimpleGrid::new(base_grid.width, base_grid.height, GRID_BLOCKED);
        let mut floor_rooms = std::array::from_fn(|_| new_grid());
        for rooms in &mut floor_rooms[..2] {
            identify_rooms(&base_grid, rooms, rng);
        }
        for rooms in &mut floor_rooms[..2] {
            rooms.set_box(
                entrance_x + 1,
                entrance_y,
                entrance_x + 1,
                entrance_y + 1,
                ROOM_CORRIDOR_FLAG,
            );
        }
        let mut third_floor_grid = new_grid();
        setup_third_floor(&mut third_floor_grid, &mut floor_rooms, &base_grid, rng);
        identify_rooms(&third_floor_grid, &mut floor_rooms[2], rng);
        MansionGrid {
            base_grid,
            third_floor_grid,
            floor_rooms,
            entrance_x,
            entrance_y,
        }
    }
}

/// Seed the entrance and carve its four westward corridors in native order.
fn mansion_base_grid(rng: &mut Lcg, x: i32, y: i32) -> SimpleGrid {
    let mut grid = SimpleGrid::new(11, 11, GRID_BLOCKED);
    for (dx1, dy1, dx2, dy2, cell) in [
        (0, 0, 1, 1, GRID_START_ROOM),
        (-1, 0, -1, 1, GRID_ROOM),
        (2, -2, 3, 3, GRID_BLOCKED),
        (1, -2, 1, -1, GRID_CORRIDOR),
        (1, 2, 1, 3, GRID_CORRIDOR),
        (-1, -1, -1, -1, GRID_CORRIDOR),
        (-1, 2, -1, 2, GRID_CORRIDOR),
    ] {
        grid.set_box(x + dx1, y + dy1, x + dx2, y + dy2, cell);
    }
    grid.set_box(0, 0, 11, 1, GRID_BLOCKED);
    grid.set_box(0, 9, 11, 11, GRID_BLOCKED);
    for (dx, dy, depth) in [(0, -2, 6), (0, 3, 6), (-2, -1, 3), (-2, 2, 3)] {
        recursive_corridor(&mut grid, rng, x + dx, y + dy, Dir::West, depth);
    }
    while clean_edges(&mut grid) {}
    grid
}

/// `MansionGrid.recursiveCorridor`.
fn recursive_corridor(
    grid: &mut SimpleGrid,
    rng: &mut Lcg,
    x: i32,
    y: i32,
    direction: Dir,
    depth: i32,
) {
    if depth <= 0 {
        return;
    }
    grid.set(x, y, GRID_CORRIDOR);
    grid.set_if(
        x + direction.step_x(),
        y + direction.step_z(),
        GRID_CLEAR,
        GRID_CORRIDOR,
    );
    let mut placed = None;
    for _ in 0..8 {
        let candidate = BY_2D_DATA[rng.next_bound(4) as usize];
        if candidate == direction.opposite() {
            continue;
        }
        if candidate == Dir::East && rng.next_bool() {
            continue;
        }
        let step_x = x + direction.step_x();
        let step_z = y + direction.step_z();
        if grid.get(step_x + candidate.step_x(), step_z + candidate.step_z()) == GRID_CLEAR
            && grid.get(
                step_x + candidate.step_x() * 2,
                step_z + candidate.step_z() * 2,
            ) == GRID_CLEAR
        {
            placed = Some((
                x + direction.step_x() + candidate.step_x(),
                y + direction.step_z() + candidate.step_z(),
                candidate,
            ));
            break;
        }
    }
    if let Some((next_x, next_y, next_direction)) = placed {
        recursive_corridor(grid, rng, next_x, next_y, next_direction, depth - 1);
    }
    let clockwise = direction.clock_wise();
    let counter = direction.counter_clock_wise();
    grid.set_if(
        x + clockwise.step_x(),
        y + clockwise.step_z(),
        GRID_CLEAR,
        GRID_ROOM,
    );
    grid.set_if(
        x + counter.step_x(),
        y + counter.step_z(),
        GRID_CLEAR,
        GRID_ROOM,
    );
    grid.set_if(
        x + direction.step_x() + clockwise.step_x(),
        y + direction.step_z() + clockwise.step_z(),
        GRID_CLEAR,
        GRID_ROOM,
    );
    grid.set_if(
        x + direction.step_x() + counter.step_x(),
        y + direction.step_z() + counter.step_z(),
        GRID_CLEAR,
        GRID_ROOM,
    );
    grid.set_if(
        x + direction.step_x() * 2,
        y + direction.step_z() * 2,
        GRID_CLEAR,
        GRID_ROOM,
    );
    grid.set_if(
        x + clockwise.step_x() * 2,
        y + clockwise.step_z() * 2,
        GRID_CLEAR,
        GRID_ROOM,
    );
    grid.set_if(
        x + counter.step_x() * 2,
        y + counter.step_z() * 2,
        GRID_CLEAR,
        GRID_ROOM,
    );
}

/// `MansionGrid.cleanEdges`: fill a clear cell flanked by three houses, or by
/// two houses with at most one diagonal house.
fn clean_edges(grid: &mut SimpleGrid) -> bool {
    let mut changed = false;
    for y in 0..grid.height {
        for x in 0..grid.width {
            if grid.get(x, y) != GRID_CLEAR {
                continue;
            }
            let mut houses = 0;
            houses += i32::from(grid_is_house(grid, x + 1, y));
            houses += i32::from(grid_is_house(grid, x - 1, y));
            houses += i32::from(grid_is_house(grid, x, y + 1));
            houses += i32::from(grid_is_house(grid, x, y - 1));
            if houses >= 3 {
                grid.set(x, y, GRID_ROOM);
                changed = true;
            } else if houses == 2 {
                let mut diagonals = 0;
                diagonals += i32::from(grid_is_house(grid, x + 1, y + 1));
                diagonals += i32::from(grid_is_house(grid, x - 1, y + 1));
                diagonals += i32::from(grid_is_house(grid, x + 1, y - 1));
                diagonals += i32::from(grid_is_house(grid, x - 1, y - 1));
                if diagonals <= 1 {
                    grid.set(x, y, GRID_ROOM);
                    changed = true;
                }
            }
        }
    }
    changed
}

/// `MansionGrid.setupThirdFloor`: pick a 1x2 room with a door as the stair
/// landing, block every non-house cell, and carve the third floor from it.
fn setup_third_floor(
    third_floor_grid: &mut SimpleGrid,
    floor_rooms: &mut [SimpleGrid; 3],
    base_grid: &SimpleGrid,
    rng: &mut Lcg,
) {
    let mut candidates: Vec<(i32, i32)> = Vec::new();
    let second_floor = &floor_rooms[1];
    for y in 0..third_floor_grid.height {
        for x in 0..third_floor_grid.width {
            let value = second_floor.get(x, y);
            let room_type = value & ROOM_TYPE_MASK;
            if room_type == ROOM_1X2 && (value & ROOM_DOOR_FLAG) == ROOM_DOOR_FLAG {
                candidates.push((x, y));
            }
        }
    }
    if candidates.is_empty() {
        third_floor_grid.block_all();
        return;
    }
    let (origin_x, origin_y) = candidates[rng.next_bound(candidates.len() as i32) as usize];
    let value = second_floor.get(origin_x, origin_y);
    floor_rooms[1].set(origin_x, origin_y, value | ROOM_STAIRS_FLAG);
    let id = value & ROOM_ID_MASK;
    let Some(direction) = floor_rooms[1].room_direction((origin_x, origin_y), id) else {
        third_floor_grid.block_all();
        floor_rooms[1].set(origin_x, origin_y, value);
        return;
    };
    let door_x = origin_x + direction.step_x();
    let door_y = origin_y + direction.step_z();
    for y in 0..third_floor_grid.height {
        for x in 0..third_floor_grid.width {
            if !grid_is_house(base_grid, x, y) {
                third_floor_grid.set(x, y, GRID_BLOCKED);
            } else if x == origin_x && y == origin_y {
                third_floor_grid.set(x, y, GRID_START_ROOM);
            } else if x == door_x && y == door_y {
                third_floor_grid.set(x, y, GRID_START_ROOM);
                floor_rooms[2].set(x, y, ROOM_CORRIDOR_FLAG);
            }
        }
    }
    let mut directions: Vec<Dir> = Vec::new();
    for candidate in HORIZONTAL {
        if third_floor_grid.get(door_x + candidate.step_x(), door_y + candidate.step_z())
            == GRID_CLEAR
        {
            directions.push(candidate);
        }
    }
    if directions.is_empty() {
        third_floor_grid.block_all();
        floor_rooms[1].set(origin_x, origin_y, value);
        return;
    }
    let direction = directions[rng.next_bound(directions.len() as i32) as usize];
    recursive_corridor(
        third_floor_grid,
        rng,
        door_x + direction.step_x(),
        door_y + direction.step_z(),
        direction,
        4,
    );
    while clean_edges(third_floor_grid) {}
}

/// `Util.shuffle` — descending `Collections.swap(i - 1, nextInt(i))`.
pub(crate) fn util_shuffle<T>(list: &mut [T], rng: &mut Lcg) {
    for i in (2..=list.len()).rev() {
        let j = rng.next_bound(i as i32) as usize;
        list.swap(i - 1, j);
    }
}

/// `MansionGrid.identifyRooms`: turn every clear cell of `base` inside a grid
/// room into a room record, choosing a 1x1/1x2/2x2 shape and an origin corner.
///
/// The two `nextBoolean` corner draws happen once per accepted room and are the
/// only draws besides the initial shuffle, so their order is the contract.
fn identify_rooms(base: &SimpleGrid, rooms: &mut SimpleGrid, rng: &mut Lcg) {
    let mut list: Vec<(i32, i32)> = Vec::new();
    for y in 0..base.height {
        for x in 0..base.width {
            if base.get(x, y) == GRID_ROOM {
                list.push((x, y));
            }
        }
    }
    util_shuffle(&mut list, rng);
    let mut id = 10;
    for (x, y) in list {
        if rooms.get(x, y) != 0 {
            continue;
        }
        let (mut min_x, mut max_x, mut min_y, mut max_y) = (x, x, y, y);
        let mut room_type = ROOM_1X1;
        if rooms.get(x + 1, y) == 0
            && rooms.get(x, y + 1) == 0
            && rooms.get(x + 1, y + 1) == 0
            && base.get(x + 1, y) == GRID_ROOM
            && base.get(x, y + 1) == GRID_ROOM
            && base.get(x + 1, y + 1) == GRID_ROOM
        {
            max_x += 1;
            max_y += 1;
            room_type = ROOM_2X2;
        } else if rooms.get(x - 1, y) == 0
            && rooms.get(x, y + 1) == 0
            && rooms.get(x - 1, y + 1) == 0
            && base.get(x - 1, y) == GRID_ROOM
            && base.get(x, y + 1) == GRID_ROOM
            && base.get(x - 1, y + 1) == GRID_ROOM
        {
            min_x -= 1;
            max_y += 1;
            room_type = ROOM_2X2;
        } else if rooms.get(x - 1, y) == 0
            && rooms.get(x, y - 1) == 0
            && rooms.get(x - 1, y - 1) == 0
            && base.get(x - 1, y) == GRID_ROOM
            && base.get(x, y - 1) == GRID_ROOM
            && base.get(x - 1, y - 1) == GRID_ROOM
        {
            min_x -= 1;
            min_y -= 1;
            room_type = ROOM_2X2;
        } else if rooms.get(x + 1, y) == 0 && base.get(x + 1, y) == GRID_ROOM {
            max_x += 1;
            room_type = ROOM_1X2;
        } else if rooms.get(x, y + 1) == 0 && base.get(x, y + 1) == GRID_ROOM {
            max_y += 1;
            room_type = ROOM_1X2;
        } else if rooms.get(x - 1, y) == 0 && base.get(x - 1, y) == GRID_ROOM {
            min_x -= 1;
            room_type = ROOM_1X2;
        } else if rooms.get(x, y - 1) == 0 && base.get(x, y - 1) == GRID_ROOM {
            min_y -= 1;
            room_type = ROOM_1X2;
        }
        let mut origin_x = if rng.next_bool() { min_x } else { max_x };
        let mut origin_y = if rng.next_bool() { min_y } else { max_y };
        let mut door_flag = ROOM_DOOR_FLAG;
        // The corner has to touch a corridor cell of the *base* grid: the room
        // record grid is still empty here, so probing it would never find one.
        if !base.edges_to(origin_x, origin_y, GRID_CORRIDOR) {
            origin_x = flip(origin_x, min_x, max_x);
            origin_y = flip(origin_y, min_y, max_y);
            if !base.edges_to(origin_x, origin_y, GRID_CORRIDOR) {
                origin_y = flip(origin_y, min_y, max_y);
                if !base.edges_to(origin_x, origin_y, GRID_CORRIDOR) {
                    origin_x = flip(origin_x, min_x, max_x);
                    origin_y = flip(origin_y, min_y, max_y);
                    if !base.edges_to(origin_x, origin_y, GRID_CORRIDOR) {
                        door_flag = 0;
                        origin_x = min_x;
                        origin_y = min_y;
                    }
                }
            }
        }
        for fill_y in min_y..=max_y {
            for fill_x in min_x..=max_x {
                if fill_x == origin_x && fill_y == origin_y {
                    rooms.set(
                        fill_x,
                        fill_y,
                        ROOM_ORIGIN_FLAG | door_flag | room_type | id,
                    );
                } else {
                    rooms.set(fill_x, fill_y, room_type | id);
                }
            }
        }
        id += 1;
    }
}

/// The `(x == min) ? max : min` corner flip the room-origin search uses.
fn flip(value: i32, min: i32, max: i32) -> i32 {
    if value == min {
        max
    } else {
        min
    }
}

// ---------------------------------------------------------------- mansion rooms

/// `FloorRoomCollection` — first floor, or second/third (which share every
/// implementation and differ only in the template name prefixes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MansionFloor {
    First,
    Second,
    Third,
}

impl MansionFloor {
    /// `get1x1`.
    pub(crate) fn one_by_one(self, rng: &mut Lcg) -> String {
        match self {
            Self::First => format!("1x1_a{}", rng.next_bound(5) + 1),
            Self::Second | Self::Third => format!("1x1_b{}", rng.next_bound(4) + 1),
        }
    }
    /// `get1x1Secret`.
    pub(crate) fn one_by_one_secret(self, rng: &mut Lcg) -> String {
        let _ = self;
        format!("1x1_as{}", rng.next_bound(4) + 1)
    }
    /// `get1x2SideEntrance`.
    pub(crate) fn one_by_two_side(self, rng: &mut Lcg, stairs: bool) -> String {
        match self {
            Self::First => format!("1x2_a{}", rng.next_bound(9) + 1),
            Self::Second | Self::Third => {
                if stairs {
                    "1x2_c_stairs".to_string()
                } else {
                    format!("1x2_c{}", rng.next_bound(4) + 1)
                }
            }
        }
    }
    /// `get1x2FrontEntrance`.
    pub(crate) fn one_by_two_front(self, rng: &mut Lcg, stairs: bool) -> String {
        match self {
            Self::First => format!("1x2_b{}", rng.next_bound(5) + 1),
            Self::Second | Self::Third => {
                if stairs {
                    "1x2_d_stairs".to_string()
                } else {
                    format!("1x2_d{}", rng.next_bound(5) + 1)
                }
            }
        }
    }
    /// `get1x2Secret`.
    pub(crate) fn one_by_two_secret(self, rng: &mut Lcg) -> String {
        match self {
            Self::First => format!("1x2_s{}", rng.next_bound(2) + 1),
            Self::Second | Self::Third => format!("1x2_se{}", rng.next_bound(1) + 1),
        }
    }
    /// `get2x2`.
    pub(crate) fn two_by_two(self, rng: &mut Lcg) -> String {
        match self {
            Self::First => format!("2x2_a{}", rng.next_bound(4) + 1),
            Self::Second | Self::Third => format!("2x2_b{}", rng.next_bound(5) + 1),
        }
    }
    /// `get2x2Secret`.
    pub(crate) fn two_by_two_secret(self, rng: &mut Lcg) -> String {
        let _ = rng;
        "2x2_s1".to_string()
    }
}

// ---------------------------------------------------------------- mansion placer

/// `BlockPos.above(n)`.
pub(crate) fn above(pos: Pos, n: i32) -> Pos {
    (pos.0, pos.1.wrapping_add(n), pos.2)
}

use Rotation::{Clockwise180, Clockwise90, Counterclockwise90, None as NoTurn};

/// `WoodlandMansionPieces.PlacementData` tracks the heading of the exterior walk.
struct MansionPlacement {
    rotation: Rotation,
    pos: Pos,
    wall_type: &'static str,
}

impl MansionPlacement {
    fn at(&self, offset: Pos) -> Pos {
        rotated_offset(self.pos, self.rotation, offset)
    }
}

/// `WoodlandMansionPieces.MansionPiecePlacer`.
struct MansionPlacer<'a> {
    templates: &'a Templates,
    reg: &'a BReg,
    rng: &'a mut Lcg,
    rotation: Rotation,
    start_x: i32,
    start_y: i32,
    pieces: Vec<TemplatePiece>,
}

enum RoomEntrance {
    Side,
    Front,
    Secret,
}

impl MansionPlacer<'_> {
    fn push(
        &mut self,
        name: &str,
        pos: Pos,
        rotation: Rotation,
        mirror: Mirror,
    ) -> Result<(), &'static str> {
        self.pieces.push(TemplatePiece::load(
            self.templates,
            self.reg,
            format!("minecraft:woodland_mansion/{name}"),
            pos,
            rotation,
            mirror,
            false, // Mansion pieces always write air.
        )?);
        Ok(())
    }

    fn push_local(
        &mut self,
        name: &str,
        anchor: Pos,
        offset: Pos,
        turn: Rotation,
    ) -> Result<(), &'static str> {
        self.push_local_mirrored(name, anchor, offset, turn, Mirror::None)
    }

    fn push_local_mirrored(
        &mut self,
        name: &str,
        anchor: Pos,
        offset: Pos,
        turn: Rotation,
        mirror: Mirror,
    ) -> Result<(), &'static str> {
        let pos = rotated_offset(anchor, self.rotation, offset);
        self.push(name, pos, rotate_rotation(self.rotation, turn), mirror)
    }

    /// The origin of an 8x8 grid cell, in the mansion's frame.
    fn cell_pos(&self, pos: Pos, (x, y): (i32, i32)) -> Pos {
        rotated_offset(
            pos,
            self.rotation,
            ((x - self.start_x) * 8, 0, 8 + (y - self.start_y) * 8),
        )
    }

    fn create_exterior(&mut self, pos: Pos, grid: &MansionGrid) -> Result<(), &'static str> {
        let mut ground = MansionPlacement {
            rotation: self.rotation,
            pos,
            wall_type: "wall_flat",
        };
        self.push(
            "entrance",
            ground.at((-9, 0, 0)),
            ground.rotation,
            Mirror::None,
        )?;
        ground.pos = ground.at((0, 0, 16));
        let mut windows = MansionPlacement {
            rotation: ground.rotation,
            pos: above(ground.pos, 8),
            wall_type: "wall_window",
        };
        let start = (self.start_x, self.start_y);
        let end = (self.start_x, grid.entrance_y);
        self.traverse_outer_walls(&mut ground, &grid.base_grid, start, end)?;
        self.traverse_outer_walls(&mut windows, &grid.base_grid, start, end)?;
        self.create_upper_walls(&ground, &grid.third_floor_grid)
    }

    fn create_upper_walls(
        &mut self,
        ground: &MansionPlacement,
        grid: &SimpleGrid,
    ) -> Result<(), &'static str> {
        // The first occupied row, taking its easternmost cell.
        let start = (0..grid.height)
            .flat_map(|y| (0..grid.width).rev().map(move |x| (x, y)))
            .find(|&(x, y)| grid_is_house(grid, x, y));
        if let Some(start) = start {
            let mut upper = MansionPlacement {
                rotation: ground.rotation,
                pos: self.cell_pos(above(ground.pos, 19), start),
                wall_type: "wall_window",
            };
            self.traverse_wall_piece(&mut upper)?;
            self.traverse_outer_walls(&mut upper, grid, start, start)?;
        }
        Ok(())
    }

    fn traverse_wall_piece(&mut self, data: &mut MansionPlacement) -> Result<(), &'static str> {
        self.push(
            data.wall_type,
            data.at((7, 0, 0)),
            data.rotation,
            Mirror::None,
        )?;
        data.pos = data.at((0, 0, 8));
        Ok(())
    }

    /// `traverseOuterWalls`: follow the edge until the end point and initial heading meet.
    fn traverse_outer_walls(
        &mut self,
        data: &mut MansionPlacement,
        grid: &SimpleGrid,
        (mut x, mut y): (i32, i32),
        end: (i32, i32),
    ) -> Result<(), &'static str> {
        let mut heading = Dir::South;
        loop {
            let (next_x, next_y) = grid_step((x, y), heading);
            let emit_wall = if !grid_is_house(grid, next_x, next_y) {
                data.pos = data.at((0, 0, -1));
                self.push("wall_corner", data.pos, data.rotation, Mirror::None)?;
                data.pos = data.at((6, 0, -7));
                data.rotation = rotate_rotation(data.rotation, Clockwise90);
                heading = heading.clock_wise();
                true
            } else {
                (x, y) = (next_x, next_y);
                let inner = heading.counter_clock_wise();
                let (inner_x, inner_y) = grid_step((x, y), inner);
                if grid_is_house(grid, inner_x, inner_y) {
                    data.pos = data.at((8, 0, 6));
                    data.rotation = rotate_rotation(data.rotation, Counterclockwise90);
                    heading = inner;
                    false
                } else {
                    true
                }
            };
            if (x, y) == end && heading == Dir::South {
                return Ok(());
            }
            if emit_wall {
                self.traverse_wall_piece(data)?;
            }
        }
    }

    /// Keep the native three passes: exposed slabs/fronts, upper-floor trim, then corners.
    fn create_roof(
        &mut self,
        pos: Pos,
        grid: &SimpleGrid,
        third: Option<&SimpleGrid>,
    ) -> Result<(), &'static str> {
        for cell in roof_cells(grid, third, false) {
            let anchor = self.cell_pos(pos, cell);
            self.push_local("roof", anchor, (0, 3, 0), NoTurn)?;
            self.add_roof_edges(anchor, grid, cell, false)?;
        }
        if let Some(third) = third {
            self.add_roof_trim(pos, grid, third)?;
        }
        self.create_roof_corners(pos, grid, third)
    }

    fn add_roof_edges(
        &mut self,
        anchor: Pos,
        grid: &SimpleGrid,
        cell: (i32, i32),
        trim: bool,
    ) -> Result<(), &'static str> {
        // (edge, roof_front offset, small_wall offset, turn)
        for (side, front, wall, turn) in [
            (Dir::East, (6, 0, 0), (7, 0, 0), NoTurn),
            (Dir::West, (0, 0, 7), (-1, 0, 6), Clockwise180),
            (Dir::North, (-1, 0, 0), (0, 0, -1), Counterclockwise90),
            (Dir::South, (6, 0, 6), (6, 0, 7), Clockwise90),
        ] {
            let (x, y) = grid_step(cell, side);
            if !grid_is_house(grid, x, y) {
                let (name, offset) = if trim {
                    ("small_wall", wall)
                } else {
                    ("roof_front", front)
                };
                self.push_local(name, anchor, offset, turn)?;
            }
        }
        Ok(())
    }

    fn add_roof_trim(
        &mut self,
        pos: Pos,
        grid: &SimpleGrid,
        third: &SimpleGrid,
    ) -> Result<(), &'static str> {
        for cell in roof_cells(grid, Some(third), true) {
            let anchor = self.cell_pos(pos, cell);
            self.add_roof_edges(anchor, grid, cell, true)?;
            for (edge, flank, offset, turn) in [
                (Dir::East, Dir::North, (7, 0, -2), NoTurn),
                (Dir::East, Dir::South, (8, 0, 7), Clockwise90),
                (Dir::West, Dir::North, (-2, 0, -1), Counterclockwise90),
                (Dir::West, Dir::South, (-1, 0, 8), Clockwise180),
            ] {
                let (ex, ey) = grid_step(cell, edge);
                let (fx, fy) = grid_step(cell, flank);
                if !grid_is_house(grid, ex, ey) && !grid_is_house(grid, fx, fy) {
                    self.push_local("small_wall_corner", anchor, offset, turn)?;
                }
            }
        }
        Ok(())
    }

    fn create_roof_corners(
        &mut self,
        pos: Pos,
        grid: &SimpleGrid,
        third: Option<&SimpleGrid>,
    ) -> Result<(), &'static str> {
        for cell in roof_cells(grid, third, false) {
            let anchor = self.cell_pos(pos, cell);
            // (edge, flank, (outer offset, turn), (inner offset, turn))
            for (edge, flank, outer, inner) in [
                (
                    Dir::East,
                    Dir::South,
                    ((6, 0, 6), NoTurn),
                    ((6, 0, 5), NoTurn),
                ),
                (
                    Dir::East,
                    Dir::North,
                    ((6, 0, 0), Counterclockwise90),
                    ((9, 0, -2), Clockwise90),
                ),
                (
                    Dir::West,
                    Dir::South,
                    ((0, 0, 6), Clockwise90),
                    ((-3, 0, 8), Counterclockwise90),
                ),
                (
                    Dir::West,
                    Dir::North,
                    ((0, 0, 0), Clockwise180),
                    ((0, 0, 1), Clockwise180),
                ),
            ] {
                let edge_cell = grid_step(cell, edge);
                if grid_is_house(grid, edge_cell.0, edge_cell.1) {
                    continue;
                }
                let (fx, fy) = grid_step(cell, flank);
                let (dx, dy) = grid_step(edge_cell, flank);
                if !grid_is_house(grid, fx, fy) {
                    self.push_local("roof_corner", anchor, outer.0, outer.1)?;
                } else if grid_is_house(grid, dx, dy) {
                    self.push_local("roof_inner_corner", anchor, inner.0, inner.1)?;
                }
            }
        }
        Ok(())
    }

    fn create_floor(
        &mut self,
        pos: Pos,
        layout: &MansionGrid,
        index: usize,
        floor: MansionFloor,
    ) -> Result<(), &'static str> {
        let (walk, height) = if floor == MansionFloor::Third {
            (&layout.third_floor_grid, 19)
        } else {
            (&layout.base_grid, 8 * index as i32)
        };
        let pos = above(pos, height);
        let rooms = &layout.floor_rooms[index];
        self.add_corridors(pos, walk, rooms, floor)?;
        self.add_floor_rooms(pos, walk, rooms, floor)
    }

    fn add_corridors(
        &mut self,
        pos: Pos,
        walk: &SimpleGrid,
        rooms: &SimpleGrid,
        floor: MansionFloor,
    ) -> Result<(), &'static str> {
        let (south, west) = if floor == MansionFloor::First {
            ("carpet_south_1", "carpet_west_1")
        } else {
            ("carpet_south_2", "carpet_west_2")
        };
        let carpets = [
            (Dir::North, "carpet_north", (1, 1, 0)),
            (Dir::East, "carpet_east", (5, 1, 1)),
            (Dir::South, south, (-1, 0, 5)),
            (Dir::West, west, (-1, 0, -1)),
        ];
        for cell in walk.cells() {
            if walk.get(cell.0, cell.1) != GRID_CORRIDOR {
                continue;
            }
            let anchor = self.cell_pos(pos, cell);
            self.push_local("corridor_floor", anchor, (0, 0, 0), NoTurn)?;
            for (side, name, offset) in carpets {
                let (x, y) = grid_step(cell, side);
                if walk.get(x, y) == GRID_CORRIDOR || rooms.get(x, y) & ROOM_CORRIDOR_FLAG != 0 {
                    self.push_local(name, anchor, offset, NoTurn)?;
                }
            }
        }
        Ok(())
    }

    fn add_floor_rooms(
        &mut self,
        pos: Pos,
        walk: &SimpleGrid,
        rooms: &SimpleGrid,
        floor: MansionFloor,
    ) -> Result<(), &'static str> {
        let mut doors = Vec::new();
        for cell in walk.cells() {
            let tile = walk.get(cell.0, cell.1);
            if tile != GRID_ROOM && !(floor == MansionFloor::Third && tile == GRID_START_ROOM) {
                continue;
            }
            let record = rooms.get(cell.0, cell.1);
            doors.clear();
            if record & ROOM_DOOR_FLAG != 0 {
                doors.extend(HORIZONTAL.into_iter().filter(|&side| {
                    let (x, y) = grid_step(cell, side);
                    walk.get(x, y) == GRID_CORRIDOR
                }));
            }
            let door = if !doors.is_empty() {
                Some(doors[self.rng.next_bound(doors.len() as i32) as usize])
            } else if record & ROOM_ORIGIN_FLAG != 0 {
                Some(Dir::Up)
            } else {
                None
            };
            let anchor = rotated_offset(self.cell_pos(pos, cell), self.rotation, (-1, 0, 0));
            self.add_room_walls(anchor, walk, rooms, cell, door, floor)?;
            self.fill_room(anchor, rooms, cell, door, floor)?;
        }
        Ok(())
    }

    fn add_room_walls(
        &mut self,
        anchor: Pos,
        walk: &SimpleGrid,
        rooms: &SimpleGrid,
        cell: (i32, i32),
        door: Option<Dir>,
        floor: MansionFloor,
    ) -> Result<(), &'static str> {
        let record = rooms.get(cell.0, cell.1);
        let stair_landing = floor == MansionFloor::Third
            && walk.get(cell.0, cell.1) == GRID_START_ROOM
            && record & ROOM_CORRIDOR_FLAG != 0;
        let (wall_name, door_name) = if floor == MansionFloor::First {
            ("indoors_wall_1", "indoors_door_1")
        } else {
            ("indoors_wall_2", "indoors_door_2")
        };
        for (side, offset, turn) in [
            (Dir::West, (0, 0, 0), NoTurn),
            (Dir::East, (8, 0, 0), NoTurn),
            (Dir::South, (7, 0, 7), Clockwise90),
            (Dir::North, (7, 0, -1), Clockwise90),
        ] {
            let (x, y) = grid_step(cell, side);
            // West/south also bound adjoining rooms. East/north only face corridors.
            let needs_wall = if matches!(side, Dir::West | Dir::South) {
                grid_is_house(walk, x, y) && rooms.room_id(x, y) != (record & ROOM_ID_MASK)
            } else {
                walk.get(x, y) == GRID_CORRIDOR && !stair_landing
            };
            if needs_wall {
                let name = if door == Some(side) {
                    door_name
                } else {
                    wall_name
                };
                self.push_local(name, anchor, offset, turn)?;
            }
        }
        Ok(())
    }

    fn fill_room(
        &mut self,
        anchor: Pos,
        rooms: &SimpleGrid,
        cell: (i32, i32),
        door: Option<Dir>,
        floor: MansionFloor,
    ) -> Result<(), &'static str> {
        let record = rooms.get(cell.0, cell.1);
        match (record & ROOM_TYPE_MASK, door) {
            (ROOM_1X1, _) => self.add_room_1x1(anchor, door, floor),
            (ROOM_1X2, Some(door)) => {
                if let Some(direction) = rooms.room_direction(cell, record & ROOM_ID_MASK) {
                    self.add_room_1x2(
                        anchor,
                        direction,
                        door,
                        floor,
                        record & ROOM_STAIRS_FLAG != 0,
                    )?;
                }
                Ok(())
            }
            (ROOM_2X2, Some(Dir::Up)) => {
                let name = floor.two_by_two_secret(self.rng);
                self.push_local(&name, anchor, (1, 0, 0), NoTurn)
            }
            (ROOM_2X2, Some(door)) => {
                let direction = door.clock_wise();
                let (x, y) = grid_step(cell, direction);
                let direction = if rooms.room_id(x, y) == (record & ROOM_ID_MASK) {
                    direction
                } else {
                    direction.opposite()
                };
                self.add_room_2x2(anchor, direction, door, floor)
            }
            _ => Ok(()),
        }
    }

    fn add_room_1x1(
        &mut self,
        pos: Pos,
        direction: Option<Dir>,
        floor: MansionFloor,
    ) -> Result<(), &'static str> {
        // Native draws a regular room even when it goes on to choose a secret room.
        let mut name = floor.one_by_one(self.rng);
        let turn = match direction {
            Some(Dir::East) => NoTurn,
            Some(Dir::North) => Counterclockwise90,
            Some(Dir::West) => Clockwise180,
            Some(Dir::South) => Clockwise90,
            _ => {
                name = floor.one_by_one_secret(self.rng);
                NoTurn
            }
        };
        let offset = relative_room_corner((1, 0, 0), turn, 7, 7);
        self.push_local(&name, pos, offset, turn)
    }

    fn add_room_1x2(
        &mut self,
        pos: Pos,
        room: Dir,
        door: Dir,
        floor: MansionFloor,
        stairs: bool,
    ) -> Result<(), &'static str> {
        use Dir::{East, North, South, Up, West};
        use Mirror::{FrontBack, LeftRight}; // Port variants for native indices 2 and 1.
        use RoomEntrance::{Front, Secret, Side};
        let (entrance, offset, turn, mirror) = match (door, room) {
            (East, South) => (Side, (1, 0, 0), NoTurn, Mirror::None),
            (East, North) => (Side, (1, 0, 6), NoTurn, LeftRight),
            (West, North) => (Side, (7, 0, 6), Clockwise180, Mirror::None),
            (West, South) => (Side, (7, 0, 0), NoTurn, FrontBack),
            (South, East) => (Side, (1, 0, 0), Clockwise90, LeftRight),
            (South, West) => (Side, (7, 0, 0), Clockwise90, Mirror::None),
            (North, West) => (Side, (7, 0, 6), Clockwise90, FrontBack),
            (North, East) => (Side, (1, 0, 6), Counterclockwise90, Mirror::None),
            (South, North) => (Front, (1, 0, -8), NoTurn, Mirror::None),
            (North, South) => (Front, (7, 0, 14), Clockwise180, Mirror::None),
            (West, East) => (Front, (15, 0, 0), Clockwise90, Mirror::None),
            (East, West) => (Front, (-7, 0, 6), Counterclockwise90, Mirror::None),
            (Up, East) => (Secret, (15, 0, 0), Clockwise90, Mirror::None),
            (Up, South) => (Secret, (1, 0, 0), NoTurn, Mirror::None),
            _ => return Ok(()),
        };
        let name = match entrance {
            Side => floor.one_by_two_side(self.rng, stairs),
            Front => floor.one_by_two_front(self.rng, stairs),
            Secret => floor.one_by_two_secret(self.rng),
        };
        self.push_local_mirrored(&name, pos, offset, turn, mirror)
    }

    fn add_room_2x2(
        &mut self,
        pos: Pos,
        room: Dir,
        door: Dir,
        floor: MansionFloor,
    ) -> Result<(), &'static str> {
        use Dir::{East, North, South, West};
        use Mirror::{FrontBack, LeftRight};
        let (offset, turn, mirror) = match (door, room) {
            (East, South) => ((-7, 0, 0), NoTurn, Mirror::None),
            (East, North) => ((-7, 0, 6), NoTurn, LeftRight),
            (North, East) => ((1, 0, 14), Counterclockwise90, Mirror::None),
            (North, West) => ((7, 0, 14), Counterclockwise90, LeftRight),
            (South, West) => ((7, 0, -8), Clockwise90, Mirror::None),
            (South, East) => ((1, 0, -8), Clockwise90, LeftRight),
            (West, North) => ((15, 0, 6), Clockwise180, Mirror::None),
            (West, South) => ((15, 0, 0), NoTurn, FrontBack),
            _ => return Ok(()),
        };
        let name = floor.two_by_two(self.rng);
        self.push_local_mirrored(&name, pos, offset, turn, mirror)
    }
}

fn roof_cells<'a>(
    grid: &'a SimpleGrid,
    third: Option<&'a SimpleGrid>,
    covered: bool,
) -> impl Iterator<Item = (i32, i32)> + 'a {
    grid.cells().filter(move |&(x, y)| {
        grid_is_house(grid, x, y)
            && third.is_some_and(|third| grid_is_house(third, x, y)) == covered
    })
}

/// `StructureTemplate.calculateRelativePosition(pos, mirror, rotation, x, y)`
/// with `mirror = NONE`: the 7x7 (or 8x8) room corner a room piece anchors at.
pub(crate) fn relative_room_corner(pos: Pos, rotation: Rotation, x: i32, y: i32) -> Pos {
    let x = x - 1;
    let y = y - 1;
    match rotation {
        Rotation::None => (pos.0.wrapping_add(0), pos.1, pos.2.wrapping_add(0)),
        Rotation::Clockwise90 => (pos.0.wrapping_add(y), pos.1, pos.2.wrapping_add(0)),
        Rotation::Clockwise180 => (pos.0.wrapping_add(x), pos.1, pos.2.wrapping_add(y)),
        Rotation::Counterclockwise90 => (pos.0.wrapping_add(0), pos.1, pos.2.wrapping_add(x)),
    }
}

/// `WoodlandMansionPieces.MansionPiecePlacer.createMansion`.
fn create_mansion(
    templates: &Templates,
    reg: &BReg,
    rng: &mut Lcg,
    pos: Pos,
    rotation: Rotation,
    grid: &MansionGrid,
) -> Result<Vec<TemplatePiece>, &'static str> {
    let mut placer = MansionPlacer {
        templates,
        reg,
        rng,
        rotation,
        start_x: grid.entrance_x + 1,
        start_y: grid.entrance_y + 1,
        pieces: Vec::new(),
    };
    placer.create_exterior(pos, grid)?;
    placer.create_roof(
        above(pos, 16),
        &grid.base_grid,
        Some(&grid.third_floor_grid),
    )?;
    placer.create_roof(above(pos, 27), &grid.third_floor_grid, None)?;
    for (index, floor) in [
        MansionFloor::First,
        MansionFloor::Second,
        MansionFloor::Third,
    ]
    .into_iter()
    .enumerate()
    {
        placer.create_floor(pos, grid, index, floor)?;
    }
    Ok(placer.pieces)
}

/// `WoodlandMansionPieces.generateMansion`.
pub(crate) fn generate_mansion(
    templates: &Templates,
    reg: &BReg,
    pos: Pos,
    rotation: Rotation,
    rng: &mut Lcg,
) -> Result<Vec<TemplatePiece>, &'static str> {
    let grid = MansionGrid::build(rng);
    create_mansion(templates, reg, rng, pos, rotation, &grid)
}

/// `WoodlandMansionStructure.findGenerationPoint`.
pub(crate) fn find_mansion_pieces(
    world: &mut dyn TemplateWorld,
    cx: i32,
    cz: i32,
    rng: &mut Lcg,
) -> Option<(Pos, EndCityMansionPieceBuilder)> {
    let rotation = random_rotation(&mut |bound| rng.next_bound(bound));
    let pos = lowest_y_in_5by5_box_offset_7(world, cx, cz, rotation);
    if pos.1 < 60 {
        return None;
    }
    let builder: EndCityMansionPieceBuilder = Box::new(move |_world, templates, rng, reg| {
        generate_mansion(templates, reg, pos, rotation, rng).unwrap_or_default()
    });
    Some((pos, builder))
}

// ---------------------------------------------------------------- chunk placement

/// `minecraft:chests/end_city_treasure` (`BuiltInLootTables.END_CITY_TREASURE`).
pub(crate) const END_CITY_TREASURE: &str = "minecraft:chests/end_city_treasure";
/// `minecraft:chests/woodland_mansion` (`BuiltInLootTables.WOODLAND_MANSION`).
pub(crate) const WOODLAND_MANSION_LOOT: &str = "minecraft:chests/woodland_mansion";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StructureFlavour {
    EndCity,
    Mansion,
}

fn merge_out(acc: &mut PlacementOut, part: PlacementOut) {
    acc.wrote |= part.wrote;
    acc.writes += part.writes;
    acc.be_events.extend(part.be_events);
    acc.entities.extend(part.entities);
}

/// The block entity the harness would hold at `pos` after the recorded events.
fn live_block_entity(out: &PlacementOut, pos: Pos) -> Option<&BlockEntityRecord> {
    let mut live = None;
    for event in &out.be_events {
        match event {
            BeEvent::Set(record) if record.pos == pos => live = Some(record),
            BeEvent::Clear(cleared) if *cleared == pos => live = None,
            _ => {}
        }
    }
    live
}

/// `RandomizableContainer.setBlockEntityLootTable`: only a live randomizable
/// container takes a loot table, and only then does the stream lose a `nextLong`.
fn end_city_chest_loot(out: &mut PlacementOut, pos: Pos, rng: &mut Xor) {
    let Some(record) = live_block_entity(out, pos) else {
        return;
    };
    let name = record.id.to_string();
    if !is_randomizable_container(&name) {
        return;
    }
    let seed = rng.next_i64();
    for event in out.be_events.iter_mut().rev() {
        if let BeEvent::Set(entry) = event {
            if entry.pos == pos && entry.id == "minecraft:chest" {
                entry.loot_table = Some(END_CITY_TREASURE.to_string());
                entry.loot_seed = Some(seed);
                return;
            }
        }
    }
}

/// `BlockState.isAir()`: the registry's own air flag, which `minecraft:air`,
/// `minecraft:cave_air` and `minecraft:void_air` all carry.
fn is_air_state(reg: &BReg, state: u32) -> bool {
    reg.is_air(state)
}

/// `StructurePiece.createChest` for the mansion's `Chest*` data markers: the
/// existing cell must not already be a chest, then the state is written and the
/// chest's loot table seed is drawn.
#[allow(clippy::too_many_arguments)]
fn mansion_create_chest<W: TemplateWorld>(
    world: &mut W,
    reg: &BReg,
    out: &mut PlacementOut,
    chunk_bb: &BB,
    rng: &mut Xor,
    pos: Pos,
    facing: Dir,
) {
    if !bb_inside(chunk_bb, pos) {
        return;
    }
    if reg.name_of(world.get(pos.0, pos.1, pos.2)) == "minecraft:chest" {
        return;
    }
    let Some(state) = reg.checked_state(
        "minecraft:chest",
        &[("facing".to_owned(), facing.name().to_owned())],
    ) else {
        return;
    };
    if !world.set(pos.0, pos.1, pos.2, state) {
        return;
    }
    out.writes += 1;
    let seed = rng.next_i64();
    out.be_events.push(BeEvent::Clear(pos));
    out.be_events.push(BeEvent::Set(BlockEntityRecord {
        pos,
        id: "minecraft:chest",
        loot_table: Some(WOODLAND_MANSION_LOOT.to_string()),
        loot_seed: Some(seed),
        items: None,
        spawner_entity: None,
    }));
}

/// `TemplateStructurePiece.handleDataMarker` for both structures, driven from
/// the raw palette's `minecraft:structure_block` cell with `mode: "DATA"`.
#[allow(clippy::too_many_arguments)]
fn handle_data_marker<W: TemplateWorld>(
    world: &mut W,
    reg: &BReg,
    out: &mut PlacementOut,
    flavour: StructureFlavour,
    metadata: &str,
    pos: Pos,
    rotation: Rotation,
    chunk_bb: &BB,
    rng: &mut Xor,
) {
    match flavour {
        StructureFlavour::EndCity => {
            end_city_marker(world, out, metadata, pos, rotation, chunk_bb, rng)
        }
        StructureFlavour::Mansion if metadata.starts_with("Chest") => {
            let facing = match metadata {
                "ChestWest" => Dir::West,
                "ChestEast" => Dir::East,
                "ChestNorth" => Dir::North,
                _ => Dir::South,
            };
            mansion_create_chest(
                world,
                reg,
                out,
                chunk_bb,
                rng,
                pos,
                rotate_direction(rotation, facing),
            );
        }
        StructureFlavour::Mansion => mansion_mob_marker(world, reg, out, metadata, pos),
    }
}

fn end_city_marker<W: TemplateWorld>(
    world: &W,
    out: &mut PlacementOut,
    metadata: &str,
    pos: Pos,
    rotation: Rotation,
    chunk_bb: &BB,
    rng: &mut Xor,
) {
    if metadata.starts_with("Chest") {
        let below = (pos.0, pos.1 - 1, pos.2);
        if bb_inside(chunk_bb, below) {
            end_city_chest_loot(out, below, rng);
        }
        return;
    }
    if !bb_inside(chunk_bb, pos) || !in_spawnable_bounds(world, pos) {
        return;
    }
    let (id, hanging) = if metadata.starts_with("Sentry") {
        ("minecraft:shulker", None)
    } else if metadata.starts_with("Elytra") {
        // The ship's frame carries one elytra and consumes no RNG.
        (
            "minecraft:item_frame",
            Some(HangingSpawn {
                facing: rotate_direction(rotation, Dir::South).data_3d(),
                item: Some(("minecraft:elytra".to_string(), 1)),
            }),
        )
    } else {
        return;
    };
    out.entities.push(EntitySpawn {
        pos,
        id: id.to_string(),
        hanging,
    });
}

fn mansion_mob_marker<W: TemplateWorld>(
    world: &mut W,
    reg: &BReg,
    out: &mut PlacementOut,
    metadata: &str,
    pos: Pos,
) {
    let (id, count) = match metadata {
        "Mage" => ("minecraft:evoker", 1),
        "Warrior" => ("minecraft:vindicator", 1),
        "Group of Allays" => ("minecraft:allay", world.level_next_bound(pos, 3) + 1),
        _ => return,
    };
    for _ in 0..count {
        out.entities.push(EntitySpawn {
            pos,
            id: id.to_string(),
            hanging: None,
        });
    }
    for _ in 0..count {
        if world.set(pos.0, pos.1, pos.2, reg.state_of("minecraft:air", None)) {
            out.writes += 1;
            out.be_events.push(BeEvent::Clear(pos));
        }
    }
}

/// `Level.isInSpawnableBounds`.
fn in_spawnable_bounds<W: TemplateWorld + ?Sized>(world: &W, pos: Pos) -> bool {
    let min_y = world.min_y();
    let max_y = min_y + world.height();
    pos.1 >= min_y
        && pos.1 < max_y
        && pos.0 >= -30_000_000
        && pos.2 >= -30_000_000
        && pos.0 < 30_000_000
        && pos.2 < 30_000_000
}

/// `TemplateStructurePiece.postProcess` for one piece: place the template with
/// the chunk box as clip, then deliver that chunk's data markers.
#[allow(clippy::too_many_arguments)]
fn place_template_piece<W: TemplateWorld>(
    world: &mut W,
    templates: &Templates,
    reg: &BReg,
    piece: &TemplatePiece,
    flavour: StructureFlavour,
    rng: &mut Xor,
    chunk_bb: &BB,
    anchor: Pos,
    world_seed: u64,
    out: &mut PlacementOut,
) -> Result<(), &'static str> {
    let template = templates.get(&piece.template, reg)?;
    let mut settings = piece.settings();
    settings.clip = Some(*chunk_bb);
    let part = place_in_world(
        world, &template, piece.pos, anchor, &settings, rng, world_seed, reg,
    )?;
    let wrote = part.wrote;
    merge_out(out, part);
    if !wrote {
        return Ok(());
    }
    for marker in filter_blocks(
        &template,
        piece.pos,
        &settings,
        "minecraft:structure_block",
        reg,
    ) {
        let Some(tag) = &marker.nbt else {
            continue;
        };
        if nbt_str(tag, "mode").as_deref() != Some("DATA") {
            continue;
        }
        let Some(metadata) = nbt_str(tag, "metadata") else {
            continue;
        };
        handle_data_marker(
            world,
            reg,
            out,
            flavour,
            &metadata,
            marker.pos,
            piece.rotation,
            chunk_bb,
            rng,
        );
    }
    Ok(())
}

/// `StructureStart.placeInChunk`: the piece list in order, each intersecting
/// piece's `postProcess`, then the structure's `afterPlace`.
#[expect(clippy::too_many_arguments, reason = "preserve the established placement and simulation call signature")]
pub(crate) fn place_chunk<W: TemplateWorld>(
    world: &mut W,
    templates: &Templates,
    pieces: &[TemplatePiece],
    flavour: StructureFlavour,
    rng: &mut Xor,
    chunk_bb: &BB,
    world_seed: u64,
    reg: &BReg,
) -> Result<PlacementOut, &'static str> {
    let mut out = PlacementOut::default();
    if pieces.is_empty() {
        return Ok(out);
    }
    let first = &pieces[0].bb;
    let anchor = (
        first[0] + (first[3] - first[0] + 1) / 2,
        first[1],
        first[2] + (first[5] - first[2] + 1) / 2,
    );
    for piece in pieces {
        if !bb_overlaps(&piece.bb, chunk_bb) {
            continue;
        }
        place_template_piece(
            world, templates, reg, piece, flavour, rng, chunk_bb, anchor, world_seed, &mut out,
        )?;
    }
    if flavour == StructureFlavour::Mansion {
        out.writes += mansion_after_place(world, pieces, chunk_bb, reg);
    }
    Ok(out)
}

/// `minecraft:end_city`'s per-chunk placement: `StructureStart.placeInChunk`
/// over the end city piece list.
#[allow(clippy::too_many_arguments)]
pub(crate) fn place_end_city_chunk<W: TemplateWorld>(
    world: &mut W,
    templates: &Templates,
    pieces: &[TemplatePiece],
    rng: &mut Xor,
    chunk_bb: &BB,
    world_seed: u64,
    reg: &BReg,
) -> Result<PlacementOut, &'static str> {
    place_chunk(
        world,
        templates,
        pieces,
        StructureFlavour::EndCity,
        rng,
        chunk_bb,
        world_seed,
        reg,
    )
}

/// `minecraft:mansion`'s per-chunk placement: the pieces, then the
/// `afterPlace` foundation pass.
#[allow(clippy::too_many_arguments)]
pub(crate) fn place_mansion_chunk<W: TemplateWorld>(
    world: &mut W,
    templates: &Templates,
    pieces: &[TemplatePiece],
    rng: &mut Xor,
    chunk_bb: &BB,
    world_seed: u64,
    reg: &BReg,
) -> Result<PlacementOut, &'static str> {
    place_chunk(
        world,
        templates,
        pieces,
        StructureFlavour::Mansion,
        rng,
        chunk_bb,
        world_seed,
        reg,
    )
}

/// `WoodlandMansionStructure.afterPlace`: fill the foundation under every
/// structural cell of this chunk with cobblestone down to sea level or the first
/// non-air block.
pub(crate) fn mansion_after_place<W: TemplateWorld>(
    world: &mut W,
    pieces: &[TemplatePiece],
    chunk_bb: &BB,
    reg: &BReg,
) -> usize {
    if pieces.is_empty() {
        return 0;
    }
    let mut assembled = pieces[0].bb;
    for piece in &pieces[1..] {
        bb_encapsulate(&mut assembled, &piece.bb);
    }
    let min_y = assembled[1];
    let sea_level = world.sea_level();
    let cobblestone = reg.state_of("minecraft:cobblestone", None);
    let mut written = 0;
    for x in chunk_bb[0]..=chunk_bb[3] {
        for z in chunk_bb[2]..=chunk_bb[5] {
            if min_y < world.min_y() || min_y >= world.min_y() + world.height() {
                continue;
            }
            if is_air_state(reg, world.get(x, min_y, z)) {
                continue;
            }
            if !bb_inside(&assembled, (x, min_y, z)) {
                continue;
            }
            if !pieces
                .iter()
                .any(|piece| bb_inside(&piece.bb, (x, min_y, z)))
            {
                continue;
            }
            let Some(mut y) = min_y.checked_sub(1) else {
                continue;
            };
            while y > sea_level.max(world.min_y()) {
                let state = world.get(x, y, z);
                if !is_air_state(reg, state) && !reg.is_liquid(state) {
                    break;
                }
                if world.set(x, y, z, cobblestone) {
                    written += 1;
                }
                y -= 1;
            }
        }
    }
    written
}

// ---------------------------------------------------------------- native fixture

#[cfg(all(test, megu_vanilla_data))]
mod end_city_mansion_tests {
    use super::*;
    use crate::block_registry::breg;
    use crate::generation_math::{sha256, Xor};
    use crate::json::{jarr, jint, jnum, jparse, jstr, J};
    use std::collections::HashMap;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn sha(bytes: &[u8]) -> String {
        hex(&sha256(bytes))
    }

    /// Java `DataOutputStream.writeUTF`.
    fn write_utf(out: &mut Vec<u8>, text: &str) {
        out.extend_from_slice(&(text.len() as u16).to_be_bytes());
        out.extend_from_slice(text.as_bytes());
    }

    /// The oracle's `ScriptedTerrain`, restricted to the dry-world fields the
    /// two structures use: a per-column `WORLD_SURFACE_WG` height, a top block
    /// at `height - 1`, fill below, air above, plus explicit cells.
    struct Scene {
        min_y: i32,
        height: i32,
        sea: i32,
        ws: HashMap<(i32, i32), i32>,
        cells: HashMap<Pos, u32>,
        air: u32,
        top: u32,
        fill: u32,
    }

    impl Scene {
        fn parse(terrain: Option<&J>, reg: &BReg) -> Self {
            let mut scene = Scene {
                min_y: -64,
                height: 384,
                sea: 63,
                ws: HashMap::new(),
                cells: HashMap::new(),
                air: reg.state_of("minecraft:air", None),
                top: reg.state_of("minecraft:stone", None),
                fill: reg.state_of("minecraft:stone", None),
            };
            let Some(terrain) = terrain else {
                return scene;
            };
            scene.min_y = jint(terrain, "min_y", scene.min_y);
            scene.height = jint(terrain, "height", scene.height);
            scene.sea = jint(terrain, "sea_level", scene.sea);
            if let Some(name) = jstr(terrain, "top_block") {
                scene.top = reg.state_of(&name, None);
            }
            if let Some(name) = jstr(terrain, "fill_block") {
                scene.fill = reg.state_of(&name, None);
            }
            if let Some(J::Obj(heights)) = terrain.get("heights") {
                for (key, value) in heights {
                    let (x, z) = key.split_once(',').expect("height column");
                    let x: i32 = x.parse().expect("height x");
                    let z: i32 = z.parse().expect("height z");
                    match value {
                        J::Num(h) => {
                            scene.ws.insert((x, z), *h as i32);
                        }
                        J::Obj(kind) => {
                            let kind = J::Obj(kind.clone());
                            scene.ws.insert((x, z), jint(&kind, "ws", 0));
                        }
                        _ => panic!("height column must be int or object"),
                    }
                }
            }
            if let Some(J::Obj(cells)) = terrain.get("cells") {
                for (key, value) in cells {
                    let mut parts = key.split(',');
                    let x: i32 = parts.next().expect("cell x").parse().expect("cell x");
                    let y: i32 = parts.next().expect("cell y").parse().expect("cell y");
                    let z: i32 = parts.next().expect("cell z").parse().expect("cell z");
                    let state = match value {
                        J::Str(name) => reg.state_of(name, None),
                        _ => panic!("cell must be a state name"),
                    };
                    scene.cells.insert((x, y, z), state);
                }
            }
            scene
        }

        fn base_height(&self, x: i32, z: i32) -> i32 {
            self.ws.get(&(x, z)).copied().unwrap_or(70)
        }

        fn state_at(&self, x: i32, y: i32, z: i32) -> u32 {
            if let Some(&state) = self.cells.get(&(x, y, z)) {
                return state;
            }
            let ws = self.base_height(x, z);
            if y >= ws {
                self.air
            } else if y == ws - 1 {
                self.top
            } else {
                self.fill
            }
        }
    }

    struct TestWorld {
        scene: Scene,
        writes: HashMap<Pos, u32>,
    }

    impl TestWorld {
        fn new(scene: Scene) -> Self {
            TestWorld {
                scene,
                writes: HashMap::new(),
            }
        }
    }

    impl TemplateWorld for TestWorld {
        fn get(&mut self, x: i32, y: i32, z: i32) -> u32 {
            if let Some(&state) = self.writes.get(&(x, y, z)) {
                return state;
            }
            self.scene.state_at(x, y, z)
        }
        fn set(&mut self, x: i32, y: i32, z: i32, state: u32) -> bool {
            if y < self.scene.min_y || y >= self.scene.min_y + self.scene.height {
                return false;
            }
            self.writes.insert((x, y, z), state);
            true
        }
        fn heightmap(&mut self, kind: HeightmapKind, x: i32, z: i32) -> i32 {
            let reg = breg().expect("block registry");
            let top = self.scene.min_y + self.scene.height - 1;
            let mut y = top;
            while y >= self.scene.min_y {
                let state = self.get(x, y, z);
                if reg.heightmap_counts(state, kind) {
                    return y + 1;
                }
                y -= 1;
            }
            self.scene.min_y
        }
        fn base_height(&mut self, kind: HeightmapKind, x: i32, z: i32) -> i32 {
            let _ = kind;
            self.scene.base_height(x, z)
        }
        fn sea_level(&self) -> i32 {
            self.scene.sea
        }
        fn min_y(&self) -> i32 {
            self.scene.min_y
        }
        fn height(&self) -> i32 {
            self.scene.height
        }
    }

    fn world_hash(reg: &BReg, writes: &HashMap<Pos, u32>) -> (usize, String) {
        let mut keys: Vec<Pos> = writes.keys().copied().collect();
        keys.sort_by_key(|&(x, y, z)| (x, y, z));
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(keys.len() as i32).to_be_bytes());
        for &(x, y, z) in &keys {
            write_utf(&mut bytes, &format!("{x},{y},{z}"));
            write_utf(&mut bytes, &reg.canonical_state(writes[&(x, y, z)]));
        }
        (keys.len(), sha(&bytes))
    }

    /// The live block entity per position, as `be_lines` in
    /// `template_structures.rs` builds it minus the container `Items` hash:
    /// neither structure's markers carry saved items.
    fn be_lines(events: &[BeEvent]) -> Vec<String> {
        let mut map: std::collections::BTreeMap<Pos, Option<BlockEntityRecord>> =
            Default::default();
        for event in events {
            match event {
                BeEvent::Set(record) => {
                    map.insert(record.pos, Some(record.clone()));
                }
                BeEvent::Clear(pos) => {
                    map.insert(*pos, None);
                }
            }
        }
        map.into_values().flatten()
            .map(|record| {
                format!(
                    "{},{},{}|{}|{}|{}",
                    record.pos.0,
                    record.pos.1,
                    record.pos.2,
                    record.id,
                    record.loot_table.clone().unwrap_or_default(),
                    if record.loot_table.is_some() {
                        record.loot_seed.unwrap_or(0)
                    } else {
                        0
                    },
                )
            })
            .collect()
    }

    fn hash_lines(lines: &[String]) -> String {
        if lines.is_empty() {
            "none".to_string()
        } else {
            sha(lines.join("\n").as_bytes())
        }
    }

    fn entity_lines(entities: &[EntitySpawn]) -> Vec<String> {
        let mut lines: Vec<String> = entities
            .iter()
            .map(|spawn| {
                format!(
                    "{}@{},{},{}",
                    spawn.id, spawn.pos.0, spawn.pos.1, spawn.pos.2
                )
            })
            .collect();
        lines.sort();
        lines
    }

    fn load_cases() -> &'static J {
        static CASES: std::sync::OnceLock<J> = std::sync::OnceLock::new();
        CASES.get_or_init(|| {
            let path = concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/data/end_city_mansion_cases_1211.json"
            );
            let bytes = std::fs::read(path).expect("end city / mansion cases");
            jparse(&bytes).expect("cases parse")
        })
    }

    fn short_name(template: &str) -> &str {
        template
            .rsplit_once('/')
            .map(|(_, name)| name)
            .unwrap_or(template)
    }

    fn geometry_hash(pieces: &[TemplatePiece]) -> String {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(pieces.len() as i32).to_be_bytes());
        for piece in pieces {
            write_utf(&mut bytes, short_name(&piece.template));
            for value in piece.bb {
                bytes.extend_from_slice(&value.to_be_bytes());
            }
            bytes.extend_from_slice(&(piece.rotation as i32).to_be_bytes());
            bytes.extend_from_slice(&(piece.mirror as i32).to_be_bytes());
            bytes.extend_from_slice(&(piece.gen_depth).to_be_bytes());
        }
        sha(&bytes)
    }

    fn row_failure(what: &str, got: impl std::fmt::Display, want: impl std::fmt::Display) {
        eprintln!("FAIL {what}: got={got} want={want}");
    }

    /// Runs one case end to end and reports every difference: admission, the
    /// stub position, the intermediate RNG probe, the piece tree (per piece),
    /// the placement writes, the block entities, the entity payloads and the
    /// post-placement probe.
    /// Renders one grid the way the fixture stores it: `;`-separated rows of
    /// `,`-separated cell values.
    fn grid_cells(grid: &SimpleGrid) -> String {
        let mut out = String::new();
        for y in 0..grid.height {
            if y > 0 {
                out.push(';');
            }
            for x in 0..grid.width {
                if x > 0 {
                    out.push(',');
                }
                out.push_str(&grid.get(x, y).to_string());
            }
        }
        out
    }

    /// Compares the two mansion layout grids cell by cell, so a divergence is
    /// named instead of arriving as a piece-list hash difference.
    fn compare_grids(what: &str, mine: &SimpleGrid, entrance: (i32, i32), row: &[&str]) -> usize {
        let want_entrance = (
            row[3].parse::<i32>().unwrap(),
            row[4].parse::<i32>().unwrap(),
        );
        let mut failures = 0usize;
        if entrance != want_entrance {
            row_failure(
                &format!("{what} entrance"),
                format!("{entrance:?}"),
                format!("{want_entrance:?}"),
            );
            failures += 1;
        }
        let cells = grid_cells(mine);
        if cells != row[5] {
            let want_rows: Vec<&str> = row[5].split(';').collect();
            let mine_rows: Vec<&str> = cells.split(';').collect();
            let mut reported = 0;
            for (y, (a, b)) in mine_rows.iter().zip(want_rows.iter()).enumerate() {
                if a == b {
                    continue;
                }
                let a_cells: Vec<&str> = a.split(',').collect();
                let b_cells: Vec<&str> = b.split(',').collect();
                for (x, (got, want)) in a_cells.iter().zip(b_cells.iter()).enumerate() {
                    if got != want && reported < 12 {
                        row_failure(&format!("{what} cell ({x},{y})"), got, want);
                        reported += 1;
                    }
                }
            }
            failures += 1;
        }
        failures
    }

    fn run_case(
        case: &J,
        row: &[&str],
        piece_rows: &[&[&str]],
        grid_rows: &std::collections::BTreeMap<String, Vec<&str>>,
        be_rows: &std::collections::BTreeMap<String, Vec<&str>>,
        reg: &BReg,
        templates: &Templates,
    ) -> usize {
        let id = jstr(case, "id").expect("id");
        let kind = jstr(case, "kind").expect("kind");
        let cx = jint(case, "cx", 0);
        let cz = jint(case, "cz", 0);
        let world_seed = jnum(case, "world_seed").unwrap_or(0.0) as i64 as u64;
        let post_seed = jnum(case, "post_seed").unwrap_or(0.0) as i64 as u64;
        let scene = Scene::parse(case.get("terrain"), reg);
        let mut world = TestWorld::new(scene);
        let mut gen = Lcg::for_large_feature(world_seed, cx, cz);
        let found = match kind.as_str() {
            "end_city" => find_end_city_pieces(&mut world, cx, cz, &mut gen),
            "mansion" => find_mansion_pieces(&mut world, cx, cz, &mut gen),
            other => panic!("unknown case kind {other}"),
        };
        let gen_probe = gen.next_i64();
        // `layout` rows pin admission, the stub position, the piece tree and the
        // generation stream; their placement columns are zero because the
        // mansion's wall-shape placement is not asserted (see the oracle).
        let layout_only = row[0] == "layout";
        let want_admitted: usize = row[2].parse().unwrap();
        let want_gen_probe: i64 = row[12].parse().unwrap();
        let want_pieces: usize = row[4].parse().unwrap();
        let want_geometry = row[5];
        let want_writes: usize = row[6].parse().unwrap();
        let want_state = row[7];
        let want_be_count: usize = row[8].parse().unwrap();
        let want_be = row[9];
        let want_ent_count: usize = row[10].parse().unwrap();
        let want_ent = row[11];
        let want_post_probe: i32 = row[13].parse().unwrap();
        let mut failures = 0usize;
        if gen_probe != want_gen_probe {
            row_failure(&format!("{id} gen probe"), gen_probe, want_gen_probe);
            failures += 1;
        }
        let admitted = usize::from(found.is_some());
        if admitted != want_admitted {
            row_failure(&format!("{id} admitted"), admitted, want_admitted);
            return failures + 1;
        }
        let Some((stub, build)) = found else {
            if want_post_probe != 0 && !layout_only {
                row_failure(&format!("{id} post probe"), 0, want_post_probe);
                failures += 1;
            }
            return failures;
        };
        if row[3] != format!("{},{},{}", stub.0, stub.1, stub.2) {
            row_failure(
                &format!("{id} stub"),
                format!("{},{},{}", stub.0, stub.1, stub.2),
                row[3],
            );
            failures += 1;
        }
        if jstr(case, "kind").as_deref() == Some("mansion") && !grid_rows.is_empty() {
            // The grid is built from the same stream position the piece builder
            // starts at, so replay it on a copy and keep the real stream intact.
            let snapshot = gen.state48();
            let grid = MansionGrid::build(&mut Lcg::from_raw_state(snapshot));
            let entrance = (grid.entrance_x, grid.entrance_y);
            let grids: [(&str, &SimpleGrid); 5] = [
                ("base", &grid.base_grid),
                ("third", &grid.third_floor_grid),
                ("rooms0", &grid.floor_rooms[0]),
                ("rooms1", &grid.floor_rooms[1]),
                ("rooms2", &grid.floor_rooms[2]),
            ];
            for (name, grid_ref) in grids {
                if let Some(want) = grid_rows.get(name) {
                    failures +=
                        compare_grids(&format!("{id} grid {name}"), grid_ref, entrance, want);
                }
            }
        }
        let pieces = build(&mut world, templates, &mut gen, reg);
        if pieces.len() != want_pieces {
            row_failure(&format!("{id} piece count"), pieces.len(), want_pieces);
            failures += 1;
        }
        // Per-piece rows first: a geometry mismatch then names the piece.
        if piece_rows.len() != pieces.len() {
            row_failure(&format!("{id} piece rows"), piece_rows.len(), pieces.len());
            failures += 1;
        }
        for (index, piece) in pieces.iter().enumerate() {
            let Some(want) = piece_rows.get(index) else {
                break;
            };
            let want_box = want[4];
            let got_box = format!(
                "{},{},{},{},{},{}",
                piece.bb[0], piece.bb[1], piece.bb[2], piece.bb[3], piece.bb[4], piece.bb[5]
            );
            if short_name(&piece.template) != want[3] {
                row_failure(
                    &format!("{id} piece {index} template"),
                    short_name(&piece.template),
                    want[3],
                );
                failures += 1;
            }
            if got_box != want_box {
                row_failure(&format!("{id} piece {index} box"), got_box, want_box);
                failures += 1;
            }
            if (piece.rotation as u8).to_string() != want[5] {
                row_failure(
                    &format!("{id} piece {index} rotation"),
                    piece.rotation as u8,
                    want[5],
                );
                failures += 1;
            }
            if (piece.mirror as u8).to_string() != want[6] {
                row_failure(
                    &format!("{id} piece {index} mirror"),
                    piece.mirror as u8,
                    want[6],
                );
                failures += 1;
            }
            if piece.gen_depth.to_string() != want[7] {
                row_failure(
                    &format!("{id} piece {index} genDepth"),
                    piece.gen_depth,
                    want[7],
                );
                failures += 1;
            }
        }
        let geometry = geometry_hash(&pieces);
        if geometry != want_geometry {
            row_failure(&format!("{id} geometry"), &geometry, want_geometry);
            failures += 1;
        }
        if layout_only {
            return failures;
        }
        let flavour = if kind == "end_city" {
            StructureFlavour::EndCity
        } else {
            StructureFlavour::Mansion
        };
        let mut post = Xor::seed_worldgen(post_seed);
        let mut be_events: Vec<BeEvent> = Vec::new();
        let mut entities: Vec<EntitySpawn> = Vec::new();
        for chunk in jarr(case, "chunks").expect("chunks") {
            let a = chunk.as_arr().expect("chunk");
            let (ccx, ccz) = (a[0].num() as i32, a[1].num() as i32);
            let (min_x, min_z) = (ccx << 4, ccz << 4);
            let chunk_bb: BB = [
                min_x,
                world.scene.min_y,
                min_z,
                min_x + 15,
                world.scene.min_y + world.scene.height - 1,
                min_z + 15,
            ];
            let placed = match flavour {
                StructureFlavour::EndCity => place_end_city_chunk(
                    &mut world, templates, &pieces, &mut post, &chunk_bb, world_seed, reg,
                ),
                StructureFlavour::Mansion => place_mansion_chunk(
                    &mut world, templates, &pieces, &mut post, &chunk_bb, world_seed, reg,
                ),
            };
            match placed {
                Ok(out) => {
                    be_events.extend(out.be_events);
                    entities.extend(out.entities);
                }
                Err(message) => {
                    row_failure(&format!("{id} chunk {ccx},{ccz}"), message, "ok");
                    failures += 1;
                }
            }
        }
        let post_probe = post.next_bound(1000003);
        let (write_count, state_hash) = world_hash(reg, &world.writes);
        let be = be_lines(&be_events);
        let be_hash = hash_lines(&be);
        // Per-block-entity comparison first: a hash difference then names the
        // cell and the field instead of only reporting that it differs.
        for (pos, summary) in be_rows {
            let want = summary.join("|");
            let got = be.iter().find(|line| line.starts_with(&format!("{pos}|")));
            match got {
                Some(line) => {
                    let suffix = line[pos.len() + 1..].to_string();
                    if suffix != want {
                        row_failure(&format!("{id} block entity {pos}"), suffix, want);
                        failures += 1;
                    }
                }
                None => {
                    row_failure(&format!("{id} block entity {pos}"), "missing", want);
                    failures += 1;
                }
            }
        }
        for line in &be {
            let pos = &line[..line.find('|').unwrap_or(line.len())];
            if !be_rows.contains_key(pos) {
                row_failure(&format!("{id} block entity {pos}"), line, "absent");
                failures += 1;
            }
        }
        let ent = entity_lines(&entities);
        let ent_hash = hash_lines(&ent);
        let mansion_has_allays =
            kind == "mansion" && ent.iter().any(|line| line.starts_with("minecraft:allay@"));
        if write_count != want_writes {
            row_failure(&format!("{id} writes"), write_count, want_writes);
            failures += 1;
        }
        if state_hash != want_state {
            row_failure(&format!("{id} state hash"), &state_hash, want_state);
            failures += 1;
        }
        if be.len() != want_be_count {
            row_failure(&format!("{id} block entity count"), be.len(), want_be_count);
            eprintln!("  be lines: {be:?}");
            failures += 1;
        }
        if be_hash != want_be {
            row_failure(&format!("{id} block entity hash"), &be_hash, want_be);
            failures += 1;
        }
        if mansion_has_allays {
            // `Group of Allays` draws `level.getRandom().nextInt(3) + 1`, and
            // vanilla `Mob.finalizeSpawn` shares `level.getRandom()`, so total
            // allay count depends on entity-spawn RNG outside placement.
            let mut per_pos: HashMap<&str, usize> = HashMap::new();
            for line in &ent {
                if let Some(pos_str) = line.strip_prefix("minecraft:allay@") {
                    *per_pos.entry(pos_str).or_default() += 1;
                }
            }
            for (pos_str, count) in per_pos {
                if !(1..=3).contains(&count) {
                    row_failure(&format!("{id} allay group@{pos_str}"), count, "1..=3");
                    failures += 1;
                }
            }
        } else {
            if ent.len() != want_ent_count {
                row_failure(&format!("{id} entity count"), ent.len(), want_ent_count);
                eprintln!("  entity lines: {ent:?}");
                failures += 1;
            }
            if ent_hash != want_ent {
                row_failure(&format!("{id} entity hash"), &ent_hash, want_ent);
                failures += 1;
            }
        }
        if post_probe != want_post_probe {
            row_failure(&format!("{id} post probe"), post_probe, want_post_probe);
            failures += 1;
        }
        failures
    }

    /// The pinned 1.21.1 fixture: every row of
    /// `tests/data/end_city_mansion_1211.tsv` replayed against the port, with
    /// per-piece diagnostics ahead of the hashes.
    #[test]
    fn end_city_mansion_match_1211_native_fixture() {
        let reg = breg().expect("block registry");
        let cases = load_cases();
        let case_list = jarr(cases, "structures").expect("structures");
        let tsv = include_str!("../tests/data/end_city_mansion_1211.tsv");
        let mut rows: HashMap<&str, &str> = HashMap::new();
        let mut piece_rows: HashMap<&str, Vec<Vec<&str>>> = HashMap::new();
        let mut grid_rows: HashMap<&str, std::collections::BTreeMap<String, Vec<&str>>> =
            HashMap::new();
        let mut be_rows: HashMap<&str, std::collections::BTreeMap<String, Vec<&str>>> =
            HashMap::new();
        for line in tsv.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let fields: Vec<&str> = line.split('\t').collect();
            if fields[0] == "pieces" {
                piece_rows.entry(fields[1]).or_default().push(fields);
            } else if fields[0] == "grid" {
                grid_rows
                    .entry(fields[1])
                    .or_default()
                    .insert(fields[2].to_string(), fields);
            } else if fields[0] == "be" {
                be_rows
                    .entry(fields[1])
                    .or_default()
                    .insert(fields[2].to_string(), fields[3..].to_vec());
            } else {
                rows.insert(fields[1], line);
            }
        }
        let templates =
            Templates::new(|id| crate::worldgen::Gen::dp_resource("structure", id, "nbt"));
        let mut cases_run = 0usize;
        let mut failures = 0usize;
        for case in case_list {
            let id = jstr(case, "id").expect("id");
            let row: Vec<&str> = rows
                .get(id.as_str())
                .unwrap_or_else(|| panic!("missing fixture row {id}"))
                .split('\t')
                .collect();
            let empty: Vec<Vec<&str>> = Vec::new();
            let pieces = piece_rows.get(id.as_str()).unwrap_or(&empty);
            let borrowed: Vec<&[&str]> = pieces.iter().map(|r| r.as_slice()).collect();
            let empty_grids = std::collections::BTreeMap::new();
            let grids = grid_rows.get(id.as_str()).unwrap_or(&empty_grids);
            let bes = be_rows.get(id.as_str()).unwrap_or(&empty_grids);
            cases_run += 1;
            failures += run_case(case, &row, &borrowed, grids, bes, reg, &templates);
        }
        eprintln!("end city / mansion: {cases_run} cases, {failures} failure(s)");
        assert_eq!(cases_run, rows.len(), "fixture rows without a case");
        assert_eq!(failures, 0, "{failures} case(s) diverged from native");
    }

    // ------------------------------------------------------------ focused contracts

    /// A flat scene whose every column answers `height` as its first air y.
    fn flat_scene(reg: &BReg, height: i32, sea: i32) -> Scene {
        let mut ws = HashMap::new();
        for x in -32..=32 {
            for z in -32..=32 {
                ws.insert((x, z), height);
            }
        }
        Scene {
            min_y: -64,
            height: 384,
            sea,
            ws,
            cells: HashMap::new(),
            air: reg.state_of("minecraft:air", None),
            top: reg.state_of("minecraft:stone", None),
            fill: reg.state_of("minecraft:stone", None),
        }
    }

    /// `Structure.getLowestYIn5by5BoxOffset7Blocks` reads `getFirstOccupiedHeight`
    /// (`getBaseHeight - 1`) and both structures refuse below y = 60.
    #[test]
    fn end_city_and_mansion_admission_stop_below_sixty() {
        let reg = breg().expect("block registry");
        let world_seed = 11u64;
        for (height, admitted) in [(59, false), (60, false), (61, true), (70, true)] {
            let mut world = TestWorld::new(flat_scene(reg, height, 63));
            let mut rng = Lcg::for_large_feature(world_seed, 0, 0);
            let found = find_end_city_pieces(&mut world, 0, 0, &mut rng);
            assert_eq!(
                found.is_some(),
                admitted,
                "end city at column height {height}"
            );
            if let Some((stub, _)) = found {
                assert_eq!(stub.1, height - 1, "stub y is the first occupied y");
                assert_eq!((stub.0, stub.2), (7, 7), "stub is anchored 7 blocks in");
            }
            let mut world = TestWorld::new(flat_scene(reg, height, 63));
            let mut rng = Lcg::for_large_feature(world_seed, 0, 0);
            assert_eq!(
                find_mansion_pieces(&mut world, 0, 0, &mut rng).is_some(),
                admitted,
                "mansion at column height {height}"
            );
        }
    }

    /// `EndCityPiece.handleDataMarker("Chest")` puts the treasure table on the
    /// container *below* the marker, and only a live randomizable container takes
    /// a `LootTableSeed` - which is the one draw the marker costs.
    #[test]
    fn end_city_chest_marker_requires_a_live_container() {
        let chest = BeEvent::Set(BlockEntityRecord {
            pos: (1, 70, 3),
            id: "minecraft:chest",
            loot_table: None,
            loot_seed: None,
            items: None,
            spawner_entity: None,
        });
        let mut out = PlacementOut {
            be_events: vec![chest],
            ..Default::default()
        };
        let mut rng = Xor::seed_worldgen(5);
        end_city_chest_loot(&mut out, (1, 70, 3), &mut rng);
        let BeEvent::Set(record) = &out.be_events[0] else {
            panic!("chest record");
        };
        assert_eq!(record.loot_table.as_deref(), Some(END_CITY_TREASURE));
        let seed = record.loot_seed.expect("loot seed");
        let mut reference = Xor::seed_worldgen(5);
        assert_eq!(seed, reference.next_i64(), "one nextLong decides the seed");
        assert_eq!(rng.state128(), reference.state128());

        // A cell with no container draws nothing: the marker is a no-op.
        let mut empty = PlacementOut::default();
        let mut untouched = Xor::seed_worldgen(5);
        end_city_chest_loot(&mut empty, (1, 70, 3), &mut untouched);
        assert!(empty.be_events.is_empty());
        assert_eq!(untouched.state128(), Xor::seed_worldgen(5).state128());
    }

    /// `EndCityPiece.handleDataMarker`'s `Elytra` branch: an item frame at the marker
    /// carrying one elytra, faced by the piece rotation applied to SOUTH
    /// (`new ItemFrame(level, pos, placeSettings.getRotation().rotate(Direction.SOUTH))`
    /// then `setItem(new ItemStack(Items.ELYTRA))`).
    ///
    /// The facing is the rotated one, which is what the old code threw away: the
    /// ship in the native corpus (`end-city-ship-s0`) is placed a quarter turn, so
    /// its frame faces west while the template's marker says south.
    #[test]
    fn end_ship_elytra_marker_stages_a_rotated_item_frame_with_the_elytra() {
        let reg = breg().expect("block registry");
        let chunk_bb: BB = [0, -64, 0, 15, 319, 15];
        let mut world = TestWorld::new(flat_scene(reg, 70, 63));
        // (piece rotation, `Direction.get3DDataValue` of rotation.rotate(SOUTH))
        for (rotation, facing) in [
            (Rotation::None, 3),               // south
            (Rotation::Clockwise90, 4),        // west
            (Rotation::Clockwise180, 2),       // north
            (Rotation::Counterclockwise90, 5), // east
        ] {
            let mut out = PlacementOut::default();
            let mut rng = Xor::seed_worldgen(11);
            let before = rng.state128();
            handle_data_marker(
                &mut world,
                reg,
                &mut out,
                StructureFlavour::EndCity,
                "Elytra",
                (4, 70, 4),
                rotation,
                &chunk_bb,
                &mut rng,
            );
            assert_eq!(out.entities.len(), 1, "{rotation:?} stages one frame");
            let spawn = &out.entities[0];
            assert_eq!(spawn.id, "minecraft:item_frame");
            assert_eq!(spawn.pos, (4, 70, 4));
            let hanging = spawn.hanging.as_ref().expect("hanging payload");
            assert_eq!(hanging.facing, facing, "{rotation:?} facing");
            assert_eq!(
                hanging.item,
                Some(("minecraft:elytra".to_string(), 1)),
                "the ship's frame holds one elytra"
            );
            assert_eq!(rng.state128(), before, "the frame costs no draw");
        }

        // The marker is still delivered only inside the chunk being placed.
        let mut outside = PlacementOut::default();
        let mut rng = Xor::seed_worldgen(11);
        handle_data_marker(
            &mut world,
            reg,
            &mut outside,
            StructureFlavour::EndCity,
            "Elytra",
            (20, 70, 4),
            Rotation::None,
            &chunk_bb,
            &mut rng,
        );
        assert!(outside.entities.is_empty(), "a frame in another chunk waits");
    }

    /// `WoodlandMansionPiece.handleDataMarker` writes a chest whose `facing` is
    /// the piece rotation applied to the marker's side, and draws one
    /// `LootTableSeed`; a cell that already holds a chest is left alone.
    #[test]
    fn mansion_chest_marker_faces_through_the_piece_rotation() {
        let reg = breg().expect("block registry");
        let chunk_bb: BB = [0, -64, 0, 15, 319, 15];
        let mut world = TestWorld::new(flat_scene(reg, 70, 63));
        let mut out = PlacementOut::default();
        let mut rng = Xor::seed_worldgen(9);
        handle_data_marker(
            &mut world,
            reg,
            &mut out,
            StructureFlavour::Mansion,
            "ChestWest",
            (4, 70, 4),
            Rotation::Clockwise90,
            &chunk_bb,
            &mut rng,
        );
        let state = world.writes[&(4, 70, 4)];
        assert_eq!(
            reg.get_prop(state, "facing").as_deref(),
            Some("north"),
            "west rotated a quarter turn clockwise faces north"
        );
        let seeded = out
            .be_events
            .iter()
            .filter_map(|event| match event {
                BeEvent::Set(record) => Some(record),
                BeEvent::Clear(_) => None,
            })
            .next()
            .expect("chest block entity");
        assert_eq!(seeded.loot_table.as_deref(), Some(WOODLAND_MANSION_LOOT));
        assert!(seeded.loot_seed.is_some());

        // Second marker on the same cell: refused, and no seed is drawn.
        let mut again = PlacementOut::default();
        let mut untouched = Xor::seed_worldgen(9);
        let before = untouched.state128();
        handle_data_marker(
            &mut world,
            reg,
            &mut again,
            StructureFlavour::Mansion,
            "ChestWest",
            (4, 70, 4),
            Rotation::Clockwise90,
            &chunk_bb,
            &mut untouched,
        );
        assert!(
            again.be_events.is_empty(),
            "an existing chest is not replaced"
        );
        assert_eq!(untouched.state128(), before, "and costs no draw");
    }

    /// `place_mansion_chunk` runs the template pieces and then the foundation:
    /// the wall template writes its own blocks and cobblestone appears under the
    /// lowest structural cell, down to sea level.
    #[test]
    fn mansion_chunk_places_the_template_and_its_foundation() {
        let reg = breg().expect("block registry");
        let templates =
            Templates::new(|id| crate::worldgen::Gen::dp_resource("structure", id, "nbt"));
        let mut world = TestWorld::new(flat_scene(reg, 60, 63));
        let pieces = vec![TemplatePiece {
            template: "minecraft:woodland_mansion/wall_flat".to_string(),
            pos: (0, 70, 0),
            bb: [0, 70, 0, 7, 77, 1],
            rotation: Rotation::None,
            mirror: Mirror::None,
            ignore_air: false,
            gen_depth: 0,
        }];
        let chunk_bb: BB = [0, -64, 0, 15, 319, 15];
        let mut rng = Xor::seed_worldgen(3);
        let out = place_mansion_chunk(
            &mut world,
            &templates,
            &pieces,
            &mut rng,
            &chunk_bb,
            1,
            reg,
        )
        .expect("mansion chunk");
        let cobblestone = reg.state_of("minecraft:cobblestone", None);
        assert!(
            world.writes.values().any(|state| *state != cobblestone),
            "the wall template wrote blocks of its own"
        );
        assert_eq!(
            world.writes[&(0, 69, 0)],
            cobblestone,
            "the foundation starts under the lowest piece cell"
        );
        assert!(!world.writes.contains_key(&(0, 63, 0)));
        assert!(out.writes > 0);
    }

    /// `WoodlandMansionStructure.afterPlace`: fill cobblestone under every
    /// structural cell of the chunk, from the structure's lowest y down, while
    /// the column is air and while it is above sea level.
    #[test]
    fn mansion_foundation_fills_air_columns_above_sea_level() {
        let reg = breg().expect("block registry");
        let mut scene = flat_scene(reg, 60, 63);
        // One column is solid at y = 66, so the pass must stop there instead of
        // filling on down to sea level.
        scene.ws.insert((1, 1), 67);
        let mut world = TestWorld::new(scene);
        let cobblestone = reg.state_of("minecraft:cobblestone", None);
        for (x, z) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
            world.set(x, 70, z, cobblestone);
        }
        let pieces = vec![TemplatePiece {
            template: "minecraft:woodland_mansion/wall_flat".to_string(),
            pos: (0, 70, 0),
            bb: [0, 70, 0, 1, 77, 1],
            rotation: Rotation::None,
            mirror: Mirror::None,
            ignore_air: false,
            gen_depth: 0,
        }];
        let chunk_bb: BB = [0, -64, 0, 15, 319, 15];
        let written = mansion_after_place(&mut world, &pieces, &chunk_bb, reg);
        assert_eq!(
            written,
            3 * 6 + 3,
            "three air columns fill y 69..=64; the solid one stops at y 66"
        );
        for (x, z) in [(0, 0), (1, 0), (0, 1)] {
            for y in 64..=69 {
                assert_eq!(world.writes[&(x, y, z)], cobblestone, "({x},{y},{z})");
            }
            assert!(
                !world.writes.contains_key(&(x, 63, z)),
                "sea level stops it"
            );
        }
        for y in 67..=69 {
            assert_eq!(world.writes[&(1, y, 1)], cobblestone, "(1,{y},1) air above");
        }
        assert!(
            !world.writes.contains_key(&(1, 66, 1)),
            "the first solid cell is left alone"
        );
        assert!(
            !world.writes.contains_key(&(1, 65, 1)),
            "and nothing below it"
        );
        assert!(
            !world.writes.contains_key(&(2, 65, 2)),
            "a column outside the pieces is left alone"
        );
    }

    #[test]
    fn mansion_foundation_stops_at_world_bottom_when_sea_is_lower() {
        let reg = breg().expect("block registry");
        let mut scene = flat_scene(reg, -64, -100);
        scene.height = 16;
        let mut world = TestWorld::new(scene);
        let cobblestone = reg.state_of("minecraft:cobblestone", None);
        world.set(0, -60, 0, cobblestone);
        let pieces = vec![TemplatePiece {
            template: "minecraft:woodland_mansion/wall_flat".to_string(),
            pos: (0, -60, 0),
            bb: [0, -60, 0, 0, -60, 0],
            rotation: Rotation::None,
            mirror: Mirror::None,
            ignore_air: false,
            gen_depth: 0,
        }];
        let chunk_bb: BB = [0, -64, 0, 15, -49, 15];
        assert_eq!(mansion_after_place(&mut world, &pieces, &chunk_bb, reg), 3);
        for y in -63..=-61 {
            assert_eq!(world.writes[&(0, y, 0)], cobblestone);
        }
        assert!(!world.writes.contains_key(&(0, -64, 0)));

        // Starting at the world's lowest block leaves no room for a foundation.
        let bottom_piece = [TemplatePiece {
            pos: (0, -64, 0),
            bb: [0, -64, 0, 0, -64, 0],
            ..pieces[0].clone()
        }];
        world.set(0, -64, 0, cobblestone);
        assert_eq!(mansion_after_place(&mut world, &bottom_piece, &chunk_bb, reg), 0);
    }

    #[test]
    fn mansion_foundation_replaces_water_and_lava_but_stops_on_solid() {
        let reg = breg().expect("block registry");
        let mut scene = flat_scene(reg, 60, 63);
        scene.cells.insert((0, 68, 0), reg.state_of("minecraft:water", None));
        scene.cells.insert((0, 67, 0), reg.state_of("minecraft:lava", None));
        scene.cells.insert((1, 68, 0), reg.state_of("minecraft:stone", None));
        let mut world = TestWorld::new(scene);
        let cobblestone = reg.state_of("minecraft:cobblestone", None);
        world.set(0, 70, 0, cobblestone);
        world.set(1, 70, 0, cobblestone);
        let pieces = vec![TemplatePiece {
            template: "minecraft:woodland_mansion/wall_flat".to_string(),
            pos: (0, 70, 0),
            bb: [0, 70, 0, 1, 70, 0],
            rotation: Rotation::None,
            mirror: Mirror::None,
            ignore_air: false,
            gen_depth: 0,
        }];
        let chunk_bb: BB = [0, -64, 0, 15, 319, 15];
        assert_eq!(mansion_after_place(&mut world, &pieces, &chunk_bb, reg), 7);
        for y in 64..=69 {
            assert_eq!(world.writes[&(0, y, 0)], cobblestone);
        }
        assert_eq!(world.writes[&(1, 69, 0)], cobblestone);
        assert!(!world.writes.contains_key(&(1, 68, 0)));
        assert!(!world.writes.contains_key(&(0, 63, 0)));
    }
}

#[cfg(test)]
mod end_city_mansion_signature_tests {
    use super::*;

    #[test]
    fn end_city_chest_marker_assigns_treasure_loot_to_a_live_chest() {
        let pos = (4, 71, -9);
        let mut out = PlacementOut {
            be_events: vec![BeEvent::Set(BlockEntityRecord {
                pos,
                id: "minecraft:chest",
                loot_table: None,
                loot_seed: None,
                items: None,
                spawner_entity: None,
            })],
            ..PlacementOut::default()
        };
        let mut rng = crate::generation_math::Xor::seed_worldgen(4242);
        end_city_chest_loot(&mut out, pos, &mut rng);
        let Some(BeEvent::Set(record)) = out.be_events.last() else {
            panic!("chest block entity event");
        };
        assert_eq!(record.loot_table.as_deref(), Some(END_CITY_TREASURE));
        assert!(record.loot_seed.is_some());

        let mut empty = PlacementOut::default();
        let before = crate::generation_math::Xor::seed_worldgen(4242).state128();
        let mut untouched = crate::generation_math::Xor::seed_worldgen(4242);
        end_city_chest_loot(&mut empty, pos, &mut untouched);
        assert!(empty.be_events.is_empty());
        assert_eq!(untouched.state128(), before, "no live chest means no loot-seed draw");
    }

    #[test]
    fn end_ship_elytra_frame_uses_rotation_applied_to_south() {
        let cases = [
            (Rotation::None, Dir::South),
            (Rotation::Clockwise90, Dir::West),
            (Rotation::Clockwise180, Dir::North),
            (Rotation::Counterclockwise90, Dir::East),
        ];
        for (rotation, direction) in cases {
            assert_eq!(rotate_direction(rotation, Dir::South), direction);
        }
        assert_eq!(Dir::South.data_3d(), 3);
        assert_eq!(rotate_direction(Rotation::Clockwise90, Dir::South).data_3d(), 4);
    }

    #[test]
    fn mansion_room_tables_include_secret_obsidian_allay_and_chest_rooms() {
        let mut saw = std::collections::HashSet::new();
        for seed in 0..64 {
            let mut rng = Lcg::new(seed);
            saw.insert(MansionFloor::First.one_by_one_secret(&mut rng));
            saw.insert(MansionFloor::First.one_by_two_secret(&mut rng));
            saw.insert(MansionFloor::Second.one_by_two_secret(&mut rng));
            saw.insert(MansionFloor::First.two_by_two(&mut rng));
        }
        for wanted in ["1x1_as3", "1x2_s1", "1x2_se1", "2x2_a1"] {
            assert!(saw.contains(wanted), "mansion room table missing {wanted}: {saw:?}");
        }
        let mut rng = Lcg::new(0);
        assert_eq!(MansionFloor::First.two_by_two_secret(&mut rng), "2x2_s1");
    }
}
