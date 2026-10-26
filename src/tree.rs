// tree.rs — vanilla `tree` configured feature: trunk placers, foliage placers,
// feature-size clipping, decorators, mangrove root placer, and leaf-distance
// propagation.
//
// Type and feature names follow the vanilla `tree` configured-feature names in the
// datapack (`trunk_placer`, `foliage_placer`, `minimum_size`, `decorators`, ...).

use crate::block_registry::BReg;
use crate::generation_math::Xor;
use crate::json::J;

pub(crate) type P3 = (i32, i32, i32);

// ---------- helpers ----------

/// Vanilla placers all ask the same question - may this position be written? - and
/// answer it as air-or-in-tag. Which tag differs per placer, so it is the argument
/// rather than three copies of the test with a different tag baked into each. That
/// is how a placer ends up overwriting something it should not.
fn is_replaceable(reg: &BReg, cur: u32, tags: &[&str]) -> bool {
    reg.is_air(cur) || tags.iter().any(|t| reg.in_tag(cur, t))
}

fn set_if_can_replace(ctx: &mut FC, p: P3, st: u32) -> bool {
    let reg = match breg() {
        Some(r) => r,
        None => return false,
    };
    let cur = ctx.get(p.0, p.1, p.2);
    if is_replaceable(reg, cur, &["minecraft:replaceable_by_trees"]) {
        ctx.set(p.0, p.1, p.2, st);
        return true;
    }
    false
}
/// Place a trunk block where trees may replace the current block, recording it in
/// `logs` when it was placed.
fn place_log(ctx: &mut FC, logs: &mut Vec<P3>, p: P3, st: u32) -> bool {
    let placed = set_if_can_replace(ctx, p, st);
    if placed {
        logs.push(p);
    }
    placed
}
/// In-place shuffle driven by the generation RNG: walk from the last slot down and
/// swap each slot with one drawn uniformly from the slots up to and including it.
fn shuffle_with(rng: &mut Xor, items: &mut [P3]) {
    for top in (1..items.len()).rev() {
        let pick = rng.next_bound(top as i32 + 1) as usize;
        items.swap(top, pick);
    }
}
/// The 2D offsets of a square block, x-major.
fn square_offsets(lo: i32, hi: i32) -> impl Iterator<Item = (i32, i32)> {
    (lo..=hi).flat_map(move |a| (lo..=hi).map(move |b| (a, b)))
}
fn set_if_can_replace_or_log(ctx: &mut FC, p: P3, st: u32) -> bool {
    let reg = match breg() {
        Some(r) => r,
        None => return false,
    };
    let cur = ctx.get(p.0, p.1, p.2);
    if is_replaceable(
        reg,
        cur,
        &["minecraft:replaceable_by_trees", "minecraft:logs"],
    ) {
        ctx.set(p.0, p.1, p.2, st);
        return true;
    }
    false
}
fn is_air_or_leaves(ctx: &mut FC, p: P3) -> bool {
    match breg() {
        Some(reg) => {
            let cur = ctx.get(p.0, p.1, p.2);
            is_replaceable(reg, cur, &["minecraft:leaves"])
        }
        None => false,
    }
}
fn set_dirt(ctx: &mut FC, rng: &mut Xor, p: P3, provider: &Option<Box<BSP>>) {
    if let Some(bsp) = provider {
        let st = bsp.get(ctx, rng, p.0, p.1, p.2);
        ctx.set(p.0, p.1, p.2, st);
    }
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
    // horizontal worldgen order: [south, west, east, north]
    match rng.next_bound(4) {
        0 => 3, // south
        1 => 4, // west
        2 => 5, // east
        _ => 2, // north
    }
}
fn hoff(d: u8) -> (i32, i32) {
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
impl TreeNode {
    fn new(center: P3, foliage_radius: i32, giant: bool) -> Self {
        TreeNode {
            center,
            foliage_radius,
            giant,
        }
    }
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
    pub fn generate(
        &self,
        ctx: &mut FC,
        rng: &mut Xor,
        height: i32,
        start: P3,
        below_trunk: &Option<Box<BSP>>,
        trunk_st: u32,
        extra: &TreeExtra,
    ) -> (Vec<TreeNode>, Vec<P3>) {
        let mut nodes = vec![];
        let mut logs = vec![];
        match &self.ty {
            TP::Straight => {
                set_dirt(ctx, rng, (start.0, start.1 - 1, start.2), below_trunk);
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
                    set_dirt(ctx, rng, dp, below_trunk);
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
                    set_dirt(ctx, rng, dp, below_trunk);
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
                nodes.push(TreeNode::new((tx, ey, tz), 0, true));
                // Side limbs hang from the ring of cells around the 2x2 trunk core;
                // each ring cell draws once and grows a limb on a zero.
                let core = 0..=1;
                for (ox, oz) in square_offsets(-1, 2) {
                    if core.contains(&ox) && core.contains(&oz) {
                        continue;
                    }
                    if rng.next_bound(3) > 0 {
                        continue;
                    }
                    let limb_len = rng.next_bound(3) + 2;
                    for drop in 1..=limb_len {
                        let p = (start.0 + ox, ey - drop, start.2 + oz);
                        if set_if_can_replace_or_log(ctx, p, trunk_st) {
                            logs.push(p);
                        }
                    }
                    nodes.push(TreeNode::new((start.0 + ox, ey, start.2 + oz), 0, false));
                }
            }
            TP::Forking => {
                set_dirt(ctx, rng, (start.0, start.1 - 1, start.2), below_trunk);
                // A log at (column, row above the start), reporting whether it was placed.
                let mut lay = |col: (i32, i32), row: i32| {
                    place_log(ctx, &mut logs, (col.0, start.1 + row, col.1), trunk_st)
                };
                let main_lean = rand_horizontal(rng);
                let bend_row = height - rng.next_bound(4) - 1;
                let mut bend_left = 3 - rng.next_bound(3);
                let mut column = (start.0, start.2);
                let mut crown = None;
                for row in 0..height {
                    if row >= bend_row && bend_left > 0 {
                        let step = hoff(main_lean);
                        column = (column.0 + step.0, column.1 + step.1);
                        bend_left -= 1;
                    }
                    if lay(column, row) {
                        crown = Some(row + 1);
                    }
                }
                if let Some(top) = crown {
                    nodes.push(TreeNode::new((column.0, start.1 + top, column.1), 1, false));
                }
                let side_lean = rand_horizontal(rng);
                if side_lean != main_lean {
                    let first_row = bend_row - rng.next_bound(2) - 1;
                    let run_len = 1 + rng.next_bound(3);
                    // the draws above fix the run; rows below the ground row are skipped
                    // but still count against its length
                    let rows = first_row.max(1)..height.min(first_row + run_len);
                    let mut column = (start.0, start.2);
                    let mut crown = None;
                    for row in rows {
                        let step = hoff(side_lean);
                        column = (column.0 + step.0, column.1 + step.1);
                        if lay(column, row) {
                            crown = Some(row + 1);
                        }
                    }
                    if let Some(top) = crown {
                        nodes.push(TreeNode::new((column.0, start.1 + top, column.1), 0, false));
                    }
                }
            }
            TP::Fancy => {
                let tall = height + 2;
                let top_off = ((tall as f64) * 0.618).floor() as i32;
                set_dirt(ctx, rng, (start.0, start.1 - 1, start.2), below_trunk);
                let per_layer =
                    ((1.382 + (1.0 * tall as f64 / 13.0).powf(2.0)).floor() as i32).min(1);
                let ceiling = start.1 + top_off;
                let mut limbs: Vec<(P3, i32)> =
                    vec![((start.0, start.1 + top_off, start.2), ceiling)];
                for layer in (0..=(tall - 5)).rev() {
                    let reach = fancy_branch_prob(tall, layer);
                    if reach < 0.0 {
                        continue;
                    }
                    for _ in 0..per_layer {
                        let dist = (1.0f32 * reach * (rng.next_f32() + 0.328f32)) as f64;
                        let angle = (rng.next_f32() * 2.0 * std::f32::consts::PI) as f64;
                        let dx = dist * angle.sin() + 0.5;
                        let dz = dist * angle.cos() + 0.5;
                        let tip = (
                            start.0 + dx.floor() as i32,
                            start.1 + layer - 1,
                            start.2 + dz.floor() as i32,
                        );
                        let tip_top = (tip.0, tip.1 + 5, tip.2);
                        let (ok, nl) = fancy_branch(ctx, tip, tip_top, trunk_st, false);
                        logs.extend(nl);
                        if !ok {
                            continue;
                        }
                        let rx = start.0 - tip.0;
                        let rz = start.2 - tip.2;
                        let slope_y = tip.1 as f64 - ((rx * rx + rz * rz) as f64).sqrt() * 0.381;
                        let join_y = if slope_y > ceiling as f64 {
                            ceiling
                        } else {
                            slope_y as i32
                        };
                        let base = (start.0, join_y, start.2);
                        let (ok, nl) = fancy_branch(ctx, base, tip, trunk_st, false);
                        logs.extend(nl);
                        if !ok {
                            continue;
                        }
                        limbs.push((tip, join_y));
                    }
                }
                let (_, nl) = fancy_branch(
                    ctx,
                    start,
                    (start.0, start.1 + top_off, start.2),
                    trunk_st,
                    true,
                );
                logs.extend(nl);
                for (node_p, end_y) in &limbs {
                    let join = *end_y;
                    let tip = (start.0, join, start.2);
                    if tip == *node_p || !fancy_high(tall, join - start.1) {
                        continue;
                    }
                    let (_, nl) = fancy_branch(ctx, tip, *node_p, trunk_st, true);
                    logs.extend(nl);
                }
                for (node_p, end_y) in limbs {
                    if fancy_high(tall, end_y - start.1) {
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
                set_dirt(ctx, rng, (pos.0, pos.1 - 1, pos.2), below_trunk);
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
                set_dirt(ctx, rng, (start.0, start.1 - 1, start.2), below_trunk);
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
                let dirs = [(0i32, -1i32), (0, 1), (-1, 0), (1, 0)];
                let bd = dirs[rng.next_bound(4) as usize];
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
                set_dirt(ctx, rng, (start.0, start.1 - 1, start.2), below_trunk);
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
        let _ = extra;
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
        h2
    } else if g.abs() >= f {
        0.0
    } else {
        h2 * 0.5
    }
}
fn fancy_high(h: i32, y: i32) -> bool {
    y as f64 >= h as f64 * 0.2
}
fn fancy_branch(ctx: &mut FC, a: P3, b: P3, trunk_st: u32, make: bool) -> (bool, Vec<P3>) {
    let mut logs = vec![];
    if !make && a == b {
        return (true, logs);
    }
    let off = (b.0 - a.0, b.1 - a.1, b.2 - a.2);
    let i = off.0.abs().max(off.1.abs()).max(off.2.abs());
    if i == 0 {
        return (true, logs);
    }
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
        let reg = match breg() {
            Some(r) => r,
            None => return (false, logs),
        };
        let cur = ctx.get(p.0, p.1, p.2);
        if reg.is_air(cur)
            || reg.in_tag(cur, "minecraft:replaceable_by_trees")
            || reg.in_tag(cur, "minecraft:logs")
        {
            continue;
        }
        return (false, logs);
    }
    (true, logs)
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
    // A giant canopy is one block wider on the positive sides (2x2 trunks).
    let upper = radius + giant as i32;
    for (x, z) in square_offsets(-radius, upper) {
        if leave_pos_invalid(fp, rng, x, y, z, radius, giant) {
            continue;
        }
        let p = (center.0 + x, center.1 + y, center.2 + z);
        if set_if_can_replace(ctx, p, fst) {
            out.push(p);
        }
    }
}
fn is_leaf_set(ctx: &mut FC, p: P3) -> bool {
    if let Some(reg) = breg() {
        reg.in_tag(ctx.get(p.0, p.1, p.2), "minecraft:leaves")
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
    pub fn generate(
        &self,
        offset: i32,
        _placer_radius: &IP,
        ctx: &mut FC,
        rng: &mut Xor,
        node: &TreeNode,
        fh: i32,
        radius: i32,
        fst: u32,
    ) -> Vec<P3> {
        let mut out = vec![];
        match self {
            FP::Blob { .. } => {
                for y in (offset - fh..=offset).rev() {
                    let r = (radius + node.foliage_radius - 1 - y / 2).max(0);
                    gen_square(self, &mut out, ctx, rng, node.center, r, y, node.giant, fst);
                }
            }
            FP::Spruce { .. } => {
                let mut r = rng.next_bound(2);
                let mut max = 1;
                let mut next = 0;
                for y in (-fh..=offset).rev() {
                    gen_square(self, &mut out, ctx, rng, node.center, r, y, node.giant, fst);
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
                    gen_square(self, &mut out, ctx, rng, node.center, r, y, node.giant, fst);
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
                    &mut out,
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
                    &mut out,
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
                    &mut out,
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
                    gen_square(self, &mut out, ctx, rng, node.center, r, y, node.giant, fst);
                }
            }
            FP::Fancy { .. } => {
                for y in (offset - fh..=offset).rev() {
                    let r = radius + i32::from(!(y == offset || y == offset - fh));
                    gen_square(self, &mut out, ctx, rng, node.center, r, y, node.giant, fst);
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
                    gen_square(self, &mut out, ctx, rng, node.center, r, y, node.giant, fst);
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
                        &mut out,
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
                gen_square(
                    self,
                    &mut out,
                    ctx,
                    rng,
                    pos,
                    radius + 2,
                    -1,
                    node.giant,
                    fst,
                );
                if node.giant {
                    gen_square(
                        self,
                        &mut out,
                        ctx,
                        rng,
                        pos,
                        radius + 3,
                        0,
                        node.giant,
                        fst,
                    );
                    gen_square(
                        self,
                        &mut out,
                        ctx,
                        rng,
                        pos,
                        radius + 2,
                        1,
                        node.giant,
                        fst,
                    );
                    if rng.next_bool() {
                        gen_square(self, &mut out, ctx, rng, pos, radius, 2, node.giant, fst);
                    }
                } else {
                    gen_square(
                        self,
                        &mut out,
                        ctx,
                        rng,
                        pos,
                        radius + 1,
                        0,
                        node.giant,
                        fst,
                    );
                }
            }
            FP::RandomSpread { attempts, .. } => {
                for _ in 0..*attempts {
                    let p = (
                        node.center.0 + rng.next_bound(radius) - rng.next_bound(radius),
                        node.center.1 + rng.next_bound(fh) - rng.next_bound(fh),
                        node.center.2 + rng.next_bound(radius) - rng.next_bound(radius),
                    );
                    if set_if_can_replace(ctx, p, fst) {
                        out.push(p);
                    }
                }
            }
            FP::Cherry { hang, hang_ext, .. } => {
                let pos = (node.center.0, node.center.1 + offset, node.center.2);
                let cr = radius + node.foliage_radius - 1;
                gen_square(
                    self,
                    &mut out,
                    ctx,
                    rng,
                    pos,
                    cr - 2,
                    fh - 3,
                    node.giant,
                    fst,
                );
                gen_square(
                    self,
                    &mut out,
                    ctx,
                    rng,
                    pos,
                    cr - 1,
                    fh - 4,
                    node.giant,
                    fst,
                );
                for y in (0..=fh - 5).rev() {
                    gen_square(self, &mut out, ctx, rng, pos, cr, y, node.giant, fst);
                }
                hanging_square(
                    self, &mut out, ctx, rng, pos, cr, -1, node.giant, fst, *hang, *hang_ext,
                );
                hanging_square(
                    self,
                    &mut out,
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
                        &mut out, ctx, rng, pos, r, y, node.giant, fst, fh, flipped, *side_hole,
                    );
                }
                for y in (1..=fh - 4).rev() {
                    poplar_rhombus(
                        &mut out, ctx, rng, pos, cr, y, node.giant, fst, fh, flipped, *side_hole,
                    );
                }
                poplar_rhombus(
                    &mut out,
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
                    &mut out,
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
        out
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
            if set_if_can_replace(ctx, p, fst) {
                out.push(p);
            }
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
    AlterGround(BSP),
    PlaceOnGround {
        tries: i32,
        radius: i32,
        height: i32,
        bsp: BSP,
    },
    AttachedLeaves {
        bsp: BSP,
        dirs: Vec<u8>,
        excl_xz: i32,
        excl_y: i32,
        prob: f32,
        req_empty: i32,
    },
    AttachedLogs {
        prob: f32,
        bsp: BSP,
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
    fn run(&self, ctx: &mut FC, rng: &mut Xor, roots: &[P3], logs: &[P3], foliage: &[P3]) {
        let Some(reg) = breg() else { return };
        match self {
            Deco::TrunkVine => {
                for &p in logs {
                    for &(d, face) in &[(4u8, "east"), (5, "west"), (2, "south"), (3, "north")] {
                        if rng.next_bound(3) > 0 {
                            let (ox, oz) = hoff(d);
                            let t = (p.0 + ox, p.1, p.2 + oz);
                            if ctx.is_air(t.0, t.1, t.2) {
                                let base = reg.state_of("minecraft:vine", None);
                                ctx.set(t.0, t.1, t.2, reg.with_prop(base, face, "true"));
                            }
                        }
                    }
                }
            }
            Deco::LeaveVine(prob) => {
                for &p in foliage {
                    for &(d, face) in &[(4u8, "east"), (5, "west"), (2, "south"), (3, "north")] {
                        if rng.next_f32() < *prob {
                            let (ox, oz) = hoff(d);
                            let t = (p.0 + ox, p.1, p.2 + oz);
                            if ctx.is_air(t.0, t.1, t.2) {
                                let base = reg.state_of("minecraft:vine", None);
                                let st = reg.with_prop(base, face, "true");
                                ctx.set(t.0, t.1, t.2, st);
                                let mut cur = (t.0, t.1 - 1, t.2);
                                for _ in 0..4 {
                                    if !ctx.is_air(cur.0, cur.1, cur.2) {
                                        break;
                                    }
                                    ctx.set(cur.0, cur.1, cur.2, st);
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
                let nest =
                    reg.state_of("minecraft:bee_nest", Some(&mkprops(&[("facing", "south")])));
                for p in places {
                    let front = (p.0, p.1, p.2 + 1); // south
                    if ctx.is_air(p.0, p.1, p.2) && ctx.is_air(front.0, front.1, front.2) {
                        ctx.set(p.0, p.1, p.2, nest);
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
                    for f in [3u8, 4, 5, 2] {
                        // vanilla iterates horizontal dirs in order
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
                                ctx.set(cp.0, cp.1, cp.2, st);
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
                        self_circle(ctx, rng, bsp, (c.0, pos.1, c.1));
                    }
                    for _ in 0..5 {
                        let pl = rng.next_bound(64);
                        let (xx, zz) = (pl % 8, pl / 8);
                        if xx == 0 || xx == 7 || zz == 0 || zz == 7 {
                            self_circle(ctx, rng, bsp, (pos.0 - 3 + xx, pos.1, pos.2 - 3 + zz));
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
                        ctx.set(p.0, p.1, p.2, st);
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
                shuffle_with(rng, &mut shuffled);
                let mut blacklist: std::collections::HashSet<P3> = Default::default();
                for lp in shuffled {
                    let d = dirs[rng.next_bound(dirs.len() as i32) as usize];
                    let (ox, oy, oz) = dir_off(d);
                    let pp = (lp.0 + ox, lp.1 + oy, lp.2 + oz);
                    let mut ok = true;
                    for i in 1..=*req_empty {
                        let cp = (lp.0 + ox * i, lp.1 + oy * i, lp.2 + oz * i);
                        if !ctx.is_air(cp.0, cp.1, cp.2) {
                            ok = false;
                            break;
                        }
                    }
                    if !blacklist.contains(&pp) && ok && rng.next_f32() < *prob {
                        for dx in -excl_xz..=*excl_xz {
                            for dy in -excl_y..=*excl_y {
                                for dz in -excl_xz..=*excl_xz {
                                    blacklist.insert((pp.0 + dx, pp.1 + dy, pp.2 + dz));
                                }
                            }
                        }
                        let st = bsp.get(ctx, rng, pp.0, pp.1, pp.2);
                        ctx.set(pp.0, pp.1, pp.2, st);
                    }
                }
            }
            Deco::AttachedLogs { prob, bsp, dirs } => {
                if dirs.is_empty() || logs.is_empty() {
                    return;
                }
                let mut shuffled = logs.to_vec();
                shuffle_with(rng, &mut shuffled);
                for lp in shuffled {
                    let d = dirs[rng.next_bound(dirs.len() as i32) as usize];
                    let (ox, oy, oz) = dir_off(d);
                    let pp = (lp.0 + ox, lp.1 + oy, lp.2 + oz);
                    if rng.next_f32() <= *prob && ctx.is_air(pp.0, pp.1, pp.2) {
                        let st = bsp.get(ctx, rng, pp.0, pp.1, pp.2);
                        ctx.set(pp.0, pp.1, pp.2, st);
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
                let hanger = |ctx: &mut FC, rng: &mut Xor, mut p: P3| {
                    while ctx.is_air(p.0, p.1 - 1, p.2) && rng.next_f32() >= 0.5 {
                        ctx.set(p.0, p.1, p.2, moss);
                        p.1 -= 1;
                    }
                    if ctx.is_air(p.0, p.1, p.2) {
                        ctx.set(p.0, p.1, p.2, moss_tip);
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
                shuffle_with(rng, &mut shuffled);
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
                        ctx.set(p.0, p.1, p.2, st);
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
                            ctx.set(t.0, t.1, t.2, reg.state_of(n, None));
                        }
                    }
                }
            }
        }
    }
}
fn self_circle(ctx: &mut FC, rng: &mut Xor, bsp: &BSP, p: P3) {
    // 5x5 disc without its four corner cells.
    for (ox, oz) in square_offsets(-2, 2) {
        if ox.abs() == 2 && oz.abs() == 2 {
            continue;
        }
        place_block_at(ctx, rng, bsp, (p.0 + ox, p.1, p.2 + oz));
    }
}
fn place_block_at(ctx: &mut FC, rng: &mut Xor, bsp: &BSP, p: P3) {
    for dy in (-3..=2).rev() {
        let c = (p.0, p.1 + dy, p.2);
        let cur = ctx.get(c.0, c.1, c.2);
        if let Some(reg) = breg() {
            if reg.is_air(cur) || reg.in_tag(cur, "minecraft:replaceable_by_trees") {
                let st = bsp.get(ctx, rng, c.0, c.1, c.2);
                ctx.set(c.0, c.1, c.2, st);
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
    pub root_bsp: BSP,
    pub above_chance: Option<f32>,
    pub above_bsp: Option<BSP>,
    pub grow_through: Vec<String>, // #tag or names
    pub muddy_in: Vec<String>,
    pub muddy_bsp: BSP,
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
        for d in [3u8, 4, 5, 2] {
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
    pub below_trunk: Option<Box<BSP>>,
    pub ignore_vines: bool,
    pub min_size: FS,
    pub decorators: Vec<Deco>,
    pub root_placer: Option<RootP>,
    pub trunk_bsp: BSP,
    pub foliage_bsp: BSP,
    pub foliage_placer: FP,
    pub foliage_radius: IP,
    pub foliage_offset: IP,
    pub trunk_placer: TrunkPl,
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
    // obstruction clipping: scan y 0..height+1 at size radius
    let top = {
        let mut top = height;
        'outer: for y in 0..=height + 1 {
            let j = t.min_size.radius(height, y);
            for x in -j..=j {
                for z in -j..=j {
                    let p = (trunk_start.0 + x, trunk_start.1 + y, trunk_start.2 + z);
                    let ok = {
                        let Some(reg) = breg() else { return false };
                        let cur = ctx.get(p.0, p.1, p.2);
                        (reg.is_air(cur)
                            || reg.in_tag(cur, "minecraft:replaceable_by_trees")
                            || reg.in_tag(cur, "minecraft:logs"))
                            && (t.ignore_vines || reg.name_of(cur) != "minecraft:vine")
                    };
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
    if top < height {
        if let Some(mch) = t.min_clipped {
            if top < mch {
                return false;
            }
        }
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
        top.max(0),
        trunk_start,
        &t.below_trunk,
        trunk_st,
        t,
    );
    let foliage_st = t.foliage_bsp.get(ctx, rng, pos.0, pos.1, pos.2);
    let mut foliage = vec![];
    for node in &nodes {
        let off = t.foliage_offset.get(rng);
        foliage.extend(t.foliage_placer.generate(
            off,
            &t.foliage_radius,
            ctx,
            rng,
            node,
            foliage_h,
            frad,
            foliage_st,
        ));
    }
    if logs.is_empty() && foliage.is_empty() {
        return false;
    }
    for d in &t.decorators {
        d.run(ctx, rng, &root_positions, &logs, &foliage);
    }
    update_leaves(ctx, &logs, &root_positions, &foliage);
    true
}
/// BFS leaf `distance` propagation — vanilla TreeFeature.updateLeaves
fn update_leaves(ctx: &mut FC, logs: &[P3], roots: &[P3], foliage: &[P3]) {
    if logs.is_empty() && foliage.is_empty() {
        return;
    }
    let Some(reg) = breg() else { return };
    let mut min = (i32::MAX, i32::MAX, i32::MAX);
    let mut max = (i32::MIN, i32::MIN, i32::MIN);
    for p in logs.iter().chain(roots).chain(foliage) {
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
        return;
    }
    let idx = |p: P3| -> Option<usize> {
        if p.0 < min.0 || p.0 > max.0 || p.1 < min.1 || p.1 > max.1 || p.2 < min.2 || p.2 > max.2 {
            return None;
        }
        Some(((p.0 - min.0) as usize * ys + (p.1 - min.1) as usize) * zs + (p.2 - min.2) as usize)
    };
    // Deterministic multi-source shortest-path propagation (label correcting):
    // logs/roots seed 0; leaves seed their stored `distance` (fresh=7, foreign
    // trees may have written less); each cell converges to min distance, so the
    // result is independent of queue/iteration order (Rust HashSet order is
    // randomized per instance — vanilla's Java HashSet order is fixed).
    const KIND_BLOCK: u8 = 0; // non-log, non-leaf (includes roots)
    const KIND_LOG: u8 = 1;
    const KIND_LEAF: u8 = 2;
    let n = xs * ys * zs;
    let mut kind = vec![KIND_BLOCK; n];
    let mut best = vec![u16::MAX; n];
    let mut queue: std::collections::VecDeque<(usize, u16)> = std::collections::VecDeque::new();
    for y in min.1..=max.1 {
        for x in min.0..=max.0 {
            for z in min.2..=max.2 {
                let Some(i) = idx((x, y, z)) else { continue };
                let st = ctx.get(x, y, z);
                if reg.in_tag(st, "minecraft:logs") {
                    kind[i] = KIND_LOG;
                    if best[i] > 0 {
                        best[i] = 0;
                        queue.push_back((i, 0));
                    }
                } else if reg.in_tag(st, "minecraft:leaves") {
                    kind[i] = KIND_LEAF;
                    let dd = reg
                        .get_prop(st, "distance")
                        .and_then(|v| v.parse::<u16>().ok())
                        .unwrap_or(7);
                    if dd < best[i] {
                        best[i] = dd;
                        queue.push_back((i, dd));
                    }
                }
            }
        }
    }
    for p in roots {
        if let Some(i) = idx(*p) {
            kind[i] = KIND_BLOCK;
            best[i] = u16::MAX;
        }
    }
    while let Some((i, d)) = queue.pop_front() {
        if d > best[i] {
            continue; // stale entry — a shorter path already recorded
        }
        // cell center -> absolute pos
        let cz = min.2 + (i % zs) as i32;
        let cy = min.1 + ((i / zs) % ys) as i32;
        let cx = min.0 + (i / (zs * ys)) as i32;
        for di in 0..6u8 {
            let (ox, oy, oz) = dir_off(di);
            let np = (cx + ox, cy + oy, cz + oz);
            let Some(ni) = idx(np) else { continue };
            if kind[ni] == KIND_BLOCK {
                continue;
            }
            let nd = (d + 1).min(7);
            if nd < best[ni] {
                best[ni] = nd;
                queue.push_back((ni, nd));
            }
        }
    }
    // write back: leaves with a converged distance < stored
    for y in min.1..=max.1 {
        for x in min.0..=max.0 {
            for z in min.2..=max.2 {
                let Some(i) = idx((x, y, z)) else { continue };
                if kind[i] != KIND_LEAF {
                    continue;
                }
                let st = ctx.get(x, y, z);
                if !reg.has_prop(reg.name_of(st), "distance") {
                    continue;
                }
                let cur = reg
                    .get_prop(st, "distance")
                    .and_then(|v| v.parse::<u16>().ok())
                    .unwrap_or(7);
                if best[i] < cur && best[i] >= 1 {
                    ctx.set(x, y, z, reg.with_prop(st, "distance", &best[i].to_string()));
                }
            }
        }
    }
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
    if let Some(ds) = js(j, "decorators").and_then(|d| d.into_arr()) {
        for d in ds {
            if let Some(x) = p_deco(d) {
                decorators.push(x);
            }
        }
    }
    Some(TreeExtra {
        min_clipped: mch,
        below_trunk: js(j, "below_trunk_provider").and_then(p_bsp).map(Box::new),
        ignore_vines: jnum(j, "ignore_vines").unwrap_or(0.0) != 0.0
            || matches!(js(j, "ignore_vines"), Some(J::Str(s)) if s == "true"),
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
use crate::generation_providers::{dir_off, is_water_st, p_bsp, p_ip, BSP, IP};
use crate::json::{jint, jnum, js, jstr};

#[cfg(test)]
mod tests {
    use super::*;

    fn state(name: &str) -> u32 {
        crate::name_to_state(name).unwrap_or_else(|| panic!("{name} must exist"))
    }

    /// The three placers used to spell the same test out separately with a
    /// different tag each. The tag list is now the argument, so what has to be
    /// pinned is that each caller still asks for the tags it used to ask for and
    /// that the predicate actually reads them.
    #[test]
    fn the_replaceable_predicate_says_what_each_placer_used_to_say() {
        crate::test_support::require_vanilla_data();
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
}
