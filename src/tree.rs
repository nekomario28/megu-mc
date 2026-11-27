// tree.rs — vanilla `tree` configured feature: trunk placers, foliage placers,
// feature-size clipping, decorators, mangrove root placer, and leaf-distance
// propagation.

/// One bee occupant generated with a beehive. The timers are feature-RNG state;
/// the live block-entity layer drains them when materializing the nest.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct GeneratedBeeOccupant {
    pub ticks_in_hive: i32,
    pub min_ticks_in_hive: i32,
}

/// A bee nest staged by tree decoration plus its vanilla occupant metadata.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct GeneratedBeehive {
    pub pos: (i32, i32, i32),
    pub occupants: Vec<GeneratedBeeOccupant>,
}

static GENERATED_BEEHIVES: std::sync::OnceLock<std::sync::Mutex<Vec<GeneratedBeehive>>> =
    std::sync::OnceLock::new();

fn stage_generated_beehive(record: GeneratedBeehive) {
    GENERATED_BEEHIVES
        .get_or_init(|| std::sync::Mutex::new(Vec::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(record);
}

/// Drain generated beehive occupant records exactly once.
#[allow(dead_code)]
pub(crate) fn drain_generated_beehives() -> Vec<GeneratedBeehive> {
    let mut out = GENERATED_BEEHIVES
        .get_or_init(|| std::sync::Mutex::new(Vec::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    std::mem::take(&mut *out)
}

use crate::block_registry::BReg;
use crate::generation_leaf_queue::LeafQueue;
use crate::generation_math::Xor;
use crate::json::J;

pub(crate) type P3 = (i32, i32, i32);

// ---------- helpers ----------

const BLOCK_BITSET_WORDS: usize = 24;

struct TreeTables {
    replaceable_by_trees: [u64; BLOCK_BITSET_WORDS],
    logs: [u64; BLOCK_BITSET_WORDS],
    leaves: [u64; BLOCK_BITSET_WORDS],
    replaceable_or_logs: [u64; BLOCK_BITSET_WORDS],
    vine_block_id: i32,
    air_state: u32,
    vine_east: u32,
    vine_west: u32,
    vine_south: u32,
    vine_north: u32,
    bee_nest_south: u32,
    /// `distance_from_log(st)`: 0..=7 for `Some(d)`, 255 for `None`.
    dist_of_state: Box<[u8]>,
    /// Index into `leaf_dist_states` for `with_prop(st, "distance", 1..=6)`, or `u16::MAX`.
    dist_slot: Box<[u16]>,
    dist_states: Vec<[u32; 7]>,
    /// Pre-resolved `schedule_leaf_edge_state` inputs: `0` means not a `*_leaves` with `distance`.
    edge_leaf_dist: Box<[u8]>,
    edge_leaf_default: Box<[u32]>,
}

fn fill_tag_bitset(reg: &BReg, tag: &str) -> [u64; BLOCK_BITSET_WORDS] {
    let mut bits = [0u64; BLOCK_BITSET_WORDS];
    for &bi in reg.tag_members(tag) {
        let idx = (bi as usize) >> 6;
        if idx < BLOCK_BITSET_WORDS {
            bits[idx] |= 1u64 << (bi & 63);
        }
    }
    bits
}

#[inline]
fn bitset_has(bits: &[u64; BLOCK_BITSET_WORDS], bi: i32) -> bool {
    let u = bi as usize;
    let w = u >> 6;
    w < BLOCK_BITSET_WORDS && (bits[w] & (1u64 << (u & 63))) != 0
}

impl TreeTables {
    fn build(reg: &BReg) -> Self {
        let replaceable_by_trees = fill_tag_bitset(reg, "minecraft:replaceable_by_trees");
        let logs = fill_tag_bitset(reg, "minecraft:logs");
        let leaves = fill_tag_bitset(reg, "minecraft:leaves");
        let mut replaceable_or_logs = [0u64; BLOCK_BITSET_WORDS];
        for i in 0..BLOCK_BITSET_WORDS {
            replaceable_or_logs[i] = replaceable_by_trees[i] | logs[i];
        }
        let vine_base = reg.state_of("minecraft:vine", None);
        let vine_block_id = reg.block_id_of(vine_base).unwrap_or(-1);
        let air_state = reg.state_of("minecraft:air", None);
        let vine_east = reg.with_prop(vine_base, "east", "true");
        let vine_west = reg.with_prop(vine_base, "west", "true");
        let vine_south = reg.with_prop(vine_base, "south", "true");
        let vine_north = reg.with_prop(vine_base, "north", "true");
        let bee_nest_south =
            reg.state_of("minecraft:bee_nest", Some(&mkprops(&[("facing", "south")])));

        let mut state_count = 0u32;
        while reg.block_id_of(state_count).is_some() {
            state_count += 1;
        }
        let n = state_count as usize;
        let mut dist_of_state = vec![255u8; n];
        let mut dist_slot = vec![u16::MAX; n];
        let mut dist_states: Vec<[u32; 7]> = Vec::new();
        let mut edge_leaf_dist = vec![0u8; n];
        let mut edge_leaf_default = vec![0u32; n];
        const NUMS: [&str; 7] = ["1", "2", "3", "4", "5", "6", "7"];
        for st in 0..state_count {
            let si = st as usize;
            if let Some(d) = reg.distance_from_log(st) {
                dist_of_state[si] = d;
            }
            if let Some(d_str) = reg.get_prop(st, "distance") {
                if dist_states.len() < u16::MAX as usize {
                    let slot = dist_states.len() as u16;
                    let mut row = [st; 7];
                    for (k, val) in NUMS.iter().enumerate() {
                        row[k] = reg.with_prop(st, "distance", val);
                    }
                    dist_states.push(row);
                    dist_slot[si] = slot;
                }
                let name = reg.name_of(st);
                if name.ends_with("_leaves") {
                    if let Ok(d) = d_str.parse::<u8>() {
                        edge_leaf_dist[si] = d;
                        edge_leaf_default[si] = reg.state_of(name, None);
                    }
                }
            }
        }
        Self {
            replaceable_by_trees,
            logs,
            leaves,
            replaceable_or_logs,
            vine_block_id,
            air_state,
            vine_east,
            vine_west,
            vine_south,
            vine_north,
            bee_nest_south,
            dist_of_state: dist_of_state.into_boxed_slice(),
            dist_slot: dist_slot.into_boxed_slice(),
            dist_states,
            edge_leaf_dist: edge_leaf_dist.into_boxed_slice(),
            edge_leaf_default: edge_leaf_default.into_boxed_slice(),
        }
    }

    #[inline]
    fn in_bitset(&self, bits: &[u64; BLOCK_BITSET_WORDS], reg: &BReg, st: u32, tag: &str) -> bool {
        let Some(bi) = reg.block_id_of(st) else {
            return false;
        };
        if (bi as usize) < BLOCK_BITSET_WORDS * 64 {
            bitset_has(bits, bi)
        } else {
            reg.in_tag(st, tag)
        }
    }

    #[inline]
    fn is_replaceable_by_trees(&self, reg: &BReg, cur: u32) -> bool {
        reg.is_air(cur)
            || self.in_bitset(
                &self.replaceable_by_trees,
                reg,
                cur,
                "minecraft:replaceable_by_trees",
            )
    }

    #[inline]
    fn is_replaceable_or_log(&self, reg: &BReg, cur: u32) -> bool {
        if reg.is_air(cur) {
            return true;
        }
        let Some(bi) = reg.block_id_of(cur) else {
            return false;
        };
        if (bi as usize) < BLOCK_BITSET_WORDS * 64 {
            bitset_has(&self.replaceable_or_logs, bi)
        } else {
            reg.in_tag(cur, "minecraft:replaceable_by_trees") || reg.in_tag(cur, "minecraft:logs")
        }
    }

    #[inline]
    fn is_log(&self, reg: &BReg, cur: u32) -> bool {
        self.in_bitset(&self.logs, reg, cur, "minecraft:logs")
    }

    #[inline]
    fn is_leaves(&self, reg: &BReg, cur: u32) -> bool {
        self.in_bitset(&self.leaves, reg, cur, "minecraft:leaves")
    }

    #[inline]
    fn is_vine(&self, reg: &BReg, cur: u32) -> bool {
        reg.block_id_of(cur) == Some(self.vine_block_id)
    }

    #[inline]
    fn distance_from_log(&self, reg: &BReg, st: u32) -> Option<u8> {
        match self.dist_of_state.get(st as usize) {
            Some(&255) => None,
            Some(&d) => Some(d),
            None => reg.distance_from_log(st),
        }
    }

    #[inline]
    fn with_distance(&self, reg: &BReg, st: u32, depth: usize) -> u32 {
        if (1..=7).contains(&depth) {
            if let Some(&slot) = self.dist_slot.get(st as usize) {
                if slot != u16::MAX {
                    return self.dist_states[slot as usize][depth - 1];
                }
                return st;
            }
        }
        const NUMS: [&str; 7] = ["0", "1", "2", "3", "4", "5", "6"];
        let s = NUMS.get(depth).copied().unwrap_or("7");
        reg.with_prop(st, "distance", s)
    }
}

fn tree_tables(reg: &BReg) -> &'static TreeTables {
    static TABLES: std::sync::OnceLock<TreeTables> = std::sync::OnceLock::new();
    TABLES.get_or_init(|| TreeTables::build(reg))
}

/// Vanilla placers all ask the same question - may this position be written? - and
/// answer it as air-or-in-tag. Which tag differs per placer, so it is the argument
/// rather than three copies of the test with a different tag baked into each. That
/// is how a placer ends up overwriting something it should not.
#[cfg(all(test, megu_vanilla_data))]
fn is_replaceable(reg: &BReg, cur: u32, tags: &[&str]) -> bool {
    reg.is_air(cur) || tags.iter().any(|t| reg.in_tag(cur, t))
}

fn set_if_can_replace(ctx: &mut FC, p: P3, st: u32) -> bool {
    let reg = match breg() {
        Some(r) => r,
        None => return false,
    };
    let cur = ctx.get(p.0, p.1, p.2);
    if tree_tables(reg).is_replaceable_by_trees(reg, cur) {
        ctx.set(p.0, p.1, p.2, st);
        return true;
    }
    false
}
fn set_if_can_replace_or_log(ctx: &mut FC, p: P3, st: u32) -> bool {
    let reg = match breg() {
        Some(r) => r,
        None => return false,
    };
    let cur = ctx.get(p.0, p.1, p.2);
    if tree_tables(reg).is_replaceable_or_log(reg, cur) {
        ctx.set(p.0, p.1, p.2, st);
        return true;
    }
    false
}
fn is_air_or_leaves(ctx: &mut FC, p: P3) -> bool {
    match breg() {
        Some(reg) => {
            let cur = ctx.get(p.0, p.1, p.2);
            reg.is_air(cur) || tree_tables(reg).is_leaves(reg, cur)
        }
        None => false,
    }
}
pub(crate) fn set_dirt(
    ctx: &mut FC,
    rng: &mut Xor,
    p: P3,
    provider: &Option<Box<Bsp>>,
    force: bool,
    logs: &mut Vec<P3>,
) {
    let Some(bsp) = provider else { return };
    match breg() {
        Some(reg) if !force && tree_tables(reg).is_log(reg, ctx.get(p.0, p.1, p.2)) => return,
        Some(_) => {}
        None => return,
    }
    let st = bsp.get(ctx, rng, p.0, p.1, p.2);
    ctx.set(p.0, p.1, p.2, st);
    logs.push(p);
}
fn set_axis(ctx: &mut FC, p: P3, base: u32, axis: &str) -> bool {
    if let Some(reg) = breg() {
        let st = reg.with_prop(base, "axis", axis);
        set_if_can_replace(ctx, p, st)
    } else {
        set_if_can_replace(ctx, p, base)
    }
}
fn rand_horizontal(rng: &mut Xor) -> u8 {
    horizontal_dir(rng.next_bound(4))
}
/// `Direction.Plane.HORIZONTAL.getRandomDirection(random)` (`ji$c.a(ayw)`):
///
/// ```
/// getstatic ji$c.a ; invokevirtual ji$c.a:(Layw;)Lji;     // getRandomDirection
///   -> getfield ji$c.c:[Lji;                            // the plane's faces array
///   -> Util.getRandom(T[], RandomSource) = faces[nextInt(faces.length)]
///
/// ji$c.<clinit>
///   HORIZONTAL faces = [ji.c, ji.f, ji.d, ji.e]
///   and ji's enum order is a=DOWN b=UP c=NORTH d=SOUTH e=WEST f=EAST
///   => [NORTH, EAST, SOUTH, WEST]
/// ```
///
/// The same array is what `for (Direction d : Direction.Plane.HORIZONTAL)` walks, so
/// this order is both the draw binding and the iteration order. It is the *forward*
/// array; indexing it backwards (south, west, east, north) mirrors the lean direction of
/// `DarkOakTrunkPlacer`, `ForkingTrunkPlacer`, `BendingTrunkPlacer` and
/// `UpwardsBranchingTrunkPlacer`, and the branch direction of `CherryTrunkPlacer`.
/// (`FancyTrunkPlacer` does not touch the plane.)
pub(crate) fn horizontal_dir(i: i32) -> u8 {
    match i {
        0 => 2, // north
        1 => 5, // east
        2 => 3, // south
        _ => 4, // west
    }
}

/// `Direction.Plane.HORIZONTAL`'s faces array in iteration order. Every walk over the plane
/// takes its order from here, so the array order has one owner.
fn plane_horizontal() -> [u8; 4] {
    [
        horizontal_dir(0),
        horizontal_dir(1),
        horizontal_dir(2),
        horizontal_dir(3),
    ]
}
pub(crate) fn hoff(d: u8) -> (i32, i32) {
    match d {
        2 => (0, -1),
        3 => (0, 1),
        4 => (-1, 0),
        _ => (1, 0),
    }
}

pub(crate) struct TreeNode {
    pub center: P3,
    pub foliage_radius: i32,
    pub giant: bool,
}

// ---------- trunk placers ----------

pub(crate) enum TP {
    Straight,
    Forking,
    Giant,
    MegaJungle,
    DarkOak,
    Fancy,
    Bending {
        min_height_for_leaves: i32,
        bend_length: IP,
    },
    UpwardsBranching {
        extra_steps: IP,
        branch_prob: f32,
        extra_len: IP,
        grow_through: Vec<String>,
    },
    Cherry {
        count: IP,
        horizontal: IP,
        start_off: IP,
        end_off: IP,
    },
    Poplar {
        above_branches: IP,
        branch_amount: IP,
    },
}
pub(crate) struct TrunkPl {
    pub base: i32,
    pub a: i32,
    pub b: i32,
    pub ty: TP,
}
impl TrunkPl {
    pub fn height(&self, rng: &mut Xor) -> i32 {
        self.base + rng.next_bound(self.a + 1) + rng.next_bound(self.b + 1)
    }
    // the parameters are one trunk's own inputs, in the order the generator reads them
    #[expect(clippy::too_many_arguments)]
    pub fn generate(
        &self,
        ctx: &mut FC,
        rng: &mut Xor,
        height: i32,
        start: P3,
        dirt_provider: &Option<Box<Bsp>>,
        trunk_st: u32,
        extra: &TreeExtra,
    ) -> (Vec<TreeNode>, Vec<P3>) {
        let mut nodes = vec![];
        let mut logs = vec![];
        match &self.ty {
            TP::Straight => {
                set_dirt(
                    ctx,
                    rng,
                    (start.0, start.1 - 1, start.2),
                    dirt_provider,
                    extra.force_dirt,
                    &mut logs,
                );
                for i in 0..height {
                    let p = (start.0, start.1 + i, start.2);
                    if set_if_can_replace(ctx, p, trunk_st) {
                        logs.push(p);
                    }
                }
                nodes.push(TreeNode {
                    center: (start.0, start.1 + height, start.2),
                    foliage_radius: 0,
                    giant: false,
                });
            }
            TP::Giant | TP::MegaJungle => {
                let below = (start.0, start.1 - 1, start.2);
                for dp in [
                    below,
                    (below.0 + 1, below.1, below.2),
                    (below.0, below.1, below.2 + 1),
                    (below.0 + 1, below.1, below.2 + 1),
                ] {
                    set_dirt(ctx, rng, dp, dirt_provider, extra.force_dirt, &mut logs);
                }
                for y in 0..height {
                    for (dx, dz) in [(0, 0), (1, 0), (1, 1), (0, 1)] {
                        if (dx, dz) != (0, 0) && y >= height - 1 {
                            continue;
                        }
                        let p = (start.0 + dx, start.1 + y, start.2 + dz);
                        if set_if_can_replace_or_log(ctx, p, trunk_st) {
                            logs.push(p);
                        }
                    }
                }
                nodes.push(TreeNode {
                    center: (start.0, start.1 + height, start.2),
                    foliage_radius: 0,
                    giant: true,
                });
                if matches!(self.ty, TP::MegaJungle) {
                    let mut i = height - 2 - rng.next_bound(4);
                    while i > height / 2 {
                        let f = rng.next_f32() * std::f32::consts::PI * 2.0;
                        let (mut jv, mut kv) = (0i32, 0i32);
                        for l in 0..5i32 {
                            jv = (1.5f32 + f.cos() * l as f32) as i32;
                            kv = (1.5f32 + f.sin() * l as f32) as i32;
                            let p = (start.0 + jv, start.1 + i - 3 + l / 2, start.2 + kv);
                            if set_if_can_replace_or_log(ctx, p, trunk_st) {
                                logs.push(p);
                            }
                        }
                        nodes.push(TreeNode {
                            center: (start.0 + jv, start.1 + i, start.2 + kv),
                            foliage_radius: -2,
                            giant: false,
                        });
                        i -= 2 + rng.next_bound(4);
                    }
                }
            }
            TP::DarkOak => {
                let below = (start.0, start.1 - 1, start.2);
                for dp in [
                    below,
                    (below.0 + 1, below.1, below.2),
                    (below.0, below.1, below.2 + 1),
                    (below.0 + 1, below.1, below.2 + 1),
                ] {
                    set_dirt(ctx, rng, dp, dirt_provider, extra.force_dirt, &mut logs);
                }
                let lean_dir = rand_horizontal(rng);
                let lean_h = height - rng.next_bound(4);
                let mut lean_steps = 2 - rng.next_bound(3);
                let (mut tx, mut tz) = (start.0, start.2);
                let ey = start.1 + height - 1;
                for dy in 0..height {
                    if dy >= lean_h && lean_steps > 0 {
                        let (ox, oz) = hoff(lean_dir);
                        tx += ox;
                        tz += oz;
                        lean_steps -= 1;
                    }
                    let p = (tx, start.1 + dy, tz);
                    if is_air_or_leaves(ctx, p) {
                        for (dx, dz) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                            let pp = (tx + dx, start.1 + dy, tz + dz);
                            if set_if_can_replace_or_log(ctx, pp, trunk_st) {
                                logs.push(pp);
                            }
                        }
                    }
                }
                nodes.push(TreeNode {
                    center: (tx, ey, tz),
                    foliage_radius: 0,
                    giant: true,
                });
                for ox in -1..=2 {
                    for oz in -1..=2 {
                        if (!(0..=1).contains(&ox) || !(0..=1).contains(&oz))
                            && rng.next_bound(3) <= 0
                        {
                            let len = rng.next_bound(3) + 2;
                            for by in 0..len {
                                let p = (start.0 + ox, ey - by - 1, start.2 + oz);
                                if set_if_can_replace_or_log(ctx, p, trunk_st) {
                                    logs.push(p);
                                }
                            }
                            nodes.push(TreeNode {
                                center: (start.0 + ox, ey, start.2 + oz),
                                foliage_radius: 0,
                                giant: false,
                            });
                        }
                    }
                }
            }
            TP::Forking => {
                set_dirt(
                    ctx,
                    rng,
                    (start.0, start.1 - 1, start.2),
                    dirt_provider,
                    extra.force_dirt,
                    &mut logs,
                );
                let lean_dir = rand_horizontal(rng);
                let lean_h = height - rng.next_bound(4) - 1;
                let mut lean_steps = 3 - rng.next_bound(3);
                let (mut tx, mut tz) = (start.0, start.2);
                let mut last_y = None;
                for yo in 0..height {
                    let yy = start.1 + yo;
                    if yo >= lean_h && lean_steps > 0 {
                        let (ox, oz) = hoff(lean_dir);
                        tx += ox;
                        tz += oz;
                        lean_steps -= 1;
                    }
                    let p = (tx, yy, tz);
                    if set_if_can_replace(ctx, p, trunk_st) {
                        logs.push(p);
                        last_y = Some(yy + 1);
                    }
                }
                if let Some(ey) = last_y {
                    nodes.push(TreeNode {
                        center: (tx, ey, tz),
                        foliage_radius: 1,
                        giant: false,
                    });
                }
                let (mut bx, mut bz) = (start.0, start.2);
                let branch_dir = rand_horizontal(rng);
                if branch_dir != lean_dir {
                    let branch_pos = lean_h - rng.next_bound(2) - 1;
                    let mut branch_steps = 1 + rng.next_bound(3);
                    let mut branch_last_y = None;
                    let mut yo = branch_pos;
                    while yo < height && branch_steps > 0 {
                        if yo >= 1 {
                            let yy = start.1 + yo;
                            let (ox, oz) = hoff(branch_dir);
                            bx += ox;
                            bz += oz;
                            let p = (bx, yy, bz);
                            if set_if_can_replace(ctx, p, trunk_st) {
                                logs.push(p);
                                branch_last_y = Some(yy + 1);
                            }
                        }
                        yo += 1;
                        branch_steps -= 1;
                    }
                    if let Some(ey) = branch_last_y {
                        nodes.push(TreeNode {
                            center: (bx, ey, bz),
                            foliage_radius: 0,
                            giant: false,
                        });
                    }
                }
            }
            TP::Fancy => {
                let j = height + 2;
                let k = ((j as f64) * 0.618).floor() as i32;
                set_dirt(
                    ctx,
                    rng,
                    (start.0, start.1 - 1, start.2),
                    dirt_provider,
                    extra.force_dirt,
                    &mut logs,
                );
                let l = ((1.382 + (1.0 * j as f64 / 13.0).powf(2.0)).floor() as i32).min(1);
                let m = start.1 + k;
                let mut list: Vec<(P3, i32)> = vec![];
                // Vanilla starts the attachment list at `start.b(j - 5)`; `k`
                // is the *upper end* height, not the attachment's own y. Using
                // `start + k` moved every fancy canopy up by `k - (height - 3)`.
                list.push(((start.0, start.1 + j - 5, start.2), m));
                for n in (0..=(j - 5)).rev() {
                    let f = fancy_branch_prob(j, n);
                    if f < 0.0 {
                        continue;
                    }
                    for _ in 0..l {
                        let g = f as f64 * (rng.next_f32() as f64 + 0.328);
                        let h = (rng.next_f32() * 2.0f32) as f64 * std::f64::consts::PI;
                        let p = g * h.sin() + 0.5;
                        let q = g * h.cos() + 0.5;
                        let bp = (
                            start.0 + p.floor() as i32,
                            start.1 + n - 1,
                            start.2 + q.floor() as i32,
                        );
                        let bp2 = (bp.0, bp.1 + 5, bp.2);
                        if !fancy_branch(ctx, bp, bp2, trunk_st, false, &mut logs) {
                            continue;
                        }
                        let r = start.0 - bp.0;
                        let s = start.2 - bp.2;
                        let t = bp.1 as f64 - ((r * r + s * s) as f64).sqrt() * 0.381;
                        let u = if t > m as f64 { m } else { t as i32 };
                        let bp3 = (start.0, u, start.2);
                        if !fancy_branch(ctx, bp3, bp, trunk_st, false, &mut logs) {
                            continue;
                        }
                        list.push((bp, u));
                    }
                }
                let _ = fancy_branch(
                    ctx,
                    start,
                    (start.0, start.1 + k, start.2),
                    trunk_st,
                    true,
                    &mut logs,
                );
                for (node_p, end_y) in &list {
                    let i = *end_y;
                    let bp = (start.0, i, start.2);
                    if bp == *node_p || !fancy_high(j, i - start.1) {
                        continue;
                    }
                    let _ = fancy_branch(ctx, bp, *node_p, trunk_st, true, &mut logs);
                }
                for (node_p, end_y) in list {
                    if fancy_high(j, end_y - start.1) {
                        nodes.push(TreeNode {
                            center: node_p,
                            foliage_radius: 0,
                            giant: false,
                        });
                    }
                }
            }
            TP::Bending {
                min_height_for_leaves,
                bend_length,
            } => {
                let dir = rand_horizontal(rng);
                let log_h = height - 1;
                let mut pos = start;
                set_dirt(
                    ctx,
                    rng,
                    (pos.0, pos.1 - 1, pos.2),
                    dirt_provider,
                    extra.force_dirt,
                    &mut logs,
                );
                for i in 0..=log_h {
                    if i + 1 >= log_h + rng.next_bound(2) {
                        let (ox, oz) = hoff(dir);
                        pos = (pos.0 + ox, pos.1, pos.2 + oz);
                    }
                    if set_if_can_replace(ctx, pos, trunk_st) {
                        logs.push(pos);
                    }
                    if i >= *min_height_for_leaves {
                        nodes.push(TreeNode {
                            center: pos,
                            foliage_radius: 0,
                            giant: false,
                        });
                    }
                    pos = (pos.0, pos.1 + 1, pos.2);
                }
                for _ in 0..=bend_length.get(rng) {
                    if set_if_can_replace(ctx, pos, trunk_st) {
                        logs.push(pos);
                    }
                    nodes.push(TreeNode {
                        center: pos,
                        foliage_radius: 0,
                        giant: false,
                    });
                    let (ox, oz) = hoff(dir);
                    pos = (pos.0 + ox, pos.1, pos.2 + oz);
                }
            }
            TP::UpwardsBranching {
                extra_steps,
                branch_prob,
                extra_len,
                grow_through,
            } => {
                for hp in 0..height {
                    let cp = (start.0, start.1 + hp, start.2);
                    let place_ok = {
                        let reg = breg();
                        let cur = ctx.get(cp.0, cp.1, cp.2);
                        reg.map(|r| {
                            r.is_air(cur)
                                || r.in_tag(cur, "minecraft:replaceable_by_trees")
                                || grow_through.iter().any(|g| *g == r.name_of(cur))
                        })
                        .unwrap_or(false)
                    };
                    if place_ok {
                        ctx.set(cp.0, cp.1, cp.2, trunk_st);
                        logs.push(cp);
                        if hp < height - 1 && rng.next_f32() < *branch_prob {
                            let bdir = rand_horizontal(rng);
                            let branch_pos = (extra_len.get(rng) - extra_len.get(rng) - 1).max(0);
                            let mut branch_steps = extra_steps.get(rng);
                            let mut height_along = cp.1 + branch_pos;
                            let (mut lx, mut lz) = (cp.0, cp.2);
                            let mut bpi = branch_pos;
                            while bpi < height && branch_steps > 0 {
                                if bpi >= 1 {
                                    let ph = cp.1 + bpi;
                                    let (ox, oz) = hoff(bdir);
                                    lx += ox;
                                    lz += oz;
                                    height_along = ph;
                                    let p = (lx, ph, lz);
                                    let ok2 = {
                                        let reg = breg();
                                        let cur = ctx.get(p.0, p.1, p.2);
                                        reg.map(|r| {
                                            r.is_air(cur)
                                                || r.in_tag(cur, "minecraft:replaceable_by_trees")
                                                || grow_through.iter().any(|g| *g == r.name_of(cur))
                                        })
                                        .unwrap_or(false)
                                    };
                                    if ok2 {
                                        ctx.set(p.0, p.1, p.2, trunk_st);
                                        logs.push(p);
                                        height_along = ph + 1;
                                    }
                                    nodes.push(TreeNode {
                                        center: p,
                                        foliage_radius: 0,
                                        giant: false,
                                    });
                                }
                                bpi += 1;
                                branch_steps -= 1;
                            }
                            if height_along - cp.1 > 1 {
                                let fp = (lx, height_along, lz);
                                nodes.push(TreeNode {
                                    center: fp,
                                    foliage_radius: 0,
                                    giant: false,
                                });
                                nodes.push(TreeNode {
                                    center: (fp.0, fp.1 - 2, fp.2),
                                    foliage_radius: 0,
                                    giant: false,
                                });
                            }
                        }
                    }
                    if hp == height - 1 {
                        nodes.push(TreeNode {
                            center: (cp.0, cp.1 + 1, cp.2),
                            foliage_radius: 0,
                            giant: false,
                        });
                    }
                }
            }
            TP::Cherry {
                count,
                horizontal,
                start_off,
                end_off,
            } => {
                set_dirt(
                    ctx,
                    rng,
                    (start.0, start.1 - 1, start.2),
                    dirt_provider,
                    extra.force_dirt,
                    &mut logs,
                );
                let first_off = (height - 1 + start_off.get(rng)).max(0);
                let mut second_off = (height - 1 + {
                    // second uses min..max-1 of the same uniform range
                    match start_off {
                        IP::Uniform(lo, hi) => rng.next_inbetween(*lo, hi - 1),
                        other => other.get(rng),
                    }
                })
                .max(0);
                if second_off >= first_off {
                    second_off += 1;
                }
                let branch_count = count.get(rng);
                let mid = branch_count == 3;
                let both = branch_count >= 2;
                let trunk_h = if mid {
                    height
                } else if both {
                    first_off.max(second_off) + 1
                } else {
                    first_off + 1
                };
                for y in 0..trunk_h {
                    let p = (start.0, start.1 + y, start.2);
                    if set_if_can_replace(ctx, p, trunk_st) {
                        logs.push(p);
                    }
                }
                if mid {
                    nodes.push(TreeNode {
                        center: (start.0, start.1 + trunk_h, start.2),
                        foliage_radius: 0,
                        giant: false,
                    });
                }
                // `Plane.HORIZONTAL.getRandomDirection(random)`: `CherryTrunkPlacer` draws
                // the branch direction once and mirrors it for the second branch.
                let bd = {
                    let (ox, oz) = hoff(plane_horizontal()[rng.next_bound(4) as usize]);
                    (ox, oz)
                };
                cherry_branch(
                    ctx,
                    rng,
                    height,
                    start,
                    trunk_st,
                    bd,
                    first_off,
                    first_off < trunk_h - 1,
                    horizontal,
                    end_off,
                    &mut nodes,
                    &mut logs,
                );
                if both {
                    let bd2 = (-bd.0, -bd.1);
                    cherry_branch(
                        ctx,
                        rng,
                        height,
                        start,
                        trunk_st,
                        bd2,
                        second_off,
                        second_off < trunk_h - 1,
                        horizontal,
                        end_off,
                        &mut nodes,
                        &mut logs,
                    );
                }
            }
            TP::Poplar {
                above_branches,
                branch_amount,
            } => {
                set_dirt(
                    ctx,
                    rng,
                    (start.0, start.1 - 1, start.2),
                    dirt_provider,
                    extra.force_dirt,
                    &mut logs,
                );
                let branch_h = height - above_branches.get(rng);
                for y in 0..height {
                    let p = (start.0, start.1 + y, start.2);
                    if set_if_can_replace(ctx, p, trunk_st) {
                        logs.push(p);
                    }
                    // vanilla shuffles 6 dirs, drops the vertical ones
                    let mut ds = [0u8, 1, 2, 3, 4, 5];
                    for i in (1..6).rev() {
                        let j = rng.next_bound(i as i32 + 1) as usize;
                        ds.swap(i, j);
                    }
                    if branch_h - 1 != y {
                        continue;
                    }
                    let n = branch_amount.get(rng).max(0) as usize;
                    for &d in ds.iter().filter(|&&d| d >= 2).take(n) {
                        let (ox, oz) = hoff(d);
                        let bp = (p.0 + ox, p.1, p.2 + oz);
                        let ax = if ox != 0 { "x" } else { "z" };
                        if set_axis(ctx, bp, trunk_st, ax) {
                            logs.push(bp);
                        }
                    }
                }
                nodes.push(TreeNode {
                    center: (start.0, start.1 + branch_h, start.2),
                    foliage_radius: 0,
                    giant: false,
                });
            }
        }
        (nodes, logs)
    }
}

fn fancy_branch_prob(h: i32, y: i32) -> f32 {
    if (y as f32) < (h as f32) * 0.3 {
        return -1.0;
    }
    let f = h as f32 / 2.0;
    let g = f - y as f32;
    let sq = f * f - g * g;
    if sq < 0.0 {
        return 0.0;
    }
    let h2 = sq.sqrt();
    if g == 0.0 {
        // Vanilla assigns `h2 = f` for the exact centre layer, then still
        // returns `h2 * 0.5`; returning h2 here made the centre limb twice as
        // long and moved its candidate by a cell.
        h2 * 0.5
    } else if g.abs() >= f {
        0.0
    } else {
        h2 * 0.5
    }
}
fn fancy_high(h: i32, y: i32) -> bool {
    y as f64 >= h as f64 * 0.2
}
/// `TrunkPlacer.isFree`: air, `REPLACEABLE_BY_TREES` (which already contains
/// leaves) **or logs** (`BlockTags.LOGS`, mapping `awe.u`). A stone cell is not
/// free; a branch cannot pass through rock.
#[cfg(all(test, megu_vanilla_data))]
fn fancy_branch_open(reg: &BReg, cur: u32) -> bool {
    tree_tables(reg).is_replaceable_or_log(reg, cur)
}
fn fancy_branch(ctx: &mut FC, a: P3, b: P3, trunk_st: u32, make: bool, logs: &mut Vec<P3>) -> bool {
    if !make && a == b {
        return true;
    }
    let off = (b.0 - a.0, b.1 - a.1, b.2 - a.2);
    let i = off.0.abs().max(off.1.abs()).max(off.2.abs());
    if i == 0 {
        return true;
    }
    let Some(reg) = breg() else {
        return false;
    };
    let tables = tree_tables(reg);
    let f = off.0 as f32 / i as f32;
    let g = off.1 as f32 / i as f32;
    let h = off.2 as f32 / i as f32;
    for j in 0..=i {
        let p = (
            (0.5 + j as f32 * f).floor() as i32 + a.0,
            (0.5 + j as f32 * g).floor() as i32 + a.1,
            (0.5 + j as f32 * h).floor() as i32 + a.2,
        );
        if make {
            let dx = (p.0 - a.0).abs();
            let dz = (p.2 - a.2).abs();
            let axis = if dx.max(dz) == 0 {
                "y"
            } else if dx >= dz {
                "x"
            } else {
                "z"
            };
            if set_axis(ctx, p, trunk_st, axis) {
                logs.push(p);
                continue;
            }
        }
        let cur = ctx.get(p.0, p.1, p.2);
        if tables.is_replaceable_or_log(reg, cur) {
            continue;
        }
        return false;
    }
    true
}
#[expect(clippy::too_many_arguments)]
fn cherry_branch(
    ctx: &mut FC,
    rng: &mut Xor,
    height: i32,
    origin: P3,
    trunk_st: u32,
    bdir: (i32, i32),
    off_from_origin: i32,
    middle_up: bool,
    horizontal: &IP,
    end_off: &IP,
    nodes: &mut Vec<TreeNode>,
    logs: &mut Vec<P3>,
) {
    let mut lp = (origin.0, origin.1 + off_from_origin, origin.2);
    let bend_off = height - 1 + end_off.get(rng);
    let extend = middle_up || bend_off < off_from_origin;
    let dist = horizontal.get(rng) + extend as i32;
    let bend = (
        origin.0 + bdir.0 * dist,
        origin.1 + bend_off,
        origin.2 + bdir.1 * dist,
    );
    let steps_h = if extend { 2 } else { 1 };
    let axis = if bdir.0 != 0 { "x" } else { "z" };
    for _ in 0..steps_h {
        lp = (lp.0 + bdir.0, lp.1, lp.2 + bdir.1);
        if set_axis(ctx, lp, trunk_st, axis) {
            logs.push(lp);
        }
    }
    let vdir = if bend.1 > lp.1 { 1 } else { -1 };
    loop {
        let dist_left = (bend.0 - lp.0).abs() + (bend.1 - lp.1).abs() + (bend.2 - lp.2).abs();
        if dist_left == 0 {
            nodes.push(TreeNode {
                center: (bend.0, bend.1 + 1, bend.2),
                foliage_radius: 0,
                giant: false,
            });
            return;
        }
        let chance = (bend.1 - lp.1).abs() as f32 / dist_left as f32;
        if rng.next_f32() < chance {
            lp = (lp.0, lp.1 + vdir, lp.2);
            if set_if_can_replace(ctx, lp, trunk_st) {
                logs.push(lp);
            }
        } else {
            lp = (lp.0 + bdir.0, lp.1, lp.2 + bdir.1);
            if set_axis(ctx, lp, trunk_st, axis) {
                logs.push(lp);
            }
        }
    }
}

// ---------- foliage placers ----------

fn leave_pos_invalid(f: &FP, rng: &mut Xor, dx: i32, y: i32, dz: i32, r: i32, giant: bool) -> bool {
    let (x, z) = if giant {
        (dx.abs().min((dx - 1).abs()), dz.abs().min((dz - 1).abs()))
    } else {
        (dx.abs(), dz.abs())
    };
    f.invalid(rng, x, y, z, r, giant)
}
#[expect(clippy::too_many_arguments)]
fn gen_square(
    fp: &FP,
    out: &mut Vec<P3>,
    ctx: &mut FC,
    rng: &mut Xor,
    center: P3,
    radius: i32,
    y: i32,
    giant: bool,
    fst: u32,
) {
    let i = giant as i32;
    for x in -radius..=(radius + i) {
        for z in -radius..=(radius + i) {
            if leave_pos_invalid(fp, rng, x, y, z, radius, giant) {
                continue;
            }
            put_leaf(ctx, out, (center.0 + x, center.1 + y, center.2 + z), fst);
        }
    }
}
/// `FoliagePlacer.placeLeaf` (`efu.a`), the gate every foliage placer's leaf write goes
/// through:
///
/// ```java
/// if (!TreeFeature.isFree(level, pos)) return false;   // edq.c
/// BlockState state = config.foliageProvider().getState(random, pos);
/// if (state.hasProperty(WATERLOGGED)) state = ...;      // per-position rewrite
/// setter.set(pos, state);                              // TreeFeature$1: add + setBlock
/// ```
///
/// So a canopy cell the current state refuses is absent from the tree's leaf list, and
/// from every draw a decorator that walks that list consumes (`leave_vine` draws four
/// `nextFloat` per entry). `TreeFeature$1` itself adds unconditionally *because* this
/// gate already ran.
///
/// Not modelled here, and not measured in this pass: vanilla resolves the provider state
/// per leaf cell and rewrites `WATERLOGGED` from the fluid at that cell, while this port
/// resolves the foliage state once per tree.
fn put_leaf(ctx: &mut FC, out: &mut Vec<P3>, p: P3, st: u32) {
    if set_if_can_replace(ctx, p, st) {
        out.push(p);
    }
}
fn is_leaf_set(ctx: &mut FC, p: P3) -> bool {
    if let Some(reg) = breg() {
        tree_tables(reg).is_leaves(reg, ctx.get(p.0, p.1, p.2))
    } else {
        false
    }
}
#[expect(clippy::too_many_arguments)]
fn hanging_square(
    fp: &FP,
    out: &mut Vec<P3>,
    ctx: &mut FC,
    rng: &mut Xor,
    center: P3,
    radius: i32,
    y: i32,
    giant: bool,
    fst: u32,
    hang: f32,
    ext: f32,
) {
    gen_square(fp, out, ctx, rng, center, radius, y, giant, fst);
    let i = giant as i32;
    let log_pos = (center.0, center.1 - 1, center.2);
    for along in [2u8, 3, 5, 4] {
        // north, south, east, west; to_edge = rotate clockwise
        let to_edge = match along {
            2 => 4u8,
            3 => 5,
            4 => 3,
            _ => 2,
        };
        let (ex, ez) = hoff(to_edge);
        let (ax, az) = hoff(along);
        let off_e = if matches!(to_edge, 3 | 5) {
            radius + i
        } else {
            radius
        };
        let mut p = (
            center.0 + ex * off_e - ax * radius,
            center.1 + y - 1,
            center.2 + ez * off_e - az * radius,
        );
        for _ in -radius..(radius + i) {
            let above = (p.0, p.1 + 1, p.2);
            if is_leaf_set(ctx, above) {
                let dist =
                    (p.0 - log_pos.0).abs() + (p.1 - log_pos.1).abs() + (p.2 - log_pos.2).abs();
                if dist < 7 && rng.next_f32() <= hang && set_if_can_replace(ctx, p, fst) {
                    out.push(p);
                    let dn = (p.0, p.1 - 1, p.2);
                    let dist2 = dist + 1;
                    if dist2 < 7 && rng.next_f32() <= ext && set_if_can_replace(ctx, dn, fst) {
                        out.push(dn);
                    }
                }
            }
            p = (p.0 + ax, p.1, p.2 + az);
        }
    }
}

pub(crate) enum FP {
    Blob {
        height: i32,
    },
    Spruce {
        trunk_h: IP,
    },
    Pine {
        height: IP,
    },
    Acacia,
    Bush {
        height: i32,
    },
    Fancy {
        height: i32,
    },
    Jungle {
        height: i32,
    },
    MegaPine {
        crown: IP,
    },
    DarkOak,
    RandomSpread {
        fh: IP,
        attempts: i32,
    },
    Cherry {
        height: IP,
        wide_hole: f32,
        corner_hole: f32,
        hang: f32,
        hang_ext: f32,
    },
    Poplar {
        height: IP,
        side_hole: f32,
    },
    MegaJungle {
        height: i32,
    }, // vanilla mega_jungle ≈ jungle with giant flag
}
impl FP {
    pub fn invalid(&self, rng: &mut Xor, dx: i32, y: i32, dz: i32, r: i32, giant: bool) -> bool {
        match self {
            FP::Blob { .. } => dx == r && dz == r && (rng.next_bound(2) == 0 || y == 0),
            FP::Spruce { .. } | FP::Pine { .. } => dx == r && dz == r && r > 0,
            FP::Bush { .. } => dx == r && dz == r && rng.next_bound(2) == 0,
            FP::Fancy { .. } => {
                let f = dx as f32 + 0.5;
                let g = dz as f32 + 0.5;
                f * f + g * g > (r * r) as f32
            }
            FP::Jungle { .. } | FP::MegaPine { .. } | FP::MegaJungle { .. } => {
                if dx + dz >= 7 {
                    return true;
                }
                dx * dx + dz * dz > r * r
            }
            FP::Acacia => {
                if y == 0 {
                    (dx > 1 || dz > 1) && dx != 0 && dz != 0
                } else {
                    dx == r && dz == r && r > 0
                }
            }
            FP::DarkOak => {
                if y == -1 && !giant {
                    return dx == r && dz == r;
                }
                if y == 1 {
                    return dx + dz > r * 2 - 2;
                }
                false
            }
            FP::Cherry {
                wide_hole,
                corner_hole,
                ..
            } => {
                if y == -1 && (dx == r || dz == r) && rng.next_f32() < *wide_hole {
                    return true;
                }
                let corner = dx == r && dz == r;
                if r > 2 {
                    corner || dx + dz > r * 2 - 2 && rng.next_f32() < *corner_hole
                } else {
                    corner && rng.next_f32() < *corner_hole
                }
            }
            FP::Poplar { .. } | FP::RandomSpread { .. } => false, // poplar uses its own rhombus fn
        }
    }
    /// vanilla FoliagePlacer::get_random_height(random, trunk_height)
    pub fn rand_height(&self, rng: &mut Xor, trunk_h: i32) -> i32 {
        match self {
            FP::Blob { height }
            | FP::Bush { height }
            | FP::Fancy { height }
            | FP::Jungle { height }
            | FP::MegaJungle { height } => *height,
            FP::DarkOak => 4,
            FP::Acacia => 0,
            FP::Spruce { trunk_h: t } => (trunk_h - t.get(rng)).max(4),
            FP::Pine { height } | FP::Cherry { height, .. } | FP::Poplar { height, .. } => {
                height.get(rng)
            }
            FP::MegaPine { crown } => crown.get(rng),
            FP::RandomSpread { fh, .. } => fh.get(rng),
        }
    }
    pub fn rand_radius(&self, placer_radius: &IP, rng: &mut Xor, base_h: i32) -> i32 {
        match self {
            FP::Pine { .. } => placer_radius.get(rng) + rng.next_bound((base_h + 1).max(1)),
            _ => placer_radius.get(rng),
        }
    }
    #[expect(clippy::too_many_arguments)]
    pub fn generate_into(
        &self,
        out: &mut Vec<P3>,
        offset: i32,
        _placer_radius: &IP,
        ctx: &mut FC,
        rng: &mut Xor,
        node: &TreeNode,
        fh: i32,
        radius: i32,
        fst: u32,
    ) {
        match self {
            FP::Blob { .. } => {
                for y in (offset - fh..=offset).rev() {
                    let r = (radius + node.foliage_radius - 1 - y / 2).max(0);
                    gen_square(self, out, ctx, rng, node.center, r, y, node.giant, fst);
                }
            }
            FP::Spruce { .. } => {
                let mut r = rng.next_bound(2);
                let mut max = 1;
                let mut next = 0;
                for y in (-fh..=offset).rev() {
                    gen_square(self, out, ctx, rng, node.center, r, y, node.giant, fst);
                    if r >= max {
                        r = next;
                        next = 1;
                        max = (radius + node.foliage_radius).min(max + 1);
                        continue;
                    }
                    r += 1;
                }
            }
            FP::Pine { .. } => {
                let mut r = 0;
                for y in (offset - fh..=offset).rev() {
                    gen_square(self, out, ctx, rng, node.center, r, y, node.giant, fst);
                    if r >= 1 && y == offset - fh + 1 {
                        r -= 1;
                    } else if r < radius + node.foliage_radius {
                        r += 1;
                    }
                }
            }
            FP::Acacia => {
                gen_square(
                    self,
                    out,
                    ctx,
                    rng,
                    node.center,
                    radius + node.foliage_radius,
                    -1,
                    node.giant,
                    fst,
                );
                gen_square(
                    self,
                    out,
                    ctx,
                    rng,
                    node.center,
                    radius - 1,
                    -fh,
                    node.giant,
                    fst,
                );
                gen_square(
                    self,
                    out,
                    ctx,
                    rng,
                    node.center,
                    radius + node.foliage_radius - 1,
                    0,
                    node.giant,
                    fst,
                );
            }
            FP::Bush { .. } => {
                for y in (offset - fh..=offset).rev() {
                    let r = radius + node.foliage_radius - 1 - y;
                    gen_square(self, out, ctx, rng, node.center, r, y, node.giant, fst);
                }
            }
            FP::Fancy { .. } => {
                for y in (offset - fh..=offset).rev() {
                    let r = radius + i32::from(!(y == offset || y == offset - fh));
                    gen_square(self, out, ctx, rng, node.center, r, y, node.giant, fst);
                }
            }
            FP::Jungle { .. } | FP::MegaJungle { .. } => {
                let h = if node.giant {
                    fh
                } else {
                    1 + rng.next_bound(2)
                };
                for y in (offset - h..=offset).rev() {
                    let r = radius + node.foliage_radius + 1 - y;
                    gen_square(self, out, ctx, rng, node.center, r, y, node.giant, fst);
                }
            }
            FP::MegaPine { .. } => {
                let pos = node.center;
                let mut cur_r = 0;
                for y in (pos.1 - fh + offset)..=(pos.1 + offset) {
                    let delta = pos.1 - y;
                    let cr = radius
                        + node.foliage_radius
                        + ((delta as f32 / fh as f32) * 3.5).floor() as i32;
                    let r = if delta > 0 && cr == cur_r && (y & 1) == 0 {
                        cr + 1
                    } else {
                        cr
                    };
                    gen_square(
                        self,
                        out,
                        ctx,
                        rng,
                        (pos.0, y, pos.2),
                        r,
                        0,
                        node.giant,
                        fst,
                    );
                    cur_r = cr;
                }
            }
            FP::DarkOak => {
                let pos = (node.center.0, node.center.1 + offset, node.center.2);
                gen_square(self, out, ctx, rng, pos, radius + 2, -1, node.giant, fst);
                if node.giant {
                    gen_square(self, out, ctx, rng, pos, radius + 3, 0, node.giant, fst);
                    gen_square(self, out, ctx, rng, pos, radius + 2, 1, node.giant, fst);
                    if rng.next_bool() {
                        gen_square(self, out, ctx, rng, pos, radius, 2, node.giant, fst);
                    }
                } else {
                    gen_square(self, out, ctx, rng, pos, radius + 1, 0, node.giant, fst);
                }
            }
            FP::RandomSpread { attempts, .. } => {
                for _ in 0..*attempts {
                    let p = (
                        node.center.0 + rng.next_bound(radius) - rng.next_bound(radius),
                        node.center.1 + rng.next_bound(fh) - rng.next_bound(fh),
                        node.center.2 + rng.next_bound(radius) - rng.next_bound(radius),
                    );
                    put_leaf(ctx, out, p, fst);
                }
            }
            FP::Cherry { hang, hang_ext, .. } => {
                let pos = (node.center.0, node.center.1 + offset, node.center.2);
                let cr = radius + node.foliage_radius - 1;
                gen_square(self, out, ctx, rng, pos, cr - 2, fh - 3, node.giant, fst);
                gen_square(self, out, ctx, rng, pos, cr - 1, fh - 4, node.giant, fst);
                for y in (0..=fh - 5).rev() {
                    gen_square(self, out, ctx, rng, pos, cr, y, node.giant, fst);
                }
                hanging_square(
                    self, out, ctx, rng, pos, cr, -1, node.giant, fst, *hang, *hang_ext,
                );
                hanging_square(
                    self,
                    out,
                    ctx,
                    rng,
                    pos,
                    cr - 1,
                    -2,
                    node.giant,
                    fst,
                    *hang,
                    *hang_ext,
                );
            }
            FP::Poplar { side_hole, .. } => {
                let pos = (node.center.0, node.center.1 + offset, node.center.2);
                let cr = radius + node.foliage_radius - 1;
                let flipped = rng.next_bool();
                for (r, y) in [(cr - 2, fh - 1), (cr - 1, fh - 2), (cr - 1, fh - 3)] {
                    poplar_rhombus(
                        out, ctx, rng, pos, r, y, node.giant, fst, fh, flipped, *side_hole,
                    );
                }
                for y in (1..=fh - 4).rev() {
                    poplar_rhombus(
                        out, ctx, rng, pos, cr, y, node.giant, fst, fh, flipped, *side_hole,
                    );
                }
                poplar_rhombus(
                    out,
                    ctx,
                    rng,
                    pos,
                    cr - 1,
                    0,
                    node.giant,
                    fst,
                    fh,
                    flipped,
                    *side_hole,
                );
                poplar_rhombus(
                    out,
                    ctx,
                    rng,
                    pos,
                    (cr - 2).clamp(1, 2),
                    -1,
                    node.giant,
                    fst,
                    fh,
                    flipped,
                    *side_hole,
                );
            }
        }
    }
}
#[expect(clippy::too_many_arguments)]
fn poplar_rhombus(
    out: &mut Vec<P3>,
    ctx: &mut FC,
    rng: &mut Xor,
    center: P3,
    r: i32,
    y: i32,
    giant: bool,
    fst: u32,
    fh: i32,
    flipped: bool,
    side_hole: f32,
) {
    let i = giant as i32;
    for x in -r..=(r + i) {
        for z in -r..=(r + i) {
            let partial = fh - 1 == y || fh - 2 == y;
            let (adx, adz) = (x.abs(), z.abs());
            if partial && (adx == r || adz == r) {
                continue;
            }
            let small_corner = if flipped {
                x > 0 && z > 0 || z < 0 && x < 0
            } else {
                x > 0 && z < 0 || z > 0 && x < 0
            };
            let corner = if small_corner {
                r - 1
            } else if partial {
                r + 1
            } else {
                r
            };
            let side = i32::from(rng.next_f32() <= side_hole);
            if adx + adz > r * 2 - (corner + side) {
                continue;
            }
            let p = (center.0 + x, center.1 + y, center.2 + z);
            put_leaf(ctx, out, p, fst);
        }
    }
}

// ---------- feature size ----------

pub(crate) enum FS {
    Two {
        limit: i32,
        lower: i32,
        upper: i32,
    },
    Three {
        limit: i32,
        upper_limit: i32,
        lower: i32,
        middle: i32,
        upper: i32,
    },
}
impl FS {
    fn radius(&self, height: i32, y: i32) -> i32 {
        match self {
            FS::Two {
                limit,
                lower,
                upper,
            } => {
                if y < *limit {
                    *lower
                } else {
                    *upper
                }
            }
            FS::Three {
                limit,
                upper_limit,
                lower,
                middle,
                upper,
            } => {
                if y < *limit {
                    *lower
                } else if y >= height - *upper_limit {
                    *upper
                } else {
                    *middle
                }
            }
        }
    }
}

// ---------- decorators ----------

pub(crate) enum Deco {
    TrunkVine,
    LeaveVine(f32),
    PaleMoss {
        leaves: f32,
        trunk: f32,
        #[expect(dead_code)]
        ground: f32,
    },
    Creaking(f32),
    Cocoa(f32),
    Beehive(f32),
    AlterGround(Bsp),
    PlaceOnGround {
        tries: i32,
        radius: i32,
        height: i32,
        bsp: Bsp,
    },
    AttachedLeaves {
        bsp: Bsp,
        dirs: Vec<u8>,
        excl_xz: i32,
        excl_y: i32,
        prob: f32,
        req_empty: i32,
    },
    AttachedLogs {
        prob: f32,
        bsp: Bsp,
        dirs: Vec<u8>,
    },
    ShelfMushroom(f32),
}

fn litter_positions<'a>(roots: &'a [P3], logs: &'a [P3]) -> Vec<P3> {
    if roots.is_empty() {
        return logs.to_vec();
    }
    if logs.first().is_some_and(|l| roots[0].1 == l.1) {
        return [logs, roots].concat();
    }
    roots.to_vec()
}

impl Deco {
    pub(crate) fn run(
        &self,
        ctx: &mut FC,
        rng: &mut Xor,
        roots: &[P3],
        logs: &[P3],
        foliage: &[P3],
        decorations: &mut Vec<P3>,
    ) {
        let Some(reg) = breg() else { return };
        let tables = tree_tables(reg);
        let mut set = |ctx: &mut FC, p: P3, st| {
            decorations.push(p);
            ctx.set(p.0, p.1, p.2, st);
        };
        match self {
            Deco::TrunkVine => {
                let faces = [
                    (4u8, tables.vine_east),
                    (5, tables.vine_west),
                    (2, tables.vine_south),
                    (3, tables.vine_north),
                ];
                for &p in logs {
                    for &(d, st) in &faces {
                        if rng.next_bound(3) > 0 {
                            let (ox, oz) = hoff(d);
                            let t = (p.0 + ox, p.1, p.2 + oz);
                            if ctx.is_air(t.0, t.1, t.2) {
                                set(ctx, t, st);
                            }
                        }
                    }
                }
            }
            Deco::LeaveVine(prob) => {
                let faces = [
                    (4u8, tables.vine_east),
                    (5, tables.vine_west),
                    (2, tables.vine_south),
                    (3, tables.vine_north),
                ];
                for &p in foliage {
                    for &(d, st) in &faces {
                        if rng.next_f32() < *prob {
                            let (ox, oz) = hoff(d);
                            let t = (p.0 + ox, p.1, p.2 + oz);
                            if ctx.is_air(t.0, t.1, t.2) {
                                set(ctx, t, st);
                                let mut cur = (t.0, t.1 - 1, t.2);
                                for _ in 0..4 {
                                    if !ctx.is_air(cur.0, cur.1, cur.2) {
                                        break;
                                    }
                                    set(ctx, cur, st);
                                    cur.1 -= 1;
                                }
                            }
                        }
                    }
                }
            }
            Deco::Beehive(prob) => {
                if logs.is_empty() || rng.next_f32() >= *prob {
                    return;
                }
                let hive_y = foliage.first().map_or_else(
                    || {
                        let last_y = logs.last().map_or(0, |p| p.1);
                        (logs[0].1 + 1 + rng.next_bound(3)).min(last_y)
                    },
                    |fl| (fl.1 - 1).max(logs[0].1 + 1),
                );
                let mut places = vec![];
                for lp in logs.iter().filter(|p| p.1 == hive_y) {
                    for d in [5u8, 4, 3] {
                        // east, west, south
                        let (ox, oz) = hoff(d);
                        places.push((lp.0 + ox, lp.1, lp.2 + oz));
                    }
                }
                for i in (1..places.len()).rev() {
                    let j = rng.next_bound(i as i32 + 1) as usize;
                    places.swap(i, j);
                }
                let nest = tables.bee_nest_south;
                for p in places {
                    let front = (p.0, p.1, p.2 + 1); // south
                    if ctx.is_air(p.0, p.1, p.2) && ctx.is_air(front.0, front.1, front.2) {
                        set(ctx, p, nest);
                        break;
                    }
                }
            }
            Deco::Cocoa(prob) => {
                if logs.is_empty() || rng.next_f32() >= *prob {
                    return;
                }
                let ty = logs[0].1;
                for lp in logs.iter().filter(|p| p.1 - ty <= 2) {
                    // `for (Direction dir : Direction.Plane.HORIZONTAL)`: NORTH, EAST,
                    // SOUTH, WEST, one `nextFloat()` per direction. `p = pos.relative(
                    // dir.getOpposite())` and `FACING = dir` (`ji.g()` is read before the
                    // offset, so the cocoa sits on the near side of the log).
                    for f in plane_horizontal() {
                        if rng.next_f32() <= 0.25 {
                            let (ox, oz) = hoff(f);
                            let cp = (lp.0 - ox, lp.1, lp.2 - oz);
                            if ctx.is_air(cp.0, cp.1, cp.2) {
                                let fdir = match f {
                                    3 => "south",
                                    4 => "west",
                                    5 => "east",
                                    _ => "north",
                                };
                                let st = reg.state_of(
                                    "minecraft:cocoa",
                                    Some(&mkprops(&[
                                        ("age", &rng.next_bound(3).to_string()),
                                        ("facing", fdir),
                                    ])),
                                );
                                set(ctx, cp, st);
                            }
                        }
                    }
                }
            }
            Deco::AlterGround(bsp) => {
                let list = litter_positions(roots, logs);
                let Some(first) = list.first() else { return };
                let min_y = list.iter().map(|p| p.1).min().unwrap_or(first.1);
                for pos in list.iter().filter(|p| p.1 == min_y) {
                    for c in [
                        (pos.0 - 1, pos.2 - 1),
                        (pos.0 + 2, pos.2 - 1),
                        (pos.0 - 1, pos.2 + 2),
                        (pos.0 + 2, pos.2 + 2),
                    ] {
                        self_circle(ctx, rng, bsp, (c.0, pos.1, c.1), &mut set);
                    }
                    for _ in 0..5 {
                        let pl = rng.next_bound(64);
                        let (xx, zz) = (pl % 8, pl / 8);
                        if xx == 0 || xx == 7 || zz == 0 || zz == 7 {
                            self_circle(
                                ctx,
                                rng,
                                bsp,
                                (pos.0 - 3 + xx, pos.1, pos.2 - 3 + zz),
                                &mut set,
                            );
                        }
                    }
                }
            }
            Deco::PlaceOnGround {
                tries,
                radius,
                height,
                bsp,
            } => {
                let list = litter_positions(roots, logs);
                let Some(first) = list.first() else { return };
                let y0 = first.1;
                let (mut x0, mut x1, mut z0, mut z1) = (first.0, first.0, first.2, first.2);
                for p in &list {
                    if p.1 != y0 {
                        continue;
                    }
                    x0 = x0.min(p.0);
                    x1 = x1.max(p.0);
                    z0 = z0.min(p.2);
                    z1 = z1.max(p.2);
                }
                x0 -= radius;
                x1 += radius;
                z0 -= radius;
                z1 += radius;
                let (y0, y1) = (y0, y0 + height);
                for _ in 0..*tries {
                    let p = (
                        rng.next_inbetween(x0, x1),
                        rng.next_inbetween(y0, y1),
                        rng.next_inbetween(z0, z1),
                    );
                    let cur = ctx.get(p.0, p.1, p.2);
                    let below = ctx.get(p.0, p.1 - 1, p.2);
                    if (reg.is_air(cur) || reg.name_of(cur) == "minecraft:vine")
                        && reg.side_solid(below, 1)
                        && !reg.in_tag(below, "minecraft:leaves")
                    {
                        let st = bsp.get(ctx, rng, p.0, p.1, p.2);
                        set(ctx, p, st);
                    }
                }
            }
            Deco::AttachedLeaves {
                bsp,
                dirs,
                excl_xz,
                excl_y,
                prob,
                req_empty,
            } => {
                if dirs.is_empty() || foliage.is_empty() {
                    return;
                }
                let mut shuffled = foliage.to_vec();
                for i in (1..shuffled.len()).rev() {
                    let j = rng.next_bound(i as i32 + 1) as usize;
                    shuffled.swap(i, j);
                }
                let mut blacklist: std::collections::HashSet<P3> = Default::default();
                for lp in shuffled {
                    let d = dirs[rng.next_bound(dirs.len() as i32) as usize];
                    let (ox, oy, oz) = dir_off(d);
                    let pp = (lp.0 + ox, lp.1 + oy, lp.2 + oz);
                    if blacklist.contains(&pp) {
                        continue;
                    }
                    // The bytecode's order is exclusion-set test, then `nextFloat()`, then
                    // `isPosEmpty`. Drawing the float after the empty check skips one draw
                    // for every leaf whose direction is blocked (`required_empty_blocks`
                    // greater than zero), which shifts every later decorator's stream.
                    if rng.next_f32() >= *prob {
                        continue;
                    }
                    let mut ok = true;
                    for i in 1..=*req_empty {
                        let cp = (lp.0 + ox * i, lp.1 + oy * i, lp.2 + oz * i);
                        if !ctx.is_air(cp.0, cp.1, cp.2) {
                            ok = false;
                            break;
                        }
                    }
                    if ok {
                        for dx in -excl_xz..=*excl_xz {
                            for dy in -excl_y..=*excl_y {
                                for dz in -excl_xz..=*excl_xz {
                                    blacklist.insert((pp.0 + dx, pp.1 + dy, pp.2 + dz));
                                }
                            }
                        }
                        let st = bsp.get(ctx, rng, pp.0, pp.1, pp.2);
                        set(ctx, pp, st);
                    }
                }
            }
            Deco::AttachedLogs { prob, bsp, dirs } => {
                if dirs.is_empty() || logs.is_empty() {
                    return;
                }
                let mut shuffled = logs.to_vec();
                for i in (1..shuffled.len()).rev() {
                    let j = rng.next_bound(i as i32 + 1) as usize;
                    shuffled.swap(i, j);
                }
                for lp in shuffled {
                    let d = dirs[rng.next_bound(dirs.len() as i32) as usize];
                    let (ox, oy, oz) = dir_off(d);
                    let pp = (lp.0 + ox, lp.1 + oy, lp.2 + oz);
                    if rng.next_f32() <= *prob && ctx.is_air(pp.0, pp.1, pp.2) {
                        let st = bsp.get(ctx, rng, pp.0, pp.1, pp.2);
                        set(ctx, pp, st);
                    }
                }
            }
            Deco::PaleMoss { leaves, trunk, .. } => {
                if logs.is_empty() {
                    return;
                }
                let moss = reg.state_of(
                    "minecraft:pale_hanging_moss",
                    Some(&mkprops(&[("tip", "false")])),
                );
                let moss_tip = reg.state_of(
                    "minecraft:pale_hanging_moss",
                    Some(&mkprops(&[("tip", "true")])),
                );
                let mut hanger = |ctx: &mut FC, rng: &mut Xor, mut p: P3| {
                    while ctx.is_air(p.0, p.1 - 1, p.2) && rng.next_f32() >= 0.5 {
                        set(ctx, p, moss);
                        p.1 -= 1;
                    }
                    if ctx.is_air(p.0, p.1, p.2) {
                        set(ctx, p, moss_tip);
                    }
                };
                for &p in logs {
                    if rng.next_f32() < *trunk {
                        let dn = (p.0, p.1 - 1, p.2);
                        if ctx.is_air(dn.0, dn.1, dn.2) {
                            hanger(ctx, rng, dn);
                        }
                    }
                }
                for &p in foliage {
                    if rng.next_f32() < *leaves {
                        let dn = (p.0, p.1 - 1, p.2);
                        if ctx.is_air(dn.0, dn.1, dn.2) {
                            hanger(ctx, rng, dn);
                        }
                    }
                }
            }
            Deco::Creaking(prob) => {
                if logs.is_empty() || rng.next_f32() >= *prob {
                    return;
                }
                let mut shuffled = logs.to_vec();
                for i in (1..shuffled.len()).rev() {
                    let j = rng.next_bound(i as i32 + 1) as usize;
                    shuffled.swap(i, j);
                }
                for p in shuffled {
                    let mut all_logs = true;
                    for d in 0..6u8 {
                        let (ox, oy, oz) = dir_off(d);
                        let n = ctx.get(p.0 + ox, p.1 + oy, p.2 + oz);
                        if !reg.in_tag(n, "minecraft:logs") {
                            all_logs = false;
                            break;
                        }
                    }
                    if all_logs {
                        let st = reg.state_of(
                            "minecraft:creaking_heart",
                            Some(&mkprops(&[
                                ("axis", "y"),
                                ("creaking_heart_state", "dormant"),
                                ("natural", "true"),
                            ])),
                        );
                        set(ctx, p, st);
                        break;
                    }
                }
            }
            Deco::ShelfMushroom(prob) => {
                // vanilla 1.21.4+: place red/brown mushrooms on the north/east/
                // south/west faces of trunk logs; approximated deterministically
                for (i, &p) in logs.iter().enumerate() {
                    if i == 0 {
                        continue; // skip ground-level log
                    }
                    for &(d, face) in &[(2u8, "south"), (3, "north"), (4, "east"), (5, "west")] {
                        let _ = face;
                        if rng.next_f32() >= *prob {
                            continue;
                        }
                        let (ox, oz) = hoff(d);
                        let t = (p.0 + ox, p.1, p.2 + oz);
                        if ctx.is_air(t.0, t.1, t.2) {
                            let n = if rng.next_bool() {
                                "minecraft:brown_mushroom"
                            } else {
                                "minecraft:red_mushroom"
                            };
                            set(ctx, t, reg.state_of(n, None));
                        }
                    }
                }
            }
        }
    }
}
fn self_circle(
    ctx: &mut FC,
    rng: &mut Xor,
    bsp: &Bsp,
    p: P3,
    set: &mut impl FnMut(&mut FC, P3, u32),
) {
    for xx in -2..=2i32 {
        for zz in -2..=2i32 {
            if xx.abs() != 2 || zz.abs() != 2 {
                place_block_at(ctx, rng, bsp, (p.0 + xx, p.1, p.2 + zz), set);
            }
        }
    }
}
fn place_block_at(
    ctx: &mut FC,
    rng: &mut Xor,
    bsp: &Bsp,
    p: P3,
    set: &mut impl FnMut(&mut FC, P3, u32),
) {
    for dy in (-3..=2).rev() {
        let c = (p.0, p.1 + dy, p.2);
        let cur = ctx.get(c.0, c.1, c.2);
        if let Some(reg) = breg() {
            if reg.is_air(cur) || reg.in_tag(cur, "minecraft:replaceable_by_trees") {
                let st = bsp.get(ctx, rng, c.0, c.1, c.2);
                set(ctx, c, st);
                break;
            }
            if !reg.is_air(cur) && dy < 0 {
                break;
            }
        }
    }
}

// ---------- mangrove root placer ----------

pub(crate) struct RootP {
    pub trunk_off_y: IP,
    pub root_bsp: Bsp,
    pub above_chance: Option<f32>,
    pub above_bsp: Option<Bsp>,
    pub grow_through: Vec<String>, // #tag or names
    pub muddy_in: Vec<String>,
    pub muddy_bsp: Bsp,
    pub max_w: i32,
    pub max_l: i32,
    pub skew: f32,
}
impl RootP {
    fn can_through(&self, ctx: &mut FC, p: P3) -> bool {
        let Some(reg) = breg() else { return false };
        let cur = ctx.get(p.0, p.1, p.2);
        reg.is_air(cur)
            || reg.in_tag(cur, "minecraft:replaceable_by_trees")
            || self.grow_through.iter().any(|g| {
                if let Some(t) = g.strip_prefix('#') {
                    reg.in_tag(cur, t)
                } else {
                    *g == reg.name_of(cur)
                }
            })
    }
    #[expect(clippy::too_many_arguments)]
    fn can_grow(
        &self,
        ctx: &mut FC,
        rng: &mut Xor,
        p: P3,
        dir: u8,
        origin: P3,
        off: &mut Vec<P3>,
        len: i32,
    ) -> bool {
        let max = self.max_l;
        if len == max || off.len() > max as usize {
            return false;
        }
        for next in self.offshoots(ctx, rng, p, dir, origin) {
            if self.can_through(ctx, next) {
                off.push(next);
                if !self.can_grow(ctx, rng, next, dir, origin, off, len + 1) {
                    return false;
                }
            }
        }
        true
    }
    fn offshoots(&self, ctx: &mut FC, rng: &mut Xor, p: P3, dir: u8, origin: P3) -> Vec<P3> {
        let down = (p.0, p.1 - 1, p.2);
        let (ox, oz) = hoff(dir);
        let side = (p.0 + ox, p.1, p.2 + oz);
        let side_dn = (side.0, side.1 - 1, side.2);
        let dist = (p.0 - origin.0).abs() + (p.1 - origin.1).abs() + (p.2 - origin.2).abs();
        let _ = ctx;
        if dist > self.max_w - 3 && dist <= self.max_w {
            if rng.next_f32() < self.skew {
                vec![down, side_dn]
            } else {
                vec![down]
            }
        } else if dist > self.max_w || rng.next_f32() < self.skew {
            vec![down]
        } else if rng.next_bool() {
            vec![side]
        } else {
            vec![down]
        }
    }
    fn place_root(&self, ctx: &mut FC, rng: &mut Xor, p: P3) -> bool {
        let Some(reg) = breg() else { return false };
        let cur = ctx.get(p.0, p.1, p.2);
        let waterlogged = is_water_st(reg, cur);
        if self.muddy_in.iter().any(|m| *m == reg.name_of(cur)) {
            let mut st = self.muddy_bsp.get(ctx, rng, p.0, p.1, p.2);
            if waterlogged {
                st = reg.with_prop(st, "waterlogged", "true");
            }
            ctx.set(p.0, p.1, p.2, st);
            return true;
        }
        if !self.can_through(ctx, p) {
            return false;
        }
        let mut st = self.root_bsp.get(ctx, rng, p.0, p.1, p.2);
        if waterlogged {
            st = reg.with_prop(st, "waterlogged", "true");
        }
        ctx.set(p.0, p.1, p.2, st);
        if let (Some(c), Some(ab)) = (self.above_chance, self.above_bsp.as_ref()) {
            let ap = (p.0, p.1 + 1, p.2);
            if rng.next_f32() < c && ctx.is_air(ap.0, ap.1, ap.2) {
                let st = ab.get(ctx, rng, ap.0, ap.1, ap.2);
                ctx.set(ap.0, ap.1, ap.2, st);
            }
        }
        true
    }
    pub fn generate(&self, ctx: &mut FC, rng: &mut Xor, pos: P3, trunk_pos: P3) -> Option<Vec<P3>> {
        let mut roots = vec![];
        let mut w = pos;
        while w.1 < trunk_pos.1 {
            if !self.can_through(ctx, w) {
                return None;
            }
            w.1 += 1;
        }
        roots.push((trunk_pos.0, trunk_pos.1 - 1, trunk_pos.2));
        // `for (Direction direction : Direction.Plane.HORIZONTAL)`: NORTH, EAST, SOUTH,
        // WEST (`ji$c.c` = [ji.c, ji.f, ji.d, ji.e]). `canGrow` draws, so this order is the
        // draw binding for the root branches.
        for d in plane_horizontal() {
            let (ox, oz) = hoff(d);
            let start = (trunk_pos.0 + ox, trunk_pos.1, trunk_pos.2 + oz);
            let mut off = vec![];
            if !self.can_grow(ctx, rng, start, d, trunk_pos, &mut off, 0) {
                return None;
            }
            roots.extend(off);
            roots.push(start);
        }
        let mut placed = vec![];
        for rp in roots {
            if self.place_root(ctx, rng, rp) {
                placed.push(rp);
            }
        }
        Some(placed)
    }
}

// ---------- tree top ----------

pub(crate) struct TreeExtra {
    pub min_clipped: Option<i32>,
    pub dirt_provider: Option<Box<Bsp>>,
    pub ignore_vines: bool,
    /// `force_dirt`: `TrunkPlacer.setDirtUnderneath` writes the dirt provider below the
    /// trunk when this is set *or* when the cell is not a log.
    pub force_dirt: bool,
    pub min_size: FS,
    pub decorators: Vec<Deco>,
    pub root_placer: Option<RootP>,
    pub trunk_bsp: Bsp,
    pub foliage_bsp: Bsp,
    pub foliage_placer: FP,
    pub foliage_radius: IP,
    pub foliage_offset: IP,
    pub trunk_placer: TrunkPl,
}

fn clip_allows_top(top: i32, height: i32, min_clipped: Option<i32>) -> bool {
    top >= height || min_clipped.is_some_and(|min| top >= min)
}

/// `TreeFeature.doPlace`'s vertical rejection (`edq.a(...)` in the pinned jar):
/// the lowest of the feature origin and root-adjusted trunk base must be above
/// `minBuildHeight`, and `max(originY,trunkY) + trunkHeight + 1` must not pass
/// `maxBuildHeight` (the exclusive top). The draws before this gate have already
/// happened in vanilla, so this helper is used only after they are consumed.
fn tree_vertical_span_ok(
    world_min_y: i32,
    world_height: i32,
    origin_y: i32,
    trunk_y: i32,
    height: i32,
) -> bool {
    let low = origin_y.min(trunk_y);
    let high = origin_y.max(trunk_y) + height + 1;
    low > world_min_y && high <= world_min_y + world_height
}

/// `Vec3i.hashCode` in the pinned version: `(getY() + getZ() * 31) * 31 + getX()`, with
/// Java's wrapping `int` arithmetic (`kh.hashCode`, read off the pinned jar).
fn java_blockpos_hash(p: P3) -> i32 {
    p.1.wrapping_add(p.2.wrapping_mul(31))
        .wrapping_mul(31)
        .wrapping_add(p.0)
}

/// The decorator's view of one of the tree's position lists, in the order vanilla hands it
/// over.
///
/// `TreeFeature.place` collects the trunk, foliage and root positions in `Set<BlockPos>`s
/// (`Sets.newHashSet()`), so duplicates collapse; `TreeDecorator.Context`'s constructor
/// copies each set into an `ObjectArrayList` - which walks the `HashSet` in **bucket
/// order** - and then sorts that list by `BlockPos::getY` with a stable sort. So the
/// primary key is y and the tie-break is `HashMap` bucket order, not insertion order.
///
/// This matters for every decorator that draws once per entry (`leave_vine`: four
/// `nextFloat` per leaf, `trunk_vine`: four `nextInt(3)` per log, `cocoa`/`beehive` walk
/// in list order): the draw values are the same in aggregate, but which position each
/// draw lands on is decided by this order. Getting it wrong changes which vines and
/// cocoa exist, and a vine in a later tree's `minimum_size` scan rejects that tree
/// (`ignore_vines` is false for jungle trees - measured: one wrongly ordered canopy put
/// a jungle tree's leaves 18 blocks above the reference's terrain and moved the next
/// `would_survive` filter to y=87).
pub(crate) fn decorator_list(v: &[P3]) -> Vec<P3> {
    if v.is_empty() {
        return Vec::new();
    }
    // java.util.HashSet: table starts at 16, load factor 0.75, `size > cap * 0.75` doubles.
    let mut seen = std::collections::HashSet::with_capacity(v.len());
    let mut unique = Vec::with_capacity(v.len());
    for &p in v {
        if seen.insert(p) {
            unique.push(p);
        }
    }
    let n = unique.len();
    let mut cap = 16usize;
    while n > cap * 3 / 4 {
        cap *= 2;
    }
    // Buckets in index order, each bucket in insertion order: Java 8's `HashMap.putVal`
    // appends to the tail and a resize preserves the relative order of both halves.
    // Treeification (>8 in one bucket, cap >= 64) cannot be reached by tree coordinates.
    let mut counts = vec![0usize; cap];
    let mask = cap - 1;
    for &p in &unique {
        let h = java_blockpos_hash(p);
        let spread = h ^ ((h as u32 >> 16) as i32);
        counts[(spread as usize) & mask] += 1;
    }
    let mut offsets = vec![0usize; cap];
    let mut total = 0usize;
    for i in 0..cap {
        offsets[i] = total;
        total += counts[i];
    }
    let mut out = vec![(0, 0, 0); n];
    for p in unique {
        let h = java_blockpos_hash(p);
        let spread = h ^ ((h as u32 >> 16) as i32);
        let b = (spread as usize) & mask;
        out[offsets[b]] = p;
        offsets[b] += 1;
    }
    out.sort_by_key(|p| p.1);
    out
}

pub(crate) fn gen_tree(ctx: &mut FC, rng: &mut Xor, pos: P3, t: &TreeExtra) -> bool {
    let height = t.trunk_placer.height(rng);
    // Vanilla TreeFeature.place draws the foliage height and radius immediately after the
    // trunk height, before the root placer and the trunk: foliageHeight(random, height),
    // then foliageRadius(random, height - foliageHeight). Drawing them after the trunk
    // (as this used to) shifted every later random value, which moved the placed tree.
    let foliage_h = t.foliage_placer.rand_height(rng, height);
    let base_h = height - foliage_h;
    let frad = t.foliage_placer.rand_radius(&t.foliage_radius, rng, base_h);
    let trunk_start = t
        .root_placer
        .as_ref()
        .map_or(pos, |p| (pos.0, pos.1 + p.trunk_off_y.get(rng), pos.2));
    if !tree_vertical_span_ok(
        ctx.g.world_min_y,
        ctx.g.world_height,
        pos.1,
        trunk_start.1,
        height,
    ) {
        return false;
    }
    // obstruction clipping: scan y 0..height+1 at size radius
    let Some(reg) = breg() else { return false };
    let tables = tree_tables(reg);
    let top = {
        let mut top = height;
        'outer: for y in 0..=height + 1 {
            let j = t.min_size.radius(height, y);
            for x in -j..=j {
                for z in -j..=j {
                    let cur = ctx.get(trunk_start.0 + x, trunk_start.1 + y, trunk_start.2 + z);
                    let ok = tables.is_replaceable_or_log(reg, cur)
                        && (t.ignore_vines || !tables.is_vine(reg, cur));
                    if ok {
                        continue;
                    }
                    top = y.saturating_sub(2);
                    break 'outer;
                }
            }
        }
        top
    };
    // `TreeFeature.place` accepts a clipped tree only when the config carries a
    // `min_clipped_height` that the clipped top still reaches; without one, any
    // obstruction in the min-size scan rejects the tree. This used to proceed
    // when `min_clipped` was absent, which consumed the first tree's foliage
    // draws instead of moving to the next count position.
    if !clip_allows_top(top, height, t.min_clipped) {
        return false;
    }
    let root_positions = match &t.root_placer {
        Some(rp) => match rp.generate(ctx, rng, pos, trunk_start) {
            Some(v) => v,
            None => return false,
        },
        None => vec![],
    };
    let trunk_st = t.trunk_bsp.get(ctx, rng, pos.0, pos.1, pos.2);
    let (nodes, logs) = t.trunk_placer.generate(
        ctx,
        rng,
        // Vanilla passes the clipped height through even when it is negative: a
        // config whose `min_clipped_height` is zero can accept an obstruction at
        // the first layer (`top == -1`). Clamping that value changed branch counts
        // and foliage-node heights for exactly those accepted trees.
        top,
        trunk_start,
        &t.dirt_provider,
        trunk_st,
        t,
    );
    let foliage_st = t.foliage_bsp.get(ctx, rng, pos.0, pos.1, pos.2);
    let mut foliage = Vec::with_capacity(64);
    for node in &nodes {
        let off = t.foliage_offset.get(rng);
        t.foliage_placer.generate_into(
            &mut foliage,
            off,
            &t.foliage_radius,
            ctx,
            rng,
            node,
            foliage_h,
            frad,
            foliage_st,
        );
    }
    if logs.is_empty() && foliage.is_empty() {
        return false;
    }
    // `TreeDecorator.Context`'s constructor orders the three lists: it copies each
    // `HashSet` into an `ObjectArrayList` (bucket order) and sorts that by `BlockPos::getY`
    // (stable). See `decorator_list`. The copy is what the decorators see; `update_leaves`
    // is handed the unsorted positions, as it is in vanilla.
    let mut decorations = Vec::new();
    if !t.decorators.is_empty() {
        let mut dec_logs: Option<Vec<P3>> = None;
        let mut dec_foliage: Option<Vec<P3>> = None;
        let mut dec_roots: Option<Vec<P3>> = None;
        for d in &t.decorators {
            // Fast probability gate for single-roll decorators so rejected rolls
            // skip `decorator_list` allocation/bucketing while consuming the exact
            // same RNG draw.
            match d {
                Deco::Beehive(prob) | Deco::Cocoa(prob) | Deco::Creaking(prob) => {
                    if logs.is_empty() || rng.next_f32() >= *prob {
                        continue;
                    }
                    let dl = dec_logs.get_or_insert_with(|| decorator_list(&logs));
                    let df = dec_foliage.get_or_insert_with(|| decorator_list(&foliage));
                    let dr = dec_roots.get_or_insert_with(|| decorator_list(&root_positions));
                    // Probability already passed (consume 0.0 < 2.0 via 2.0 to skip re-draw).
                    match d {
                        Deco::Beehive(_) => {
                            let hive_y = df.first().map_or_else(
                                || {
                                    let last_y = dl.last().map_or(0, |p| p.1);
                                    (dl[0].1 + 1 + rng.next_bound(3)).min(last_y)
                                },
                                |fl| (fl.1 - 1).max(dl[0].1 + 1),
                            );
                            let mut places = vec![];
                            for lp in dl.iter().filter(|p| p.1 == hive_y) {
                                for dir in [5u8, 4, 3] {
                                    let (ox, oz) = hoff(dir);
                                    places.push((lp.0 + ox, lp.1, lp.2 + oz));
                                }
                            }
                            // Vanilla's decorator shuffles candidate positions through a
                            // separate random source. Cloning here gives that shuffle the
                            // same deterministic input without consuming the feature RNG
                            // draws the following decorators/placements own.
                            let shuffle_seed =
                                (crate::generation_math::hash_block_pos(dl[0].0, hive_y, dl[0].2)
                                    as u64)
                                    ^ 0x9e37_79b9_7f4a_7c15;
                            let mut shuffle_rng = crate::generation_math::Xor::seed(shuffle_seed);
                            for i in (1..places.len()).rev() {
                                let j = shuffle_rng.next_bound(i as i32 + 1) as usize;
                                places.swap(i, j);
                            }
                            let nest = tables.bee_nest_south;
                            for p in places {
                                let front = (p.0, p.1, p.2 + 1);
                                if ctx.is_air(p.0, p.1, p.2)
                                    && ctx.is_air(front.0, front.1, front.2)
                                {
                                    decorations.push(p);
                                    ctx.set(p.0, p.1, p.2, nest);
                                    // Hive creation draws two or three bee occupants on
                                    // the feature RNG. Keep their initial occupation timers
                                    // beside the nest; dropping them would make the placed
                                    // block a permanently empty hive unlike vanilla.
                                    let bees = 2 + rng.next_bound(2);
                                    let occupants = (0..bees)
                                        .map(|_| GeneratedBeeOccupant {
                                            ticks_in_hive: rng.next_bound(20),
                                            min_ticks_in_hive: 20 + rng.next_bound(20),
                                        })
                                        .collect();
                                    stage_generated_beehive(GeneratedBeehive { pos: p, occupants });
                                    break;
                                }
                            }
                        }
                        Deco::Cocoa(_) => {
                            let ty = dl[0].1;
                            for lp in dl.iter().filter(|p| p.1 - ty <= 2) {
                                for f in plane_horizontal() {
                                    if rng.next_f32() <= 0.25 {
                                        let (ox, oz) = hoff(f);
                                        let cp = (lp.0 - ox, lp.1, lp.2 - oz);
                                        if ctx.is_air(cp.0, cp.1, cp.2) {
                                            let fdir = match f {
                                                3 => "south",
                                                4 => "west",
                                                5 => "east",
                                                _ => "north",
                                            };
                                            let st = reg.state_of(
                                                "minecraft:cocoa",
                                                Some(&mkprops(&[
                                                    ("age", &rng.next_bound(3).to_string()),
                                                    ("facing", fdir),
                                                ])),
                                            );
                                            decorations.push(cp);
                                            ctx.set(cp.0, cp.1, cp.2, st);
                                        }
                                    }
                                }
                            }
                        }
                        Deco::Creaking(_) => {
                            let _ = dr;
                            let mut shuffled = dl.clone();
                            for i in (1..shuffled.len()).rev() {
                                let j = rng.next_bound(i as i32 + 1) as usize;
                                shuffled.swap(i, j);
                            }
                            for p in shuffled {
                                let mut all_logs = true;
                                for dir in 0..6u8 {
                                    let (ox, oy, oz) = dir_off(dir);
                                    let n = ctx.get(p.0 + ox, p.1 + oy, p.2 + oz);
                                    if !tables.is_log(reg, n) {
                                        all_logs = false;
                                        break;
                                    }
                                }
                                if all_logs {
                                    let st = reg.state_of(
                                        "minecraft:creaking_heart",
                                        Some(&mkprops(&[
                                            ("axis", "y"),
                                            ("creaking_heart_state", "dormant"),
                                            ("natural", "true"),
                                        ])),
                                    );
                                    decorations.push(p);
                                    ctx.set(p.0, p.1, p.2, st);
                                    break;
                                }
                            }
                        }
                        _ => unreachable!(),
                    }
                }
                _ => {
                    let dl = dec_logs.get_or_insert_with(|| decorator_list(&logs));
                    let df = dec_foliage.get_or_insert_with(|| decorator_list(&foliage));
                    let dr = dec_roots.get_or_insert_with(|| decorator_list(&root_positions));
                    d.run(ctx, rng, dr, dl, df, &mut decorations);
                }
            }
        }
    }
    if let Some(shape) = update_leaves(ctx, &logs, &root_positions, &foliage, &decorations) {
        apply_edge_shape_updates(ctx, &shape);
    }
    true
}
/// Native TreeFeature.updateLeaves, not a global shortest-path relaxation.
/// Only this tree's trunk writes seed bucket zero. Roots and decorator writes
/// are already visited and extend the box used for the walk and edge updates.
/// Foreign logs/leaves are discovered through neighbours, not independently seeded.
struct LeafShape {
    min: P3,
    filled: Vec<bool>,
    ys: usize,
    zs: usize,
}

impl LeafShape {
    fn positions(&self) -> impl Iterator<Item = P3> + '_ {
        self.filled
            .iter()
            .enumerate()
            .filter(|&(_, &filled)| filled)
            .map(|(i, _)| {
                (
                    self.min.0 + (i / (self.ys * self.zs)) as i32,
                    self.min.1 + (i / self.zs % self.ys) as i32,
                    self.min.2 + (i % self.zs) as i32,
                )
            })
    }
}

fn update_leaves(
    ctx: &mut FC,
    logs: &[P3],
    roots: &[P3],
    foliage: &[P3],
    decorations: &[P3],
) -> Option<LeafShape> {
    if logs.is_empty() && foliage.is_empty() {
        return None;
    }
    let reg = breg()?;
    let tables = tree_tables(reg);
    let mut min = (i32::MAX, i32::MAX, i32::MAX);
    let mut max = (i32::MIN, i32::MIN, i32::MIN);
    for p in logs.iter().chain(roots).chain(foliage).chain(decorations) {
        min.0 = min.0.min(p.0);
        min.1 = min.1.min(p.1);
        min.2 = min.2.min(p.2);
        max.0 = max.0.max(p.0);
        max.1 = max.1.max(p.1);
        max.2 = max.2.max(p.2);
    }
    let (xs, ys, zs) = (
        (max.0 - min.0 + 1) as usize,
        (max.1 - min.1 + 1) as usize,
        (max.2 - min.2 + 1) as usize,
    );
    if xs == 0 || ys == 0 || zs == 0 || xs * ys * zs > 4_000_000 {
        return None;
    }
    let idx = |p: P3| -> Option<usize> {
        if p.0 < min.0 || p.0 > max.0 || p.1 < min.1 || p.1 > max.1 || p.2 < min.2 || p.2 > max.2 {
            return None;
        }
        Some(((p.0 - min.0) as usize * ys + (p.1 - min.1) as usize) * zs + (p.2 - min.2) as usize)
    };
    let mut visited = vec![false; xs * ys * zs];
    for &p in roots.iter().chain(decorations) {
        if let Some(i) = idx(p) {
            visited[i] = true;
        }
    }
    // Native first collects trunk writes in a HashSet, then adds that set to
    // bucket zero; the two insertion/iteration steps are separately observable.
    let mut trunks = LeafQueue::new();
    for &p in logs {
        trunks.insert(p);
    }
    let mut buckets: [LeafQueue; 7] = std::array::from_fn(|_| LeafQueue::new());
    while let Some(p) = trunks.pop() {
        buckets[0].insert(p);
    }
    let mut depth = 0;
    while depth < 7 {
        let Some(p) = buckets[depth].pop() else {
            depth += 1;
            continue;
        };
        let Some(i) = idx(p) else { continue };
        // Native does not skip an already visited popped entry: a position
        // can have been queued in multiple buckets and be written again.
        if depth != 0 {
            let st = ctx.get(p.0, p.1, p.2);
            ctx.set(p.0, p.1, p.2, tables.with_distance(reg, st, depth));
        }
        visited[i] = true;
        for dir in 0..6 {
            let (dx, dy, dz) = dir_off(dir);
            let n = (p.0 + dx, p.1 + dy, p.2 + dz);
            let Some(j) = idx(n) else { continue };
            if visited[j] {
                continue;
            }
            let st = ctx.get(n.0, n.1, n.2);
            let Some(d) = tables.distance_from_log(reg, st) else {
                continue;
            };
            let next = (d as usize).min(depth + 1);
            if next < 7 {
                buckets[next].insert(n);
                // This cursor changes inside the direction loop in native
                // bytecode; later neighbours use its new value.
                depth = depth.min(next);
            }
        }
    }
    Some(LeafShape {
        min,
        filled: visited,
        ys,
        zs,
    })
}

#[inline]
fn schedule_leaf_edge_fast(
    ctx: &mut FC,
    reg: &BReg,
    tables: &TreeTables,
    cell: P3,
    st: u32,
    neighbor: u32,
) {
    let si = st as usize;
    let Some(&distance) = tables.edge_leaf_dist.get(si) else {
        return;
    };
    if distance == 0 {
        return;
    }
    let next = tables.distance_from_log(reg, neighbor).unwrap_or(7) + 1;
    if next != 1 || distance != 1 {
        ctx.g
            .schedule_generated_leaf(cell, tables.edge_leaf_default[si]);
    }
}

thread_local! {
    /// Reusable padded `(xs+2)*(ys+2)*(zs+2)` state cache for `apply_edge_shape_updates`.
    /// Each boundary cell of a tree is shared by 2-6 exposed faces; caching its state
    /// (and updating on the rare vine write) avoids redundant `FC::get` HashMap lookups.
    static EDGE_STATE_CACHE: std::cell::RefCell<Vec<u32>> = const { std::cell::RefCell::new(Vec::new()) };
}

#[expect(clippy::too_many_arguments)]
#[inline]
fn handle_edge_face_cached(
    ctx: &mut FC,
    reg: &BReg,
    tables: &TreeTables,
    cache: &mut [u32],
    p_idx: usize,
    q_idx: usize,
    direction: u8,
    p: P3,
    q: P3,
) {
    let mut inside = cache[p_idx];
    if inside == u32::MAX {
        inside = ctx.get(p.0, p.1, p.2);
        cache[p_idx] = inside;
    }
    let mut outside = cache[q_idx];
    if outside == u32::MAX {
        outside = ctx.get(q.0, q.1, q.2);
        cache[q_idx] = outside;
    }
    schedule_leaf_edge_fast(ctx, reg, tables, p, inside, outside);
    schedule_leaf_edge_fast(ctx, reg, tables, q, outside, inside);
    if tables.is_vine(reg, inside) {
        if let Some(st) = vine_edge_state(ctx, reg, p, inside, direction) {
            ctx.set(p.0, p.1, p.2, st);
            cache[p_idx] = st;
        }
    }
    if tables.is_vine(reg, outside) {
        if let Some(st) = vine_edge_state(ctx, reg, q, outside, direction ^ 1) {
            ctx.set(q.0, q.1, q.2, st);
            cache[q_idx] = st;
        }
    }
}

/// `StructureTemplate.updateShapeAtEdge`'s face walk over `shape`'s filled bitmap: the three
/// transition scans native `DiscreteVoxelShape.forAllFaces` performs — z (outer x, inner y),
/// y (outer z, inner x), then x (outer y, inner z). `visit` gets `(direction, filled_index,
/// empty_index, filled_pos, empty_pos)`, where the indices address the `(xs+2)*(ys+2)*(zs+2)`
/// padded cache the caller keeps and the positions are world coordinates. The sequence is
/// exactly what `crate::voxel_faces::exposed_faces(shape.positions())` derives from the
/// occupied set; a unit test pins the two against each other.
fn for_each_dense_edge(
    shape: &LeafShape,
    xs: usize,
    mut visit: impl FnMut(u8, usize, usize, P3, P3),
) {
    let filled = &shape.filled;
    let (mx, my, mz) = shape.min;
    let (ys, zs) = (shape.ys, shape.zs);
    let yz = ys * zs;
    let (pys, pzs) = (ys + 2, zs + 2);
    let pyz = pys * pzs;
    for x in 0..xs {
        let wx = mx.wrapping_add(x as i32);
        let xbase = x * yz;
        let pxbase = (x + 1) * pyz;
        for y in 0..ys {
            let wy = my.wrapping_add(y as i32);
            let row = xbase + y * zs;
            let prow = pxbase + (y + 1) * pzs;
            let mut prev = false;
            for z in 0..=zs {
                let cur = z < zs && filled[row + z];
                if prev != cur {
                    let wz = mz.wrapping_add(z as i32);
                    let i_prev = prow + z;
                    let i_cur = prow + z + 1;
                    if prev {
                        visit(3, i_prev, i_cur, (wx, wy, wz - 1), (wx, wy, wz));
                    } else {
                        visit(2, i_cur, i_prev, (wx, wy, wz), (wx, wy, wz - 1));
                    }
                }
                prev = cur;
            }
        }
    }
    for z in 0..zs {
        let wz = mz.wrapping_add(z as i32);
        let pz = z + 1;
        for x in 0..xs {
            let wx = mx.wrapping_add(x as i32);
            let col = x * yz + z;
            let pcol = (x + 1) * pyz + pz;
            let mut prev = false;
            for y in 0..=ys {
                let cur = y < ys && filled[col + y * zs];
                if prev != cur {
                    let wy = my.wrapping_add(y as i32);
                    let i_prev = pcol + y * pzs;
                    let i_cur = i_prev + pzs;
                    if prev {
                        visit(1, i_prev, i_cur, (wx, wy - 1, wz), (wx, wy, wz));
                    } else {
                        visit(0, i_cur, i_prev, (wx, wy, wz), (wx, wy - 1, wz));
                    }
                }
                prev = cur;
            }
        }
    }
    for y in 0..ys {
        let wy = my.wrapping_add(y as i32);
        let yoff = y * zs;
        let pyoff = (y + 1) * pzs;
        for z in 0..zs {
            let wz = mz.wrapping_add(z as i32);
            let line = yoff + z;
            let pline = pyoff + (z + 1);
            let mut prev = false;
            for x in 0..=xs {
                let cur = x < xs && filled[x * yz + line];
                if prev != cur {
                    let wx = mx.wrapping_add(x as i32);
                    let i_prev = x * pyz + pline;
                    let i_cur = i_prev + pyz;
                    if prev {
                        visit(5, i_prev, i_cur, (wx - 1, wy, wz), (wx, wy, wz));
                    } else {
                        visit(4, i_cur, i_prev, (wx, wy, wz), (wx - 1, wy, wz));
                    }
                }
                prev = cur;
            }
        }
    }
}

/// `StructureTemplate.updateShapeAtEdge(level, 3, shape, x, y, z)`, which `TreeFeature.place`
/// runs over the shape `updateLeaves` returns, after the decorators.
///
/// Two rules live in it. `LeavesBlock.updateShape` only reserves a tick - it never writes the
/// state it was asked about - and `VineBlock.updateShape` is an immediate recompute that can
/// turn a vine into air. The leaf half was modelled here long before the vine half, which is
/// how a vine a trunk/leaf decorator had just placed could survive with nothing sturdy behind
/// it: `TreeFeature.place`'s obstruction scan counts a vine as an obstruction when the config
/// does not set `ignore_vines`, so the leftover cell moved the *next* tree's placement. Measured
/// on seed 4242 chunk (74,-41): with the half present the port's tree origins in that window
/// equal the native capture's, in order (109); without it, 19 of them move.
///
/// Native order, per visited face: read the inside and the outside state, update the inside
/// with the edge direction, write it, then update the outside with the opposite direction and
/// the inside cell's *new* state. `VineBlock.updateShape` ignores that argument, but the order
/// decides what the outside's recompute reads back out of the level - it sees the inside's new
/// state through `ctx.get`, because the write went into the same map.
fn apply_edge_shape_updates(ctx: &mut FC, shape: &LeafShape) {
    let Some(reg) = breg() else { return };
    let tables = tree_tables(reg);
    let (ys, zs) = (shape.ys, shape.zs);
    let yz = ys.saturating_mul(zs);
    if yz == 0 {
        return;
    }
    let xs = shape.filled.len() / yz;
    if shape.filled.len() <= 65_536 && xs * yz == shape.filled.len() {
        let (pys, pzs) = (ys + 2, zs + 2);
        let pyz = pys * pzs;
        let padded_len = (xs + 2) * pyz;
        let mut cache = EDGE_STATE_CACHE.with(|c| std::mem::take(&mut *c.borrow_mut()));
        cache.clear();
        cache.resize(padded_len, u32::MAX);
        for_each_dense_edge(shape, xs, |direction, p_idx, q_idx, p, q| {
            handle_edge_face_cached(ctx, reg, tables, &mut cache, p_idx, q_idx, direction, p, q);
        });
        EDGE_STATE_CACHE.with(|c| {
            *c.borrow_mut() = cache;
        });
        return;
    }
    for (direction, p, q) in crate::voxel_faces::exposed_faces(shape.positions()) {
        let inside = ctx.get(p.0, p.1, p.2);
        let outside = ctx.get(q.0, q.1, q.2);
        crate::generation_context::schedule_leaf_edge_state(ctx, reg, p, inside, outside);
        crate::generation_context::schedule_leaf_edge_state(ctx, reg, q, outside, inside);
        if let Some(st) = vine_edge_state(ctx, reg, p, inside, direction) {
            ctx.set(p.0, p.1, p.2, st);
        }
        if let Some(st) = vine_edge_state(ctx, reg, q, outside, direction ^ 1) {
            ctx.set(q.0, q.1, q.2, st);
        }
    }
}

/// `VineBlock`'s attachment properties by `Direction` index (declaration order), which is the
/// order the recompute below walks `Direction.Plane.HORIZONTAL` in.
const VINE_FACES: [&str; 6] = ["down", "up", "north", "south", "west", "east"];

/// `VineBlock.updateShape` (`dov.a(dtc, ji, dtc, dcx, jd, jd)`), reduced to the two things it
/// can do: recompute the six attachment flags and become air when none of them survives.
/// `Some(new_state)` only when the state actually changes - the native comparison is an
/// identity one on interned states.
///
/// From the pinned jar (`dov.class`, server.jar SHA-1 59353fb40c36d304f2035d51e7d6e6baa98dc05c):
/// the short circuit is `Direction.DOWN` (index 0, the first constant in the clinit), not `UP`;
/// `getUpdatedState` refreshes the `up` flag from the cell *above* (`jd.d()`) and keeps a
/// horizontal face when `pos.relative(d)` is face-sturdy toward the vine *or* the cell above is
/// a vine with that same face (the hanging-chain rule); and the air test counts five flags -
/// `{north, south, west, east, up}` (`dov.g` minus `DOWN`) - so a vine with only `up` survives.
/// That last rule is not `dov.p`, the four-face predicate the tick path uses.
fn vine_edge_state(
    ctx: &mut FC,
    reg: &crate::block_registry::BReg,
    cell: P3,
    st: u32,
    toward_neighbor: u8,
) -> Option<u32> {
    if reg.name_of(st) != "minecraft:vine" {
        return None;
    }
    // `VINE_EDGE_TRACE=<cx>,<cz>`: one line per decision, the same fields the native
    // `-Dvinechunk` VSHAPE trace prints, so the two sides diff decision by decision. A vine
    // the pass kept and a vine the pass never visited leave the same blocks.
    let traced = crate::trace_env::env_str!("VINE_EDGE_TRACE")
        .is_some_and(|want| want == "all" || want == format!("{},{}", cell.0 >> 4, cell.2 >> 4));
    if toward_neighbor == 0 {
        if traced {
            eprintln!(
                "VEDGE 0 {} {} {} before={st} after={st}",
                cell.0, cell.1, cell.2
            );
        }
        return None;
    }
    let above = (cell.0, cell.1 + 1, cell.2);
    let above_state = ctx.get(above.0, above.1, above.2);
    let mut cur = st;
    // `getUpdatedState`'s first step: an already-set `up` flag is re-asked, never added.
    if reg.get_prop(st, "up").as_deref() == Some("true") {
        let up = reg.supports_multiface(above_state, 0);
        cur = reg.with_prop(cur, "up", if up { "true" } else { "false" });
    }
    for d in [2u8, 3, 4, 5] {
        let prop = VINE_FACES[d as usize];
        if reg.get_prop(cur, prop).as_deref() != Some("true") {
            continue;
        }
        let (dx, dy, dz) = dir_off(d);
        let neighbour = ctx.get(cell.0 + dx, cell.1 + dy, cell.2 + dz);
        let mut attached = reg.supports_multiface(neighbour, d);
        if !attached {
            attached = reg.name_of(above_state) == "minecraft:vine"
                && reg.get_prop(above_state, prop).as_deref() == Some("true");
        }
        cur = reg.with_prop(cur, prop, if attached { "true" } else { "false" });
    }
    let keeps = [2u8, 3, 4, 5]
        .iter()
        .any(|&d| reg.get_prop(cur, VINE_FACES[d as usize]).as_deref() == Some("true"))
        || reg.get_prop(cur, "up").as_deref() == Some("true");
    let tables = tree_tables(reg);
    let out = if keeps { cur } else { tables.air_state };
    if traced {
        eprintln!(
            "VEDGE {} {} {} {} before={st} after={out}",
            toward_neighbor, cell.0, cell.1, cell.2
        );
    }
    (out != st).then_some(out)
}

// ---------- JSON parsing ----------

fn ip(j: &J) -> IP {
    p_ip(j).unwrap_or(IP::Const(0))
}
fn p_tp(j: &J) -> Option<TrunkPl> {
    let ty = jstr(j, "type")?;
    let ty = ty.strip_prefix("minecraft:").unwrap_or(&ty);
    let base = jint(j, "base_height", 0);
    let a = jint(j, "height_rand_a", 0);
    let b = jint(j, "height_rand_b", 0);
    let tp = match ty {
        "straight_trunk_placer" => TP::Straight,
        "forking_trunk_placer" => TP::Forking,
        "giant_trunk_placer" => TP::Giant,
        "mega_jungle_trunk_placer" => TP::MegaJungle,
        "dark_oak_trunk_placer" => TP::DarkOak,
        "fancy_trunk_placer" => TP::Fancy,
        "bending_trunk_placer" => TP::Bending {
            min_height_for_leaves: jint(j, "min_height_for_leaves", 1),
            bend_length: ip(js(j, "bend_length")?),
        },
        "upwards_branching_trunk_placer" => TP::UpwardsBranching {
            extra_steps: ip(js(j, "extra_branch_steps")?),
            branch_prob: jnum(j, "place_branch_per_log_probability").unwrap_or(0.0) as f32,
            extra_len: ip(js(j, "extra_branch_length")?),
            grow_through: match js(j, "can_grow_through") {
                Some(J::Arr(v)) => v.iter().map(|x| x.str().to_string()).collect(),
                Some(J::Str(s)) => vec![s.clone()],
                _ => vec![],
            },
        },
        "cherry_trunk_placer" => TP::Cherry {
            count: ip(js(j, "branch_count")?),
            horizontal: ip(js(j, "branch_horizontal_length")?),
            start_off: ip(js(j, "branch_start_offset_from_top")?),
            end_off: ip(js(j, "branch_end_offset_from_top")?),
        },
        "poplar_trunk_placer" => TP::Poplar {
            above_branches: ip(js(j, "trunk_height_above_branches")?),
            branch_amount: ip(js(j, "branch_amount")?),
        },
        _ => return None,
    };
    Some(TrunkPl { base, a, b, ty: tp })
}
fn p_fp(j: &J) -> Option<(FP, IP, IP)> {
    let ty = jstr(j, "type")?;
    let ty = ty.strip_prefix("minecraft:").unwrap_or(&ty);
    let radius = ip(js(j, "radius").unwrap_or(&J::Num(0.0)));
    let offset = ip(js(j, "offset").unwrap_or(&J::Num(0.0)));
    let fp = match ty {
        "blob_foliage_placer" => FP::Blob {
            height: jint(j, "height", 3),
        },
        "spruce_foliage_placer" => FP::Spruce {
            trunk_h: ip(js(j, "trunk_height")?),
        },
        "pine_foliage_placer" => FP::Pine {
            height: ip(js(j, "height")?),
        },
        "acacia_foliage_placer" => FP::Acacia,
        "bush_foliage_placer" => FP::Bush {
            height: jint(j, "height", 2),
        },
        "fancy_foliage_placer" => FP::Fancy {
            height: jint(j, "height", 4),
        },
        "jungle_foliage_placer" => FP::Jungle {
            height: jint(j, "height", 2),
        },
        "mega_jungle_foliage_placer" => FP::MegaJungle {
            height: jint(j, "height", 10),
        },
        "mega_pine_foliage_placer" => FP::MegaPine {
            crown: ip(js(j, "crown_height")?),
        },
        "dark_oak_foliage_placer" => FP::DarkOak,
        "random_spread_foliage_placer" => FP::RandomSpread {
            fh: ip(js(j, "foliage_height")?),
            attempts: jint(j, "leaf_placement_attempts", 10),
        },
        "cherry_foliage_placer" => FP::Cherry {
            height: ip(js(j, "height")?),
            wide_hole: jnum(j, "wide_bottom_layer_hole_chance").unwrap_or(0.0) as f32,
            corner_hole: jnum(j, "corner_hole_chance").unwrap_or(0.0) as f32,
            hang: jnum(j, "hanging_leaves_chance").unwrap_or(0.0) as f32,
            hang_ext: jnum(j, "hanging_leaves_extension_chance").unwrap_or(0.0) as f32,
        },
        "poplar_foliage_placer" => FP::Poplar {
            height: ip(js(j, "height")?),
            side_hole: jnum(j, "side_hole_chance").unwrap_or(0.0) as f32,
        },
        _ => return None,
    };
    Some((fp, radius, offset))
}
fn p_fs(j: &J) -> Option<(FS, Option<i32>)> {
    let ty = jstr(j, "type")?;
    let ty = ty.strip_prefix("minecraft:").unwrap_or(&ty);
    let mch = jnum(j, "min_clipped_height").map(|v| v as i32);
    Some((
        match ty {
            "three_layers_feature_size" => FS::Three {
                limit: jint(j, "limit", 0),
                upper_limit: jint(j, "upper_limit", 0),
                lower: jint(j, "lower_size", 0),
                middle: jint(j, "middle_size", 0),
                upper: jint(j, "upper_size", 0),
            },
            _ => FS::Two {
                limit: jint(j, "limit", 0),
                lower: jint(j, "lower_size", 0),
                upper: jint(j, "upper_size", 0),
            },
        },
        mch,
    ))
}
fn p_deco(j: &J) -> Option<Deco> {
    let ty = jstr(j, "type")?;
    let ty = ty.strip_prefix("minecraft:").unwrap_or(&ty);
    Some(match ty {
        "trunk_vine" => Deco::TrunkVine,
        "leave_vine" | "leaves_vine" => {
            Deco::LeaveVine(jnum(j, "probability").unwrap_or(0.25) as f32)
        }
        "pale_moss" => Deco::PaleMoss {
            leaves: jnum(j, "leaves_probability").unwrap_or(0.0) as f32,
            trunk: jnum(j, "trunk_probability").unwrap_or(0.0) as f32,
            ground: jnum(j, "ground_probability").unwrap_or(0.0) as f32,
        },
        "creaking_heart" => Deco::Creaking(jnum(j, "probability").unwrap_or(0.0) as f32),
        "cocoa" => Deco::Cocoa(jnum(j, "probability").unwrap_or(0.0) as f32),
        "beehive" => Deco::Beehive(jnum(j, "probability").unwrap_or(0.0) as f32),
        "alter_ground" => Deco::AlterGround(p_bsp(js(j, "provider")?)?),
        "place_on_ground" => Deco::PlaceOnGround {
            tries: jint(j, "tries", 2),
            radius: jint(j, "radius", 2),
            height: jint(j, "height", 1),
            bsp: p_bsp(js(j, "block_state_provider")?)?,
        },
        "attached_to_leaves" => Deco::AttachedLeaves {
            bsp: p_bsp(js(j, "block_provider")?)?,
            dirs: match js(j, "directions") {
                Some(J::Arr(v)) => v
                    .iter()
                    .map(|x| match x.str() {
                        "down" => 0u8,
                        "up" => 1,
                        "north" => 2,
                        "south" => 3,
                        "west" => 4,
                        _ => 5,
                    })
                    .collect(),
                _ => vec![0],
            },
            excl_xz: jint(j, "exclusion_radius_xz", 0),
            excl_y: jint(j, "exclusion_radius_y", 0),
            prob: jnum(j, "probability").unwrap_or(0.0) as f32,
            req_empty: jint(j, "required_empty_blocks", 0),
        },
        "attached_to_logs" => Deco::AttachedLogs {
            prob: jnum(j, "probability").unwrap_or(0.0) as f32,
            bsp: p_bsp(js(j, "block_provider")?)?,
            dirs: match js(j, "directions") {
                Some(J::Arr(v)) => v
                    .iter()
                    .map(|x| match x.str() {
                        "down" => 0u8,
                        "up" => 1,
                        "north" => 2,
                        "south" => 3,
                        "west" => 4,
                        _ => 5,
                    })
                    .collect(),
                _ => vec![],
            },
        },
        "shelf_mushroom" => Deco::ShelfMushroom(jnum(j, "probability").unwrap_or(0.4) as f32),
        _ => return None,
    })
}
fn p_root(j: &J) -> Option<RootP> {
    if !jstr(j, "type")?.ends_with("mangrove_root_placer") {
        return None;
    }
    let mr = js(j, "mangrove_root_placement")?;
    let above = js(j, "above_root_placement");
    Some(RootP {
        trunk_off_y: ip(js(j, "trunk_offset_y")?),
        root_bsp: p_bsp(js(j, "root_provider")?)?,
        above_chance: above
            .and_then(|a| jnum(a, "above_root_placement_chance"))
            .map(|v| v as f32),
        above_bsp: above
            .and_then(|a| js(a, "above_root_provider"))
            .and_then(p_bsp),
        grow_through: match js(mr, "can_grow_through") {
            Some(J::Arr(v)) => v.iter().map(|x| x.str().to_string()).collect(),
            Some(J::Str(s)) => vec![s.clone()],
            _ => vec![],
        },
        muddy_in: match js(mr, "muddy_roots_in") {
            Some(J::Arr(v)) => v.iter().map(|x| x.str().to_string()).collect(),
            _ => vec![],
        },
        muddy_bsp: p_bsp(js(mr, "muddy_roots_provider")?)?,
        max_w: jint(mr, "max_root_width", 8),
        max_l: jint(mr, "max_root_length", 15),
        skew: jnum(mr, "random_skew_chance").unwrap_or(0.0) as f32,
    })
}

pub(crate) fn p_tree(j: &J) -> Option<TreeExtra> {
    let trunk_placer = p_tp(js(j, "trunk_placer")?)?;
    let (foliage_placer, foliage_radius, foliage_offset) = p_fp(js(j, "foliage_placer")?)?;
    let (min_size, mch) = p_fs(js(j, "minimum_size")?)?;
    let mut decorators = vec![];
    if let Some(ds) = js(j, "decorators").and_then(|d| d.as_arr()) {
        for d in ds {
            if let Some(x) = p_deco(d) {
                decorators.push(x);
            }
        }
    }
    Some(TreeExtra {
        min_clipped: mch,
        dirt_provider: js(j, "dirt_provider")
            .or_else(|| js(j, "below_trunk_provider"))
            .and_then(p_bsp)
            .map(Box::new),
        ignore_vines: jnum(j, "ignore_vines").unwrap_or(0.0) != 0.0
            || matches!(js(j, "ignore_vines"), Some(J::Str(s)) if s == "true"),
        force_dirt: jnum(j, "force_dirt").unwrap_or(0.0) != 0.0
            || matches!(js(j, "force_dirt"), Some(J::Str(s)) if s == "true"),
        min_size,
        decorators,
        root_placer: js(j, "root_placer").and_then(p_root),
        trunk_bsp: p_bsp(js(j, "trunk_provider")?)?,
        foliage_bsp: p_bsp(js(j, "foliage_provider")?)?,
        foliage_placer,
        foliage_radius,
        foliage_offset,
        trunk_placer,
    })
}

use crate::block_registry::{breg, mkprops};
use crate::generation_context::FC;
use crate::generation_providers::{dir_off, is_water_st, p_bsp, p_ip, Bsp, IP};
use crate::json::{jint, jnum, js, jstr};

#[cfg(all(test, megu_vanilla_data))]
mod tests {
    use super::*;

    fn state(name: &str) -> u32 {
        crate::name_to_state(name).unwrap_or_else(|| panic!("{name} must exist"))
    }

    #[test]
    fn leaf_updates_match_native_buckets_before_runtime_ticks() {
        fn point(s: &str) -> P3 {
            let a: Vec<i32> = s.split(',').map(|s| s.parse().unwrap()).collect();
            assert_eq!(a.len(), 3);
            (a[0], a[1], a[2])
        }
        fn positions(s: &str, origin: P3) -> Vec<P3> {
            s.split(';')
                .filter(|s| !s.is_empty())
                .map(|s| {
                    let p = point(s);
                    (p.0 + origin.0, p.1 + origin.1, p.2 + origin.2)
                })
                .collect()
        }
        let reg = breg().unwrap();
        let mut seen = std::collections::HashSet::new();
        for row in include_str!("../tests/data/leaf_distance_1211.tsv").lines() {
            let f: Vec<_> = row.split('\t').collect();
            assert_eq!(f.len(), 10);
            assert!(seen.insert((f[0], f[1])), "duplicate native vector");
            let origin = point(f[1]);
            let min = positions(f[2], origin)[0];
            let max = positions(f[3], origin)[0];
            let logs = positions(f[4], origin);
            let blocked = positions(f[5], origin);
            let mut g = crate::worldgen::Gen::load(0, crate::name_to_state).unwrap();
            for x in min.0..=max.0 {
                for y in min.1..=max.1 {
                    for z in min.2..=max.2 {
                        g.feat_write(x, y, z, 0);
                    }
                }
            }
            for cell in f[6].split(';') {
                let (pos, kind) = cell.split_once(':').unwrap();
                let p = positions(pos, origin)[0];
                let st = match kind {
                    "T" => reg.state_of("minecraft:oak_log", None),
                    "S" => reg.state_of("minecraft:stone", None),
                    _ if kind.starts_with('C') => reg.with_prop(
                        reg.state_of("minecraft:scaffolding", None),
                        "distance",
                        &kind[1..],
                    ),
                    _ => {
                        let mut st = reg.state_of("minecraft:oak_leaves", None);
                        st = reg.with_prop(st, "distance", &kind[1..]);
                        st = reg.with_prop(
                            st,
                            "persistent",
                            if kind.starts_with(['P', 'B']) {
                                "true"
                            } else {
                                "false"
                            },
                        );
                        reg.with_prop(
                            st,
                            "waterlogged",
                            if kind.starts_with(['W', 'B']) {
                                "true"
                            } else {
                                "false"
                            },
                        )
                    }
                };
                g.feat_write(p.0, p.1, p.2, st);
            }
            let mut ctx = FC::new(&mut g);
            // updateLeaves receives this exact box; the two foliage boundary
            // positions only define its extent and are not placements.
            let _ = update_leaves(&mut ctx, &logs, &blocked, &[min, max], &[]);
            for cell in f[7].split(';') {
                let (pos, kind) = cell.split_once(':').unwrap();
                let p = positions(pos, origin)[0];
                let st = ctx.get(p.0, p.1, p.2);
                assert_eq!(
                    reg.get_prop(st, "distance").as_deref(),
                    Some(&kind[1..]),
                    "native vector {} at {pos}",
                    f[0]
                );
                assert_eq!(
                    reg.get_prop(st, "persistent").as_deref(),
                    Some(if kind.starts_with(['P', 'B']) {
                        "true"
                    } else {
                        "false"
                    })
                );
                assert_eq!(
                    reg.get_prop(st, "waterlogged").as_deref(),
                    Some(if kind.starts_with(['W', 'B']) {
                        "true"
                    } else {
                        "false"
                    })
                );
            }
            assert_eq!(
                ctx.wr.len(),
                f[8].parse::<usize>().unwrap(),
                "native write coverage {}",
                f[0]
            );
        }
        assert_eq!(
            seen.len(),
            117,
            "native leaf vectors must not be empty/truncated"
        );
    }

    #[test]
    fn leaf_edge_schedules_match_native_order() {
        fn point(s: &str) -> P3 {
            let a: Vec<i32> = s.split(',').map(|v| v.parse().unwrap()).collect();
            assert_eq!(a.len(), 3);
            (a[0] - 1024, a[1] + 70, a[2] + 512)
        }
        fn points(s: &str) -> Vec<P3> {
            s.split(';').filter(|v| !v.is_empty()).map(point).collect()
        }
        let reg = breg().unwrap();
        let mut cases = 0;
        for row in include_str!("../tests/data/leaf_edge_1211.tsv").lines() {
            let f: Vec<_> = row.split('\t').collect();
            assert_eq!(f.len(), 9);
            let min = point(f[1]);
            let max = point(f[2]);
            let logs = points(f[3]);
            let blocked = points(f[4]);
            let mut g = crate::worldgen::Gen::load(0, crate::name_to_state).unwrap();
            for x in min.0 - 1..=max.0 + 1 {
                for y in min.1 - 1..=max.1 + 1 {
                    for z in min.2 - 1..=max.2 + 1 {
                        g.feat_write(x, y, z, 0);
                    }
                }
            }
            for cell in f[5].split(';') {
                let (pos, kind) = cell.split_once(':').unwrap();
                let p = point(pos);
                let st = match kind {
                    "T" => reg.state_of("minecraft:oak_log", None),
                    "S" => reg.state_of("minecraft:stone", None),
                    _ => {
                        let st = reg.with_prop(
                            reg.state_of("minecraft:oak_leaves", None),
                            "distance",
                            &kind[1..],
                        );
                        reg.with_prop(
                            st,
                            "persistent",
                            if kind.starts_with('P') {
                                "true"
                            } else {
                                "false"
                            },
                        )
                    }
                };
                g.feat_write(p.0, p.1, p.2, st);
            }
            let mut ctx = FC::new(&mut g);
            let shape = update_leaves(&mut ctx, &logs, &blocked, &[min, max], &[]).unwrap();
            apply_edge_shape_updates(&mut ctx, &shape);
            let got = ctx.g.generated_leaf_ticks();
            let want: Vec<_> = f[8]
                .split(';')
                .filter(|v| !v.is_empty())
                .map(|v| {
                    let (coord, rest) = v.split_once(':').unwrap();
                    assert_eq!(rest, "Block{minecraft:oak_leaves}:1");
                    (point(coord), reg.state_of("minecraft:oak_leaves", None))
                })
                .collect();
            assert_eq!(got, want, "native edge case {}", f[0]);
            cases += 1;
        }
        assert_eq!(cases, 39);
    }

    /// The vine half of `StructureTemplate.updateShapeAtEdge`, which `TreeFeature.place` runs
    /// over the shape `updateLeaves` returns.
    ///
    /// Every expectation is a `VineBlock.updateShape` fact read out of the pinned 1.21.1 jar
    /// (`dov.a(dtc,ji,dtc,dcx,jd,jd)` bytecode, `.cache` outside this repository), not a value
    /// the port produced: the `Direction.DOWN` short circuit, the `up` flag refreshed from the
    /// cell *above*, the four horizontal faces recomputed with the vine-above fallback, and air
    /// only when all five of `north/south/west/east/up` are false. Each case names the rule it
    /// pins; the first four are the ones the absent half got wrong.
    #[test]
    fn the_tree_edge_pass_applies_vine_update_shape() {
        let reg = breg().expect("the block registry must load from assets/");
        let log = reg.state_of("minecraft:oak_log", None);
        let stone = reg.state_of("minecraft:stone", None);
        let air = reg.state_of("minecraft:air", None);
        // An unwritten cell reads as state 0 in this test (`test_default_air`), so state 0 has
        // to be air for the expectations below to mean "this cell was left alone".
        assert_eq!(
            air, 0,
            "the default-air read in this test assumes air is state 0"
        );
        let vine = |faces: &[&str]| {
            let mut st = reg.state_of("minecraft:vine", None);
            for f in faces {
                st = reg.with_prop(st, f, "true");
            }
            st
        };
        // One shape cell: the pass visits exactly its six neighbours, and (2,1,0) is the one
        // the EAST face reaches (with the WEST direction, so the DOWN short circuit is not
        // what any of these cases is exercising by accident).
        let shape = LeafShape {
            min: (1, 1, 0),
            filled: vec![true],
            ys: 1,
            zs: 1,
        };
        let run = |world: &[((i32, i32, i32), u32)], read: (i32, i32, i32)| -> u32 {
            let mut g = crate::worldgen::Gen::load(0, crate::name_to_state).unwrap();
            g.feat_write(1, 1, 0, log);
            for (p, st) in world {
                g.feat_write(p.0, p.1, p.2, *st);
            }
            let mut ctx = FC::new(&mut g);
            ctx.test_default_air = true;
            apply_edge_shape_updates(&mut ctx, &shape);
            ctx.get(read.0, read.1, read.2)
        };
        // The vine under test is (2,1,0) unless a case says otherwise.
        let at = (2, 1, 0);
        // native: up unset, east has nothing behind it, no vine above -> all five false -> AIR
        assert_eq!(
            run(&[(at, vine(&["east"]))], at),
            air,
            "a vine with nothing behind it becomes air"
        );
        // control: a sturdy block behind the face keeps the vine exactly as it was
        assert_eq!(
            run(&[(at, vine(&["east"])), ((3, 1, 0), stone)], at),
            vine(&["east"]),
            "a supported face is kept"
        );
        // native counts `up` in the air test, so clearing the only horizontal face still keeps
        // a vine that hangs from the block above
        assert_eq!(
            run(&[(at, vine(&["up", "east"])), ((2, 2, 0), stone)], at),
            vine(&["up"]),
            "`up` survives the air test even with no horizontal face"
        );
        // native reads `up` from the cell ABOVE; a block below is not support
        assert_eq!(
            run(&[(at, vine(&["up"])), ((2, 0, 0), stone)], at),
            air,
            "`up` is refreshed from above, so a block below cannot hold the vine up"
        );
        // the chain rule also reads the cell ABOVE
        assert_eq!(
            run(&[(at, vine(&["north"])), ((2, 2, 0), vine(&["north"]))], at),
            vine(&["north"]),
            "a vine above carrying the same face keeps the face"
        );
        assert_eq!(
            run(&[(at, vine(&["north"])), ((2, 0, 0), vine(&["north"]))], at),
            air,
            "a vine below carrying the same face does not"
        );
        // the short circuit is DOWN, so the cell above the shape is never recomputed
        assert_eq!(
            run(&[((1, 2, 0), vine(&["east"]))], (1, 2, 0)),
            vine(&["east"]),
            "the DOWN edge short circuits and leaves the neighbour alone"
        );
    }

    #[test]
    fn the_centre_fancy_layer_is_half_the_trunk_half_height() {
        assert_eq!(fancy_branch_prob(14, 7), 3.5);
        assert_eq!(fancy_branch_prob(12, 6), 3.0);
    }

    #[test]
    fn a_fancy_branch_passes_through_replaceables_and_logs_only() {
        let reg = breg().expect("the block registry must load from assets/");
        assert!(fancy_branch_open(reg, state("oak_log")));
        assert!(fancy_branch_open(reg, state("oak_leaves")));
        assert!(fancy_branch_open(reg, 0));
        assert!(!fancy_branch_open(reg, state("stone")));
    }

    #[test]
    fn a_tree_reads_the_dirt_provider_field_the_pinned_data_uses() {
        let j = crate::json::jparse(include_bytes!(
            "../datapacks/vanilla/data/minecraft/worldgen/configured_feature/oak.json"
        ))
        .expect("the pinned oak config must parse");
        let config = js(&j, "config").expect("oak.json wraps its parameters in config");
        let tree = p_tree(config).expect("the pinned oak config must load");
        assert!(
            tree.dirt_provider.is_some(),
            "oak.json spells the trunk-base provider as dirt_provider"
        );
    }

    #[test]
    fn a_clipped_tree_needs_a_min_clipped_height_that_the_clip_reaches() {
        assert!(clip_allows_top(7, 7, None), "an unclipped tree is accepted");
        assert!(clip_allows_top(8, 7, None), "a taller clip is accepted");
        assert!(
            !clip_allows_top(6, 7, None),
            "an unclipped config must reject a clipped tree"
        );
        assert!(
            clip_allows_top(6, 7, Some(6)),
            "a min_clipped_height at the clip is accepted"
        );
        assert!(
            !clip_allows_top(5, 7, Some(6)),
            "a min_clipped_height above the clip is rejected"
        );
    }

    /// The three placers used to spell the same test out separately with a
    /// different tag each. The tag list is now the argument, so what has to be
    /// pinned is that each caller still asks for the tags it used to ask for and
    /// that the predicate actually reads them.
    #[test]
    fn the_replaceable_predicate_says_what_each_placer_used_to_say() {
        let reg = breg().expect("the block registry must load from assets/");
        let stone = state("stone");
        let log = state("oak_log");

        // Air is replaceable for every placer; stone is replaceable for none.
        for tags in [
            &["minecraft:leaves"][..],
            &["minecraft:replaceable_by_trees"][..],
            &["minecraft:replaceable_by_trees", "minecraft:logs"][..],
        ] {
            assert!(
                is_replaceable(reg, 0, tags),
                "air must be replaceable: {tags:?}"
            );
            assert!(
                !is_replaceable(reg, stone, tags),
                "stone must not be replaceable: {tags:?}"
            );
        }

        // The tag argument has to be read: a log is not replaceable by trees, and
        // is only replaceable where the placer names the logs tag.
        let by_trees = &["minecraft:replaceable_by_trees"][..];
        let or_log = &["minecraft:replaceable_by_trees", "minecraft:logs"][..];
        assert!(
            reg.in_tag(log, "minecraft:logs"),
            "the registry must know oak_log is a log"
        );
        assert!(
            !is_replaceable(reg, log, by_trees),
            "trees alone must not overwrite a log"
        );
        assert!(
            is_replaceable(reg, log, or_log),
            "a log must be replaceable when the placer names logs"
        );

        // The predicate is the disjunction it replaced, for every caller's tags.
        for tags in [
            &["minecraft:leaves"][..],
            &["minecraft:replaceable_by_trees"][..],
            &["minecraft:replaceable_by_trees", "minecraft:logs"][..],
        ] {
            for st in [0u32, stone, log, state("oak_leaves")] {
                let expected = reg.is_air(st) || tags.iter().any(|t| reg.in_tag(st, t));
                assert_eq!(
                    is_replaceable(reg, st, tags),
                    expected,
                    "state {st} tags {tags:?}"
                );
            }
        }
    }

    fn pinned_tree(name: &str) -> TreeExtra {
        let path =
            format!("datapacks/vanilla/data/minecraft/worldgen/configured_feature/{name}.json");
        let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
        let j = crate::json::jparse(&bytes).unwrap_or_else(|| panic!("{path}: invalid json"));
        assert_eq!(
            jstr(&j, "type").as_deref(),
            Some("minecraft:tree"),
            "{name} must be a vanilla tree configured feature"
        );
        p_tree(js(&j, "config").expect("tree config"))
            .unwrap_or_else(|| panic!("{name} must parse without dropping its tree shape"))
    }

    #[test]
    fn vanilla_1211_tree_species_parse_into_their_native_placers_and_decorators() {
        for (name, trunk, foliage) in [
            ("oak", "straight", "blob"),
            ("swamp_oak", "straight", "blob"),
            ("fancy_oak", "fancy", "fancy"),
            ("birch", "straight", "blob"),
            ("super_birch_bees_0002", "straight", "blob"),
            ("spruce", "straight", "spruce"),
            ("pine", "straight", "pine"),
            ("mega_spruce", "giant", "mega_pine"),
            ("mega_pine", "giant", "mega_pine"),
            ("jungle_tree", "straight", "blob"),
            ("mega_jungle_tree", "mega_jungle", "jungle"),
            ("acacia", "forking", "acacia"),
            ("dark_oak", "dark_oak", "dark_oak"),
            ("mangrove", "upwards_branching", "random_spread"),
            ("tall_mangrove", "upwards_branching", "random_spread"),
            ("cherry", "cherry", "cherry"),
            ("azalea_tree", "bending", "random_spread"),
        ] {
            let tree = pinned_tree(name);
            let trunk_ok = matches!(
                (trunk, &tree.trunk_placer.ty),
                ("straight", TP::Straight)
                    | ("fancy", TP::Fancy)
                    | ("giant", TP::Giant)
                    | ("mega_jungle", TP::MegaJungle)
                    | ("forking", TP::Forking)
                    | ("dark_oak", TP::DarkOak)
                    | ("upwards_branching", TP::UpwardsBranching { .. })
                    | ("cherry", TP::Cherry { .. })
                    | ("bending", TP::Bending { .. })
            );
            let foliage_ok = matches!(
                (foliage, &tree.foliage_placer),
                ("blob", FP::Blob { .. })
                    | ("fancy", FP::Fancy { .. })
                    | ("spruce", FP::Spruce { .. })
                    | ("pine", FP::Pine { .. })
                    | ("mega_pine", FP::MegaPine { .. })
                    | ("jungle", FP::Jungle { .. } | FP::MegaJungle { .. })
                    | ("acacia", FP::Acacia)
                    | ("dark_oak", FP::DarkOak)
                    | ("random_spread", FP::RandomSpread { .. })
                    | ("cherry", FP::Cherry { .. })
            );
            assert!(trunk_ok, "{name}: wrong trunk placer");
            assert!(foliage_ok, "{name}: wrong foliage placer");
        }

        for name in ["mega_spruce", "mega_pine"] {
            let tree = pinned_tree(name);
            assert!(
                tree.decorators
                    .iter()
                    .any(|d| matches!(d, Deco::AlterGround(_))),
                "{name}: podzol alter-ground decorator missing"
            );
        }
        let jungle = pinned_tree("jungle_tree");
        assert!(jungle
            .decorators
            .iter()
            .any(|d| matches!(d, Deco::Cocoa(_))));
        assert!(jungle
            .decorators
            .iter()
            .any(|d| matches!(d, Deco::TrunkVine)));
        assert!(jungle
            .decorators
            .iter()
            .any(|d| matches!(d, Deco::LeaveVine(_))));
        for name in ["mangrove", "tall_mangrove"] {
            let tree = pinned_tree(name);
            assert!(tree.root_placer.is_some(), "{name}: mangrove roots missing");
            assert!(
                tree.decorators
                    .iter()
                    .any(|d| matches!(d, Deco::AttachedLeaves { .. })),
                "{name}: hanging propagules missing"
            );
            let roots = tree.root_placer.as_ref().expect("mangrove root placer");
            assert!(
                matches!(roots.above_bsp.as_ref(), Some(Bsp::Simple(st)) if breg().is_some_and(|reg| reg.name_of(*st) == "minecraft:moss_carpet")),
                "{name}: moss carpet above-root provider missing"
            );
        }
        let cherry = pinned_tree("cherry_bees_005");
        assert!(
            cherry
                .decorators
                .iter()
                .any(|d| matches!(d, Deco::Beehive(p) if (*p - 0.05).abs() < f32::EPSILON)),
            "cherry_bees_005 must keep its 0.05 bee-nest chance"
        );
    }

    #[test]
    fn tree_vertical_span_uses_vanilla_exclusive_build_height() {
        assert!(tree_vertical_span_ok(-64, 384, 64, 64, 6));
        assert!(
            !tree_vertical_span_ok(-64, 384, -64, -64, 6),
            "the low endpoint must be above minBuildHeight"
        );
        assert!(
            !tree_vertical_span_ok(-64, 384, 319, 319, 2),
            "origin + height + 1 reaches exclusive maxBuildHeight"
        );
        assert!(
            tree_vertical_span_ok(-64, 384, 319, 319, -1),
            "a negative clipped height is not clamped before the span test"
        );
        assert!(
            !tree_vertical_span_ok(-64, 384, 70, 64, 250),
            "a root-adjusted lower trunk base still uses the upper origin for the top span"
        );
    }

    #[test]
    fn a_tree_at_the_build_bounds_returns_false_before_writing() {
        let tree = pinned_tree("oak");
        for y in [-64, 320] {
            let mut g = crate::worldgen::Gen::load(0, crate::name_to_state).unwrap();
            let mut ctx = FC::new(&mut g);
            let mut rng = Xor::seed_worldgen(0);
            assert!(
                !gen_tree(&mut ctx, &mut rng, (0, y, 0), &tree),
                "an oak rooted at y={y} must be rejected by the vanilla vertical span"
            );
            assert!(
                ctx.wr.is_empty(),
                "a vertically rejected tree must not write blocks"
            );
        }
    }

    /// `for_each_dense_edge` is the padded-cache half of `updateShapeAtEdge`'s face walk; the
    /// sparse fallback below it walks `voxel_faces::exposed_faces`. Both must emit the same
    /// `(direction, filled, empty)` sequence for the same shape or one of them stops being the
    /// native traversal. This samples filled bitmaps exhaustively enough to cover a single
    /// cell, cavities, disjoint blobs and shapes that touch every box face.
    #[test]
    fn the_dense_edge_walk_emits_the_same_faces_as_exposed_faces() {
        fn check(shape: &LeafShape, xs: usize) {
            let mut dense = Vec::new();
            for_each_dense_edge(shape, xs, |d, _, _, p, q| dense.push((d, p, q)));
            let sparse = crate::voxel_faces::exposed_faces(shape.positions());
            assert_eq!(
                dense.len(),
                sparse.len(),
                "min {:?}: dense walk emitted a different face count",
                shape.min
            );
            for (i, (a, b)) in dense.iter().zip(sparse.iter()).enumerate() {
                assert_eq!(a, b, "face {i} of shape at min {:?}", shape.min);
            }
        }
        let build = |min: P3, dims: (usize, usize, usize), cells: &[(usize, usize, usize)]| {
            let (xs, ys, zs) = dims;
            let mut filled = vec![false; xs * ys * zs];
            for &(x, y, z) in cells {
                filled[x * ys * zs + y * zs + z] = true;
            }
            (
                LeafShape {
                    min,
                    filled,
                    ys,
                    zs,
                },
                xs,
            )
        };
        // Single cell, two disjoint cells, a slab touching every face, a box with a cavity,
        // and an L through the corner.
        for (dims, cells) in [
            ((1, 1, 1), vec![(0, 0, 0)]),
            ((3, 2, 3), vec![(0, 0, 0), (2, 1, 2)]),
            (
                (4, 3, 5),
                (0..4)
                    .flat_map(|x| (0..5).map(move |z| (x, 1, z)))
                    .collect::<Vec<_>>(),
            ),
            (
                (3, 3, 3),
                (0..3)
                    .flat_map(|x| {
                        (0..3).flat_map(move |y| {
                            (0..3)
                                .filter(move |&z| !(x == 1 && y == 1 && z == 1))
                                .map(move |z| (x, y, z))
                        })
                    })
                    .collect::<Vec<_>>(),
            ),
            (
                (3, 3, 3),
                vec![(0, 0, 0), (1, 0, 0), (2, 0, 0), (0, 1, 0), (0, 0, 1)],
            ),
        ] {
            let (shape, xs) = build((-7, 60, 4), dims, &cells);
            check(&shape, xs);
        }
        // Deterministic pseudo-random masks: every fill pattern exercises the transition
        // logic somewhere, and the comparison against `exposed_faces` is order-exact.
        let mut seed = 0x9E3779B97F4A7C15u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for i in 0..512 {
            let (xs, ys, zs) = (
                1 + (next() % 4) as usize,
                1 + (next() % 3) as usize,
                1 + (next() % 4) as usize,
            );
            let mut filled = Vec::with_capacity(xs * ys * zs);
            for _ in 0..xs * ys * zs {
                filled.push(next() & 3 != 0);
            }
            if i % 64 == 0 {
                filled.iter_mut().for_each(|v| *v = true);
            }
            let min = (
                (next() % 100) as i32 - 50,
                (next() % 256) as i32 - 32,
                (next() % 100) as i32 - 50,
            );
            check(
                &LeafShape {
                    min,
                    filled,
                    ys,
                    zs,
                },
                xs,
            );
        }
    }
}
