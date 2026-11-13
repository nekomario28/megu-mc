//! Procedural NORMAL mineshaft layout after candidate/biome admission.
//! No saved-piece injection or block writes: admission, Mesa terrain adjustment,
//! and decoration dispatch are separate contracts. Body/ordering: pinned elw/elv.
use crate::generation_math::Lcg;

type Bounds = [i32; 6];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Facing {
    North,
    South,
    West,
    East,
}

impl Facing {
    fn along_z(self) -> bool {
        matches!(self, Self::North | Self::South)
    }
    fn data2d(self) -> i32 {
        match self {
            Self::South => 0,
            Self::West => 1,
            Self::North => 2,
            Self::East => 3,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Room {
        entrances: Vec<Bounds>,
    },
    Corridor {
        rails: bool,
        spider: bool,
        spider_placed: bool,
        sections: i32,
    },
    Crossing {
        direction: Facing,
        two_floors: bool,
    },
    Stairs,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Piece {
    pub(crate) bounds: Bounds,
    pub(crate) depth: i32,
    pub(crate) orientation: Option<Facing>,
    pub(crate) kind: Kind,
}

pub(crate) struct Layout {
    pub(crate) pieces: Vec<Piece>,
    /// Context, discarded double, room, children, height adjustment: no draws.
    pub(crate) checkpoints: [u64; 5],
    pub(crate) vertical_shift: i32,
}

fn add_i32(x: i32, y: i32) -> i32 {
    x.wrapping_add(y)
}
fn translated(mut b: Bounds, x: i32, y: i32, z: i32) -> Bounds {
    for i in 0..6 {
        b[i] = add_i32(b[i], [x, y, z][i % 3]);
    }
    b
}
fn intersects(x: &Bounds, y: &Bounds) -> bool {
    (0..3).all(|i| x[i + 3] >= y[i] && x[i] <= y[i + 3])
}

struct Builder {
    pieces: Vec<Piece>,
    random: Lcg,
}

impl Builder {
    fn collision(&self, b: &Bounds) -> bool {
        self.pieces.iter().any(|p| intersects(&p.bounds, b))
    }

    fn add(
        &mut self,
        x: i32,
        y: i32,
        z: i32,
        direction: Facing,
        parent_depth: i32,
    ) -> Option<usize> {
        let root = self.pieces[0].bounds;
        if parent_depth > 8
            || x.wrapping_sub(root[0]).wrapping_abs() > 80
            || z.wrapping_sub(root[2]).wrapping_abs() > 80
        {
            return None;
        }
        let depth = parent_depth + 1;
        let choice = self.random.next_bound(100);
        let (bounds, kind, orientation) = if choice >= 80 {
            let h = if self.random.next_bound(4) == 0 { 6 } else { 2 };
            let local = match direction {
                Facing::North => [-1, 0, -4, 3, h, 0],
                Facing::South => [-1, 0, 0, 3, h, 4],
                Facing::West => [-4, 0, -1, 0, h, 3],
                Facing::East => [0, 0, -1, 4, h, 3],
            };
            let b = translated(local, x, y, z);
            if self.collision(&b) {
                return None;
            }
            (
                b,
                Kind::Crossing {
                    direction,
                    two_floors: h > 2,
                },
                None,
            )
        } else if choice >= 70 {
            let local = match direction {
                Facing::North => [0, -5, -8, 2, 2, 0],
                Facing::South => [0, -5, 0, 2, 2, 8],
                Facing::West => [-8, -5, 0, 0, 2, 2],
                Facing::East => [0, -5, 0, 8, 2, 2],
            };
            let b = translated(local, x, y, z);
            if self.collision(&b) {
                return None;
            }
            (b, Kind::Stairs, Some(direction))
        } else {
            let mut sections = self.random.next_bound(3) + 2;
            let bounds = loop {
                if sections == 0 {
                    return None;
                }
                let end = sections * 5 - 1;
                let local = match direction {
                    Facing::North => [0, 0, -end, 2, 2, 0],
                    Facing::South => [0, 0, 0, 2, 2, end],
                    Facing::West => [-end, 0, 0, 0, 2, 2],
                    Facing::East => [0, 0, 0, end, 2, 2],
                };
                let b = translated(local, x, y, z);
                if !self.collision(&b) {
                    break b;
                }
                sections -= 1;
            };
            let rails = self.random.next_bound(3) == 0;
            // Short-circuiting owns a draw: rails corridors do not roll spiders.
            let spider = !rails && self.random.next_bound(23) == 0;
            (
                bounds,
                Kind::Corridor {
                    rails,
                    spider,
                    spider_placed: false,
                    sections,
                },
                Some(direction),
            )
        };
        let index = self.pieces.len();
        self.pieces.push(Piece {
            bounds,
            depth,
            orientation,
            kind,
        });
        // Append before recursively expanding, preserving native depth-first order.
        self.grow(index);
        Some(index)
    }

    fn grow(&mut self, index: usize) {
        let p = self.pieces[index].clone();
        let b = p.bounds;
        let d = p.depth;
        match p.kind {
            Kind::Room { .. } => {
                let height_bound = (b[4] - b[1] - 3).max(1);
                for dir in [Facing::North, Facing::South, Facing::West, Facing::East] {
                    let span = if dir.along_z() {
                        b[3] - b[0] + 1
                    } else {
                        b[5] - b[2] + 1
                    };
                    let mut offset = 0;
                    while offset < span {
                        offset += self.random.next_bound(span);
                        if offset + 3 > span {
                            break;
                        }
                        let y = add_i32(b[1], self.random.next_bound(height_bound) + 1);
                        let (x, z) = match dir {
                            Facing::North => (add_i32(b[0], offset), add_i32(b[2], -1)),
                            Facing::South => (add_i32(b[0], offset), add_i32(b[5], 1)),
                            Facing::West => (add_i32(b[0], -1), add_i32(b[2], offset)),
                            Facing::East => (add_i32(b[3], 1), add_i32(b[2], offset)),
                        };
                        if let Some(child) = self.add(x, y, z, dir, d) {
                            let c = self.pieces[child].bounds;
                            let entrance = match dir {
                                Facing::North => [c[0], c[1], b[2], c[3], c[4], add_i32(b[2], 1)],
                                Facing::South => [c[0], c[1], add_i32(b[5], -1), c[3], c[4], b[5]],
                                Facing::West => [b[0], c[1], c[2], add_i32(b[0], 1), c[4], c[5]],
                                Facing::East => [add_i32(b[3], -1), c[1], c[2], b[3], c[4], c[5]],
                            };
                            if let Kind::Room { entrances } = &mut self.pieces[index].kind {
                                entrances.push(entrance);
                            }
                        }
                        offset += 4;
                    }
                }
            }
            Kind::Stairs => {
                let dir = p.orientation.expect("stairs orientation");
                let (x, z) = match dir {
                    Facing::North => (b[0], add_i32(b[2], -1)),
                    Facing::South => (b[0], add_i32(b[5], 1)),
                    Facing::West => (add_i32(b[0], -1), b[2]),
                    Facing::East => (add_i32(b[3], 1), b[2]),
                };
                self.add(x, b[1], z, dir, d);
            }
            Kind::Crossing {
                direction,
                two_floors,
            } => {
                let first = match direction {
                    Facing::North => [Facing::North, Facing::West, Facing::East],
                    Facing::South => [Facing::South, Facing::West, Facing::East],
                    Facing::West => [Facing::North, Facing::South, Facing::West],
                    Facing::East => [Facing::North, Facing::South, Facing::East],
                };
                for dir in first {
                    self.cross_child(b, b[1], dir, d);
                }
                if two_floors {
                    // Draw and recurse one at a time; pre-drawing four booleans shifts RNG.
                    for dir in [Facing::North, Facing::West, Facing::East, Facing::South] {
                        if self.random.next_bool() {
                            self.cross_child(b, add_i32(b[1], 4), dir, d);
                        }
                    }
                }
            }
            Kind::Corridor { .. } => {
                let dir = p.orientation.expect("corridor orientation");
                let choice = self.random.next_bound(4);
                let (x, z, next) = match (dir, choice) {
                    (Facing::North, 0 | 1) => (b[0], add_i32(b[2], -1), dir),
                    (Facing::North, 2) => (add_i32(b[0], -1), b[2], Facing::West),
                    (Facing::North, _) => (add_i32(b[3], 1), b[2], Facing::East),
                    (Facing::South, 0 | 1) => (b[0], add_i32(b[5], 1), dir),
                    (Facing::South, 2) => (add_i32(b[0], -1), add_i32(b[5], -3), Facing::West),
                    (Facing::South, _) => (add_i32(b[3], 1), add_i32(b[5], -3), Facing::East),
                    (Facing::West, 0 | 1) => (add_i32(b[0], -1), b[2], dir),
                    (Facing::West, 2) => (b[0], add_i32(b[2], -1), Facing::North),
                    (Facing::West, _) => (b[0], add_i32(b[5], 1), Facing::South),
                    (Facing::East, 0 | 1) => (add_i32(b[3], 1), b[2], dir),
                    (Facing::East, 2) => (add_i32(b[3], -3), add_i32(b[2], -1), Facing::North),
                    (Facing::East, _) => (add_i32(b[3], -3), add_i32(b[5], 1), Facing::South),
                };
                let y = add_i32(b[1], self.random.next_bound(3) - 1);
                self.add(x, y, z, next, d);
                if d < 8 {
                    let (start, end) = if dir.along_z() {
                        (b[2], b[5])
                    } else {
                        (b[0], b[3])
                    };
                    let mut at = add_i32(start, 3);
                    while add_i32(at, 3) <= end {
                        let branch = self.random.next_bound(5);
                        if dir.along_z() {
                            if branch == 0 {
                                self.add(add_i32(b[0], -1), b[1], at, Facing::West, d + 1);
                            } else if branch == 1 {
                                self.add(add_i32(b[3], 1), b[1], at, Facing::East, d + 1);
                            }
                        } else if branch == 0 {
                            self.add(at, b[1], add_i32(b[2], -1), Facing::North, d + 1);
                        } else if branch == 1 {
                            self.add(at, b[1], add_i32(b[5], 1), Facing::South, d + 1);
                        }
                        at = add_i32(at, 5);
                    }
                }
            }
        }
    }

    fn cross_child(&mut self, b: Bounds, y: i32, dir: Facing, depth: i32) {
        let (x, z) = match dir {
            Facing::North => (add_i32(b[0], 1), add_i32(b[2], -1)),
            Facing::South => (add_i32(b[0], 1), add_i32(b[5], 1)),
            Facing::West => (add_i32(b[0], -1), add_i32(b[2], 1)),
            Facing::East => (add_i32(b[3], 1), add_i32(b[2], 1)),
        };
        self.add(x, y, z, dir, depth);
    }
}

/// Native findGenerationPoint / generatePiecesAndAdjust, NORMAL branch only.
/// Candidate admission is the caller's responsibility; this always builds a layout.
pub(crate) fn normal_layout(seed: u64, cx: i32, cz: i32, sea_level: i32, min_y: i32) -> Layout {
    let mut random = Lcg::for_large_feature(seed, cx, cz);
    let context = random.state48();
    // elw.findGenerationPoint discards this nextDouble before constructing the room.
    random.next_f64();
    let before_room = random.state48();
    let x = add_i32(cx.wrapping_shl(4), 2);
    let z = add_i32(cz.wrapping_shl(4), 2);
    let bounds = [
        x,
        50,
        z,
        add_i32(x, 7 + random.next_bound(6)),
        54 + random.next_bound(6),
        add_i32(z, 7 + random.next_bound(6)),
    ];
    let after_room = random.state48();
    let mut builder = Builder {
        random,
        pieces: vec![Piece {
            bounds,
            depth: 0,
            orientation: None,
            kind: Kind::Room {
                entrances: Vec::new(),
            },
        }],
    };
    builder.grow(0);
    let after_children = builder.random.state48();
    let low = builder.pieces.iter().map(|p| p.bounds[1]).min().unwrap();
    let high = builder.pieces.iter().map(|p| p.bounds[4]).max().unwrap();
    let ceiling = sea_level - 10;
    let mut target = high - low + 1 + min_y + 1;
    if target < ceiling {
        target += builder.random.next_bound(ceiling - target);
    }
    let vertical_shift = target - high;
    for p in &mut builder.pieces {
        p.bounds = translated(p.bounds, 0, vertical_shift, 0);
        if let Kind::Room { entrances } = &mut p.kind {
            for entrance in entrances {
                *entrance = translated(*entrance, 0, vertical_shift, 0);
            }
        }
    }
    Layout {
        pieces: builder.pieces,
        vertical_shift,
        checkpoints: [
            context,
            before_room,
            after_room,
            after_children,
            builder.random.state48(),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn csv(b: &Bounds) -> String {
        b.iter().map(i32::to_string).collect::<Vec<_>>().join(",")
    }
    fn piece_line(p: &Piece) -> String {
        let (name, data) = match &p.kind {
            Kind::Room { entrances } => (
                "msroom",
                entrances.iter().map(csv).collect::<Vec<_>>().join(";"),
            ),
            Kind::Corridor {
                rails,
                spider,
                spider_placed,
                sections,
            } => (
                "mscorridor",
                format!(
                    "{},{},{},{}",
                    *rails as u8, *spider as u8, *spider_placed as u8, sections
                ),
            ),
            Kind::Crossing {
                direction,
                two_floors,
            } => (
                "mscrossing",
                format!("{},{}", direction.data2d(), *two_floors as u8),
            ),
            Kind::Stairs => ("msstairs", String::new()),
        };
        let payload = if data.is_empty() { "-" } else { data.as_str() };
        format!(
            "P\t{name}\t{}\t{}\t{}\t{payload}",
            p.depth,
            p.orientation.map_or(-1, Facing::data2d),
            csv(&p.bounds)
        )
    }
    #[test]
    fn normal_layout_matches_native_pieces_and_rng_boundaries() {
        let mut lines = include_str!("../tests/data/mineshaft_layout_1211.tsv").lines();
        let mut cases = 0;
        let mut pieces = 0;
        while let Some(line) = lines.next() {
            let f: Vec<_> = line.split('\t').collect();
            assert_eq!(f[0], "C");
            assert_eq!(f.len(), 11);
            let result = normal_layout(
                f[1].parse::<i64>().unwrap() as u64,
                f[2].parse().unwrap(),
                f[3].parse().unwrap(),
                63,
                -64,
            );
            assert_eq!(
                result.vertical_shift,
                f[4].parse::<i32>().unwrap(),
                "shift: {line}"
            );
            for (i, actual) in result.checkpoints.iter().enumerate() {
                assert_eq!(
                    *actual,
                    f[5 + i].parse::<u64>().unwrap(),
                    "RNG boundary {i}: {line}"
                );
            }
            assert_eq!(
                result.pieces.len(),
                f[10].parse::<usize>().unwrap(),
                "count: {line}"
            );
            for (i, p) in result.pieces.iter().enumerate() {
                assert_eq!(
                    piece_line(p),
                    lines.next().expect("native piece missing"),
                    "piece {i}: {line}"
                );
                pieces += 1;
            }
            cases += 1;
        }
        assert_eq!((cases, pieces), (6, 904));
    }
}
