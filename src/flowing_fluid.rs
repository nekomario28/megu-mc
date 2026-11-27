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
use megu_core::ext_gate::ExtGate;
use megu_core::id::ResourceLocation;
use std::sync::{Arc, OnceLock, RwLock};

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

/// `FluidState.isSource()`: level 0 is the source, 1..=7 is flowing and 8.. is falling.
///
/// The arithmetic lives on the level alone so the vanilla [`Fluid`] and a modded
/// [`LiquidCell::Fluid`] answer from one owner; [`Fluid::source`] and friends are the
/// vanilla-facing spellings of these three functions.
#[inline]
fn is_source_level(level: u8) -> bool {
    level == 0
}

/// `FluidState.isSource() == false` above the source level and `getAmount() == 8`
/// both for a source and for a falling column.
#[inline]
fn is_falling_level(level: u8) -> bool {
    level >= 8
}

/// `FluidState.getAmount()`: 8 at the source and in a falling column, `8 - level`
/// for a flowing one.
#[inline]
fn amount_of_level(level: u8) -> u8 {
    if is_source_level(level) || is_falling_level(level) {
        8
    } else {
        8 - level
    }
}

impl Fluid {
    pub(crate) fn source(self) -> bool {
        is_source_level(self.level)
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

/// The fluid identity the shared flow machine runs on: a vanilla [`Kind`], or one
/// slot of [`CustomFluidRegistry`].
///
/// Vanilla water and lava keep their own zero-cost path: [`Liquid::Vanilla`] is the
/// value every vanilla tick builds, and the machine's comparisons against it are the
/// same `kind == kind` integer compares they were before this extension existed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Liquid {
    Vanilla(Kind),
    /// The id [`CustomFluidRegistry::register`] handed out. It never appears on the
    /// vanilla [`Cell`] vocabulary, so a vanilla caller cannot observe one.
    Custom(u16),
}

/// The flow machine's cell vocabulary: [`Cell`] plus a modded fluid identity.
///
/// [`tick_in_dim`] reads through [`Reader::fluid_cell`] and answers a [`Plan`], so the
/// public vanilla vocabulary is unchanged; a custom fluid keeps its registry id here
/// instead of being coerced into one of the two vanilla kinds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LiquidCell {
    Air,
    Solid,
    /// The [`Cell::Passable`] arm: non-air, no fluid, empty collision shape.
    Passable,
    Fluid {
        liquid: Liquid,
        level: u8,
    },
}

impl LiquidCell {
    /// A vanilla [`Cell`] in the extended vocabulary, i.e. whatever
    /// [`Reader::fluid_cell`]'s default answer carries. `#[inline]`, so the vanilla tick's
    /// read path compiles to the match [`Reader::cell`] already answered.
    #[inline]
    fn from_cell(cell: Cell) -> Self {
        match cell {
            Cell::Air => LiquidCell::Air,
            Cell::Solid => LiquidCell::Solid,
            Cell::Passable => LiquidCell::Passable,
            Cell::Fluid(fluid) => LiquidCell::Fluid {
                liquid: Liquid::Vanilla(fluid.kind),
                level: fluid.level,
            },
        }
    }

    /// The vanilla [`Cell`] this reads as, or `None` for a modded fluid — the vanilla
    /// [`Plan`] has no room for one, and inventing a kind for it would be a guess.
    #[inline]
    fn to_cell(self) -> Option<Cell> {
        match self {
            LiquidCell::Air => Some(Cell::Air),
            LiquidCell::Solid => Some(Cell::Solid),
            LiquidCell::Passable => Some(Cell::Passable),
            LiquidCell::Fluid {
                liquid: Liquid::Vanilla(kind),
                level,
            } => Some(Cell::Fluid(Fluid { kind, level })),
            LiquidCell::Fluid {
                liquid: Liquid::Custom(_),
                ..
            } => None,
        }
    }
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

    /// The cell at `pos` in the extended vocabulary, i.e. [`Cell`] plus a modded
    /// fluid identity. The default maps [`Reader::cell`] into it, which is the whole
    /// answer for a world that only holds vanilla fluids; a world a mod loader fills
    /// with custom fluids overrides this so [`tick_custom_fluid`] can see them.
    #[inline]
    fn fluid_cell(&mut self, pos: Pos) -> Result<LiquidCell, Self::Error> {
        Ok(LiquidCell::from_cell(self.cell(pos)?))
    }

    /// The registry name of whatever occupies `pos` — the fluid's own name where a
    /// fluid is present, the block's name otherwise. It is what a modded fluid's
    /// [`FluidTypeSpec::contact_reactions`] table is matched against.
    ///
    /// The default is `None`: a reader with no name vocabulary has no contact
    /// reactions, which is the honest "none of these rules applies" rather than a
    /// guessed name.
    fn name_at(&mut self, pos: Pos) -> Result<Option<String>, Self::Error> {
        let _ = pos;
        Ok(None)
    }
}

pub(crate) struct Plan {
    pub writes: Vec<(Pos, Cell)>,
    /// Lava-to-block changes that the caller must apply with its block writer.
    pub solidifications: Vec<(Pos, Solidification)>,
    pub updated_self: Option<Fluid>,
}

/// One modded fluid's state: its [`CustomFluidRegistry`] id and fluid level.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CustomFluidState {
    pub(crate) id: u16,
    pub(crate) level: u8,
}

/// One block a modded fluid solidified into at `pos`, resolved from its spec's
/// [`FluidContactReaction`] table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CustomSolidification {
    pub(crate) pos: Pos,
    /// The registry name of the block the reaction writes, i.e. the spec's
    /// `source_result` or `flowing_result` for the contact that fired.
    pub(crate) block: String,
    /// Whether the reacting fluid was a source, i.e. which of the two results was used.
    pub(crate) fluid_was_source: bool,
}

/// The plan of one modded fluid tick, the counterpart of [`Plan`] for a fluid the
/// vanilla vocabulary cannot name.
///
/// `writes` carry the same cell vocabulary the tick read (`LiquidCell`), so a caller
/// writes an air cell or a [`Liquid::Custom`] fluid and never has to map the modded id
/// back through a vanilla kind.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CustomPlan {
    pub(crate) writes: Vec<(Pos, LiquidCell)>,
    /// Contact reactions, resolved to block names. Vanilla's [`Solidification`] enum is
    /// a closed set; a modded fluid's results are whatever its spec declared.
    pub(crate) solidifications: Vec<CustomSolidification>,
    pub(crate) updated_self: Option<CustomFluidState>,
}

/// The raw output of the shared flow machine before it is spelled in a vocabulary.
struct Flow {
    writes: Vec<(Pos, LiquidCell)>,
    reactions: Vec<(Pos, Reaction)>,
    updated_self: Option<u8>,
}

/// What a contact turned into, in either vocabulary: the vanilla [`Solidification`], or a
/// slot of the driving spec's contact table plus which of its two results applies.
///
/// `Copy`, so a plan can hold it without cloning a name per reaction; the name is
/// resolved once, when the plan is spelled for its caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reaction {
    Solidification(Solidification),
    Custom { reaction: u16, source: bool },
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

struct Context<'a, 's, W> {
    world: &'a mut W,
    overlay: CoordMap<Pos, LiquidCell>,
    writes: Vec<(Pos, LiquidCell)>,
    reactions: Vec<(Pos, Reaction)>,
    /// The fluid this tick is scheduled for.
    liquid: Liquid,
    /// `getDropOff` for `liquid` in the caller's dimension.
    drop_off: u8,
    /// `getSlopeFindDistance` for `liquid` in the caller's dimension: 4 for
    /// water and ultrawarm lava, 2 for lava elsewhere and whatever the spec says
    /// for a modded fluid.
    slope_limit: u8,
    /// `FlowableFluid.canConvertToSource` (`waterSourceConversion` /
    /// `lavaSourceConversion` for the vanilla kinds, `FluidTypeSpec::can_convert_to_source`
    /// for a modded one), resolved per fluid by the caller so the rule itself is not
    /// re-checked here.
    source_conversion: bool,
    /// The spec driving a [`Liquid::Custom`] tick: its contact table is what a modded
    /// fluid solidifies against. `None` for the two vanilla kinds, which carry their
    /// arms in code.
    spec: Option<&'s FluidTypeSpec>,
}

impl<W: Reader> Context<'_, '_, W> {
    #[inline]
    fn get(&mut self, p: Pos) -> Result<LiquidCell, W::Error> {
        if let Some(&cell) = self.overlay.get(&p) {
            Ok(cell)
        } else {
            self.world.fluid_cell(p)
        }
    }
    #[inline]
    fn same(&self, cell: LiquidCell) -> Option<u8> {
        match cell {
            LiquidCell::Fluid { liquid, level } if liquid == self.liquid => Some(level),
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
    fn put(&mut self, p: Pos, cell: LiquidCell) -> Result<(), W::Error> {
        if self.get(p)? != cell {
            self.overlay.insert(p, cell);
            self.writes.push((p, cell));
        }
        Ok(())
    }
    fn can_pass(&mut self, p: Pos) -> Result<bool, W::Error> {
        let cell = self.get(p)?;
        Ok(!matches!(cell, LiquidCell::Solid) && !self.same(cell).is_some_and(is_source_level))
    }
    fn hole(&mut self, p: Pos) -> Result<bool, W::Error> {
        let below = self.get(add(p, (0, -1, 0)))?;
        Ok(self.same(below).is_some() || !matches!(below, LiquidCell::Solid))
    }
    fn new_liquid(&mut self, p: Pos) -> Result<Option<u8>, W::Error> {
        let mut maximum = 0;
        let mut sources = 0;
        for d in HORIZONTAL {
            let cell = self.get(add(p, d))?;
            if let Some(level) = self.same(cell) {
                maximum = maximum.max(amount_of_level(level));
                sources += u8::from(is_source_level(level));
            }
        }
        // `source_conversion` carries `FlowableFluid.canConvertToSource`, resolved per
        // fluid by the caller.
        if self.source_conversion && sources >= 2 {
            let below = self.get(add(p, (0, -1, 0)))?;
            if matches!(below, LiquidCell::Solid) || self.same(below).is_some_and(is_source_level) {
                return Ok(Some(0));
            }
        }
        let above = self.get(add(p, (0, 1, 0)))?;
        if self.same(above).is_some() {
            return Ok(Some(8));
        }
        let amount = maximum.saturating_sub(self.drop_off);
        Ok((amount > 0).then_some(8 - amount))
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
    /// The spec rule that turns a contact at `p` into a block: the name of whatever
    /// occupies `p` is matched against [`FluidTypeSpec::contact_reactions`], and `source`
    /// picks `source_result` over `flowing_result`.
    ///
    /// `None` leaves the contact alone — a fluid with no rule for what it touched does
    /// not react to it. Reaching [`Reader::name_at`] is what keeps the answer the world's
    /// own rather than a guess from the id's spelling.
    fn custom_contact(&mut self, p: Pos, source: bool) -> Result<Option<Reaction>, W::Error> {
        let Some(spec) = self.spec else {
            return Ok(None);
        };
        let Some(name) = self.world.name_at(p)? else {
            return Ok(None);
        };
        let Some(index) = spec
            .contact_reactions
            .iter()
            .position(|reaction| reaction.other_fluid_or_block == name)
        else {
            return Ok(None);
        };
        Ok(u16::try_from(index)
            .ok()
            .map(|reaction| Reaction::Custom { reaction, source }))
    }
    fn spread_to(&mut self, p: Pos, level: u8) -> Result<(), W::Error> {
        match self.get(p)? {
            // `spreadTo`: a non-air cell outside the waterlogged arm is destroyed first and the
            // fluid written over it. `beforeDestroyingBlock` is a no-op for water.
            LiquidCell::Air | LiquidCell::Passable => self.put(
                p,
                LiquidCell::Fluid {
                    liquid: self.liquid,
                    level,
                },
            ),
            LiquidCell::Fluid {
                liquid: other,
                level: other_level,
            } if other != self.liquid => {
                match (self.liquid, other) {
                    // Vanilla water entering lava: `minecraft:obsidian` for a lava source and
                    // `minecraft:cobblestone` for a flowing one, at the lava's own cell.
                    (Liquid::Vanilla(Kind::Water), Liquid::Vanilla(Kind::Lava)) => {
                        self.reactions.push((
                            p,
                            Reaction::Solidification(if is_source_level(other_level) {
                                Solidification::Obsidian
                            } else {
                                Solidification::Cobblestone
                            }),
                        ));
                    }
                    // Lava entering water reacts at its originating lava cell instead.
                    // The caller's lava tick recognizes that contact before spreading.
                    (Liquid::Vanilla(Kind::Lava), Liquid::Vanilla(Kind::Water)) => {
                        return Err(self.world.unsupported_contact(p));
                    }
                    // A modded fluid's own contact table decides, keyed on *its* state: a
                    // spec's `source_result`/`flowing_result` pair describes the fluid that
                    // arrives. No rule means no reaction and no write.
                    _ => {
                        if let Some(reaction) = self.custom_contact(p, is_source_level(level))? {
                            self.reactions.push((p, reaction));
                        }
                    }
                }
                Ok(())
            }
            // Same fluid already in the target: `spreadTo` writes `fluidState.createLegacyBlock()`
            // over it all the same, which is how the computed falling state replaces the flowing
            // cell underneath a source. `put` drops the write when the level is unchanged.
            LiquidCell::Fluid { .. } => self.put(
                p,
                LiquidCell::Fluid {
                    liquid: self.liquid,
                    level,
                },
            ),
            // Unreachable through `canSpreadTo`, which requires a wall the fluid can pass and a
            // cell `canHoldFluid` accepts; a full cube is refused rather than overwritten.
            LiquidCell::Solid => Ok(()),
        }
    }
    fn spread_sides(&mut self, p: Pos, level: u8) -> Result<(), W::Error> {
        let strength = if is_falling_level(level) {
            7
        } else {
            amount_of_level(level).saturating_sub(self.drop_off)
        };
        if strength == 0 {
            return Ok(());
        }
        let mut best = 1000;
        // `getSpread` returns an `EnumMap<Direction, FluidState>`, so at most four entries, and
        // the caller applies them in `entrySet()` order: `Direction` ordinals are
        // DOWN, UP, NORTH, SOUTH, WEST, EAST. A fixed array keeps the hot path allocation-free.
        let mut choices: [(usize, Pos, Option<u8>); 4] = [(0, (0, 0, 0), None); 4];
        let mut chosen = 0;
        for (i, d) in HORIZONTAL.into_iter().enumerate() {
            let q = add(p, d);
            if !self.can_pass(q)? {
                continue;
            }
            let next = self.new_liquid(q)?;
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
            if let Some(level) = next {
                self.spread_to(q, level)?;
            }
        }
        Ok(())
    }
    /// `LiquidBlock.shouldSpreadLiquid`: the block the fluid turns into when it touches
    /// something that reacts with it, written at the fluid's own cell.
    ///
    /// Vanilla lava walks [`LIQUID_CONTACT`] once, testing the visited cell's fluid for
    /// water first and the cell itself for `minecraft:blue_ice` second, so a water
    /// contact that comes later in the order wins over an earlier blue-ice one. `flag` —
    /// the block below is `minecraft:soul_soil` — is read once, before the loop. A modded
    /// fluid walks the same order and matches each visited cell's [`Reader::name_at`]
    /// against its spec's table; vanilla water has no arm at all.
    fn contact_reaction(&mut self, p: Pos, source: bool) -> Result<Option<Reaction>, W::Error> {
        match self.liquid {
            Liquid::Vanilla(Kind::Water) => Ok(None),
            Liquid::Vanilla(Kind::Lava) => {
                let soul_soil_below = self.contact(add(p, (0, -1, 0)))? == Contact::SoulSoil;
                for offset in LIQUID_CONTACT {
                    let neighbour = add(p, offset);
                    if matches!(
                        self.get(neighbour)?,
                        LiquidCell::Fluid {
                            liquid: Liquid::Vanilla(Kind::Water),
                            ..
                        }
                    ) {
                        return Ok(Some(Reaction::Solidification(if source {
                            Solidification::Obsidian
                        } else {
                            Solidification::Cobblestone
                        })));
                    }
                    if soul_soil_below && self.contact(neighbour)? == Contact::BlueIce {
                        return Ok(Some(Reaction::Solidification(Solidification::Basalt)));
                    }
                }
                Ok(None)
            }
            Liquid::Custom(_) => {
                for offset in LIQUID_CONTACT {
                    if let Some(reaction) = self.custom_contact(add(p, offset), source)? {
                        return Ok(Some(reaction));
                    }
                }
                Ok(None)
            }
        }
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

/// The physics one fluid brings to a tick: the getters `FlowingFluid` lets a fluid
/// override, resolved once by the caller from the fluid's dimension and its spec.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Physics {
    /// `getDropOff` for this fluid in this dimension.
    drop_off: u8,
    /// `getSlopeFindDistance` for this fluid in this dimension.
    slope_limit: u8,
    /// `canConvertToSource` for this fluid.
    source_conversion: bool,
}

/// One scheduled fluid tick of `liquid` at `p`, in the vocabulary the fluid belongs to.
///
/// This is the whole machine — contact, level decay, fall, side spread — over
/// [`LiquidCell`], and both public entry points are thin spellings of its answer: a
/// vanilla [`Kind`] for [`tick_in_dim`], a [`CustomFluidRegistry`] id for
/// [`tick_custom_fluid`]. The physics a modded fluid gets are therefore the physics the
/// vanilla kinds get, with `drop_off`, `slope_limit`, `source_conversion` and the contact
/// table supplied by its [`FluidTypeSpec`] instead of by the two hardcoded arms.
fn run_flow<W: Reader>(
    world: &mut W,
    p: Pos,
    liquid: Liquid,
    physics: Physics,
    spec: Option<&FluidTypeSpec>,
    expected_source: bool,
) -> Result<Flow, W::Error> {
    let mut cx = Context {
        world,
        overlay: CoordMap::default(),
        writes: Vec::new(),
        reactions: Vec::new(),
        liquid,
        drop_off: physics.drop_off,
        slope_limit: physics.slope_limit,
        source_conversion: physics.source_conversion,
        spec,
    };
    let cell = cx.get(p)?;
    let Some(mut current) = cx
        .same(cell)
        .filter(|level| is_source_level(*level) == expected_source)
    else {
        return Ok(Flow {
            writes: cx.writes,
            reactions: cx.reactions,
            updated_self: None,
        });
    };

    // `LiquidBlock.shouldSpreadLiquid` for the fluid's own kind: lava's water/basalt arms,
    // nothing for water, and the spec's contact table for a modded fluid.
    if let Some(reaction) = cx.contact_reaction(p, is_source_level(current))? {
        cx.reactions.push((p, reaction));
        return Ok(Flow {
            writes: cx.writes,
            reactions: cx.reactions,
            updated_self: None,
        });
    }
    let mut updated_self = None;
    if !is_source_level(current) {
        match cx.new_liquid(p)? {
            None => {
                cx.put(p, LiquidCell::Air)?;
                return Ok(Flow {
                    writes: cx.writes,
                    reactions: cx.reactions,
                    updated_self,
                });
            }
            Some(next) if next != current => {
                cx.put(
                    p,
                    LiquidCell::Fluid {
                        liquid,
                        level: next,
                    },
                )?;
                current = next;
                updated_self = Some(next);
            }
            Some(_) => {}
        }
    }
    let below = add(p, (0, -1, 0));
    let below_state = cx.get(below)?;
    if let LiquidCell::Fluid {
        liquid: other,
        level: other_level,
    } = below_state
    {
        if other != liquid {
            let reaction = match (liquid, other) {
                // `LavaFluid.spreadTo(Direction.DOWN)` turns a `LiquidBlock` below into
                // `minecraft:stone` — whatever fluid that block carried.
                (Liquid::Vanilla(Kind::Lava), _) => {
                    Some(Reaction::Solidification(Solidification::Stone))
                }
                // Water spreading down onto lava.
                (Liquid::Vanilla(Kind::Water), Liquid::Vanilla(Kind::Lava)) => {
                    Some(Reaction::Solidification(if is_source_level(other_level) {
                        Solidification::Obsidian
                    } else {
                        Solidification::Cobblestone
                    }))
                }
                // A modded fluid consults its own contact table for what is below, and a
                // vanilla fluid meeting one it has no rule for writes nothing.
                _ => cx.custom_contact(below, is_source_level(current))?,
            };
            if let Some(reaction) = reaction {
                cx.reactions.push((below, reaction));
            }
            // Either way the tick stops at the foreign fluid: vanilla always has a result
            // here, and a modded fluid with no rule writes nothing rather than guessing.
            return Ok(Flow {
                writes: cx.writes,
                reactions: cx.reactions,
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
        if let Some(next) = cx.new_liquid(below)? {
            cx.spread_to(below, next)?;
        }
        let mut sources = 0;
        for d in HORIZONTAL {
            let state = cx.get(add(p, d))?;
            sources += u8::from(cx.same(state).is_some_and(is_source_level));
        }
        if sources >= 3 {
            cx.spread_sides(p, current)?;
        }
    } else if is_source_level(current) || !cx.hole(p)? {
        cx.spread_sides(p, current)?;
    }
    Ok(Flow {
        writes: cx.writes,
        reactions: cx.reactions,
        updated_self,
    })
}

/// A [`Flow`] spelled in the vanilla vocabulary: every write is a [`Cell`] and every
/// reaction a [`Solidification`].
fn vanilla_plan(kind: Kind, flow: Flow) -> Plan {
    Plan {
        writes: flow
            .writes
            .into_iter()
            .filter_map(|(pos, cell)| match cell.to_cell() {
                Some(cell) => Some((pos, cell)),
                None => {
                    // A vanilla tick writes only `Liquid::Vanilla`: the context's own liquid
                    // is the only fluid it ever puts, and it never re-spells a neighbour.
                    debug_assert!(
                        matches!(
                            cell,
                            LiquidCell::Fluid {
                                liquid: Liquid::Custom(_),
                                ..
                            }
                        ),
                        "a vanilla fluid tick wrote a cell it cannot express"
                    );
                    None
                }
            })
            .collect(),
        solidifications: flow
            .reactions
            .into_iter()
            .filter_map(|(pos, reaction)| match reaction {
                Reaction::Solidification(solidification) => Some((pos, solidification)),
                // A vanilla context carries no contact table, so it never records one.
                Reaction::Custom { .. } => None,
            })
            .collect(),
        updated_self: flow.updated_self.map(|level| Fluid { kind, level }),
    }
}

/// A [`Flow`] spelled for a modded fluid: the cells stay in the extended vocabulary (a
/// modded fluid has no vanilla [`Cell`]), and each reaction resolves through the spec's
/// contact table to the block name the caller writes.
fn custom_plan(spec: &FluidTypeSpec, id: u16, flow: Flow) -> CustomPlan {
    CustomPlan {
        writes: flow.writes,
        solidifications: flow
            .reactions
            .into_iter()
            .filter_map(|(pos, reaction)| match reaction {
                Reaction::Custom { reaction, source } => spec
                    .contact_reactions
                    .get(reaction as usize)
                    .map(|rule| CustomSolidification {
                        pos,
                        block: if source {
                            rule.source_result.clone()
                        } else {
                            rule.flowing_result.clone()
                        },
                        fluid_was_source: source,
                    }),
                // A modded context records only its own contact table.
                Reaction::Solidification(_) => None,
            })
            .collect(),
        updated_self: flow
            .updated_self
            .map(|level| CustomFluidState { id, level }),
    }
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
            if lava_spreads_fast_in_dim(dim) { 4 } else { 2 },
        ),
    };
    let flow = run_flow(
        world,
        p,
        Liquid::Vanilla(kind),
        Physics {
            drop_off,
            slope_limit,
            source_conversion,
        },
        None,
        expected_source,
    )?;
    Ok(vanilla_plan(kind, flow))
}

// ---------- modded fluid types ----------

/// How many modded fluid types one process may register: the bound that keeps a hostile or
/// broken pack from growing the table without limit.
pub(crate) const MAX_CUSTOM_FLUIDS: usize = 256;

/// `FlowingFluid.getDropOff` costs at least one level per step and at most seven, so a
/// registered spec is clamped into that range before it is stored.
const MIN_CUSTOM_DROP_OFF: u8 = 1;
const MAX_CUSTOM_DROP_OFF: u8 = 7;

/// `getSlopeFindDistance` is `1..=4` in vanilla — water and ultrawarm lava use 4, lava 2 —
/// so a registered spec is clamped into the same range.
const MIN_CUSTOM_SLOPE_DISTANCE: u8 = 1;
const MAX_CUSTOM_SLOPE_DISTANCE: u8 = 4;

/// The lowest tick delay a registered spec may carry. `getTickDelay` never answers 0 in
/// vanilla: a scheduled tick that reschedules itself after zero game ticks would re-enter
/// the same cell forever, so the bound is enforced rather than trusted.
const MIN_CUSTOM_TICK_DELAY: u32 = 1;

/// `DimensionType.ultraWarm` as a function of the protocol dimension index. Ultrawarm is
/// exactly the predicate vanilla lava reads, so a modded fluid's `ultrawarm_*` fields answer
/// from the same one place instead of re-deciding which dimension is warm.
fn is_ultrawarm_dim(dim: i32) -> bool {
    lava_spreads_fast_in_dim(dim)
}

/// One contact between a modded fluid and something else, and the block it solidifies into.
///
/// `other_fluid_or_block` is matched against [`Reader::name_at`], i.e. against the registry
/// name of the fluid or block the reacting fluid touched. Both result names go through
/// [`megu_core::id::ResourceLocation`] when the spec is registered, so a reaction cannot name
/// something the rest of the engine could not resolve.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FluidContactReaction {
    pub(crate) other_fluid_or_block: String,
    /// The block written when the reacting fluid is a source.
    pub(crate) source_result: String,
    /// The block written when the reacting fluid is flowing.
    pub(crate) flowing_result: String,
}

impl FluidContactReaction {
    pub(crate) fn new(
        other_fluid_or_block: &str,
        source_result: &str,
        flowing_result: &str,
    ) -> Self {
        Self {
            other_fluid_or_block: other_fluid_or_block.to_string(),
            source_result: source_result.to_string(),
            flowing_result: flowing_result.to_string(),
        }
    }
}

/// Every `FlowingFluid` getter a fluid may override, for a fluid the engine does not know.
///
/// A vanilla [`Kind`] answers these from code (`LavaFluid`'s dimension arms, water's fixed
/// values); a modded fluid answers them from its spec, and the machine itself is the same
/// one — [`run_flow`] reads [`Physics`] and nothing else, so a modded fluid decays, falls,
/// slopes and spreads exactly the way the vanilla two do.
///
/// Registration clamps the numeric fields into their `FlowingFluid` ranges
/// (`drop_off`/`ultrawarm_drop_off` into `1..=7`, `slope_distance` /
/// `ultrawarm_slope_distance` into `1..=4`) and refuses a spec whose `id` or whose reaction
/// names are not valid resource locations, so `None` from [`register_custom_fluid`] always
/// means "refused", never "registered something unusable".
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FluidTypeSpec {
    /// The canonical `namespace:path` the fluid registers under, e.g. `mymod:crude_oil`.
    pub(crate) id: String,
    /// `getTickDelay` in a dimension that is not ultrawarm.
    pub(crate) tick_delay: u32,
    /// `getTickDelay` in an ultrawarm dimension.
    pub(crate) ultrawarm_tick_delay: u32,
    /// `getDropOff` outside an ultrawarm dimension, clamped into `1..=7`.
    pub(crate) drop_off: u8,
    /// `getDropOff` in an ultrawarm dimension, clamped into `1..=7`.
    pub(crate) ultrawarm_drop_off: u8,
    /// `getSlopeFindDistance` outside an ultrawarm dimension, clamped into `1..=4`.
    pub(crate) slope_distance: u8,
    /// `getSlopeFindDistance` in an ultrawarm dimension, clamped into `1..=4`.
    pub(crate) ultrawarm_slope_distance: u8,
    /// `canConvertToSource`: whether two neighbouring sources and a supported floor turn a
    /// flowing cell of this fluid into a source.
    pub(crate) can_convert_to_source: bool,
    /// The contact table, in match order: the first rule whose `other_fluid_or_block` the
    /// touched cell answers wins.
    pub(crate) contact_reactions: Vec<FluidContactReaction>,
}

impl FluidTypeSpec {
    /// A spec with water's physics — tick delay 5 in both dimensions, `drop_off` 1, slope
    /// distance 4 — no source conversion and no contact reactions.
    ///
    /// Source conversion is off by default because it is a gamerule in vanilla, not a fluid
    /// property: a modded fluid converts only when its loader says so.
    pub(crate) fn new(id: &str) -> Self {
        Self {
            id: id.to_string(),
            tick_delay: 5,
            ultrawarm_tick_delay: 5,
            drop_off: 1,
            ultrawarm_drop_off: 1,
            slope_distance: 4,
            ultrawarm_slope_distance: 4,
            can_convert_to_source: false,
            contact_reactions: Vec::new(),
        }
    }

    /// `getTickDelay` for this fluid in `dim`.
    pub(crate) fn tick_delay_for_dim(&self, dim: i32) -> u64 {
        u64::from(if is_ultrawarm_dim(dim) {
            self.ultrawarm_tick_delay
        } else {
            self.tick_delay
        })
    }

    /// `getDropOff` for this fluid in `dim`.
    pub(crate) fn drop_off_for_dim(&self, dim: i32) -> u8 {
        if is_ultrawarm_dim(dim) {
            self.ultrawarm_drop_off
        } else {
            self.drop_off
        }
    }

    /// `getSlopeFindDistance` for this fluid in `dim`.
    pub(crate) fn slope_distance_for_dim(&self, dim: i32) -> u8 {
        if is_ultrawarm_dim(dim) {
            self.ultrawarm_slope_distance
        } else {
            self.slope_distance
        }
    }

    /// The spec with every numeric field inside its `FlowingFluid` range.
    fn clamped(mut self) -> Self {
        self.drop_off = self
            .drop_off
            .clamp(MIN_CUSTOM_DROP_OFF, MAX_CUSTOM_DROP_OFF);
        self.ultrawarm_drop_off = self
            .ultrawarm_drop_off
            .clamp(MIN_CUSTOM_DROP_OFF, MAX_CUSTOM_DROP_OFF);
        self.slope_distance = self
            .slope_distance
            .clamp(MIN_CUSTOM_SLOPE_DISTANCE, MAX_CUSTOM_SLOPE_DISTANCE);
        self.ultrawarm_slope_distance = self
            .ultrawarm_slope_distance
            .clamp(MIN_CUSTOM_SLOPE_DISTANCE, MAX_CUSTOM_SLOPE_DISTANCE);
        self.tick_delay = self.tick_delay.max(MIN_CUSTOM_TICK_DELAY);
        self.ultrawarm_tick_delay = self.ultrawarm_tick_delay.max(MIN_CUSTOM_TICK_DELAY);
        self
    }

    /// Every name a contact rule carries must be a resource location, or the rule could
    /// never match a name [`Reader::name_at`] answers.
    fn names_are_valid(&self) -> bool {
        self.contact_reactions.iter().all(|reaction| {
            ResourceLocation::parse(reaction.other_fluid_or_block.trim()).is_ok()
                && ResourceLocation::parse(reaction.source_result.trim()).is_ok()
                && ResourceLocation::parse(reaction.flowing_result.trim()).is_ok()
        })
    }
}

/// The registered modded fluids, in registration (and therefore id) order.
#[derive(Default)]
struct CustomFluidTable {
    fluids: Vec<Arc<FluidTypeSpec>>,
    /// Canonical name -> id, so registering a name twice keeps one id.
    by_name: std::collections::HashMap<String, u16>,
}

/// The process-wide registry of modded fluid types.
///
/// The [`ExtGate`] is the hot path's answer to "does any modded fluid exist?". An engine
/// with no loader answers `false` from one relaxed load and never touches the table; the
/// first registration arms the gate and [`CustomFluidRegistry::clear`] parks it again, so a
/// reload cycle leaves no lock traffic behind.
///
/// The table is append-only — an id never changes hands inside a process — and bounded by
/// [`MAX_CUSTOM_FLUIDS`], so a chunk palette, a scheduled tick or a saved id that already
/// carries one stays valid for the rest of the run.
pub(crate) struct CustomFluidRegistry {
    gate: ExtGate,
    table: OnceLock<RwLock<CustomFluidTable>>,
}

impl CustomFluidRegistry {
    /// Whether anything is registered (one relaxed load, no lock).
    pub(crate) fn is_armed(&self) -> bool {
        self.gate.is_armed()
    }

    fn table(&self) -> &RwLock<CustomFluidTable> {
        self.table
            .get_or_init(|| RwLock::new(CustomFluidTable::default()))
    }

    /// Register `spec` and answer its id, or the id its name already owns.
    ///
    /// `None` means refused: an `id` that is not a valid [`ResourceLocation`], a contact
    /// reaction naming one that is not, or a table already at [`MAX_CUSTOM_FLUIDS`].
    /// Registration is append-only, so the answer for a name never changes inside a process.
    pub(crate) fn register(&self, spec: FluidTypeSpec) -> Option<u16> {
        let id = ResourceLocation::parse(spec.id.trim()).ok()?;
        if !spec.names_are_valid() {
            return None;
        }
        let mut spec = spec.clamped();
        spec.id = id.as_str().to_string();
        let mut table = self.table().write().unwrap_or_else(|e| e.into_inner());
        if let Some(&registered) = table.by_name.get(&spec.id) {
            return Some(registered);
        }
        if table.fluids.len() >= MAX_CUSTOM_FLUIDS {
            return None;
        }
        let slot = u16::try_from(table.fluids.len()).ok()?;
        table.by_name.insert(spec.id.clone(), slot);
        table.fluids.push(Arc::new(spec));
        drop(table);
        let _ = self.gate.arm();
        Some(slot)
    }

    /// The spec of `id`, or `None` when the gate is parked or the id was never handed out.
    ///
    /// The [`Arc`] is a cheap clone, so a caller can drop the registry lock before ticking
    /// the world with the spec it just took.
    pub(crate) fn spec(&self, id: u16) -> Option<Arc<FluidTypeSpec>> {
        if !self.gate.is_armed() {
            return None;
        }
        self.table()
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .fluids
            .get(usize::from(id))
            .cloned()
    }

    /// The id a canonical name already owns, or `None` when nothing registered it.
    pub(crate) fn by_name(&self, name: &str) -> Option<u16> {
        if !self.gate.is_armed() {
            return None;
        }
        let name = ResourceLocation::parse(name.trim()).ok()?;
        self.table()
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .by_name
            .get(name.as_str())
            .copied()
    }

    /// Drop every registration and park the gate: the reload path a loader runs before it
    /// contributes its fluid types again.
    pub(crate) fn clear(&self) {
        {
            let mut table = self.table().write().unwrap_or_else(|e| e.into_inner());
            table.fluids.clear();
            table.by_name.clear();
        }
        let _ = self.gate.disarm();
    }
}

/// The process-wide modded fluid registry. [`ExtGate::new`] is `const`, so this needs no
/// lazy-initialisation machinery and no lock of its own.
static CUSTOM_FLUIDS: CustomFluidRegistry = CustomFluidRegistry {
    gate: ExtGate::new(),
    table: OnceLock::new(),
};

/// The process-wide modded fluid registry.
pub(crate) fn custom_fluids() -> &'static CustomFluidRegistry {
    &CUSTOM_FLUIDS
}

/// Register a modded fluid type and answer its id (see [`CustomFluidRegistry::register`]).
pub(crate) fn register_custom_fluid(spec: FluidTypeSpec) -> Option<u16> {
    CUSTOM_FLUIDS.register(spec)
}

/// The registered spec of `id`, or `None` for an id nothing registered.
pub(crate) fn custom_fluid_spec(id: u16) -> Option<Arc<FluidTypeSpec>> {
    CUSTOM_FLUIDS.spec(id)
}

/// The id a canonical fluid name already owns.
pub(crate) fn custom_fluid_by_name(name: &str) -> Option<u16> {
    CUSTOM_FLUIDS.by_name(name)
}

/// Drop every registered modded fluid and park the gate.
pub(crate) fn clear_custom_fluids() {
    CUSTOM_FLUIDS.clear()
}

/// The `getTickDelay` a scheduler reschedules a modded fluid tick with, in `dim`.
///
/// This is the counterpart of [`fluid_tick_delay_for_dim`] for a fluid the engine has no
/// `Kind` for: a scheduler that hardcodes 5 would run a 40-tick fluid at water speed.
pub(crate) fn custom_fluid_tick_delay(id: u16, dim: i32) -> Option<u64> {
    CUSTOM_FLUIDS
        .spec(id)
        .map(|spec| spec.tick_delay_for_dim(dim))
}

/// [`tick_custom_fluid_in_dim`] in the dimension the reader says its world belongs to, i.e.
/// [`Reader::dim`], mirroring [`tick`] for the vanilla kinds.
pub(crate) fn tick_custom_fluid<W: Reader>(
    world: &mut W,
    p: Pos,
    fluid: u16,
    expected_source: bool,
) -> Result<CustomPlan, W::Error> {
    let dim = world.dim();
    tick_custom_fluid_in_dim(world, p, fluid, expected_source, dim)
}

/// The modded-fluid entry point: one scheduled `FlowingFluid.tick` of the fluid `fluid` at
/// `p`, in the dimension whose protocol index is `dim`.
///
/// The algorithm is [`run_flow`], i.e. the one [`tick_in_dim`] runs; what differs is where
/// the physics come from. `drop_off`, `slope_distance` and `can_convert_to_source` are read
/// off the spec for `dim`, and the contact arms are its [`FluidTypeSpec::contact_reactions`]
/// table matched against the names the world answers — instead of vanilla's two kinds.
///
/// An id nothing registered answers an empty plan rather than an error: the machine has no
/// error to report, and a modded fluid that is not in the registry is a fluid this engine was
/// never told about.
pub(crate) fn tick_custom_fluid_in_dim<W: Reader>(
    world: &mut W,
    p: Pos,
    fluid: u16,
    expected_source: bool,
    dim: i32,
) -> Result<CustomPlan, W::Error> {
    if !CUSTOM_FLUIDS.is_armed() {
        return Ok(CustomPlan::default());
    }
    let Some(spec) = CUSTOM_FLUIDS.spec(fluid) else {
        return Ok(CustomPlan::default());
    };
    let physics = Physics {
        drop_off: spec.drop_off_for_dim(dim),
        slope_limit: spec.slope_distance_for_dim(dim),
        source_conversion: spec.can_convert_to_source,
    };
    let flow = run_flow(
        world,
        p,
        Liquid::Custom(fluid),
        physics,
        Some(&spec),
        expected_source,
    )?;
    Ok(custom_plan(&spec, fluid, flow))
}

/// A [`Reader`] over empty air: the concrete instantiation the signature pins below name, so
/// a build with no world still links the generic modded-fluid entry points and
/// `clippy -D warnings` never reads them as dead code.
struct AirWorld;

impl Reader for AirWorld {
    type Error = Pos;
    fn cell(&mut self, _pos: Pos) -> Result<Cell, Pos> {
        Ok(Cell::Air)
    }
    fn unsupported_contact(&mut self, pos: Pos) -> Pos {
        pos
    }
}

// Loader-only entry points: one pin per signature. Registration happens in a mod loader,
// which this crate does not contain, so without these every entry point below would read as
// dead code in a build that does not compile the tests.
const _: fn(FluidTypeSpec) -> Option<u16> = register_custom_fluid;
const _: fn(u16) -> Option<Arc<FluidTypeSpec>> = custom_fluid_spec;
const _: fn(&str) -> Option<u16> = custom_fluid_by_name;
const _: fn() = clear_custom_fluids;
const _: fn(u16, i32) -> Option<u64> = custom_fluid_tick_delay;
const _: fn() -> &'static CustomFluidRegistry = custom_fluids;
const _: fn(&str) -> FluidTypeSpec = FluidTypeSpec::new;
const _: fn(&str, &str, &str) -> FluidContactReaction = FluidContactReaction::new;
const _: fn(&mut AirWorld, Pos, u16, bool) -> Result<CustomPlan, Pos> =
    tick_custom_fluid::<AirWorld>;

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

    /// A world a mod loader fills: [`LiquidCell`]s, so a modded fluid keeps its registry id,
    /// and the registry name of whatever sits in a cell, which is what a spec's contact table
    /// is matched against.
    struct ModWorld {
        cells: HashMap<Pos, LiquidCell>,
        names: HashMap<Pos, String>,
        dim: i32,
    }

    impl ModWorld {
        fn new(dim: i32) -> Self {
            Self {
                cells: HashMap::new(),
                names: HashMap::new(),
                dim,
            }
        }

        /// A stone floor with stone above it and open sky over that, the scene every spread
        /// test in this module starts from. A channel has to be opened explicitly, exactly as
        /// the vanilla fixtures do it.
        fn with_floor(radius: i32, dim: i32) -> Self {
            let mut world = Self::new(dim);
            for x in -radius..=radius {
                for z in -radius..=radius {
                    world.put((x, -1, z), LiquidCell::Solid);
                    world.put((x, 0, z), LiquidCell::Solid);
                    world.put((x, 1, z), LiquidCell::Air);
                }
            }
            world
        }

        fn put(&mut self, pos: Pos, cell: LiquidCell) {
            self.cells.insert(pos, cell);
        }

        fn name(&mut self, pos: Pos, name: &str) {
            self.names.insert(pos, name.to_string());
        }

        fn custom(id: u16, level: u8) -> LiquidCell {
            LiquidCell::Fluid {
                liquid: Liquid::Custom(id),
                level,
            }
        }
    }

    impl Reader for ModWorld {
        type Error = Pos;
        fn cell(&mut self, pos: Pos) -> Result<Cell, Pos> {
            Ok(match self.fluid_cell(pos)? {
                LiquidCell::Air => Cell::Air,
                LiquidCell::Solid => Cell::Solid,
                LiquidCell::Passable => Cell::Passable,
                LiquidCell::Fluid {
                    liquid: Liquid::Vanilla(kind),
                    level,
                } => Cell::Fluid(fluid(kind, level)),
                // A modded fluid has no vanilla `Cell`: the custom tick reads `fluid_cell`.
                LiquidCell::Fluid {
                    liquid: Liquid::Custom(_),
                    ..
                } => Cell::Air,
            })
        }
        fn fluid_cell(&mut self, pos: Pos) -> Result<LiquidCell, Pos> {
            Ok(self.cells.get(&pos).copied().unwrap_or(LiquidCell::Air))
        }
        fn name_at(&mut self, pos: Pos) -> Result<Option<String>, Pos> {
            Ok(self.names.get(&pos).cloned())
        }
        fn unsupported_contact(&mut self, pos: Pos) -> Pos {
            pos
        }
        fn dim(&self) -> i32 {
            self.dim
        }
    }

    /// The registry starts parked, so an engine with no loader answers every modded-fluid
    /// lookup from one relaxed load, and the vanilla kinds keep the delays they always had.
    ///
    /// These assertions are off the production functions, not off a restatement of them: a
    /// registration that armed the gate at startup would show up here as an armed registry.
    #[test]
    fn custom_fluid_registry_starts_parked_and_leaves_vanilla_kinds_alone() {
        let _guard = crate::world_guard();
        clear_custom_fluids();
        assert!(
            !custom_fluids().is_armed(),
            "a fresh process registers no modded fluid"
        );
        assert_eq!(custom_fluid_spec(0), None);
        assert_eq!(custom_fluid_by_name("mymod:molten_glass"), None);
        assert_eq!(custom_fluid_tick_delay(0, -1), None);
        assert_eq!(fluid_tick_delay_for_dim(Kind::Water, 0), 5);
        assert_eq!(fluid_tick_delay_for_dim(Kind::Lava, -1), 10);
        assert_eq!(fluid_tick_delay_for_dim(Kind::Lava, 0), 30);
        // Vanilla water still flows through the same entry point while the gate is parked.
        let mut world = ModWorld::with_floor(2, 0);
        world.put((1, 0, 0), LiquidCell::Air);
        world.put(
            (0, 0, 0),
            LiquidCell::Fluid {
                liquid: Liquid::Vanilla(Kind::Water),
                level: 0,
            },
        );
        let plan = tick(&mut world, (0, 0, 0), Kind::Water, true, true).unwrap();
        assert!(plan
            .writes
            .contains(&((1, 0, 0), Cell::Fluid(fluid(Kind::Water, 1)))));
    }

    /// A registered viscous fluid — a 40-tick delay, a 2-level drop-off, and a contact table
    /// that turns it into glass against water — flows through the same machine vanilla water
    /// does, with the physics its spec declares.
    #[test]
    fn a_registered_viscous_fluid_flows_with_its_own_physics_and_contact_table() {
        let _guard = crate::world_guard();
        clear_custom_fluids();
        let mut spec = FluidTypeSpec::new("mymod:molten_glass");
        spec.tick_delay = 40;
        spec.ultrawarm_tick_delay = 15;
        spec.drop_off = 2;
        spec.ultrawarm_drop_off = 1;
        spec.contact_reactions.push(FluidContactReaction::new(
            "minecraft:water",
            "minecraft:glass",
            "minecraft:glass_pane",
        ));
        let id = register_custom_fluid(spec).expect("a valid spec registers");
        assert!(custom_fluids().is_armed());
        assert_eq!(custom_fluid_by_name("mymod:molten_glass"), Some(id));
        let stored = custom_fluid_spec(id).expect("registered");
        assert_eq!(stored.id, "mymod:molten_glass");
        assert_eq!(stored.drop_off_for_dim(0), 2);
        assert_eq!(stored.drop_off_for_dim(-1), 1);
        assert_eq!(custom_fluid_tick_delay(id, 0), Some(40));
        assert_eq!(custom_fluid_tick_delay(id, -1), Some(15));
        // Append-only: the same name keeps the id it already owns.
        assert_eq!(
            register_custom_fluid(FluidTypeSpec::new("mymod:molten_glass")),
            Some(id)
        );

        // A source on a floor spreads at its own drop-off: 8 - 2 = 6, so a neighbour is level
        // 2, where vanilla water writes level 1.
        let channel = [(0, 0, -1), (1, 0, 0), (0, 0, 1), (-1, 0, 0)];
        let mut world = ModWorld::with_floor(4, 0);
        for pos in channel {
            world.put(pos, LiquidCell::Air);
        }
        world.put((0, 0, 0), ModWorld::custom(id, 0));
        let plan = tick_custom_fluid(&mut world, (0, 0, 0), id, true).expect("tick");
        assert_eq!(plan.updated_self, None);
        assert!(
            plan.solidifications.is_empty(),
            "{:?}",
            plan.solidifications
        );
        for pos in channel {
            assert!(
                plan.writes.contains(&(pos, ModWorld::custom(id, 2))),
                "{pos:?} takes the 2-level drop-off: {:?}",
                plan.writes
            );
        }
        // The same scene under vanilla water writes level 1, so the difference is the spec's.
        let mut water = ModWorld::with_floor(4, 0);
        for pos in channel {
            water.put(pos, LiquidCell::Air);
        }
        water.put(
            (0, 0, 0),
            LiquidCell::Fluid {
                liquid: Liquid::Vanilla(Kind::Water),
                level: 0,
            },
        );
        let plan = tick(&mut water, (0, 0, 0), Kind::Water, true, true).unwrap();
        assert!(plan
            .writes
            .contains(&((1, 0, 0), Cell::Fluid(fluid(Kind::Water, 1)))));

        // The contact table, not a hardcoded kind: water above the source solidifies it into
        // the spec's `source_result`.
        let mut contact = ModWorld::with_floor(2, 0);
        contact.put((0, 0, 0), ModWorld::custom(id, 0));
        contact.put(
            (0, 1, 0),
            LiquidCell::Fluid {
                liquid: Liquid::Vanilla(Kind::Water),
                level: 0,
            },
        );
        contact.name((0, 1, 0), "minecraft:water");
        let plan = tick_custom_fluid(&mut contact, (0, 0, 0), id, true).expect("tick");
        assert_eq!(
            plan.solidifications,
            vec![CustomSolidification {
                pos: (0, 0, 0),
                block: "minecraft:glass".to_string(),
                fluid_was_source: true,
            }]
        );
        assert!(plan.writes.is_empty(), "{:?}", plan.writes);

        // A flowing cell of the same fluid uses the flowing result.
        let mut flowing = ModWorld::with_floor(2, 0);
        flowing.put((0, 0, 0), ModWorld::custom(id, 3));
        flowing.put(
            (1, 0, 0),
            LiquidCell::Fluid {
                liquid: Liquid::Vanilla(Kind::Water),
                level: 0,
            },
        );
        flowing.name((1, 0, 0), "minecraft:water");
        let plan = tick_custom_fluid(&mut flowing, (0, 0, 0), id, false).expect("tick");
        assert_eq!(
            plan.solidifications,
            vec![CustomSolidification {
                pos: (0, 0, 0),
                block: "minecraft:glass_pane".to_string(),
                fluid_was_source: false,
            }]
        );

        // An id nothing registered flows nowhere, and the registered one still does.
        let mut unregistered = ModWorld::with_floor(2, 0);
        unregistered.put((0, 0, 0), ModWorld::custom(id, 0));
        assert_eq!(
            tick_custom_fluid(&mut unregistered, (0, 0, 0), id + 1, true).expect("tick"),
            CustomPlan::default()
        );

        clear_custom_fluids();
        assert!(!custom_fluids().is_armed());
    }

    /// Registration refuses what the machine could not run — a name that is not a resource
    /// location, a reaction naming one — and clamps the numeric fields into the ranges
    /// `FlowingFluid` defines rather than trusting a loader's spelling of them.
    #[test]
    fn custom_fluid_registration_clamps_ranges_and_refuses_unusable_names() {
        let _guard = crate::world_guard();
        clear_custom_fluids();
        assert_eq!(
            register_custom_fluid(FluidTypeSpec::new("Not A Fluid")),
            None
        );
        let mut unclamped = FluidTypeSpec::new("mymod:brine");
        unclamped.drop_off = 0;
        unclamped.ultrawarm_drop_off = 200;
        unclamped.slope_distance = 0;
        unclamped.ultrawarm_slope_distance = 9;
        unclamped.tick_delay = 0;
        let id = register_custom_fluid(unclamped).expect("registered");
        let stored = custom_fluid_spec(id).expect("registered");
        assert_eq!(stored.drop_off_for_dim(0), 1);
        assert_eq!(stored.drop_off_for_dim(-1), 7);
        assert_eq!(stored.slope_distance_for_dim(0), 1);
        assert_eq!(stored.slope_distance_for_dim(-1), 4);
        assert_eq!(custom_fluid_tick_delay(id, 0), Some(1));
        // A reaction has to name something `Reader::name_at` could answer.
        let mut bad_rule = FluidTypeSpec::new("mymod:sludge");
        bad_rule.contact_reactions.push(FluidContactReaction::new(
            "Not A Fluid",
            "minecraft:stone",
            "minecraft:cobblestone",
        ));
        assert_eq!(register_custom_fluid(bad_rule), None);
        assert_eq!(custom_fluid_by_name("mymod:sludge"), None);
        // Names are canonicalised, so a loader's stray whitespace is the same registration.
        assert_eq!(custom_fluid_by_name("  mymod:brine  "), Some(id));
        clear_custom_fluids();
        assert!(!custom_fluids().is_armed());
    }
}
