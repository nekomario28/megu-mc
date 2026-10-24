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
use crate::json::{jarr, jbool, jf32, jint, jnum, jparse, js, jstr, J};
use crate::worldgen::*;
use std::collections::{HashMap, HashSet};

#[cfg(test)]
#[path = "generation_contract_tests.rs"]
mod generation_contract_tests;

// ---------- configured features ----------

enum FRef {
    /// a *configured* feature, referenced by name
    Name(String),
    /// a *placed* feature, referenced by name. The configured-feature contract
    /// (random_selector, simple_random_selector, weighted_random_selector,
    /// sequence, ...) holds `Holder<PlacedFeature>`, so the referenced feature's
    /// placement modifiers run too - with the same RNG. Treating the reference as
    /// a bare configured feature dropped those modifiers, which silently removed
    /// e.g. `block_predicate_filter(would_survive)`: every random_selector tree
    /// candidate then grew a tree where vanilla rejected it.
    Placed(String),
    Inline(Box<Cf>),
}
struct Pf {
    feat: FRef,
    mods: Vec<PM>,
    name: String,
}

enum Cf {
    Ore {
        size: i32,
        discard: f32,
        targets: Vec<(RT, u32)>,
        scattered: bool,
    },
    Disk {
        half: i32,
        radius: IP,
        bsp: Bsp,
        target: Pr,
    },
    SimpleBlock {
        to: Bsp,
        on: Vec<String>,
        in_: Vec<String>,
        under: Vec<String>,
    },
    Spring {
        fluid: u32,
        req_below: bool,
        rock: i32,
        hole: i32,
        valid: HashSet<String>,
    },
    /// `minecraft:sea_pickle` (`edj`, `SeaPickleFeature`): `count` attempts, each
    /// drawing two `nextInt(8)` apart for an offset in +-8, taking the OCEAN_FLOOR
    /// height there, then drawing the pickle count 1..4 *before* the two survival
    /// tests (the target must be water, and the block below must satisfy
    /// `SeaPickleBlock.mayPlaceOn`: a non-empty UP collision face, or magma).
    SeaPickle {
        count: IP,
    },
    /// `minecraft:ice_spike` (`ecq`, `IceSpikeFeature`), config `{}`: walk down
    /// through air to the first block, require it to be snow, then stack `radius`
    /// that shrinks linearly over `nextInt(4) + 7` layers, with a rare floating
    /// spike and a downward drill that replaces snow/powder/air/ice with packed ice
    /// until y = 50.
    IceSpike,
    /// `minecraft:root_system` (`edg`, `RootSystemFeature`): from the origin, walk
    /// up while the position still passes `allowed_tree_position` and has
    /// `required_vertical_space_for_tree` of air-or-water above it, then try to run
    /// the nested placed feature (the "tree"); on success the same column gets a
    /// root column of `rooted_dirt` up to that height, and then a field of hanging
    /// roots is scattered around the origin.
    RootSystem(Box<RootSystemCfg>),
    /// `minecraft:desert_well` (`ebw`), config `{}`: walk down from the origin's
    /// above-cell while it is empty, require `sand` (the config's
    /// `BlockPredicate.matchesBlocks(SAND)`), then build a slab base plate, a water
    /// column with a sand floor, a rim, four posts, a slab roof on sandstone pillars,
    /// and two `suspicious_sand` blocks at a random horizontal neighbour.
    ///
    /// Known gap, the same one the port's chests carry: the blocks are placed but the
    /// archaeology loot table is not attached to the suspicious sand (no block-entity
    /// loot support), so brushing it yields nothing.
    DesertWell,
    /// `minecraft:large_dripstone` (`ecu`), config `eel`.
    LargeDripstone(Box<LargeDripstoneCfg>),
    /// `minecraft:dripstone_cluster` (`eby`), config `eeg`.
    DripstoneCluster(Box<DripstoneClusterCfg>),
    /// `minecraft:iceberg` (`ecr`), config `{state}`.
    Iceberg {
        state: u32,
    },
    Geode(Box<GeodeCfg>),
    RandSel(Vec<(Pf, f32)>, Option<Box<Pf>>),
    /// `minecraft:random_patch` (`edc`): `tries` times, pick a position by drawing
    /// `nextInt(xz+1) - nextInt(xz+1)` per horizontal axis and the same with
    /// `y_spread + 1` vertically, then run the nested *placed* feature there (its own
    /// modifiers, on this RNG). Skipping the type left every grass/flower/mushroom
    /// patch unplaced (measured: 34 of the pack's 60 configured-feature types had no
    /// arm, and `short_grass` was 0 cells against vanilla's 102).
    RandomPatch {
        feature: Box<Pf>,
        tries: i32,
        xz: i32,
        y: i32,
    },
    /// `minecraft:seagrass` (`edk`): draw dx/dz in ±8, take the OCEAN_FLOOR height
    /// there, and if that cell is water draw `nextDouble() < probability` to choose
    /// tall or short seagrass. The block's own survival rule (`SeagrassBlock`, `dmx`)
    /// is "the block below has a sturdy top face and is not magma" - not the dirt rule.
    Seagrass {
        prob: f64,
    },
    /// Kelp grows upward from OCEAN_FLOOR (`BlockPos.above`, obfuscated `d`), with one
    /// height draw and an age draw only when placing the head (`KelpFeature`, `ecs`).
    Kelp,
    SimpleRand(Vec<Pf>),
    WeightedSel(Vec<(Pf, i32)>),
    BoolSel(Pf, Pf),
    Seq(Vec<Pf>),
    VegPatch {
        depth: IP,
        extra_bottom: f32,
        extra_edge: f32,
        replaceable: String,
        ground: Bsp,
        floor: bool,
        chance: f32,
        vfeat: Box<Pf>,
        xz: IP,
        vrange: i32,
        waterlog: bool,
    },
    BlockColumn {
        layers: Vec<(IP, Bsp)>,
        down: bool,
        allowed: Option<Pr>,
        prio: bool,
    },
    BlockPile(Bsp),
    Lake {
        fluid: Bsp,
        barrier: Bsp,
    },
    BlueIce,
    BlockBlob {
        state: u32,
    },
    UnderMagma {
        floor: i32,
        prob: f32,
        radius: i32,
    },
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
    Fallen {
        trunk: Bsp,
    },
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
            feat: FRef::Placed(ns_name(s)),
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
            match p_mod(m) {
                Some(pm) => mods.push(pm),
                None => {
                    // A dropped modifier silently changes every later draw, so
                    // make it visible instead of guessing.
                    if std::env::var("PF_UNKNOWN").is_ok() {
                        eprintln!(
                            "PFUNKNOWN {} modifier={}",
                            jstr(j, "feature").unwrap_or_default(),
                            jstr(m, "type").unwrap_or_default()
                        );
                    }
                }
            }
        }
    }
    Some(Pf {
        feat: fref,
        mods,
        name: String::new(),
    })
}

/// `minecraft:geode` (`eck`, `GeodeFeature`), read from the decompiled 1.21.1
/// class and its config records (`eej`/`dyv`/`dyx`/`dyw` in the mappings):
///
/// - `distribution_points.sample` points are drawn, each offset by
///   `outer_wall_distance` on all three axes, rejected outright when more than
///   `invalid_blocks_threshold` of them land in air or in `invalid_blocks`;
/// - `d0 = points / outer_wall_distance.max` and the layer thresholds are
///   `1/sqrt(layer + d0)` for inner/middle/outer (the crack uses
///   `base_crack_size + random.nextDouble()/2` and adds `d0` only when there are
///   more than three points);
/// - every block of the `[min_gen_offset, max_gen_offset]` cube gets
///   `d6 = sum over points of 1/sqrt(distSqr + point_offset) + noise` where the
///   *noise term is added once per point*, `d7` the same over the crack points;
/// - `d6 < d4` is outside, then the branches are `d1` (the filling provider: the
///   hollow core, and with a crack the six neighbours are replaced by their
///   fluid), `d2` (inner, or the alternate inner at `use_alternate_layer0_chance`
///   - that same draw also decides whether the block becomes an inner placement
///     to hang a crystal on), `d3` (middle) and `d4` (outer);
/// - a block is only written where the existing block is *not* in
///   `cannot_replace`, but the provider is asked for its state either way, which
///   is where the draw happens;
/// - crystals are placed from `inner_placements`, aimed at each of the six
///   directions in turn, `WATERLOGGED` from the target's fluid, and stop at the
///   first direction whose target is air or a water source.
struct GeodeCfg {
    filling: Bsp,
    inner: Bsp,
    alt_inner: Bsp,
    middle: Bsp,
    outer: Bsp,
    placements: Vec<u32>,
    cannot_replace: String,
    invalid: String,
    layers: [f64; 4], // filling, inner, middle, outer
    crack_chance: f64,
    crack_size: f64,
    crack_offset: i32,
    use_alt_layer0: f64,
    use_potential_placements: f64,
    placements_require_alt: bool,
    outer_wall_distance: IP,
    distribution_points: IP,
    point_offset: IP,
    min_gen_offset: i32,
    max_gen_offset: i32,
    noise_multiplier: f64,
    invalid_blocks_threshold: i32,
}

// ---------- large dripstone (`ecu`) ----------

/// `LargeDripstoneConfiguration` (`eel`): nine fields, all used.
pub(crate) struct LargeDripstoneCfg {
    /// `floor_to_ceiling_search_range` (codec default 30), the span scan range.
    range: i32,
    /// `column_radius`: the widest the spike may be, clamped by the cave's height.
    radius: IP,
    /// `height_scale`: sampled once per spike, scales the whole taper.
    height_scale: FP,
    /// `max_column_radius_to_cave_height_ratio`: caps the radius from the span height.
    ratio: f32,
    /// `stalactite_bluntness` / `stalagmite_bluntness`: sampled per direction.
    stalactite_bluntness: FP,
    stalagmite_bluntness: FP,
    /// `wind_speed`: sampled only when both spikes are big enough to sway.
    wind_speed: FP,
    /// `min_radius_for_wind` / `min_bluntness_for_wind`: the sway gate.
    min_radius_for_wind: i32,
    min_bluntness_for_wind: f32,
}

/// `DripstoneUtils.getDripstoneHeight(double,double,double,double)` (`ebz.a`): the
/// taper both dripstone features use. `d` is the distance from the column's axis,
/// `max_radius` its radius, `height_scale` and `bluntness` the config's samples.
/// Vanilla also declares an unused local `0.384` here; the literal appears twice in the
/// expression, which is where that came from.
fn drip_taper(mut d: f64, max_radius: f64, height_scale: f64, bluntness: f64) -> f64 {
    if d < bluntness {
        d = bluntness;
    }
    let t = d / max_radius * 0.384;
    let a = 0.75 * t.powf(1.3333333333333333);
    let b = t.powf(0.6666666666666666);
    let c = 0.3333333333333333 * t.ln();
    let v = height_scale * (a - b - c);
    v.max(0.0) / 0.384 * max_radius
}

/// `DripstoneUtils.isCircleMostlyEmbeddedInStone` (`ebz.a(dds,jd,int)`): the support
/// test both spike searches use - the centre must be solid, and six sample points per
/// unit of radius around it must all be solid too. The loop accumulates in f32 and the
/// step depends on the radius, so the number of samples is part of the behaviour.
fn drip_circle_is_stone(ctx: &mut FC, pos: (i32, i32, i32), radius: i32) -> bool {
    if drip_is_empty_or_water_or_lava(ctx, pos) {
        return false;
    }
    let step = 6.0f32 / radius as f32;
    let mut a = 0.0f32;
    while a < (std::f64::consts::PI * 2.0) as f32 {
        let dx = (mth_cos(a) * radius as f32) as i32;
        let dz = (mth_sin(a) * radius as f32) as i32;
        if drip_is_empty_or_water_or_lava(ctx, (pos.0 + dx, pos.1, pos.2 + dz)) {
            return false;
        }
        a += step;
    }
    true
}

/// `LargeDripstoneFeature$WindOffsetter` (`ecu$b`): the sway. `none()` carries no
/// vector (so every offset is the identity); otherwise the displacement is the wind
/// vector scaled by the distance from the feature's own y, floored.
struct DripWind {
    y: i32,
    v: Option<(f64, f64)>,
}
impl DripWind {
    fn none() -> DripWind {
        DripWind { y: 0, v: None }
    }
    fn new(y: i32, rng: &mut Xor, speed: f32) -> DripWind {
        let a = mth_next_f32(rng, 0.0, std::f32::consts::PI);
        // the components are f32 products widened to the Vec3's doubles
        DripWind {
            y,
            v: Some(((mth_cos(a) * speed) as f64, (mth_sin(a) * speed) as f64)),
        }
    }
    fn at(&self, p: (i32, i32, i32)) -> (i32, i32, i32) {
        match self.v {
            None => p,
            Some((vx, vz)) => {
                let dy = (self.y - p.1) as f64;
                (
                    p.0 + (vx * dy).floor() as i32,
                    p.1,
                    p.2 + (vz * dy).floor() as i32,
                )
            }
        }
    }
}

/// `LargeDripstoneFeature$LargeDripstone` (`ecu$a`): one spike, either hanging from
/// the span's ceiling (`up == false`) or standing on its floor (`up == true`).
struct DripSpike {
    pos: (i32, i32, i32),
    up: bool,
    radius: i32,
    /// The sampled bluntness / height scale, widened to f64 as the fields are.
    bluntness: f64,
    height_scale: f64,
}
impl DripSpike {
    /// `getHeight(float)` (`ecu$a.a(F)`): truncate the taper at a distance from the axis.
    fn height_at(&self, d: f32) -> i32 {
        drip_taper(
            d as f64,
            self.radius as f64,
            self.height_scale,
            self.bluntness,
        ) as i32
    }
    /// `isValid(eel)` (`ecu$a.a(eel)`): the wind only blows on big, blunt spikes.
    fn valid(&self, cfg: &LargeDripstoneCfg) -> bool {
        self.radius >= cfg.min_radius_for_wind
            && self.bluntness >= cfg.min_bluntness_for_wind as f64
    }
    /// `LargeDripstoneFeature$LargeDripstone.place(WorldGenLevel, WindOffsetter)`
    /// (`ecu$a.a(dds,ecu$b)`): find the solid cell the spike grows out of, walking away
    /// from the cave (down for a stalagmite, up for a stalactite) at most
    /// `min(10, getHeight(0))` steps, halving the radius when nothing is found.
    fn find_base(&mut self, ctx: &mut FC, wind: &DripWind) -> bool {
        let Some(reg) = breg() else { return false };
        while self.radius > 1 {
            let mut p = self.pos;
            let steps = self.height_at(0.0).min(10);
            for _ in 0..steps {
                if reg.name_of(ctx.get(p.0, p.1, p.2)) == "minecraft:lava" {
                    return false;
                }
                if drip_circle_is_stone(ctx, wind.at(p), self.radius) {
                    self.pos = p;
                    return true;
                }
                p.1 += if self.up { -1 } else { 1 };
            }
            self.radius /= 2;
        }
        false
    }
    /// `LargeDripstoneFeature$LargeDripstone.placeDripstone(WorldGenLevel,
    /// RandomSource, WindOffsetter)` (`ecu$a.a(dds,ayw,ecu$b)`): the body. Every cell of
    /// the disc gets a taper height, a 1-in-5 chance of a random 0.8..1.0 multiplier, and
    /// dripstone blocks down (or up) until the taper runs out - stopping if it walks
    /// into base stone after having placed something.
    fn build(&self, ctx: &mut FC, rng: &mut Xor, wind: &DripWind) {
        let Some(reg) = breg() else { return };
        let drip = reg.state_of("minecraft:dripstone_block", None);
        for dx in -self.radius..=self.radius {
            for dz in -self.radius..=self.radius {
                // `Mth.sqrt(float)` is a double sqrt narrowed to float
                let dist = ((dx * dx + dz * dz) as f64).sqrt() as f32;
                if dist > self.radius as f32 {
                    continue;
                }
                let mut h = self.height_at(dist);
                if h <= 0 {
                    continue;
                }
                if rng.next_f32() < 0.2 {
                    h = (h as f32 * mth_next_f32(rng, 0.8, 1.0)) as i32;
                }
                let mut p = (self.pos.0 + dx, self.pos.1, self.pos.2 + dz);
                let mut placed = false;
                let surface = if self.up {
                    ctx.hm_ws(p.0, p.2)
                } else {
                    i32::MAX
                };
                let mut i = 0;
                while i < h && p.1 < surface {
                    let q = wind.at(p);
                    let st = ctx.get(q.0, q.1, q.2);
                    // `ebz.b(dcx,jd)` - two arguments, so `isEmptyOrWaterOrLava`, not the
                    // one-argument `ebz.b(dtc)` (`isDripstoneBase`): the spike is built
                    // *in* the empty cell and stops when it reaches base stone.
                    if drip_is_empty_or_water_or_lava(ctx, q) {
                        placed = true;
                        ctx.set(q.0, q.1, q.2, drip);
                    } else if placed && reg.in_tag(st, "minecraft:base_stone_overworld") {
                        break;
                    }
                    p.1 += if self.up { 1 } else { -1 };
                    i += 1;
                }
            }
        }
    }
}

/// `minecraft:large_dripstone` (`ecu`, `LargeDripstoneFeature`).
///
/// The origin's column must be a *two-sided* `Column` (both a floor and a ceiling) at
/// least 4 cells tall - that is the cave the spikes grow into. The radius is the cave
/// height times `max_column_radius_to_cave_height_ratio`, clamped into the config's
/// `column_radius` range, and then drawn once. Two spikes are made from it (a
/// stalactite at `ceiling - 1` and a stalagmite at `floor + 1`), each finding its own
/// solid base, and the wind offsetter is built only if both are big and blunt enough.
///
/// Draw order: the radius, then the first spike's bluntness + height scale, then the
/// second's, then the wind's speed + angle (only when the gate passes), then the builds.
///
/// Note: the jar also carries `LargeDripstoneFeature.placeColumn`
/// (`ecu.a(dds,jd,dyo$b,ecu$b)`), which writes `Blocks.DIAMOND_BLOCK`, `GOLD_BLOCK` and
/// `CREEPER_HEAD` - and nothing calls it (no call site for its descriptor exists in the
/// class, and `place` only calls the two methods above). It is dead in the reference
/// build, so it is deliberately not ported; what *is* live writes `dripstone_block`.
fn gen_large_dripstone(
    ctx: &mut FC,
    rng: &mut Xor,
    origin: (i32, i32, i32),
    cfg: &LargeDripstoneCfg,
) {
    if !drip_is_empty_or_water(ctx, origin) {
        return;
    }
    let Some(col) = drip_column(ctx, origin, cfg.range, DripEnd::DripstoneBaseOrLava) else {
        return;
    };
    let (Some(ceiling), Some(floor)) = (col.ceiling, col.floor) else {
        return;
    };
    let height = ceiling - floor - 1;
    if height < 4 {
        return;
    }
    let est = (height as f32 * cfg.ratio) as i32;
    let est = est.clamp(cfg.radius.min_inclusive(), cfg.radius.max_inclusive());
    let radius = rng.next_inbetween(cfg.radius.min_inclusive(), est);

    let mut stalactite = DripSpike {
        pos: (origin.0, ceiling - 1, origin.2),
        up: false,
        radius,
        bluntness: cfg.stalactite_bluntness.get(rng) as f64,
        height_scale: cfg.height_scale.get(rng) as f64,
    };
    let mut stalagmite = DripSpike {
        pos: (origin.0, floor + 1, origin.2),
        up: true,
        radius,
        bluntness: cfg.stalagmite_bluntness.get(rng) as f64,
        height_scale: cfg.height_scale.get(rng) as f64,
    };
    let wind = if stalactite.valid(cfg) && stalagmite.valid(cfg) {
        let speed = cfg.wind_speed.get(rng);
        DripWind::new(origin.1, rng, speed)
    } else {
        DripWind::none()
    };

    let ok_hang = stalactite.find_base(ctx, &wind);
    let ok_stand = stalagmite.find_base(ctx, &wind);
    if ok_hang {
        stalactite.build(ctx, rng, &wind);
    }
    if ok_stand {
        stalagmite.build(ctx, rng, &wind);
    }
}

// ---------- dripstone cluster (`eby`) ----------

/// `DripstoneClusterConfiguration` (`eeg`). Every field is used, and three of them are
/// providers whose draw count depends on the pack's chosen type.
pub(crate) struct DripstoneClusterCfg {
    /// `floor_to_ceiling_search_range`: how far the empty/water span is scanned.
    range: i32,
    /// `height`: the tip length before the column's own span caps it.
    height: IP,
    /// `radius`: drawn twice (x, then z), so a cluster is an ellipse.
    radius: IP,
    /// `max_stalagmite_stalactite_height_diff`: how far the two tips may differ.
    max_diff: i32,
    /// `height_deviation`: the Gaussian deviation of a tip's length.
    height_deviation: i32,
    /// `dripstone_block_layer_thickness`: layers of dripstone block behind a tip.
    thickness: IP,
    /// `density`: `nextFloat() > density` gives the column no tips at all.
    density: FP,
    /// `wetness`: chance the floor cell becomes a water pool.
    wetness: FP,
    /// `chance_of_dripstone_column_at_max_distance_from_center`.
    chance: f32,
    /// `max_distance_from_edge_affecting_chance_of_dripstone_column`.
    max_dist_edge: i32,
    /// `max_distance_from_center_affecting_height_bias`.
    max_dist_center: i32,
}

/// `Column` (`dyo`): the span of empty/water cells a column sits in. `ceiling` is the
/// first neither-empty-nor-water cell found scanning **up**, `floor` the one found
/// scanning down - so `floor < pos.y < ceiling` whenever both are present.
#[derive(Clone, Copy)]
struct DripCol {
    ceiling: Option<i32>,
    floor: Option<i32>,
}
impl DripCol {
    /// `Column.getHeight()`: only a two-sided column has one.
    fn height(&self) -> Option<i32> {
        match (self.ceiling, self.floor) {
            (Some(c), Some(f)) => Some(c - f - 1),
            _ => None,
        }
    }
}

/// `minecraft:dripstone_cluster` (`eby`, `DripstoneClusterFeature`).
///
/// A cluster is an ellipse of columns (the radius is drawn separately for x and z).
/// Each column finds the empty/water span it sits in, may turn its floor cell into a
/// water pool, then may hang a stalactite from the span's ceiling and stand a
/// stalagmite on its floor. When the two tips would overlap, their meeting point is
/// drawn and both lengths are shortened to it.
///
/// Draw order per column (it matters - the pack's `wetness` and `density` are
/// providers, and both `clamped_normal` and `uniform` floats draw):
/// `nextFloat < wetness`, `nextDouble < chance`, [thickness, height gate + Gaussian],
/// `nextDouble < chance`, [thickness, (randomBetweenInclusive | height gate +
/// Gaussian)], (meeting point), `nextBoolean`.
fn gen_dripstone_cluster(
    ctx: &mut FC,
    rng: &mut Xor,
    origin: (i32, i32, i32),
    cfg: &DripstoneClusterCfg,
) {
    if !drip_is_empty_or_water(ctx, origin) {
        return;
    }
    let height = cfg.height.get(rng);
    let wetness = cfg.wetness.get(rng);
    let density = cfg.density.get(rng);
    let rx = cfg.radius.get(rng);
    let rz = cfg.radius.get(rng);
    for dx in -rx..=rx {
        for dz in -rz..=rz {
            let chance = drip_chance(rx, rz, dx, dz, cfg);
            let p = (origin.0 + dx, origin.1, origin.2 + dz);
            drip_place_column(
                ctx,
                rng,
                p,
                dx,
                dz,
                wetness,
                chance as f64,
                height,
                density,
                cfg,
            );
        }
    }
}

/// `DripstoneUtils.isEmptyOrWater(LevelAccessor, BlockPos)` (`ebz.a(dcx,jd)`), which is
/// also the `start` predicate of the column scan.
fn drip_is_empty_or_water(ctx: &mut FC, pos: (i32, i32, i32)) -> bool {
    let Some(reg) = breg() else { return false };
    let st = ctx.get(pos.0, pos.1, pos.2);
    reg.is_air(st) || reg.name_of(st) == "minecraft:water"
}

/// `DripstoneUtils.isNeitherEmptyNorWater` (`ebz.d`): the column scan's `end`
/// predicate. Note lava counts as neither, so a lava lake can end a column.
fn drip_is_neither_empty_nor_water(ctx: &mut FC, pos: (i32, i32, i32)) -> bool {
    let Some(reg) = breg() else { return false };
    let st = ctx.get(pos.0, pos.1, pos.2);
    !reg.is_air(st) && reg.name_of(st) != "minecraft:water"
}

/// The `end` predicate of a `Column` scan: the cluster stops at the first cell that is
/// neither empty nor water, the large dripstone at the first dripstone base or lava
/// cell (`LargeDripstoneFeature` passes `DripstoneUtils::isDripstoneBaseOrLava`).
#[derive(Clone, Copy, PartialEq)]
enum DripEnd {
    NeitherEmptyNorWater,
    DripstoneBaseOrLava,
}

/// One direction of `Column.scanDirection` (`dyo.a`): walk while the cell is still
/// empty-or-water (at most `range - 1` steps), then accept the cell we stopped on only
/// if it satisfies the end predicate.
fn drip_scan(
    ctx: &mut FC,
    pos: (i32, i32, i32),
    range: i32,
    up: bool,
    end: DripEnd,
) -> Option<i32> {
    let mut y = pos.1;
    let mut i = 1;
    while i < range && drip_is_empty_or_water(ctx, (pos.0, y, pos.2)) {
        y += if up { 1 } else { -1 };
        i += 1;
    }
    let hit = match end {
        DripEnd::NeitherEmptyNorWater => drip_is_neither_empty_nor_water(ctx, (pos.0, y, pos.2)),
        DripEnd::DripstoneBaseOrLava => drip_is_dripstone_base_or_lava(ctx, (pos.0, y, pos.2)),
    };
    if hit {
        Some(y)
    } else {
        None
    }
}

/// `Column.getColumnAt` (`dyo.a`): the up scan is the ceiling, the down scan the floor
/// (that is the field order of `Column.create(floor, ceiling)`).
fn drip_column(ctx: &mut FC, pos: (i32, i32, i32), range: i32, end: DripEnd) -> Option<DripCol> {
    if !drip_is_empty_or_water(ctx, pos) {
        return None;
    }
    let ceiling = drip_scan(ctx, pos, range, true, end);
    let floor = drip_scan(ctx, pos, range, false, end);
    Some(DripCol { ceiling, floor })
}

/// `DripstoneUtils.isEmptyOrWaterOrLava` (`ebz.b(dcx,jd)`): the circle test's predicate.
fn drip_is_empty_or_water_or_lava(ctx: &mut FC, pos: (i32, i32, i32)) -> bool {
    let Some(reg) = breg() else { return false };
    let st = ctx.get(pos.0, pos.1, pos.2);
    reg.is_air(st) || reg.name_of(st) == "minecraft:water" || reg.name_of(st) == "minecraft:lava"
}

/// `DripstoneUtils.isDripstoneBase(BlockState)` (`ebz.b(dtc)`): dripstone block, or a
/// block in `dripstone_replaceable_blocks` (which includes the overworld base stones).
fn drip_is_dripstone_base(reg: &BReg, st: u32) -> bool {
    reg.name_of(st) == "minecraft:dripstone_block"
        || reg.in_tag(st, "minecraft:dripstone_replaceable_blocks")
}

/// `DripstoneUtils.isDripstoneBaseOrLava(BlockState)` (`ebz.a(dtc)`): the large
/// dripstone's column end predicate.
fn drip_is_dripstone_base_or_lava(ctx: &mut FC, pos: (i32, i32, i32)) -> bool {
    let Some(reg) = breg() else { return false };
    let st = ctx.get(pos.0, pos.1, pos.2);
    drip_is_dripstone_base(reg, st) || reg.name_of(st) == "minecraft:lava"
}

/// `DripstoneClusterFeature.getChanceOfStalagmiteOrStalactite`
/// (`eby.a(int,int,int,int,eeg)`): the config's chance at the cluster's edge rising to
/// 1.0 `max_dist_edge` cells inside it (float `clampedMap` - the casts are in the
/// bytecode too).
fn drip_chance(rx: i32, rz: i32, dx: i32, dz: i32, cfg: &DripstoneClusterCfg) -> f32 {
    let a = rx - dx.abs();
    let b = rz - dz.abs();
    let m = a.min(b);
    clamped_map_f32(m as f32, 0.0, cfg.max_dist_edge as f32, cfg.chance, 1.0)
}

/// `DripstoneClusterFeature.getDripstoneHeight` (`eby.a(ayw,int,int,float,int,eeg)`):
/// a `nextFloat` gate against the density, then a biased Gaussian whose mean falls from
/// half the maximum at the cluster's centre to zero at `max_dist_center`.
fn drip_height(
    rng: &mut Xor,
    dx: i32,
    dz: i32,
    density: f32,
    max: i32,
    cfg: &DripstoneClusterCfg,
) -> i32 {
    if rng.next_f32() > density {
        return 0;
    }
    let dist = dx.abs() + dz.abs();
    let mean = clamped_map_f64(
        dist as f64,
        0.0,
        cfg.max_dist_center as f64,
        max as f64 / 2.0,
        0.0,
    ) as f32;
    fp_clamped_normal(rng, mean, cfg.height_deviation as f32, 0.0, max as f32) as i32
}

/// `DripstoneClusterFeature.canBeAdjacentToWater` (`eby.a(dcx,jd)`): base stone, or a
/// water fluid state (the port models water as a block, so the fluid test is a name
/// test).
fn drip_can_be_adjacent_to_water(ctx: &mut FC, pos: (i32, i32, i32)) -> bool {
    let Some(reg) = breg() else { return false };
    let st = ctx.get(pos.0, pos.1, pos.2);
    reg.in_tag(st, "minecraft:base_stone_overworld") || reg.name_of(st) == "minecraft:water"
}

/// `DripstoneClusterFeature.isLava` (`eby.a(dcz,jd)`). Note the decompiled call sites
/// cast to `dcz` (LevelReader) to pick this overload: `eby` has a *second* `a(dcx,jd)`
/// (`canBeAdjacentToWater`), and reading the wrong one makes every stone ceiling reject
/// its own stalactite - which is how this was found (the fixture grew nothing).
fn drip_is_lava(ctx: &mut FC, pos: (i32, i32, i32)) -> bool {
    let Some(reg) = breg() else { return false };
    reg.name_of(ctx.get(pos.0, pos.1, pos.2)) == "minecraft:lava"
}

/// `DripstoneClusterFeature.canPlacePool` (`eby.b`). The water-below test is *negated*
/// in the bytecode as it appears here: a cell that already has water under it is
/// rejected, and the cell must instead be base stone / water on all four sides and
/// above.
fn drip_can_place_pool(ctx: &mut FC, pos: (i32, i32, i32)) -> bool {
    let Some(reg) = breg() else { return false };
    let n = reg.name_of(ctx.get(pos.0, pos.1, pos.2));
    if n == "minecraft:water"
        || n == "minecraft:dripstone_block"
        || n == "minecraft:pointed_dripstone"
    {
        return false;
    }
    if reg.name_of(ctx.get(pos.0, pos.1 - 1, pos.2)) == "minecraft:water" {
        return false;
    }
    // `Direction.Plane.HORIZONTAL` is NORTH, EAST, SOUTH, WEST.
    for (dx, dz) in [(0, -1), (1, 0), (0, 1), (-1, 0)] {
        if !drip_can_be_adjacent_to_water(ctx, (pos.0 + dx, pos.1, pos.2 + dz)) {
            return false;
        }
    }
    drip_can_be_adjacent_to_water(ctx, (pos.0, pos.1 + 1, pos.2))
}

/// `DripstoneUtils.placeDripstoneBlockIfPossible` (`ebz.c(dcx,jd)`): a block in the
/// `dripstone_replaceable_blocks` tag becomes a dripstone block, and nothing else does.
/// The boolean *is* the loop's continue condition in `replaceBlocks...`.
fn drip_place_dripstone_block(ctx: &mut FC, pos: (i32, i32, i32)) -> bool {
    let Some(reg) = breg() else { return false };
    let st = ctx.get(pos.0, pos.1, pos.2);
    if reg.in_tag(st, "minecraft:dripstone_replaceable_blocks") {
        let block = reg.state_of("minecraft:dripstone_block", None);
        ctx.set(pos.0, pos.1, pos.2, block);
        true
    } else {
        false
    }
}

/// `DripstoneClusterFeature.replaceBlocksWithDripstoneBlocks` (`eby.a(dds,jd,int,ji)`).
fn drip_replace_blocks_with_dripstone_blocks(
    ctx: &mut FC,
    pos: (i32, i32, i32),
    thickness: i32,
    up: bool,
) {
    let mut p = pos;
    for _ in 0..thickness {
        if !drip_place_dripstone_block(ctx, p) {
            return;
        }
        p.1 += if up { 1 } else { -1 };
    }
}

/// `DripstoneUtils.createPointedDripstone` (`ebz.a(ji,dtz)`).
fn drip_pointed(reg: &BReg, up: bool, thickness: &str) -> u32 {
    let st = reg.state_of("minecraft:pointed_dripstone", None);
    let st = reg.with_prop(st, "vertical_direction", if up { "up" } else { "down" });
    reg.with_prop(st, "thickness", thickness)
}

/// `DripstoneUtils.buildBaseToTipColumn` (`ebz.a(ji,int,boolean,Consumer)`): base,
/// then `height - 3` middles, then frustum, then the tip (`tip_merge` when the two
/// tips of a column meet). Each layer is placed where the consumer's mutable position
/// currently is, then the position moves on.
fn drip_build_base_to_tip(ctx: &mut FC, pos: (i32, i32, i32), up: bool, height: i32, merge: bool) {
    let Some(reg) = breg() else { return };
    let mut steps: Vec<&str> = Vec::new();
    if height >= 3 {
        steps.push("base");
        steps.extend(std::iter::repeat_n("middle", (height - 3) as usize));
    }
    if height >= 2 {
        steps.push("frustum");
    }
    if height >= 1 {
        steps.push(if merge { "tip_merge" } else { "tip" });
    }
    let dir = if up { 1 } else { -1 };
    let mut m = pos;
    for t in steps {
        let st = drip_pointed(reg, up, t);
        // `state.setValue(WATERLOGGED, level.isWaterAt(m))`
        let wl = reg.name_of(ctx.get(m.0, m.1, m.2)) == "minecraft:water";
        let st = reg.with_prop(st, "waterlogged", if wl { "true" } else { "false" });
        ctx.set(m.0, m.1, m.2, st);
        m.1 += dir;
    }
}

/// `DripstoneUtils.growPointedDripstone` (`ebz.a(dcx,jd,ji,int,boolean)`): the tip only
/// grows when the cell opposite the growth direction is dripstone block or in the
/// `dripstone_replaceable_blocks` tag (`isDripstoneBase`).
fn drip_grow_pointed_dripstone(
    ctx: &mut FC,
    pos: (i32, i32, i32),
    up: bool,
    height: i32,
    merge: bool,
) {
    let Some(reg) = breg() else { return };
    let base = ctx.get(pos.0, pos.1 + if up { -1 } else { 1 }, pos.2);
    if !drip_is_dripstone_base(reg, base) {
        return;
    }
    drip_build_base_to_tip(ctx, pos, up, height, merge);
}

/// `DripstoneClusterFeature.placeColumn` (`eby.a(dds,ayw,jd,int,int,float,double,int,
/// float,eeg)`) - one column of the cluster.
#[allow(clippy::too_many_arguments)]
fn drip_place_column(
    ctx: &mut FC,
    rng: &mut Xor,
    pos: (i32, i32, i32),
    dx: i32,
    dz: i32,
    wetness: f32,
    chance: f64,
    height: i32,
    density: f32,
    cfg: &DripstoneClusterCfg,
) {
    let Some(reg) = breg() else { return };
    let Some(col) = drip_column(ctx, pos, cfg.range, DripEnd::NeitherEmptyNorWater) else {
        return;
    };
    let (ceiling, floor) = (col.ceiling, col.floor);
    if ceiling.is_none() && floor.is_none() {
        return;
    }

    let wet = rng.next_f32() < wetness;
    let col2 = match (wet, floor) {
        (true, Some(f)) if drip_can_place_pool(ctx, (pos.0, f, pos.2)) => {
            // The floor cell itself becomes water and the column's floor moves one
            // down, so a later stalagmite stands in the pool.
            let water = reg.state_of("minecraft:water", None);
            ctx.set(pos.0, f, pos.2, water);
            DripCol {
                ceiling,
                floor: Some(f - 1),
            }
        }
        _ => col,
    };
    let floor2 = col2.floor;

    let roll1 = rng.next_f64() < chance;
    let h_ceil = match ceiling {
        Some(c) if roll1 && !drip_is_lava(ctx, (pos.0, c, pos.2)) => {
            let t = cfg.thickness.get(rng);
            drip_replace_blocks_with_dripstone_blocks(ctx, (pos.0, c, pos.2), t, true);
            let max = match floor2 {
                Some(f) => height.min(c - f),
                None => height,
            };
            drip_height(rng, dx, dz, density, max, cfg)
        }
        _ => 0,
    };

    let roll2 = rng.next_f64() < chance;
    let h_floor = match floor2 {
        Some(f) if roll2 && !drip_is_lava(ctx, (pos.0, f, pos.2)) => {
            let t = cfg.thickness.get(rng);
            drip_replace_blocks_with_dripstone_blocks(ctx, (pos.0, f, pos.2), t, false);
            match ceiling {
                Some(_) => (h_ceil + rng.next_inbetween(-cfg.max_diff, cfg.max_diff)).max(0),
                None => drip_height(rng, dx, dz, density, height, cfg),
            }
        }
        _ => 0,
    };

    // If the two tips would meet, draw the meeting point inside the span both can
    // reach and shorten each side to it.
    let (h1, h2) = match (ceiling, floor2) {
        (Some(c), Some(f)) if c - h_ceil <= f + h_floor => {
            let lo = (c - h_ceil).max(f + 1);
            let hi = (f + h_floor).min(c - 1);
            let pick = rng.next_inbetween(lo, hi + 1);
            (c - pick, pick - 1 - f)
        }
        _ => (h_ceil, h_floor),
    };

    let merge = rng.next_bool()
        && h1 > 0
        && h2 > 0
        && col2.height().is_some()
        && h1 + h2 == col2.height().unwrap();

    if let Some(c) = ceiling {
        drip_grow_pointed_dripstone(ctx, (pos.0, c - 1, pos.2), false, h1, merge);
    }
    if let Some(f) = floor2 {
        drip_grow_pointed_dripstone(ctx, (pos.0, f + 1, pos.2), true, h2, merge);
    }
}

/// `minecraft:iceberg` (`ecr`, `IcebergFeature`), config `{state}` (packed_ice for
/// `iceberg_packed`, blue_ice for `iceberg_blue`). The origin is moved to sea level,
/// then two shapes are possible: a "tall" one (`flag1`, chosen by a `nextDouble`) and
/// a flat one, each built from a height-dependent radius and then carved back with a
/// signed-distance test (a circle for the flat shape, an ellipse rotated by
/// `angle` for the tall one).
///
/// Letters resolved in the mappings first - this feature is dense with them:
/// `Blocks.a/G/L/dN/dO/dP/iC/mW` are AIR / WATER / GRAVEL / SNOW / ICE / SNOW_BLOCK /
/// PACKED_ICE / BLUE_ICE; `ChunkGenerator.f()` is `getSeaLevel()`; `RandomSource.h()`
/// is `nextBoolean()`, `.i()` is `nextFloat()`, `.j()` is `nextDouble()`;
/// `Mth.a(FFF)` is `clamp`; `BlockPos.f()/.g()/.h()/.i()` are north/south/west/east.
///
/// Every branch that draws is kept as the reference has it (a `||` still
/// short-circuits, a `nextInt` inside an `if` is still inside it), because the
/// iceberg is drawn from a single stream across all of its helpers.
#[allow(clippy::too_many_arguments)]
fn gen_iceberg(ctx: &mut FC, rng: &mut Xor, origin: (i32, i32, i32), state: u32) {
    let pos = (origin.0, crate::worldgen::sea_level(), origin.2);
    let fire = rng.next_f64() > 0.7;
    let angle = rng.next_f64() * 2.0 * std::f64::consts::PI;
    let i = 11 - rng.next_bound(5);
    let j = 3 + rng.next_bound(3);
    let tall = rng.next_f64() > 0.7;
    let mut l = if tall {
        rng.next_bound(6) + 6
    } else {
        rng.next_bound(15) + 3
    };
    if !tall && rng.next_f64() > 0.9 {
        l += rng.next_bound(19) + 7;
    }
    let i1 = (l + rng.next_bound(11)).min(18);
    let j1 = (l + rng.next_bound(7) - rng.next_bound(5)).min(11);
    let k1 = if tall { i } else { 11 };

    for dx in -k1..k1 {
        for dz in -k1..k1 {
            for y in 0..l {
                let r = if tall {
                    iceberg_radius_round(y, l, j1)
                } else {
                    iceberg_radius_ellipse(rng, y, l, j1)
                };
                if tall || dx < r {
                    iceberg_block(
                        ctx, rng, pos, l, dx, y, dz, r, k1, tall, j, angle, fire, state,
                    );
                }
            }
        }
    }
    iceberg_smooth(ctx, pos, j1, l, tall, i);
    for dx in -k1..k1 {
        for dz in -k1..k1 {
            let mut y = -1;
            while y > -i1 {
                // `Mth.floor(k1 * (1 - y^2 / (i1 * 8)))`: the tall shape's narrowing
                // underside
                let r = if tall {
                    (k1 as f64 * (1.0 - (y as f64).powi(2) / (i1 as f64 * 8.0))).floor() as i32
                } else {
                    k1
                };
                let r2 = iceberg_radius_steep(rng, -y, i1, j1);
                if dx < r2 {
                    iceberg_block(
                        ctx, rng, pos, i1, dx, y, dz, r2, r, tall, j, angle, fire, state,
                    );
                }
                y -= 1;
            }
        }
    }
    let top = if tall {
        rng.next_f64() > 0.1
    } else {
        rng.next_f64() > 0.7
    };
    if top {
        iceberg_place_top(ctx, rng, j1, l, pos, tall, i, angle, j);
    }
}

/// `IcebergFeature.heightDependentRadiusEllipse`: one `nextFloat`, plus a
/// `nextInt(5)`/`nextInt(6)` pair only on the tall branch.
fn iceberg_radius_ellipse(rng: &mut Xor, y: i32, max_height: i32, scale: i32) -> i32 {
    let f = 3.5 - rng.next_f32();
    let mut f1 = (1.0 - (y as f64).powi(2) / (max_height as f64 * f as f64)) as f32 * scale as f32;
    if max_height > 15 + rng.next_bound(5) {
        let v = if y < 3 + rng.next_bound(6) { y / 2 } else { y };
        f1 = (1.0 - v as f32 / (max_height as f32 * f * 0.4)) * scale as f32;
    }
    (f1 / 2.0).floor() as i32
}

/// `heightDependentRadiusRound`: no draws.
fn iceberg_radius_round(y: i32, max_height: i32, scale: i32) -> i32 {
    let f1 = (1.0 - (y as f64).powi(2) / max_height as f64) as f32 * scale as f32;
    (f1 / 2.0).floor() as i32
}

/// `heightDependentRadiusSteep`: one `nextFloat`.
fn iceberg_radius_steep(rng: &mut Xor, y: i32, max_height: i32, scale: i32) -> i32 {
    let f = 1.0 + rng.next_f32() / 2.0;
    let f1 = (1.0 - y as f32 / (max_height as f32 * f)) * scale as f32;
    (f1 / 2.0).floor() as i32
}

/// `isIcebergState`: the feature's own three materials.
fn is_iceberg_state(reg: &BReg, st: u32) -> bool {
    matches!(
        reg.name_of(st),
        "minecraft:packed_ice" | "minecraft:snow_block" | "minecraft:blue_ice"
    )
}

/// `signedDistanceEllipse`: the rotated ellipse the tall shape carves with.
fn iceberg_ellipse_dist(
    dx: i32,
    dz: i32,
    offset: (i32, i32, i32),
    r1: i32,
    r2: i32,
    angle: f64,
) -> f64 {
    let x = (dx - offset.0) as f64;
    let z = (dz - offset.2) as f64;
    (((x * angle.cos() - z * angle.sin()) / r1 as f64).powi(2)
        + ((x * angle.sin() + z * angle.cos()) / r2 as f64).powi(2))
        - 1.0
}

/// `signedDistanceCircle`: one `nextFloat` per call.
fn iceberg_circle_dist(dx: i32, dz: i32, offset: (i32, i32, i32), r: i32, rng: &mut Xor) -> f64 {
    let f = 10.0 * rng.next_f32().clamp(0.2, 0.8) as f64 / r as f64;
    f + (dx - offset.0).pow(2) as f64 + (dz - offset.2).pow(2) as f64 - (r as f64).powi(2)
}

/// `heightDependentRadiusOffset`: no draws.
fn iceberg_offset(y: i32, max_height: i32, j: i32) -> i32 {
    if y > 0 && max_height - y <= 3 {
        j - (4 - (max_height - y))
    } else {
        j
    }
}

/// `placeIcebergBlock`: the write gate, with its `nextDouble` and `nextInt` draws.
#[allow(clippy::too_many_arguments)]
fn iceberg_place_block(
    ctx: &mut FC,
    rng: &mut Xor,
    pos: (i32, i32, i32),
    level: i32,
    max_height: i32,
    tall: bool,
    fire: bool,
    state: u32,
) {
    let Some(reg) = breg() else { return };
    let cur = ctx.get(pos.0, pos.1, pos.2);
    let n = reg.name_of(cur);
    if !(reg.is_air(cur)
        || n == "minecraft:snow_block"
        || n == "minecraft:ice"
        || n == "minecraft:water")
    {
        return;
    }
    let roll = if !tall { true } else { rng.next_f64() > 0.05 };
    let d = if tall { 3 } else { 2 };
    let snow = fire
        && n != "minecraft:water"
        && (level as f64)
            <= rng.next_bound((max_height / d).max(1)) as f64 + max_height as f64 * 0.6
        && roll;
    let st = if snow {
        reg.state_of("minecraft:snow_block", None)
    } else {
        state
    };
    ctx.set(pos.0, pos.1, pos.2, st);
}

/// `generateIcebergBlock` (the ellipse/circle test plus the depth guard).
#[allow(clippy::too_many_arguments)]
fn iceberg_block(
    ctx: &mut FC,
    rng: &mut Xor,
    pos: (i32, i32, i32),
    max_height: i32,
    dx: i32,
    y: i32,
    dz: i32,
    r: i32,
    radius_scale: i32,
    tall: bool,
    j: i32,
    angle: f64,
    fire: bool,
    state: u32,
) {
    let d0 = if tall {
        iceberg_ellipse_dist(
            dx,
            dz,
            pos,
            radius_scale,
            iceberg_offset(y, max_height, j),
            angle,
        )
    } else {
        iceberg_circle_dist(dx, dz, pos, r, rng)
    };
    if d0 >= 0.0 {
        return;
    }
    let p = (pos.0 + dx, pos.1 + y, pos.2 + dz);
    let d1 = if tall {
        -0.5
    } else {
        -6.0 - rng.next_bound(3) as f64
    };
    if d0 > d1 && rng.next_f64() > 0.9 {
        return;
    }
    iceberg_place_block(ctx, rng, p, max_height - y, max_height, tall, fire, state);
}

/// `IcebergFeature.smooth`: removes single ice cells whose neighbour count is too
/// low, and cells whose block below is air.
fn iceberg_smooth(
    ctx: &mut FC,
    pos: (i32, i32, i32),
    radius: i32,
    max_height: i32,
    tall: bool,
    i: i32,
) {
    let Some(reg) = breg() else { return };
    let r = if tall { i } else { radius / 2 };
    let air = reg.state_of("minecraft:air", None);
    for dx in -r..=r {
        for dz in -r..=r {
            for y in 0..=max_height {
                let p = (pos.0 + dx, pos.1 + y, pos.2 + dz);
                let cur = ctx.get(p.0, p.1, p.2);
                let n = reg.name_of(cur);
                if !(is_iceberg_state(reg, cur) || n == "minecraft:snow") {
                    continue;
                }
                if ctx.is_air(p.0, p.1 - 1, p.2) {
                    ctx.set(p.0, p.1, p.2, air);
                    ctx.set(p.0, p.1 - 1, p.2, air);
                } else if is_iceberg_state(reg, cur) {
                    let neighbours = [
                        ctx.get(p.0 - 1, p.1, p.2),
                        ctx.get(p.0 + 1, p.1, p.2),
                        ctx.get(p.0, p.1, p.2 - 1),
                        ctx.get(p.0, p.1, p.2 + 1),
                    ];
                    let empty = neighbours
                        .iter()
                        .filter(|&&s| !is_iceberg_state(reg, s))
                        .count();
                    if empty >= 3 {
                        ctx.set(p.0, p.1, p.2, air);
                    }
                }
            }
        }
    }
}

/// `placeTop` and its per-ring helper: the pointed cap above sea level.
#[allow(clippy::too_many_arguments)]
fn iceberg_place_top(
    ctx: &mut FC,
    rng: &mut Xor,
    radius: i32,
    max_height: i32,
    pos: (i32, i32, i32),
    tall: bool,
    i: i32,
    angle: f64,
    j: i32,
) {
    let sx = if rng.next_bool() { -1 } else { 1 };
    let sz = if rng.next_bool() { -1 } else { 1 };
    let mut r1 = rng.next_bound((radius / 2 - 2).max(1));
    if rng.next_bool() {
        r1 = radius / 2 + 1 - rng.next_bound((radius - radius / 2 - 1).max(1));
    }
    let mut r2 = rng.next_bound((radius / 2 - 2).max(1));
    if rng.next_bool() {
        r2 = radius / 2 + 1 - rng.next_bound((radius - radius / 2 - 1).max(1));
    }
    if tall {
        r1 = rng.next_bound((i - 5).max(1));
        r2 = r1;
    }
    let offset = (sx * r1, 0, sz * r2);
    let a = if tall {
        angle + std::f64::consts::FRAC_PI_2
    } else {
        rng.next_f64() * 2.0 * std::f64::consts::PI
    };
    for y in 0..max_height - 3 {
        let r = iceberg_radius_ellipse(rng, y, max_height, radius);
        iceberg_top_ring(ctx, r, y, pos, false, a, offset, i, j);
    }
    let mut y = -1;
    while y > -max_height + rng.next_bound(5) {
        let r = iceberg_radius_steep(rng, -y, max_height, radius);
        iceberg_top_ring(ctx, r, y, pos, true, a, offset, i, j);
        y -= 1;
    }
}

/// The inner ring helper of `placeTop`: it may leave water (the tall shape) or air.
#[allow(clippy::too_many_arguments)]
fn iceberg_top_ring(
    ctx: &mut FC,
    radius: i32,
    y: i32,
    pos: (i32, i32, i32),
    tall: bool,
    angle: f64,
    offset: (i32, i32, i32),
    i: i32,
    j: i32,
) {
    let Some(reg) = breg() else { return };
    let r1 = radius + 1 + i / 3;
    let r2 = (radius - 3).min(3) + j / 2 - 1;
    let air = reg.state_of("minecraft:air", None);
    let water = reg.state_of("minecraft:water", None);
    let snow = reg.state_of("minecraft:snow", None);
    for dx in -r1..r1 {
        for dz in -r1..r1 {
            let d = iceberg_ellipse_dist(dx, dz, offset, r1, r2, angle);
            if d >= 0.0 {
                continue;
            }
            let p = (pos.0 + dx, pos.1 + y, pos.2 + dz);
            let cur = ctx.get(p.0, p.1, p.2);
            if !(is_iceberg_state(reg, cur) || reg.name_of(cur) == "minecraft:snow_block") {
                continue;
            }
            if tall {
                ctx.set(p.0, p.1, p.2, water);
            } else {
                ctx.set(p.0, p.1, p.2, air);
                if ctx.get(p.0, p.1 - 1, p.2) == snow {
                    ctx.set(p.0, p.1 - 1, p.2, air);
                }
            }
        }
    }
}

/// `minecraft:desert_well` (`ebw`). Letters resolved in the mappings first:
/// `Blocks.I/J/aV/jF/G` are SAND / SUSPICIOUS_SAND / SANDSTONE / SANDSTONE_SLAB /
/// WATER, `BlockPos.d()`/`e()` are `above()`/`below()`, and `.f()/.g()/.h()/.i()` are
/// north/south/west/east - that order matters, because the two suspicious sand blocks
/// come from `nextInt(5)` over `[origin, east, south, west, north]`.
fn gen_desert_well(ctx: &mut FC, rng: &mut Xor, pos: (i32, i32, i32)) {
    let Some(reg) = breg() else { return };
    let sand = reg.state_of("minecraft:sand", None);
    let slab = reg.state_of("minecraft:sandstone_slab", None);
    let sandstone = reg.state_of("minecraft:sandstone", None);
    let water = reg.state_of("minecraft:water", None);
    let suspicious = reg.state_of("minecraft:suspicious_sand", None);
    let min_y = crate::worldgen::min_y();

    let mut p = (pos.0, pos.1 + 1, pos.2);
    while p.1 > min_y + 2 && ctx.is_air(p.0, p.1, p.2) {
        p.1 -= 1;
    }
    if ctx.get(p.0, p.1, p.2) != sand {
        return;
    }
    // the footprint needs ground: two empty cells under one column reject the well
    for dx in -2..=2 {
        for dz in -2..=2 {
            if ctx.is_air(p.0 + dx, p.1 - 1, p.2 + dz) && ctx.is_air(p.0 + dx, p.1 - 2, p.2 + dz) {
                return;
            }
        }
    }

    for dy in -2..=0 {
        for dx in -2..=2 {
            for dz in -2..=2 {
                ctx.set(p.0 + dx, p.1 + dy, p.2 + dz, slab);
            }
        }
    }
    ctx.set(p.0, p.1, p.2, water);
    const HORIZ: [(i32, i32); 4] = [(1, 0), (0, 1), (-1, 0), (0, -1)];
    for (dx, dz) in HORIZ {
        ctx.set(p.0 + dx, p.1, p.2 + dz, water);
    }
    let below = (p.0, p.1 - 1, p.2);
    ctx.set(below.0, below.1, below.2, sand);
    for (dx, dz) in HORIZ {
        ctx.set(below.0 + dx, below.1, below.2 + dz, sand);
    }

    for dx in -2i32..=2 {
        for dz in -2i32..=2 {
            if dx.abs() == 2 || dz.abs() == 2 {
                ctx.set(p.0 + dx, p.1 + 1, p.2 + dz, slab);
            }
        }
    }
    for (dx, dz) in [(2, 0), (-2, 0), (0, 2), (0, -2)] {
        ctx.set(p.0 + dx, p.1 + 1, p.2 + dz, slab);
    }
    for dx in -1..=1 {
        for dz in -1..=1 {
            let st = if dx == 0 && dz == 0 { sandstone } else { slab };
            ctx.set(p.0 + dx, p.1 + 4, p.2 + dz, st);
        }
    }
    for y in 1..=3 {
        for (dx, dz) in [(-1, -1), (-1, 1), (1, -1), (1, 1)] {
            ctx.set(p.0 + dx, p.1 + y, p.2 + dz, sandstone);
        }
    }

    let shaft = [
        p,
        (p.0 + 1, p.1, p.2),
        (p.0, p.1, p.2 + 1),
        (p.0 - 1, p.1, p.2),
        (p.0, p.1, p.2 - 1),
    ];
    let i1 = rng.next_bound(shaft.len() as i32) as usize;
    ctx.set(shaft[i1].0, shaft[i1].1 + 1, shaft[i1].2, suspicious);
    let i2 = rng.next_bound(shaft.len() as i32) as usize;
    ctx.set(shaft[i2].0, shaft[i2].1 + 2, shaft[i2].2, suspicious);
}

/// `minecraft:root_system` (`edg`). Obfuscated letters resolved in the official
/// mappings before writing this, because three of them are not guessable:
/// `BlockPos.d()`/`e()` are `above()`/`below()`, `BlockState.i()` is `isAir()`,
/// `BlockState.e()` is `isSolid()`, `LevelReader.u(pos)` is `isEmptyBlock`, and
/// `FluidTags.a`/`b` are WATER/LAVA.
struct RootSystemCfg {
    feature: Pf,
    required_vspace: i32,
    root_radius: i32,
    root_replaceable: String,
    root_state: Bsp,
    root_attempts: i32,
    root_column_max_height: i32,
    hanging_radius: i32,
    hanging_span: i32,
    hanging_state: Bsp,
    hanging_attempts: i32,
    water_for_tree: i32,
    allowed_tree_position: Pr,
}

/// `getFluidState(pos).is(FluidTags.WATER)`: the port knows liquid *blocks*, not
/// fluid states, so a waterlogged solid would not count here. In the azalea and
/// sulfur pool configs the vertical space is open water, so the two agree.
fn is_water_fluid(reg: &BReg, st: u32) -> bool {
    reg.is_liquid(st) && reg.name_of(st).starts_with("minecraft:water")
}

fn is_lava_fluid(reg: &BReg, st: u32) -> bool {
    reg.is_liquid(st) && reg.name_of(st).starts_with("minecraft:lava")
}

/// `RootSystemFeature.isValidVerticalState`: air, or water within
/// `allowed_vertical_water_for_tree` blocks of the tree's base.
fn root_valid_vertical(reg: &BReg, st: u32, i: i32, max_water: i32) -> bool {
    if reg.is_air(st) {
        return true;
    }
    i < max_water && is_water_fluid(reg, st)
}

fn root_has_space_for_tree(
    ctx: &mut FC,
    c: &RootSystemCfg,
    reg: &BReg,
    from: (i32, i32, i32),
) -> bool {
    let mut p = from;
    for i in 1..=c.required_vspace {
        p.1 += 1;
        let st = ctx.get(p.0, p.1, p.2);
        if !root_valid_vertical(reg, st, i, c.water_for_tree) {
            return false;
        }
    }
    true
}

/// One layer of the root column: `root_placement_attempts` tries, each drawing
/// dx and dz in +-`root_radius` *from the layer's centre* (`MutableBlockPos.move`
/// resets to the position it is passed), replacing anything in the
/// `root_replaceable` tag with the provider's state.
fn root_place_layer(
    ctx: &mut FC,
    rng: &mut Xor,
    c: &RootSystemCfg,
    reg: &BReg,
    x: i32,
    y: i32,
    z: i32,
) {
    for _ in 0..c.root_attempts {
        let q = (
            x + rng.next_bound(c.root_radius) - rng.next_bound(c.root_radius),
            y,
            z + rng.next_bound(c.root_radius) - rng.next_bound(c.root_radius),
        );
        if reg.in_tag(ctx.get(q.0, q.1, q.2), &c.root_replaceable) {
            let st = c.root_state.get(ctx, rng, q.0, q.1, q.2);
            ctx.set(q.0, q.1, q.2, st);
        }
    }
}

fn root_place_column(
    ctx: &mut FC,
    rng: &mut Xor,
    c: &RootSystemCfg,
    reg: &BReg,
    from: (i32, i32, i32),
    to_y: i32,
) {
    for y in from.1..to_y {
        root_place_layer(ctx, rng, c, reg, from.0, y, from.2);
    }
}

/// The hanging-root pass around the origin: each try draws a 3-axis offset
/// (dx, dy, dz, each `nextInt(span) - nextInt(span)`), and places only into an
/// empty cell whose block *above* is sturdy on its bottom face - which is also
/// what `HangingRootsBlock.canSurvive` checks, so one test covers both.
fn root_place_hanging(
    ctx: &mut FC,
    rng: &mut Xor,
    c: &RootSystemCfg,
    reg: &BReg,
    origin: (i32, i32, i32),
) {
    for _ in 0..c.hanging_attempts {
        let q = (
            origin.0 + rng.next_bound(c.hanging_radius) - rng.next_bound(c.hanging_radius),
            origin.1 + rng.next_bound(c.hanging_span) - rng.next_bound(c.hanging_span),
            origin.2 + rng.next_bound(c.hanging_radius) - rng.next_bound(c.hanging_radius),
        );
        if !ctx.is_air(q.0, q.1, q.2) {
            continue;
        }
        let st = c.hanging_state.get(ctx, rng, q.0, q.1, q.2);
        let above = (q.0, q.1 + 1, q.2);
        if reg.side_solid(ctx.get(above.0, above.1, above.2), 0) {
            ctx.set(q.0, q.1, q.2, st);
        }
    }
}

fn gen_root_system(ctx: &mut FC, rng: &mut Xor, pos: (i32, i32, i32), c: &RootSystemCfg) {
    let Some(reg) = breg() else { return };
    if !ctx.is_air(pos.0, pos.1, pos.2) {
        return;
    }
    let mut cursor = pos;
    let mut placed_tree = false;
    // `placeHangingRoots` runs only when the tree was placed (vanilla's
    // `if (placeRootsAndTree(...))`), so the draws after it are conditional too
    for i in 0..c.root_column_max_height {
        cursor.1 += 1;
        if !test_pred(
            &c.allowed_tree_position,
            reg,
            ctx,
            cursor.0,
            cursor.1,
            cursor.2,
        ) {
            continue;
        }
        if !root_has_space_for_tree(ctx, c, reg, cursor) {
            continue;
        }
        let below = (cursor.0, cursor.1 - 1, cursor.2);
        let bst = ctx.get(below.0, below.1, below.2);
        // `getFluidState(below).is(LAVA) || !getBlockState(below).isSolid()`
        if is_lava_fluid(reg, bst) || !reg.is_solid(bst) {
            return;
        }
        // The port's features do not report success, and `ctx.wr` is a *map*: writing
        // over a cell that is already recorded does not change its length. Compose the
        // whole map instead of counting it, so a tree that only overwrites still counts.
        let before = ctx.wr.clone();
        gen_pf(&c.feature, freg(), ctx, rng, cursor);
        if ctx.wr != before {
            root_place_column(ctx, rng, c, reg, pos, pos.1 + i);
            placed_tree = true;
            break;
        }
    }
    if placed_tree {
        root_place_hanging(ctx, rng, c, reg, pos);
    }
}

/// `GeodeFeature.placeBlock`: the write is skipped when the existing block is in
/// `cannot_replace`, and the caller has already asked the provider for its state.
fn geode_place(ctx: &mut FC, p: (i32, i32, i32), st: u32, cannot_replace: &str) {
    let Some(reg) = breg() else { return };
    if ctx.out_of_h(p.1) {
        return;
    }
    if !reg.in_tag(ctx.get(p.0, p.1, p.2), cannot_replace) {
        ctx.set(p.0, p.1, p.2, st);
    }
}

/// `Feature.isAirOrLeaves`: air, or any block in the vanilla leaves tag.
fn is_air_or_leaves(reg: &BReg, st: u32) -> bool {
    reg.is_air(st) || reg.in_tag(st, "minecraft:leaves")
}

/// `IceSpikeFeature` (`ecq`). The `&&`/`||` short circuits matter: the corner
/// `nextFloat()` is drawn only for edge cells, so the stream depends on the shape
/// the loop has reached.
fn gen_ice_spike(ctx: &mut FC, rng: &mut Xor, pos: (i32, i32, i32)) {
    let Some(reg) = breg() else { return };
    // `Blocks.dP`/`dO`/`iC` in the bytecode: snow_block, ice, packed_ice
    let snow = reg.state_of("minecraft:snow_block", None);
    let ice_block = reg.state_of("minecraft:ice", None);
    let packed = reg.state_of("minecraft:packed_ice", None);
    let min_y = crate::worldgen::min_y();

    let mut p = pos;
    while ctx.is_air(p.0, p.1, p.2) && p.1 > min_y + 2 {
        p.1 -= 1;
    }
    if ctx.get(p.0, p.1, p.2) != snow {
        return;
    }
    p.1 += rng.next_bound(4);
    let height = rng.next_bound(4) + 7;
    let radius_max = height / 4 + rng.next_bound(2);
    if radius_max > 1 && rng.next_bound(60) == 0 {
        p.1 += 10 + rng.next_bound(30);
    }

    for h in 0..height {
        let f = (1.0 - h as f32 / height as f32) * radius_max as f32;
        let r = f.floor() as i32;
        for dx in -r..=r {
            let fx = dx.abs() as f32 - 0.25;
            for dz in -r..=r {
                let fz = dz.abs() as f32 - 0.25;
                // Java negates float comparisons (`!(a > b)`), which is true for NaN
                // where Rust's `<=` is false, so the negation is kept and only named
                // out of clippy's way - the shape of the comparison is the reference's.
                let outside = fx * fx + fz * fz > f * f;
                let inside = (dx == 0 && dz == 0) || !outside;
                let edge = dx != -r && dx != r && dz != -r && dz != r;
                // the draw happens only when the cell is on the rim (Java `||`)
                let passes = if edge {
                    true
                } else {
                    let roll = rng.next_f32() > 0.75;
                    !roll
                };
                if !(inside && passes) {
                    continue;
                }
                for (yy, sign) in [(h, 1), (h, -1)] {
                    if sign < 0 && (h == 0 || r <= 1) {
                        continue;
                    }
                    let q = (p.0 + dx, p.1 + yy * sign, p.2 + dz);
                    let at = ctx.get(q.0, q.1, q.2);
                    if is_air_or_leaves(reg, at) || at == snow || at == ice_block {
                        ctx.set(q.0, q.1, q.2, packed);
                    }
                }
            }
        }
    }

    let bottom_r = (radius_max - 1).clamp(0, 1);
    for dx in -bottom_r..=bottom_r {
        for dz in -bottom_r..=bottom_r {
            let mut q = (p.0 + dx, p.1 - 1, p.2 + dz);
            let mut budget = 50;
            if dx.abs() == 1 && dz.abs() == 1 {
                budget = rng.next_bound(5);
            }
            while q.1 > 50 {
                let at = ctx.get(q.0, q.1, q.2);
                if !(is_air_or_leaves(reg, at) || at == snow || at == ice_block || at == packed) {
                    break;
                }
                ctx.set(q.0, q.1, q.2, packed);
                q.1 -= 1;
                budget -= 1;
                if budget <= 0 {
                    // `BlockPos.c(int)` is `below(int)`: the drill skips *down*, and
                    // that is also what ends the loop (a jump up never reaches 50).
                    q.1 -= rng.next_bound(5) + 1;
                    budget = rng.next_bound(5);
                }
            }
        }
    }
}

fn gen_geode(ctx: &mut FC, rng: &mut Xor, origin: (i32, i32, i32), g: &GeodeCfg) {
    let Some(reg) = breg() else { return };
    let points_n = g.distribution_points.get(rng);
    let noise = crate::generation_math::normal_new_l(ctx.g.wseed(), -4, &[1.0]);
    let d0 = points_n as f64 / g.outer_wall_distance.max_inclusive().max(1) as f64;
    let d1 = 1.0 / g.layers[0].sqrt();
    let d2 = 1.0 / (g.layers[1] + d0).sqrt();
    let d3 = 1.0 / (g.layers[2] + d0).sqrt();
    let d4 = 1.0 / (g.layers[3] + d0).sqrt();
    let d5 =
        1.0 / (g.crack_size + rng.next_f64() / 2.0 + if points_n > 3 { d0 } else { 0.0 }).sqrt();
    let has_crack = (rng.next_f32() as f64) < g.crack_chance;

    let mut points: Vec<((i32, i32, i32), i32)> = Vec::with_capacity(points_n.max(0) as usize);
    let mut invalid = 0;
    for _ in 0..points_n.max(0) {
        let p = (
            origin.0 + g.outer_wall_distance.get(rng),
            origin.1 + g.outer_wall_distance.get(rng),
            origin.2 + g.outer_wall_distance.get(rng),
        );
        let st = ctx.get(p.0, p.1, p.2);
        if reg.is_air(st) || reg.in_tag(st, &g.invalid) {
            invalid += 1;
            if invalid > g.invalid_blocks_threshold {
                return;
            }
        }
        points.push((p, g.point_offset.get(rng)));
    }

    let mut crack_points: Vec<(i32, i32, i32)> = Vec::new();
    if has_crack {
        let ext = points_n * 2 + 1;
        let offsets = match rng.next_bound(4) {
            0 => [(ext, 7, 0), (ext, 5, 0), (ext, 1, 0)],
            1 => [(0, 7, ext), (0, 5, ext), (0, 1, ext)],
            2 => [(ext, 7, ext), (ext, 5, ext), (ext, 1, ext)],
            _ => [(0, 7, 0), (0, 5, 0), (0, 1, 0)],
        };
        for (a, b, c) in offsets {
            crack_points.push((origin.0 + a, origin.1 + b, origin.2 + c));
        }
    }

    let dist_sqr = |p: (i32, i32, i32), q: (i32, i32, i32)| {
        let (dx, dy, dz) = ((p.0 - q.0) as f64, (p.1 - q.1) as f64, (p.2 - q.2) as f64);
        dx * dx + dy * dy + dz * dz
    };
    let mut inner_placements: Vec<(i32, i32, i32)> = Vec::new();
    for x in (origin.0 + g.min_gen_offset)..=(origin.0 + g.max_gen_offset) {
        for y in (origin.1 + g.min_gen_offset)..=(origin.1 + g.max_gen_offset) {
            for z in (origin.2 + g.min_gen_offset)..=(origin.2 + g.max_gen_offset) {
                let at = (x, y, z);
                let noise_v = noise.get(x as f64, y as f64, z as f64) * g.noise_multiplier;
                let mut d6 = 0.0;
                for (p, off) in points.iter() {
                    d6 += 1.0 / (dist_sqr(at, *p) + *off as f64).sqrt() + noise_v;
                }
                let mut d7 = 0.0;
                for cp in crack_points.iter() {
                    d7 += 1.0 / (dist_sqr(at, *cp) + g.crack_offset as f64).sqrt() + noise_v;
                }
                if d6 < d4 {
                    continue;
                }
                if has_crack && d7 >= d5 && d6 < d1 {
                    let st = g.filling.get(ctx, rng, x, y, z);
                    geode_place(ctx, at, st, &g.cannot_replace);
                    for (dx, dy, dz) in [
                        (0, -1, 0),
                        (0, 1, 0),
                        (0, 0, -1),
                        (0, 0, 1),
                        (-1, 0, 0),
                        (1, 0, 0),
                    ] {
                        let np = (x + dx, y + dy, z + dz);
                        if !ctx.out_of_h(np.1) {
                            let ns = ctx.get(np.0, np.1, np.2);
                            if !reg.is_air(ns) {
                                // vanilla writes the neighbour's fluid legacy block: a
                                // fluid keeps its own state, anything else becomes air
                                let air = reg.state_of("minecraft:air", None);
                                ctx.set(np.0, np.1, np.2, if reg.is_liquid(ns) { ns } else { air });
                            }
                        }
                    }
                } else if d6 >= d1 {
                    let st = g.filling.get(ctx, rng, x, y, z);
                    geode_place(ctx, at, st, &g.cannot_replace);
                } else if d6 >= d2 {
                    let alt = (rng.next_f32() as f64) < g.use_alt_layer0;
                    let st = if alt {
                        g.alt_inner.get(ctx, rng, x, y, z)
                    } else {
                        g.inner.get(ctx, rng, x, y, z)
                    };
                    geode_place(ctx, at, st, &g.cannot_replace);
                    if (!g.placements_require_alt || alt)
                        && (rng.next_f32() as f64) < g.use_potential_placements
                    {
                        inner_placements.push(at);
                    }
                } else if d6 >= d3 {
                    let st = g.middle.get(ctx, rng, x, y, z);
                    geode_place(ctx, at, st, &g.cannot_replace);
                } else {
                    let st = g.outer.get(ctx, rng, x, y, z);
                    geode_place(ctx, at, st, &g.cannot_replace);
                }
            }
        }
    }

    if g.placements.is_empty() {
        return;
    }
    const DIRS: [(i32, i32, i32, &str); 6] = [
        (0, -1, 0, "down"),
        (0, 1, 0, "up"),
        (0, 0, -1, "north"),
        (0, 0, 1, "south"),
        (-1, 0, 0, "west"),
        (1, 0, 0, "east"),
    ];
    for p in inner_placements {
        let mut st = g.placements[rng.next_bound(g.placements.len() as i32) as usize];
        for (dx, dy, dz, dir) in DIRS {
            if reg.has_prop(reg.name_of(st), "facing") {
                st = reg.with_prop(st, "facing", dir);
            }
            let np = (p.0 + dx, p.1 + dy, p.2 + dz);
            if ctx.out_of_h(np.1) {
                continue;
            }
            let ns = ctx.get(np.0, np.1, np.2);
            if reg.has_prop(reg.name_of(st), "waterlogged") {
                let wl = if reg.is_liquid(ns) { "true" } else { "false" };
                st = reg.with_prop(st, "waterlogged", wl);
            }
            // `BuddingAmethystBlock.canClusterGrowAtState`: air, or a water *source*
            let water_source =
                reg.is_liquid(ns) && reg.get_prop(ns, "level").map(|l| l == "0").unwrap_or(false);
            if reg.is_air(ns) || water_source {
                ctx.set(np.0, np.1, np.2, st);
                break;
            }
        }
    }
}

fn p_cf(j: &J) -> Option<Cf> {
    let ty = jstr(j, "type")?;
    // 1.21.1 wraps a configured feature's parameters in a "config" object;
    // 1.21.2+ (and this repository's older copy) inlines them beside "type".
    let j = js(j, "config").unwrap_or(j);
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
        "geode" => {
            let b = js(j, "blocks")?;
            let lay = js(j, "layers")?;
            let crack = js(j, "crack")?;
            let tag = |k: &str| -> Option<String> {
                Some(
                    ns_name(js(b, k)?.str().trim_start_matches('#'))
                        .trim_start_matches('#')
                        .to_string(),
                )
            };
            let mut placements = vec![];
            for p in jarr(b, "inner_placements")
                .map(|v| v.as_slice())
                .unwrap_or(&[])
            {
                let (n, pr) = bs_parts(p)?;
                placements.push(breg()?.state_of(&n, pr));
            }
            Cf::Geode(Box::new(GeodeCfg {
                filling: p_bsp(js(b, "filling_provider")?)?,
                inner: p_bsp(js(b, "inner_layer_provider")?)?,
                alt_inner: p_bsp(js(b, "alternate_inner_layer_provider")?)?,
                middle: p_bsp(js(b, "middle_layer_provider")?)?,
                outer: p_bsp(js(b, "outer_layer_provider")?)?,
                placements,
                cannot_replace: tag("cannot_replace")?,
                invalid: tag("invalid_blocks")?,
                layers: [
                    jnum(lay, "filling")?,
                    jnum(lay, "inner_layer")?,
                    jnum(lay, "middle_layer")?,
                    jnum(lay, "outer_layer")?,
                ],
                crack_chance: jnum(crack, "generate_crack_chance")?,
                crack_size: jnum(crack, "base_crack_size")?,
                crack_offset: jint(crack, "crack_point_offset", 0),
                use_alt_layer0: jnum(j, "use_alternate_layer0_chance")?,
                use_potential_placements: jnum(j, "use_potential_placements_chance")?,
                placements_require_alt: jbool(j, "placements_require_layer0_alternate", true),
                outer_wall_distance: p_ip(js(j, "outer_wall_distance")?)?,
                distribution_points: p_ip(js(j, "distribution_points")?)?,
                point_offset: p_ip(js(j, "point_offset")?)?,
                min_gen_offset: jint(j, "min_gen_offset", 0),
                max_gen_offset: jint(j, "max_gen_offset", 0),
                noise_multiplier: jnum(j, "noise_multiplier")?,
                invalid_blocks_threshold: jint(j, "invalid_blocks_threshold", 0),
            }))
        }
        "root_system" => Cf::RootSystem(Box::new(RootSystemCfg {
            feature: pf_any(js(j, "feature")?)?,
            required_vspace: jint(j, "required_vertical_space_for_tree", 0),
            root_radius: jint(j, "root_radius", 0),
            root_replaceable: ns_name(js(j, "root_replaceable")?.str().trim_start_matches('#')),
            root_state: p_bsp(js(j, "root_state_provider")?)?,
            root_attempts: jint(j, "root_placement_attempts", 0),
            root_column_max_height: jint(j, "root_column_max_height", 0),
            hanging_radius: jint(j, "hanging_root_radius", 0),
            hanging_span: jint(j, "hanging_roots_vertical_span", 0),
            hanging_state: p_bsp(js(j, "hanging_root_state_provider")?)?,
            hanging_attempts: jint(j, "hanging_root_placement_attempts", 0),
            water_for_tree: jint(j, "allowed_vertical_water_for_tree", 0),
            allowed_tree_position: p_pred(js(j, "allowed_tree_position")?)?,
        })),
        "large_dripstone" => Cf::LargeDripstone(Box::new(LargeDripstoneCfg {
            range: jint(j, "floor_to_ceiling_search_range", 30),
            radius: p_ip(js(j, "column_radius")?)?,
            height_scale: p_fp(js(j, "height_scale")?)?,
            ratio: jf32(j, "max_column_radius_to_cave_height_ratio", 1.0),
            stalactite_bluntness: p_fp(js(j, "stalactite_bluntness")?)?,
            stalagmite_bluntness: p_fp(js(j, "stalagmite_bluntness")?)?,
            wind_speed: p_fp(js(j, "wind_speed")?)?,
            min_radius_for_wind: jint(j, "min_radius_for_wind", 0),
            min_bluntness_for_wind: jf32(j, "min_bluntness_for_wind", 0.0),
        })),
        "dripstone_cluster" => Cf::DripstoneCluster(Box::new(DripstoneClusterCfg {
            range: jint(j, "floor_to_ceiling_search_range", 12),
            height: p_ip(js(j, "height")?)?,
            radius: p_ip(js(j, "radius")?)?,
            max_diff: jint(j, "max_stalagmite_stalactite_height_diff", 1),
            height_deviation: jint(j, "height_deviation", 3),
            thickness: p_ip(js(j, "dripstone_block_layer_thickness")?)?,
            density: p_fp(js(j, "density")?)?,
            wetness: p_fp(js(j, "wetness")?)?,
            chance: jf32(
                j,
                "chance_of_dripstone_column_at_max_distance_from_center",
                0.1,
            ),
            max_dist_edge: jint(
                j,
                "max_distance_from_edge_affecting_chance_of_dripstone_column",
                3,
            ),
            max_dist_center: jint(j, "max_distance_from_center_affecting_height_bias", 8),
        })),
        "iceberg" => Cf::Iceberg {
            state: {
                let (n, p) = bs_parts(js(j, "state")?)?;
                breg()?.state_of(&n, p)
            },
        },
        "desert_well" => Cf::DesertWell,
        "ice_spike" => Cf::IceSpike,
        "sea_pickle" => Cf::SeaPickle {
            count: p_ip(js(j, "count")?)?,
        },
        "simple_block" => Cf::SimpleBlock {
            to: p_bsp(js(j, "to_place")?)?,
            on: name_list(j, "can_place_on"),
            in_: name_list(j, "can_place_in"),
            under: name_list(j, "can_place_under"),
        },
        "spring_feature" => Cf::Spring {
            // the config holds a *fluid* state and vanilla places its legacy block
            // (`createLegacyBlock`): falling water is level 8, a source is level 0
            fluid: {
                let st = js(j, "state")?;
                let (n, p) = bs_parts(st)?;
                // the config holds a *fluid* state; vanilla places its legacy block
                // (`createLegacyBlock`): falling water is level 8, a source level 0
                let falling = J::Obj(vec![(
                    "level".to_string(),
                    J::Str(
                        if jbool(st, "falling", false) {
                            "8"
                        } else {
                            "0"
                        }
                        .to_string(),
                    ),
                )]);
                let props = if n.ends_with("water") || n.ends_with("lava") {
                    Some(&falling)
                } else {
                    p
                };
                breg()?.state_of(&n, props)
            },
            req_below: jbool(j, "requires_block_below", true),
            rock: jint(j, "rock_count", 4),
            hole: jint(j, "hole_count", 1),
            valid: name_list(j, "valid_blocks").into_iter().collect(),
        },
        "seagrass" => Cf::Seagrass {
            prob: jnum(j, "probability").unwrap_or(0.0),
        },
        "kelp" => Cf::Kelp,
        // `minecraft:flower` is the same config shape (a patch of simple blocks), just a
        // different type name - it was one of the 34 types with no arm, so every flower
        // patch in the pack did nothing.
        "flower" | "random_patch" => Cf::RandomPatch {
            feature: Box::new(pf_any(js(j, "feature")?)?),
            tries: jint(j, "tries", 0),
            xz: jint(j, "xz_spread", 0),
            y: jint(j, "y_spread", 0),
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
            Cf::BlockBlob {
                state: breg()?.state_of(&n, p),
            }
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

// The biome list `FeatureSorter.buildFeaturesPerStep` receives for the overworld: the
// noise biome source's possible biomes. It is *not* the 64-entry registry - the nether,
// the end and `the_void` are absent - and the order matters: it seeds the per-feature
// index that every placed feature's RNG offset is derived from. Steps whose lists differ
// between biomes (2, 9, 10) are sensitive to it; the ore step (6) is not, because every
// biome lists the same ores in the same order. Measured from a 1.21.1 server by logging
// `deg.a`'s first argument (see tools/carve-agent).
const OW_BIOMES: &[&str] = &[
    "minecraft:mushroom_fields",
    "minecraft:deep_frozen_ocean",
    "minecraft:frozen_ocean",
    "minecraft:deep_cold_ocean",
    "minecraft:cold_ocean",
    "minecraft:deep_ocean",
    "minecraft:ocean",
    "minecraft:deep_lukewarm_ocean",
    "minecraft:lukewarm_ocean",
    "minecraft:warm_ocean",
    "minecraft:stony_shore",
    "minecraft:swamp",
    "minecraft:mangrove_swamp",
    "minecraft:snowy_slopes",
    "minecraft:snowy_plains",
    "minecraft:snowy_beach",
    "minecraft:windswept_gravelly_hills",
    "minecraft:grove",
    "minecraft:windswept_hills",
    "minecraft:snowy_taiga",
    "minecraft:windswept_forest",
    "minecraft:taiga",
    "minecraft:plains",
    "minecraft:meadow",
    "minecraft:beach",
    "minecraft:forest",
    "minecraft:old_growth_spruce_taiga",
    "minecraft:flower_forest",
    "minecraft:birch_forest",
    "minecraft:dark_forest",
    "minecraft:savanna_plateau",
    "minecraft:savanna",
    "minecraft:jungle",
    "minecraft:badlands",
    "minecraft:desert",
    "minecraft:wooded_badlands",
    "minecraft:jagged_peaks",
    "minecraft:stony_peaks",
    "minecraft:frozen_river",
    "minecraft:river",
    "minecraft:ice_spikes",
    "minecraft:old_growth_pine_taiga",
    "minecraft:sunflower_plains",
    "minecraft:old_growth_birch_forest",
    "minecraft:sparse_jungle",
    "minecraft:bamboo_jungle",
    "minecraft:eroded_badlands",
    "minecraft:windswept_savanna",
    "minecraft:cherry_grove",
    "minecraft:frozen_peaks",
    "minecraft:dripstone_caves",
    "minecraft:lush_caves",
    "minecraft:deep_dark",
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
        sorted.sort_by_key(|a| (a.0, a.1));
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
    for (name, b) in Gen::dp_files_any(&["configured_feature", "feature"]) {
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
                    if let Some(arr) = s.as_arr() {
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
        let Some(steps) = reg.bsteps.get(*b) else {
            continue;
        };
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
        let Some(steps) = reg.bsteps.get(*b) else {
            continue;
        };
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
    keys.sort_by_key(|a| (a.0, a.1));
    for k in keys {
        dfs_step(k, &edges, &mut visited, &mut in_progress, &mut order);
    }
    order.reverse();
    let mut by_step: Vec<Vec<(u32, String)>> = vec![vec![]; max_steps];
    for (step, slot) in by_step.iter_mut().enumerate() {
        for (gi, k) in order.iter().filter(|k| k.0 == step).enumerate() {
            if let Some(name) = name_of.get(k) {
                slot.push((gi as u32, name.clone()));
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
            match fr.cfg.get(n) {
                Some(cf) => gen_cf(cf, fr, ctx, rng, pos),
                None => {
                    // A configured feature that failed to parse is skipped
                    // silently, which looks exactly like "the feature chose not
                    // to place anything".
                    if std::env::var("CF_MISSING").is_ok() {
                        eprintln!("CFMISSING {n}");
                    }
                }
            }
        }
        FRef::Placed(n) => match fr.placed.get(n) {
            // the child runs its own modifier chain on the same RNG stream
            Some(pf) => gen_pf(pf, fr, ctx, rng, pos),
            None => {
                if std::env::var("CF_MISSING").is_ok() {
                    eprintln!("CFMISSING placed {n}");
                }
            }
        },
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
            let (x, z) = (pos.0 + rng.next_bound(16), pos.2 + rng.next_bound(16));
            if let Ok(spec) = std::env::var("INSQ_TRACE") {
                if let Some((a, c)) = spec.split_once(',') {
                    if let (Ok(cx), Ok(cz)) = (a.parse::<i32>(), c.parse::<i32>()) {
                        if (x >> 4) == cx && (z >> 4) == cz {
                            let cf = CUR_FEAT.load(std::sync::atomic::Ordering::Relaxed);
                            eprintln!("INSQ {x} {z} feature={},{}", cf >> 32, cf & 0xFFFF);
                        }
                    }
                }
            }
            out.push((x, pos.1, z));
        }
        PM::Offset(xz, ysp) => {
            out.push((
                pos.0 + xz.get(rng),
                pos.1 + ysp.get(rng),
                pos.2 + xz.get(rng),
            ));
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
        PM::Fixed => out.push(pos),
        PM::Biome => {
            if let Some(b) = ctx.biome(pos.0, pos.1, pos.2) {
                let ok = fr
                    .ball
                    .get(&b)
                    .map(|s| s.contains(&pf.name))
                    .unwrap_or(false);
                if let Ok(spec) = std::env::var("BIOMEF_TRACE") {
                    if let Some((a, c)) = spec.split_once(',') {
                        if let (Ok(cx), Ok(cz)) = (a.parse::<i32>(), c.parse::<i32>()) {
                            if (pos.0 >> 4) == cx && (pos.2 >> 4) == cz {
                                let cf = CUR_FEAT.load(std::sync::atomic::Ordering::Relaxed);
                                eprintln!(
                                    "BIOMEF {} {} {} {} biome={} feature={},{}",
                                    pos.0,
                                    pos.1,
                                    pos.2,
                                    ok,
                                    b,
                                    cf >> 32,
                                    cf & 0xFFFF
                                );
                            }
                        }
                    }
                }
                if ok {
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

fn ore_should_place(
    discard: f32,
    ctx: &mut FC,
    rt: &RT,
    pos: (i32, i32, i32),
    r: &mut Xor,
) -> bool {
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
    sx: f64,
    ex: f64,
    sz: f64,
    ez: f64,
    sy: f64,
    ey: f64,
    xb: i32,
    yb: i32,
    zb: i32,
    hs: i32,
    vs: i32,
    size: i32,
    discard: f32,
    targets: &[(RT, u32)],
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

/// (feature index << 32) | step of the placed feature currently being generated.
pub(crate) static CUR_FEAT: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(-1);

fn gen_ore(
    ctx: &mut FC,
    rng: &mut Xor,
    pos: (i32, i32, i32),
    size: i32,
    discard: f32,
    targets: &[(RT, u32)],
    scattered: bool,
) {
    if let Ok(spec) = std::env::var("ORE_TRACE") {
        if let Some((a, b)) = spec.split_once(',') {
            if a.parse::<i32>() == Ok(pos.0 >> 4) && b.parse::<i32>() == Ok(pos.2 >> 4) {
                let cf = CUR_FEAT.load(std::sync::atomic::Ordering::Relaxed);
                eprintln!(
                    "ORE {} {} {} size={} feature={},{}",
                    pos.0,
                    pos.1,
                    pos.2,
                    size,
                    cf >> 32,
                    cf & 0xFFFF
                );
            }
        }
    }
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
    let before = ctx.wr.len();
    for xp in n..=(n + q) {
        for zp in p..=(p + q) {
            if o <= ctx.hm_oc(xp, zp) {
                gen_vein(
                    ctx, rng, d, e, h, j, l, mm, n, o, p, q, r2, size, discard, targets,
                );
                if let Ok(spec) = std::env::var("ORE_CELL_TRACE") {
                    if spec == format!("{},{},{}", pos.0, pos.1, pos.2) {
                        eprintln!(
                            "ORECELL origin={pos:?} size={size} wrote={} radius={:?}",
                            ctx.wr.len() - before,
                            (d, e, h, j, l, mm)
                        );
                    }
                }
                return;
            }
        }
    }
}

/// Test helper: the cells one vein writes for a seed and position, with a rule test that
/// always matches, so the geometry can be compared cell for cell with vanilla's own output
/// (see evidence/gameplay/feature-parity/ore_andesite_666.txt).
#[cfg(test)]
pub(crate) fn ore_cells_for_test(
    seed: u64,
    pos: (i32, i32, i32),
    size: i32,
) -> Vec<(i32, i32, i32)> {
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
        Cf::Iceberg { state } => gen_iceberg(ctx, rng, pos, *state),
        Cf::DripstoneCluster(cfg) => gen_dripstone_cluster(ctx, rng, pos, cfg),
        Cf::LargeDripstone(cfg) => gen_large_dripstone(ctx, rng, pos, cfg),
        Cf::DesertWell => gen_desert_well(ctx, rng, pos),
        Cf::RootSystem(c) => gen_root_system(ctx, rng, pos, c),
        Cf::IceSpike => gen_ice_spike(ctx, rng, pos),
        Cf::SeaPickle { count } => {
            let Some(reg) = breg() else { return };
            let water = reg.state_of("minecraft:water", None);
            let n = count.get(rng);
            for _ in 0..n.max(0) {
                let dx = rng.next_bound(8) - rng.next_bound(8);
                let dz = rng.next_bound(8) - rng.next_bound(8);
                let (x, z) = (pos.0 + dx, pos.2 + dz);
                let y = ctx.hm_oc(x, z);
                // the pickles draw comes before both survival tests, so a rejected
                // cell still moves the stream
                let pickles = rng.next_bound(4) + 1;
                if ctx.out_of_h(y) || y <= crate::worldgen::min_y() {
                    continue;
                }
                if ctx.get(x, y, z) != water {
                    continue;
                }
                let below = ctx.get(x, y - 1, z);
                if !(reg.side_solid(below, 1) || reg.name_of(below) == "minecraft:magma_block") {
                    continue;
                }
                // `SeaPickleBlock`'s default state is pickles=1 waterlogged=true, and
                // the feature only sets pickles
                let props = J::Obj(vec![
                    ("pickles".to_string(), J::Str(pickles.to_string())),
                    ("waterlogged".to_string(), J::Str("true".to_string())),
                ]);
                let st = reg.state_of("minecraft:sea_pickle", Some(&props));
                ctx.set(x, y, z, st);
            }
        }
        Cf::Geode(g) => gen_geode(ctx, rng, pos, g),
        Cf::Tree(t) => {
            if let Ok(spec) = std::env::var("TREE_TRACE") {
                // same one-chunk radius as the agent's -Dtreechunk
                if let Some((a, b)) = spec.split_once(',') {
                    if let (Ok(cx), Ok(cz)) = (a.parse::<i32>(), b.parse::<i32>()) {
                        let (dx, dz) = (pos.0 >> 4, pos.2 >> 4);
                        if (cx - dx).abs() <= 1 && (cz - dz).abs() <= 1 {
                            let cf = CUR_FEAT.load(std::sync::atomic::Ordering::Relaxed);
                            eprintln!(
                                "TREE {} {} {} feature={},{}",
                                pos.0,
                                pos.1,
                                pos.2,
                                cf >> 32,
                                cf & 0xFFFF
                            );
                        }
                    }
                }
            }
            crate::tree::gen_tree(ctx, rng, pos, t);
        }
        Cf::Ore {
            size,
            discard,
            targets,
            scattered,
        } => {
            gen_ore(ctx, rng, pos, *size, *discard, targets, *scattered);
        }
        Cf::Disk {
            half,
            radius,
            bsp,
            target,
        } => {
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
                        if std::env::var("DISK_TRACE").is_ok() && cx == 0 && cz == 0 {
                            let cf = CUR_FEAT.load(std::sync::atomic::Ordering::Relaxed);
                            eprintln!(
                                "DISK {x} {y} {z} block={} target={} feature={},{}",
                                reg.name_of(ctx.get(x, y, z)),
                                test_pred(target, reg, ctx, x, y, z),
                                cf >> 32,
                                cf & 0xFFFF
                            );
                        }
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
                    && !on
                        .iter()
                        .any(|t| *t == reg.name_of(ctx.get(pos.0, pos.1 - 1, pos.2)))
                {
                    return;
                }
                if !in_.is_empty()
                    && !in_
                        .iter()
                        .any(|t| *t == reg.name_of(ctx.get(pos.0, pos.1, pos.2)))
                {
                    return;
                }
                if !under.is_empty()
                    && !under
                        .iter()
                        .any(|t| *t == reg.name_of(ctx.get(pos.0, pos.1 + 1, pos.2)))
                {
                    return;
                }
            }
            let st = to.get(ctx, rng, pos.0, pos.1, pos.2);
            // `SimpleBlockFeature` (edl) places only where the state `canSurvive`
            // (`edl.a`: `if ($$4.a($$2, $$3)) ... else return false`). Without it a
            // patch places its block on any air cell inside the box - measured:
            // short_grass 362 cells against vanilla's 102, with the draw streams
            // matching exactly, so the outer placement and the patch spreads were
            // already right and only this check was missing.
            if let Some(reg) = breg() {
                let ok = can_place(reg, ctx, reg.name_of(st), pos.0, pos.1, pos.2);
                if std::env::var("SIMPLE_TRACE").is_ok() {
                    static N: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
                    if N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 100_000 {
                        eprintln!(
                            "SIMPLE {} {} {} state={} below={} at={} ok={ok}",
                            pos.0,
                            pos.1,
                            pos.2,
                            reg.name_of(st),
                            reg.name_of(ctx.get(pos.0, pos.1 - 1, pos.2)),
                            reg.name_of(ctx.get(pos.0, pos.1, pos.2))
                        );
                    }
                }
                if !ok {
                    return;
                }
            }
            ctx.set(pos.0, pos.1, pos.2, st);
        }
        Cf::Spring {
            fluid,
            req_below,
            rock,
            hole,
            valid,
        } => {
            // `SpringFeature` (edp): the block *below* must be valid, and with
            // `requires_block_below` the one below that too. This used to test the
            // block above, which is the opposite geometry (a spring hanging from a
            // ceiling instead of emerging from a rock floor).
            let Some(reg) = breg() else { return };
            if !valid.contains(reg.name_of(ctx.get(pos.0, pos.1 - 1, pos.2))) {
                return;
            }
            if *req_below && !valid.contains(reg.name_of(ctx.get(pos.0, pos.1 - 2, pos.2))) {
                return;
            }
            let at = ctx.get(pos.0, pos.1, pos.2);
            if !reg.is_air(at) && !valid.contains(reg.name_of(at)) {
                return;
            }
            let mut rocks = 0;
            let mut air = 0;
            // vanilla scans west, east, north, south and *above*
            for d in [4u8, 5, 2, 3, 1] {
                let (dx, dy, dz) = dir_off(d);
                let s = ctx.get(pos.0 + dx, pos.1 + dy, pos.2 + dz);
                if valid.contains(reg.name_of(s)) {
                    rocks += 1;
                }
                if reg.is_air(s) {
                    air += 1;
                }
            }
            if std::env::var("SPRING_TRACE").is_ok() {
                static SHOWN: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
                if SHOWN.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 6 {
                    eprintln!(
                        "SPRING {} {} {} at={} below={} below2={} rocks={rocks} holes={air} want={rock},{hole}",
                        pos.0, pos.1, pos.2,
                        reg.name_of(at),
                        reg.name_of(ctx.get(pos.0, pos.1 - 1, pos.2)),
                        reg.name_of(ctx.get(pos.0, pos.1 - 2, pos.2)),
                    );
                }
            }
            if rocks == *rock && air == *hole {
                ctx.set(pos.0, pos.1, pos.2, *fluid);
            }
        }
        Cf::Seagrass { prob } => {
            let Some(reg) = breg() else { return };
            // dx, dz (4 draws), the heightmap read (none), the water test (none),
            // then the tall/short draw - that order is what the streams compare
            let (x, z) = (
                pos.0 + rng.next_bound(8) - rng.next_bound(8),
                pos.2 + rng.next_bound(8) - rng.next_bound(8),
            );
            let y = ctx.hm_oc(x, z);
            if reg.name_of(ctx.get(x, y, z)) != "minecraft:water" {
                return;
            }
            let tall = rng.next_f64() < *prob;
            let below = ctx.get(x, y - 1, z);
            // `SeagrassBlock.mayPlaceOn`: sturdy top face, and not magma
            if !reg.side_solid(below, 1) || reg.name_of(below) == "minecraft:magma_block" {
                return;
            }
            let half = |v: &str| J::Obj(vec![("half".to_string(), J::Str(v.to_string()))]);
            if tall {
                if reg.name_of(ctx.get(x, y + 1, z)) != "minecraft:water" {
                    return;
                }
                let lo = reg.state_of("minecraft:tall_seagrass", Some(&half("lower")));
                let hi = reg.state_of("minecraft:tall_seagrass", Some(&half("upper")));
                ctx.set(x, y, z, lo);
                ctx.set(x, y + 1, z, hi);
            } else {
                ctx.set(x, y, z, reg.state_of("minecraft:seagrass", None));
            }
        }
        Cf::Kelp => {
            let Some(reg) = breg() else { return };
            let base_y = ctx.hm_oc(pos.0, pos.2);
            let mut p = (pos.0, base_y, pos.2);
            let trace = std::env::var("KELP_TRACE").is_ok();
            if !is_water_st(reg, ctx.get(p.0, p.1, p.2)) {
                if trace {
                    eprintln!("KELP x={} z={} y0={base_y} no-water-at-base", pos.0, pos.2);
                }
                return;
            }
            let max_height = 1 + rng.next_bound(10);
            for i in 0..=max_height {
                let here_water = is_water_st(reg, ctx.get(p.0, p.1, p.2));
                let above_water = is_water_st(reg, ctx.get(p.0, p.1 + 1, p.2));
                let plant_survives = can_place(reg, ctx, "minecraft:kelp_plant", p.0, p.1, p.2);
                if here_water && above_water && plant_survives {
                    if i == max_height {
                        let age = rng.next_bound(4) + 20;
                        let head = reg.with_prop(
                            reg.state_of("minecraft:kelp", None),
                            "age",
                            &age.to_string(),
                        );
                        ctx.set(p.0, p.1, p.2, head);
                    } else {
                        ctx.set(p.0, p.1, p.2, reg.state_of("minecraft:kelp_plant", None));
                    }
                    if trace && i == max_height {
                        eprintln!(
                            "KELP x={} z={} y0={base_y} maxh={max_height} stem={max_height} full-height head_y={}",
                            p.0,
                            p.2,
                            base_y + max_height
                        );
                    }
                } else if i > 0 {
                    if trace {
                        eprintln!(
                            "KELP x={} z={} y0={base_y} maxh={max_height} stem={i} stopped_at={} here_water={here_water} above_water={above_water} survives={plant_survives}",
                            p.0, p.2, p.1
                        );
                    }
                    let base = (p.0, p.1 - 1, p.2);
                    let below_is_head =
                        reg.name_of(ctx.get(base.0, base.1 - 1, base.2)) == "minecraft:kelp";
                    if can_place(reg, ctx, "minecraft:kelp", base.0, base.1, base.2)
                        && !below_is_head
                    {
                        let age = rng.next_bound(4) + 20;
                        let head = reg.with_prop(
                            reg.state_of("minecraft:kelp", None),
                            "age",
                            &age.to_string(),
                        );
                        ctx.set(base.0, base.1, base.2, head);
                    }
                    break;
                }
                // `KelpFeature.place` moves up unconditionally: `pos.move(UP)` is
                // outside its if/else. Advancing only on a placement makes the
                // rejected-at-i=0 case write its head one cell lower than vanilla.
                p.1 += 1;
            }
        }
        Cf::RandomPatch {
            feature,
            tries,
            xz,
            y,
        } => {
            let (xs, ys) = (xz + 1, y + 1);
            for _ in 0..*tries {
                // the arguments are evaluated left to right, so the draws go
                // xz, xz, y, y, xz, xz - a different grouping shifts every position
                let dx = rng.next_bound(xs) - rng.next_bound(xs);
                let dy = rng.next_bound(ys) - rng.next_bound(ys);
                let dz = rng.next_bound(xs) - rng.next_bound(xs);
                gen_pf(feature, fr, ctx, rng, (pos.0 + dx, pos.1 + dy, pos.2 + dz));
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
        Cf::VegPatch {
            depth,
            extra_bottom,
            extra_edge,
            replaceable,
            ground,
            floor,
            chance,
            vfeat,
            xz,
            vrange,
            waterlog,
        } => {
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
                    let pp = if *waterlog {
                        sp
                    } else {
                        (sp.0, sp.1 - inward, sp.2)
                    };
                    gen_pf(vfeat, fr, ctx, rng, pp);
                    if *waterlog {
                        let placed = ctx.get(pp.0, pp.1, pp.2);
                        if !reg.is_air(placed) && !is_water_st(reg, ctx.get(pp.0, pp.1, pp.2)) {
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
        Cf::BlockColumn {
            layers,
            down,
            allowed,
            prio,
        } => {
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
                        None => {
                            ctx.is_air(mp.0, mp.1, mp.2)
                                || reg.name_of(ctx.get(mp.0, mp.1, mp.2)) == "minecraft:water"
                        }
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
                        let bp = (
                            pos.0 + x as i32 - 8,
                            pos.1 + y as i32 - 4,
                            pos.2 + z as i32 - 8,
                        );
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
                            let bp = (
                                pos.0 + x as i32 - 8,
                                pos.1 + y as i32 - 4,
                                pos.2 + z as i32 - 8,
                            );
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
                                let bp = (
                                    pos.0 + x as i32 - 8,
                                    pos.1 + y as i32 - 4,
                                    pos.2 + z as i32 - 8,
                                );
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
                        if n == "minecraft:water"
                            || n == "minecraft:ice"
                            || n == "minecraft:packed_ice"
                        {
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
                        if reg.is_air(cur)
                            || reg.is_replaceable(cur)
                            || reg.name_of(cur).contains("grass")
                            || reg.name_of(cur).contains("snow")
                        {
                            ctx.set(bp.0, bp.1, bp.2, *state);
                        }
                    }
                }
            }
        }
        Cf::UnderMagma {
            floor,
            prob,
            radius,
        } => {
            // `UnderwaterMagmaFeature` (eds): Column.scan requires the origin to be
            // water, walks down while it stays water, and only then places magma
            // around the floor. Every cell in the cube draws `nextFloat()` first
            // and is kept when the draw is below the probability *and* the cell is
            // fully enclosed (itself, the cell above, and all six neighbours are
            // neither water nor air).
            let Some(reg) = breg() else { return };
            let magma = reg.state_of("minecraft:magma_block", None);
            if !is_water_st(reg, ctx.get(pos.0, pos.1, pos.2)) {
                return;
            }
            let mut y = pos.1;
            let mut found = false;
            for _ in 1..*floor {
                if !is_water_st(reg, ctx.get(pos.0, y, pos.2)) {
                    found = true;
                    break;
                }
                y -= 1;
            }
            if !found {
                return;
            }
            let r = *radius;
            let wet_air = |ctx: &mut FC, reg: &crate::block_registry::BReg, p: (i32, i32, i32)| {
                let st = ctx.get(p.0, p.1, p.2);
                reg.is_air(st) || is_water_st(reg, st)
            };
            let enclosed = |ctx: &mut FC, reg: &crate::block_registry::BReg, p: (i32, i32, i32)| {
                if wet_air(ctx, reg, p) || wet_air(ctx, reg, (p.0, p.1 + 1, p.2)) {
                    return false;
                }
                for (dx, dy, dz) in [
                    (1, 0, 0),
                    (-1, 0, 0),
                    (0, 1, 0),
                    (0, -1, 0),
                    (0, 0, 1),
                    (0, 0, -1),
                ] {
                    if wet_air(ctx, reg, (p.0 + dx, p.1 + dy, p.2 + dz)) {
                        return false;
                    }
                }
                true
            };
            for dx in -r..=r {
                for dy in -r..=r {
                    for dz in -r..=r {
                        let bp = (pos.0 + dx, y + dy, pos.2 + dz);
                        let draw = rng.next_f32();
                        if draw >= *prob {
                            continue;
                        }
                        if !enclosed(ctx, reg, bp) {
                            continue;
                        }
                        ctx.set(bp.0, bp.1, bp.2, magma);
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
                    let Some(b) = ctx.biome(x, top, z) else {
                        continue;
                    };
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
        Cf::Multiface {
            name,
            on,
            ceiling,
            wall,
            floor,
            search,
        } => {
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
            let place_on_faces =
                |ctx: &mut FC, rng: &mut Xor, p: (i32, i32, i32), exclude: Option<u8>| -> bool {
                    // shuffled direction order
                    let mut dd: Vec<u8> = dirs
                        .iter()
                        .copied()
                        .filter(|d| Some(*d) != exclude)
                        .collect();
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
                    if place_on_faces(
                        ctx,
                        rng,
                        p,
                        Some(match d {
                            0 => 1,
                            1 => 0,
                            2 => 3,
                            3 => 2,
                            4 => 5,
                            _ => 4,
                        }),
                    ) {
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
                    Some(&mkprops(&[
                        ("age", "1"),
                        ("leaves", leaves),
                        ("stage", stage),
                    ])),
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
                if !reg.side_solid(
                    s,
                    match d {
                        2 => 3,
                        3 => 2,
                        4 => 5,
                        _ => 4,
                    },
                ) {
                    continue;
                }
                let face = match d {
                    2 => "south",
                    3 => "north",
                    4 => "east",
                    _ => "west",
                };
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
                        let boundary = dx == min_x
                            || dy == -1
                            || dz == min_z
                            || dx == max_x
                            || dy == 4
                            || dz == max_z;
                        let cur = ctx.get(wp.0, wp.1, wp.2);
                        if boundary {
                            let below = ctx.get(wp.0, wp.1 - 1, wp.2);
                            if wp.1 >= ctx.g.min_y && !reg.is_solid(below) {
                                ctx.set(wp.0, wp.1, wp.2, cave_air);
                            } else if reg.is_solid(cur) && reg.name_of(cur) != "minecraft:chest" {
                                ctx.set(
                                    wp.0,
                                    wp.1,
                                    wp.2,
                                    if dy == -1 && rng.next_bound(4) != 0 {
                                        mossy
                                    } else {
                                        cobble
                                    },
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
                let (x, z) = if horiz {
                    (p.0 + i, p.2)
                } else {
                    (p.0, p.2 + i)
                };
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

// ---------- carvers (GenerationStep.Carving.AIR) ----------
//
// Vanilla `NoiseBasedChunkGenerator.applyCarvers` walks the -8..8 chunk
// neighbourhood, asks each neighbour's biome (sampled at that chunk's minimum
// corner, y=0) for its carver list, reseeds a `LegacyRandomSource` per carver
// index and lets every carver that "starts" carve tunnels into THIS chunk
// through the chunk's shared `CarvingMask`. The port keeps that shape: one pass
// per target chunk, run before its features, writing into the same override map
// the feature layer uses so everything downstream sees the caves.
//
// Every constant below is from the 1.21.1 bytecode (`ebb` CaveWorldCarver,
// `ebe` WorldCarver, `eaw` CanyonWorldCarver, `dux` CarvingMask,
// `dzb.a(arb,long,...)` applyCarvers), not from a decompiler's guess.

/// `2 * getRange() - 1` as a block distance: WorldCarver.getRange() is 8 and the
/// cave tunnel count is `112 - nextInt(112 / 4)` steps; the canyon multiplies the
/// same 112 by its `distance_factor`.
const CARVE_STEPS: i32 = 112;

/// `2 * PI` as the 1.21.1 bytecode spells it (`ldc 6.2831855f`).
const TAU: f32 = 6.283_185_5;

#[derive(Clone, Copy, PartialEq, Debug)]
enum CKind {
    Cave,
    Canyon,
}

/// A `VerticalAnchor`: absolute, above the bottom of the world, or below its top.
#[derive(Clone, Copy)]
enum Anchor {
    Abs(i32),
    AboveBottom(i32),
    BelowTop(i32),
}

impl Anchor {
    fn resolve(self, min_y: i32, height: i32) -> i32 {
        match self {
            Anchor::Abs(v) => v,
            Anchor::AboveBottom(v) => min_y + v,
            Anchor::BelowTop(v) => min_y + height - v,
        }
    }
}

/// A `FloatProvider` sample. `uniform` is min-inclusive/max-exclusive; the
/// `trapezoid` form draws twice and lands in the middle more often.
#[derive(Clone, Copy)]
enum FProv {
    Const(f32),
    Uniform(f32, f32),
    Trapezoid(f32, f32, f32),
}

impl FProv {
    fn sample(self, r: &mut Lcg) -> f32 {
        match self {
            FProv::Const(v) => v,
            FProv::Uniform(lo, hi) => r.next_f32() * (hi - lo) + lo,
            FProv::Trapezoid(lo, hi, plateau) => {
                let range = hi - lo;
                let half = (range - plateau) / 2.0;
                let flat = range - half;
                lo + r.next_f32() * flat + r.next_f32() * half
            }
        }
    }
}

/// A `HeightProvider`. `minecraft:uniform` is `eia` in 1.21.1, which resolves
/// both anchors and calls `Mth.randomBetweenInclusive` - a *single*
/// `nextInt(max - min + 1)`. `ehv` (trapezoid, `nextInt` twice) is a different
/// provider; copying its shape shifts every later draw in the tunnel.
#[derive(Clone, Copy)]
enum HProv {
    Const(Anchor),
    Uniform(Anchor, Anchor),
}

impl HProv {
    fn sample(self, r: &mut Lcg, min_y: i32, height: i32) -> i32 {
        match self {
            HProv::Const(a) => a.resolve(min_y, height),
            HProv::Uniform(a, b) => {
                let lo = a.resolve(min_y, height);
                let hi = b.resolve(min_y, height);
                if lo > hi {
                    // `eia` warns once and returns the minimum.
                    return lo;
                }
                r.next_bound(hi - lo + 1) + lo
            }
        }
    }
}

struct CarverDef {
    kind: CKind,
    probability: f32,
    y: HProv,
    y_scale: FProv,
    lava: Anchor,
    replaceable: String,
    // CaveCarverConfiguration
    floor: FProv,
    horiz: FProv,
    vert: FProv,
    // CanyonCarverConfiguration
    rotation: FProv,
    canyon: Option<CanyonShape>,
}

/// `CanyonShapeConfiguration`, flattened. Field order matches its codec:
/// distance_factor, thickness, width_smoothness, horizontal_radius_factor,
/// vertical_radius_default_factor, vertical_radius_center_factor.
#[derive(Clone, Copy)]
struct CanyonShape {
    distance: FProv,
    thickness: FProv,
    width_smoothness: i32,
    horizontal_radius: FProv,
    vertical_default: f32,
    vertical_center: f32,
}

pub(crate) struct CarverReg {
    cfg: HashMap<String, CarverDef>,
    /// biome -> carver ids, in the order the biome lists them (the index is the
    /// seed offset, so the order is load-bearing)
    biome: HashMap<String, Vec<String>>,
}

pub(crate) fn creg() -> &'static CarverReg {
    static R: std::sync::OnceLock<CarverReg> = std::sync::OnceLock::new();
    R.get_or_init(load_creg)
}

fn load_creg() -> CarverReg {
    let mut reg = CarverReg {
        cfg: HashMap::new(),
        biome: HashMap::new(),
    };
    for (name, b) in Gen::dp_files("configured_carver") {
        if let Some(j) = jparse(&b) {
            match p_carver(&j) {
                Some(d) => {
                    reg.cfg.insert(name, d);
                }
                // A carver type this port does not implement must not silently
                // become a different algorithm: it stays out of the registry and
                // the miss is reported once.
                None => eprintln!("[carver] unsupported configured_carver {name}"),
            }
        }
    }
    for (name, b) in Gen::dp_files("biome") {
        if let Some(j) = jparse(&b) {
            if let Some(list) = jarr(&j, "carvers") {
                reg.biome
                    .insert(name, list.iter().map(|v| ns_name(v.str())).collect());
            }
        }
    }
    reg
}

fn p_anchor(j: &J) -> Option<Anchor> {
    if let Some(v) = jnum(j, "absolute") {
        return Some(Anchor::Abs(v as i32));
    }
    if let Some(v) = jnum(j, "above_bottom") {
        return Some(Anchor::AboveBottom(v as i32));
    }
    if let Some(v) = jnum(j, "below_top") {
        return Some(Anchor::BelowTop(v as i32));
    }
    None
}

fn p_fprov(j: &J) -> FProv {
    // the codec is `either(float, FloatProvider)`: a bare number is a constant
    if let J::Num(v) = j {
        return FProv::Const(*v as f32);
    }
    let ty = jstr(j, "type").unwrap_or_default();
    match ty.rsplit(':').next().unwrap_or(&ty) {
        "constant" => FProv::Const(jf32(j, "value", 0.0)),
        "trapezoid" => FProv::Trapezoid(
            jf32(j, "min", 0.0),
            jf32(j, "max", 0.0),
            jf32(j, "plateau", 0.0),
        ),
        _ => {
            let lo = jf32(j, "min_inclusive", jf32(j, "min_exclusive", 0.0));
            let hi = jf32(j, "max_exclusive", jf32(j, "max_inclusive", 0.0));
            FProv::Uniform(lo, hi)
        }
    }
}

fn fprov(j: &J, key: &str) -> FProv {
    js(j, key).map(p_fprov).unwrap_or(FProv::Const(0.0))
}

fn p_hprov(j: &J) -> Option<HProv> {
    let ty = jstr(j, "type")?;
    match ty.rsplit(':').next().unwrap_or(&ty) {
        "constant" => Some(HProv::Const(p_anchor(js(j, "value")?)?)),
        "uniform" => Some(HProv::Uniform(
            p_anchor(js(j, "min_inclusive")?)?,
            p_anchor(js(j, "max_inclusive")?)?,
        )),
        _ => None,
    }
}

fn p_carver(j: &J) -> Option<CarverDef> {
    let ty = jstr(j, "type")?;
    let kind = match ty.rsplit(':').next().unwrap_or(&ty) {
        "cave" => CKind::Cave,
        "canyon" => CKind::Canyon,
        _ => return None,
    };
    let cfg = js(j, "config")?;
    let canyon = js(cfg, "shape").map(|s| CanyonShape {
        distance: fprov(s, "distance_factor"),
        thickness: fprov(s, "thickness"),
        width_smoothness: jint(s, "width_smoothness", 3),
        horizontal_radius: fprov(s, "horizontal_radius_factor"),
        vertical_default: jf32(s, "vertical_radius_default_factor", 1.0),
        vertical_center: jf32(s, "vertical_radius_center_factor", 0.0),
    });
    if kind == CKind::Canyon && canyon.is_none() {
        return None;
    }
    Some(CarverDef {
        kind,
        probability: jf32(cfg, "probability", 0.0),
        y: p_hprov(js(cfg, "y")?)?,
        y_scale: p_fprov(js(cfg, "yScale")?),
        lava: p_anchor(js(cfg, "lava_level")?)?,
        // the JSON keeps the `#` tag marker; `in_tag` is keyed without it
        replaceable: ns_name(jstr(cfg, "replaceable")?.trim_start_matches('#')),
        floor: fprov(cfg, "floor_level"),
        horiz: fprov(cfg, "horizontal_radius_multiplier"),
        vert: fprov(cfg, "vertical_radius_multiplier"),
        canyon,
        rotation: fprov(cfg, "vertical_rotation"),
    })
}

/// `WorldgenRandom.setLargeFeatureSeed`: two raw longs and a XOR, no `|1` — the
/// population seed used by the feature layer is a different function.
fn set_large_feature_seed(r: &mut Lcg, seed: u64, cx: i32, cz: i32) {
    if let Ok(spec) = std::env::var("CARVE_RAW") {
        if let Some((a, b)) = spec.split_once(',') {
            if a.parse::<i32>() == Ok(cx) && b.parse::<i32>() == Ok(cz) {
                crate::generation_math::RAW_ARM.store(0, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }
    let mut s = Lcg::new(seed);
    let l = s.next_i64();
    let m = s.next_i64();
    *r = Lcg::new(((cx as i64).wrapping_mul(l) ^ (cz as i64).wrapping_mul(m) ^ seed as i64) as u64);
}

/// `CaveWorldCarver.getYScale`: two floats plus an occasional 1-in-10 room bulge.
fn cave_rng_y_scale(r: &mut Lcg) -> f32 {
    let mut f = r.next_f32() * 2.0 + r.next_f32();
    if r.next_bound(10) == 0 {
        f *= r.next_f32() * r.next_f32() * 3.0 + 1.0;
    }
    f
}

/// `CanyonWorldCarver.getWidths`: one squared width per block of world height,
/// re-drawn every `width_smoothness` blocks. The height index is absolute world
/// height, so the array is `getGenDepth()` long from the bottom.
fn canyon_widths(depth: usize, smoothness: i32, r: &mut Lcg) -> Vec<f32> {
    let mut out = vec![0.0f32; depth];
    let mut f = 1.0f32;
    for (i, slot) in out.iter_mut().enumerate() {
        // `i == 0 ||` short-circuits, so the first entry never draws nextInt
        if i == 0 || r.next_bound(smoothness) == 0 {
            f = 1.0 + r.next_f32() * r.next_f32();
        }
        *slot = f * f;
    }
    out
}

/// `CanyonWorldCarver.getY`: a triangular vertical-radius window over the
/// branch index, times `yScale`, times a 0.75..1.0 jitter.
fn canyon_y(shape: CanyonShape, r: &mut Lcg, y_scale: f64, branch_count: f32, i: f32) -> f64 {
    let w = 1.0 - ((0.5 - i / branch_count).abs() * 2.0);
    let f = shape.vertical_default + shape.vertical_center * w;
    f as f64 * y_scale * (r.next_f32() * 0.25 + 0.75) as f64
}

/// One target chunk's carving state: the shared mask, the cells written so far,
/// and the `CarvingContext` facts the carve reads (world bounds, floor level).
struct CarveRun {
    tx: i32,
    tz: i32,
    min_y: i32,
    height: i32,
    /// `floor_level` of the carver being applied: `skip` rejects cells at or
    /// below it, and vanilla builds that predicate once per carve call.
    floor: f64,
    mask: Vec<u64>,
    out: HashMap<(u8, i32, u8), u32>,
    /// Probe mode: pretend every cell is stone and every aquifer sample is cave
    /// air, and print the writes. That is the world `CarverProbe.java` gives
    /// vanilla, so the two cell sets can be compared without a worldgen behind
    /// either side.
    probe: bool,
    probe_air: u32,
    probe_lava: u32,
    /// First cell the current source chunk wrote, for the `SRC` trace that is
    /// compared against the real server's carve agent.
    mark: Option<(i32, i32, i32)>,
}

impl CarveRun {
    #[inline]
    fn idx(&self, lx: i32, y: i32, lz: i32) -> usize {
        (((y - self.min_y) << 8) | ((lz & 15) << 4) | (lx & 15)) as usize
    }
    #[inline]
    fn mask_get(&self, lx: i32, y: i32, lz: i32) -> bool {
        let i = self.idx(lx, y, lz);
        self.mask[i >> 6] >> (i & 63) & 1 != 0
    }
    #[inline]
    fn mask_set(&mut self, lx: i32, y: i32, lz: i32) {
        let i = self.idx(lx, y, lz);
        self.mask[i >> 6] |= 1 << (i & 63);
    }
    /// The chunk's block state as the carver sees it: cells carved earlier in
    /// this pass, else the surface-rule terrain.
    fn read(&self, g: &mut Gen, cc: &ChunkCols, x: i32, y: i32, z: i32) -> u32 {
        if self.probe {
            return breg().map_or(1, |b| b.state_of("minecraft:stone", None));
        }
        // Vanilla hands `carveBlock` the *target* chunk and both the read and the
        // write go through `pos` folded with `& 15`, so out-of-chunk candidates
        // are judged against this chunk's block at the folded position. Reading
        // the neighbour's terrain instead (tried 2026-10-22) changes nothing and
        // is not what the bytecode does.
        match self.out.get(&((x & 15) as u8, y, (z & 15) as u8)) {
            Some(&st) => st,
            None => g.terrain_cell(cc, x, y, z),
        }
    }
}

/// `NoiseBasedChunkGenerator.applyCarvers` for one target chunk.
///
/// Diagnostics are env-gated and deliberately small, because each one is a way to
/// compare against vanilla without a second implementation standing in for it:
///
/// * `MC_SPINE_NOCARVE` — skip carvers (the "no carvers" world fixture).
/// * `CARVE_TRACE` — one line per chunk: configs loaded, starts, cells written.
/// * `CARVE_PROBE=<tx,tz>|all` — carve an all-stone world (every ellipsoid cell is
///   replaceable and the aquifer answers cave_air) and print the writes. That is
///   the world `tools/vanilla-probe/CarverProbe.java` gives vanilla, so the two
///   cell sets compare directly.
/// * `CARVE_ONLY=<carver id>` — run a single carver (cave / canyon / …).
/// * `CARVE_DUMP_WRITES=<tx,tz>` — print the real (terrain-filtered) writes.
pub(crate) fn apply_carvers(g: &mut Gen, tx: i32, tz: i32) {
    if std::env::var("MC_SPINE_NOCARVE").is_ok() {
        return;
    }
    let reg = creg();
    if reg.cfg.is_empty() || reg.biome.is_empty() || breg().is_none() {
        return;
    }
    let Some(cc) = g.cols(tx, tz) else { return };
    let trace = std::env::var("CARVE_TRACE").is_ok();
    let here = format!("{tx},{tz}");
    let probe_env = std::env::var("CARVE_PROBE").ok();
    let probe_here = probe_env.as_deref() == Some("all") || probe_env.as_deref() == Some(&here);
    let dump_here = std::env::var("CARVE_DUMP_WRITES").ok().as_deref() == Some(&here);
    let only = std::env::var("CARVE_ONLY").ok();
    let (min_y, height, wseed) = (g.min_y, g.height, g.wseed());
    let mut run = CarveRun {
        tx,
        tz,
        min_y,
        height,
        floor: 0.0,
        mask: vec![0u64; (height as usize * 256).div_ceil(64)],
        out: HashMap::new(),
        probe: probe_here,
        probe_air: breg().map_or(12959, |b| b.state_of("minecraft:cave_air", None)),
        probe_lava: breg().map_or(96, |b| b.state_of("minecraft:lava", None)),
        mark: None,
    };
    let mut starts = 0u32;
    for ox in tx - 8..=tx + 8 {
        for oz in tz - 8..=tz + 8 {
            // the carver list comes from the biome at the neighbour chunk's
            // minimum corner at y=0, not from its centre
            let Some(biome) = g.biome_name_at(ox * 16, 0, oz * 16) else {
                continue;
            };
            let Some(list) = reg.biome.get(&biome) else {
                continue;
            };
            for (k, name) in list.iter().enumerate() {
                if only.as_deref().is_some_and(|only| only != name) {
                    continue;
                }
                let Some(c) = reg.cfg.get(name) else { continue };
                let mut r = Lcg::new(0);
                set_large_feature_seed(&mut r, wseed.wrapping_add(k as u64), ox, oz);
                if r.next_f32() > c.probability {
                    continue;
                }
                starts += 1;
                if let Ok(spec) = std::env::var("CARVE_RNG") {
                    if spec == format!("{ox},{oz}") || spec == "all" {
                        crate::generation_math::RNG_TRACE
                            .store(400, std::sync::atomic::Ordering::Relaxed);
                    }
                }
                if std::env::var("CARVE_TRACE").is_ok() {
                    eprintln!("START target={tx},{tz} origin={ox},{oz} k={k} {name} ({biome})");
                }
                run.mark = None;
                match c.kind {
                    CKind::Cave => run.cave(g, &cc, c, &mut r, ox, oz),
                    CKind::Canyon => run.canyon(g, &cc, c, &mut r, ox, oz),
                }
                if let Ok(spec) = std::env::var("CARVE_SRC") {
                    if spec == "all" || spec == format!("{tx},{tz}") {
                        if let Some((x, y, z)) = run.mark {
                            eprintln!("SRC ox={ox} oz={oz} k={k} first={x},{y},{z}");
                        }
                    }
                }
            }
        }
    }
    if trace {
        eprintln!(
            "[carve] chunk {tx},{tz}: {} starts, {} mask bits, {} cells written",
            starts,
            run.mask.iter().map(|w| w.count_ones() as u64).sum::<u64>(),
            run.out.len()
        );
    }
    if probe_here || dump_here {
        let tag = if probe_here { "CELL" } else { "WCELL" };
        let mut cells: Vec<((u8, i32, u8), u32)> = run.out.iter().map(|(k, v)| (*k, *v)).collect();
        cells.sort();
        for ((lx, y, lz), st) in cells {
            eprintln!(
                "{tag} {} {} {} {}",
                tx * 16 + lx as i32,
                y,
                tz * 16 + lz as i32,
                st
            );
        }
        eprintln!(
            "PROBE {} cells={}",
            if probe_here { "stone" } else { "world" },
            run.out.len()
        );
    }
    for ((lx, y, lz), st) in std::mem::take(&mut run.out) {
        g.feat_write(tx * 16 + lx as i32, y, tz * 16 + lz as i32, st);
    }
}

impl CarveRun {
    /// `CaveWorldCarver.carve`.
    fn cave(&mut self, g: &mut Gen, cc: &ChunkCols, c: &CarverDef, r: &mut Lcg, ox: i32, oz: i32) {
        // nextInt(nextInt(nextInt(getRange()) + 1) + 1); getRange() is 15 here
        let inner = r.next_bound(15);
        let mid = r.next_bound(inner + 1);
        let tunnels = r.next_bound(mid + 1);
        if std::env::var("CV_TRACE").is_ok() {
            eprintln!("CVSTART ox={ox} oz={oz} inner={inner} mid={mid} tunnels={tunnels}");
        }
        for _ in 0..tunnels {
            let x = (ox * 16 + r.next_bound(16)) as f64;
            let y = c.y.sample(r, self.min_y, self.height) as f64;
            let z = (oz * 16 + r.next_bound(16)) as f64;
            let hrm = c.horiz.sample(r) as f64;
            let vrm = c.vert.sample(r) as f64;
            self.floor = c.floor.sample(r) as f64;
            let mut branches = 1i32;
            if r.next_bound(4) == 0 {
                let room_scale = c.y_scale.sample(r) as f64;
                let thick = 1.0f32 + r.next_f32() * 6.0;
                // carveRoom: an ellipsoid 1.5 blocks fatter than the tunnel,
                // squashed vertically by yScale, centred one block +x
                let hr = 1.5 + (mth_sin(std::f32::consts::FRAC_PI_2) * thick) as f64;
                self.ellipsoid(g, cc, c, x + 1.0, y, z, hr, hr * room_scale);
                branches += r.next_bound(4);
            }
            if std::env::var("CV_TRACE").is_ok() {
                eprintln!("CVTUN ox={ox} oz={oz} x={x} y={y} z={z} hrm={hrm} vrm={vrm} floor={} branches={branches}", self.floor);
            }
            for _ in 0..branches {
                let yaw = r.next_f32() * TAU;
                let pitch = (r.next_f32() - 0.5) / 4.0;
                let y_scale = cave_rng_y_scale(r);
                // a tunnel is up to CARVE_STEPS long, minus up to a quarter
                let count = CARVE_STEPS - r.next_bound(CARVE_STEPS / 4);
                let seed = r.next_i64() as u64;
                if let Ok(spec) = std::env::var("CV_TUN") {
                    if spec == "all" || spec == format!("{ox},{oz}") {
                        eprintln!(
                            "TUN ox={ox} oz={oz} seed={seed} x={} y={} z={} yaw={yaw} pitch={pitch} count={count}",
                            x.floor(),
                            y.floor(),
                            z.floor()
                        );
                    }
                }
                self.tunnel(
                    g, cc, c, seed, x, y, z, hrm, vrm, y_scale, yaw, pitch, 0, count, 1.0,
                );
            }
        }
    }

    /// `CanyonWorldCarver.carve`.
    fn canyon(
        &mut self,
        g: &mut Gen,
        cc: &ChunkCols,
        c: &CarverDef,
        r: &mut Lcg,
        ox: i32,
        oz: i32,
    ) {
        // `p_carver` refuses a canyon config without a shape, so this cannot be
        // reached with one; the guard keeps the borrow honest.
        let Some(shape) = c.canyon else { return };
        let x = (ox * 16 + r.next_bound(16)) as f64;
        let y = c.y.sample(r, self.min_y, self.height) as f64;
        let z = (oz * 16 + r.next_bound(16)) as f64;
        let yaw = r.next_f32() * TAU;
        let rotation = c.rotation.sample(r);
        let y_scale = c.y_scale.sample(r) as f64;
        let thickness = shape.thickness.sample(r);
        let count = (CARVE_STEPS as f32 * shape.distance.sample(r)) as i32;
        let seed = r.next_i64() as u64;
        self.canyon_tunnel(
            g, cc, c, seed, x, y, z, thickness, yaw, rotation, 0, count, y_scale,
        );
    }

    /// `CanyonWorldCarver.carveTunnel`: the cave walk with a per-height vertical
    /// width profile (`getWidths`) instead of an ellipsoid, and its own `skip`.
    #[expect(clippy::too_many_arguments)]
    fn canyon_tunnel(
        &mut self,
        g: &mut Gen,
        cc: &ChunkCols,
        c: &CarverDef,
        seed: u64,
        mut x: f64,
        mut y: f64,
        mut z: f64,
        thickness: f32,
        mut yaw: f32,
        mut pitch: f32,
        branch_index: i32,
        branch_count: i32,
        y_scale: f64,
    ) {
        let Some(shape) = c.canyon else { return };
        let mut r = Lcg::new(seed);
        let widths = canyon_widths(self.height as usize, shape.width_smoothness, &mut r);
        let mut yaw_drift = 0.0f32;
        let mut pitch_drift = 0.0f32;
        for i in branch_index..branch_count {
            let mut d0 = 1.5
                + (mth_sin(std::f32::consts::PI * i as f32 / branch_count as f32) * thickness)
                    as f64;
            let d1 = d0 * y_scale;
            d0 *= shape.horizontal_radius.sample(&mut r) as f64;
            let d1 = canyon_y(shape, &mut r, d1, branch_count as f32, i as f32);
            let cp = mth_cos(pitch);
            let sp = mth_sin(pitch);
            x += (mth_cos(yaw) * cp) as f64;
            y += sp as f64;
            z += (mth_sin(yaw) * cp) as f64;
            pitch *= 0.7;
            pitch += pitch_drift * 0.05;
            yaw += yaw_drift * 0.05;
            pitch_drift *= 0.8;
            yaw_drift *= 0.5;
            pitch_drift += (r.next_f32() - r.next_f32()) * r.next_f32() * 2.0;
            yaw_drift += (r.next_f32() - r.next_f32()) * r.next_f32() * 4.0;
            if r.next_bound(4) == 0 {
                continue;
            }
            if !can_reach(self.tx, self.tz, x, z, i, branch_count, thickness) {
                return;
            }
            self.canyon_ellipsoid(g, cc, c, x, y, z, d0, d1, &widths);
        }
    }

    /// `CanyonWorldCarver.carveEllipsoid`: the canyon profile is a vertical
    /// stack of widths, so the `skip` test is `(x^2 + z^2) * width + y^2 / 6`.
    #[expect(clippy::too_many_arguments)]
    fn canyon_ellipsoid(
        &mut self,
        g: &mut Gen,
        cc: &ChunkCols,
        c: &CarverDef,
        x: f64,
        y: f64,
        z: f64,
        hr: f64,
        vr: f64,
        widths: &[f32],
    ) -> bool {
        // The iteration range below is clipped to this chunk, so an ellipsoid
        // centred far away contributes nothing; the distance test is only a
        // cheap way to skip those. (Removing it changes no cell: measured
        // 2026-10-22.)
        let mid_x = (self.tx * 16 + 8) as f64;
        let mid_z = (self.tz * 16 + 8) as f64;
        let reach = 16.0 + hr * 2.0;
        if (x - mid_x).abs() > reach || (z - mid_z).abs() > reach {
            return false;
        }
        let (min_x, min_z) = (self.tx * 16, self.tz * 16);
        let x0 = ((x - hr).floor() as i32 - min_x - 1).max(0);
        let x1 = ((x + hr).floor() as i32 - min_x + 15).min(15);
        let y0 = ((y - vr).floor() as i32 - 1).max(self.min_y + 1);
        let y1 = ((y + vr).floor() as i32 + 1).min(self.min_y + self.height - 1 - 7);
        let z0 = ((z - hr).floor() as i32 - min_z - 1).max(0);
        let z1 = ((z + hr).floor() as i32 - min_z + 15).min(15);
        let mut carved = false;
        for lx in x0..=x1 {
            let bx = min_x + lx;
            let nx = (bx as f64 + 0.5 - x) / hr;
            for lz in z0..=z1 {
                let bz = min_z + lz;
                let nz = (bz as f64 + 0.5 - z) / hr;
                if nx * nx + nz * nz >= 1.0 {
                    continue;
                }
                let mut yy = y1;
                while yy > y0 {
                    // vanilla normalizes the y cell centre with `- 0.5`, not the
                    // `+ 0.5` the x and z coordinates use (carveEllipsoid's
                    // `i2d; ldc 0.5; dsub`). Using +0.5 lifts every ellipsoid by
                    // one cell at its top edge.
                    let ny = (yy as f64 - 0.5 - y) / vr;
                    let w = widths
                        .get((yy - self.min_y - 1) as usize)
                        .copied()
                        .unwrap_or(1.0) as f64;
                    if (nx * nx + nz * nz) * w + ny * ny / 6.0 < 1.0 && !self.mask_get(lx, yy, lz) {
                        self.mask_set(lx, yy, lz);
                        carved |= self.carve_block(g, cc, c, bx, yy, bz);
                    }
                    yy -= 1;
                }
            }
        }
        carved
    }

    /// `CaveWorldCarver.carveTunnel`: a walk along (yaw, pitch) that lays an
    /// ellipsoid every step it does not reject, and forks once when `yScale` is
    /// large. The RNG is `RandomSource.create(seed)` — a `LegacyRandomSource`.
    #[expect(clippy::too_many_arguments)]
    fn tunnel(
        &mut self,
        g: &mut Gen,
        cc: &ChunkCols,
        c: &CarverDef,
        seed: u64,
        mut x: f64,
        mut y: f64,
        mut z: f64,
        hrm: f64,
        vrm: f64,
        y_scale: f32,
        mut yaw: f32,
        mut pitch: f32,
        branch_index: i32,
        branch_count: i32,
        ratio: f64,
    ) {
        let mut r = Lcg::new(seed);
        let split = r.next_bound(branch_count / 2) + branch_count / 4;
        let flat = r.next_bound(6) == 0;
        let mut pitch_drift = 0.0f32;
        let mut yaw_drift = 0.0f32;
        for i in branch_index..branch_count {
            let d0 = 1.5
                + (mth_sin(std::f32::consts::PI * i as f32 / branch_count as f32) * y_scale) as f64;
            let d1 = d0 * ratio;
            let cp = mth_cos(pitch);
            x += (mth_cos(yaw) * cp) as f64;
            y += mth_sin(pitch) as f64;
            z += (mth_sin(yaw) * cp) as f64;
            pitch *= if flat { 0.92 } else { 0.7 };
            pitch += pitch_drift * 0.1;
            yaw += yaw_drift * 0.1;
            pitch_drift *= 0.9;
            yaw_drift *= 0.75;
            pitch_drift += (r.next_f32() - r.next_f32()) * r.next_f32() * 2.0;
            yaw_drift += (r.next_f32() - r.next_f32()) * r.next_f32() * 4.0;
            if i == split && y_scale > 1.0 {
                let s1 = r.next_i64() as u64;
                let sc1 = r.next_f32() * 0.5 + 0.5;
                self.tunnel(
                    g,
                    cc,
                    c,
                    s1,
                    x,
                    y,
                    z,
                    hrm,
                    vrm,
                    sc1,
                    yaw - std::f32::consts::FRAC_PI_2,
                    pitch / 3.0,
                    i,
                    branch_count,
                    1.0,
                );
                let s2 = r.next_i64() as u64;
                let sc2 = r.next_f32() * 0.5 + 0.5;
                self.tunnel(
                    g,
                    cc,
                    c,
                    s2,
                    x,
                    y,
                    z,
                    hrm,
                    vrm,
                    sc2,
                    yaw + std::f32::consts::FRAC_PI_2,
                    pitch / 3.0,
                    i,
                    branch_count,
                    1.0,
                );
                return;
            } else if r.next_bound(4) != 0 {
                if !can_reach(self.tx, self.tz, x, z, i, branch_count, y_scale) {
                    return;
                }
                self.ellipsoid(g, cc, c, x, y, z, d0 * hrm, d1 * vrm);
            }
        }
    }

    /// `WorldCarver.carveEllipsoid`: clips the ellipsoid to this chunk and carves
    /// every cell inside it that the mask has not claimed and `skip` accepts.
    // the parameters are `WorldCarver.carveEllipsoid`'s own inputs
    #[expect(clippy::too_many_arguments)]
    fn ellipsoid(
        &mut self,
        g: &mut Gen,
        cc: &ChunkCols,
        c: &CarverDef,
        x: f64,
        y: f64,
        z: f64,
        hr: f64,
        vr: f64,
    ) -> bool {
        // The iteration range below is clipped to this chunk, so an ellipsoid
        // centred far away contributes nothing; the distance test is only a
        // cheap way to skip those. (Removing it changes no cell: measured
        // 2026-10-22.)
        let mid_x = (self.tx * 16 + 8) as f64;
        let mid_z = (self.tz * 16 + 8) as f64;
        let reach = 16.0 + hr * 2.0;
        if (x - mid_x).abs() > reach || (z - mid_z).abs() > reach {
            return false;
        }
        let (min_x, min_z) = (self.tx * 16, self.tz * 16);
        let x0 = ((x - hr).floor() as i32 - min_x - 1).max(0);
        let x1 = ((x + hr).floor() as i32 - min_x + 15).min(15);
        let y0 = ((y - vr).floor() as i32 - 1).max(self.min_y + 1);
        // fresh worlds have no below-zero retrogen, so the top stops 7 short
        let y1 = ((y + vr).floor() as i32 + 1).min(self.min_y + self.height - 1 - 7);
        let z0 = ((z - hr).floor() as i32 - min_z - 1).max(0);
        let z1 = ((z + hr).floor() as i32 - min_z + 15).min(15);
        let mut carved = false;
        for lx in x0..=x1 {
            let bx = min_x + lx;
            let nx = (bx as f64 + 0.5 - x) / hr;
            for lz in z0..=z1 {
                let bz = min_z + lz;
                let nz = (bz as f64 + 0.5 - z) / hr;
                if nx * nx + nz * nz >= 1.0 {
                    continue;
                }
                let mut yy = y1;
                while yy > y0 {
                    // vanilla normalizes the y cell centre with `- 0.5`, not the
                    // `+ 0.5` the x and z coordinates use (carveEllipsoid's
                    // `i2d; ldc 0.5; dsub`). Using +0.5 lifts every ellipsoid by
                    // one cell at its top edge.
                    let ny = (yy as f64 - 0.5 - y) / vr;
                    // `skip`: at or below the floor level, or outside the sphere
                    if ny > self.floor
                        && nx * nx + ny * ny + nz * nz < 1.0
                        && !self.mask_get(lx, yy, lz)
                    {
                        self.mask_set(lx, yy, lz);
                        carved |= self.carve_block(g, cc, c, bx, yy, bz);
                    }
                    yy -= 1;
                }
            }
        }
        carved
    }

    /// `WorldCarver.carveBlock`: replace a `#overworld_carver_replaceables` cell
    /// with the aquifer's substance (lava below the lava level), and restore the
    /// top material under a cave mouth that was opened through the surface.
    fn carve_block(
        &mut self,
        g: &mut Gen,
        cc: &ChunkCols,
        c: &CarverDef,
        x: i32,
        y: i32,
        z: i32,
    ) -> bool {
        let cur = self.read(g, cc, x, y, z);
        let br = match breg() {
            Some(br) => br,
            None => return false,
        };
        let reached_surface = cur == br.state_of("minecraft:grass_block", None)
            || cur == br.state_of("minecraft:podzol", None);
        if !br.in_tag(cur, &c.replaceable) {
            return false;
        }
        let lava_level = c.lava.resolve(self.min_y, self.height);
        let probed = if self.probe {
            let lava = self.probe_lava;
            Some(if y <= lava_level {
                lava
            } else {
                self.probe_air
            })
        } else {
            g.carve_state(x, y, z, lava_level)
        };
        let Some(st) = probed else {
            return false;
        };
        // vanilla writes with the *target* chunk and lets the position fold
        // (`x & 15`), so a tunnel from a source chunk up to 8 chunks away paints
        // its cells into the target chunk at folded coordinates. Keep that as
        // the default; MC_SPINE_NOFOLD=1 clips instead, for experiments.
        if std::env::var("MC_SPINE_NOFOLD").is_ok() && ((x >> 4) != self.tx || (z >> 4) != self.tz)
        {
            return false;
        }
        if self.mark.is_none() {
            self.mark = Some((x, y, z));
        }
        self.out.insert(((x & 15) as u8, y, (z & 15) as u8), st);
        if reached_surface && self.read(g, cc, x, y - 1, z) == br.state_of("minecraft:dirt", None) {
            let fluid = g.is_fluid_state(st);
            if let Some(top) = g.top_material_at(x, y - 1, z, fluid) {
                self.out
                    .insert(((x & 15) as u8, y - 1, (z & 15) as u8), top);
            }
        }
        true
    }
}

/// `WorldCarver.canReach`: stop walking a tunnel once it is so far from the
/// chunk and so late in its run that it cannot come back.
fn can_reach(
    tx: i32,
    tz: i32,
    x: f64,
    z: f64,
    branch_index: i32,
    branch_count: i32,
    y_scale: f32,
) -> bool {
    let dx = x - (tx * 16 + 8) as f64;
    let dz = z - (tz * 16 + 8) as f64;
    let left = (branch_count - branch_index) as f64;
    let reach = y_scale as f64 + 2.0 + 16.0;
    dx * dx + dz * dz - left * left <= reach * reach
}

// ---------- entry point ----------

fn pop_seed(seed: u64, bx: i32, bz: i32) -> u64 {
    // `WorldgenRandom.setDecorationSeed` (dzx.a(JII)J) runs on the *Xoroshiro*
    // source that `applyBiomeDecoration` wraps, not on an LCG, and combines the
    // two products with XOR:
    //   setSeed(seed); l = nextLong() | 1; m = nextLong() | 1;
    //   n = (x * l + z * m) ^ seed;      // products *added*, then XOR the seed
    let mut r = Xor::seed_worldgen(seed);
    let l = r.next_i64_legacy() | 1;
    let m = r.next_i64_legacy() | 1;
    let n = ((bx as i64)
        .wrapping_mul(l)
        .wrapping_add((bz as i64).wrapping_mul(m))
        ^ seed as i64) as u64;
    if std::env::var("POP_DBG").is_ok() {
        eprintln!("POPDBG seed={seed} bx={bx} bz={bz} l={l} m={m} -> {n}");
    }
    n
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
/// Dumps the feature-profile counters the test-gated probes fill.
#[cfg(test)]
pub(crate) fn feat_prof_dump() {
    let Some(m) = FEAT_PROF.get() else { return };
    let m = m.lock().unwrap();
    let mut v: Vec<(String, u128, u64)> = m.iter().map(|(k, (t, c))| (k.clone(), *t, *c)).collect();
    v.sort_by_key(|x| std::cmp::Reverse(x.1));
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

/// Populate one chunk (and the 3x3 around it, as a feature read would) so that a
/// diagnostic sees the same feature-layer state a generation loop would.
pub(crate) fn populate(g: &mut Gen, sx: i32, sz: i32) {
    // Vanilla's chunk status order is CARVERS then FEATURES, and both write into
    // the chunk's block state, so the carver pass runs here — before the feature
    // gate, because "no features" is a comparison fixture, not a carver switch
    // (`MC_SPINE_NOCARVE` is).
    apply_carvers(g, sx, sz);
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
    // Vanilla unions the *biome palettes of this chunk's sections*
    // (`applyBiomeDecoration` collects `section.getBiomes()` into a set), so a
    // feature runs if any 4x4x4 cell of the chunk carries its biome. Sampling a few
    // columns at y=0 instead reads the cave biomes and misses a surface biome such as
    // `river` - which silently skipped `trees_water` (step 9 index 48) in chunk
    // (-2,1) and left the canopies of its trees missing here (measured: the draw
    // streams for that chunk's trees_birch_and_oak then diverged at draw 120).
    let mut biomes: Vec<String> = vec![];
    let mut seen: HashSet<String> = HashSet::new();
    for dx in -1..=1 {
        for dz in -1..=1 {
            for b in g.chunk_biome_set(sx + dx, sz + dz).iter() {
                if seen.insert(b.clone()) {
                    biomes.push(b.clone());
                }
            }
        }
    }
    if let Ok(spec) = std::env::var("FEAT_LIST") {
        if let Ok(st) = spec.parse::<usize>() {
            if let Some(l) = fr.order.get(st) {
                for (gi, name) in l.iter() {
                    eprintln!("FEATLIST {st} {gi} {name}");
                }
            }
            return;
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
            let Some(pf) = fr.placed.get(name) else {
                continue;
            };
            let deco = pop
                .wrapping_add(*gi as u64)
                .wrapping_add(10_000u64.wrapping_mul(step as u64));
            CUR_FEAT.store(
                ((*gi as i64) << 32) | step as i64,
                std::sync::atomic::Ordering::Relaxed,
            );
            crate::generation_math::XTRACE_FEAT.store(
                ((*gi as i64) << 32) | step as i64,
                std::sync::atomic::Ordering::Relaxed,
            );
            // feature placement sees a `WorldgenRandom` (legacy draw semantics)
            let mut rng = Xor::seed_worldgen(deco);
            if let Ok(spec) = std::env::var("FSEED_ARM") {
                if spec == format!("{sx},{sz},{gi},{step}") {
                    crate::generation_math::XTRACE.store(0, std::sync::atomic::Ordering::Relaxed);
                    crate::generation_math::NTRACE.store(0, std::sync::atomic::Ordering::Relaxed);
                    crate::generation_math::FTRACE.store(0, std::sync::atomic::Ordering::Relaxed);
                    crate::generation_math::DTRACE.store(0, std::sync::atomic::Ordering::Relaxed);
                }
            }
            if let Ok(spec) = std::env::var("FSEED_TRACE") {
                if let Some((a, b)) = spec.split_once(',') {
                    if a.parse::<i32>() == Ok(sx) && b.parse::<i32>() == Ok(sz) {
                        eprintln!("FSEED {pop} {gi} {step}");
                    }
                }
            }
            let before = ctx.wr.len();
            let t0 = if profile {
                Some(std::time::Instant::now())
            } else {
                None
            };
            gen_pf(pf, fr, &mut ctx, &mut rng, (sx * 16, 0, sz * 16));
            if let Some(t0) = t0 {
                feat_prof_record(name, t0.elapsed().as_nanos());
            }
            if trace {
                let mut h: u64 = 0;
                for (k, v) in ctx.wr.iter() {
                    h ^= (k.0 as u64)
                        .wrapping_mul(31)
                        .wrapping_add(k.1 as u64)
                        .rotate_left(7)
                        ^ (k.2 as u64)
                        ^ (*v as u64).wrapping_mul(131);
                }
                eprintln!(
                    "TRACE step={} feat={} +{} writes h={:016x}",
                    step,
                    name,
                    ctx.wr.len() - before,
                    h
                );
            }
        }
    }
    for ((x, y, z), st) in std::mem::take(&mut ctx.wr) {
        g.feat_write(x, y, z, st);
    }
}
