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
//   * `mansion` - admission, stub, both layout grids cell by cell, and the whole
//     557-619 piece tree.
//
// The mansion's *block placement* is recorded by the oracle but left out of the
// asserted rows. Its templates place `cobblestone_wall`s, and the shared
// `WallBlock` shape rule (`block_registry::wall_after_full_neighbor`) is a
// bounded one: at a piece or chunk boundary a wall side whose neighbour is
// written by a later piece comes out `none` where native recomputes `low`
// (measured on seed 1: 112 of 78 123 written cells, every one of them a
// `north`/`south` connection). That is a `block_registry` gap this file cannot
// close from the placement side, so the fixture does not claim it.
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
}

/// `Rotation.getRotated(Rotation)`: index addition modulo four
/// (`NONE, CLOCKWISE_90, CLOCKWISE_180, COUNTERCLOCKWISE_90`).
pub(crate) fn rotate_rotation(this: Rotation, other: Rotation) -> Rotation {
    Rotation::from_index(((this as u8 + other as u8) % 4) as u8).expect("rotation index")
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
    Box<dyn FnOnce(&mut dyn TemplateWorld, &mut Templates, &mut Lcg, &BReg) -> Vec<TemplatePiece>>;

// ---------------------------------------------------------------- end city

/// `EndCityPieces.SectionGenerator` — the four section shapes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EndCitySection {
    HouseTower,
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
    templates: &mut Templates,
    reg: &BReg,
    name: &str,
    pos: Pos,
    rotation: Rotation,
    overwrite: bool,
) -> Result<TemplatePiece, &'static str> {
    let template_id = format!("minecraft:end_city/{name}");
    let template = templates.get(&template_id, reg)?;
    let bb = template_bb(&template, pos, Mirror::None, rotation, (0, 0, 0));
    Ok(TemplatePiece {
        template: template_id,
        pos,
        bb,
        rotation,
        mirror: Mirror::None,
        // `overwrite` selects `BlockIgnoreProcessor.STRUCTURE_BLOCK`; otherwise
        // `STRUCTURE_AND_AIR` also drops air cells.
        ignore_air: !overwrite,
        gen_depth: 0,
    })
}

/// `EndCityPieces.addPiece`: the child is placed at
/// `parent.templatePosition + transform(offset, parentSettings)
///  - transform(ZERO, childSettings)`.
///
/// Both settings are mirror-NONE with the same rotation and a `ZERO` rotation
/// pivot, so `transform(ZERO, child)` is `(0, 0, 0)` and the delta is the
/// rotated offset.
fn end_city_add_piece(
    templates: &mut Templates,
    reg: &BReg,
    parent: &TemplatePiece,
    offset: Pos,
    name: &str,
    rotation: Rotation,
    overwrite: bool,
) -> Result<TemplatePiece, &'static str> {
    let delta = zero_pos(offset, parent.mirror, parent.rotation, (0, 0, 0));
    let pos = (
        parent.pos.0.wrapping_add(delta.0),
        parent.pos.1.wrapping_add(delta.1),
        parent.pos.2.wrapping_add(delta.2),
    );
    end_city_piece(templates, reg, name, pos, rotation, overwrite)
}

fn add_helper(list: &mut Vec<TemplatePiece>, piece: TemplatePiece) -> TemplatePiece {
    list.push(piece.clone());
    piece
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

/// `EndCityPieces.recursiveChildren`: generate into a scratch list, stamp every
/// candidate with one `nextInt()` depth, and accept the batch only when no
/// candidate collides with an accepted piece of a different depth.
#[allow(clippy::too_many_arguments)]
fn end_city_recursive_children(
    templates: &mut Templates,
    reg: &BReg,
    section: EndCitySection,
    depth: i32,
    piece: &TemplatePiece,
    pos: Option<Pos>,
    list: &mut Vec<TemplatePiece>,
    rng: &mut Lcg,
    ship_created: &mut bool,
) -> bool {
    if depth > END_CITY_MAX_GEN_DEPTH {
        return false;
    }
    let mut scratch: Vec<TemplatePiece> = Vec::new();
    if !end_city_generate(
        templates,
        reg,
        section,
        depth,
        piece,
        pos,
        &mut scratch,
        rng,
        ship_created,
    ) {
        return false;
    }
    let stamp = rng.next_i32();
    let mut blocked = false;
    for candidate in &mut scratch {
        candidate.gen_depth = stamp;
        if let Some(existing) = find_collision_piece(list, &candidate.bb) {
            if existing.gen_depth != piece.gen_depth {
                blocked = true;
                break;
            }
        }
    }
    if blocked {
        return false;
    }
    list.append(&mut scratch);
    true
}

/// `SectionGenerator.generate` for one section shape.
#[allow(clippy::too_many_arguments)]
fn end_city_generate(
    templates: &mut Templates,
    reg: &BReg,
    section: EndCitySection,
    depth: i32,
    piece: &TemplatePiece,
    pos: Option<Pos>,
    list: &mut Vec<TemplatePiece>,
    rng: &mut Lcg,
    ship_created: &mut bool,
) -> bool {
    let rotation = piece.settings().rotation;
    match section {
        EndCitySection::HouseTower => {
            if depth > END_CITY_MAX_GEN_DEPTH {
                return false;
            }
            // `HOUSE_TOWER_GENERATOR` is the one section that consumes the
            // `pos` argument: it is the offset its caller wants the base floor
            // anchored at (the tower-bridge recursion passes `(-3, j + 1, -11)`).
            let base_offset = pos.unwrap_or((0, 0, 0));
            let mut current = add_helper(
                list,
                match end_city_add_piece(
                    templates,
                    reg,
                    piece,
                    base_offset,
                    "base_floor",
                    rotation,
                    true,
                ) {
                    Ok(p) => p,
                    Err(_) => return false,
                },
            );
            let choice = rng.next_bound(3);
            if choice == 0 {
                let Ok(next) = end_city_add_piece(
                    templates,
                    reg,
                    &current,
                    (-1, 4, -1),
                    "base_roof",
                    rotation,
                    true,
                ) else {
                    return false;
                };
                current = add_helper(list, next);
            } else if choice == 1 {
                let Ok(next) = end_city_add_piece(
                    templates,
                    reg,
                    &current,
                    (-1, 0, -1),
                    "second_floor_2",
                    rotation,
                    false,
                ) else {
                    return false;
                };
                current = add_helper(list, next);
                let Ok(next) = end_city_add_piece(
                    templates,
                    reg,
                    &current,
                    (-1, 8, -1),
                    "second_roof",
                    rotation,
                    false,
                ) else {
                    return false;
                };
                current = add_helper(list, next);
                end_city_recursive_children(
                    templates,
                    reg,
                    EndCitySection::Tower,
                    depth + 1,
                    &current,
                    None,
                    list,
                    rng,
                    ship_created,
                );
            } else if choice == 2 {
                let Ok(next) = end_city_add_piece(
                    templates,
                    reg,
                    &current,
                    (-1, 0, -1),
                    "second_floor_2",
                    rotation,
                    false,
                ) else {
                    return false;
                };
                current = add_helper(list, next);
                let Ok(next) = end_city_add_piece(
                    templates,
                    reg,
                    &current,
                    (-1, 4, -1),
                    "third_floor_2",
                    rotation,
                    false,
                ) else {
                    return false;
                };
                current = add_helper(list, next);
                let Ok(next) = end_city_add_piece(
                    templates,
                    reg,
                    &current,
                    (-1, 8, -1),
                    "third_roof",
                    rotation,
                    true,
                ) else {
                    return false;
                };
                current = add_helper(list, next);
                end_city_recursive_children(
                    templates,
                    reg,
                    EndCitySection::Tower,
                    depth + 1,
                    &current,
                    None,
                    list,
                    rng,
                    ship_created,
                );
            }
            true
        }
        EndCitySection::Tower => {
            // `TOWER_GENERATOR.generate` ignores `pos` (it is always null).
            let _ = pos;
            let mut current = add_helper(
                list,
                match end_city_add_piece(
                    templates,
                    reg,
                    piece,
                    (3 + rng.next_bound(2), -3, 3 + rng.next_bound(2)),
                    "tower_base",
                    rotation,
                    true,
                ) {
                    Ok(p) => p,
                    Err(_) => return false,
                },
            );
            let Ok(next) = end_city_add_piece(
                templates,
                reg,
                &current,
                (0, 7, 0),
                "tower_piece",
                rotation,
                true,
            ) else {
                return false;
            };
            current = add_helper(list, next);
            let mut bridge_piece = if rng.next_bound(3) == 0 {
                Some(current.clone())
            } else {
                None
            };
            let count = 1 + rng.next_bound(3);
            for i in 0..count {
                let Ok(next) = end_city_add_piece(
                    templates,
                    reg,
                    &current,
                    (0, 4, 0),
                    "tower_piece",
                    rotation,
                    true,
                ) else {
                    return false;
                };
                current = add_helper(list, next);
                if i < count - 1 && rng.next_bool() {
                    bridge_piece = Some(current.clone());
                }
            }
            match bridge_piece {
                Some(bridge) => {
                    for (bridge_rotation, offset) in END_CITY_TOWER_BRIDGES {
                        if rng.next_bool() {
                            let Ok(end) = end_city_add_piece(
                                templates,
                                reg,
                                &bridge,
                                offset,
                                "bridge_end",
                                rotate_rotation(rotation, bridge_rotation),
                                true,
                            ) else {
                                return false;
                            };
                            let end = add_helper(list, end);
                            end_city_recursive_children(
                                templates,
                                reg,
                                EndCitySection::TowerBridge,
                                depth + 1,
                                &end,
                                None,
                                list,
                                rng,
                                ship_created,
                            );
                        }
                    }
                    let Ok(top) = end_city_add_piece(
                        templates,
                        reg,
                        &current,
                        (-1, 4, -1),
                        "tower_top",
                        rotation,
                        true,
                    ) else {
                        return false;
                    };
                    current = add_helper(list, top);
                }
                None => {
                    if depth == 7 {
                        let Ok(top) = end_city_add_piece(
                            templates,
                            reg,
                            &current,
                            (-1, 4, -1),
                            "tower_top",
                            rotation,
                            true,
                        ) else {
                            return false;
                        };
                        current = add_helper(list, top);
                    } else {
                        return end_city_recursive_children(
                            templates,
                            reg,
                            EndCitySection::FatTower,
                            depth + 1,
                            &current,
                            None,
                            list,
                            rng,
                            ship_created,
                        );
                    }
                }
            }
            true
        }
        EndCitySection::TowerBridge => {
            let _ = pos;
            let length = rng.next_bound(4) + 1;
            let mut current = add_helper(
                list,
                match end_city_add_piece(
                    templates,
                    reg,
                    piece,
                    (0, 0, -4),
                    "bridge_piece",
                    rotation,
                    true,
                ) {
                    Ok(p) => p,
                    Err(_) => return false,
                },
            );
            current.gen_depth = -1;
            let mut offset = 0;
            for _ in 0..length {
                if rng.next_bool() {
                    let Ok(next) = end_city_add_piece(
                        templates,
                        reg,
                        &current,
                        (0, offset, -4),
                        "bridge_piece",
                        rotation,
                        true,
                    ) else {
                        return false;
                    };
                    current = add_helper(list, next);
                    offset = 0;
                } else if rng.next_bool() {
                    let Ok(next) = end_city_add_piece(
                        templates,
                        reg,
                        &current,
                        (0, offset, -4),
                        "bridge_steep_stairs",
                        rotation,
                        true,
                    ) else {
                        return false;
                    };
                    current = add_helper(list, next);
                    offset = 4;
                } else {
                    let Ok(next) = end_city_add_piece(
                        templates,
                        reg,
                        &current,
                        (0, offset, -8),
                        "bridge_gentle_stairs",
                        rotation,
                        true,
                    ) else {
                        return false;
                    };
                    current = add_helper(list, next);
                    offset = 4;
                }
            }
            if !*ship_created && rng.next_bound(10 - depth) == 0 {
                let dup = rng.next_bound(8);
                let dz = rng.next_bound(10);
                let Ok(ship) = end_city_add_piece(
                    templates,
                    reg,
                    &current,
                    (-8 + dup, offset, -70 + dz),
                    "ship",
                    rotation,
                    true,
                ) else {
                    return false;
                };
                list.push(ship);
                *ship_created = true;
            } else if !end_city_recursive_children(
                templates,
                reg,
                EndCitySection::HouseTower,
                depth + 1,
                &current,
                Some((-3, offset + 1, -11)),
                list,
                rng,
                ship_created,
            ) {
                return false;
            }
            let Ok(end) = end_city_add_piece(
                templates,
                reg,
                &current,
                (4, offset, 0),
                "bridge_end",
                rotate_rotation(rotation, Rotation::Clockwise180),
                true,
            ) else {
                return false;
            };
            let end = add_helper(list, end);
            let _ = end;
            // `setGenDepth(-1)` on the returned end piece; it is already in the
            // list, so update the stored copy too.
            if let Some(last) = list.last_mut() {
                last.gen_depth = -1;
            }
            true
        }
        EndCitySection::FatTower => {
            let _ = pos;
            let mut current = add_helper(
                list,
                match end_city_add_piece(
                    templates,
                    reg,
                    piece,
                    (-3, 4, -3),
                    "fat_tower_base",
                    rotation,
                    true,
                ) {
                    Ok(p) => p,
                    Err(_) => return false,
                },
            );
            let Ok(next) = end_city_add_piece(
                templates,
                reg,
                &current,
                (0, 4, 0),
                "fat_tower_middle",
                rotation,
                true,
            ) else {
                return false;
            };
            current = add_helper(list, next);
            for _ in 0..2 {
                if rng.next_bound(3) == 0 {
                    break;
                }
                let Ok(next) = end_city_add_piece(
                    templates,
                    reg,
                    &current,
                    (0, 8, 0),
                    "fat_tower_middle",
                    rotation,
                    true,
                ) else {
                    return false;
                };
                current = add_helper(list, next);
                for (bridge_rotation, offset) in END_CITY_FAT_TOWER_BRIDGES {
                    if rng.next_bool() {
                        let Ok(end) = end_city_add_piece(
                            templates,
                            reg,
                            &current,
                            offset,
                            "bridge_end",
                            rotate_rotation(rotation, bridge_rotation),
                            true,
                        ) else {
                            return false;
                        };
                        let end = add_helper(list, end);
                        end_city_recursive_children(
                            templates,
                            reg,
                            EndCitySection::TowerBridge,
                            depth + 1,
                            &end,
                            None,
                            list,
                            rng,
                            ship_created,
                        );
                    }
                }
            }
            let Ok(top) = end_city_add_piece(
                templates,
                reg,
                &current,
                (-2, 8, -2),
                "fat_tower_top",
                rotation,
                true,
            ) else {
                return false;
            };
            add_helper(list, top);
            true
        }
    }
}

/// `EndCityPieces.startHouseTower`: the four `init()` calls reset the shared
/// `shipCreated` flag, then the fixed base/second/third stack is laid down and
/// the tower recursion starts.
pub(crate) fn start_house_tower(
    templates: &mut Templates,
    reg: &BReg,
    pos: Pos,
    rotation: Rotation,
    list: &mut Vec<TemplatePiece>,
    rng: &mut Lcg,
) -> Result<(), &'static str> {
    let mut ship_created = false;
    let base = end_city_piece(templates, reg, "base_floor", pos, rotation, true)?;
    let mut current = add_helper(list, base);
    current = add_helper(
        list,
        end_city_add_piece(
            templates,
            reg,
            &current,
            (-1, 0, -1),
            "second_floor_1",
            rotation,
            false,
        )?,
    );
    current = add_helper(
        list,
        end_city_add_piece(
            templates,
            reg,
            &current,
            (-1, 4, -1),
            "third_floor_1",
            rotation,
            false,
        )?,
    );
    current = add_helper(
        list,
        end_city_add_piece(
            templates,
            reg,
            &current,
            (-1, 8, -1),
            "third_roof",
            rotation,
            true,
        )?,
    );
    let _ = end_city_recursive_children(
        templates,
        reg,
        EndCitySection::Tower,
        1,
        &current,
        None,
        list,
        rng,
        &mut ship_created,
    );
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
    let builder: EndCityMansionPieceBuilder = Box::new(move |world, templates, rng, reg| {
        let mut pieces = Vec::new();
        let _ = start_house_tower(templates, reg, pos, rotation, &mut pieces, rng);
        drop(world);
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
        let mut base_grid = SimpleGrid::new(11, 11, 5);
        base_grid.set_box(entrance_x, entrance_y, entrance_x + 1, entrance_y + 1, 3);
        base_grid.set_box(
            entrance_x - 1,
            entrance_y,
            entrance_x - 1,
            entrance_y + 1,
            2,
        );
        base_grid.set_box(
            entrance_x + 2,
            entrance_y - 2,
            entrance_x + 3,
            entrance_y + 3,
            5,
        );
        base_grid.set_box(
            entrance_x + 1,
            entrance_y - 2,
            entrance_x + 1,
            entrance_y - 1,
            1,
        );
        base_grid.set_box(
            entrance_x + 1,
            entrance_y + 2,
            entrance_x + 1,
            entrance_y + 3,
            1,
        );
        base_grid.set(entrance_x - 1, entrance_y - 1, 1);
        base_grid.set(entrance_x - 1, entrance_y + 2, 1);
        base_grid.set_box(0, 0, 11, 1, 5);
        base_grid.set_box(0, 9, 11, 11, 5);
        recursive_corridor(
            &mut base_grid,
            rng,
            entrance_x,
            entrance_y - 2,
            Dir::West,
            6,
        );
        recursive_corridor(
            &mut base_grid,
            rng,
            entrance_x,
            entrance_y + 3,
            Dir::West,
            6,
        );
        recursive_corridor(
            &mut base_grid,
            rng,
            entrance_x - 2,
            entrance_y - 1,
            Dir::West,
            3,
        );
        recursive_corridor(
            &mut base_grid,
            rng,
            entrance_x - 2,
            entrance_y + 2,
            Dir::West,
            3,
        );
        while clean_edges(&mut base_grid) {}

        let mut floor_rooms = [
            SimpleGrid::new(11, 11, 5),
            SimpleGrid::new(11, 11, 5),
            SimpleGrid::new(11, 11, 5),
        ];
        identify_rooms(&base_grid, &mut floor_rooms[0], rng);
        identify_rooms(&base_grid, &mut floor_rooms[1], rng);
        floor_rooms[0].set_box(
            entrance_x + 1,
            entrance_y,
            entrance_x + 1,
            entrance_y + 1,
            ROOM_CORRIDOR_FLAG,
        );
        floor_rooms[1].set_box(
            entrance_x + 1,
            entrance_y,
            entrance_x + 1,
            entrance_y + 1,
            ROOM_CORRIDOR_FLAG,
        );
        let mut third_floor_grid = SimpleGrid::new(base_grid.width, base_grid.height, 5);
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

    /// `MansionGrid.isRoomId`.
    pub(crate) fn is_room_id(
        &self,
        grid: &SimpleGrid,
        x: i32,
        y: i32,
        floor: usize,
        id: i32,
    ) -> bool {
        let _ = grid;
        (self.floor_rooms[floor].get(x, y) & ROOM_ID_MASK) == id
    }

    /// `MansionGrid.get1x2RoomDirection`: the first horizontal direction whose
    /// neighbour is the same room id.
    pub(crate) fn one_by_two_direction(
        &self,
        x: i32,
        y: i32,
        floor: usize,
        id: i32,
    ) -> Option<Dir> {
        HORIZONTAL.into_iter().find(|direction| {
            self.is_room_id(
                &self.base_grid,
                x + direction.step_x(),
                y + direction.step_z(),
                floor,
                id,
            )
        })
    }
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
        third_floor_grid.set_box(
            0,
            0,
            third_floor_grid.width,
            third_floor_grid.height,
            GRID_BLOCKED,
        );
        return;
    }
    let (origin_x, origin_y) = candidates[rng.next_bound(candidates.len() as i32) as usize];
    let value = second_floor.get(origin_x, origin_y);
    floor_rooms[1].set(origin_x, origin_y, value | ROOM_STAIRS_FLAG);
    // `get1x2RoomDirection(baseGrid, x, y, 1, id)` — the base grid, floor 1.
    let id = value & ROOM_ID_MASK;
    let Some(direction) = HORIZONTAL.into_iter().find(|direction| {
        (floor_rooms[1].get(origin_x + direction.step_x(), origin_y + direction.step_z())
            & ROOM_ID_MASK)
            == id
    }) else {
        third_floor_grid.set_box(
            0,
            0,
            third_floor_grid.width,
            third_floor_grid.height,
            GRID_BLOCKED,
        );
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
        third_floor_grid.set_box(
            0,
            0,
            third_floor_grid.width,
            third_floor_grid.height,
            GRID_BLOCKED,
        );
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

/// `relative(pos, direction, n)`.
pub(crate) fn relative(pos: Pos, direction: Dir, n: i32) -> Pos {
    (
        pos.0.wrapping_add(direction.step_x() * n),
        pos.1,
        pos.2.wrapping_add(direction.step_z() * n),
    )
}

/// `BlockPos.above(n)`.
pub(crate) fn above(pos: Pos, n: i32) -> Pos {
    (pos.0, pos.1.wrapping_add(n), pos.2)
}

/// `WoodlandMansionPieces.PlacementData`.
#[derive(Clone, Debug)]
struct MansionPlacement {
    rotation: Rotation,
    pos: Pos,
    wall_type: &'static str,
}

/// `WoodlandMansionPieces.MansionPiecePlacer`.
struct MansionPlacer<'a> {
    templates: &'a mut Templates,
    reg: &'a BReg,
    rng: &'a mut Lcg,
    start_x: i32,
    start_y: i32,
}

impl MansionPlacer<'_> {
    fn piece(
        &mut self,
        name: &str,
        pos: Pos,
        rotation: Rotation,
        mirror: Mirror,
    ) -> Result<TemplatePiece, &'static str> {
        let template_id = format!("minecraft:woodland_mansion/{name}");
        let template = self.templates.get(&template_id, self.reg)?;
        let bb = template_bb(&template, pos, mirror, rotation, (0, 0, 0));
        Ok(TemplatePiece {
            template: template_id,
            pos,
            bb,
            rotation,
            mirror,
            // `WoodlandMansionPiece.makeSettings` always ignores only
            // `minecraft:structure_block`, so air cells are written.
            ignore_air: false,
            gen_depth: 0,
        })
    }

    fn push(
        &mut self,
        list: &mut Vec<TemplatePiece>,
        name: &str,
        pos: Pos,
        rotation: Rotation,
        mirror: Mirror,
    ) -> Result<(), &'static str> {
        let piece = self.piece(name, pos, rotation, mirror)?;
        list.push(piece);
        Ok(())
    }

    /// `entrance`.
    fn entrance(
        &mut self,
        list: &mut Vec<TemplatePiece>,
        data: &mut MansionPlacement,
    ) -> Result<(), &'static str> {
        let direction = rotate_direction(data.rotation, Dir::West);
        let pos = relative(data.pos, direction, 9);
        let rotation = data.rotation;
        self.push(list, "entrance", pos, rotation, Mirror::None)?;
        data.pos = relative(data.pos, rotate_direction(data.rotation, Dir::South), 16);
        Ok(())
    }

    /// `traverseWallPiece`.
    fn traverse_wall_piece(
        &mut self,
        list: &mut Vec<TemplatePiece>,
        data: &mut MansionPlacement,
    ) -> Result<(), &'static str> {
        let pos = relative(data.pos, rotate_direction(data.rotation, Dir::East), 7);
        let rotation = data.rotation;
        let wall_type = data.wall_type;
        self.push(list, wall_type, pos, rotation, Mirror::None)?;
        data.pos = relative(data.pos, rotate_direction(data.rotation, Dir::South), 8);
        Ok(())
    }

    /// `traverseTurn`.
    fn traverse_turn(
        &mut self,
        list: &mut Vec<TemplatePiece>,
        data: &mut MansionPlacement,
    ) -> Result<(), &'static str> {
        data.pos = relative(data.pos, rotate_direction(data.rotation, Dir::South), -1);
        let pos = data.pos;
        let rotation = data.rotation;
        self.push(list, "wall_corner", pos, rotation, Mirror::None)?;
        data.pos = relative(data.pos, rotate_direction(data.rotation, Dir::South), -7);
        data.pos = relative(data.pos, rotate_direction(data.rotation, Dir::West), -6);
        data.rotation = rotate_rotation(data.rotation, Rotation::Clockwise90);
        Ok(())
    }

    /// `traverseInnerTurn`.
    fn traverse_inner_turn(
        &mut self,
        list: &mut Vec<TemplatePiece>,
        data: &mut MansionPlacement,
    ) -> Result<(), &'static str> {
        data.pos = relative(data.pos, rotate_direction(data.rotation, Dir::South), 6);
        data.pos = relative(data.pos, rotate_direction(data.rotation, Dir::East), 8);
        data.rotation = rotate_rotation(data.rotation, Rotation::Counterclockwise90);
        Ok(())
    }

    /// `traverseOuterWalls`: walk the outside of a room cluster, emitting wall
    /// pieces and turns until the start point and heading come round again.
    #[allow(clippy::too_many_arguments)]
    fn traverse_outer_walls(
        &mut self,
        list: &mut Vec<TemplatePiece>,
        data: &mut MansionPlacement,
        grid: &SimpleGrid,
        direction: Dir,
        start_x: i32,
        start_y: i32,
        end_x: i32,
        end_y: i32,
    ) -> Result<(), &'static str> {
        let mut x = start_x;
        let mut y = start_y;
        let mut heading = direction;
        let initial = direction;
        loop {
            if !grid_is_house(grid, x + heading.step_x(), y + heading.step_z()) {
                self.traverse_turn(list, data)?;
                heading = heading.clock_wise();
                if x == end_x && y == end_y && initial == heading {
                    return Ok(());
                }
                self.traverse_wall_piece(list, data)?;
            } else if grid_is_house(
                grid,
                x + heading.step_x() + heading.counter_clock_wise().step_x(),
                y + heading.step_z() + heading.counter_clock_wise().step_z(),
            ) {
                self.traverse_inner_turn(list, data)?;
                x += heading.step_x();
                y += heading.step_z();
                heading = heading.counter_clock_wise();
            } else {
                x += heading.step_x();
                y += heading.step_z();
                if x == end_x && y == end_y && initial == heading {
                    return Ok(());
                }
                self.traverse_wall_piece(list, data)?;
            }
            if x == end_x && y == end_y && initial == heading {
                return Ok(());
            }
        }
    }

    /// `createRoof`: the flat roof slabs plus the corner trim, for the base
    /// floors (`third` present) and for the third floor itself (`third` null).
    fn create_roof(
        &mut self,
        list: &mut Vec<TemplatePiece>,
        offset: Pos,
        rotation: Rotation,
        grid: &SimpleGrid,
        third: Option<&SimpleGrid>,
    ) -> Result<(), &'static str> {
        let south = rotate_direction(rotation, Dir::South);
        let east = rotate_direction(rotation, Dir::East);
        let west = rotate_direction(rotation, Dir::West);
        let north = rotate_direction(rotation, Dir::North);
        for i in 0..grid.height {
            for j in 0..grid.width {
                let mut pos = relative(offset, south, 8 + (i - self.start_y) * 8);
                pos = relative(pos, east, (j - self.start_x) * 8);
                let covered = third.is_some_and(|third| grid_is_house(third, j, i));
                if !grid_is_house(grid, j, i) || covered {
                    continue;
                }
                self.push(list, "roof", above(pos, 3), rotation, Mirror::None)?;
                if !grid_is_house(grid, j + 1, i) {
                    let p = relative(pos, east, 6);
                    self.push(list, "roof_front", p, rotation, Mirror::None)?;
                }
                if !grid_is_house(grid, j - 1, i) {
                    let p = relative(pos, south, 7);
                    self.push(
                        list,
                        "roof_front",
                        p,
                        rotate_rotation(rotation, Rotation::Clockwise180),
                        Mirror::None,
                    )?;
                }
                if !grid_is_house(grid, j, i - 1) {
                    let p = relative(pos, west, 1);
                    self.push(
                        list,
                        "roof_front",
                        p,
                        rotate_rotation(rotation, Rotation::Counterclockwise90),
                        Mirror::None,
                    )?;
                }
                if !grid_is_house(grid, j, i + 1) {
                    let p = relative(relative(pos, east, 6), south, 6);
                    self.push(
                        list,
                        "roof_front",
                        p,
                        rotate_rotation(rotation, Rotation::Clockwise90),
                        Mirror::None,
                    )?;
                }
            }
        }
        let third_option = third;
        let third = match third {
            // `createRoof` jumps straight to the corner pass when the third
            // floor grid is absent, so only the `small_wall` pass is skipped.
            None => return self.create_roof_corners(list, offset, rotation, grid, None),
            Some(third) => third,
        };
        for i in 0..grid.height {
            for j in 0..grid.width {
                let mut pos = relative(offset, south, 8 + (i - self.start_y) * 8);
                pos = relative(pos, east, (j - self.start_x) * 8);
                if !grid_is_house(grid, j, i) || !grid_is_house(third, j, i) {
                    continue;
                }
                if !grid_is_house(grid, j + 1, i) {
                    let p = relative(pos, east, 7);
                    self.push(list, "small_wall", p, rotation, Mirror::None)?;
                }
                if !grid_is_house(grid, j - 1, i) {
                    let p = relative(relative(pos, west, 1), south, 6);
                    self.push(
                        list,
                        "small_wall",
                        p,
                        rotate_rotation(rotation, Rotation::Clockwise180),
                        Mirror::None,
                    )?;
                }
                if !grid_is_house(grid, j, i - 1) {
                    // `relative(rotate(WEST), 0)` is a no-op offset, then one
                    // block north.
                    let p = relative(pos, north, 1);
                    self.push(
                        list,
                        "small_wall",
                        p,
                        rotate_rotation(rotation, Rotation::Counterclockwise90),
                        Mirror::None,
                    )?;
                }
                if !grid_is_house(grid, j, i + 1) {
                    let p = relative(relative(pos, east, 6), south, 7);
                    self.push(
                        list,
                        "small_wall",
                        p,
                        rotate_rotation(rotation, Rotation::Clockwise90),
                        Mirror::None,
                    )?;
                }
                if !grid_is_house(grid, j + 1, i) {
                    if !grid_is_house(grid, j, i - 1) {
                        let p = relative(relative(pos, east, 7), north, 2);
                        self.push(list, "small_wall_corner", p, rotation, Mirror::None)?;
                    }
                    if !grid_is_house(grid, j, i + 1) {
                        let p = relative(relative(pos, east, 8), south, 7);
                        self.push(
                            list,
                            "small_wall_corner",
                            p,
                            rotate_rotation(rotation, Rotation::Clockwise90),
                            Mirror::None,
                        )?;
                    }
                }
                if !grid_is_house(grid, j - 1, i) {
                    if !grid_is_house(grid, j, i - 1) {
                        let p = relative(relative(pos, west, 2), north, 1);
                        self.push(
                            list,
                            "small_wall_corner",
                            p,
                            rotate_rotation(rotation, Rotation::Counterclockwise90),
                            Mirror::None,
                        )?;
                    }
                    if !grid_is_house(grid, j, i + 1) {
                        let p = relative(relative(pos, west, 1), south, 8);
                        self.push(
                            list,
                            "small_wall_corner",
                            p,
                            rotate_rotation(rotation, Rotation::Clockwise180),
                            Mirror::None,
                        )?;
                    }
                }
            }
        }
        self.create_roof_corners(list, offset, rotation, grid, third_option)
    }

    /// `createRoof`'s corner pass: `roof_corner` / `roof_inner_corner` around
    /// every covered cell, skipped where the third floor already covers it.
    fn create_roof_corners(
        &mut self,
        list: &mut Vec<TemplatePiece>,
        offset: Pos,
        rotation: Rotation,
        grid: &SimpleGrid,
        third: Option<&SimpleGrid>,
    ) -> Result<(), &'static str> {
        let south = rotate_direction(rotation, Dir::South);
        let east = rotate_direction(rotation, Dir::East);
        let west = rotate_direction(rotation, Dir::West);
        let north = rotate_direction(rotation, Dir::North);
        for i in 0..grid.height {
            for j in 0..grid.width {
                let mut pos = relative(offset, south, 8 + (i - self.start_y) * 8);
                pos = relative(pos, east, (j - self.start_x) * 8);
                let covered = third.is_some_and(|third| grid_is_house(third, j, i));
                if !grid_is_house(grid, j, i) || covered {
                    continue;
                }
                if !grid_is_house(grid, j + 1, i) {
                    let near = relative(pos, east, 6);
                    if !grid_is_house(grid, j, i + 1) {
                        let p = relative(near, south, 6);
                        self.push(list, "roof_corner", p, rotation, Mirror::None)?;
                    } else if grid_is_house(grid, j + 1, i + 1) {
                        let p = relative(near, south, 5);
                        self.push(list, "roof_inner_corner", p, rotation, Mirror::None)?;
                    }
                    if !grid_is_house(grid, j, i - 1) {
                        self.push(
                            list,
                            "roof_corner",
                            near,
                            rotate_rotation(rotation, Rotation::Counterclockwise90),
                            Mirror::None,
                        )?;
                    } else if grid_is_house(grid, j + 1, i - 1) {
                        let p = relative(relative(pos, east, 9), north, 2);
                        self.push(
                            list,
                            "roof_inner_corner",
                            p,
                            rotate_rotation(rotation, Rotation::Clockwise90),
                            Mirror::None,
                        )?;
                    }
                }
                if !grid_is_house(grid, j - 1, i) {
                    if !grid_is_house(grid, j, i + 1) {
                        let p = relative(pos, south, 6);
                        self.push(
                            list,
                            "roof_corner",
                            p,
                            rotate_rotation(rotation, Rotation::Clockwise90),
                            Mirror::None,
                        )?;
                    } else if grid_is_house(grid, j - 1, i + 1) {
                        let p = relative(relative(pos, south, 8), west, 3);
                        self.push(
                            list,
                            "roof_inner_corner",
                            p,
                            rotate_rotation(rotation, Rotation::Counterclockwise90),
                            Mirror::None,
                        )?;
                    }
                    if !grid_is_house(grid, j, i - 1) {
                        self.push(
                            list,
                            "roof_corner",
                            pos,
                            rotate_rotation(rotation, Rotation::Clockwise180),
                            Mirror::None,
                        )?;
                    } else if grid_is_house(grid, j - 1, i - 1) {
                        let p = relative(pos, south, 1);
                        self.push(
                            list,
                            "roof_inner_corner",
                            p,
                            rotate_rotation(rotation, Rotation::Clockwise180),
                            Mirror::None,
                        )?;
                    }
                }
            }
        }
        Ok(())
    }

    /// `addRoom1x1`.
    fn add_room_1x1(
        &mut self,
        list: &mut Vec<TemplatePiece>,
        pos: Pos,
        rotation: Rotation,
        direction: Option<Dir>,
        floor: MansionFloor,
    ) -> Result<(), &'static str> {
        let mut room_rotation = Rotation::None;
        let mut name = floor.one_by_one(self.rng);
        match direction {
            Some(Dir::East) => {}
            Some(Dir::North) => {
                room_rotation = rotate_rotation(room_rotation, Rotation::Counterclockwise90)
            }
            Some(Dir::West) => {
                room_rotation = rotate_rotation(room_rotation, Rotation::Clockwise180)
            }
            Some(Dir::South) => {
                room_rotation = rotate_rotation(room_rotation, Rotation::Clockwise90)
            }
            _ => name = floor.one_by_one_secret(self.rng),
        }
        // `StructureTemplate.calculateRelativePosition(new BlockPos(1, 0, 0),
        // Mirror.NONE, roomRotation, 7, 7)`: a 7x7 room anchored one cell in,
        // rotated so its door faces `direction`.
        let p = relative_room_corner((1, 0, 0), room_rotation, 7, 7);
        room_rotation = rotate_rotation(room_rotation, rotation);
        let p = rotate_pos(p, rotation);
        let placed = (pos.0.wrapping_add(p.0), pos.1, pos.2.wrapping_add(p.2));
        self.push(list, &name, placed, room_rotation, Mirror::None)
    }

    /// `addRoom1x2`.
    #[allow(clippy::too_many_arguments)]
    fn add_room_1x2(
        &mut self,
        list: &mut Vec<TemplatePiece>,
        pos: Pos,
        rotation: Rotation,
        room_direction: Option<Dir>,
        door_direction: Option<Dir>,
        floor: MansionFloor,
        stairs: bool,
    ) -> Result<(), &'static str> {
        let (Some(room_direction), Some(door_direction)) = (room_direction, door_direction) else {
            return Ok(());
        };
        let east = rotate_direction(rotation, Dir::East);
        let south = rotate_direction(rotation, Dir::South);
        let north = rotate_direction(rotation, Dir::North);
        let west = rotate_direction(rotation, Dir::West);
        match (door_direction, room_direction) {
            (Dir::East, Dir::South) => {
                let name = floor.one_by_two_side(self.rng, stairs);
                let p = relative(pos, east, 1);
                self.push(list, &name, p, rotation, Mirror::None)
            }
            (Dir::East, Dir::North) => {
                let name = floor.one_by_two_side(self.rng, stairs);
                let p = relative(relative(pos, east, 1), south, 6);
                self.push(list, &name, p, rotation, Mirror::from_index(1).unwrap())
            }
            (Dir::West, Dir::North) => {
                let name = floor.one_by_two_side(self.rng, stairs);
                let p = relative(relative(pos, east, 7), south, 6);
                self.push(
                    list,
                    &name,
                    p,
                    rotate_rotation(rotation, Rotation::Clockwise180),
                    Mirror::None,
                )
            }
            (Dir::West, Dir::South) => {
                let name = floor.one_by_two_side(self.rng, stairs);
                let p = relative(pos, east, 7);
                self.push(list, &name, p, rotation, Mirror::from_index(2).unwrap())
            }
            (Dir::South, Dir::East) => {
                let name = floor.one_by_two_side(self.rng, stairs);
                let p = relative(pos, east, 1);
                self.push(
                    list,
                    &name,
                    p,
                    rotate_rotation(rotation, Rotation::Clockwise90),
                    Mirror::from_index(1).unwrap(),
                )
            }
            (Dir::South, Dir::West) => {
                let name = floor.one_by_two_side(self.rng, stairs);
                let p = relative(pos, east, 7);
                self.push(
                    list,
                    &name,
                    p,
                    rotate_rotation(rotation, Rotation::Clockwise90),
                    Mirror::None,
                )
            }
            (Dir::North, Dir::West) => {
                let name = floor.one_by_two_side(self.rng, stairs);
                let p = relative(relative(pos, east, 7), south, 6);
                self.push(
                    list,
                    &name,
                    p,
                    rotate_rotation(rotation, Rotation::Clockwise90),
                    Mirror::from_index(2).unwrap(),
                )
            }
            (Dir::North, Dir::East) => {
                let name = floor.one_by_two_side(self.rng, stairs);
                let p = relative(relative(pos, east, 1), south, 6);
                // No mirror: the bytecode reaches the four-argument
                // `WoodlandMansionPiece` constructor here, unlike the other
                // three side-entrance arms that pass `Mirror.LEFT_RIGHT` or
                // `Mirror.FRONT_BACK`.
                self.push(
                    list,
                    &name,
                    p,
                    rotate_rotation(rotation, Rotation::Counterclockwise90),
                    Mirror::None,
                )
            }
            // `get1x2FrontEntrance`: the door and the room direction are the
            // two horizontal axes, which is what makes the entrance face the
            // corridor the room's origin corner touches.
            (Dir::South, Dir::North) => {
                let name = floor.one_by_two_front(self.rng, stairs);
                let p = relative(relative(pos, east, 1), north, 8);
                self.push(list, &name, p, rotation, Mirror::None)
            }
            (Dir::North, Dir::South) => {
                let name = floor.one_by_two_front(self.rng, stairs);
                let p = relative(relative(pos, east, 7), south, 14);
                self.push(
                    list,
                    &name,
                    p,
                    rotate_rotation(rotation, Rotation::Clockwise180),
                    Mirror::None,
                )
            }
            (Dir::West, Dir::East) => {
                let name = floor.one_by_two_front(self.rng, stairs);
                let p = relative(pos, east, 15);
                self.push(
                    list,
                    &name,
                    p,
                    rotate_rotation(rotation, Rotation::Clockwise90),
                    Mirror::None,
                )
            }
            (Dir::East, Dir::West) => {
                let name = floor.one_by_two_front(self.rng, stairs);
                let p = relative(relative(pos, west, 7), south, 6);
                self.push(
                    list,
                    &name,
                    p,
                    rotate_rotation(rotation, Rotation::Counterclockwise90),
                    Mirror::None,
                )
            }
            // `get1x2Secret`: the origin flag with no corridor neighbour.
            (Dir::Up, Dir::East) => {
                let name = floor.one_by_two_secret(self.rng);
                let p = relative(pos, east, 15);
                self.push(
                    list,
                    &name,
                    p,
                    rotate_rotation(rotation, Rotation::Clockwise90),
                    Mirror::None,
                )
            }
            (Dir::Up, Dir::South) => {
                let name = floor.one_by_two_secret(self.rng);
                let p = relative(relative(pos, east, 1), north, 0);
                self.push(list, &name, p, rotation, Mirror::None)
            }
            _ => Ok(()),
        }
    }

    /// `addRoom2x2`.
    #[allow(clippy::too_many_arguments)]
    fn add_room_2x2(
        &mut self,
        list: &mut Vec<TemplatePiece>,
        pos: Pos,
        rotation: Rotation,
        room_direction: Option<Dir>,
        door_direction: Option<Dir>,
        floor: MansionFloor,
    ) -> Result<(), &'static str> {
        let (Some(room_direction), Some(door_direction)) = (room_direction, door_direction) else {
            return Ok(());
        };
        let mut dx = 0;
        let mut dz = 0;
        let mut room_rotation = rotation;
        let mut mirror = Mirror::None;
        match (door_direction, room_direction) {
            (Dir::East, Dir::South) => dx = -7,
            (Dir::East, Dir::North) => {
                dx = -7;
                dz = 6;
                mirror = Mirror::from_index(1).unwrap();
            }
            (Dir::North, Dir::East) => {
                dx = 1;
                dz = 14;
                room_rotation = rotate_rotation(rotation, Rotation::Counterclockwise90);
            }
            (Dir::North, Dir::West) => {
                dx = 7;
                dz = 14;
                room_rotation = rotate_rotation(rotation, Rotation::Counterclockwise90);
                mirror = Mirror::from_index(1).unwrap();
            }
            (Dir::South, Dir::West) => {
                dx = 7;
                dz = -8;
                room_rotation = rotate_rotation(rotation, Rotation::Clockwise90);
            }
            (Dir::South, Dir::East) => {
                dx = 1;
                dz = -8;
                room_rotation = rotate_rotation(rotation, Rotation::Clockwise90);
                mirror = Mirror::from_index(1).unwrap();
            }
            (Dir::West, Dir::North) => {
                dx = 15;
                dz = 6;
                room_rotation = rotate_rotation(rotation, Rotation::Clockwise180);
            }
            (Dir::West, Dir::South) => {
                dx = 15;
                mirror = Mirror::from_index(2).unwrap();
            }
            _ => return Ok(()),
        }
        let name = floor.two_by_two(self.rng);
        let p = relative(
            relative(pos, rotate_direction(rotation, Dir::East), dx),
            rotate_direction(rotation, Dir::South),
            dz,
        );
        self.push(list, &name, p, room_rotation, mirror)
    }

    /// `addRoom2x2Secret`.
    fn add_room_2x2_secret(
        &mut self,
        list: &mut Vec<TemplatePiece>,
        pos: Pos,
        rotation: Rotation,
        floor: MansionFloor,
    ) -> Result<(), &'static str> {
        let name = floor.two_by_two_secret(self.rng);
        let p = relative(pos, rotate_direction(rotation, Dir::East), 1);
        self.push(list, &name, p, rotation, Mirror::None)
    }
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
    templates: &mut Templates,
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
        start_x: grid.entrance_x + 1,
        start_y: grid.entrance_y + 1,
    };
    let mut list: Vec<TemplatePiece> = Vec::new();
    let mut ground = MansionPlacement {
        rotation,
        pos,
        wall_type: "wall_flat",
    };
    let mut windows = MansionPlacement {
        rotation,
        pos,
        wall_type: "wall_window",
    };
    placer.entrance(&mut list, &mut ground)?;
    windows.pos = above(ground.pos, 8);
    windows.rotation = ground.rotation;
    windows.wall_type = "wall_window";
    let base = &grid.base_grid;
    let third = &grid.third_floor_grid;
    let start_x = placer.start_x;
    let start_y = placer.start_y;
    let end_x = grid.entrance_x + 1;
    let end_y = grid.entrance_y;
    placer.traverse_outer_walls(
        &mut list,
        &mut ground,
        base,
        Dir::South,
        start_x,
        start_y,
        end_x,
        end_y,
    )?;
    placer.traverse_outer_walls(
        &mut list,
        &mut windows,
        base,
        Dir::South,
        start_x,
        start_y,
        end_x,
        end_y,
    )?;
    let mut upper = MansionPlacement {
        rotation: ground.rotation,
        pos: above(ground.pos, 19),
        wall_type: "wall_window",
    };
    'search: for k in 0..third.height {
        let mut l = third.width - 1;
        while l >= 0 {
            if grid_is_house(third, l, k) {
                upper.pos = relative(
                    upper.pos,
                    rotate_direction(rotation, Dir::South),
                    8 + (k - placer.start_y) * 8,
                );
                upper.pos = relative(
                    upper.pos,
                    rotate_direction(rotation, Dir::East),
                    (l - placer.start_x) * 8,
                );
                placer.traverse_wall_piece(&mut list, &mut upper)?;
                placer.traverse_outer_walls(
                    &mut list,
                    &mut upper,
                    third,
                    Dir::South,
                    l,
                    k,
                    l,
                    k,
                )?;
                break 'search;
            }
            l -= 1;
        }
    }
    placer.create_roof(&mut list, above(pos, 16), rotation, base, Some(third))?;
    placer.create_roof(&mut list, above(pos, 27), rotation, third, None)?;

    for floor_index in 0..3usize {
        let floor_kind = match floor_index {
            0 => MansionFloor::First,
            1 => MansionFloor::Second,
            _ => MansionFloor::Third,
        };
        let floor_pos = above(
            pos,
            8 * floor_index as i32 + if floor_index == 2 { 3 } else { 0 },
        );
        let room_grid = &grid.floor_rooms[floor_index];
        let walk_grid = if floor_index == 2 { third } else { base };
        let carpet_south = if floor_index == 0 {
            "carpet_south_1"
        } else {
            "carpet_south_2"
        };
        let carpet_west = if floor_index == 0 {
            "carpet_west_1"
        } else {
            "carpet_west_2"
        };
        for n in 0..walk_grid.height {
            for o in 0..walk_grid.width {
                if walk_grid.get(o, n) != GRID_CORRIDOR {
                    continue;
                }
                let mut p = relative(
                    floor_pos,
                    rotate_direction(rotation, Dir::South),
                    8 + (n - placer.start_y) * 8,
                );
                p = relative(
                    p,
                    rotate_direction(rotation, Dir::East),
                    (o - placer.start_x) * 8,
                );
                placer.push(&mut list, "corridor_floor", p, rotation, Mirror::None)?;
                if walk_grid.get(o, n - 1) == GRID_CORRIDOR
                    || (room_grid.get(o, n - 1) & ROOM_CORRIDOR_FLAG) == ROOM_CORRIDOR_FLAG
                {
                    let carpet = relative(p, rotate_direction(rotation, Dir::East), 1);
                    placer.push(
                        &mut list,
                        "carpet_north",
                        above(carpet, 1),
                        rotation,
                        Mirror::None,
                    )?;
                }
                if walk_grid.get(o + 1, n) == GRID_CORRIDOR
                    || (room_grid.get(o + 1, n) & ROOM_CORRIDOR_FLAG) == ROOM_CORRIDOR_FLAG
                {
                    let mut carpet = relative(p, rotate_direction(rotation, Dir::South), 1);
                    carpet = relative(carpet, rotate_direction(rotation, Dir::East), 5);
                    placer.push(
                        &mut list,
                        "carpet_east",
                        above(carpet, 1),
                        rotation,
                        Mirror::None,
                    )?;
                }
                if walk_grid.get(o, n + 1) == GRID_CORRIDOR
                    || (room_grid.get(o, n + 1) & ROOM_CORRIDOR_FLAG) == ROOM_CORRIDOR_FLAG
                {
                    let mut carpet = relative(p, rotate_direction(rotation, Dir::South), 5);
                    carpet = relative(carpet, rotate_direction(rotation, Dir::West), 1);
                    placer.push(&mut list, carpet_south, carpet, rotation, Mirror::None)?;
                }
                if walk_grid.get(o - 1, n) == GRID_CORRIDOR
                    || (room_grid.get(o - 1, n) & ROOM_CORRIDOR_FLAG) == ROOM_CORRIDOR_FLAG
                {
                    let mut carpet = relative(p, rotate_direction(rotation, Dir::West), 1);
                    carpet = relative(carpet, rotate_direction(rotation, Dir::North), 1);
                    placer.push(&mut list, carpet_west, carpet, rotation, Mirror::None)?;
                }
            }
        }
        let indoors_wall = if floor_index == 0 {
            "indoors_wall_1"
        } else {
            "indoors_wall_2"
        };
        let indoors_door = if floor_index == 0 {
            "indoors_door_1"
        } else {
            "indoors_door_2"
        };
        let mut doors: Vec<Dir> = Vec::new();
        for p in 0..walk_grid.height {
            for q in 0..walk_grid.width {
                let mut stair_landing = floor_index == 2 && walk_grid.get(q, p) == GRID_START_ROOM;
                if walk_grid.get(q, p) != GRID_ROOM && !stair_landing {
                    continue;
                }
                let record = room_grid.get(q, p);
                let room_type = record & ROOM_TYPE_MASK;
                let room_id = record & ROOM_ID_MASK;
                stair_landing =
                    stair_landing && (record & ROOM_CORRIDOR_FLAG) == ROOM_CORRIDOR_FLAG;
                doors.clear();
                if (record & ROOM_DOOR_FLAG) == ROOM_DOOR_FLAG {
                    for direction in HORIZONTAL {
                        if walk_grid.get(q + direction.step_x(), p + direction.step_z())
                            == GRID_CORRIDOR
                        {
                            doors.push(direction);
                        }
                    }
                }
                let mut door: Option<Dir> = None;
                if !doors.is_empty() {
                    door = Some(doors[placer.rng.next_bound(doors.len() as i32) as usize]);
                } else if (record & ROOM_ORIGIN_FLAG) == ROOM_ORIGIN_FLAG {
                    door = Some(Dir::Up);
                }
                let mut anchor = relative(
                    floor_pos,
                    rotate_direction(rotation, Dir::South),
                    8 + (p - placer.start_y) * 8,
                );
                anchor = relative(
                    anchor,
                    rotate_direction(rotation, Dir::East),
                    -1 + (q - placer.start_x) * 8,
                );
                if grid_is_house(walk_grid, q - 1, p)
                    && !grid.is_room_id(walk_grid, q - 1, p, floor_index, room_id)
                {
                    let name = if door == Some(Dir::West) {
                        indoors_door
                    } else {
                        indoors_wall
                    };
                    placer.push(&mut list, name, anchor, rotation, Mirror::None)?;
                }
                if walk_grid.get(q + 1, p) == GRID_CORRIDOR && !stair_landing {
                    let p5 = relative(anchor, rotate_direction(rotation, Dir::East), 8);
                    let name = if door == Some(Dir::East) {
                        indoors_door
                    } else {
                        indoors_wall
                    };
                    placer.push(&mut list, name, p5, rotation, Mirror::None)?;
                }
                if grid_is_house(walk_grid, q, p + 1)
                    && !grid.is_room_id(walk_grid, q, p + 1, floor_index, room_id)
                {
                    let mut p6 = relative(anchor, rotate_direction(rotation, Dir::South), 7);
                    p6 = relative(p6, rotate_direction(rotation, Dir::East), 7);
                    let name = if door == Some(Dir::South) {
                        indoors_door
                    } else {
                        indoors_wall
                    };
                    placer.push(
                        &mut list,
                        name,
                        p6,
                        rotate_rotation(rotation, Rotation::Clockwise90),
                        Mirror::None,
                    )?;
                }
                if walk_grid.get(q, p - 1) == GRID_CORRIDOR && !stair_landing {
                    let mut p7 = relative(anchor, rotate_direction(rotation, Dir::North), 1);
                    p7 = relative(p7, rotate_direction(rotation, Dir::East), 7);
                    let name = if door == Some(Dir::North) {
                        indoors_door
                    } else {
                        indoors_wall
                    };
                    placer.push(
                        &mut list,
                        name,
                        p7,
                        rotate_rotation(rotation, Rotation::Clockwise90),
                        Mirror::None,
                    )?;
                }
                if room_type == ROOM_1X1 {
                    placer.add_room_1x1(&mut list, anchor, rotation, door, floor_kind)?;
                } else if room_type == ROOM_1X2 && door.is_some() {
                    let room_direction = grid.one_by_two_direction(q, p, floor_index, room_id);
                    let stairs = (record & ROOM_STAIRS_FLAG) == ROOM_STAIRS_FLAG;
                    placer.add_room_1x2(
                        &mut list,
                        anchor,
                        rotation,
                        room_direction,
                        door,
                        floor_kind,
                        stairs,
                    )?;
                } else if room_type == ROOM_2X2 && door.is_some() && door != Some(Dir::Up) {
                    let mut room_direction = door.expect("door").clock_wise();
                    if !grid.is_room_id(
                        walk_grid,
                        q + room_direction.step_x(),
                        p + room_direction.step_z(),
                        floor_index,
                        room_id,
                    ) {
                        room_direction = room_direction.opposite();
                    }
                    placer.add_room_2x2(
                        &mut list,
                        anchor,
                        rotation,
                        Some(room_direction),
                        door,
                        floor_kind,
                    )?;
                } else if room_type == ROOM_2X2 && door == Some(Dir::Up) {
                    placer.add_room_2x2_secret(&mut list, anchor, rotation, floor_kind)?;
                }
            }
        }
    }
    Ok(list)
}

/// `WoodlandMansionPieces.generateMansion`.
pub(crate) fn generate_mansion(
    templates: &mut Templates,
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
            if metadata.starts_with("Chest") {
                let below = (pos.0, pos.1 - 1, pos.2);
                // The marker is only delivered when the cell below is inside the
                // chunk being placed.
                if bb_inside(chunk_bb, below) {
                    end_city_chest_loot(out, below, rng);
                }
                return;
            }
            if !bb_inside(chunk_bb, pos) {
                return;
            }
            if !in_spawnable_bounds(world, pos) {
                return;
            }
            if metadata.starts_with("Sentry") {
                out.entities.push(EntitySpawn {
                    pos,
                    id: "minecraft:shulker".to_string(),
                });
            } else if metadata.starts_with("Elytra") {
                let _ = rotate_direction(rotation, Dir::South);
                out.entities.push(EntitySpawn {
                    pos,
                    id: "minecraft:item_frame".to_string(),
                });
            }
        }
        StructureFlavour::Mansion => {
            if metadata.starts_with("Chest") {
                let facing = if metadata == "ChestWest" {
                    rotate_direction(rotation, Dir::West)
                } else if metadata == "ChestEast" {
                    rotate_direction(rotation, Dir::East)
                } else if metadata == "ChestSouth" {
                    rotate_direction(rotation, Dir::South)
                } else if metadata == "ChestNorth" {
                    rotate_direction(rotation, Dir::North)
                } else {
                    rotate_direction(rotation, Dir::South)
                };
                mansion_create_chest(world, reg, out, chunk_bb, rng, pos, facing);
                return;
            }
            let mut spawned = 0;
            match metadata {
                "Mage" => {
                    out.entities.push(EntitySpawn {
                        pos,
                        id: "minecraft:evoker".to_string(),
                    });
                    spawned = 1;
                }
                "Warrior" => {
                    out.entities.push(EntitySpawn {
                        pos,
                        id: "minecraft:vindicator".to_string(),
                    });
                    spawned = 1;
                }
                "Group of Allays" => {
                    let count = rng.next_bound(3) + 1;
                    for _ in 0..count {
                        out.entities.push(EntitySpawn {
                            pos,
                            id: "minecraft:allay".to_string(),
                        });
                    }
                    spawned = count;
                }
                _ => return,
            }
            for _ in 0..spawned {
                if !world.set(pos.0, pos.1, pos.2, reg.state_of("minecraft:air", None)) {
                    continue;
                }
                out.writes += 1;
                out.be_events.push(BeEvent::Clear(pos));
            }
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
    templates: &mut Templates,
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
pub(crate) fn place_chunk<W: TemplateWorld>(
    world: &mut W,
    templates: &mut Templates,
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
    templates: &mut Templates,
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
    templates: &mut Templates,
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
            if !bb_inside(&assembled, (x, min_y, z)) {
                continue;
            }
            if !pieces
                .iter()
                .any(|piece| bb_inside(&piece.bb, (x, min_y, z)))
            {
                continue;
            }
            let mut y = min_y - 1;
            while y > sea_level {
                let inside = y >= world.min_y() && y < world.min_y() + world.height();
                if inside && !is_air_state(reg, world.get(x, y, z)) {
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

#[cfg(test)]
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

    fn point(value: &J) -> Pos {
        let a = value.as_arr().expect("point");
        (a[0].num() as i32, a[1].num() as i32, a[2].num() as i32)
    }

    fn box_of(value: &J) -> BB {
        let a = value.as_arr().expect("box");
        [
            a[0].num() as i32,
            a[1].num() as i32,
            a[2].num() as i32,
            a[3].num() as i32,
            a[4].num() as i32,
            a[5].num() as i32,
        ]
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
        water: u32,
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
                water: reg.state_of("minecraft:water", None),
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
        map.into_iter()
            .filter_map(|(_, record)| record)
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
        templates: &mut Templates,
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
        if ent.len() != want_ent_count {
            row_failure(&format!("{id} entity count"), ent.len(), want_ent_count);
            eprintln!("  entity lines: {ent:?}");
            failures += 1;
        }
        if ent_hash != want_ent {
            row_failure(&format!("{id} entity hash"), &ent_hash, want_ent);
            failures += 1;
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
        let mut templates =
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
            failures += run_case(case, &row, &borrowed, grids, bes, reg, &mut templates);
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
            water: reg.state_of("minecraft:water", None),
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
        let mut templates =
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
            &mut templates,
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
        let cobblestone = reg.state_of("minecraft:cobblestone", None);
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
}
