// feature.rs — datapack-driven placement and configured-feature execution for
// mc-spine. Loads placed/configured feature JSON and biome feature lists, runs
// the vanilla placement-modifier chain per chunk (3x3 neighborhood, population
// seeds) and writes block-state overrides into Gen::feat_c.
//
// The block registry, the placement providers/predicates and the generation
// context now live in block_registry.rs, generation_providers.rs and
// generation_context.rs; this module composes them and owns the tree/feature
// registry. State ids still come from assets/blocks_1211.json.

use crate::block_registry::*;
use crate::generation_context::FC;
use crate::generation_math::*;
use crate::generation_providers::*;
use crate::json::{jarr, jbool, jf32, jint, jnum, jstr, js, jparse, J};
use crate::worldgen::*;
use std::collections::{HashMap, HashSet};

#[cfg(test)]
#[path = "generation_contract_tests.rs"]
mod generation_contract_tests;

// ---------- configured features ----------

enum FRef {
    Name(String),
    Inline(Box<Cf>),
}
struct Pf {
    feat: FRef,
    mods: Vec<PM>,
    name: String,
}

enum Cf {
    Ore { size: i32, discard: f32, targets: Vec<(RT, u32)>, scattered: bool },
    Disk { half: i32, radius: IP, bsp: BSP, target: Pr },
    SimpleBlock { to: BSP, on: Vec<String>, in_: Vec<String>, under: Vec<String> },
    Spring { fluid: u32, req_below: bool, rock: i32, hole: i32, valid: HashSet<String> },
    RandSel(Vec<(Pf, f32)>, Option<Box<Pf>>),
    SimpleRand(Vec<Pf>),
    WeightedSel(Vec<(Pf, i32)>),
    BoolSel(Pf, Pf),
    Seq(Vec<Pf>),
    VegPatch {
        depth: IP,
        extra_bottom: f32,
        extra_edge: f32,
        replaceable: String,
        ground: BSP,
        floor: bool,
        chance: f32,
        vfeat: Box<Pf>,
        xz: IP,
        vrange: i32,
        waterlog: bool,
    },
    BlockColumn { layers: Vec<(IP, BSP)>, down: bool, allowed: Option<Pr>, prio: bool },
    BlockPile(BSP),
    Lake { fluid: BSP, barrier: BSP },
    BlueIce,
    BlockBlob { state: u32 },
    UnderMagma { floor: i32, prob: f32, radius: i32 },
    FreezeTop,
    Multiface {
        name: String,
        on: HashSet<String>,
        ceiling: bool,
        wall: bool,
        floor: bool,
        search: i32,
    },
    Bamboo(f32),
    Vines,
    MonsterRoom,
    Fallen { trunk: BSP },
    Tree(Box<crate::tree::TreeExtra>),
    Unknown(#[expect(dead_code)] String),
}

fn name_list(j: &J, k: &str) -> Vec<String> {
    match js(j, k) {
        Some(J::Str(s)) => vec![ns_name(s)],
        Some(J::Arr(v)) => v.iter().map(|x| ns_name(x.str())).collect(),
        _ => vec![],
    }
}
fn pf_any(j: &J) -> Option<Pf> {
    match j {
        J::Str(s) => Some(Pf {
            feat: FRef::Name(ns_name(s)),
            mods: vec![],
            name: String::new(),
        }),
        J::Obj(_) => p_pf(j),
        _ => None,
    }
}
fn p_pf(j: &J) -> Option<Pf> {
    let fref = match js(j, "feature")? {
        J::Str(s) => FRef::Name(ns_name(s)),
        o @ J::Obj(_) => {
            if js(o, "feature").is_some() {
                // placed-feature-shaped inline dict: wrap with no modifiers
                return p_pf(o);
            }
            FRef::Inline(Box::new(p_cf(o)?))
        }
        _ => return None,
    };
    let mut mods = vec![];
    if let Some(ms) = jarr(j, "placement") {
        for m in ms {
            if let Some(pm) = p_mod(m) {
                mods.push(pm);
            }
        }
    }
    Some(Pf { feat: fref, mods, name: String::new() })
}

fn p_cf(j: &J) -> Option<Cf> {
    let ty = jstr(j, "type")?;
    let ty = ty.strip_prefix("minecraft:").unwrap_or(&ty);
    Some(match ty {
        "ore" | "scattered_ore" => {
            let mut targets = vec![];
            for t in jarr(j, "targets").map(|v| v.as_slice()).unwrap_or(&[]) {
                let rt = js(t, "target").and_then(p_rt)?;
                let (n, p) = js(t, "state").and_then(bs_parts)?;
                targets.push((rt, breg()?.state_of(&n, p)));
            }
            Cf::Ore {
                size: jint(j, "size", 0),
                discard: jf32(j, "discard_chance_on_air_exposure", 0.0),
                targets,
                scattered: ty == "scattered_ore",
            }
        }
        "disk" => Cf::Disk {
            half: jint(j, "half_height", 0),
            radius: p_ip(js(j, "radius")?)?,
            bsp: p_bsp(js(j, "state_provider")?)?,
            target: p_pred(js(j, "target")?)?,
        },
        "simple_block" => Cf::SimpleBlock {
            to: p_bsp(js(j, "to_place")?)?,
            on: name_list(j, "can_place_on"),
            in_: name_list(j, "can_place_in"),
            under: name_list(j, "can_place_under"),
        },
        "spring_feature" => Cf::Spring {
            fluid: {
                let (n, p) = js(j, "state").and_then(bs_parts)?;
                breg()?.state_of(&n, p)
            },
            req_below: jbool(j, "requires_block_below", true),
            rock: jint(j, "rock_count", 4),
            hole: jint(j, "hole_count", 1),
            valid: name_list(j, "valid_blocks").into_iter().collect(),
        },
        "random_selector" => {
            let mut v = vec![];
            for e in jarr(j, "features")? {
                let pf = js(e, "feature").and_then(pf_any)?;
                let c = jf32(e, "chance", 0.0);
                v.push((pf, c));
            }
            Cf::RandSel(v, js(j, "default").and_then(pf_any).map(Box::new))
        }
        "simple_random_selector" => {
            let mut v = vec![];
            for e in jarr(j, "features")? {
                if let Some(pf) = pf_any(e) {
                    v.push(pf);
                }
            }
            Cf::SimpleRand(v)
        }
        "weighted_random_selector" => {
            let mut v = vec![];
            for e in jarr(j, "features")? {
                let pf = js(e, "data").and_then(pf_any)?;
                v.push((pf, jint(e, "weight", 1)));
            }
            Cf::WeightedSel(v)
        }
        "random_boolean_selector" => Cf::BoolSel(
            p_pf(js(j, "feature_true")?)?,
            p_pf(js(j, "feature_false")?)?,
        ),
        "sequence" => {
            let mut v = vec![];
            for e in jarr(j, "features")? {
                if let Some(pf) = pf_any(e) {
                    v.push(pf);
                }
            }
            Cf::Seq(v)
        }
        "vegetation_patch" | "waterlogged_vegetation_patch" => Cf::VegPatch {
            depth: p_ip(js(j, "depth")?)?,
            extra_bottom: jf32(j, "extra_bottom_block_chance", 0.0),
            extra_edge: jf32(j, "extra_edge_column_chance", 0.0),
            replaceable: jstr(j, "replaceable")
                .unwrap_or_default()
                .trim_start_matches('#')
                .to_string(),
            ground: p_bsp(js(j, "ground_state")?)?,
            floor: jstr(j, "surface").as_deref() != Some("ceiling"),
            chance: jf32(j, "vegetation_chance", 0.0),
            vfeat: Box::new(p_pf(js(j, "vegetation_feature")?)?),
            xz: p_ip(js(j, "xz_radius")?)?,
            vrange: jint(j, "vertical_range", 1),
            waterlog: ty == "waterlogged_vegetation_patch",
        },
        "block_column" => {
            let mut layers = vec![];
            for e in jarr(j, "layers")? {
                layers.push((p_ip(js(e, "height")?)?, p_bsp(js(e, "contents")?)?));
            }
            Cf::BlockColumn {
                layers,
                down: jstr(j, "direction").as_deref() != Some("up"),
                allowed: js(j, "allowed_placement").and_then(p_pred),
                prio: jbool(j, "prioritize_tip", false),
            }
        }
        "block_pile" => Cf::BlockPile(p_bsp(js(j, "state_provider")?)?),
        "lake" => Cf::Lake {
            fluid: p_bsp(js(j, "fluid")?)?,
            barrier: p_bsp(js(j, "barrier")?)?,
        },
        "blue_ice" => Cf::BlueIce,
        "block_blob" | "forest_rock" => {
            let (n, p) = js(j, "state").and_then(bs_parts)?;
            Cf::BlockBlob { state: breg()?.state_of(&n, p) }
        }
        "underwater_magma" => Cf::UnderMagma {
            floor: jint(j, "floor_search_range", 5),
            prob: jf32(j, "placement_probability_per_valid_position", 0.5),
            radius: jint(j, "placement_radius_around_floor", 1),
        },
        "freeze_top_layer" => Cf::FreezeTop,
        "multiface_growth" => Cf::Multiface {
            name: ns_name(&jstr(j, "block")?),
            on: name_list(j, "can_be_placed_on").into_iter().collect(),
            ceiling: jbool(j, "can_place_on_ceiling", false),
            wall: jbool(j, "can_place_on_wall", false),
            floor: jbool(j, "can_place_on_floor", true),
            search: jint(j, "search_range", 10),
        },
        "bamboo" => Cf::Bamboo(jf32(j, "probability", 0.0)),
        "vines" => Cf::Vines,
        "monster_room" => Cf::MonsterRoom,
        "tree" => Cf::Tree(Box::new(crate::tree::p_tree(j)?)),
        "fallen_tree" => Cf::Fallen {
            trunk: p_bsp(js(j, "trunk_provider")?)?,
        },
        _ => Cf::Unknown(ty.to_string()),
    })
}

// ---------- feature registry + ordering ----------

pub(crate) struct FReg {
    cfg: HashMap<String, Cf>,
    placed: HashMap<String, Pf>,
    bsteps: HashMap<String, Vec<Vec<String>>>,
    ball: HashMap<String, HashSet<String>>,
    btemp: HashMap<String, f32>,
    order: Vec<Vec<(u32, String)>>,
    steps: usize,
}

// vanilla overworld biome order for feature-ordering ties (registry order)
const OW_BIOMES: &[&str] = &[
    "minecraft:plains", "minecraft:sunflower_plains", "minecraft:snowy_plains",
    "minecraft:ice_spikes", "minecraft:desert", "minecraft:swamp",
    "minecraft:mangrove_swamp", "minecraft:forest", "minecraft:flower_forest",
    "minecraft:birch_forest", "minecraft:dark_forest",
    "minecraft:old_growth_birch_forest", "minecraft:old_growth_pine_taiga",
    "minecraft:old_growth_spruce_taiga", "minecraft:taiga", "minecraft:snowy_taiga",
    "minecraft:savanna", "minecraft:savanna_plateau", "minecraft:windswept_hills",
    "minecraft:windswept_gravelly_hills", "minecraft:windswept_forest",
    "minecraft:windswept_savanna", "minecraft:jungle", "minecraft:sparse_jungle",
    "minecraft:bamboo_jungle", "minecraft:badlands", "minecraft:eroded_badlands",
    "minecraft:wooded_badlands", "minecraft:meadow", "minecraft:cherry_grove",
    "minecraft:grove", "minecraft:snowy_slopes", "minecraft:frozen_peaks",
    "minecraft:jagged_peaks", "minecraft:stony_peaks", "minecraft:river",
    "minecraft:frozen_river", "minecraft:beach", "minecraft:snowy_beach",
    "minecraft:stony_shore", "minecraft:warm_ocean", "minecraft:lukewarm_ocean",
    "minecraft:deep_lukewarm_ocean", "minecraft:ocean", "minecraft:deep_ocean",
    "minecraft:cold_ocean", "minecraft:deep_cold_ocean", "minecraft:frozen_ocean",
    "minecraft:deep_frozen_ocean", "minecraft:mushroom_fields",
    "minecraft:dripstone_caves", "minecraft:lush_caves", "minecraft:deep_dark",
    "minecraft:the_void", "minecraft:small_end_islands", "minecraft:end_midlands",
    "minecraft:end_highlands", "minecraft:end_barrens", "minecraft:the_end",
    "minecraft:nether_wastes", "minecraft:soul_sand_valley",
    "minecraft:crimson_forest", "minecraft:warped_forest", "minecraft:basalt_deltas",
];

/// Vanilla's DFS (obf `ayd.a`): post-order visit over the feature edges, neighbours in
/// (step, featureIndex) order. A cycle is a data error, so it stops rather than recursing forever.
fn dfs_step(
    v: (usize, u32),
    edges: &HashMap<(usize, u32), Vec<(usize, u32)>>,
    visited: &mut std::collections::HashSet<(usize, u32)>,
    in_progress: &mut std::collections::HashSet<(usize, u32)>,
    order: &mut Vec<(usize, u32)>,
) {
    if visited.contains(&v) || in_progress.contains(&v) {
        return;
    }
    in_progress.insert(v);
    if let Some(neigh) = edges.get(&v) {
        let mut sorted = neigh.clone();
        sorted.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
        for m in sorted {
            dfs_step(m, edges, visited, in_progress, order);
        }
    }
    in_progress.remove(&v);
    visited.insert(v);
    order.push(v);
}

fn load_freg() -> FReg {
    let mut reg = FReg {
        cfg: HashMap::new(),
        placed: HashMap::new(),
        bsteps: HashMap::new(),
        ball: HashMap::new(),
        btemp: HashMap::new(),
        order: vec![],
        steps: 0,
    };
    for (name, b) in Gen::dp_files("feature") {
        if let Some(j) = jparse(&b) {
            if let Some(cf) = p_cf(&j) {
                reg.cfg.insert(name, cf);
            }
        }
    }
    for (name, b) in Gen::dp_files("placed_feature") {
        if let Some(j) = jparse(&b) {
            if let Some(mut pf) = p_pf(&j) {
                pf.name = name.clone();
                reg.placed.insert(name, pf);
            }
        }
    }
    let mut max_steps = 0usize;
    for (name, b) in Gen::dp_files("biome") {
        if let Some(j) = jparse(&b) {
            if let Some(t) = jnum(&j, "temperature") {
                reg.btemp.insert(name.clone(), t as f32);
            }
            let mut steps: Vec<Vec<String>> = vec![];
            if let Some(fs) = jarr(&j, "features") {
                for s in fs {
                    let mut step = vec![];
                    if let Some(arr) = s.into_arr() {
                        for v in arr {
                            step.push(ns_name(v.str()));
                        }
                    }
                    steps.push(step);
                }
            }
            max_steps = max_steps.max(steps.len());
            reg.ball
                .insert(name.clone(), steps.iter().flatten().cloned().collect());
            reg.bsteps.insert(name, steps);
        }
    }
    // Vanilla FeatureSorter.buildFeaturesPerStep (obf `deg.a`): a global first-seen index per
    // feature, cross-step edges over each biome's flattened feature list, a DFS post-order
    // reversed under a step-then-index comparator, then grouped by step. The old Kahn FIFO
    // order differed, which moved every placed feature's RNG offset (one extra tree at
    // chunk (0,0)) and every sky-spill column it touched.
    let mut fidx: HashMap<String, u32> = HashMap::new();
    let mut counter = 0u32;
    for b in OW_BIOMES {
        let Some(steps) = reg.bsteps.get(*b) else { continue };
        for list in steps {
            for f in list {
                if !fidx.contains_key(f) {
                    fidx.insert(f.clone(), counter);
                    counter += 1;
                }
            }
        }
    }
    // A node is (step, featureIndex); the name is kept alongside for the output.
    let mut name_of: HashMap<(usize, u32), String> = HashMap::new();
    let mut edges: HashMap<(usize, u32), Vec<(usize, u32)>> = HashMap::new();
    for b in OW_BIOMES {
        let Some(steps) = reg.bsteps.get(*b) else { continue };
        let mut flat: Vec<(usize, u32)> = vec![];
        for (i, list) in steps.iter().enumerate() {
            for f in list {
                if let Some(&fi) = fidx.get(f) {
                    name_of.entry((i, fi)).or_insert_with(|| f.clone());
                    flat.push((i, fi));
                }
            }
        }
        for w in flat.windows(2) {
            if w[0] != w[1] {
                edges.entry(w[0]).or_default().push(w[1]);
            }
        }
    }
    let mut order: Vec<(usize, u32)> = vec![];
    let mut visited: std::collections::HashSet<(usize, u32)> = std::collections::HashSet::new();
    let mut in_progress: std::collections::HashSet<(usize, u32)> = std::collections::HashSet::new();
    let mut keys: Vec<(usize, u32)> = edges.keys().copied().collect();
    keys.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
    for k in keys {
        dfs_step(k, &edges, &mut visited, &mut in_progress, &mut order);
    }
    order.reverse();
    let mut by_step: Vec<Vec<(u32, String)>> = vec![vec![]; max_steps];
    for step in 0..max_steps {
        for (gi, k) in order.iter().filter(|k| k.0 == step).enumerate() {
            if let Some(name) = name_of.get(k) {
                by_step[step].push((gi as u32, name.clone()));
            }
        }
    }
    reg.order = by_step;
    reg.steps = max_steps;
    reg
}

static FREG: std::sync::OnceLock<FReg> = std::sync::OnceLock::new();
fn freg() -> &'static FReg {
    FREG.get_or_init(load_freg)
}


fn foliage_noise() -> &'static OSimp {
    static N: std::sync::OnceLock<OSimp> = std::sync::OnceLock::new();
    N.get_or_init(|| OSimp::new_l(&mut Lcg::new(2345), &[0]))
}

// ---------- placed feature execution ----------

fn gen_pf(pf: &Pf, fr: &'static FReg, ctx: &mut FC, rng: &mut Xor, pos: (i32, i32, i32)) {
    // Modifiers are streams of positions, not passes over a list. Vanilla builds
    // `Stream.of(origin).flatMap(modifier::getPositions)` for each modifier in turn, and the
    // terminal operation places the feature as each position is pulled. So a position produced by
    // `count` goes through `in_square`, `height_range` and the feature before the next position is
    // produced, and every draw lands in that order on the shared RandomSource.
    //
    // Applying one modifier to all positions before the next modifier is not equivalent: with
    // `count: 2` this server drew x1,z1,x2,z2,y1,y2 while vanilla draws x1,z1,y1,x2,z2,y2. The
    // vein origins then differ for every multi-position feature - `ore_andesite_lower` in chunk
    // (0,0) put its veins at y=15 and y=18 instead of y=49 and y=18 - which is why no step index
    // reproduced vanilla's ore cells.
    apply_modifiers(pf.mods.as_slice(), pf, fr, ctx, rng, pos);
}

fn apply_modifiers(
    mods: &[PM],
    pf: &Pf,
    fr: &'static FReg,
    ctx: &mut FC,
    rng: &mut Xor,
    pos: (i32, i32, i32),
) {
    let Some((m, rest)) = mods.split_first() else {
        gen_cf_ref(&pf.feat, fr, ctx, rng, pos);
        return;
    };
    let mut out = Vec::new();
    mod_positions(m, fr, ctx, rng, pos, pf, &mut out);
    for p in out {
        apply_modifiers(rest, pf, fr, ctx, rng, p);
    }
}

fn gen_cf_ref(fref: &FRef, fr: &'static FReg, ctx: &mut FC, rng: &mut Xor, pos: (i32, i32, i32)) {
    match fref {
        FRef::Name(n) => {
            if let Some(cf) = fr.cfg.get(n) {
                gen_cf(cf, fr, ctx, rng, pos);
            }
        }
        FRef::Inline(cf) => gen_cf(cf, fr, ctx, rng, pos),
    }
}

fn mod_positions(
    m: &PM,
    fr: &'static FReg,
    ctx: &mut FC,
    rng: &mut Xor,
    pos: (i32, i32, i32),
    pf: &Pf,
    out: &mut Vec<(i32, i32, i32)>,
) {
    match m {
        PM::Count(ip) => {
            for _ in 0..ip.get(rng).max(0) {
                out.push(pos);
            }
        }
        PM::CountEveryLayer(ip) => {
            let n = ip.get(rng).max(0);
            let mut layer = 0i32;
            loop {
                let mut any = false;
                for _ in 0..n {
                    let x = rng.next_bound(16) + pos.0;
                    let z = rng.next_bound(16) + pos.2;
                    let mut y = ctx.hm_oc(x, z).min(ctx.g.min_y + ctx.g.height - 1);
                    let mut found = 0;
                    let mut cur = ctx.get(x, y, z);
                    while y > ctx.g.min_y {
                        y -= 1;
                        let nb = ctx.get(x, y, z);
                        let spawn = |s: u32| {
                            let r = breg();
                            r.map(|r| r.is_air(s)).unwrap_or(true)
                                || matches!(
                                    r.map(|r| r.name_of(s)),
                                    Some("minecraft:water") | Some("minecraft:lava")
                                )
                        };
                        if !spawn(nb)
                            && spawn(cur)
                            && breg().map(|r| r.name_of(nb)) != Some("minecraft:bedrock")
                        {
                            if found == layer {
                                out.push((x, y + 1, z));
                                any = true;
                                break;
                            }
                            found += 1;
                        }
                        cur = nb;
                    }
                }
                layer += 1;
                if !any {
                    break;
                }
            }
        }
        PM::InSquare => {
            out.push((pos.0 + rng.next_bound(16), pos.1, pos.2 + rng.next_bound(16)));
        }
        PM::Offset(xz, ysp) => {
            out.push((pos.0 + xz.get(rng), pos.1 + ysp.get(rng), pos.2 + xz.get(rng)));
        }
        PM::HeightRange(hp) => {
            out.push((pos.0, hp.get(rng, ctx.g.min_y, ctx.g.height), pos.2));
        }
        PM::Heightmap(kind) => {
            let top = if *kind == 1 {
                ctx.hm_oc(pos.0, pos.2)
            } else {
                ctx.hm_ws(pos.0, pos.2)
            };
            if top > ctx.g.min_y {
                out.push((pos.0, top, pos.2));
            }
        }
        PM::Biome => {
            if let Some(b) = ctx.biome(pos.0, pos.1, pos.2) {
                if fr.ball.get(&b).map(|s| s.contains(&pf.name)).unwrap_or(false) {
                    out.push(pos);
                }
            }
        }
        PM::BlockPred(p) => {
            if let Some(reg) = breg() {
                if test_pred(p, reg, ctx, pos.0, pos.1, pos.2) {
                    out.push(pos);
                }
            }
        }
        PM::Rarity(ch) => {
            if *ch > 0 && rng.next_f32() < 1.0 / *ch as f32 {
                out.push(pos);
            }
        }
        PM::SurfaceRel(min_i, max_i) => {
            let y = ctx.hm_ws(pos.0, pos.2);
            let lo = y.saturating_add(*min_i);
            let hi = y.saturating_add(*max_i);
            if lo <= pos.1 && pos.1 <= hi {
                out.push(pos);
            }
        }
        PM::SurfWaterDepth(max) => {
            let wt = ctx.hm_ws(pos.0, pos.2);
            let oc = ctx.hm_oc(pos.0, pos.2);
            if wt - oc <= *max {
                out.push(pos);
            }
        }
        PM::NoiseCount(ratio, factor, offset) => {
            let n = foliage_noise().sample(pos.0 as f64 / *factor, pos.2 as f64 / *factor);
            let cnt = ((n + *offset) * *ratio as f64).ceil().max(0.0) as i32;
            for _ in 0..cnt {
                out.push(pos);
            }
        }
        PM::NoiseThrCount(level, below, above) => {
            let n = foliage_noise().sample(pos.0 as f64 / 200.0, pos.2 as f64 / 200.0);
            let cnt = if n < *level { *below } else { *above };
            for _ in 0..cnt.max(0) {
                out.push(pos);
            }
        }
        PM::RandomChance(c) => {
            if rng.next_f32() < *c {
                out.push(pos);
            }
        }
        PM::EnvScan(up, allowed, target, max_steps) => {
            if let Some(reg) = breg() {
                let allowed_pred: &Pr = allowed.as_ref().unwrap_or(&Pr::True);
                if !test_pred(allowed_pred, reg, ctx, pos.0, pos.1, pos.2) {
                    return;
                }
                let (dx, dy, dz) = dir_off(*up);
                let mut p = pos;
                for _ in 0..*max_steps {
                    if test_pred(target, reg, ctx, p.0, p.1, p.2) {
                        out.push(p);
                        return;
                    }
                    p = (p.0 + dx, p.1 + dy, p.2 + dz);
                    if ctx.out_of_h(p.1) {
                        return;
                    }
                    if !test_pred(allowed_pred, reg, ctx, p.0, p.1, p.2) {
                        break;
                    }
                }
                if test_pred(target, reg, ctx, p.0, p.1, p.2) {
                    out.push(p);
                }
            }
        }
        PM::Carving => {}
    }
}

// ---------- configured feature generation ----------

fn ore_should_place(discard: f32, ctx: &mut FC, rt: &RT, pos: (i32, i32, i32), r: &mut Xor) -> bool {
    let st = ctx.get(pos.0, pos.1, pos.2);
    let Some(reg) = breg() else { return false };
    if !test_rt(rt, reg, st, pos.1, r) {
        return false;
    }
    if discard <= 0.0 {
        return true;
    }
    if discard >= 1.0 {
        return false;
    }
    if r.next_f32() >= discard {
        return true;
    }
    for d in 0..6u8 {
        let (dx, dy, dz) = dir_off(d);
        if reg.is_air(ctx.get(pos.0 + dx, pos.1 + dy, pos.2 + dz)) {
            return false;
        }
    }
    true
}

#[expect(clippy::too_many_arguments)]
fn gen_vein(
    ctx: &mut FC,
    rng: &mut Xor,
    sx: f64, ex: f64, sz: f64, ez: f64, sy: f64, ey: f64,
    xb: i32, yb: i32, zb: i32, hs: i32, vs: i32,
    size: i32, discard: f32, targets: &[(RT, u32)],
) {
    let j = size.max(1);
    let mut ds = vec![0.0; (j * 4) as usize];
    for k in 0..j {
        let f = k as f32 / j as f32;
        ds[k as usize * 4] = sx + (ex - sx) * f as f64;
        ds[k as usize * 4 + 1] = sy + (ey - sy) * f as f64;
        ds[k as usize * 4 + 2] = sz + (ez - sz) * f as f64;
        // The radius is the one place vanilla uses the table: `Mth.sin(float)` over
        // the f32 fraction, then double arithmetic around it.
        let hh = rng.next_f64() * j as f64 / 16.0;
        let s1 = crate::generation_math::mth_sin(std::f32::consts::PI * f) + 1.0f32;
        ds[k as usize * 4 + 3] = (s1 as f64 * hh + 1.0) / 2.0;
    }
    for k in 0..(j - 1) {
        if ds[k as usize * 4 + 3] <= 0.0 {
            continue;
        }
        for m in (k + 1)..j {
            if ds[m as usize * 4 + 3] <= 0.0 {
                continue;
            }
            let hv = ds[k as usize * 4 + 3] - ds[m as usize * 4 + 3];
            let dv = ds[k as usize * 4] - ds[m as usize * 4];
            let ev = ds[k as usize * 4 + 1] - ds[m as usize * 4 + 1];
            let gv = ds[k as usize * 4 + 2] - ds[m as usize * 4 + 2];
            if hv * hv > dv * dv + ev * ev + gv * gv {
                if hv > 0.0 {
                    ds[m as usize * 4 + 3] = -1.0;
                } else {
                    ds[k as usize * 4 + 3] = -1.0;
                }
            }
        }
    }
    let bsz = (hs.max(1) * vs.max(1) * hs.max(1)) as usize;
    let mut bits = vec![false; bsz];
    for m in 0..j {
        let d = ds[m as usize * 4 + 3];
        if d <= 0.0 {
            continue;
        }
        let cx = ds[m as usize * 4];
        let cy = ds[m as usize * 4 + 1];
        let cz = ds[m as usize * 4 + 2];
        let x0 = ((cx - d).floor() as i32).max(xb);
        let y0 = ((cy - d).floor() as i32).max(yb);
        let z0 = ((cz - d).floor() as i32).max(zb);
        let x1 = ((cx + d).floor() as i32).max(x0);
        let y1 = ((cy + d).floor() as i32).max(y0);
        let z1 = ((cz + d).floor() as i32).max(z0);
        for xx in x0..=x1 {
            let ux = (xx as f64 + 0.5 - cx) / d;
            if ux * ux >= 1.0 {
                continue;
            }
            for yy in y0..=y1 {
                let uy = (yy as f64 + 0.5 - cy) / d;
                if ux * ux + uy * uy >= 1.0 {
                    continue;
                }
                if ctx.out_of_h(yy) {
                    continue;
                }
                let base = (xx - xb) + (yy - yb) * hs;
                for zz in z0..=z1 {
                    let uz = (zz as f64 + 0.5 - cz) / d;
                    if ux * ux + uy * uy + uz * uz >= 1.0 {
                        continue;
                    }
                    let bi = (base + (zz - zb) * hs * vs) as usize;
                    if bi >= bits.len() || bits[bi] {
                        continue;
                    }
                    bits[bi] = true;
                    for (rt, st) in targets {
                        if ore_should_place(discard, ctx, rt, (xx, yy, zz), rng) {
                            ctx.set(xx, yy, zz, *st);
                            break;
                        }
                    }
                }
            }
        }
    }
}


fn gen_ore(
    ctx: &mut FC,
    rng: &mut Xor,
    pos: (i32, i32, i32),
    size: i32,
    discard: f32,
    targets: &[(RT, u32)],
    scattered: bool,
) {
            if scattered {
                let count = rng.next_bound(size + 1);
                for i in 0..count {
                    let md = i.min(7);
                    let off = |rng: &mut Xor| {
                        let f1 = rng.next_f32();
                        let f2 = rng.next_f32();
                        ((f1 - f2) * md as f32).round() as i32
                    };
                    let tp = (pos.0 + off(rng), pos.1 + off(rng), pos.2 + off(rng));
                    if ctx.out_of_h(tp.1) {
                        continue;
                    }
                    for (rt, st) in targets {
                        if ore_should_place(discard, ctx, rt, tp, rng) {
                            ctx.set(tp.0, tp.1, tp.2, *st);
                            break;
                        }
                    }
                }
                return;
            }
            let f = rng.next_f32() * std::f32::consts::PI;
            let g = size as f32 / 8.0;
            let i = ((size as f32 / 16.0 * 2.0 + 1.0) / 2.0).ceil() as i32;
            // The centre uses `Math.sin((double)f)`, not the table: vanilla widens the
            // float angle and calls double-precision sin/cos here.
            let sin_f = (f as f64).sin();
            let cos_f = (f as f64).cos();
            let d = pos.0 as f64 + sin_f * g as f64;
            let e = pos.0 as f64 - sin_f * g as f64;
            let h = pos.2 as f64 + cos_f * g as f64;
            let j = pos.2 as f64 - cos_f * g as f64;
            let l = pos.1 as f64 + rng.next_bound(3) as f64 - 2.0;
            let mm = pos.1 as f64 + rng.next_bound(3) as f64 - 2.0;
            let n = pos.0 - g.ceil() as i32 - i;
            let o = pos.1 - 2 - i;
            let p = pos.2 - g.ceil() as i32 - i;
            let q = 2 * (g.ceil() as i32 + i);
            let r2 = 2 * (2 + i);
            for xp in n..=(n + q) {
                for zp in p..=(p + q) {
                    if o <= ctx.hm_oc(xp, zp) {
                        gen_vein(ctx, rng, d, e, h, j, l, mm, n, o, p, q, r2, size, discard, targets);
                        return;
                    }
                }
            }
}


/// Test helper: the cells one vein writes for a seed and position, with a rule test that
/// always matches, so the geometry can be compared cell for cell with vanilla's own output
/// (see evidence/gameplay/feature-parity/ore_andesite_666.txt).
#[cfg(test)]
pub(crate) fn ore_cells_for_test(seed: u64, pos: (i32, i32, i32), size: i32) -> Vec<(i32, i32, i32)> {
    let mut g = crate::worldgen::Gen::load(0, crate::name_to_state).expect("datapack");
    let mut ctx = FC::new(&mut g);
    let mut rng = Xor::seed(seed);
    let targets = [(RT::True, 0u32)];
    gen_ore(&mut ctx, &mut rng, pos, size, 0.0, &targets, false);
    let mut v: Vec<(i32, i32, i32)> = ctx.wr.keys().copied().collect();
    v.sort();
    v
}

fn gen_cf(cf: &Cf, fr: &'static FReg, ctx: &mut FC, rng: &mut Xor, pos: (i32, i32, i32)) {
    match cf {
        Cf::Unknown(_) => {}
        Cf::Tree(t) => {
            crate::tree::gen_tree(ctx, rng, pos, t);
        }
        Cf::Ore { size, discard, targets, scattered } => {
            gen_ore(ctx, rng, pos, *size, *discard, targets, *scattered);
        }
        Cf::Disk { half, radius, bsp, target } => {
            let Some(reg) = breg() else { return };
            let top = pos.1 + half;
            let bottom = pos.1 - half - 1;
            let rr = radius.get(rng);
            for cx in -rr..=rr {
                for cz in -rr..=rr {
                    if cx * cx + cz * cz > rr * rr {
                        continue;
                    }
                    let (x, z) = (pos.0 + cx, pos.2 + cz);
                    let mut y = top;
                    while y > bottom {
                        if !ctx.out_of_h(y) && test_pred(target, reg, ctx, x, y, z) {
                            let st = bsp.get(ctx, rng, x, y, z);
                            ctx.set(x, y, z, st);
                        }
                        y -= 1;
                    }
                }
            }
        }
        Cf::SimpleBlock { to, on, in_, under } => {
            if let Some(reg) = breg() {
                if !on.is_empty()
                    && !on.iter().any(|t| *t == reg.name_of(ctx.get(pos.0, pos.1 - 1, pos.2)))
                {
                    return;
                }
                if !in_.is_empty()
                    && !in_.iter().any(|t| *t == reg.name_of(ctx.get(pos.0, pos.1, pos.2)))
                {
                    return;
                }
                if !under.is_empty()
                    && !under.iter().any(|t| *t == reg.name_of(ctx.get(pos.0, pos.1 + 1, pos.2)))
                {
                    return;
                }
            }
            let st = to.get(ctx, rng, pos.0, pos.1, pos.2);
            ctx.set(pos.0, pos.1, pos.2, st);
        }
        Cf::Spring { fluid, req_below, rock, hole, valid } => {
            let Some(reg) = breg() else { return };
            if !valid.contains(reg.name_of(ctx.get(pos.0, pos.1 + 1, pos.2))) {
                return;
            }
            if *req_below && !valid.contains(reg.name_of(ctx.get(pos.0, pos.1 - 1, pos.2))) {
                return;
            }
            let at = ctx.get(pos.0, pos.1, pos.2);
            if !reg.is_air(at) && !valid.contains(reg.name_of(at)) {
                return;
            }
            let mut rocks = 0;
            let mut air = 0;
            for d in [4u8, 5, 2, 3, 0] {
                let (dx, dy, dz) = dir_off(d);
                let s = ctx.get(pos.0 + dx, pos.1 + dy, pos.2 + dz);
                if valid.contains(reg.name_of(s)) {
                    rocks += 1;
                }
                if reg.is_air(s) {
                    air += 1;
                }
            }
            if rocks == *rock && air == *hole {
                ctx.set(pos.0, pos.1, pos.2, *fluid);
            }
        }
        Cf::RandSel(list, default) => {
            for (pf, chance) in list {
                if rng.next_f32() < *chance {
                    gen_pf(pf, fr, ctx, rng, pos);
                    return;
                }
            }
            if let Some(d) = default {
                gen_pf(d, fr, ctx, rng, pos);
            }
        }
        Cf::SimpleRand(list) => {
            if list.is_empty() {
                return;
            }
            let i = rng.next_bound(list.len() as i32) as usize;
            gen_pf(&list[i], fr, ctx, rng, pos);
        }
        Cf::WeightedSel(list) => {
            let total: i32 = list.iter().map(|e| e.1.max(0)).sum();
            if total <= 0 {
                return;
            }
            let mut roll = rng.next_bound(total);
            for (pf, w) in list {
                roll -= (*w).max(0);
                if roll < 0 {
                    gen_pf(pf, fr, ctx, rng, pos);
                    return;
                }
            }
        }
        Cf::BoolSel(t, f) => {
            if rng.next_bool() {
                gen_pf(t, fr, ctx, rng, pos);
            } else {
                gen_pf(f, fr, ctx, rng, pos);
            }
        }
        Cf::Seq(list) => {
            for pf in list {
                gen_pf(pf, fr, ctx, rng, pos);
            }
        }
        Cf::VegPatch { depth, extra_bottom, extra_edge, replaceable, ground, floor, chance, vfeat, xz, vrange, waterlog } => {
            let Some(reg) = breg() else { return };
            let xr = xz.get(rng) + 1;
            let zr = xz.get(rng) + 1;
            let inward: i32 = if *floor { -1 } else { 1 };
            let mut surface_pts: Vec<(i32, i32, i32)> = vec![];
            for dx in -xr..=xr {
                for dz in -zr..=zr {
                    let xe = dx == -xr || dx == xr;
                    let ze = dz == -zr || dz == zr;
                    if xe && ze {
                        continue;
                    }
                    if (xe || ze) && (*extra_edge == 0.0 || rng.next_f32() > *extra_edge) {
                        continue;
                    }
                    let mut p = (pos.0 + dx, pos.1, pos.2 + dz);
                    for _ in 0..*vrange {
                        if !ctx.is_air(p.0, p.1, p.2) {
                            break;
                        }
                        p.1 += inward;
                    }
                    for _ in 0..*vrange {
                        if ctx.is_air(p.0, p.1, p.2) {
                            break;
                        }
                        p.1 -= inward;
                    }
                    let below = (p.0, p.1 + inward, p.2);
                    let bs = ctx.get(below.0, below.1, below.2);
                    let sdir = if *floor { 1usize } else { 0 }; // face opposing inward walk
                    if !(ctx.is_air(p.0, p.1, p.2) && reg.side_solid(bs, sdir)) {
                        continue;
                    }
                    let mut dep = depth.get(rng);
                    if *extra_bottom > 0.0 && rng.next_f32() < *extra_bottom {
                        dep += 1;
                    }
                    let mut bp = below;
                    let mut placed_any = false;
                    for i in 0..dep {
                        let cur = ctx.get(bp.0, bp.1, bp.2);
                        let mut st = ground.get(ctx, rng, bp.0, bp.1, bp.2);
                        if *waterlog && is_water_st(reg, cur) {
                            st = reg.with_prop(st, "waterlogged", "true");
                        }
                        if reg.name_of(st) == reg.name_of(cur) {
                            break;
                        }
                        if !reg.in_tag(cur, replaceable) {
                            if i == 0 {
                                break;
                            }
                            break;
                        }
                        ctx.set(bp.0, bp.1, bp.2, st);
                        bp.1 += inward;
                        placed_any = true;
                    }
                    if placed_any {
                        surface_pts.push(below);
                    }
                }
            }
            let water_st = reg.state_of("minecraft:water", None);
            for sp in surface_pts {
                if *waterlog {
                    // waterlogged patches fill the surface point with water and
                    // place vegetation at the water cell itself
                    let below_s = ctx.get(sp.0, sp.1, sp.2);
                    let mut exposed = false;
                    for (dx, dy, dz) in [(1, 0, 0), (-1, 0, 0), (0, 0, 1), (0, 0, -1), (0, -1, 0)] {
                        let s = ctx.get(sp.0 + dx, sp.1 + dy, sp.2 + dz);
                        let face = match (dx, dy, dz) {
                            (1, 0, 0) => 4usize,
                            (-1, 0, 0) => 5,
                            (0, 0, 1) => 2,
                            (0, 0, -1) => 3,
                            _ => 1,
                        };
                        if !reg.side_solid(s, face) {
                            exposed = true;
                            break;
                        }
                    }
                    if exposed {
                        let _ = below_s;
                        continue;
                    }
                    ctx.set(sp.0, sp.1, sp.2, water_st);
                }
                if *chance > 0.0 && rng.next_f32() < *chance {
                    let pp = if *waterlog { sp } else { (sp.0, sp.1 - inward, sp.2) };
                    gen_pf(vfeat, fr, ctx, rng, pp);
                    if *waterlog {
                        let placed = ctx.get(pp.0, pp.1, pp.2);
                        if !reg.is_air(placed) && is_water_st(reg, ctx.get(pp.0, pp.1, pp.2)) == false {
                            // waterlog the placed vegetation
                            let wst = reg.with_prop(placed, "waterlogged", "true");
                            if wst != placed {
                                ctx.set(pp.0, pp.1, pp.2, wst);
                            }
                        }
                    }
                }
            }
        }
        Cf::BlockColumn { layers, down, allowed, prio } => {
            let Some(reg) = breg() else { return };
            let n = layers.len();
            let mut heights: Vec<i32> = layers.iter().map(|(h, _)| h.get(rng)).collect();
            let total: i32 = heights.iter().sum();
            if total == 0 {
                return;
            }
            let d = if *down { -1 } else { 1 };
            // measure allowed run length
            let mut allowed_len = 0;
            {
                let mut mp = (pos.0, pos.1 + d, pos.2);
                while allowed_len < total {
                    let ok = match allowed {
                        Some(pr) => test_pred(pr, reg, ctx, mp.0, mp.1, mp.2),
                        None => ctx.is_air(mp.0, mp.1, mp.2)
                            || reg.name_of(ctx.get(mp.0, mp.1, mp.2)) == "minecraft:water",
                    };
                    if !ok {
                        // vanilla adjust_layer_heights: trim layers from tip (prio)
                        // or base
                        let mut trim = total - allowed_len;
                        let idxs: Vec<usize> = if *prio {
                            (0..n).collect()
                        } else {
                            (0..n).rev().collect()
                        };
                        for i in idxs {
                            let cut = heights[i].min(trim);
                            heights[i] -= cut;
                            trim -= cut;
                            if trim <= 0 {
                                break;
                            }
                        }
                        break;
                    }
                    mp.1 += d;
                    allowed_len += 1;
                }
            }
            let mut mp = pos;
            for (i, (_h, b)) in layers.iter().enumerate() {
                for _ in 0..heights[i] {
                    let st = b.get(ctx, rng, mp.0, mp.1, mp.2);
                    ctx.set(mp.0, mp.1, mp.2, st);
                    mp.1 += d;
                }
            }
        }
        Cf::BlockPile(bsp) => {
            let st = bsp.get(ctx, rng, pos.0, pos.1, pos.2);
            if ctx.is_air(pos.0, pos.1, pos.2) {
                ctx.set(pos.0, pos.1, pos.2, st);
            }
        }
        Cf::Lake { fluid, barrier } => {
            let Some(reg) = breg() else { return };
            // vanilla LakeFeature: 4-7 ellipsoid blobs in a 16x8x16 grid
            let mut grid = [false; 16 * 16 * 8];
            let gidx = |x: usize, z: usize, y: usize| (x * 16 + z) * 8 + y;
            let spots = rng.next_bound(4) + 4;
            for _ in 0..spots {
                let xr = rng.next_f64() * 6.0 + 3.0;
                let yr = rng.next_f64() * 4.0 + 2.0;
                let zr = rng.next_f64() * 6.0 + 3.0;
                let xp = rng.next_f64() * (16.0 - xr - 2.0) + 1.0 + xr / 2.0;
                let yp = rng.next_f64() * (8.0 - yr - 4.0) + 2.0 + yr / 2.0;
                let zp = rng.next_f64() * (16.0 - zr - 2.0) + 1.0 + zr / 2.0;
                for xx in 1..15usize {
                    for zz in 1..15usize {
                        for yy in 1..7usize {
                            let xd = (xx as f64 - xp) / (xr / 2.0);
                            let yd = (yy as f64 - yp) / (yr / 2.0);
                            let zd = (zz as f64 - zp) / (zr / 2.0);
                            if xd * xd + yd * yd + zd * zd < 1.0 {
                                grid[gidx(xx, zz, yy)] = true;
                            }
                        }
                    }
                }
            }
            let shell = |x: usize, z: usize, y: usize| -> bool {
                !grid[gidx(x, z, y)]
                    && (x < 15 && grid[gidx(x + 1, z, y)]
                        || x > 0 && grid[gidx(x - 1, z, y)]
                        || z < 15 && grid[gidx(x, z + 1, y)]
                        || z > 0 && grid[gidx(x, z - 1, y)]
                        || y < 7 && grid[gidx(x, z, y + 1)]
                        || y > 0 && grid[gidx(x, z, y - 1)])
            };
            let fluid_st = fluid.get(ctx, rng, pos.0, pos.1, pos.2);
            let barrier_st = barrier.get(ctx, rng, pos.0, pos.1, pos.2);
            let air_st = reg.state_of("minecraft:cave_air", None);
            for x in 0..16usize {
                for z in 0..16usize {
                    for y in 0..8usize {
                        if !shell(x, z, y) {
                            continue;
                        }
                        let bp = (pos.0 + x as i32 - 8, pos.1 + y as i32 - 4, pos.2 + z as i32 - 8);
                        let cur = ctx.get(bp.0, bp.1, bp.2);
                        if y >= 4 && reg.is_liquid(cur) {
                            return;
                        }
                        if y < 4 && !reg.is_solid(cur) && cur != fluid_st {
                            return;
                        }
                    }
                }
            }
            for x in 0..16usize {
                for z in 0..16usize {
                    for y in 0..8usize {
                        if grid[gidx(x, z, y)] {
                            let bp = (pos.0 + x as i32 - 8, pos.1 + y as i32 - 4, pos.2 + z as i32 - 8);
                            let cur = ctx.get(bp.0, bp.1, bp.2);
                            if !reg.in_tag(cur, "minecraft:features_cannot_replace") {
                                ctx.set(bp.0, bp.1, bp.2, if y >= 4 { air_st } else { fluid_st });
                            }
                        }
                    }
                }
            }
            if !reg.is_air(barrier_st) {
                for x in 0..16usize {
                    for z in 0..16usize {
                        for y in 0..8usize {
                            if shell(x, z, y) && (y < 4 || rng.next_bound(2) != 0) {
                                let bp = (pos.0 + x as i32 - 8, pos.1 + y as i32 - 4, pos.2 + z as i32 - 8);
                                let cur = ctx.get(bp.0, bp.1, bp.2);
                                if reg.is_solid(cur)
                                    && !reg.in_tag(cur, "minecraft:lava_pool_stone_cannot_replace")
                                {
                                    ctx.set(bp.0, bp.1, bp.2, barrier_st);
                                }
                            }
                        }
                    }
                }
            }
            // freeze the water surface row in cold biomes
            if reg.name_of(fluid_st) == "minecraft:water" {
                let ice = reg.state_of("minecraft:ice", None);
                for x in 0..16usize {
                    for z in 0..16usize {
                        let bp = (pos.0 + x as i32 - 8, pos.1 + 4 - 4, pos.2 + z as i32 - 8);
                        if let Some(b) = ctx.biome(bp.0, bp.1, bp.2) {
                            let t = fr.btemp.get(&b).copied().unwrap_or(0.8);
                            if t < 0.15 {
                                let cur = ctx.get(bp.0, bp.1, bp.2);
                                if !reg.in_tag(cur, "minecraft:features_cannot_replace")
                                    && is_water_st(reg, cur)
                                {
                                    ctx.set(bp.0, bp.1, bp.2, ice);
                                }
                            }
                        }
                    }
                }
            }
        }
        Cf::BlueIce => {
            let Some(reg) = breg() else { return };
            // vanilla: drop to the ocean floor, then scatter blue/packed ice
            let mut p = pos;
            if ctx.is_air(p.0, p.1, p.2) {
                while p.1 > ctx.g.min_y + 1 && ctx.is_air(p.0, p.1, p.2) {
                    p.1 -= 1;
                }
            } else {
                while p.1 < ctx.g.min_y + ctx.g.height - 1 && !ctx.is_air(p.0, p.1, p.2) {
                    p.1 += 1;
                }
            }
            if ctx.is_air(p.0, p.1, p.2) {
                return;
            }
            p.1 -= 1;
            let ice = reg.state_of("minecraft:blue_ice", None);
            let packed = reg.state_of("minecraft:packed_ice", None);
            for dx in -3i32..=3 {
                for dz in -3i32..=3 {
                    for dy in -2i32..=2 {
                        let d2 = dx * dx + dz * dz + dy * dy;
                        if d2 > 8 {
                            continue;
                        }
                        let bp = (p.0 + dx, p.1 + dy, p.2 + dz);
                        let cur = ctx.get(bp.0, bp.1, bp.2);
                        let n = reg.name_of(cur);
                        if n == "minecraft:water" || n == "minecraft:ice" || n == "minecraft:packed_ice" {
                            let st = if rng.next_bound(6) == 0 { ice } else { packed };
                            ctx.set(bp.0, bp.1, bp.2, st);
                        }
                    }
                }
            }
        }
        Cf::BlockBlob { state } => {
            // vanilla BlockBlobFeature: descend to first solid ground, place blob
            let Some(reg) = breg() else { return };
            let mut p = pos;
            for _ in 0..64 {
                if ctx.is_air(p.0, p.1, p.2) {
                    let below = ctx.get(p.0, p.1 - 1, p.2);
                    if reg.in_tag(below, "minecraft:dirt")
                        || reg.in_tag(below, "minecraft:base_stone_overworld")
                        || reg.is_solid(below)
                    {
                        break;
                    }
                    p.1 -= 1;
                } else {
                    p.1 += 1;
                }
                if p.1 < ctx.g.min_y + 3 || p.1 > ctx.g.min_y + ctx.g.height - 4 {
                    return;
                }
            }
            // three-block blob: base + two above, radius falloff by chance
            for dy in 0..3i32 {
                let r = if dy == 0 { 2 } else { 1 };
                for dx in -r..=r {
                    for dz in -r..=r {
                        let bp = (p.0 + dx, p.1 + dy, p.2 + dz);
                        if dx * dx + dz * dz > r * r + 1 {
                            continue;
                        }
                        let cur = ctx.get(bp.0, bp.1, bp.2);
                        if reg.is_air(cur) || reg.is_replaceable(cur) || reg.name_of(cur).contains("grass") || reg.name_of(cur).contains("snow") {
                            ctx.set(bp.0, bp.1, bp.2, *state);
                        }
                    }
                }
            }
        }
        Cf::UnderMagma { floor, prob, radius } => {
            let Some(reg) = breg() else { return };
            let magma = reg.state_of("minecraft:magma_block", None);
            let mut p = pos;
            let mut found = false;
            for _ in 0..*floor {
                let cur = ctx.get(p.0, p.1 - 1, p.2);
                if !is_water_st(reg, cur) && !reg.is_air(cur) {
                    found = true;
                    break;
                }
                p.1 -= 1;
            }
            if !found {
                return;
            }
            let r = *radius;
            for dx in -r..=r {
                for dz in -r..=r {
                    for dy in -1..=r + 2 {
                        let bp = (p.0 + dx, p.1 + dy - 1, p.2 + dz);
                        let cur = ctx.get(bp.0, bp.1, bp.2);
                        let n = reg.name_of(cur);
                        let target = matches!(
                            n,
                            "minecraft:netherrack" | "minecraft:dirt" | "minecraft:gravel"
                                | "minecraft:sand" | "minecraft:clay" | "minecraft:stone"
                                | "minecraft:deepslate"
                        ) || n.contains("stone");
                        if !target {
                            continue;
                        }
                        if is_water_st(reg, ctx.get(bp.0, bp.1 + 1, bp.2))
                            || is_water_st(reg, ctx.get(bp.0, bp.1 + 2, bp.2))
                        {
                            continue;
                        }
                        if rng.next_f32() < *prob {
                            ctx.set(bp.0, bp.1, bp.2, magma);
                        }
                    }
                }
            }
        }
        Cf::FreezeTop => {
            let Some(reg) = breg() else { return };
            let bx = pos.0 & !15;
            let bz = pos.2 & !15;
            let ice = reg.state_of("minecraft:ice", None);
            let snow = reg.state_of("minecraft:snow", None);
            for lx in 0..16i32 {
                for lz in 0..16i32 {
                    let (x, z) = (bx + lx, bz + lz);
                    let top = ctx.hm_ws(x, z);
                    if top <= ctx.g.min_y + 1 {
                        continue;
                    }
                    let Some(b) = ctx.biome(x, top, z) else { continue };
                    let temp = fr.btemp.get(&b).copied().unwrap_or(0.8);
                    if temp >= 0.15 {
                        continue;
                    }
                    let cur = ctx.get(x, top - 1, z);
                    if is_water_st(reg, cur) {
                        ctx.set(x, top - 1, z, ice);
                    } else if reg.is_solid(cur)
                        && ctx.is_air(x, top, z)
                        && top < ctx.g.min_y + ctx.g.height
                    {
                        ctx.set(x, top, z, snow);
                    }
                }
            }
        }
        Cf::Multiface { name, on, ceiling, wall, floor, search } => {
            let Some(reg) = breg() else { return };
            // valid placement directions (face the lichen occupies)
            let mut dirs: Vec<u8> = vec![];
            if *ceiling {
                dirs.push(1);
            }
            if *floor {
                dirs.push(0);
            }
            if *wall {
                dirs.extend([2u8, 3, 4, 5]);
            }
            if dirs.is_empty() {
                return;
            }
            let place_on_faces = |ctx: &mut FC,
                                  rng: &mut Xor,
                                  p: (i32, i32, i32),
                                  exclude: Option<u8>|
             -> bool {
                // shuffled direction order
                let mut dd: Vec<u8> = dirs.iter().copied().filter(|d| Some(*d) != exclude).collect();
                for i in (1..dd.len()).rev() {
                    let j = rng.next_bound(i as i32 + 1) as usize;
                    dd.swap(i, j);
                }
                for &d in &dd {
                    let (dx, dy, dz) = dir_off(d);
                    let nb = (p.0 + dx, p.1 + dy, p.2 + dz);
                    let nb_st = ctx.get(nb.0, nb.1, nb.2);
                    if !on.contains(reg.name_of(nb_st)) {
                        continue;
                    }
                    let face_prop = match d {
                        0 => "down",
                        1 => "up",
                        2 => "north",
                        3 => "south",
                        4 => "west",
                        _ => "east",
                    };
                    let cur = ctx.get(p.0, p.1, p.2);
                    let base = if reg.name_of(cur) == name.as_str() {
                        cur
                    } else {
                        reg.state_of(name, None)
                    };
                    let mut st = reg.with_prop(base, face_prop, "true");
                    if is_water_st(reg, cur) {
                        st = reg.with_prop(st, "waterlogged", "true");
                    }
                    ctx.set(p.0, p.1, p.2, st);
                    return true;
                }
                false
            };
            let cur = ctx.get(pos.0, pos.1, pos.2);
            if !(reg.is_air(cur) || is_water_st(reg, cur)) {
                return;
            }
            if place_on_faces(ctx, rng, pos, None) {
                return;
            }
            // spread: walk in a shuffled direction, keep trying
            for &d in &dirs {
                let (dx, dy, dz) = dir_off(d);
                let mut p = pos;
                for _ in 0..*search {
                    p = (p.0 + dx, p.1 + dy, p.2 + dz);
                    let c = ctx.get(p.0, p.1, p.2);
                    if !(reg.is_air(c) || is_water_st(reg, c)) && reg.name_of(c) != name.as_str() {
                        break;
                    }
                    if place_on_faces(ctx, rng, p, Some(match d { 0 => 1, 1 => 0, 2 => 3, 3 => 2, 4 => 5, _ => 4 })) {
                        return;
                    }
                }
            }
        }
        Cf::Bamboo(prob) => {
            let Some(reg) = breg() else { return };
            if !ctx.is_air(pos.0, pos.1, pos.2)
                || !can_place(reg, ctx, "minecraft:bamboo", pos.0, pos.1, pos.2)
            {
                return;
            }
            let height = rng.next_bound(12) + 5;
            if rng.next_f32() < *prob {
                let r = rng.next_bound(4) + 1;
                let podzol = reg.state_of("minecraft:podzol", None);
                for xx in pos.0 - r..=pos.0 + r {
                    for zz in pos.2 - r..=pos.2 + r {
                        let xd = xx - pos.0;
                        let zd = zz - pos.2;
                        if xd * xd + zd * zd <= r * r {
                            let py = ctx.hm_ws(xx, zz) - 1;
                            let cur = ctx.get(xx, py, zz);
                            if reg.in_tag(cur, "minecraft:bamboo_plantable_on")
                                || is_dirtish(reg, cur)
                            {
                                ctx.set(xx, py, zz, podzol);
                            }
                        }
                    }
                }
            }
            let mk = |leaves: &str, stage: &str| {
                reg.state_of(
                    "minecraft:bamboo",
                    Some(&mkprops(&[("age", "1"), ("leaves", leaves), ("stage", stage)])),
                )
            };
            let trunk = mk("none", "0");
            let mut by = pos.1;
            for _ in 0..height {
                if ctx.is_air(pos.0, by, pos.2) {
                    ctx.set(pos.0, by, pos.2, trunk);
                    by += 1;
                } else {
                    break;
                }
            }
            if by - pos.1 >= 3 {
                ctx.set(pos.0, by - 1, pos.2, mk("large", "1"));
                ctx.set(pos.0, by - 2, pos.2, mk("large", "0"));
                ctx.set(pos.0, by - 3, pos.2, mk("small", "0"));
            }
        }
        Cf::Vines => {
            let Some(reg) = breg() else { return };
            let base = reg.state_of("minecraft:vine", None);
            if !ctx.is_air(pos.0, pos.1, pos.2) {
                return;
            }
            for d in [2u8, 3, 4, 5] {
                let (dx, _dy, dz) = dir_off(d);
                let s = ctx.get(pos.0 + dx, pos.1, pos.2 + dz);
                if !reg.side_solid(s, match d { 2 => 3, 3 => 2, 4 => 5, _ => 4 }) {
                    continue;
                }
                let face = match d { 2 => "south", 3 => "north", 4 => "east", _ => "west" };
                let mut y = pos.1;
                while ctx.is_air(pos.0, y, pos.2) && y > pos.1 - 4 {
                    let st = reg.with_prop(base, face, "true");
                    ctx.set(pos.0, y, pos.2, st);
                    y -= 1;
                }
                break;
            }
        }
        Cf::MonsterRoom => {
            let Some(reg) = breg() else { return };
            let cobble = reg.state_of("minecraft:cobblestone", None);
            let mossy = reg.state_of("minecraft:mossy_cobblestone", None);
            let cave_air = reg.state_of("minecraft:cave_air", None);
            let xr = rng.next_bound(2) + 2;
            let zr = rng.next_bound(2) + 2;
            let (min_x, max_x) = (-xr - 1, xr + 1);
            let (min_z, max_z) = (-zr - 1, zr + 1);
            let mut holes = 0;
            for dx in min_x..=max_x {
                for dy in -1..=4i32 {
                    for dz in min_z..=max_z {
                        let cp = (pos.0 + dx, pos.1 + dy, pos.2 + dz);
                        let solid = reg.is_solid(ctx.get(cp.0, cp.1, cp.2));
                        if (dy == -1 || dy == 4) && !solid {
                            return;
                        }
                        let on_wall = dx == min_x || dx == max_x || dz == min_z || dz == max_z;
                        if on_wall
                            && dy == 0
                            && ctx.is_air(cp.0, cp.1, cp.2)
                            && ctx.is_air(cp.0, cp.1 + 1, cp.2)
                        {
                            holes += 1;
                        }
                    }
                }
            }
            if !(1..=5).contains(&holes) {
                return;
            }
            for dx in min_x..=max_x {
                for dy in (-1..=3i32).rev() {
                    for dz in min_z..=max_z {
                        let wp = (pos.0 + dx, pos.1 + dy, pos.2 + dz);
                        let boundary = dx == min_x || dy == -1 || dz == min_z
                            || dx == max_x || dy == 4 || dz == max_z;
                        let cur = ctx.get(wp.0, wp.1, wp.2);
                        if boundary {
                            let below = ctx.get(wp.0, wp.1 - 1, wp.2);
                            if wp.1 >= ctx.g.min_y && !reg.is_solid(below) {
                                ctx.set(wp.0, wp.1, wp.2, cave_air);
                            } else if reg.is_solid(cur)
                                && reg.name_of(cur) != "minecraft:chest"
                            {
                                ctx.set(
                                    wp.0,
                                    wp.1,
                                    wp.2,
                                    if dy == -1 && rng.next_bound(4) != 0 { mossy } else { cobble },
                                );
                            }
                        } else {
                            let n = reg.name_of(cur);
                            if n != "minecraft:chest" && n != "minecraft:spawner" {
                                ctx.set(wp.0, wp.1, wp.2, cave_air);
                            }
                        }
                    }
                }
            }
            // spawner at center
            let spawner = reg.state_of("minecraft:spawner", None);
            if ctx.is_air(pos.0, pos.1, pos.2) {
                ctx.set(pos.0, pos.1, pos.2, spawner);
            }
            // chests (no loot table — chest block only)
            let chest = reg.state_of("minecraft:chest", None);
            for _ in 0..2 {
                for _ in 0..3 {
                    let cx = pos.0 + rng.next_bound(xr * 2 + 1) - xr;
                    let cz = pos.2 + rng.next_bound(zr * 2 + 1) - zr;
                    if !ctx.is_air(cx, pos.1, cz) {
                        continue;
                    }
                    let walls = [(-1i32, 0i32), (1, 0), (0, -1), (0, 1)]
                        .iter()
                        .filter(|(dx, dz)| reg.is_solid(ctx.get(cx + dx, pos.1, cz + dz)))
                        .count();
                    if walls == 1 {
                        ctx.set(cx, pos.1, cz, chest);
                        break;
                    }
                }
            }
        }
        Cf::Fallen { trunk } => {
            let Some(reg) = breg() else { return };
            let len = 2 + rng.next_bound(3) + rng.next_bound(3); // vanilla-ish log len
            let horiz = rng.next_bool();
            // find ground: scan down from pos to first solid
            let mut p = pos;
            for _ in 0..8 {
                let below = ctx.get(p.0, p.1 - 1, p.2);
                if ctx.is_air(p.0, p.1, p.2) && reg.is_solid(below) {
                    break;
                }
                if ctx.is_air(p.0, p.1, p.2) {
                    p.1 -= 1;
                } else {
                    p.1 += 1;
                }
                if p.1 < ctx.g.min_y + 2 || p.1 > ctx.g.min_y + ctx.g.height - 2 {
                    return;
                }
            }
            for i in 0..len {
                let (x, z) = if horiz { (p.0 + i, p.2) } else { (p.0, p.2 + i) };
                let below = ctx.get(x, p.1 - 1, z);
                if !(is_dirtish(reg, below) || reg.is_solid(below)) {
                    break;
                }
                let cur = ctx.get(x, p.1, z);
                if reg.is_air(cur) || reg.is_replaceable(cur) || reg.name_of(cur).contains("snow") {
                    let st = trunk.get(ctx, rng, x, p.1, z);
                    let st = reg.with_prop(st, "axis", if horiz { "x" } else { "z" });
                    ctx.set(x, p.1, z, st);
                }
            }
        }
    }
}

// ---------- entry point ----------

fn pop_seed(seed: u64, bx: i32, bz: i32) -> u64 {
    let mut r = Lcg::new(seed);
    let l = r.next_i64() | 1;
    let m = r.next_i64() | 1;
    ((bx as i64).wrapping_mul(l).wrapping_add((bz as i64).wrapping_mul(m)) as u64) ^ seed
}

/// populate chunk (sx,sz): evaluate the vanilla feature pipeline and merge
/// block-state overrides into Gen::feat_c.
/// Env-gated per-placed-feature timing, so the feature-population hotspot can be attributed to a
/// named feature instead of guessed. No behaviour change; the map is only written when
/// `FEAT_PROFILE` is set.
pub(crate) static FEAT_PROF: std::sync::OnceLock<std::sync::Mutex<HashMap<String, (u128, u64)>>> =
    std::sync::OnceLock::new();
pub(crate) fn feat_prof_record(name: &str, ns: u128) {
    let m = FEAT_PROF.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let mut g = m.lock().unwrap();
    let e = g.entry(name.to_string()).or_insert((0, 0));
    e.0 += ns;
    e.1 += 1;
}
pub(crate) fn feat_prof_dump() {
    let Some(m) = FEAT_PROF.get() else { return };
    let m = m.lock().unwrap();
    let mut v: Vec<(String, u128, u64)> = m.iter().map(|(k, (t, c))| (k.clone(), *t, *c)).collect();
    v.sort_by(|a, b| b.1.cmp(&a.1));
    let total: u128 = v.iter().map(|x| x.1).sum();
    eprintln!("FEATPROF total={:.1}ms", total as f64 / 1e6);
    eprintln!(
        "FEATPROF terrain reads={:.1}ms n={} ({:.0} ns/call)",
        crate::generation_context::READ_NS.load(std::sync::atomic::Ordering::Relaxed) as f64 / 1e6,
        crate::generation_context::READ_N.load(std::sync::atomic::Ordering::Relaxed),
        crate::generation_context::READ_NS.load(std::sync::atomic::Ordering::Relaxed) as f64
            / crate::generation_context::READ_N
                .load(std::sync::atomic::Ordering::Relaxed)
                .max(1) as f64
    );
    for (name, ns, cnt) in v.iter().take(25) {
        eprintln!("FEATPROF {:8.2}ms n={cnt:5}  {name}", *ns as f64 / 1e6);
    }
}

pub(crate) fn populate(g: &mut Gen, sx: i32, sz: i32) {
    // Diagnostic: MC_SPINE_NOFEAT=1 skips the whole feature layer so a block diff can
    // attribute the remainder to terrain or to features.
    if std::env::var("MC_SPINE_NOFEAT").is_ok() {
        return;
    }
    let fr = freg();
    if fr.steps == 0 || breg().is_none() {
        return;
    }
    // biomes of the 3x3 neighborhood (chunk center at y=0, quart sample)
    let mut biomes: Vec<String> = vec![];
    let mut seen: HashSet<String> = HashSet::new();
    for dx in -1..=1 {
        for dz in -1..=1 {
            if let Some(b) = g.biome_name_at((sx + dx) * 16 + 8, 0, (sz + dz) * 16 + 8) {
                if seen.insert(b.clone()) {
                    biomes.push(b);
                }
            }
        }
    }
    let pop = pop_seed(g.wseed(), sx * 16, sz * 16);
    let mut ctx = FC::new(g);
    let trace = std::env::var("FEAT_TRACE").is_ok();
    let profile = std::env::var("FEAT_PROFILE").is_ok();
    for step in 0..fr.steps {
        for (gi, name) in &fr.order[step] {
            let present = biomes.iter().any(|b| {
                fr.bsteps
                    .get(b)
                    .and_then(|s| s.get(step))
                    .map(|l| l.iter().any(|f| f == name))
                    .unwrap_or(false)
            });
            if !present {
                continue;
            }
            let Some(pf) = fr.placed.get(name) else { continue };
            let deco = pop
                .wrapping_add(*gi as u64)
                .wrapping_add(10_000u64.wrapping_mul(step as u64));
            let mut rng = Xor::seed(deco);
            let before = ctx.wr.len();
            let t0 = if profile { Some(std::time::Instant::now()) } else { None };
            gen_pf(pf, fr, &mut ctx, &mut rng, (sx * 16, 0, sz * 16));
            if let Some(t0) = t0 {
                feat_prof_record(name, t0.elapsed().as_nanos());
            }
            if trace {
                let mut h: u64 = 0;
                for (k, v) in ctx.wr.iter() {
                    h ^= (k.0 as u64).wrapping_mul(31).wrapping_add(k.1 as u64).rotate_left(7)
                        ^ (k.2 as u64) ^ (*v as u64).wrapping_mul(131);
                }
                eprintln!("TRACE step={} feat={} +{} writes h={:016x}", step, name, ctx.wr.len() - before, h);
            }
        }
    }
    for ((x, y, z), st) in std::mem::take(&mut ctx.wr) {
        g.feat_write(x, y, z, st);
    }
}
