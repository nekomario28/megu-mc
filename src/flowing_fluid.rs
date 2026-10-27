//! FlowingFluid's scheduled update for full-cube, air and liquid cells.
//!
//! Non-full collision shapes and cross-fluid contact require additional native
//! shape/contact contracts; callers receive an error before committing writes.
use std::collections::HashMap;

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
    fn drop_off(self) -> u8 {
        if self.kind == Kind::Water {
            1
        } else {
            2
        }
    }
    fn slope_limit(self) -> u8 {
        if self.kind == Kind::Water {
            4
        } else {
            2
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cell {
    Air,
    Solid,
    Fluid(Fluid),
}

pub(crate) trait Reader {
    type Error;
    fn cell(&mut self, pos: Pos) -> Result<Cell, Self::Error>;
    fn unsupported_contact(&mut self, pos: Pos) -> Self::Error;
}

pub(crate) struct Plan {
    pub writes: Vec<(Pos, Cell)>,
    pub updated_self: Option<Fluid>,
}

const HORIZONTAL: [Pos; 4] = [(0, 0, -1), (1, 0, 0), (0, 0, 1), (-1, 0, 0)];
fn add(p: Pos, d: Pos) -> Pos {
    (p.0 + d.0, p.1 + d.1, p.2 + d.2)
}
fn opposite(d: Pos) -> Pos {
    (-d.0, -d.1, -d.2)
}

struct Context<'a, W> {
    world: &'a mut W,
    overlay: HashMap<Pos, Cell>,
    writes: Vec<(Pos, Cell)>,
    kind: Kind,
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
        let amount = maximum.saturating_sub(if self.kind == Kind::Water { 1 } else { 2 });
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
            Cell::Fluid(other) if other.kind != fluid.kind => {
                Err(self.world.unsupported_contact(p))
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
            fluid.amount().saturating_sub(fluid.drop_off())
        };
        if strength == 0 {
            return Ok(());
        }
        let mut best = 1000;
        let mut choices = Vec::new();
        for (i, d) in HORIZONTAL.into_iter().enumerate() {
            let q = add(p, d);
            let next = self.new_liquid(q, source_conversion)?;
            if !self.can_pass(q)? {
                continue;
            }
            let distance = if self.hole(q)? {
                0
            } else {
                self.slope(q, 1, opposite(d), fluid.slope_limit())?
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
    let mut cx = Context {
        world,
        overlay: HashMap::new(),
        writes: Vec::new(),
        kind,
    };
    let cell = cx.get(p)?;
    let Some(mut current) = cx.same(cell).filter(|f| f.source() == expected_source) else {
        return Ok(Plan {
            writes: cx.writes,
            updated_self: None,
        });
    };
    let mut updated_self = None;
    if !current.source() {
        match cx.new_liquid(p, source_conversion)? {
            None => {
                cx.put(p, Cell::Air)?;
                return Ok(Plan {
                    writes: cx.writes,
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
    if matches!(below_state, Cell::Air) {
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
        updated_self,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn cell(s: &str) -> Option<Cell> {
        match s {
            "A" => Some(Cell::Air),
            "S" => Some(Cell::Solid),
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
}
