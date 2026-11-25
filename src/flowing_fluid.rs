//! FlowingFluid's scheduled update for full-cube, air, replaceable and liquid cells.
//!
//! A cell with a **non-empty, non-full** collision shape still needs a native shape contract;
//! callers receive an error before committing writes. A cell whose collision shape is empty
//! (a plant, `minecraft:short_grass` in the
//! measured case) is modelled: `FlowingFluid.canPassThrough` intersects the two collision
//! shapes and an empty one intersects nothing, while `canHoldFluid` answers
//! `!state.blocksMotion()` for every block outside its sign/door/ladder/sugar-cane/bubble-column
//! and portal/structure-void families, so the fluid enters and `spreadTo` destroys the block
//! before writing itself.
use megu_core::coords::CoordMap;

pub(crate) type Pos = (i32, i32, i32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Kind {
    Water,
    Lava,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Fluid {
    pub kind: Kind,
    pub level: u8,
}

impl Fluid {
    pub(crate) fn source(self) -> bool {
        self.level == 0
    }
    fn falling(self) -> bool {
        self.level >= 8
    }
    fn amount(self) -> u8 {
        if self.source() || self.falling() {
            8
        } else {
            8 - self.level
        }
    }
}

/// `DimensionType.ultraWarm` as a function of the protocol dimension index
/// (Overworld 0, Nether -1, End 1): the only vanilla dimension where lava
/// flows at water speed and reach.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn lava_spreads_fast_in_dim(dim: i32) -> bool {
    dim == -1
}

/// `LavaFluid.getTickDelay`: 10 game ticks in an ultrawarm dimension, 30 game
/// ticks everywhere else.
#[cfg_attr(any(not(test), not(megu_vanilla_data)), allow(dead_code))]
pub(crate) fn lava_tick_delay_for_dim(dim: i32) -> u32 {
    if lava_spreads_fast_in_dim(dim) {
        10
    } else {
        30
    }
}

/// `LavaFluid.getDropOff`: ultrawarm lava loses 1 level per step and so reaches
/// 7 blocks horizontally like water; elsewhere it loses 2 and reaches 3.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn lava_drop_off_for_dim(dim: i32) -> u8 {
    if lava_spreads_fast_in_dim(dim) {
        1
    } else {
        2
    }
}

/// `FlowingFluid.getTickDelay` for a scheduled tick of `kind` in `dim`: water
/// is always 5 game ticks; lava is dimension-aware.
#[cfg_attr(any(not(test), not(megu_vanilla_data)), allow(dead_code))]
pub(crate) fn fluid_tick_delay(kind: Kind, dim: i32) -> u32 {
    match kind {
        Kind::Water => 5,
        Kind::Lava => lava_tick_delay_for_dim(dim),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cell {
    Air,
    Solid,
    Fluid(Fluid),
    /// Non-air, no fluid, **empty collision shape**: `FlowingFluid.canHoldFluid` is true here
    /// (nothing for an entity to collide with, and not in the families it refuses) and
    /// `canPassThrough` cannot fail against an empty shape, so a spread *replaces* the cell the
    /// way `spreadTo`'s `beforeDestroyingBlock` + `setBlock(fluidState.createLegacyBlock())`
    /// does. A non-empty partial shape stays [`Cell::Solid`]: its pass-through answer depends on
    /// the two shapes' geometry, which this machine does not model.
    Passable,
}

/// Block produced when lava meets water. This stays separate from [`Cell`] because fluid
/// writes and block replacement have different ownership in the live world.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Solidification {
    Obsidian,
    Cobblestone,
    Stone,
}

pub(crate) trait Reader {
    type Error;
    fn cell(&mut self, pos: Pos) -> Result<Cell, Self::Error>;
    fn unsupported_contact(&mut self, pos: Pos) -> Self::Error;
}

pub(crate) struct Plan {
    pub writes: Vec<(Pos, Cell)>,
    /// Lava-to-block changes that the caller must apply with its block writer.
    #[cfg_attr(not(test), allow(dead_code))]
    pub solidifications: Vec<(Pos, Solidification)>,
    pub updated_self: Option<Fluid>,
}

const HORIZONTAL: [Pos; 4] = [(0, 0, -1), (1, 0, 0), (0, 0, 1), (-1, 0, 0)];
fn add(p: Pos, d: Pos) -> Pos {
    // Vanilla neighbour arithmetic is Java `int` math: it wraps at the i32
    // edges, where a plain `+` panics under the test profile's overflow checks.
    (
        p.0.wrapping_add(d.0),
        p.1.wrapping_add(d.1),
        p.2.wrapping_add(d.2),
    )
}
fn opposite(d: Pos) -> Pos {
    (-d.0, -d.1, -d.2)
}

struct Context<'a, W> {
    world: &'a mut W,
    overlay: CoordMap<Pos, Cell>,
    writes: Vec<(Pos, Cell)>,
    solidifications: Vec<(Pos, Solidification)>,
    kind: Kind,
    /// `getDropOff` for `kind` in the caller's dimension.
    drop_off: u8,
    /// `getSlopeFindDistance` for `kind` in the caller's dimension: 4 for
    /// water and ultrawarm lava, 2 for lava elsewhere.
    slope_limit: u8,
}

impl<W: Reader> Context<'_, W> {
    fn get(&mut self, p: Pos) -> Result<Cell, W::Error> {
        if let Some(&cell) = self.overlay.get(&p) {
            Ok(cell)
        } else {
            self.world.cell(p)
        }
    }
    fn same(&self, cell: Cell) -> Option<Fluid> {
        match cell {
            Cell::Fluid(f) if f.kind == self.kind => Some(f),
            _ => None,
        }
    }
    fn put(&mut self, p: Pos, cell: Cell) -> Result<(), W::Error> {
        if self.get(p)? != cell {
            self.overlay.insert(p, cell);
            self.writes.push((p, cell));
        }
        Ok(())
    }
    fn can_pass(&mut self, p: Pos) -> Result<bool, W::Error> {
        let cell = self.get(p)?;
        Ok(!matches!(cell, Cell::Solid) && !self.same(cell).is_some_and(Fluid::source))
    }
    fn hole(&mut self, p: Pos) -> Result<bool, W::Error> {
        let below = self.get(add(p, (0, -1, 0)))?;
        Ok(self.same(below).is_some() || !matches!(below, Cell::Solid))
    }
    fn new_liquid(&mut self, p: Pos, source_conversion: bool) -> Result<Option<Fluid>, W::Error> {
        let mut maximum = 0;
        let mut sources = 0;
        for d in HORIZONTAL {
            let cell = self.get(add(p, d))?;
            if let Some(f) = self.same(cell) {
                maximum = maximum.max(f.amount());
                sources += u8::from(f.source());
            }
        }
        // `source_conversion` carries `FlowableFluid.canConvertToSource` — the
        // waterSourceConversion / lavaSourceConversion gamerules, resolved per
        // kind by the caller — so the rule itself is not re-checked here.
        if source_conversion && sources >= 2 {
            let below = self.get(add(p, (0, -1, 0)))?;
            if matches!(below, Cell::Solid) || self.same(below).is_some_and(Fluid::source) {
                return Ok(Some(Fluid {
                    kind: self.kind,
                    level: 0,
                }));
            }
        }
        let above = self.get(add(p, (0, 1, 0)))?;
        if self.same(above).is_some() {
            return Ok(Some(Fluid {
                kind: self.kind,
                level: 8,
            }));
        }
        let amount = maximum.saturating_sub(self.drop_off);
        Ok((amount > 0).then_some(Fluid {
            kind: self.kind,
            level: 8 - amount,
        }))
    }
    fn slope(&mut self, p: Pos, depth: u8, back: Pos, limit: u8) -> Result<u16, W::Error> {
        let mut best = 1000;
        for d in HORIZONTAL {
            if d == back {
                continue;
            }
            let q = add(p, d);
            if self.can_pass(q)? {
                if self.hole(q)? {
                    return Ok(u16::from(depth));
                }
                if depth < limit {
                    best = best.min(self.slope(q, depth + 1, opposite(d), limit)?);
                }
            }
        }
        Ok(best)
    }
    fn spread_to(&mut self, p: Pos, fluid: Fluid) -> Result<(), W::Error> {
        match self.get(p)? {
            Cell::Air => self.put(p, Cell::Fluid(fluid)),
            // `spreadTo`: a non-air cell outside the waterlogged arm is destroyed first and the
            // fluid written over it. `beforeDestroyingBlock` is a no-op for water.
            Cell::Passable => self.put(p, Cell::Fluid(fluid)),
            Cell::Fluid(other) if other.kind != fluid.kind => {
                if other.kind == Kind::Lava {
                    self.solidifications.push((
                        p,
                        if other.source() {
                            Solidification::Obsidian
                        } else {
                            Solidification::Cobblestone
                        },
                    ));
                } else {
                    // Lava entering water horizontally reacts at its originating lava cell.
                    // The caller's lava tick recognizes that contact before spreading.
                    return Err(self.world.unsupported_contact(p));
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
    fn spread_sides(
        &mut self,
        p: Pos,
        fluid: Fluid,
        source_conversion: bool,
    ) -> Result<(), W::Error> {
        let strength = if fluid.falling() {
            7
        } else {
            fluid.amount().saturating_sub(self.drop_off)
        };
        if strength == 0 {
            return Ok(());
        }
        let mut best = 1000;
        let mut choices = Vec::new();
        for (i, d) in HORIZONTAL.into_iter().enumerate() {
            let q = add(p, d);
            if !self.can_pass(q)? {
                continue;
            }
            let next = self.new_liquid(q, source_conversion)?;
            let distance = if self.hole(q)? {
                0
            } else {
                self.slope(q, 1, opposite(d), self.slope_limit)?
            };
            if distance < best {
                choices.clear();
                best = distance;
            }
            if distance <= best {
                choices.push((i, q, next));
            }
        }
        // EnumMap<Direction, ...> iterates NORTH, SOUTH, WEST, EAST.
        choices.sort_by_key(|(i, _, _)| [0, 3, 1, 2][*i]);
        for (_, q, next) in choices {
            if let Some(f) = next {
                self.spread_to(q, f)?;
            }
        }
        Ok(())
    }
}

pub(crate) fn tick<W: Reader>(
    world: &mut W,
    p: Pos,
    kind: Kind,
    expected_source: bool,
    source_conversion: bool,
) -> Result<Plan, W::Error> {
    tick_in_dim(world, p, kind, expected_source, source_conversion, 0)
}

/// `tick` with the protocol dimension index the world belongs to (Overworld 0,
/// Nether -1, End 1). Only lava's rules are dimension-aware in vanilla 1.21.1:
/// `LavaFluid.getDropOff` and `getSlopeFindDistance` take the water values in an
/// ultrawarm dimension, which is why Nether lava reaches 7 blocks horizontally.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn tick_in_dim<W: Reader>(
    world: &mut W,
    p: Pos,
    kind: Kind,
    expected_source: bool,
    source_conversion: bool,
    dim: i32,
) -> Result<Plan, W::Error> {
    let (drop_off, slope_limit) = match kind {
        Kind::Water => (1, 4),
        Kind::Lava => (
            lava_drop_off_for_dim(dim),
            if lava_spreads_fast_in_dim(dim) {
                4
            } else {
                2
            },
        ),
    };
    let mut cx = Context {
        world,
        overlay: CoordMap::default(),
        writes: Vec::new(),
        solidifications: Vec::new(),
        kind,
        drop_off,
        slope_limit,
    };
    let cell = cx.get(p)?;
    let Some(mut current) = cx.same(cell).filter(|f| f.source() == expected_source) else {
        return Ok(Plan {
            writes: cx.writes,
            solidifications: cx.solidifications,
            updated_self: None,
        });
    };

    // LiquidBlock.shouldSpreadLiquid checks horizontal neighbours and the block above.
    // Lava touching water at one of these faces solidifies *at the lava position*.
    if kind == Kind::Lava {
        for offset in HORIZONTAL.into_iter().chain([(0, 1, 0)]) {
            if matches!(
                cx.get(add(p, offset))?,
                Cell::Fluid(Fluid {
                    kind: Kind::Water,
                    ..
                })
            ) {
                cx.solidifications.push((
                    p,
                    if current.source() {
                        Solidification::Obsidian
                    } else {
                        Solidification::Cobblestone
                    },
                ));
                return Ok(Plan {
                    writes: cx.writes,
                    solidifications: cx.solidifications,
                    updated_self: None,
                });
            }
        }
    }
    let mut updated_self = None;
    if !current.source() {
        match cx.new_liquid(p, source_conversion)? {
            None => {
                cx.put(p, Cell::Air)?;
                return Ok(Plan {
                    writes: cx.writes,
                    solidifications: cx.solidifications,
                    updated_self,
                });
            }
            Some(next) if next != current => {
                cx.put(p, Cell::Fluid(next))?;
                current = next;
                updated_self = Some(next);
            }
            Some(_) => {}
        }
    }
    let below = add(p, (0, -1, 0));
    let below_state = cx.get(below)?;
    if let Cell::Fluid(other) = below_state {
        if other.kind != kind {
            cx.solidifications.push((
                below,
                if kind == Kind::Lava {
                    Solidification::Stone
                } else if other.source() {
                    Solidification::Obsidian
                } else {
                    Solidification::Cobblestone
                },
            ));
            return Ok(Plan {
                writes: cx.writes,
                solidifications: cx.solidifications,
                updated_self,
            });
        }
    }
    // `canSpreadTo(..., Direction.DOWN, below, ...)`: the same empty-shape answer that lets a
    // fluid take a plant cell sideways lets it fall through one.
    if matches!(below_state, Cell::Air | Cell::Passable) {
        if let Some(next) = cx.new_liquid(below, source_conversion)? {
            cx.spread_to(below, next)?;
        }
        let mut sources = 0;
        for d in HORIZONTAL {
            let state = cx.get(add(p, d))?;
            sources += u8::from(cx.same(state).is_some_and(Fluid::source));
        }
        if sources >= 3 {
            cx.spread_sides(p, current, source_conversion)?;
        }
    } else if current.source() || !cx.hole(p)? {
        cx.spread_sides(p, current, source_conversion)?;
    }
    Ok(Plan {
        writes: cx.writes,
        solidifications: cx.solidifications,
        updated_self,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    struct Fixture(HashMap<Pos, Option<Cell>>);
    impl Reader for Fixture {
        type Error = Pos;
        fn cell(&mut self, p: Pos) -> Result<Cell, Pos> {
            self.0.get(&p).copied().flatten().ok_or(p)
        }
        fn unsupported_contact(&mut self, p: Pos) -> Pos {
            p
        }
    }
    #[cfg(megu_vanilla_data)]
    fn cell(s: &str) -> Option<Cell> {
        match s {
            "A" => Some(Cell::Air),
            "S" => Some(Cell::Solid),
            "P" => Some(Cell::Passable),
            "U" => None,
            _ => Some(Cell::Fluid(Fluid {
                kind: if s.starts_with('W') {
                    Kind::Water
                } else {
                    Kind::Lava
                },
                level: s[1..].parse().unwrap(),
            })),
        }
    }

    fn fixture_with_floor(center: Fluid) -> Fixture {
        let mut cells = HashMap::new();
        for x in -6..=6 {
            for z in -6..=6 {
                cells.insert((x, -1, z), Some(Cell::Solid));
                cells.insert((x, 0, z), Some(Cell::Solid));
                cells.insert((x, 1, z), Some(Cell::Air));
            }
        }
        cells.insert((0, 0, 0), Some(Cell::Fluid(center)));
        Fixture(cells)
    }

    fn fluid(kind: Kind, level: u8) -> Fluid {
        Fluid { kind, level }
    }

    #[test]
    fn two_horizontal_water_sources_create_source_only_with_supported_floor() {
        for floor in [Cell::Solid, Cell::Fluid(fluid(Kind::Water, 0)), Cell::Air] {
            let mut fixture = fixture_with_floor(fluid(Kind::Water, 3));
            fixture.0.insert((0, -1, 0), Some(floor));
            fixture
                .0
                .insert((-1, 0, 0), Some(Cell::Fluid(fluid(Kind::Water, 0))));
            fixture
                .0
                .insert((1, 0, 0), Some(Cell::Fluid(fluid(Kind::Water, 0))));
            let plan = tick(&mut fixture, (0, 0, 0), Kind::Water, false, true).unwrap();
            let should_convert = floor != Cell::Air;
            assert_eq!(
                plan.updated_self == Some(fluid(Kind::Water, 0)),
                should_convert
            );
            assert_eq!(
                plan.writes
                    .contains(&((0, 0, 0), Cell::Fluid(fluid(Kind::Water, 0)))),
                should_convert
            );
        }
        let mut one_source = fixture_with_floor(fluid(Kind::Water, 3));
        one_source
            .0
            .insert((-1, 0, 0), Some(Cell::Fluid(fluid(Kind::Water, 0))));
        let plan = tick(&mut one_source, (0, 0, 0), Kind::Water, false, true).unwrap();
        assert_ne!(plan.updated_self, Some(fluid(Kind::Water, 0)));

        // `source_conversion` is `FlowableFluid.canConvertToSource` resolved by
        // the caller per kind — lava converts when the flag is on and does not
        // when it is off; the kind restriction no longer lives here.
        let mut lava = fixture_with_floor(fluid(Kind::Lava, 3));
        lava.0
            .insert((-1, 0, 0), Some(Cell::Fluid(fluid(Kind::Lava, 0))));
        lava.0
            .insert((1, 0, 0), Some(Cell::Fluid(fluid(Kind::Lava, 0))));
        let plan = tick(&mut lava, (0, 0, 0), Kind::Lava, false, false).unwrap();
        assert_ne!(plan.updated_self, Some(fluid(Kind::Lava, 0)));
        let plan = tick(&mut lava, (0, 0, 0), Kind::Lava, false, true).unwrap();
        assert_eq!(plan.updated_self, Some(fluid(Kind::Lava, 0)));
    }

    #[test]
    fn slope_search_prefers_water_drop_reachable_at_four_but_not_lava_two() {
        for kind in [Kind::Water, Kind::Lava] {
            let mut fixture = fixture_with_floor(fluid(kind, 0));
            for x in -1..=4 {
                fixture.0.insert((x, 0, 0), Some(Cell::Air));
            }
            fixture
                .0
                .insert((0, 0, 0), Some(Cell::Fluid(fluid(kind, 0))));
            fixture.0.insert((4, -1, 0), Some(Cell::Air));
            let plan = tick(&mut fixture, (0, 0, 0), kind, true, kind == Kind::Water).unwrap();
            assert!(plan.writes.contains(&(
                (1, 0, 0),
                Cell::Fluid(fluid(kind, if kind == Kind::Water { 1 } else { 2 }))
            )));
            assert_eq!(
                plan.writes.iter().any(|(pos, _)| *pos == (-1, 0, 0)),
                kind == Kind::Lava,
                "only water finds the distant downhill route: {:?}",
                plan.writes
            );
        }
    }

    #[test]
    fn falling_fluid_and_waterlogged_sources_keep_correct_levels_and_host() {
        let mut fixture = fixture_with_floor(fluid(Kind::Water, 0));
        fixture.0.insert((0, -1, 0), Some(Cell::Passable));
        let plan = tick(&mut fixture, (0, 0, 0), Kind::Water, true, true).unwrap();
        assert!(plan
            .writes
            .contains(&((0, -1, 0), Cell::Fluid(fluid(Kind::Water, 8)))));
        assert!(!plan.writes.iter().any(|(pos, _)| *pos == (0, 0, 0)));

        // A waterlogged host is exposed to this solver as a water source. The
        // caller retains the host state, and only the neighbouring air takes water.
        let mut waterlogged = fixture_with_floor(fluid(Kind::Water, 0));
        waterlogged.0.insert((1, 0, 0), Some(Cell::Air));
        let plan = tick(&mut waterlogged, (0, 0, 0), Kind::Water, true, true).unwrap();
        assert_eq!(
            plan.writes,
            vec![((1, 0, 0), Cell::Fluid(fluid(Kind::Water, 1)))]
        );
    }

    #[test]
    fn lava_contact_produces_obsidian_cobblestone_and_downward_stone() {
        for (level, expected) in [
            (0, Solidification::Obsidian),
            (3, Solidification::Cobblestone),
        ] {
            for water in [(1, 0, 0), (0, 1, 0)] {
                let mut fixture = fixture_with_floor(fluid(Kind::Lava, level));
                fixture
                    .0
                    .insert(water, Some(Cell::Fluid(fluid(Kind::Water, 0))));
                let plan = tick(&mut fixture, (0, 0, 0), Kind::Lava, level == 0, false).unwrap();
                assert_eq!(plan.solidifications, vec![((0, 0, 0), expected)]);
                assert!(plan.writes.is_empty());
            }
        }
        let mut fixture = fixture_with_floor(fluid(Kind::Lava, 0));
        fixture
            .0
            .insert((0, -1, 0), Some(Cell::Fluid(fluid(Kind::Water, 0))));
        let plan = tick(&mut fixture, (0, 0, 0), Kind::Lava, true, false).unwrap();
        assert_eq!(
            plan.solidifications,
            vec![((0, -1, 0), Solidification::Stone)]
        );
    }

    #[test]
    fn water_spreading_into_lava_solidifies_the_lava_target() {
        for (level, expected) in [
            (0, Solidification::Obsidian),
            (3, Solidification::Cobblestone),
        ] {
            let mut fixture = fixture_with_floor(fluid(Kind::Water, 0));
            fixture
                .0
                .insert((1, 0, 0), Some(Cell::Fluid(fluid(Kind::Lava, level))));
            let plan = tick(&mut fixture, (0, 0, 0), Kind::Water, true, true).unwrap();
            assert_eq!(plan.solidifications, vec![((1, 0, 0), expected)]);
            assert!(!plan.writes.iter().any(|(pos, _)| *pos == (1, 0, 0)));
        }
    }

    #[test]
    fn fluid_crosses_positive_and_negative_chunk_edges() {
        for origin in [15, -1] {
            let mut fixture = fixture_with_floor(fluid(Kind::Water, 0));
            fixture.0.insert((1, 0, 0), Some(Cell::Air));
            fixture.0 = fixture
                .0
                .into_iter()
                .map(|((x, y, z), v)| ((x + origin, y, z), v))
                .collect();
            let plan = tick(&mut fixture, (origin, 0, 0), Kind::Water, true, true).unwrap();
            assert_eq!(
                plan.writes,
                vec![((origin + 1, 0, 0), Cell::Fluid(fluid(Kind::Water, 1)))]
            );
        }
    }
    /// The empty-collision-shape arm, on two synthetic scenes: `canHoldFluid` answers true for a
    /// plant and `canPassThrough` cannot fail against an empty shape, so the fluid enters and
    /// `spreadTo` replaces the block. A full-cube cell in the same position takes nothing, which
    /// is what makes these a gate for the arm rather than for the scene.
    ///
    /// The native cell this was measured from is `(100,19,-513)` of the village capture
    /// (`short_grass` here, water in native); it sits outside the compared window, so these
    /// tests pin the model's answer and not a fixture row.
    #[test]
    fn a_source_replaces_an_empty_collision_shape_cell_sideways() {
        let mut cells: HashMap<Pos, Option<Cell>> = HashMap::new();
        for x in -2..=2 {
            for z in -2..=2 {
                cells.insert((x, -1, z), Some(Cell::Solid));
                cells.insert((x, 0, z), Some(Cell::Solid));
                // `getNewLiquid` reads the cell above every candidate, so the scene has to
                // answer it: open sky above the one-wide channel.
                cells.insert((x, 1, z), Some(Cell::Air));
            }
        }
        cells.insert(
            (0, 0, 0),
            Some(Cell::Fluid(Fluid {
                kind: Kind::Water,
                level: 0,
            })),
        );
        cells.insert((1, 0, 0), Some(Cell::Passable));
        let mut fixture = Fixture(cells.clone());
        let plan = tick(&mut fixture, (0, 0, 0), Kind::Water, true, true).unwrap();
        let flowing = Cell::Fluid(Fluid {
            kind: Kind::Water,
            level: 1,
        });
        assert!(
            plan.writes.contains(&((1, 0, 0), flowing)),
            "the plant east of the source is replaced by a flowing level: {:?}",
            plan.writes
        );
        // The same scene with a full cube there writes nothing: the arm is the empty shape.
        cells.insert((1, 0, 0), Some(Cell::Solid));
        let mut fixture = Fixture(cells);
        let plan = tick(&mut fixture, (0, 0, 0), Kind::Water, true, true).unwrap();
        assert!(
            plan.writes.is_empty(),
            "a full cube takes no fluid: {:?}",
            plan.writes
        );
    }

    #[test]
    fn a_source_falls_through_an_empty_collision_shape_cell() {
        let mut cells: HashMap<Pos, Option<Cell>> = HashMap::new();
        for x in -2..=2 {
            for z in -2..=2 {
                cells.insert((x, -2, z), Some(Cell::Solid));
                cells.insert((x, -1, z), Some(Cell::Solid));
                cells.insert((x, 0, z), Some(Cell::Solid));
                cells.insert((x, 1, z), Some(Cell::Air));
            }
        }
        cells.insert(
            (0, 0, 0),
            Some(Cell::Fluid(Fluid {
                kind: Kind::Water,
                level: 0,
            })),
        );
        cells.insert((0, -1, 0), Some(Cell::Passable));
        let mut fixture = Fixture(cells);
        let plan = tick(&mut fixture, (0, 0, 0), Kind::Water, true, true).unwrap();
        let falling = Cell::Fluid(Fluid {
            kind: Kind::Water,
            level: 8,
        });
        assert!(
            plan.writes.contains(&((0, -1, 0), falling)),
            "the plant below the source takes the falling fluid: {:?}",
            plan.writes
        );
    }

    #[cfg(megu_vanilla_data)]
    #[test]
    fn cave_fluid_ticks_match_native_writes() {
        let mut count = 0;
        for line in include_str!("../tests/data/fluid_tick_1211.tsv").lines() {
            let f: Vec<_> = line.split('\t').collect();
            assert_eq!(f.len(), 7);
            let p = (
                f[0].parse::<i32>().unwrap(),
                f[1].parse::<i32>().unwrap(),
                f[2].parse::<i32>().unwrap(),
            );
            let kind = if f[3] == "W" { Kind::Water } else { Kind::Lava };
            let mut cells = HashMap::new();
            let mut data = f[5].split(',');
            for dy in -1..=1 {
                for dx in -6..=6 {
                    for dz in -6..=6 {
                        cells.insert((p.0 + dx, p.1 + dy, p.2 + dz), cell(data.next().unwrap()));
                    }
                }
            }
            assert!(data.next().is_none());
            let mut fixture = Fixture(cells);
            let plan = tick(&mut fixture, p, kind, f[4] == "1", kind == Kind::Water)
                .unwrap_or_else(|at| panic!("unsupported native fixture cell {at:?} at {p:?}"));
            let expected: Vec<_> = f[6]
                .split(';')
                .filter(|v| !v.is_empty())
                .map(|entry| {
                    let (coord, state) = entry.split_once(':').unwrap();
                    let n: Vec<i32> = coord.split(',').map(|v| v.parse().unwrap()).collect();
                    ((n[0], n[1], n[2]), cell(state).unwrap())
                })
                .collect();
            assert_eq!(plan.writes, expected, "native tick at {p:?}");
            count += 1;
        }
        assert_eq!(count, 28);
    }

    /// Vanilla 1.21.1 `LavaFluid`: `getTickDelay` is 10 game ticks in an
    /// ultrawarm dimension and 30 elsewhere; `getDropOff` is 1 there (a
    /// 7-block reach like water) and 2 elsewhere (a 3-block reach).
    #[test]
    fn p5b_parity_lava_tick_delay_and_drop_off_are_dimension_aware() {
        assert_eq!(lava_tick_delay_for_dim(-1), 10);
        assert_eq!(lava_tick_delay_for_dim(0), 30);
        assert_eq!(lava_tick_delay_for_dim(1), 30);
        assert_eq!(lava_drop_off_for_dim(-1), 1);
        assert_eq!(lava_drop_off_for_dim(0), 2);
        assert_eq!(lava_drop_off_for_dim(1), 2);
        assert!(lava_spreads_fast_in_dim(-1));
        assert!(!lava_spreads_fast_in_dim(0));
        assert!(!lava_spreads_fast_in_dim(1));
        assert_eq!(fluid_tick_delay(Kind::Water, -1), 5);
        assert_eq!(fluid_tick_delay(Kind::Lava, -1), 10);
        assert_eq!(fluid_tick_delay(Kind::Lava, 0), 30);
    }

    /// One source spread across an open channel, ticking each newly written
    /// cell the way the live scheduler would: Nether lava reaches 7 blocks
    /// horizontally with levels 1..=7; Overworld lava reaches 3 with levels
    /// 2, 4, 6.
    #[test]
    fn p5b_parity_nether_lava_flows_seven_overworld_lava_three() {
        for (dim, reach, last_level) in [(-1, 7, 7), (0, 3, 6), (1, 3, 6)] {
            let mut cells: HashMap<Pos, Option<Cell>> = HashMap::new();
            for x in -10..=10 {
                for z in -10..=10 {
                    cells.insert((x, -1, z), Some(Cell::Solid));
                    cells.insert((x, 0, z), Some(Cell::Solid));
                    cells.insert((x, 1, z), Some(Cell::Air));
                }
            }
            for x in 0..=8 {
                cells.insert((x, 0, 0), Some(Cell::Air));
            }
            cells.insert((0, 0, 0), Some(Cell::Fluid(fluid(Kind::Lava, 0))));
            let mut fixture = Fixture(cells);
            let mut frontier = vec![((0, 0, 0), fluid(Kind::Lava, 0))];
            let mut spread: HashMap<Pos, u8> = HashMap::new();
            for _ in 0..64 {
                let Some((p, f)) = frontier.pop() else { break };
                let plan = tick_in_dim(&mut fixture, p, Kind::Lava, f.source(), false, dim)
                    .unwrap();
                for (q, cell) in plan.writes {
                    let Cell::Fluid(next) = cell else { continue };
                    if spread.insert(q, next.level).is_none() {
                        fixture.0.insert(q, Some(cell));
                        frontier.push((q, next));
                    }
                }
            }
            for x in 1..=reach {
                assert_eq!(
                    spread.get(&(x, 0, 0)),
                    Some(&(x as u8 * lava_drop_off_for_dim(dim))),
                    "dim {dim} lava level at distance {x}: {spread:?}"
                );
            }
            assert_eq!(spread.len(), reach as usize, "dim {dim} reach: {spread:?}");
            assert_eq!(spread[&(reach, 0, 0)], last_level);
        }
    }

    /// `getSlopeFindDistance` is 4 for ultrawarm lava like water — the distant
    /// downhill route exists and the flow prefers it — and 2 elsewhere, where
    /// every direction ties and spreads.
    #[test]
    fn p5b_parity_nether_lava_finds_the_distant_downhill_route() {
        for (dim, writes_west) in [(-1, false), (0, true)] {
            let mut fixture = fixture_with_floor(fluid(Kind::Lava, 0));
            for x in [-1, 1, 2, 3, 4] {
                fixture.0.insert((x, 0, 0), Some(Cell::Air));
            }
            fixture.0.insert((4, -1, 0), Some(Cell::Air));
            let plan = tick_in_dim(&mut fixture, (0, 0, 0), Kind::Lava, true, false, dim).unwrap();
            assert_eq!(
                plan.writes.iter().any(|(pos, _)| *pos == (-1, 0, 0)),
                writes_west,
                "dim {dim}: {:?}",
                plan.writes
            );
            assert!(plan.writes.contains(&(
                (1, 0, 0),
                Cell::Fluid(fluid(Kind::Lava, lava_drop_off_for_dim(dim)))
            )));
        }
    }

    /// Neighbour generation is Java `int` arithmetic: a position one step off
    /// the i32 edge wraps to the other edge instead of panicking.
    #[test]
    fn bughunt_root_neighbour_math_wraps_at_i32_edges() {
        let _guard = crate::world_guard();
        struct OpenWorld(HashMap<Pos, Cell>);
        impl Reader for OpenWorld {
            type Error = Pos;
            fn cell(&mut self, p: Pos) -> Result<Cell, Pos> {
                Ok(self.0.get(&p).copied().unwrap_or(Cell::Air))
            }
            fn unsupported_contact(&mut self, p: Pos) -> Pos {
                p
            }
        }
        let world = |p: Pos, below: Pos, kind| {
            OpenWorld(HashMap::from([
                (p, Cell::Fluid(fluid(kind, 0))),
                (below, Cell::Solid),
            ]))
        };
        // A horizontal step off each X/Z edge lands on the wrapped neighbour.
        for (p, wrapped) in [
            ((i32::MIN, 0, 0), (i32::MAX, 0, 0)),
            ((i32::MAX, 0, 0), (i32::MIN, 0, 0)),
            ((0, 0, i32::MIN), (0, 0, i32::MAX)),
            ((0, 0, i32::MAX), (0, 0, i32::MIN)),
        ] {
            let mut w = world(p, (p.0, -1, p.2), Kind::Water);
            let plan = tick(&mut w, p, Kind::Water, true, true).unwrap();
            assert!(plan.writes.iter().any(|(w, _)| *w == wrapped), "{p:?}");
        }
        // The below/above lookups wrap at the Y edges the same way.
        for (p, kind) in [
            ((0, i32::MIN, 0), Kind::Water),
            ((0, i32::MAX, 0), Kind::Lava),
        ] {
            let mut w = world(p, (0, p.1.wrapping_sub(1), 0), kind);
            tick(&mut w, p, kind, true, true).unwrap();
        }
    }
}
