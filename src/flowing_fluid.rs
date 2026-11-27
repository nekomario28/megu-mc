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
//!
//! Three vanilla rules read a *block* where the [`Cell`] model only carries a fluid, so they are
//! asked through [`Reader::contact`] rather than through [`Cell`]:
//!
//! * `LiquidBlock.shouldSpreadLiquid`'s basalt arm, which is reached from the lava tick this
//!   module owns (see [`Solidification::Basalt`]).
//! * `SpongeBlock.removeWaterBreadthFirstSearch`, exposed as [`sponge_absorb_water`] —
//!   entered from `SpongeBlock.onPlace`, never from a fluid tick.
//! * `BubbleColumnBlock.updateColumn`, exposed as [`bubble_column_update`] — entered from
//!   `SoulSandBlock`/`MagmaBlock` on the block-tick lane. `soul_sand` →
//!   `bubble_column[drag=false]`, `magma_block` → `bubble_column[drag=true]`, upward
//!   through full water sources, back to `water` when the base is neither.
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
pub(crate) fn lava_spreads_fast_in_dim(dim: i32) -> bool {
    dim == -1
}

/// `LavaFluid.getTickDelay`: 10 game ticks in an ultrawarm dimension, 30 game
/// ticks everywhere else.
pub(crate) fn lava_tick_delay_for_dim(dim: i32) -> u32 {
    if lava_spreads_fast_in_dim(dim) {
        10
    } else {
        30
    }
}

/// `LavaFluid.getDropOff`: ultrawarm lava loses 1 level per step and so reaches
/// 7 blocks horizontally like water; elsewhere it loses 2 and reaches 3.
pub(crate) fn lava_drop_off_for_dim(dim: i32) -> u8 {
    if lava_spreads_fast_in_dim(dim) {
        1
    } else {
        2
    }
}

/// `FlowingFluid.getTickDelay` for a scheduled tick of `kind` in `dim`: water is
/// always 5 game ticks; lava is dimension-aware (10 in the Nether, 30 elsewhere).
///
/// This is the delay a scheduler reschedules a fluid tick with, so it is part of this
/// machine's contract and not a test-only helper: a scheduler that hardcodes 5 for
/// water and 30 for lava makes Nether lava run at Overworld speed.
pub(crate) fn fluid_tick_delay_for_dim(kind: Kind, dim: i32) -> u64 {
    match kind {
        Kind::Water => 5,
        Kind::Lava => u64::from(lava_tick_delay_for_dim(dim)),
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

/// Block produced when lava meets water, and by the one contact rule that needs a
/// block identity. This stays separate from [`Cell`] because fluid writes and block
/// replacement have different ownership in the live world.
///
/// Vanilla 1.21.1 `LiquidBlock.shouldSpreadLiquid` sets one of three blocks, and every
/// one of them is written at the lava's own position: `minecraft:obsidian` when a
/// visited cell's fluid is water and the lava is a source, `minecraft:cobblestone` when
/// it is water and the lava is flowing, and `minecraft:basalt` when the block below the
/// lava is `minecraft:soul_soil` and a visited cell **is** `minecraft:blue_ice`. The
/// visited cells are `pos.relative(direction.getOpposite())` over
/// `POSSIBLE_FLOW_DIRECTIONS` — UP, NORTH, SOUTH, WEST, EAST, first hit wins — and the
/// water test runs before the basalt test for each of them, which is why
/// [`tick_in_dim`] interleaves the two in one loop instead of running two passes.
///
/// [`Solidification::Stone`] is the other direction: `LavaFluid.spreadTo(Direction.DOWN)`
/// turns the cell below into `minecraft:stone` when its block is a `LiquidBlock`, and
/// has no basalt arm of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Solidification {
    Obsidian,
    Cobblestone,
    Stone,
    /// `minecraft:basalt`: lava resting on `minecraft:soul_soil` that touches
    /// `minecraft:blue_ice`. The block identities come from [`Reader::contact`].
    Basalt,
}

/// The block identities the three contact rules ask about, i.e. what
/// `level.getBlockState(pos)` answers where [`Cell`] answers what the *fluid* there
/// is. A reader with no block vocabulary leaves every answer at [`Contact::Other`],
/// which is the honest "none of these rules applies".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Contact {
    /// No rule below: stone, dirt, `minecraft:air`, and anything else outside these
    /// four identities.
    Other,
    /// `minecraft:soul_soil`, the block `shouldSpreadLiquid`'s basalt arm rests on.
    SoulSoil,
    /// `minecraft:blue_ice`, the neighbour that arm requires.
    BlueIce,
    /// `minecraft:soul_sand`, the base that makes a bubble column drag up.
    SoulSand,
    /// `minecraft:magma_block`, the base that makes a bubble column drag down.
    MagmaBlock,
    /// `minecraft:bubble_column`, which `BubbleColumnBlock.canBePlacedIn` accepts as a
    /// cell a column may overwrite even though it is not water.
    BubbleColumn,
}

pub(crate) trait Reader {
    type Error;
    fn cell(&mut self, pos: Pos) -> Result<Cell, Self::Error>;
    fn unsupported_contact(&mut self, pos: Pos) -> Self::Error;

    /// The protocol dimension index of this reader's world — Overworld 0, Nether -1,
    /// End 1 — which [`tick`] hands to [`tick_in_dim`] to pick the liquid's
    /// `getDropOff`, `getSlopeFindDistance` and `getTickDelay`.
    ///
    /// The default is the Overworld: a reader built on one dimension must override
    /// this, or the fluid it ticks flows at Overworld speed and reach.
    fn dim(&self) -> i32 {
        0
    }

    /// `level.getBlockState(pos)` for the rules in [`Contact`]. The default is
    /// [`Contact::Other`]: a reader that only knows fluids cannot answer a block
    /// identity, and saying so leaves the basalt, sponge and bubble-column rules off
    /// instead of guessing them.
    fn contact(&mut self, pos: Pos) -> Result<Contact, Self::Error> {
        let _ = pos;
        Ok(Contact::Other)
    }
}

pub(crate) struct Plan {
    pub writes: Vec<(Pos, Cell)>,
    /// Lava-to-block changes that the caller must apply with its block writer.
    pub solidifications: Vec<(Pos, Solidification)>,
    pub updated_self: Option<Fluid>,
}

const HORIZONTAL: [Pos; 4] = [(0, 0, -1), (1, 0, 0), (0, 0, 1), (-1, 0, 0)];
/// `LiquidBlock.POSSIBLE_FLOW_DIRECTIONS` is `ImmutableList.of(DOWN, SOUTH, NORTH, EAST, WEST)`
/// and every entry is read at `pos.relative(direction.getOpposite())`, so the cells
/// `shouldSpreadLiquid` visits — first hit wins — are UP, NORTH, SOUTH, WEST, EAST.
const LIQUID_CONTACT: [Pos; 5] = [(0, 1, 0), (0, 0, -1), (0, 0, 1), (-1, 0, 0), (1, 0, 0)];
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
    /// `level.getBlockState(p)` for the [`Contact`] rules. Unlike [`Context::get`] this
    /// reads the world and not the overlay: no planned write in this machine produces a
    /// block a contact rule looks for, because [`Context::put`] only ever writes air or
    /// a fluid over the cell.
    fn contact(&mut self, p: Pos) -> Result<Contact, W::Error> {
        self.world.contact(p)
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
            // Same fluid already in the target: `spreadTo` writes `fluidState.createLegacyBlock()`
            // over it all the same, which is how the computed falling state replaces the flowing
            // cell underneath a source. `put` drops the write when the level is unchanged.
            Cell::Fluid(_) => self.put(p, Cell::Fluid(fluid)),
            // Unreachable through `canSpreadTo`, which requires a wall the fluid can pass and a
            // cell `canHoldFluid` accepts; a full cube is refused rather than overwritten.
            Cell::Solid => Ok(()),
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
        // `getSpread` returns an `EnumMap<Direction, FluidState>`, so at most four entries, and
        // the caller applies them in `entrySet()` order: `Direction` ordinals are
        // DOWN, UP, NORTH, SOUTH, WEST, EAST. A fixed array keeps the hot path allocation-free.
        let mut choices: [(usize, Pos, Option<Fluid>); 4] = [(0, (0, 0, 0), None); 4];
        let mut chosen = 0;
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
                chosen = 0;
                best = distance;
            }
            if distance <= best {
                choices[chosen] = (i, q, next);
                chosen += 1;
            }
        }
        // Rank of each `HORIZONTAL` entry in `EnumMap` order: north, south, west, east.
        choices[..chosen].sort_by_key(|(i, _, _)| [0, 3, 1, 2][*i]);
        for (_, q, next) in choices[..chosen].iter().copied() {
            if let Some(f) = next {
                self.spread_to(q, f)?;
            }
        }
        Ok(())
    }
}

/// [`tick_in_dim`] in the dimension the reader says its world belongs to, i.e.
/// [`Reader::dim`]. A reader bound to the Nether therefore gets Nether lava speed and
/// reach without restating the dimension here; a reader that leaves [`Reader::dim`] at
/// its Overworld default gets Overworld rules.
pub(crate) fn tick<W: Reader>(
    world: &mut W,
    p: Pos,
    kind: Kind,
    expected_source: bool,
    source_conversion: bool,
) -> Result<Plan, W::Error> {
    let dim = world.dim();
    tick_in_dim(world, p, kind, expected_source, source_conversion, dim)
}

/// The entry point: one scheduled `FlowingFluid.tick` of `kind` at `p`, in the
/// dimension whose protocol index is `dim` (Overworld 0, Nether -1, End 1).
///
/// Only lava's rules are dimension-aware in vanilla 1.21.1: `LavaFluid.getDropOff` and
/// `getSlopeFindDistance` take the water values in an ultrawarm dimension, which is why
/// Nether lava reaches 7 blocks horizontally with levels 1..=7 instead of 3 with levels
/// 2, 4, 6. [`Reader::dim`] is where a caller-scheduled tick gets that index from, and
/// [`fluid_tick_delay_for_dim`] is the `getTickDelay` (10 vs 30) the same index implies.
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

    // `LiquidBlock.shouldSpreadLiquid` walks POSSIBLE_FLOW_DIRECTIONS once, and each
    // iteration tests the visited cell's fluid for water first and the cell itself for
    // `minecraft:blue_ice` second, so a water contact that comes later in the order wins
    // over an earlier blue-ice one. `flag` — the block below the lava is
    // `minecraft:soul_soil` — is read once, before the loop, exactly as vanilla does.
    if kind == Kind::Lava {
        let soul_soil_below = cx.contact(add(p, (0, -1, 0)))? == Contact::SoulSoil;
        for offset in LIQUID_CONTACT {
            let neighbour = add(p, offset);
            if matches!(
                cx.get(neighbour)?,
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
            if soul_soil_below && cx.contact(neighbour)? == Contact::BlueIce {
                cx.solidifications.push((p, Solidification::Basalt));
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
    // `canSpreadTo(level, pos, blockState, DOWN, below, belowState, level.getFluidState(below),
    // newLiquid.getType())` is exactly `can_pass` for this cell model: a *source* of this fluid
    // below is already the full column and blocks the fall, while a *flowing* cell of this fluid
    // does not — `spreadTo` overwrites it with the computed falling state, which is what turns a
    // half-filled hole under a source into a cascade.
    if cx.can_pass(below)? {
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

/// `Direction.values()` in ordinal order — DOWN, UP, NORTH, SOUTH, WEST, EAST — which is
/// the order `SpongeBlock.removeWaterBreadthFirstSearch` drains its queue in and the
/// order `BubbleColumnBlock`'s vertical walk does not use.
const NEIGHBOURS: [Pos; 6] = [
    (0, -1, 0),
    (0, 1, 0),
    (0, 0, -1),
    (0, 0, 1),
    (-1, 0, 0),
    (1, 0, 0),
];

/// `SpongeBlock.MAX_COUNT + 1`, as `BlockPos.breadthFirstTraversal` reads it: the walk
/// counts every cell its predicate accepts — the sponge's own cell included, since
/// vanilla's predicate answers true for it — and returns once that count reaches 65. One
/// sponge therefore absorbs at most 64 water cells, the same count
/// `world_mechanisms::sponge_absorb_water` arrives at.
pub(crate) const SPONGE_MAX_CELLS: usize = 65;
/// `SpongeBlock.MAX_DEPTH`: a cell six steps from the sponge is absorbed but never
/// expanded, so the search is a depth-6 ball around it.
pub(crate) const SPONGE_MAX_DEPTH: u8 = 6;

/// The water cells `SpongeBlock.removeWaterBreadthFirstSearch` replaces with
/// `minecraft:air`, in the order the search absorbs them.
///
/// This is a fixed-capacity list and not a `Vec`: vanilla's caps are
/// [`SPONGE_MAX_CELLS`] and [`SPONGE_MAX_DEPTH`], so a sponge with no water in reach
/// allocates nothing and returns an empty list. `Deref<Target = [Pos]>` gives the
/// slice view, so `len`, `is_empty` and iteration come from the slice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SpongeAbsorb {
    cells: [Pos; SPONGE_MAX_CELLS],
    len: usize,
}

impl std::ops::Deref for SpongeAbsorb {
    type Target = [Pos];
    fn deref(&self) -> &[Pos] {
        &self.cells[..self.len]
    }
}

/// `SpongeBlock.removeWaterBreadthFirstSearch` over `BlockPos.breadthFirstTraversal`:
/// every water cell within a Manhattan depth of [`SPONGE_MAX_DEPTH`] of `origin`,
/// breadth first, inside the [`SPONGE_MAX_CELLS`] accepted-cell budget.
///
/// The caps are the vanilla constants rather than parameters: a caller that could ask
/// for a bigger search could ask for a sponge that absorbs more water than vanilla's.
///
/// Vanilla's traversal has three orderings this keeps. A cell expands only when its
/// predicate accepted it, so the walk never leaves the water body it started in; a cell
/// at the depth cap is accepted but does not expand; and the budget counts accepted
/// cells, so the sponge's own cell is one of them and the absorbed list stops one short.
/// Two states keep it allocation-free: the queue is a fixed array of `(cell, depth)`
/// — only an accepted cell is enqueued, so it cannot outgrow the budget — and "already
/// absorbed" is a linear scan of the resolved cells instead of a set.
pub(crate) fn sponge_absorb_water<W: Reader + ?Sized>(
    world: &mut W,
    origin: Pos,
) -> Result<SpongeAbsorb, W::Error> {
    let mut absorbed = SpongeAbsorb {
        cells: [origin; SPONGE_MAX_CELLS],
        len: 0,
    };
    let mut accepted = 1usize;
    let mut queue = [(origin, 0u8); SPONGE_MAX_CELLS];
    let mut head = 0usize;
    let mut tail = 1usize;
    while head < tail && accepted < SPONGE_MAX_CELLS {
        let (pos, depth) = queue[head];
        head += 1;
        // `if (depth >= maxDepth) continue;`: the cell at the cap absorbs water it
        // touches but does not queue search past itself.
        if depth >= SPONGE_MAX_DEPTH {
            continue;
        }
        for d in NEIGHBOURS {
            let next = add(pos, d);
            // The predicate: `FluidState.is(FluidTags.WATER)`. Flowing water passes too,
            // and a waterlogged host reads as the source water it carries.
            if !matches!(
                world.cell(next)?,
                Cell::Fluid(Fluid {
                    kind: Kind::Water,
                    ..
                })
            ) {
                continue;
            }
            if absorbed.cells[..absorbed.len].contains(&next) {
                continue;
            }
            if accepted == SPONGE_MAX_CELLS {
                break;
            }
            accepted += 1;
            absorbed.cells[absorbed.len] = next;
            absorbed.len += 1;
            if tail < SPONGE_MAX_CELLS {
                queue[tail] = (next, depth + 1);
                tail += 1;
            }
        }
    }
    Ok(absorbed)
}

/// The tallest column `BubbleColumnBlock.updateColumn` can fill: the 1.21.1 build
/// height (Y -64..320, 384 cells), where `LevelAccessor.setBlock` starts returning
/// false and vanilla's walk stops. A `Reader` that keeps answering "placeable" would
/// otherwise walk forever.
pub(crate) const BUBBLE_COLUMN_MAX_HEIGHT: usize = 384;

/// `BubbleColumnBlock`'s column state, i.e. the `drag` a column carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Column {
    /// `bubble_column[drag=false]`: entities rise. Vanilla's answer for a
    /// `minecraft:soul_sand` base.
    Up,
    /// `bubble_column[drag=true]`: entities sink. Vanilla's answer for a
    /// `minecraft:magma_block` base.
    Down,
    /// `minecraft:water`: the fallback for every other base block — a column that is
    /// not a column.
    Water,
}

/// `BubbleColumnBlock.getColumnState`: the column state the block a column starts on
/// produces.
///
/// `minecraft:soul_sand` drags up and `minecraft:magma_block` drags down; every other
/// base block falls back to `minecraft:water`. A base that is already a
/// `minecraft:bubble_column` keeps its own `drag`, which vanilla returns unchanged and
/// which the caller already holds, so it classifies as the water fallback here.
pub(crate) fn bubble_column_state(base: Contact) -> Column {
    match base {
        Contact::SoulSand => Column::Up,
        Contact::MagmaBlock => Column::Down,
        _ => Column::Water,
    }
}

/// `BubbleColumnBlock.canBePlacedIn`: a cell a column may fill is an existing
/// `minecraft:bubble_column`, or a fluid state that is `minecraft:water` with
/// `getAmount() >= 8` **and** `isSource()` — which is exactly a level-0 water cell.
/// Air is not placeable in 1.21.1, so a column stops at it.
fn bubble_column_placeable(cell: Cell, contact: Contact) -> bool {
    match cell {
        Cell::Fluid(Fluid {
            kind: Kind::Water,
            level: 0,
        }) => true,
        _ => contact == Contact::BubbleColumn,
    }
}

/// `BubbleColumnBlock.updateColumn`: the column `origin` starts, and the cells it
/// overwrites in `out`, bottom first.
///
/// `origin` is the cell directly above the base block, so the classification comes
/// from `origin`'s own base ([`bubble_column_state`]) and may be [`Column::Water`]
/// when that base is neither soul sand nor magma. `None` is vanilla's early return:
/// `canBePlacedIn` refused `origin`, so nothing is written and `out` stays empty. When
/// it does run, `out` receives `origin` and every placeable cell above it — each of
/// them rewritten with the *same* column state, existing bubble columns included —
/// stopping at the first cell that is neither.
pub(crate) fn bubble_column_update<W: Reader + ?Sized>(
    world: &mut W,
    origin: Pos,
    out: &mut Vec<Pos>,
) -> Result<Option<Column>, W::Error> {
    out.clear();
    let cell = world.cell(origin)?;
    let contact = world.contact(origin)?;
    if !bubble_column_placeable(cell, contact) {
        return Ok(None);
    }
    let column = bubble_column_state(world.contact(add(origin, (0, -1, 0)))?);
    let mut pos = origin;
    for _ in 0..BUBBLE_COLUMN_MAX_HEIGHT {
        let cell = world.cell(pos)?;
        let contact = world.contact(pos)?;
        if !bubble_column_placeable(cell, contact) {
            break;
        }
        out.push(pos);
        pos = add(pos, (0, 1, 0));
    }
    Ok(Some(column))
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

    /// A world that also answers [`Contact`] and [`Reader::dim`], for the rules that
    /// read a block identity rather than a fluid. Unlisted cells are air and unlisted
    /// block identities are [`Contact::Other`], which is what an open sky and plain
    /// stone answer to the three contact rules.
    struct ContactWorld {
        cells: HashMap<Pos, Cell>,
        contacts: HashMap<Pos, Contact>,
        dim: i32,
    }
    impl ContactWorld {
        fn new(dim: i32) -> Self {
            Self {
                cells: HashMap::new(),
                contacts: HashMap::new(),
                dim,
            }
        }
    }
    impl Reader for ContactWorld {
        type Error = Pos;
        fn cell(&mut self, p: Pos) -> Result<Cell, Pos> {
            Ok(self.cells.get(&p).copied().unwrap_or(Cell::Air))
        }
        fn unsupported_contact(&mut self, p: Pos) -> Pos {
            p
        }
        fn dim(&self) -> i32 {
            self.dim
        }
        fn contact(&mut self, p: Pos) -> Result<Contact, Pos> {
            Ok(self.contacts.get(&p).copied().unwrap_or(Contact::Other))
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
    /// 7-block reach like water) and 2 elsewhere (a 3-block reach). These are the
    /// scheduler-facing numbers, so they are pinned off the production functions and
    /// not off a `#[cfg(test)]` restatement of them.
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
        assert_eq!(fluid_tick_delay_for_dim(Kind::Water, -1), 5);
        assert_eq!(fluid_tick_delay_for_dim(Kind::Water, 0), 5);
        assert_eq!(fluid_tick_delay_for_dim(Kind::Lava, -1), 10);
        assert_eq!(fluid_tick_delay_for_dim(Kind::Lava, 0), 30);
        assert_eq!(fluid_tick_delay_for_dim(Kind::Lava, 1), 30);
        // The reader-side view of the same rule: a dimension-bound reader answers the
        // delay its own world implies, which is what a scheduler rescheduling a tick
        // asks instead of hardcoding 5/30.
        let mut nether = ContactWorld::new(-1);
        assert_eq!(fluid_tick_delay_for_dim(Kind::Lava, nether.dim()), 10);
        assert_eq!(fluid_tick_delay_for_dim(Kind::Water, nether.dim()), 5);
        let mut overworld = ContactWorld::new(0);
        assert_eq!(fluid_tick_delay_for_dim(Kind::Lava, overworld.dim()), 30);
        // `tick` reads the dimension off the reader, so a Nether reader gets Nether
        // lava drop-off without the caller naming the dimension.
        for world in [&mut nether, &mut overworld] {
            world.cells.insert((0, -1, 0), Cell::Solid);
            world
                .cells
                .insert((0, 0, 0), Cell::Fluid(fluid(Kind::Lava, 0)));
        }
        let plan = tick(&mut nether, (0, 0, 0), Kind::Lava, true, false).unwrap();
        assert!(
            plan.writes
                .contains(&((1, 0, 0), Cell::Fluid(fluid(Kind::Lava, 1)))),
            "Nether lava drops one level per step: {:?}",
            plan.writes
        );
        let plan = tick(&mut overworld, (0, 0, 0), Kind::Lava, true, false).unwrap();
        assert!(
            plan.writes
                .contains(&((1, 0, 0), Cell::Fluid(fluid(Kind::Lava, 2)))),
            "Overworld lava drops two levels per step: {:?}",
            plan.writes
        );
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

    /// `FlowingFluid.spread` reaches the cell below through `canSpreadTo(..., Direction.DOWN,
    /// ...)`, which refuses only a *source* of the same fluid: a flowing cell below is a valid
    /// target and `spreadTo` writes the computed falling state over it. That is how a half-filled
    /// hole under a source becomes a cascade, and why the side spread in the same tick is gated
    /// on three source neighbours rather than on the source's own reach.
    #[test]
    fn a_flowing_cell_below_a_source_is_rewritten_as_falling_water() {
        let mut fixture = fixture_with_floor(fluid(Kind::Water, 0));
        fixture
            .0
            .insert((0, -1, 0), Some(Cell::Fluid(fluid(Kind::Water, 1))));
        let plan = tick(&mut fixture, (0, 0, 0), Kind::Water, true, true).unwrap();
        assert_eq!(
            plan.writes,
            vec![((0, -1, 0), Cell::Fluid(fluid(Kind::Water, 8)))],
            "the flowing cell below takes the falling state and nothing spreads sideways"
        );

        // A source below is already the full column: `isSourceBlockOfThisType` refuses the fall,
        // so `spread` takes the `spreadToSides` arm instead and writes nothing here.
        let mut full = fixture_with_floor(fluid(Kind::Water, 0));
        full.0
            .insert((0, -1, 0), Some(Cell::Fluid(fluid(Kind::Water, 0))));
        let plan = tick(&mut full, (0, 0, 0), Kind::Water, true, true).unwrap();
        assert!(plan.writes.is_empty(), "{:?}", plan.writes);
    }

    /// `getSpread` collects into an `EnumMap<Direction, FluidState>` and `spreadToSides` applies it
    /// in `entrySet()` order — direction ordinals DOWN, UP, NORTH, SOUTH, WEST, EAST — not in the
    /// `Direction.Plane.HORIZONTAL` array order the candidates were gathered in.
    #[test]
    fn side_spread_writes_in_enum_map_direction_order() {
        let mut fixture = fixture_with_floor(fluid(Kind::Water, 0));
        for p in [(0, 0, -1), (1, 0, 0), (0, 0, 1), (-1, 0, 0)] {
            fixture.0.insert(p, Some(Cell::Air));
        }
        let plan = tick(&mut fixture, (0, 0, 0), Kind::Water, true, true).unwrap();
        let positions: Vec<Pos> = plan.writes.iter().map(|(p, _)| *p).collect();
        assert_eq!(
            positions,
            vec![(0, 0, -1), (0, 0, 1), (-1, 0, 0), (1, 0, 0)],
            "north, south, west, east"
        );
        assert!(plan
            .writes
            .iter()
            .all(|(_, cell)| *cell == Cell::Fluid(fluid(Kind::Water, 1))));
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

    /// `LiquidBlock.shouldSpreadLiquid`'s basalt arm: lava whose block below is
    /// `minecraft:soul_soil` and whose visited cell is `minecraft:blue_ice` becomes
    /// `minecraft:basalt` at the lava's own position. The arm does not care whether the
    /// lava is a source — only the water arm splits on that — so both levels answer
    /// basalt.
    #[test]
    fn lava_on_soul_soil_touching_blue_ice_becomes_basalt() {
        for level in [0, 3] {
            for ice in [(1, 0, 0), (0, 1, 0), (0, 0, -1), (0, 0, 1), (-1, 0, 0)] {
                let mut world = ContactWorld::new(-1);
                world
                    .cells
                    .insert((0, 0, 0), Cell::Fluid(fluid(Kind::Lava, level)));
                world.contacts.insert((0, -1, 0), Contact::SoulSoil);
                world.contacts.insert(ice, Contact::BlueIce);
                let plan = tick(&mut world, (0, 0, 0), Kind::Lava, level == 0, false).unwrap();
                assert_eq!(
                    plan.solidifications,
                    vec![((0, 0, 0), Solidification::Basalt)],
                    "lava level {level} over soul soil beside blue ice at {ice:?}"
                );
                assert!(plan.writes.is_empty(), "{:?}", plan.writes);
            }
        }
        // Both halves are required: blue ice under plain stone is not basalt, and soul
        // soil with no blue ice in reach is not either.
        let mut no_soil = ContactWorld::new(-1);
        no_soil
            .cells
            .insert((0, 0, 0), Cell::Fluid(fluid(Kind::Lava, 0)));
        no_soil.contacts.insert((1, 0, 0), Contact::BlueIce);
        let plan = tick(&mut no_soil, (0, 0, 0), Kind::Lava, true, false).unwrap();
        assert!(plan.solidifications.is_empty());
        let mut no_ice = ContactWorld::new(-1);
        no_ice
            .cells
            .insert((0, 0, 0), Cell::Fluid(fluid(Kind::Lava, 0)));
        no_ice.contacts.insert((0, -1, 0), Contact::SoulSoil);
        let plan = tick(&mut no_ice, (0, 0, 0), Kind::Lava, true, false).unwrap();
        assert!(plan.solidifications.is_empty());
        // A reader with no block vocabulary cannot produce basalt at all, which is the
        // fail-closed answer rather than a guessed one.
        let mut fluid_only = fixture_with_floor(fluid(Kind::Lava, 0));
        let plan = tick(&mut fluid_only, (0, 0, 0), Kind::Lava, true, false).unwrap();
        assert!(plan.solidifications.is_empty());
    }

    /// The water test runs before the blue-ice test inside the *same* iteration, so the
    /// direction order decides a scene where both are present: water at UP wins because
    /// UP comes first, while blue ice at UP wins over water at NORTH.
    #[test]
    fn should_spread_liquid_tests_water_before_blue_ice_per_direction() {
        let mut water_first = ContactWorld::new(-1);
        water_first
            .cells
            .insert((0, 0, 0), Cell::Fluid(fluid(Kind::Lava, 0)));
        water_first
            .cells
            .insert((0, 1, 0), Cell::Fluid(fluid(Kind::Water, 0)));
        water_first.contacts.insert((0, -1, 0), Contact::SoulSoil);
        water_first.contacts.insert((0, 0, -1), Contact::BlueIce);
        let plan = tick(&mut water_first, (0, 0, 0), Kind::Lava, true, false).unwrap();
        assert_eq!(
            plan.solidifications,
            vec![((0, 0, 0), Solidification::Obsidian)]
        );

        let mut ice_first = ContactWorld::new(-1);
        ice_first
            .cells
            .insert((0, 0, 0), Cell::Fluid(fluid(Kind::Lava, 0)));
        ice_first
            .cells
            .insert((0, 0, -1), Cell::Fluid(fluid(Kind::Water, 0)));
        ice_first.contacts.insert((0, -1, 0), Contact::SoulSoil);
        ice_first.contacts.insert((0, 1, 0), Contact::BlueIce);
        let plan = tick(&mut ice_first, (0, 0, 0), Kind::Lava, true, false).unwrap();
        assert_eq!(
            plan.solidifications,
            vec![((0, 0, 0), Solidification::Basalt)]
        );
    }

    /// `SpongeBlock.removeWaterBreadthFirstSearch` absorbs inside `MAX_COUNT + 1` = 65
    /// accepted cells, and the sponge's own cell is one of them, so a tank deep enough
    /// to fill the budget yields 64 water cells. The walk allocates nothing while doing
    /// it.
    #[test]
    fn a_sponge_absorbs_within_vanillas_sixty_five_cell_budget() {
        let mut world = ContactWorld::new(0);
        for x in -2..=2 {
            for y in -2..=2 {
                for z in -2..=2 {
                    if (x, y, z) != (0, 0, 0) {
                        world
                            .cells
                            .insert((x, y, z), Cell::Fluid(fluid(Kind::Water, 0)));
                    }
                }
            }
        }
        let absorbed = sponge_absorb_water(&mut world, (0, 0, 0)).unwrap();
        assert_eq!(SPONGE_MAX_CELLS, 65, "MAX_COUNT + 1");
        assert_eq!(
            absorbed.len(),
            SPONGE_MAX_CELLS - 1,
            "the sponge's own cell spends one of the 65 accepted cells"
        );
        let mut unique = absorbed.to_vec();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), absorbed.len(), "a cell is absorbed once");
        assert_eq!(
            absorbed[0],
            (0, -1, 0),
            "DOWN is the first direction vanilla drains"
        );
    }

    /// The depth cap is `MAX_DEPTH = 6` and it gates *expansion*: a cell six steps out is
    /// absorbed, the cell seven steps out is never reached.
    #[test]
    fn a_sponge_reaches_six_cells_and_no_further() {
        let mut world = ContactWorld::new(0);
        for x in 1..=8 {
            world
                .cells
                .insert((x, 0, 0), Cell::Fluid(fluid(Kind::Water, 0)));
        }
        let absorbed = sponge_absorb_water(&mut world, (0, 0, 0)).unwrap();
        assert_eq!(
            absorbed.to_vec(),
            (1..=6).map(|x| (x, 0, 0)).collect::<Vec<Pos>>(),
            "depth {} stops the walk",
            SPONGE_MAX_DEPTH
        );
        assert_eq!(SPONGE_MAX_DEPTH, 6);
    }

    /// No water in reach: nothing absorbed, nothing allocated — the queue and the result
    /// are fixed arrays. Flowing water is absorbed like a source (vanilla asks
    /// `FluidTags.WATER`), lava is not.
    #[test]
    fn a_sponge_in_dry_air_absorbs_nothing_and_ignores_lava() {
        let mut dry = ContactWorld::new(0);
        dry.cells.insert((3, 0, 0), Cell::Solid);
        assert!(sponge_absorb_water(&mut dry, (0, 0, 0)).unwrap().is_empty());

        let mut world = ContactWorld::new(0);
        world
            .cells
            .insert((1, 0, 0), Cell::Fluid(fluid(Kind::Water, 5)));
        world
            .cells
            .insert((2, 0, 0), Cell::Fluid(fluid(Kind::Water, 8)));
        world
            .cells
            .insert((-1, 0, 0), Cell::Fluid(fluid(Kind::Lava, 0)));
        let absorbed = sponge_absorb_water(&mut world, (0, 0, 0)).unwrap();
        assert_eq!(absorbed.to_vec(), vec![(1, 0, 0), (2, 0, 0)]);
    }

    /// `BubbleColumnBlock.getColumnState`: `minecraft:soul_sand` drags up,
    /// `minecraft:magma_block` drags down, every other base is the water fallback.
    #[test]
    fn bubble_column_state_classifies_soul_sand_up_and_magma_down() {
        assert_eq!(bubble_column_state(Contact::SoulSand), Column::Up);
        assert_eq!(bubble_column_state(Contact::MagmaBlock), Column::Down);
        assert_eq!(bubble_column_state(Contact::Other), Column::Water);
        assert_eq!(bubble_column_state(Contact::SoulSoil), Column::Water);
        assert_eq!(bubble_column_state(Contact::BlueIce), Column::Water);
    }

    /// `BubbleColumnBlock.updateColumn`: the column takes its drag from the base below,
    /// overwrites every full water source above it with that column state, keeps going
    /// through an existing bubble column, and stops at flowing water or air.
    #[test]
    fn bubble_column_fills_water_sources_upward_and_stops_at_a_gap() {
        let mut world = ContactWorld::new(0);
        world.contacts.insert((0, 0, 0), Contact::SoulSand);
        world
            .cells
            .insert((0, 1, 0), Cell::Fluid(fluid(Kind::Water, 0)));
        world
            .cells
            .insert((0, 2, 0), Cell::Fluid(fluid(Kind::Water, 0)));
        world.contacts.insert((0, 3, 0), Contact::BubbleColumn);
        let mut out = Vec::new();
        let column = bubble_column_update(&mut world, (0, 1, 0), &mut out).unwrap();
        assert_eq!(column, Some(Column::Up));
        assert_eq!(out, vec![(0, 1, 0), (0, 2, 0), (0, 3, 0)]);
        // Air above the column ends the walk.
        assert!(!out.contains(&(0, 4, 0)));

        // A magma base drags the other way.
        world.contacts.insert((0, 0, 0), Contact::MagmaBlock);
        let column = bubble_column_update(&mut world, (0, 1, 0), &mut out).unwrap();
        assert_eq!(column, Some(Column::Down));

        // A base that is neither keeps vanilla's water fallback.
        world.contacts.insert((0, 0, 0), Contact::Other);
        let column = bubble_column_update(&mut world, (0, 1, 0), &mut out).unwrap();
        assert_eq!(column, Some(Column::Water));

        // Flowing water is not `canBePlacedIn`, so the column stops below it.
        world.contacts.insert((0, 0, 0), Contact::SoulSand);
        world
            .cells
            .insert((0, 2, 0), Cell::Fluid(fluid(Kind::Water, 8)));
        let column = bubble_column_update(&mut world, (0, 1, 0), &mut out).unwrap();
        assert_eq!(column, Some(Column::Up));
        assert_eq!(out, vec![(0, 1, 0)]);

        // `updateColumn` returns early when its own cell cannot hold a column, and the
        // scratch list is left empty rather than stale.
        let column = bubble_column_update(&mut world, (0, 4, 0), &mut out).unwrap();
        assert_eq!(column, None);
        assert!(out.is_empty());
    }
}
