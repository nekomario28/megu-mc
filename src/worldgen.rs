// worldgen.rs — datapack-driven vanilla worldgen slice for mc-spine.
// Loads vanilla worldgen JSONs (noise_settings / density_function / noise)
// from datapacks/<pack>/data/minecraft/worldgen/ and evaluates the vanilla
// density-function pipeline: per-chunk cell grid + trilinear interp.
// Algorithm referenced from vanilla/Pumpkin; implementation is independent.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use crate::json::{J, jbool, jparse};

use crate::generation_math::*;

// ---------- density functions ----------

enum SV {
    V(f64),
    S(Box<Spl>),
}
struct Spl {
    coord: usize,
    pts: Vec<(f64, SV, f64)>,
}

enum DF {
    C(f64),
    Add(usize, usize),
    Sub(usize, usize),
    Mul(usize, usize),
    Min(usize, usize),
    Max(usize, usize),
    Div(usize, usize),
    Neg(usize),
    Abs(usize),
    Sq(usize),
    Cube(usize),
    HNeg(usize),
    QNeg(usize),
    Squeeze(usize),
    Clamp {
        i: usize,
        lo: f64,
        hi: f64,
    },
    Lerp {
        a: usize,
        f: usize,
        s: usize,
    },
    Grad {
        fc: f64,
        fv: f64,
        tc: f64,
        tv: f64,
    }, // axis always y here
    Cache(usize),
    // vanilla `interpolated`: sampled at 4x8x4 cell corners, trilinearly
    // interpolated at the query position (a per-chunk grid, see ChunkCols).
    Interp(usize),
    Noise {
        ni: usize,
        sx: usize,
        sy: usize,
        sz: usize,
        xzs: f64,
        ys: f64,
    },
    ShiftA(usize),
    ShiftB(usize),
    RangeChoice {
        i: usize,
        mi: f64,
        ma: f64,
        wr: usize,
        wo: usize,
    },
    IntervalSelect {
        i: usize,
        th: Vec<f64>,
        fns: Vec<usize>,
    },
    Spline(Box<Spl>),
    OldBlended {
        min: SmearStack,
        max: SmearStack,
        main: SmearStack,
        xzm: f64,
        ym: f64,
        mxm: f64,
        mym: f64,
    },
    BlendD(usize),
    Zero,
    FindTopSurface {
        density: usize,
        lower: i32,
        upper: usize,
        cell_h: i32,
    },
}

// ---------- vanilla surface (material_rule) ----------

#[derive(Clone, Copy)]
enum YA {
    Abs(i32),
    AB(i32),
    BT(i32),
}
impl YA {
    fn get(&self, min_y: i32, top: i32) -> i32 {
        match *self {
            YA::Abs(v) => v,
            YA::AB(v) => min_y + v,
            YA::BT(v) => top - v,
        }
    }
}

#[derive(Clone, Copy)]
enum Cond {
    False,
    Biome(u64), // bitmask over biome leaf ids (<64 distinct names)
    NoiseTh {
        ni: usize,
        is3d: bool,
        lo: f64,
        hi: f64,
    },
    VGrad {
        lo_w: u64,
        hi_w: u64,
        t: YA,
        f: YA,
    },
    YAbove {
        a: YA,
        mult: i32,
        sd: bool,
    },
    Water {
        off: i32,
        mult: i32,
        sd: bool,
    },
    Steep,
    Not(usize),
    Hole,
    Prelim,
    StoneD {
        off: i32,
        sdep: bool,
        sec: i32,
        ceil: bool,
    },
}

enum Rule {
    Seq(Box<[usize]>),
    Cond(usize, usize),
    Block(u32),
    Bandlands,
    OreVein {
        density: usize,
        rich: usize,
        gap: usize,
        ore: u32,
        raw: u32,
        fill: u32,
        rawc: f32,
        ymin: i32,
        ymax: i32,
    },
}

// per-column surface-rule result: dense y->state map over [lo..lo+len)
struct SurfMat {
    lo: i16,
    vals: Box<[u16]>, // u16::MAX = no override -> caller uses default
}

// scan context for material rules (vanilla MaterialRuleContext)
struct SC {
    x: i32,
    y: i32,
    z: i32,
    run_depth: i32,
    sec: f32,
    sec_ok: bool,
    steep: Option<bool>,
    sda: i32,
    sdb: i32,
    fluid: i32,
    est_min: i32,
    nt: HashMap<usize, bool>,             // per-column memo for 2d noise_threshold
    memo: HashMap<(usize, i32, i32, i32), f64>,
}
impl SC {
    fn new() -> SC {
        SC {
            x: 0, y: 0, z: 0, run_depth: 0, sec: 0.0, sec_ok: false,
            steep: None, sda: 0, sdb: 0, fluid: i32::MIN, est_min: i32::MAX,
            nt: HashMap::new(), memo: HashMap::new(),
        }
    }
}

// bound on the shared quart -> biome cache
const BIO_CAP: usize = 1 << 18;

/// One router entry: either a resource name or an inline density-function object.
fn router_df(g: &mut Gen, files: &HashMap<String, Vec<u8>>, j: &J) -> Option<usize> {
    let name = j.str();
    if !name.is_empty() {
        if let Some(i) = g.resolve_ref(files, name) {
            return Some(i);
        }
    }
    g.node(j, files)
}

/// The three vein functions plus the six block states `OreVeinSampler` uses.
#[derive(Clone, Copy)]
struct VeinCfg {
    toggle: usize,
    ridged: usize,
    gap: usize,
    copper_ore: u32,
    copper_raw: u32,
    copper_fill: u32,
    iron_ore: u32,
    iron_raw: u32,
    iron_fill: u32,
}

pub struct Gen {
    dfs: Vec<DF>,
    names: HashMap<String, usize>,
    noises: Vec<Normal>,
    noise_names: HashMap<String, usize>,
    factory: Split,
    base: String,
    pub final_density: usize,
    // router ids: [temperature, vegetation, continentalness, erosion, depth, ridges]
    climate: Option<[usize; 6]>,
    // vanilla depth = grad(y) + offset(x,z) — (fc,fv,tc,tv, offset_df)
    depth_split: Option<(f64, f64, f64, f64, usize)>,
    // 1.21.1 ore veins: `ore_veins_enabled` plus the router's vein functions
    vein: Option<VeinCfg>,
    // scratch memo for the vein sampler's density evaluations
    memo: HashMap<(usize, i32, i32, i32), f64>,
    btree: Option<BT>,
    leaf_ps: Vec<[[i32; 2]; 7]>,
    biome_names: Vec<String>,
    pub sea: i32,
    pub min_y: i32,
    pub height: i32,
    rules: Vec<Rule>,
    conds: Vec<Cond>,
    rule_ids: HashMap<String, usize>,
    cond_ids: HashMap<String, usize>,
    bleaf: HashMap<String, u16>,
    surface_rule: Option<usize>,
    prelim_df: Option<usize>,
    surf_ni: Option<usize>,
    sec_ni: Option<usize>,
    band_off_ni: Option<usize>,
    bands: Option<Box<[u32; 192]>>,
    ore_split: Option<Split>,
    pub def_st: u32,
    pub def_fl: u32,
    // resolved once from the caller's state lookup; a generation module must not
    // reach the crate root for a block state (see tools/layers_check.sh).
    bedrock_st: u32,
    // aquifer (vanilla NoiseBasedAquifer): DF ids + fluid states + caches
    aq: Option<AQD>,
    aq_lv: HashMap<(i32, i32, i32), (i32, u8)>,
    aq_lvo: Vec<(i32, i32, i32)>,
    aq_near: HashMap<(i32, i32, i32), (i32, i32, i32)>,
    aq_neo: Vec<(i32, i32, i32)>,
    aq_skip: HashMap<(i32, i32), i32>,
    aq_sko: Vec<(i32, i32)>,
    prelim_c: HashMap<(i32, i32), i32>,
    /// per chunk: the distinct biomes of its section palettes, i.e. what vanilla's
    /// `applyBiomeDecoration` unions over the 3x3 before deciding which features run
    chunk_biomes: HashMap<(i32, i32), std::sync::Arc<Vec<String>>>,
    chunk_biomes_ord: std::collections::VecDeque<(i32, i32)>,
    prelim_ord: Vec<(i32, i32)>,
    clim_c: HashMap<(i32, i32), ([i64; 5], f64)>,
    clim_ord: Vec<(i32, i32)>,
    // quart -> biome leaf, shared across SCs (the deep material rule asks per block)
    bioq: HashMap<(i32, i32, i32), u16>,
    bioq_ord: std::collections::VecDeque<(i32, i32, i32)>,
    col_c: HashMap<(i32, i32), std::sync::Arc<ChunkCols>>,
    col_ord: Vec<(i32, i32)>,
    // feature layer: chunk -> {(y-min)<<8|z<<4|x -> state}
    feat_c: HashMap<(i32, i32), HashMap<u32, u32>>,
    feat_ord: Vec<(i32, i32)>,
    deep_c: HashMap<(i32, i32, i32), u32>,
    deep_ord: std::collections::VecDeque<(i32, i32, i32)>,
    pop_done: HashSet<(i32, i32)>,
    /// Decoration region whose lexicographic scan order to follow (`MC_SPINE_POP_REGION`).
    pop_region: Option<(i32, i32, i32, i32)>,
    // per column: (highest non-air + 1, highest solid + 1) written by the feature layer
    feat_hm: HashMap<(i32, i32), (i32, i32)>,
    world_seed: u64,
}
struct AQD {
    barrier: usize,
    flood: usize,
    spread: usize,
    lava: usize,
    excl: usize,
    split: Split,
    water: u32,
    lava_st: u32,
}

/// `AQ_TRACE="<cx>,<cz>"` prints the aquifer's decision terms for that chunk.
fn aq_trace_at(x: i32, z: i32) -> bool {
    use std::sync::OnceLock;
    static SPEC: OnceLock<Option<(i32, i32)>> = OnceLock::new();
    let spec = SPEC.get_or_init(|| {
        std::env::var("AQ_TRACE").ok().and_then(|v| {
            let (a, b) = v.split_once(',')?;
            Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
        })
    });
    matches!(spec, Some((cx, cz)) if *cx == x >> 4 && *cz == z >> 4)
}

/// Diagnostics: `WG_COUNT=1` prints how many density-function nodes one chunk
/// walks. Measured 2026-10-22: ~1.6M calls per chunk, which is where the
/// per-chunk time goes (see evidence/foundation/performance/README.md).
pub(crate) static EVAL_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl Gen {
    fn df_files() -> HashMap<String, Vec<u8>> {
        Self::dp_files("density_function")
    }
    // datapacks/*/data/<ns>/worldgen/<subdir>/**.json -> "ns:relpath" (no .json)
    /// The packs under `datapacks/` in *precedence* order: `vanilla` first (the
    /// lowest priority, like the built-in data a datapack overrides), then the rest by
    /// name so the order is deterministic. Iterating `read_dir` instead made
    /// precedence arbitrary - a control pack meant to isolate one feature could be
    /// silently ignored (measured: a ctl pack that zeroed every ore except
    /// `ore_gravel` was ignored, and the port still ran 241 ore placements where the
    /// server ran 126).
    pub(crate) fn packs() -> Vec<std::path::PathBuf> {
        let base = std::path::Path::new("datapacks");
        let mut dirs: Vec<std::path::PathBuf> = std::fs::read_dir(base)
            .map(|rd| rd.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        dirs.sort_by_key(|p| {
            let name = p.file_name().unwrap_or_default().to_string_lossy().to_string();
            (name != "vanilla", name)
        });
        dirs
    }

    pub(crate) fn dp_files(subdir: &str) -> HashMap<String, Vec<u8>> {
        let mut m = HashMap::new();
        {
            for p in Gen::packs() {
                let root = p.join("data");
                if let Ok(nss) = std::fs::read_dir(&root) {
                    for ns in nss.flatten() {
                        let dfr = ns.path().join(format!("worldgen/{}", subdir));
                        let mut stack = vec![dfr.clone()];
                        while let Some(d) = stack.pop() {
                            if let Ok(rd) = std::fs::read_dir(&d) {
                                for e in rd.flatten() {
                                    let p = e.path();
                                    if p.is_dir() {
                                        stack.push(p);
                                    } else if p.extension().map(|e| e == "json").unwrap_or(false) {
                                        if let Ok(rel) = p.strip_prefix(&dfr) {
                                            let key = format!(
                                                "{}:{}",
                                                ns.file_name().to_string_lossy(),
                                                rel.with_extension("").to_string_lossy()
                                            );
                                            if let Ok(b) = std::fs::read(&p) {
                                                // later packs override earlier ones, and
                                                // `packs()` puts `vanilla` first (lowest
                                                // priority). `or_insert` made the first
                                                // pack win, so a control pack could never
                                                // override the bundled data.
                                                m.insert(key, b);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        m
    }

    pub fn load(seed: u64, res: fn(&str) -> Option<u32>) -> Option<Gen> {
        let files = Self::df_files();
        let mut g = Gen {
            dfs: vec![],
            names: HashMap::new(),
            noises: vec![],
            noise_names: HashMap::new(),
            factory: Xor::seed(seed).splitter(),
            base: String::new(),
            final_density: 0,
            climate: None,
            depth_split: None,
            btree: None,
            leaf_ps: Vec::new(),
            biome_names: Vec::new(),
            sea: 63,
            min_y: -64,
            height: 384,
            rules: vec![],
            conds: vec![],
            rule_ids: HashMap::new(),
            cond_ids: HashMap::new(),
            bleaf: HashMap::new(),
            surface_rule: None,
            prelim_df: None,
            surf_ni: None,
            sec_ni: None,
            band_off_ni: None,
            bands: None,
            ore_split: None,
            def_st: 1,
            def_fl: 0,
            bedrock_st: res("minecraft:bedrock").unwrap_or(1),
            aq: None,
            aq_lv: HashMap::new(),
            aq_lvo: Vec::new(),
            aq_near: HashMap::new(),
            aq_neo: Vec::new(),
            aq_skip: HashMap::new(),
            aq_sko: Vec::new(),
            prelim_c: HashMap::new(),
            chunk_biomes: HashMap::new(),
            chunk_biomes_ord: std::collections::VecDeque::new(),
            prelim_ord: Vec::new(),
            clim_c: HashMap::new(),
            clim_ord: Vec::new(),
            bioq: HashMap::new(),
            bioq_ord: std::collections::VecDeque::new(),
            col_c: HashMap::new(),
            col_ord: Vec::new(),
            feat_c: HashMap::new(),
            feat_ord: Vec::new(),
            deep_c: HashMap::new(),
            deep_ord: std::collections::VecDeque::new(),
            pop_done: HashSet::new(),
            pop_region: std::env::var("MC_SPINE_POP_REGION").ok().and_then(|v| {
                let p: Vec<i32> = v.split(',').filter_map(|n| n.parse().ok()).collect();
                if p.len() == 4 {
                    Some((p[0], p[1], p[2], p[3]))
                } else {
                    None
                }
            }),
            vein: None,
            memo: HashMap::new(),
            feat_hm: HashMap::new(),
            world_seed: seed,
        };
        // noise settings (sea level, bounds, router names)
        let mut ns_json: Option<J> = None;
        'outer: for p in Gen::packs() {
            let f = p.join("data/minecraft/worldgen/noise_settings/overworld.json");
            if let Ok(b) = std::fs::read(&f) {
                ns_json = jparse(&b);
                g.base = p.to_string_lossy().to_string();
                break 'outer;
            }
        }
        let ns = ns_json?;
        g.sea = ns.get("sea_level").map(J::num).unwrap_or(63.0) as i32;
        if let Some(n) = ns.get("noise") {
            g.min_y = n.get("min_y").map(J::num).unwrap_or(-64.0) as i32;
            g.height = n.get("height").map(J::num).unwrap_or(384.0) as i32;
        }
        let router = ns.get("noise_router");
        let fd = router
            .and_then(|r| r.get("final_density"))
            .map(|j| j.str().to_string())?;
        let idx = g.resolve_ref(&files, &fd)?;
        g.final_density = idx;
        // 1.21.1 generates ore veins in the noise chunk (`ore_veins_enabled`)
        // from three router functions; 1.21.2+ moved them into a material rule.
        if jbool(&ns, "ore_veins_enabled", false) {
            let get = |k: &str| router.and_then(|r| r.get(k)).cloned();
            if let (Some(a), Some(b), Some(c)) =
                (get("vein_toggle"), get("vein_ridged"), get("vein_gap"))
            {
                // the vein functions are *inline* objects in 1.21.1, not names
                let (ta, tb, tc) = (
                    router_df(&mut g, &files, &a),
                    router_df(&mut g, &files, &b),
                    router_df(&mut g, &files, &c),
                );
                if std::env::var("MC_SPINE_VEINDBG").is_ok() {
                    eprintln!("VEINCFG refs a={:?} b={:?} c={:?}", ta, tb, tc);
                }
                if let (Some(ta), Some(tb), Some(tc)) = (ta, tb, tc) {
                    let st = |n: &str| crate::name_to_state(n).unwrap_or(0);
                    g.vein = Some(VeinCfg {
                        toggle: ta,
                        ridged: tb,
                        gap: tc,
                        copper_ore: st("minecraft:copper_ore"),
                        copper_raw: st("minecraft:raw_copper_block"),
                        copper_fill: st("minecraft:granite"),
                        iron_ore: st("minecraft:deepslate_iron_ore"),
                        iron_raw: st("minecraft:raw_iron_block"),
                        iron_fill: st("minecraft:tuff"),
                    });
                }
            }
        }
        // climate router ids for biome multi-noise sampling
        {
            let keys = [
                "temperature",
                "vegetation",
                "continents",
                "erosion",
                "depth",
                "ridges",
            ];
            let mut ids = [0usize; 6];
            let mut ok = true;
            for (i, k) in keys.iter().enumerate() {
                match router
                    .and_then(|r| r.get(k))
                    .map(|j| j.str().to_string())
                    .and_then(|n| g.resolve_ref(&files, &n))
                {
                    Some(v) => ids[i] = v,
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
            if ok {
                g.climate = Some(ids);
                // vanilla depth = grad(y) + offset(x,z): split it so the
                // expensive offset term can be cached per column.
                if let DF::Add(a, b) = g.dfs[ids[4]] {
                    let (gi, oi) = if matches!(g.dfs[a], DF::Grad { .. }) {
                        (a, b)
                    } else if matches!(g.dfs[b], DF::Grad { .. }) {
                        (b, a)
                    } else {
                        (usize::MAX, usize::MAX)
                    };
                    if gi != usize::MAX {
                        if let DF::Grad { fc, fv, tc, tv } = g.dfs[gi] {
                            g.depth_split = Some((fc, fv, tc, tv, oi));
                        }
                    }
                }
            }
        }
        // serialized overworld biome tree (pumpkin-derived, vanilla-equivalent)
        {
            let tp = std::path::Path::new(&g.base).join("multi_noise_biome_tree.json");
            if let Ok(b) = std::fs::read(&tp) {
                if let Some(j) = jparse(&b) {
                    if let Some(ow) = j.get("overworld") {
                        g.btree =
                            bt_parse(ow, &mut g.biome_names, &mut g.bleaf, &mut g.leaf_ps);
                    }
                }
            }
        }
        // vanilla surface rules (material_rule) — needs biome tree loaded first
        // (biome conditions resolve to leaf ids).
        {
            let rfiles = Self::dp_files("material_rule");
            let cfiles = Self::dp_files("material_condition");
            g.surf_ni = g.noise_id("minecraft:surface");
            g.sec_ni = g.noise_id("minecraft:surface_secondary");
            g.band_off_ni = g.noise_id("minecraft:clay_bands_offset");
            g.ore_split = Some(g.factory.of_str("minecraft:ore").splitter());
            g.def_st = ns
                .get("default_block")
                .and_then(|b| res(b.str()))
                .unwrap_or(1);
            g.def_fl = ns
                .get("default_fluid")
                .and_then(|b| res(b.str()))
                .unwrap_or(0);
            g.bands = create_bands(&mut g.factory.of_str("minecraft:clay_bands"), res);
            // The aquifer's `minEst` column comes from `aquifers.surface_level`
            // (`minecraft:overworld/preliminary_surface_level`, the raw
            // find_top_surface with cell_height 8). The noise router also carries
            // a `chunk_surface_level`, but that is `interpolated(cell_size_xz=16)`
            // wrapped around the same function - a different number, and not what
            // NoiseBasedAquifer reads. Prefer the aquifers entry; keep the router
            // key as a fallback for datapacks that only publish that one.
            if let Some(aq) = ns
                .get("aquifers")
                .and_then(|a| a.get("surface_level"))
                .or_else(|| router.and_then(|r| r.get("chunk_surface_level")))
                .map(|v| v.str().to_string())
            {
                g.prelim_df = g.resolve_ref(&files, &aq);
            }
            if let Some(mr) = ns.get("material_rule").map(|v| v.str().to_string()) {
                g.surface_rule = g.resolve_rule(&mr, &files, &rfiles, &cfiles, res);
            }
        }
        // aquifer config: noise DF ids + positional deriver (minecraft:aquifer)
        let noaq = std::env::var("MC_SPINE_NOAQ").is_ok() || !jbool(&ns, "aquifers_enabled", true);
        if let Some(aq) = ns.get("aquifers").filter(|_| !noaq) {
            let get = |g: &mut Gen, k: &str| aq.get(k).and_then(|j| g.node(j, &files));
            let (b, f, s, l, e) = (
                get(&mut g, "barrier"),
                get(&mut g, "fluid_level_floodedness"),
                get(&mut g, "fluid_level_spread"),
                get(&mut g, "lava"),
                get(&mut g, "exclusion"),
            );
            if let (Some(barrier), Some(flood), Some(spread), Some(lava), Some(excl)) =
                (b, f, s, l, e)
            {
                g.aq = Some(AQD {
                    barrier,
                    flood,
                    spread,
                    lava,
                    excl,
                    split: g.factory.of_str("minecraft:aquifer").splitter(),
                    water: res("minecraft:water").unwrap_or(80),
                    lava_st: res("minecraft:lava").unwrap_or(1),
                });
            }
        }
        Some(g)
    }

    // ---- material rule parsing ----

    fn resolve_rule(
        &mut self,
        name: &str,
        files: &HashMap<String, Vec<u8>>,
        rf: &HashMap<String, Vec<u8>>,
        cf: &HashMap<String, Vec<u8>>,
        res: fn(&str) -> Option<u32>,
    ) -> Option<usize> {
        if let Some(&i) = self.rule_ids.get(name) {
            return Some(i);
        }
        let b = rf.get(name)?;
        let j = jparse(b)?;
        let idx = self.rules.len();
        self.rule_ids.insert(name.to_string(), idx);
        self.rules.push(Rule::Block(0)); // placeholder for recursion
        let r = self.parse_rule(&j, files, rf, cf, res)?;
        self.rules[idx] = r;
        Some(idx)
    }

    fn rule_node(
        &mut self,
        j: &J,
        files: &HashMap<String, Vec<u8>>,
        rf: &HashMap<String, Vec<u8>>,
        cf: &HashMap<String, Vec<u8>>,
        res: fn(&str) -> Option<u32>,
    ) -> Option<usize> {
        match j {
            J::Str(s) => self.resolve_rule(s, files, rf, cf, res),
            J::Obj(_) => {
                let r = self.parse_rule(j, files, rf, cf, res)?;
                self.rules.push(r);
                Some(self.rules.len() - 1)
            }
            _ => None,
        }
    }

    fn parse_rule(
        &mut self,
        j: &J,
        files: &HashMap<String, Vec<u8>>,
        rf: &HashMap<String, Vec<u8>>,
        cf: &HashMap<String, Vec<u8>>,
        res: fn(&str) -> Option<u32>,
    ) -> Option<Rule> {
        let ty = j.get("type")?.str().strip_prefix("minecraft:")?.to_string();
        Some(match ty.as_str() {
            "sequence" => Rule::Seq(
                j.get("sequence")?
                    .into_arr()?
                    .iter()
                    .filter_map(|c| self.rule_node(c, files, rf, cf, res))
                    .collect(),
            ),
            "condition" => Rule::Cond(
                self.cond_node(j.get("if_true")?, cf)?,
                self.rule_node(j.get("then_run")?, files, rf, cf, res)?,
            ),
            "block" => Rule::Block(res(j.get("result_state")?.str()).unwrap_or(self.def_st)),
            "bandlands" => Rule::Bandlands,
            "ore_vein" => {
                // 1.21.1 has no `material_rule`; when the 1.21.1 sampler is
                // active (ore_veins_enabled), the 1.21.2+ rule would place a
                // second vein on top of it.
                if self.vein.is_some() {
                    return None;
                }
                let density = self.node(j.get("density")?, files)?;
                // bounds check fast-path: density root is range_choice on minecraft:y
                let (mut ymin, mut ymax) = (i32::MIN, i32::MAX);
                let yid = self.names.get("minecraft:y").copied();
                if let (Some(yid), DF::RangeChoice { i, mi, ma, .. }) =
                    (yid, &self.dfs[density])
                {
                    if *i == yid {
                        ymin = *mi as i32;
                        ymax = *ma as i32;
                    }
                }
                Rule::OreVein {
                    density,
                    rich: self.node(j.get("richness")?, files)?,
                    gap: self.node(j.get("filler_gap")?, files)?,
                    ore: res(j.get("ore_block")?.str()).unwrap_or(self.def_st),
                    raw: res(j.get("raw_ore_block")?.str()).unwrap_or(self.def_st),
                    fill: res(j.get("filler_block")?.str()).unwrap_or(self.def_st),
                    rawc: j.get("raw_ore_chance")?.num() as f32,
                    ymin,
                    ymax,
                }
            }
            _ => return None,
        })
    }

    fn resolve_cond(&mut self, name: &str, cf: &HashMap<String, Vec<u8>>) -> Option<usize> {
        // `minecraft:not_underwater` is a built-in ConditionSource, not a datapack
        // file. Its condition (`SurfaceRules$WaterConditionSource$1WaterCondition`)
        // is already "not underwater": waterHeight == MIN, or blockY + stoneDepth
        // above the water surface. Negating it put grass on submerged floors.
        if name == "minecraft:not_underwater" || name == "not_underwater" {
            self.conds.push(Cond::Water {
                off: 0,
                mult: 0,
                sd: false,
            });
            return Some(self.conds.len() - 1);
        }
        if let Some(&i) = self.cond_ids.get(name) {
            return Some(i);
        }
        let b = cf.get(name)?;
        let j = jparse(b)?;
        let idx = self.conds.len();
        self.cond_ids.insert(name.to_string(), idx);
        self.conds.push(Cond::False);
        let c = self.parse_cond(&j, cf)?;
        self.conds[idx] = c;
        Some(idx)
    }

    fn cond_node(&mut self, j: &J, cf: &HashMap<String, Vec<u8>>) -> Option<usize> {
        match j {
            J::Str(s) => self.resolve_cond(s, cf),
            J::Obj(_) => {
                let c = self.parse_cond(j, cf)?;
                self.conds.push(c);
                Some(self.conds.len() - 1)
            }
            _ => None,
        }
    }

    fn parse_cond(&mut self, j: &J, cf: &HashMap<String, Vec<u8>>) -> Option<Cond> {
        let ty = j.get("type")?.str().strip_prefix("minecraft:")?.to_string();
        let n = |k: &str| j.get(k).map(J::num).unwrap_or(0.0);
        let bj = |k: &str| matches!(j.get(k), Some(J::Num(v)) if *v != 0.0);
        let ya = |k: &str| -> Option<YA> {
            let v = j.get(k)?;
            if let Some(x) = v.get("absolute") {
                Some(YA::Abs(x.num() as i32))
            } else if let Some(x) = v.get("above_bottom") {
                Some(YA::AB(x.num() as i32))
            } else {
                v.get("below_top").map(|x| YA::BT(x.num() as i32))
            }
        };
        Some(match ty.as_str() {
            "biome" => {
                let mut mask = 0u64;
                let mut one = |s: &str| {
                    if let Some(&l) = self.bleaf.get(s) {
                        mask |= 1u64 << l;
                    }
                };
                match j.get("biome_is")? {
                    J::Str(s) => one(s),
                    J::Arr(a) => {
                        for e in a {
                            one(e.str());
                        }
                    }
                    _ => {}
                }
                if mask == 0 {
                    Cond::False
                } else {
                    Cond::Biome(mask)
                }
            }
            "noise_threshold" => Cond::NoiseTh {
                ni: self.noise_id(j.get("noise")?.str())?,
                is3d: bj("is_3d"),
                lo: n("min_threshold"),
                hi: n("max_threshold"),
            },
            "vertical_gradient" => {
                // Guava Longs.fromBytes (vanilla StringHash.fromString) is
                // big-endian; `Split::of_str` already reads it that way.
                let h = md5(j.get("random_name")?.str().as_bytes());
                Cond::VGrad {
                    lo_w: u64::from_be_bytes(h[0..8].try_into().unwrap()),
                    hi_w: u64::from_be_bytes(h[8..16].try_into().unwrap()),
                    t: ya("true_at_and_below")?,
                    f: ya("false_at_and_above")?,
                }
            }
            "y_above" => Cond::YAbove {
                a: ya("anchor")?,
                mult: n("surface_depth_multiplier") as i32,
                sd: bj("add_stone_depth"),
            },
            "water" => Cond::Water {
                off: n("offset") as i32,
                mult: n("surface_depth_multiplier") as i32,
                sd: bj("add_stone_depth"),
            },
            "stone_depth" => Cond::StoneD {
                off: n("offset") as i32,
                sdep: bj("add_surface_depth"),
                sec: n("secondary_depth_range") as i32,
                ceil: j.get("surface_type").map(J::str) == Some("ceiling"),
            },
            // The condition is already "not underwater" (see resolve_cond).
            "not_underwater" => Cond::Water {
                off: 0,
                mult: 0,
                sd: false,
            },
            "not" => Cond::Not(self.cond_node(j.get("invert")?, cf)?),
            "hole" => Cond::Hole,
            "above_preliminary_surface" => Cond::Prelim,
            "steep" => Cond::Steep,
            _ => Cond::False,
        })
    }

    fn resolve_ref(&mut self, files: &HashMap<String, Vec<u8>>, name: &str) -> Option<usize> {
        if let Some(&i) = self.names.get(name) {
            return Some(i);
        }
        let b = files.get(name)?;
        let j = jparse(b)?;
        let idx = self.dfs.len();
        self.names.insert(name.to_string(), idx);
        self.dfs.push(DF::Zero); // placeholder for recursion safety
        let d = self.parse_df(&j, files)?;
        self.dfs[idx] = d;
        Some(idx)
    }

    fn node(&mut self, j: &J, files: &HashMap<String, Vec<u8>>) -> Option<usize> {
        match j {
            J::Num(n) => {
                self.dfs.push(DF::C(*n));
                Some(self.dfs.len() - 1)
            }
            J::Str(s) => self.resolve_ref(files, s),
            J::Obj(_) => {
                let d = self.parse_df(j, files)?;
                self.dfs.push(d);
                Some(self.dfs.len() - 1)
            }
            _ => None,
        }
    }

    fn noise_id(&mut self, name: &str) -> Option<usize> {
        if let Some(&i) = self.noise_names.get(name) {
            return Some(i);
        }
        let path = format!(
            "{}/data/minecraft/worldgen/noise/{}.json",
            self.base,
            name.rsplit(':').next().unwrap_or(name)
        );
        let b = std::fs::read(&path).ok()?;
        let j = jparse(&b)?;
        let base_octave = j.get("base_octave")?.num() as i32;
        let base_amplitude = j.get("base_amplitude")?.num();
        // absent modifiers = single implicit octave (vanilla amplitudes [1.0])
        let mods: Vec<f64> = match j.get("amplitude_modifiers") {
            Some(J::Arr(a)) => a.iter().map(J::num).collect(),
            _ => vec![1.0; j.get("octave_count").map(J::num).unwrap_or(1.0).max(1.0) as usize],
        };
        let mut rng = self.factory.of_str(name);
        let n = normal_new(&mut rng, base_octave, base_amplitude, &mods);
        self.noises.push(n);
        self.noise_names.insert(name.to_string(), self.noises.len() - 1);
        Some(self.noises.len() - 1)
    }

    fn parse_spline(&mut self, j: &J, files: &HashMap<String, Vec<u8>>) -> Option<Spl> {
        let coord = self.node(j.get("coordinate")?, files)?;
        let mut pts = Vec::new();
        for p in j.get("points")?.into_arr()? {
            let loc = p.get("location")?.num();
            let der = p.get("derivative").map(J::num).unwrap_or(0.0);
            let val = match p.get("value")? {
                J::Num(n) => SV::V(*n),
                x @ J::Obj(_) => SV::S(Box::new(self.parse_spline(x, files)?)),
                _ => return None,
            };
            pts.push((loc, val, der));
        }
        Some(Spl { coord, pts })
    }

    fn parse_df(&mut self, j: &J, files: &HashMap<String, Vec<u8>>) -> Option<DF> {
        let ty = j.get("type")?.str().strip_prefix("minecraft:")?.to_string();
        let n = |k: &str| j.get(k).map(J::num).unwrap_or(0.0);
        let sub = |g: &mut Gen, k: &str| -> Option<usize> { g.node(j.get(k)?, files) };
        Some(match ty.as_str() {
            "add" => DF::Add(sub(self, "left")?, sub(self, "right")?),
            "sub" => DF::Sub(sub(self, "left")?, sub(self, "right")?),
            "mul" => DF::Mul(sub(self, "left")?, sub(self, "right")?),
            "min" => DF::Min(sub(self, "left")?, sub(self, "right")?),
            "max" => DF::Max(sub(self, "left")?, sub(self, "right")?),
            "div" => DF::Div(sub(self, "left")?, sub(self, "right")?),
            "negate" => DF::Neg(sub(self, "input")?),
            "abs" => DF::Abs(sub(self, "input")?),
            "square" => DF::Sq(sub(self, "input")?),
            "cube" => DF::Cube(sub(self, "input")?),
            "half_negative" => DF::HNeg(sub(self, "input")?),
            "quarter_negative" => DF::QNeg(sub(self, "input")?),
            "squeeze" => DF::Squeeze(sub(self, "input")?),
            "clamp" => DF::Clamp {
                i: sub(self, "input")?,
                lo: n("min"),
                hi: n("max"),
            },
            "lerp" => DF::Lerp {
                a: sub(self, "alpha")?,
                f: sub(self, "first")?,
                s: sub(self, "second")?,
            },
            "gradient" => DF::Grad {
                fc: n("from_coordinate"),
                fv: n("from_value"),
                tc: n("to_coordinate"),
                tv: n("to_value"),
            },
            "interpolated" => DF::Interp(sub(self, "input")?),
            "cache" | "flat_cache" | "cache_2d" | "cache_once" | "cache_all_in_cell" => {
                DF::Cache(sub(self, "input")?)
            }
            "noise" => {
                let zero = {
                    self.dfs.push(DF::C(0.0));
                    self.dfs.len() - 1
                };
                DF::Noise {
                    ni: self.noise_id(j.get("noise")?.str())?,
                    sx: j
                        .get("shift_x")
                        .and_then(|v| self.node(v, files))
                        .unwrap_or(zero),
                    sy: j
                        .get("shift_y")
                        .and_then(|v| self.node(v, files))
                        .unwrap_or(zero),
                    sz: j
                        .get("shift_z")
                        .and_then(|v| self.node(v, files))
                        .unwrap_or(zero),
                    xzs: n("xz_scale"),
                    ys: n("y_scale"),
                }
            }
            "shift_a" => DF::ShiftA(self.noise_id(j.get("noise")?.str())?),
            "shift_b" => DF::ShiftB(self.noise_id(j.get("noise")?.str())?),
            "range_choice" => DF::RangeChoice {
                i: sub(self, "input")?,
                mi: n("min_inclusive"),
                ma: n("max_exclusive"),
                wr: sub(self, "when_in_range")?,
                wo: sub(self, "when_out_of_range")?,
            },
            "interval_select" => {
                let mut th = vec![];
                let mut fns = vec![];
                for f in j.get("functions")?.into_arr()? {
                    fns.push(self.node(f, files)?);
                }
                for t in j.get("thresholds")?.into_arr()? {
                    th.push(t.num());
                }
                DF::IntervalSelect {
                    i: sub(self, "input")?,
                    th,
                    fns,
                }
            }
            "spline" => DF::Spline(Box::new(self.parse_spline(j.get("spline")?, files)?)),
            "old_blended_noise" => {
                let xzs = n("xz_scale");
                let ys = n("y_scale");
                let xzf = n("xz_factor");
                let yf = n("y_factor");
                let smear = n("smear_scale_multiplier");
                let xzm = 684.412 * xzs;
                let ym = 684.412 * ys;
                let lim_y = ym * smear;
                let mut rng = self.factory.of_str("minecraft:terrain");
                DF::OldBlended {
                    min: smear_fbm(&mut rng, -15, lim_y, 0.99998474),
                    max: smear_fbm(&mut rng, -15, lim_y, 0.99998474),
                    main: smear_fbm(&mut rng, -7, lim_y / yf, 12.75),
                    xzm,
                    ym,
                    mxm: xzm / xzf,
                    mym: ym / yf,
                }
            }
            "blend_density" => DF::BlendD(sub(self, "input")?),
            "blend_alpha" => DF::C(1.0),
            "blend_offset" | "beardifier" | "end_outer_islands" | "end_islands"
            | "distance_to_point" | "slice" => DF::Zero,
            "find_top_surface" => DF::FindTopSurface {
                density: sub(self, "density")?,
                lower: n("lower_bound") as i32,
                upper: sub(self, "upper_bound")?,
                cell_h: n("cell_height") as i32,
            },
            _ => DF::Zero,
        })
    }

    pub(crate) fn eval(&self, id: usize, x: i32, y: i32, z: i32, memo: &mut HashMap<(usize, i32, i32, i32), f64>) -> f64 {
        self.eval_c(id, x, y, z, memo, None)
    }

    /// Evaluate a density function. `cc` supplies the per-chunk interpolation
    /// grids: with it, an `interpolated` node returns the trilinear value of its
    /// grid instead of evaluating its input at block resolution (vanilla
    /// NoiseChunk). While the grids are being filled `cc` is None, which is a
    /// corner sample.
    fn eval_c(&self, id: usize, x: i32, y: i32, z: i32, memo: &mut HashMap<(usize, i32, i32, i32), f64>, cc: Option<&ChunkCols>) -> f64 {
        {
            static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            if *ON.get_or_init(|| std::env::var_os("WG_COUNT").is_some()) {
                EVAL_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
        if let Some(cc) = cc {
            if let DF::Interp(inner) = self.dfs[id] {
                if let Some(v) = cc.interp(id, x, y, z) {
                    return v;
                }
                return self.eval_c(inner, x, y, z, memo, Some(cc));
            }
        }
        if let DF::Cache(_) = self.dfs[id] {
            if let Some(&v) = memo.get(&(id, x, y, z)) {
                return v;
            }
        }
        let v = self.eval0_c(id, x, y, z, memo, cc);
        if let DF::Cache(_) = self.dfs[id] {
            memo.insert((id, x, y, z), v);
        }
        v
    }

    /// Terrain density at a block position, with `interpolated` resolved from the
    /// chunk's corner grids.
    pub(crate) fn block_density(&self, cc: &ChunkCols, x: i32, y: i32, z: i32) -> f64 {
        let mut memo = HashMap::new();
        self.eval_c(self.final_density, x, y, z, &mut memo, Some(cc))
    }

    /// Density-function ids a node reads, for the grid reachability walk.
    fn push_kids(&self, id: usize, out: &mut Vec<usize>) {
        fn spl_kids(s: &Spl, out: &mut Vec<usize>) {
            out.push(s.coord);
            for (_, v, _) in &s.pts {
                if let SV::S(inner) = v {
                    spl_kids(inner, out);
                }
            }
        }
        match &self.dfs[id] {
            DF::C(_)
            | DF::Zero
            | DF::Grad { .. }
            | DF::ShiftA(_)
            | DF::ShiftB(_)
            | DF::OldBlended { .. } => {}
            DF::Add(a, b)
            | DF::Sub(a, b)
            | DF::Mul(a, b)
            | DF::Min(a, b)
            | DF::Max(a, b)
            | DF::Div(a, b) => {
                out.push(*a);
                out.push(*b);
            }
            DF::Neg(a)
            | DF::Abs(a)
            | DF::Sq(a)
            | DF::Cube(a)
            | DF::HNeg(a)
            | DF::QNeg(a)
            | DF::Squeeze(a)
            | DF::Cache(a)
            | DF::Interp(a)
            | DF::BlendD(a) => out.push(*a),
            DF::Clamp { i, .. } => out.push(*i),
            DF::Lerp { a, f, s } => {
                out.push(*a);
                out.push(*f);
                out.push(*s);
            }
            DF::Noise { sx, sy, sz, .. } => {
                out.push(*sx);
                out.push(*sy);
                out.push(*sz);
            }
            DF::RangeChoice { i, wr, wo, .. } => {
                out.push(*i);
                out.push(*wr);
                out.push(*wo);
            }
            DF::IntervalSelect { i, fns, .. } => {
                out.push(*i);
                out.extend(fns.iter().copied());
            }
            DF::Spline(s) => spl_kids(s, out),
            DF::FindTopSurface { density, upper, .. } => {
                out.push(*density);
                out.push(*upper);
            }
        }
    }

    /// `interpolated` nodes reachable from final_density.
    fn interp_ids(&self) -> Vec<usize> {
        let mut seen = vec![false; self.dfs.len()];
        let mut out = Vec::new();
        let mut kids = Vec::new();
        let mut roots = vec![self.final_density];
        if let Some(v) = self.vein {
            roots.extend([v.toggle, v.ridged, v.gap]);
        }
        let mut stack = roots;
        while let Some(id) = stack.pop() {
            if seen[id] {
                continue;
            }
            seen[id] = true;
            if matches!(self.dfs[id], DF::Interp(_)) {
                out.push(id);
            }
            kids.clear();
            self.push_kids(id, &mut kids);
            for &k in &kids {
                if !seen[k] {
                    stack.push(k);
                }
            }
        }
        out
    }

    fn eval_spline_c(&self, s: &Spl, x: i32, y: i32, z: i32, memo: &mut HashMap<(usize, i32, i32, i32), f64>, cc: Option<&ChunkCols>) -> f64 {
        let loc = self.eval_c(s.coord, x, y, z, memo, cc);
        let n = s.pts.len();
        if n == 0 {
            return 0.0;
        }
        let mut sv = |sv: &SV| -> f64 {
            match sv {
                SV::V(v) => *v,
                SV::S(sp) => self.eval_spline_c(sp, x, y, z, memo, cc),
            }
        };
        let idx = s.pts.partition_point(|p| loc >= p.0);
        if idx == 0 {
            let p = &s.pts[0];
            return sv(&p.1) + p.2 * (loc - p.0);
        }
        if idx == n {
            let p = &s.pts[n - 1];
            return sv(&p.1) + p.2 * (loc - p.0);
        }
        let (x0, v0, d0) = (&s.pts[idx - 1].0, sv(&s.pts[idx - 1].1), s.pts[idx - 1].2);
        let (x1, v1, d1) = (&s.pts[idx].0, sv(&s.pts[idx].1), s.pts[idx].2);
        let t = (loc - x0) / (x1 - x0);
        let h00 = (1.0 + 2.0 * t) * (1.0 - t) * (1.0 - t);
        let h10 = t * (1.0 - t) * (1.0 - t);
        let h01 = t * t * (3.0 - 2.0 * t);
        let h11 = t * t * (t - 1.0);
        h00 * v0 + h10 * d0 * (x1 - x0) + h01 * v1 + h11 * d1 * (x1 - x0)
    }

    fn eval0_c(&self, id: usize, x: i32, y: i32, z: i32, memo: &mut HashMap<(usize, i32, i32, i32), f64>, cc: Option<&ChunkCols>) -> f64 {
        match &self.dfs[id] {
            DF::C(v) => *v,
            DF::Zero => 0.0,
            DF::Add(a, b) => self.eval_c(*a, x, y, z, memo, cc) + self.eval_c(*b, x, y, z, memo, cc),
            DF::Sub(a, b) => self.eval_c(*a, x, y, z, memo, cc) - self.eval_c(*b, x, y, z, memo, cc),
            DF::Mul(a, b) => self.eval_c(*a, x, y, z, memo, cc) * self.eval_c(*b, x, y, z, memo, cc),
            DF::Min(a, b) => self.eval_c(*a, x, y, z, memo, cc).min(self.eval_c(*b, x, y, z, memo, cc)),
            DF::Max(a, b) => self.eval_c(*a, x, y, z, memo, cc).max(self.eval_c(*b, x, y, z, memo, cc)),
            DF::Div(a, b) => {
                let d = self.eval_c(*b, x, y, z, memo, cc);
                if d == 0.0 {
                    0.0
                } else {
                    self.eval_c(*a, x, y, z, memo, cc) / d
                }
            }
            DF::Neg(a) => -self.eval_c(*a, x, y, z, memo, cc),
            DF::Abs(a) => self.eval_c(*a, x, y, z, memo, cc).abs(),
            DF::Sq(a) => {
                let v = self.eval_c(*a, x, y, z, memo, cc);
                v * v
            }
            DF::Cube(a) => {
                let v = self.eval_c(*a, x, y, z, memo, cc);
                v * v * v
            }
            DF::HNeg(a) => {
                let v = self.eval_c(*a, x, y, z, memo, cc);
                if v > 0.0 {
                    v
                } else {
                    v * 0.5
                }
            }
            DF::QNeg(a) => {
                let v = self.eval_c(*a, x, y, z, memo, cc);
                if v > 0.0 {
                    v
                } else {
                    v * 0.25
                }
            }
            DF::Squeeze(a) => {
                let v = self.eval_c(*a, x, y, z, memo, cc).clamp(-1.0, 1.0);
                v / 2.0 - v * v * v / 24.0
            }
            DF::Clamp { i, lo, hi } => self.eval_c(*i, x, y, z, memo, cc).clamp(*lo, *hi),
            DF::Lerp { a, f, s } => {
                let alpha = self.eval_c(*a, x, y, z, memo, cc);
                let fv = self.eval_c(*f, x, y, z, memo, cc);
                let sv = self.eval_c(*s, x, y, z, memo, cc);
                fv + alpha * (sv - fv)
            }
            DF::Grad { fc, fv, tc, tv } => {
                let t = ((y as f64) - fc) / (tc - fc);
                fv + t.clamp(0.0, 1.0) * (tv - fv)
            }
            DF::Cache(a) => self.eval_c(*a, x, y, z, memo, cc),
            DF::Interp(a) => self.eval_c(*a, x, y, z, memo, cc),
            DF::Noise { ni, sx, sy, sz, xzs, ys } => {
                let xs = self.eval_c(*sx, x, y, z, memo, cc);
                let ysv = self.eval_c(*sy, x, y, z, memo, cc);
                let zs = self.eval_c(*sz, x, y, z, memo, cc);
                self.noises[*ni].get(
                    x as f64 * xzs + xs,
                    y as f64 * ys + ysv,
                    z as f64 * xzs + zs,
                )
            }
            DF::ShiftA(ni) => self.noises[*ni].get(x as f64 * 0.25, 0.0, z as f64 * 0.25) * 4.0,
            DF::ShiftB(ni) => self.noises[*ni].get(z as f64 * 0.25, x as f64 * 0.25, 0.0) * 4.0,
            DF::RangeChoice { i, mi, ma, wr, wo } => {
                let v = self.eval_c(*i, x, y, z, memo, cc);
                self.eval_c(if v >= *mi && v < *ma { *wr } else { *wo }, x, y, z, memo, cc)
            }
            DF::IntervalSelect { i, th, fns } => {
                let v = self.eval_c(*i, x, y, z, memo, cc);
                let mut pick = fns.len() - 1;
                for (k, t) in th.iter().enumerate() {
                    if v < *t {
                        pick = k;
                        break;
                    }
                }
                self.eval_c(fns[pick.min(fns.len() - 1)], x, y, z, memo, cc)
            }
            DF::Spline(s) => self.eval_spline_c(s, x, y, z, memo, cc),
            DF::OldBlended { min, max, main, xzm, ym, mxm, mym } => {
                let xv = x as f64;
                let yv = y as f64;
                let zv = z as f64;
                let alpha = (main.get(xv * mxm, yv * mym, zv * mxm) + 0.5).clamp(0.0, 1.0);
                if alpha == 0.0 {
                    min.get(xv * xzm, yv * ym, zv * xzm)
                } else if alpha == 1.0 {
                    max.get(xv * xzm, yv * ym, zv * xzm)
                } else {
                    let a = min.get(xv * xzm, yv * ym, zv * xzm);
                    let b = max.get(xv * xzm, yv * ym, zv * xzm);
                    a + alpha * (b - a)
                }
            }
            DF::BlendD(a) => self.eval_c(*a, x, y, z, memo, cc),
            DF::FindTopSurface { density, lower, upper, cell_h } => {
                let ub = self.eval_c(*upper, x, 0, z, memo, cc) as i32;
                let mut yy = ub.div_euclid(*cell_h) * *cell_h;
                while yy >= *lower {
                    if self.eval_c(*density, x, yy, z, memo, cc) > 0.0 {
                        return yy as f64;
                    }
                    yy -= cell_h;
                }
                *lower as f64
            }
        }
    }
}

// ---------- material rule evaluation ----------

fn create_bands(r: &mut Xor, res: fn(&str) -> Option<u32>) -> Option<Box<[u32; 192]>> {
    let t = res("minecraft:terracotta")?;
    let o = res("minecraft:orange_terracotta")?;
    let y = res("minecraft:yellow_terracotta")?;
    let br = res("minecraft:brown_terracotta")?;
    let rd = res("minecraft:red_terracotta")?;
    let w = res("minecraft:white_terracotta")?;
    let lg = res("minecraft:light_gray_terracotta")?;
    let mut b = [t; 192];
    let mut i = 0i32;
    while i < 192 {
        i += r.next_bound(5) + 1;
        if i >= 192 {
            break;
        }
        b[i as usize] = o;
        i += 1;
    }
    fn add_bands(r: &mut Xor, b: &mut [u32; 192], mn: i32, st: u32) {
        let n = r.next_inbetween(6, 15);
        for _ in 0..n {
            let wd = mn + r.next_bound(3);
            let s = r.next_bound(192);
            let mut m = 0;
            while m < wd && s + m < 192 {
                b[(s + m) as usize] = st;
                m += 1;
            }
        }
    }
    add_bands(r, &mut b, 1, y);
    add_bands(r, &mut b, 2, br);
    add_bands(r, &mut b, 1, rd);
    let n = r.next_inbetween(9, 15);
    let (mut cur, mut idx) = (0, 0i32);
    while cur < n && idx < 192 {
        b[idx as usize] = w;
        if idx > 1 && r.next_bool() {
            b[(idx - 1) as usize] = lg;
        }
        if idx + 1 < 192 && r.next_bool() {
            b[(idx + 1) as usize] = lg;
        }
        idx += r.next_bound(16) + 4;
        cur += 1;
    }
    Some(Box::new(b))
}

// per-16-block-corner preliminary surface estimates (global, keyed by quart coord)


impl Gen {
    // surface noise -> vanilla "surface depth" (run depth), per column
    fn run_depth(&self, x: i32, z: i32) -> i32 {
        let n = self
            .surf_ni
            .map(|ni| self.noises[ni].get(x as f64, 0.0, z as f64))
            .unwrap_or(0.0);
        let mut r = self.factory.pos(x, 0, z);
        (n * 2.75 + 3.0 + ((r.next_f64() * 0.25) as f32) as f64) as i32
    }

    fn sec_depth(&self, c: &mut SC) -> f32 {
        if !c.sec_ok {
            c.sec = self
                .sec_ni
                .map(|ni| self.noises[ni].get(c.x as f64, 0.0, c.z as f64))
                .unwrap_or(0.0) as f32;
            c.sec_ok = true;
        }
        c.sec
    }

    fn leaf_at(&mut self, c: &mut SC) -> u16 {
        let (qx, qy, qz) = (c.x >> 2, c.y >> 2, c.z >> 2);
        let key = (qx, qy, qz);
        // The biome is a pure function of the quart when the search is unseeded, and the material
        // rule asks for the same quart once per block (64 blocks per quart). A per-Gen cache turns
        // millions of B-tree searches into one per quart. The key is the tuple, not a packed u64:
        // the old per-block packed key was collision-free by luck only.
        if let Some(&l) = self.bioq.get(&key) {
            return l;
        }
        let l = self
            .biome_quart(qx, qy, qz, None)
            .map(|(_, l)| l)
            .unwrap_or(u16::MAX);
        self.bioq.insert(key, l);
        self.bioq_ord.push_back(key);
        if self.bioq_ord.len() > BIO_CAP {
            if let Some(old) = self.bioq_ord.pop_front() {
                self.bioq.remove(&old);
            }
        }
        l
    }

    // find_top_surface of the preliminary_surface_level DF, cached per quart column
    pub(crate) fn prelim_est(&mut self, x: i32, z: i32) -> i32 {
        if let Ok(spec) = std::env::var("PRELIM_DUMP") {
            static DONE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
            let v = self.prelim_est_raw(x, z);
            if DONE.get().is_none() && spec == format!("{x},{z}") {
                let _ = DONE.set(());
                for dz in 0..16 {
                    for dx in 0..16 {
                        let n = self.prelim_est_raw(x + dx, z + dz);
                        eprintln!("PRELIM {} {} {}", x + dx, z + dz, n);
                    }
                }
            }
            return v;
        }
        self.prelim_est_raw(x, z)
    }
    fn prelim_est_raw(&mut self, x: i32, z: i32) -> i32 {
        let k = (x >> 2, z >> 2);
        if let Some(&v) = self.prelim_c.get(&k) {
            return v;
        }
        let v = match self.prelim_df {
            Some(id) => {
                let ax = (x >> 2) << 2;
                let az = (z >> 2) << 2;
                // 1.21.1 computes the preliminary surface in *code*, not from the
                // datapack's `find_top_surface` node: `ChunkNoiseSampler.a(long)`
                // starts at `minY + height` and steps down by 8 until
                // `initial_density_without_jaggedness` exceeds 0.390625. The
                // newer-format JSON carries its own `upper_bound` density
                // function, which here evaluates to ~47 - starting there returns
                // a surface up to 270 blocks too low, and only the aquifer reads
                // it, so terrain parity never noticed.
                match self.dfs.get(id) {
                    Some(DF::FindTopSurface { density, cell_h, .. }) => {
                        let (density, step) = (*density, (*cell_h).max(1));
                        let mut y = ((self.min_y + self.height) / step) * step;
                        let mut found = i32::MAX;
                        while y >= self.min_y {
                            if self.eval(density, ax, y, az, &mut HashMap::new()) > 0.0 {
                                found = y;
                                break;
                            }
                            y -= step;
                        }
                        found
                    }
                    _ => self.eval(id, ax, 0, az, &mut HashMap::new()) as i32,
                }
            }
            None => self.min_y,
        };
        self.prelim_c.insert(k, v);
        self.prelim_ord.push(k);
        if self.prelim_ord.len() > 8192 {
            let old = self.prelim_ord.remove(0);
            self.prelim_c.remove(&old);
        }
        v
    }

    /// Vanilla's preliminary surface level for one column: the bilinear
    /// interpolation of the four corner heights, plus the surface noise, minus
    /// the 8 block window (`SurfaceRules.Context`'s preliminary surface level).
    /// `above_preliminary_surface` is `y >= this`.
    pub(crate) fn prelim_min(&mut self, x: i32, z: i32) -> i32 {
        let (cx, cz) = (x >> 4, z >> 4);
        let h00 = self.prelim_est(cx * 16, cz * 16);
        let h10 = self.prelim_est(cx * 16 + 16, cz * 16);
        let h01 = self.prelim_est(cx * 16, cz * 16 + 16);
        let h11 = self.prelim_est(cx * 16 + 16, cz * 16 + 16);
        let est = lerp2(
            (x & 15) as f64 / 16.0,
            (z & 15) as f64 / 16.0,
            h00 as f64,
            h10 as f64,
            h01 as f64,
            h11 as f64,
        )
        .floor() as i32;
        est.saturating_add(self.run_depth(x, z)) - 8
    }

    fn test_cond(&mut self, cid: usize, c: &mut SC, surf: &[i16; 256]) -> bool {
        let cd = self.conds[cid];
        match cd {
            Cond::False => false,
            Cond::Biome(mask) => {
                let l = self.leaf_at(c);
                l != u16::MAX && l < 64 && (mask >> l) & 1 == 1
            }
            Cond::NoiseTh { ni, is3d, lo, hi } => {
                if !is3d {
                    if let Some(&b) = c.nt.get(&cid) {
                        return b;
                    }
                }
                let v = if is3d {
                    self.noises[ni].get(c.x as f64, c.y as f64, c.z as f64)
                } else {
                    self.noises[ni].get(c.x as f64, 0.0, c.z as f64)
                };
                let ok = v >= lo && v <= hi;
                if !is3d {
                    c.nt.insert(cid, ok);
                }
                ok
            }
            Cond::VGrad { lo_w, hi_w, t, f } => {
                let top = self.min_y + self.height - 1;
                let ta = t.get(self.min_y, top);
                let fa = f.get(self.min_y, top);
                if c.y <= ta {
                    return true;
                }
                if c.y >= fa {
                    return false;
                }
                let mut xr = Xor::raw(lo_w ^ self.factory.lo, hi_w ^ self.factory.hi);
                let sp = xr.splitter();
                let mut r = sp.pos(c.x, c.y, c.z);
                let m = (c.y as f32 - ta as f32) / (fa as f32 - ta as f32) * (0.0 - 1.0) + 1.0;
                r.next_f32() < m
            }
            Cond::YAbove { a, mult, sd } => {
                let top = self.min_y + self.height - 1;
                c.y + if sd { c.sda } else { 0 } >= a.get(self.min_y, top) + c.run_depth * mult
            }
            Cond::Water { off, mult, sd } => {
                c.fluid == i32::MIN
                    || c.y + if sd { c.sda } else { 0 }
                        >= c.fluid + off + c.run_depth * mult
            }
            Cond::Steep => {
                if let Some(s) = c.steep {
                    return s;
                }
                let lx = c.x & 15;
                let lz = c.z & 15;
                let t = |lx: i32, lz: i32| -> i32 {
                    surf[(lz.clamp(0, 15) as usize) * 16 + (lx.clamp(0, 15) as usize)] as i32
                };
                // pumpkin steep: north-facing (z+1 >= z-1 + 4) or west-facing (x-1 >= x+1 + 4)
                let s = t(lx, lz + 1) >= t(lx, lz - 1) + 4 || t(lx - 1, lz) >= t(lx + 1, lz) + 4;
                c.steep = Some(s);
                s
            }
            Cond::Not(i) => !self.test_cond(i, c, surf),
            Cond::Hole => c.run_depth <= 0,
            Cond::Prelim => c.y >= c.est_min,
            Cond::StoneD { off, sdep, sec, ceil } => {
                let sd = if ceil { c.sdb } else { c.sda };
                let depth = if sdep { c.run_depth } else { 0 };
                let range = if sec == 0 {
                    0
                } else {
                    lmap(self.sec_depth(c) as f64, -1.0, 1.0, 0.0, sec as f64) as i32
                };
                sd <= 1 + off + depth + range
            }
        }
    }

    fn eval_rule(&mut self, rid: usize, c: &mut SC, surf: &[i16; 256]) -> Option<u32> {
        match &self.rules[rid] {
            Rule::Block(st) => Some(*st),
            Rule::Bandlands => {
                let bands = self.bands.as_ref()?;
                let ni = self.band_off_ni?;
                let off = (self.noises[ni].get(c.x as f64, 0.0, c.z as f64) * 4.0).round() as i32;
                Some(bands[((c.y + off).rem_euclid(192)) as usize])
            }
            Rule::Seq(_) => {
                let mut i = 0;
                loop {
                    let next = match &self.rules[rid] {
                        Rule::Seq(v) => v.get(i).copied(),
                        _ => None,
                    };
                    match next {
                        None => return None,
                        Some(sub) => {
                            i += 1;
                            if let Some(st) = self.eval_rule(sub, c, surf) {
                                return Some(st);
                            }
                        }
                    }
                }
            }
            Rule::Cond(ci, ri) => {
                let (ci, ri) = (*ci, *ri);
                if self.test_cond(ci, c, surf) {
                    self.eval_rule(ri, c, surf)
                } else {
                    None
                }
            }
            Rule::OreVein { density, rich, gap, ore, raw, fill, rawc, ymin, ymax } => {
                let (density, rich, gap, ore, raw, fill, rawc) =
                    (*density, *rich, *gap, *ore, *raw, *fill, *rawc);
                let (ymin, ymax) = (*ymin, *ymax);
                if c.y < ymin || c.y >= ymax {
                    return None;
                }
                let d = self.eval(density, c.x, c.y, c.z, &mut c.memo);
                if d <= 0.0 {
                    return None;
                }
                let sp = self.ore_split.as_ref()?;
                let mut r = sp.pos(c.x, c.y, c.z);
                // `OreVeinSampler` (dzi in 1.21.1) keeps a cell with probability
                // VEIN_SOLIDNESS = 0.7; the density value is *not* the probability
                // (it decides the edge roundoff and the richness band instead).
                if r.next_f32() > 0.7 {
                    return None;
                }
                let richness = self.eval(rich, c.x, c.y, c.z, &mut c.memo) as f32;
                let gapv = self.eval(gap, c.x, c.y, c.z, &mut c.memo);
                if r.next_f32() < richness && gapv > -0.3 {
                    Some(if r.next_f32() < rawc { raw } else { ore })
                } else {
                    Some(fill)
                }
            }
        }
    }
}

// ---------- chunk column generation ----------

pub(crate) struct ChunkCols {
    // 5 x 5 x ny corner grid per `interpolated` node reachable from
    // final_density, indexed by density-function id. Vanilla interpolates only
    // the input of `interpolated`; squeeze/min/add stay per block.
    grids: Vec<Option<Box<[f64]>>>,
    surf: [i16; 256],
    ny: usize, // cell count y (48)
    min_y: i32,
    mats: Vec<Option<SurfMat>>, // per-column material-rule results (surface window)
    rd: [i16; 256],             // per-column run_depth (surface noise)
    wtop: [i16; 256],           // per-column top of solid-or-fluid (heightmap basis)
}

static GEN: Mutex<Option<Gen>> = Mutex::new(None);

pub fn init(seed: u64, res: fn(&str) -> Option<u32>) -> bool {
    match Gen::load(seed, res) {
        Some(g) => {
            *GEN.lock().unwrap() = Some(g);
            true
        }
        None => false,
    }
}
/// `MC_SPINE_DFDUMP=x,y,z` diagnostic: print the vein-sampler inputs.
pub fn dfdump(spec: &str) {
    let p: Vec<i32> = spec.split(',').filter_map(|n| n.parse().ok()).collect();
    if p.len() != 3 {
        return;
    }
    if let Some(g) = GEN.lock().unwrap().as_mut() {
        g.dump_dfs(p[0], p[1], p[2]);
    }
}
pub fn enabled() -> bool {
    GEN.lock().unwrap().is_some()
}

pub fn chunk_cols(cx: i32, cz: i32) -> Option<std::sync::Arc<ChunkCols>> {
    GEN.lock().unwrap().as_mut()?.cols(cx, cz)
}

impl Gen {
    /// Per-Gen cached chunk column builder (terrain layer only — no features).
    pub(crate) fn cols(&mut self, cx: i32, cz: i32) -> Option<std::sync::Arc<ChunkCols>> {
    if let Some(v) = self.col_c.get(&(cx, cz)) {
        return Some(v.clone());
    }
    let g = self;
    let ny = ((g.height / 8) + 1) as usize;
    let mut memo = HashMap::new();
    // One corner grid per `interpolated` node on the final_density path. The
    // grid holds the node's input; the enclosing operators are evaluated per
    // block in block_density(). Interpolating final_density itself would
    // interpolate squeeze(), which is cubic and not vanilla.
    let mut grids: Vec<Option<Box<[f64]>>> = (0..g.dfs.len()).map(|_| None).collect();
    for iid in g.interp_ids() {
        let inner = match g.dfs[iid] {
            DF::Interp(i) => i,
            _ => continue,
        };
        let mut cells = vec![0.0f64; 5 * 5 * ny];
        for zi in 0..5usize {
            for xi in 0..5usize {
                for yi in 0..ny {
                    let bx = cx * 16 + (xi as i32) * 4;
                    let by = g.min_y + (yi as i32) * 8;
                    let bz = cz * 16 + (zi as i32) * 4;
                    cells[(zi * 5 + xi) * ny + yi] = g.eval(inner, bx, by, bz, &mut memo);
                }
            }
        }
        grids[iid] = Some(cells.into_boxed_slice());
    }
    let mut cc = ChunkCols {
        grids,
        surf: [0i16; 256],
        ny,
        min_y: g.min_y,
        mats: Vec::new(),
        rd: [0i16; 256],
        wtop: [0i16; 256],
    };
    let top = g.min_y + g.height - 1;
    for zi in 0..16usize {
        for xi in 0..16usize {
            let wx = cx * 16 + xi as i32;
            let wz = cz * 16 + zi as i32;
            let mut sy = g.min_y - 1;
            let mut yy = top;
            while yy >= g.min_y {
                if g.block_density(&cc, wx, yy, wz) > 0.0 {
                    sy = yy;
                    break;
                }
                yy -= 1;
            }
            cc.surf[zi * 16 + xi] = sy as i16;
        }
    }

    // heightmap basis: topmost solid-or-fluid per column. Columns whose
    // solid top sits below sea level probe the aquifer for water so
    // OCEAN_FLOOR/WORLD_SURFACE_WG land on the water surface.
    let water_st = g.def_fl;
    for lz in 0..16usize {
        for lx in 0..16usize {
            let sy = cc.surf[lz * 16 + lx] as i32;
            let mut wt = sy;
            if sy < g.sea {
                let wx = cx * 16 + lx as i32;
                let wz = cz * 16 + lz as i32;
                let mut yy = g.sea;
                while yy > sy {
                    if g.aq_state(wx, yy, wz, -1.0) == Some(water_st) {
                        wt = yy;
                        break;
                    }
                    yy -= 1;
                }
            }
            cc.wtop[lz * 16 + lx] = wt as i16;
        }
    }
    // vanilla surface material rules: per-column top-down scan over the
    // surface window [est_min .. max(surf, sea)] tracking vanilla's
    // stone_depth_above/below and fluid_height.
    let mut mats: Vec<Option<SurfMat>> = (0..256).map(|_| None).collect();
    let mut rd = [0i16; 256];
    if let Some(root) = g.surface_rule {
        let mut c = SC::new();
        for lz in 0..16usize {
            for lx in 0..16usize {
                let x = cx * 16 + lx as i32;
                let z = cz * 16 + lz as i32;
                let surf_y = cc.surf[lz * 16 + lx] as i32;
                // vanilla's SurfaceSystem scan sets waterHeight to the topmost
                // fluid cell it has seen since the last air cell. For the window
                // the fluid above the terrain is the open water column, whose
                // top the heightmap probe already found.
                let water_surface = if (cc.wtop[lz * 16 + lx] as i32) > surf_y {
                    cc.wtop[lz * 16 + lx] as i32
                } else {
                    i32::MIN
                };
                let scan_top = surf_y.max(g.sea);
                c.x = x;
                c.z = z;
                c.run_depth = g.run_depth(x, z);
                c.sec_ok = false;
                c.steep = None;
                c.nt.clear();
                rd[lz * 16 + lx] = c.run_depth as i16;
                let est_min = g.prelim_min(x, z);
                c.est_min = est_min;
                if scan_top < est_min {
                    continue;
                }
                let mut vals = vec![u16::MAX; (scan_top - est_min + 1) as usize];
                let mut min = i32::MAX;
                let mut sda = 0i32;
                let mut fluid = i32::MIN;
                let mut y = scan_top;
                while y >= est_min {
                    let d = g.block_density(&cc, x, y, z);
                    if d <= 0.0 {
                        // A fluid cell sets waterHeight once; air resets it, as
                        // vanilla's buildSurface scan does. Above the terrain the
                        // open column is water up to the probed surface; below it
                        // only an aquifer water/lava cell counts.
                        let fluid_cell = if y > surf_y {
                            y <= water_surface
                        } else {
                            g.aq_state(x, y, z, d).is_some_and(|st| g.is_fluid_state(st))
                        };
                        if fluid_cell {
                            if fluid == i32::MIN {
                                fluid = y + 1;
                            }
                        } else {
                            sda = 0;
                            fluid = i32::MIN;
                        }
                        y -= 1;
                        continue;
                    }
                    if min >= y {
                        min = g.min_y;
                        let mut sy = y - 1;
                        while sy >= g.min_y {
                            if g.block_density(&cc, x, sy, z) <= 0.0 {
                                min = sy + 1;
                                break;
                            }
                            sy -= 1;
                        }
                    }
                    sda += 1;
                    c.y = y;
                    c.sda = sda;
                    c.sdb = y - min + 1;
                    c.fluid = fluid;
                    if let Some(st) = g.eval_rule(root, &mut c, &cc.surf) {
                        vals[(y - est_min) as usize] = st.min(u16::MAX as u32 - 1) as u16;
                    }
                    y -= 1;
                }
                mats[lz * 16 + lx] = Some(SurfMat {
                    lo: est_min as i16,
                    vals: vals.into_boxed_slice(),
                });
            }
        }
    }
    cc.mats = mats;
    cc.rd = rd;
    let arc = std::sync::Arc::new(cc);
    g.col_ord.push((cx, cz));
    if g.col_ord.len() > 128 {
        let old = g.col_ord.remove(0);
        g.col_c.remove(&old);
    }
    g.col_c.insert((cx, cz), arc.clone());
    Some(arc)
    }
}

impl Gen {
    /// Full block decision (feature layer, material window, aquifer, deep rules)
    /// without taking the GEN lock. `terrain_section` and the free `block_state`
    /// use it so a chunk fill takes one lock instead of five per block.
    pub(crate) fn block_state_at(
        &mut self, x: i32, y: i32, z: i32, populate: impl FnMut(&mut Gen, i32, i32),
    ) -> u32 {
        if let Some(st) = self.feat_state(x, y, z, populate) {
            return st;
        }
        let cc = match self.cols(x >> 4, z >> 4) {
            Some(c) => c,
            None => return 0,
        };
        let d = self.block_density(&cc, x, y, z);
        if d > 0.0 {
            if let Some(st) = cc.mat((x & 15) as usize, (z & 15) as usize, y) {
                let st = if st == u32::MAX { self.def_st } else { st };
                return self.vein_override(&cc, x, y, z, st);
            }
            let st = self.deep_state_cached(x, y, z, &cc);
            return self.vein_override(&cc, x, y, z, st);
        }
        match self.aq_state(x, y, z, d) {
            Some(0) => 0,
            Some(st) => st,
            None => self.deep_state_cached(x, y, z, &cc),
        }
    }

    /// `MC_SPINE_DFDUMP=x,y,z`: print the four density functions the vein
    /// sampler's caller passes, for comparison with the live server.
    pub(crate) fn dump_dfs(&mut self, x: i32, y: i32, z: i32) {
        let mut memo = std::mem::take(&mut self.memo);
        let cc = self.cols(x >> 4, z >> 4);
        let ccr = cc.as_deref();
        let fd = self.eval_c(self.final_density, x, y, z, &mut memo, ccr);
        let v = self.vein.map(|v| {
            (
                self.eval_c(v.toggle, x, y, z, &mut memo, ccr),
                self.eval_c(v.ridged, x, y, z, &mut memo, ccr),
                self.eval_c(v.gap, x, y, z, &mut memo, ccr),
            )
        });
        self.memo = memo;
        eprintln!("DFDUMP {x} {y} {z} final={fd:.6} vein={v:?}");
    }

    /// 1.21.1 `OreVeinSampler` (`dzi` in the obfuscated jar). The terrain state
    /// is overridden where a vein is present:
    ///
    /// ```text
    /// t = vein_toggle(ctx);  type = t > 0 ? COPPER : IRON
    /// if y outside [min, max) of the type                     -> no vein
    /// if |t| + map(min(up, down), 0, 20, -0.2, 0) < 0.4       -> no vein
    /// r = random at (x,y,z); if r.nextFloat() > 0.7           -> no vein
    /// if vein_ridged(ctx) >= 0                                -> no vein
    /// richness = map(|t|, 0.4, 0.6, 0.1, 0.3)
    /// if r.nextFloat() < richness && vein_gap(ctx) > -0.3
    ///     -> r.nextFloat() < 0.02 ? raw ore : ore
    /// else -> filler
    /// ```
    ///
    /// COPPER is y 0..50 with copper_ore / raw_copper_block / granite, IRON is
    /// y -60..-8 with deepslate_iron_ore / raw_iron_block / tuff.
    fn vein_override(&mut self, cc: &ChunkCols, x: i32, y: i32, z: i32, st: u32) -> u32 {
        let v = match self.vein {
            Some(v) => v,
            None => {
                static ONCE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
                if std::env::var("MC_SPINE_VEINDBG").is_ok()
                    && ONCE.swap(false, std::sync::atomic::Ordering::Relaxed)
                {
                    eprintln!("VEINCFG sampler active but vein=None");
                }
                return st;
            }
        };
        if st == 0 {
            return st;
        }
        let map = |v: f64, lo: f64, hi: f64, from: f64, to: f64| {
            if v <= lo {
                from
            } else if v >= hi {
                to
            } else {
                (v - lo) / (hi - lo) * (to - from) + from
            }
        };
        let mut memo = std::mem::take(&mut self.memo);
        let t = self.eval_c(v.toggle, x, y, z, &mut memo, Some(cc));
        let (min_y, max_y, ore, raw, fill) = if t > 0.0 {
            (0i32, 50i32, v.copper_ore, v.copper_raw, v.copper_fill)
        } else {
            (-60i32, -8i32, v.iron_ore, v.iron_raw, v.iron_fill)
        };
        let dbg = std::env::var("MC_SPINE_VEINTRACE").ok()
            .and_then(|v| {
                let p: Vec<i32> = v.split(',').filter_map(|n| n.parse().ok()).collect();
                if p.len() == 3 && p[0] == x && p[1] == y && p[2] == z { Some(()) } else { None }
            })
            .is_some();
        if dbg {
            let inner = match self.dfs[v.toggle] {
                DF::Interp(i) => i,
                _ => v.toggle,
            };
            let direct = self.eval_c(inner, x, y, z, &mut memo, None);
            let nodirect = self.eval_c(v.toggle, x, y, z, &mut memo, None);
            eprintln!(
                "VEIN {x} {y} {z} t={t:.4} direct={direct:.4} interp_no_cc={nodirect:.4} st={st}"
            );
        }
        let (up, down) = (max_y - y, y - min_y);
        let out = if up < 0 || down < 0 {
            None
        } else {
            let at = t.abs();
            let roundoff = map(up.min(down) as f64, 0.0, 20.0, -0.2, 0.0);
            if at + roundoff < 0.4 {
                None
            } else {
                // `RandomState` builds `this.g = this.a.fromHashOf("ore")
                // .forkPositional()` and hands *that* to OreVeinifier, not the
                // base factory (decompiled dzm: `this.g = this.a.a(akr.b("ore")).e()`).
                let mut r = match self.ore_split.as_ref() {
                    Some(f) => f.pos(x, y, z),
                    None => self.factory.pos(x, y, z),
                };
                let r1 = r.next_f32();
                if dbg {
                    eprintln!("VEIN   up={up} down={down} at={at:.4} round={roundoff:.4} r1={r1:.4}");
                }
                if r1 > 0.7 {
                    None
                } else if self.eval_c(v.ridged, x, y, z, &mut memo, Some(cc)) >= 0.0 {
                    if dbg { eprintln!("VEIN   ridged reject"); }
                    None
                } else {
                    let richness = map(at, 0.4, 0.6, 0.1, 0.3);
                    let r2 = r.next_f32();
                    let gv = self.eval_c(v.gap, x, y, z, &mut memo, Some(cc));
                    let r3 = r.next_f32();
                    if dbg {
                        eprintln!("VEIN   rich={richness:.4} r2={r2:.4} gap={gv:.4} r3={r3:.4}");
                    }
                    if (r2 as f64) < richness && gv > -0.3 {
                        Some(if r3 < 0.02 { raw } else { ore })
                    } else {
                        Some(fill)
                    }
                }
            }
        };
        self.memo = memo;
        out.unwrap_or(st)
    }

    /// Terrain-layer block state at a world position (no feature layer).
    /// Mirrors wg2_block: material window -> deep rules -> aquifer.
    pub(crate) fn terrain_state(&mut self, x: i32, y: i32, z: i32) -> u32 {
        let cc = match self.cols(x >> 4, z >> 4) {
            Some(c) => c,
            None => return 0,
        };
        self.terrain_cell(&cc, x, y, z)
    }

    /// `terrain_state` with the chunk columns already fetched. Feature placement
    /// reads thousands of cells from one chunk; fetching `cols()` (a HashMap
    /// lookup plus an Arc clone) per cell was ~1.2 us of the 1.8 us read.
    pub(crate) fn terrain_cell(&mut self, cc: &ChunkCols, x: i32, y: i32, z: i32) -> u32 {
        if y < self.min_y || y >= self.min_y + self.height {
            return 0;
        }
        let d = self.block_density(cc, x, y, z);
        if d > 0.0 {
            if let Some(st) = cc.mat((x & 15) as usize, (z & 15) as usize, y) {
                let st = if st == u32::MAX { self.def_st } else { st };
                return self.vein_override(cc, x, y, z, st);
            }
            let st = self.deep_state_cached(x, y, z, cc);
            return self.vein_override(cc, x, y, z, st);
        }
        match self.aq_state(x, y, z, d) {
            Some(st) => st,
            None => self.deep_state_cached(x, y, z, cc),
        }
    }
    /// WORLD_SURFACE_WG-style height (first non-air + 1) at a column.
    pub(crate) fn top_ws(&mut self, x: i32, z: i32) -> i32 {
        match self.cols(x >> 4, z >> 4) {
            Some(c) => c.wtop((x & 15) as usize, (z & 15) as usize) + 1,
            None => self.min_y,
        }
    }
    /// OCEAN_FLOOR_WG-style height (first solid + 1).
    pub(crate) fn top_oc(&mut self, x: i32, z: i32) -> i32 {
        match self.cols(x >> 4, z >> 4) {
            Some(c) => c.surface((x & 15) as usize, (z & 15) as usize) + 1,
            None => self.min_y,
        }
    }
    /// biome registry name at a block position (quart-sampled).
    pub(crate) fn biome_name_at(&mut self, x: i32, y: i32, z: i32) -> Option<String> {
        let (_, leaf) = self.biome_quart(x >> 2, y >> 2, z >> 2, None)?;
        self.biome_name_of_leaf(leaf)
    }
    pub(crate) fn biome_name_of_leaf(&self, leaf: u16) -> Option<String> {
        self.biome_names.get(leaf as usize).cloned()
    }
    /// feature-layer lookup: populates the 3x3 neighborhood on demand, then
    /// returns the override state for (x,y,z) if a feature wrote one.
    pub(crate) fn feat_state(
        &mut self, x: i32, y: i32, z: i32,
        mut populate: impl FnMut(&mut Gen, i32, i32),
    ) -> Option<u32> {
        let (cx, cz) = (x >> 4, z >> 4);
        // Decoration order. Vanilla decorates a chunk when the ticket order reaches
        // it; for a world's spawn area that is a lexicographic scan of a square around
        // the spawn, so a chunk sees the canopies of everything *earlier* in that scan
        // and nothing else. `MC_SPINE_POP_REGION=x0,z0,x1,z1` reproduces that order
        // (which the fixture compares against); the default decorates this chunk's 3x3
        // relatively, which is what a single-chunk read does.
        if let Some((px0, pz0, px1, pz1)) = self.pop_region {
            for sx in px0..=px1 {
                for sz in pz0..=pz1 {
                    if (sx, sz) > (cx, cz) {
                        break;
                    }
                    if !self.pop_done.contains(&(sx, sz)) {
                        self.pop_done.insert((sx, sz));
                        populate(self, sx, sz);
                    }
                }
            }
            if !self.pop_done.contains(&(cx, cz)) {
                self.pop_done.insert((cx, cz));
                populate(self, cx, cz);
            }
        } else {
            let nbr = std::env::var("MC_SPINE_FEAT_NBR").map(|v| v != "0").unwrap_or(true);
            let (x0, x1) = if nbr { (cx - 1, cx + 1) } else { (cx, cx) };
            let (z0, z1) = if nbr { (cz - 1, cz + 1) } else { (cz, cz) };
            for sx in x0..=x1 {
                for sz in z0..=z1 {
                    if !self.pop_done.contains(&(sx, sz)) {
                        self.pop_done.insert((sx, sz));
                        populate(self, sx, sz);
                    }
                }
            }
        }
        // bound the override maps (LRU over chunks)
        while self.feat_ord.len() > 384 {
            let old = self.feat_ord.remove(0);
            self.feat_c.remove(&old);
            self.pop_done.remove(&old);
        }
        let k = ((y - self.min_y) as u32) << 8 | (((z & 15) as u32) << 4) | (x & 15) as u32;
        self.feat_c.get(&(cx, cz))?.get(&k).copied()
    }
    /// The feature layer's heightmap: for each column, the highest non-air and the
    /// highest solid block a *feature* has written. Vanilla keeps this per chunk and
    /// updates it on every `setBlock`, so a tree placed for one chunk raises the
    /// heightmap a neighbouring chunk's placements read - `heightmap(OCEAN_FLOOR)`
    /// counts canopy leaves, and a later candidate then fails `would_survive` on the
    /// leaf above it. Keeping the tally inside one chunk's placement pass loses that
    /// (measured: vanilla's candidate (0,70,17) against this port's (0,64,17)).
    pub(crate) fn note_feature_write(&mut self, x: i32, y: i32, z: i32, st: u32) {
        let reg = crate::block_registry::breg();
        let solid = reg.map(|r| r.is_solid(st)).unwrap_or(false);
        let air = reg.map(|r| r.is_air(st)).unwrap_or(st == 0);
        let e = self.feat_hm.entry((x, z)).or_insert((i32::MIN, i32::MIN));
        if !air {
            e.0 = e.0.max(y + 1);
        }
        if solid {
            e.1 = e.1.max(y + 1);
        }
    }

    /// The distinct biomes of a chunk's sections, cached per chunk: vanilla unions
    /// these over the 3x3 (and intersects with the generator's possible biomes)
    /// before running a feature, so a *neighbour's* river makes `trees_water` run for
    /// this chunk. Sampling a few columns at y=0 instead reads the cave biomes and
    /// silently skipped it.
    pub(crate) fn chunk_biome_set(&mut self, cx: i32, cz: i32) -> std::sync::Arc<Vec<String>> {
        if let Some(v) = self.chunk_biomes.get(&(cx, cz)) {
            return v.clone();
        }
        let mut seen: Vec<String> = vec![];
        let mut set = std::collections::HashSet::new();
        let (qx0, qz0) = (cx * 4, cz * 4);
        let qy_max = (self.height / 16) * 4;
        for qy in 0..qy_max {
            for lx in 0..4 {
                for lz in 0..4 {
                    if let Some((_, leaf)) = self.biome_quart(qx0 + lx, qy, qz0 + lz, None) {
                        if let Some(name) = self.biome_name_of_leaf(leaf) {
                            if set.insert(name.clone()) {
                                seen.push(name);
                            }
                        }
                    }
                }
            }
        }
        let arc = std::sync::Arc::new(seen);
        self.chunk_biomes_ord.push_back((cx, cz));
        while self.chunk_biomes.len() > 384 {
            if let Some(old) = self.chunk_biomes_ord.pop_front() {
                self.chunk_biomes.remove(&old);
            } else {
                break;
            }
        }
        self.chunk_biomes.insert((cx, cz), arc.clone());
        arc
    }

    /// Populate the 3x3 around a chunk if it has not been populated yet, so a
    /// diagnostic reads the same feature-layer state a generation loop would.
    pub(crate) fn ensure_populated(&mut self, cx: i32, cz: i32) {
        for dx in -1..=1 {
            for dz in -1..=1 {
                if !self.pop_done.contains(&(cx + dx, cz + dz)) {
                    crate::feature::populate(self, cx + dx, cz + dz);
                }
            }
        }
    }

    /// Highest non-air (`0`) / solid (`1`) feature-written block + 1 in a column.
    pub(crate) fn feature_height(&self, x: i32, z: i32, solid: usize) -> i32 {
        match self.feat_hm.get(&(x, z)) {
            Some(e) => if solid == 0 { e.0 } else { e.1 },
            None => i32::MIN,
        }
    }

    pub(crate) fn feat_write(&mut self, x: i32, y: i32, z: i32, st: u32) {
        if y < self.min_y || y >= self.min_y + self.height {
            return;
        }
        let key = (x >> 4, z >> 4);
        let k = ((y - self.min_y) as u32) << 8 | (((z & 15) as u32) << 4) | (x & 15) as u32;
        if !self.feat_c.contains_key(&key) {
            self.feat_ord.push(key);
        }
        self.feat_c.entry(key).or_default().insert(k, st);
    }
    pub(crate) fn wseed(&self) -> u64 {
        self.world_seed
    }
    /// feature overrides for a whole chunk (populate flushes here)
    #[allow(dead_code)]
    pub(crate) fn feat_map(&self, cx: i32, cz: i32) -> Option<&HashMap<u32, u32>> {
        self.feat_c.get(&(cx, cz))
    }
}

impl ChunkCols {
    pub fn surface(&self, lx: usize, lz: usize) -> i32 {
        self.surf[lz * 16 + lx] as i32
    }
    /// topmost solid-or-fluid y (WORLD_SURFACE_WG / OCEAN_FLOOR_WG basis)
    pub fn wtop(&self, lx: usize, lz: usize) -> i32 {
        self.wtop[lz * 16 + lx] as i32
    }
    /// material-rule result inside the surface window.
    /// Some(u32::MAX) = scanned but no rule matched -> default block.
    /// None = outside the scanned window -> caller runs deep_state.
    pub fn mat(&self, lx: usize, lz: usize, y: i32) -> Option<u32> {
        let m = self.mats[lz * 16 + lx].as_ref()?;
        let i = y - m.lo as i32;
        if i < 0 || i as usize >= m.vals.len() {
            return None;
        }
        let v = m.vals[i as usize];
        Some(if v == u16::MAX { u32::MAX } else { v as u32 })
    }
    /// Trilinear value of this chunk's grid for `interpolated` node `id`, or
    /// `None` when that node has no grid (not reachable from final_density).
    #[inline]
    fn interp(&self, id: usize, x: i32, y: i32, z: i32) -> Option<f64> {
        let cells = self.grids.get(id)?.as_deref()?;
        Some(trilinear(cells, self.ny, self.min_y, x, y, z))
    }
}

#[inline]
fn trilinear(cells: &[f64], ny: usize, min_y: i32, x: i32, y: i32, z: i32) -> f64 {
    let lx = (x & 15) as f64;
    let lz = (z & 15) as f64;
    let ly = (y - min_y) as f64;
    let xi = (lx / 4.0).floor().min(3.0) as usize;
    let zi = (lz / 4.0).floor().min(3.0) as usize;
    let yi = (ly / 8.0).floor().min((ny - 2) as f64) as usize;
    let fx = lx / 4.0 - xi as f64;
    let fz = lz / 4.0 - zi as f64;
    let fy = ly / 8.0 - yi as f64;
    let g = |xi: usize, yi: usize, zi: usize| cells[(zi * 5 + xi) * ny + yi];
    let c000 = g(xi, yi, zi);
    let c100 = g(xi + 1, yi, zi);
    let c010 = g(xi, yi + 1, zi);
    let c110 = g(xi + 1, yi + 1, zi);
    let c001 = g(xi, yi, zi + 1);
    let c101 = g(xi + 1, yi, zi + 1);
    let c011 = g(xi, yi + 1, zi + 1);
    let c111 = g(xi + 1, yi + 1, zi + 1);
    lerp3(fx, fy, fz, c000, c100, c010, c110, c001, c101, c011, c111)
}

// public API -------------------------------------------------------------

/// Density at a block position (trilerped from the per-chunk cell grid).
pub fn density_at(x: i32, y: i32, z: i32) -> Option<f64> {
    let cc = chunk_cols(x >> 4, z >> 4)?;
    Some(density_in(&cc, x, y, z))
}

/// Block density for a column already fetched with `chunk_cols`.
pub fn density_in(cc: &ChunkCols, x: i32, y: i32, z: i32) -> f64 {
    match GEN.lock().unwrap().as_ref() {
        Some(g) => g.block_density(cc, x, y, z),
        None => 0.0,
    }
}

pub fn debug_eval(name: &str, x: i32, y: i32, z: i32) -> Option<f64> {
    let mut g = GEN.lock().unwrap();
    let g = g.as_mut()?;
    let mut m = HashMap::new();
    if let Some(nn) = name.strip_prefix("noise:") {
        let ni = g.noise_id(&format!("minecraft:{}", nn))?;
        return Some(g.noises[ni].get(x as f64, y as f64, z as f64));
    }
    let files = Gen::df_files();
    let id = g.resolve_ref(&files, name)?;
    Some(g.eval(id, x, y, z, &mut m))
}

#[expect(dead_code)]
pub fn sea_level() -> i32 {
    GEN.lock().unwrap().as_ref().map(|g| g.sea).unwrap_or(63)
}
pub fn default_state() -> u32 {
    GEN.lock().unwrap().as_ref().map(|g| g.def_st).unwrap_or(1)
}

/// material-rule result for blocks below the surface window (bedrock,
/// ore veins, deepslate gradient, underground fallback -> default block).
/// stone_depth/fluid context is pinned "deep" — vanilla underground rules
/// do not read them.
pub fn deep_state(x: i32, y: i32, z: i32, cc: &ChunkCols) -> u32 {
    let mut gg = GEN.lock().unwrap();
    match gg.as_mut() {
        Some(g) => g.deep_state_cached(x, y, z, cc),
        None => 1,
    }
}

/// `MC_SPINE_BEDROCK=flat` replaces the per-cell vanilla bedrock gradient with a solid
/// five-block floor. It is a deliberate parity/cheapness option for players who prefer the
/// flat floor; measured at ~8us a chunk (about 0.14% of the terrain layer), so the vanilla
/// per-cell pattern stays the default.
pub fn flat_bedrock() -> bool {
    static FLAT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAT.get_or_init(|| std::env::var("MC_SPINE_BEDROCK").ok().as_deref() == Some("flat"))
}
impl Gen {
    /// in-lock deep rule evaluation (feature layer / tests use this; the free
    /// `deep_state` wrapper locks GEN).
    /// The material rule below the surface window. Running the rule is the most expensive
    /// terrain work the feature pass does, and the same cell is read again by the send pass
    /// (`wg2_block`) immediately after, so the result is memoized here rather than recomputed.
    pub(crate) fn deep_state_cached(&mut self, x: i32, y: i32, z: i32, cc: &ChunkCols) -> u32 {
        if let Some(&v) = self.deep_c.get(&(x, y, z)) { return v; }
        let v = self.deep_state_i(x, y, z, cc);
        self.deep_c.insert((x, y, z), v);
        self.deep_ord.push_back((x, y, z));
        while self.deep_ord.len() > 500_000 {
            if let Some(old) = self.deep_ord.pop_front() { self.deep_c.remove(&old); }
        }
        v
    }
    /// Water or lava, as the aquifer decided it for this cell.
    pub(crate) fn is_fluid_state(&self, st: u32) -> bool {
        if st == self.def_fl {
            return true;
        }
        self.aq.as_ref().is_some_and(|a| st == a.lava_st)
    }

    /// Vanilla `WorldCarver.getCarveState`: at or below the lava level the cell
    /// becomes lava, otherwise the aquifer's substance at density 0. `None` is
    /// the aquifer's barrier (the material rule keeps the cell solid) and means
    /// "do not carve here".
    pub(crate) fn carve_state(&mut self, x: i32, y: i32, z: i32, lava_level: i32) -> Option<u32> {
        if y <= lava_level {
            return Some(self.aq.as_ref().map(|a| a.lava_st).unwrap_or(0));
        }
        self.aq_state(x, y, z, 0.0)
    }

    /// Vanilla `CarvingContext.topMaterial` / `SurfaceSystem.buildSurface` for a
    /// single position: the surface rule evaluated with one block of stone above
    /// and below, and the fluid height the carved state implies. A carver calls
    /// this to restore the top material under a cave mouth it opened.
    pub(crate) fn top_material_at(
        &mut self, x: i32, y: i32, z: i32, is_fluid: bool,
    ) -> Option<u32> {
        let root = self.surface_rule?;
        let cc = self.cols(x >> 4, z >> 4)?;
        let mut c = SC::new();
        c.x = x;
        c.z = z;
        c.y = y;
        c.run_depth = self.run_depth(x, z);
        c.est_min = self.prelim_min(x, z);
        c.sda = 1;
        c.sdb = 1;
        c.fluid = if is_fluid { y + 1 } else { i32::MIN };
        self.eval_rule(root, &mut c, &cc.surf)
    }

    pub(crate) fn deep_state_i(&mut self, x: i32, y: i32, z: i32, cc: &ChunkCols) -> u32 {
        // Opt-in flat floor: the vanilla bedrock band is y in [min_y, min_y+4].
        if flat_bedrock() && y >= self.min_y && y < self.min_y + 5 {
            return self.bedrock_st;
        }
        let root = match self.surface_rule {
            Some(r) => r,
            None => return self.def_st,
        };
        let mut c = SC::new();
        c.x = x;
        c.z = z;
        c.y = y;
        c.run_depth = cc.rd[((z & 15) as usize) * 16 + (x & 15) as usize] as i32;
        c.est_min = i32::MAX; // above_preliminary_surface -> false
        c.sda = 32000;
        c.sdb = 32000;
        c.fluid = i32::MIN;
        self.eval_rule(root, &mut c, &cc.surf).unwrap_or(self.def_st)
    }
}
pub fn min_y() -> i32 {
    GEN.lock().unwrap().as_ref().map(|g| g.min_y).unwrap_or(-64)
}
pub fn max_y() -> i32 {
    GEN.lock()
        .unwrap()
        .as_ref()
        .map(|g| g.min_y + g.height - 1)
        .unwrap_or(319)
}

/// First solid (density>0) y when scanning down from max_y. -min_y-1 if none.
pub fn surface_of(x: i32, z: i32) -> i32 {
    let mut y = max_y();
    while y >= min_y() {
        if density_at(x, y, z).unwrap_or(0.0) > 0.0 {
            return y;
        }
        y -= 1;
    }
    min_y() - 1
}

// ---------- aquifer (vanilla NoiseBasedAquifer port) ----------

const AQ_MIN: i32 = -32512; // vanilla MIN_HEIGHT_CELL (sentinel magnitude affects calc_density)
const AQ_OFF: [(i32, i32); 13] = [
    (0, 0), (-2, -1), (-1, -1), (0, -1), (1, -1), (-3, 0), (-2, 0), (-1, 0), (1, 0),
    (-2, 1), (-1, 1), (0, 1), (1, 1),
];
#[inline]
fn lxz(v: i32) -> i32 {
    v.div_euclid(16)
}
#[inline]
fn ly12(v: i32) -> i32 {
    v.div_euclid(12)
}
// fluid level = (max_y exclusive, kind); kind: 1=water, 2=lava
impl Gen {
    #[inline]
    fn aq_pick(&self, y: i32) -> (i32, u8) {
        if y < -54 {
            (-54, 2)
        } else {
            (self.sea, 1)
        }
    }
    #[inline]
    fn lvl_block(max_y: i32, kind: u8, y: i32) -> u8 {
        if y < max_y {
            kind
        } else {
            0
        }
    }
    // per-cell random sample position (16x12x16 grid, minecraft:aquifer deriver)
    /// `AQ_POS="x,y,z"` prints the 12 sample positions for that query block,
    /// for diffing against `tools/vanilla-probe/AquiferPos.java`.
    fn aq_pos_trace(&mut self, x: i32, y: i32, z: i32) {
        use std::sync::OnceLock;
        static SPEC: OnceLock<Option<String>> = OnceLock::new();
        let Some(spec) = SPEC.get_or_init(|| std::env::var("AQ_POS").ok()) else {
            return;
        };
        if *spec != format!("{x},{y},{z}") {
            return;
        }
        let (sx, sy, sz) = (lxz(x - 5), ly12(y + 1), lxz(z - 5));
        for dx in 0..=1 {
            for dy in -1..=1 {
                for dz in 0..=1 {
                    let (px, py, pz) = self.aq_rpos_c(sx + dx, sy + dy, sz + dz);
                    eprintln!(
                        "APOS {} {} {} {} {} {}",
                        sx + dx, sy + dy, sz + dz, px, py, pz
                    );
                }
            }
        }
    }
    fn aq_rpos(&self, gx: i32, gy: i32, gz: i32) -> (i32, i32, i32) {
        let mut r = self.aq.as_ref().unwrap().split.pos(gx, gy, gz);
        (
            gx * 16 + r.next_bound(10),
            gy * 12 + r.next_bound(9),
            gz * 16 + r.next_bound(10),
        )
    }
    // highest y still needing full sampling: max prelim surface over the
    // chunk region +8, snapped to a cell boundary (pumpkin skip_sampling_above_y)
    fn aq_skip_above(&mut self, cx: i32, cz: i32) -> i32 {
        if let Some(&v) = self.aq_skip.get(&(cx, cz)) {
            return v;
        }
        let mut mx = i32::MIN;
        let mut z = (cz - 1) * 16;
        while z <= (cz + 1) * 16 + 9 {
            let mut x = (cx - 1) * 16;
            while x <= (cx + 1) * 16 + 9 {
                mx = mx.max(self.prelim_est(x, z));
                x += 4;
            }
            z += 4;
        }
        let adj = mx + 8;
        let v = (ly12(adj + 12) + 1) * 12 + 11 - 1;
        self.aq_skip.insert((cx, cz), v);
        self.aq_sko.push((cx, cz));
        if self.aq_sko.len() > 4096 {
            let old = self.aq_sko.remove(0);
            self.aq_skip.remove(&old);
        }
        v
    }
    // per-cell random position, cached per cell (pumpkin packed_positions)
    fn aq_rpos_c(&mut self, gx: i32, gy: i32, gz: i32) -> (i32, i32, i32) {
        let key = (gx, gy, gz);
        if let Some(&v) = self.aq_near.get(&key) {
            return v;
        }
        let v = self.aq_rpos(gx, gy, gz);
        self.aq_near.insert(key, v);
        self.aq_neo.push(key);
        if self.aq_neo.len() > 16384 {
            let old = self.aq_neo.remove(0);
            self.aq_near.remove(&old);
        }
        v
    }
    // 12 neighbor-cell random positions -> 4 nearest positions to (x,y,z)
    // (vanilla sorts per query point — result depends on (x,y,z), not just cell)
    /// `NoiseBasedAquifer.computeSubstance` nearest-four selection.
    ///
    /// Bytecode order is x outer, y middle, z inner (`dx 0..1`, `dy -1..1`,
    /// `dz 0..1`) and every comparison is `>=`, i.e. on a tie the *later*
    /// cell is inserted in front and the earlier one shifts down. Symmetric
    /// cells tie often, and which cells land in slots 2/3 picks the pairs the
    /// three pressure tests use, so the tie rule is observable.
    fn aq_near4(&mut self, x: i32, y: i32, z: i32) -> [(i32, i32, i32); 4] {
        let sx = lxz(x - 5);
        let sy = ly12(y + 1);
        let sz = lxz(z - 5);
        let mut near = [((0, 0, 0), i64::MAX); 4];
        for dx in 0..=1 {
            for dy in -1..=1 {
                for dz in 0..=1 {
                    let (px, py, pz) = self.aq_rpos_c(sx + dx, sy + dy, sz + dz);
                    let (ex, ey, ez) = (px - x, py - y, pz - z);
                    let h = ex as i64 * ex as i64 + ey as i64 * ey as i64 + ez as i64 * ez as i64;
                    let p = ((px, py, pz), h);
                    if near[0].1 >= h {
                        near[3] = near[2];
                        near[2] = near[1];
                        near[1] = near[0];
                        near[0] = p;
                    } else if near[1].1 >= h {
                        near[3] = near[2];
                        near[2] = near[1];
                        near[1] = p;
                    } else if near[2].1 >= h {
                        near[3] = near[2];
                        near[2] = p;
                    } else if near[3].1 >= h {
                        near[3] = p;
                    }
                }
            }
        }
        [near[0].0, near[1].0, near[2].0, near[3].0]
    }
    // FluidLevel (max_y, kind) at a cell-random position, cached per cell
    // (pumpkin get_water_level). Only ever called with a cell's own random
    // position — arbitrary positions must use aq_fluid_level_at to avoid
    // polluting the per-cell cache (pumpkin's get_fluid_level is uncached).
    fn aq_water_level(&mut self, rx: i32, ry: i32, rz: i32) -> (i32, u8) {
        // Cache key = the cell the sample position belongs to: vanilla's
        // `NoiseBasedAquifer.a(long)` keys on `floorDiv(px,16)`, `floorDiv(py,12)`,
        // `floorDiv(pz,16)` - **without** the `-5/+1` offsets the neighbour
        // selection uses. Those offsets are not a bijection on positions: a
        // position with r>=5 in cell c and one with r<5 in cell c+1 land on the
        // same key, so the two cells would share one level.
        let key = (rx.div_euclid(16), ry.div_euclid(12), rz.div_euclid(16));
        if let Some(&v) = self.aq_lv.get(&key) {
            return v;
        }
        let v = self.aq_fluid_level_at(rx, ry, rz);
        self.aq_lv_ins(key, v);
        v
    }
    fn aq_fluid_level_at(&mut self, rx: i32, ry: i32, rz: i32) -> (i32, u8) {
        let def = self.aq_pick(ry);
        let (j, k) = (ry + 12, ry - 12);
        let mut bl = false;
        let mut min_est = i32::MAX;
        for (ox, oz) in AQ_OFF {
            let (xx, zz) = (rx + ox * 16, rz + oz * 16);
            let n = self.prelim_est(xx, zz);
            let o = n + 8;
            let center = ox == 0 && oz == 0;
            if center && k > o {
                return def;
            }
            if j > o || center {
                let fl = self.aq_pick(o);
                if Self::lvl_block(fl.0, fl.1, o) != 0 {
                    if center {
                        bl = true;
                    }
                    if j > o {
                        return fl;
                    }
                }
            }
            if n < min_est {
                min_est = n;
            }
        }
        let p = self.aq_fluid_y(rx, ry, rz, def, min_est, bl);
        let kind = self.aq_fluid_kind(rx, ry, rz, def, p);
        (p, kind)
    }
    fn aq_lv_ins(&mut self, key: (i32, i32, i32), v: (i32, u8)) {
        self.aq_lv.insert(key, v);
        self.aq_lvo.push(key);
        if self.aq_lvo.len() > 16384 {
            let old = self.aq_lvo.remove(0);
            self.aq_lv.remove(&old);
        }
    }
    fn aq_fluid_y(
        &mut self,
        x: i32,
        y: i32,
        z: i32,
        def: (i32, u8),
        min_est: i32,
        map_y: bool,
    ) -> i32 {
        let mut memo = HashMap::new();
        let deep = self.eval(self.aq.as_ref().unwrap().excl, x, y, z, &mut memo) > 0.0;
        let (d, e) = if deep {
            (-1.0, -1.0)
        } else {
            let top = min_est + 8 - y;
            let f = if map_y {
                1.0 - (top as f64).clamp(0.0, 64.0) / 64.0
            } else {
                0.0
            };
            let g = self
                .eval(self.aq.as_ref().unwrap().flood, x, y, z, &mut memo)
                .clamp(-1.0, 1.0);
            (
                g - lmap(f, 1.0, 0.0, -0.8, 0.4),
                g - lmap(f, 1.0, 0.0, -0.3, 0.8),
            )
        };
        if e > 0.0 {
            def.0
        } else if d > 0.0 {
            self.aq_noise_level(x, y, z, min_est)
        } else {
            AQ_MIN
        }
    }
    fn aq_noise_level(&mut self, x: i32, y: i32, z: i32, min_est: i32) -> i32 {
        let (gx, gy, gz) = (x.div_euclid(16), y.div_euclid(40), z.div_euclid(16));
        let s = self.eval(self.aq.as_ref().unwrap().spread, gx, gy, gz, &mut HashMap::new()) * 10.0;
        min_est.min(((s / 3.0).floor() as i32) * 3 + gy * 40 + 20)
    }
    fn aq_fluid_kind(&mut self, x: i32, y: i32, z: i32, def: (i32, u8), level: i32) -> u8 {
        if level <= -10 && level != AQ_MIN && def.1 != 2 {
            let s = self.eval(
                self.aq.as_ref().unwrap().lava,
                x.div_euclid(64),
                y.div_euclid(40),
                z.div_euclid(64),
                &mut HashMap::new(),
            );
            if s.abs() > 0.3 {
                return 2;
            }
        }
        def.1
    }
    fn aq_calc_density(
        &mut self,
        x: i32,
        y: i32,
        z: i32,
        l1: (i32, u8),
        l2: (i32, u8),
        barrier: &mut Option<f64>,
    ) -> f64 {
        let b1 = Self::lvl_block(l1.0, l1.1, y);
        let b2 = Self::lvl_block(l2.0, l2.1, y);
        if (b1 == 2 && b2 == 1) || (b1 == 1 && b2 == 2) {
            return 2.0;
        }
        let diff = (l1.0 - l2.0).abs();
        if diff == 0 {
            return 0.0;
        }
        let avg = 0.5 * (l1.0 + l2.0) as f64;
        let sc = y as f64 + 0.5 - avg;
        let hd = diff as f64 / 2.0;
        let o = hd - sc.abs();
        let q = if sc > 0.0 {
            if o > 0.0 {
                o / 1.5
            } else {
                o / 2.5
            }
        } else {
            let p = 3.0 + o;
            if p > 0.0 {
                p / 3.0
            } else {
                p / 10.0
            }
        };
        let r = if (-2.0..=2.0).contains(&q) {
            match *barrier {
                Some(v) => v,
                None => {
                    let v =
                        self.eval(self.aq.as_ref().unwrap().barrier, x, y, z, &mut HashMap::new());
                    *barrier = Some(v);
                    v
                }
            }
        } else {
            0.0
        };
        2.0 * (r + q)
    }
}

/// Vanilla aquifer: fluid state at a non-solid position.
/// Some(state) = air/water/lava; None = barrier pushed it solid (material rules).
pub fn aquifer_state(x: i32, y: i32, z: i32, density: f64) -> Option<u32> {
    GEN.lock().unwrap().as_mut()?.aq_state(x, y, z, density)
}

/// The block state at a world position, taking the GEN lock once.
pub fn block_state(x: i32, y: i32, z: i32, populate: impl FnMut(&mut Gen, i32, i32)) -> Option<u32> {
    GEN.lock()
        .unwrap()
        .as_mut()
        .map(|g| g.block_state_at(x, y, z, populate))
}

/// Base terrain states for one 16x16x16 section, in the send layout
/// `(ly << 8) | (lz << 4) | lx`, with one GEN lock for the whole section.
pub fn terrain_section(
    cx: i32, sy: i32, cz: i32, mut populate: impl FnMut(&mut Gen, i32, i32),
) -> Option<Box<[u32; 4096]>> {
    let mut gg = GEN.lock().unwrap();
    let g = gg.as_mut()?;
    let mut arr = Box::new([0u32; 4096]);
    for ly in 0..16i32 {
        let y = sy + ly;
        for lz in 0..16i32 {
            for lx in 0..16i32 {
                let x = cx * 16 + lx;
                let z = cz * 16 + lz;
                arr[((ly << 8) | (lz << 4) | lx) as usize] = g.block_state_at(x, y, z, &mut populate);
            }
        }
    }
    Some(arr)
}

/// feature-layer override lookup (populates 3x3 chunks lazily on first touch)
pub fn feature_state(
    x: i32, y: i32, z: i32, populate: impl FnMut(&mut Gen, i32, i32),
) -> Option<u32> {
    GEN.lock().unwrap().as_mut()?.feat_state(x, y, z, populate)
}

impl Gen {
    pub(crate) fn aq_state(&mut self, x: i32, y: i32, z: i32, density: f64) -> Option<u32> {
    let g = self;
    if density > 0.0 {
        return None;
    }
    if g.aq.is_none() {
        return Some(if y < -54 {
            1
        } else if y < g.sea {
            g.def_fl
        } else {
            0
        });
    }
    let (water, lava) = {
        let a = g.aq.as_ref().unwrap();
        (a.water, a.lava_st)
    };
    let def = g.aq_pick(y);
    let def_blk = Gen::lvl_block(def.0, def.1, y);
    if y > g.aq_skip_above(x >> 4, z >> 4) {
        if aq_trace_at(x, z) {
            eprintln!("AQD {x} {y} {z} d={density} skip {def_blk}");
        }
        return Some(match def_blk {
            1 => water,
            2 => lava,
            _ => 0,
        });
    }
    if def_blk == 2 {
        if aq_trace_at(x, z) {
            eprintln!("AQD {x} {y} {z} d={density} lava");
        }
        return Some(lava);
    }
    g.aq_pos_trace(x, y, z);
    let near = g.aq_near4(x, y, z);
    let d = |p: (i32, i32, i32)| -> i64 {
        let (dx, dy, dz) = (p.0 - x, p.1 - y, p.2 - z);
        dx as i64 * dx as i64 + dy as i64 * dy as i64 + dz as i64 * dz as i64
    };
    let (d0, d1) = (d(near[0]), d(near[1]));
    let l2 = g.aq_water_level(near[0].0, near[0].1, near[0].2);
    let blk = Gen::lvl_block(l2.0, l2.1, y);
    let sim12 = 1.0 - (d1 - d0).abs() as f64 / 25.0;
    if aq_trace_at(x, z) {
        eprintln!(
            "AQ x={x} y={y} z={z} sim12={sim12:.4} near0={:?} lvl0={:?} near1={:?} lvl1={:?} d0={d0} d1={d1}",
            near[0], l2, near[1], g.aq_water_level(near[1].0, near[1].1, near[1].2)
        );
    }
    if sim12 <= 0.0 {
        if aq_trace_at(x, z) {
            eprintln!("AQD {x} {y} {z} d={density} sim {blk} sim12={sim12:.4} l0={l2:?}");
        }
        return Some(match blk {
            1 => water,
            2 => lava,
            _ => 0,
        });
    }
    // water directly above a lava level stays water (vanilla schedules a tick)
    let pb = g.aq_pick(y - 1);
    if blk == 1 && Gen::lvl_block(pb.0, pb.1, y - 1) == 2 {
        if aq_trace_at(x, z) {
            eprintln!("AQD {x} {y} {z} d={density} lavaunder {blk} sim12={sim12:.4} l0={l2:?}");
        }
        return Some(water);
    }
    let mut barrier = None;
    let l3 = g.aq_water_level(near[1].0, near[1].1, near[1].2);
    let c12 = g.aq_calc_density(x, y, z, l2, l3, &mut barrier);
    let (mut c13, mut c23) = (0.0f64, 0.0f64);
    if density + sim12 * c12 > 0.0 {
        if aq_trace_at(x, z) {
            eprintln!("AQD {x} {y} {z} d={density} null 12 sim={sim12:.4} c12={c12:.4} c13=0 c23=0 l0={:?} l1={:?} l2={:?}", l2, l3, (0, 0));
        }
        return None;
    }
    let l4 = g.aq_water_level(near[2].0, near[2].1, near[2].2);
    let d2 = d(near[2]);
    let sim13 = 1.0 - (d2 - d0).abs() as f64 / 25.0;
    if sim13 > 0.0 {
        c13 = g.aq_calc_density(x, y, z, l2, l4, &mut barrier);
        let b13 = sim12 * sim13 * c13;
        if density + b13 > 0.0 {
            if aq_trace_at(x, z) {
                eprintln!("AQD {x} {y} {z} d={density} null 13 sim={sim12:.4} c12={c12:.4} s13={sim13:.4} c13={c13:.4} l0={:?} l1={:?} l2={:?}", l2, l3, l4);
            }
            return None;
        }
    }
    let sim23 = 1.0 - (d2 - d1).abs() as f64 / 25.0;
    if sim23 > 0.0 {
        c23 = g.aq_calc_density(x, y, z, l3, l4, &mut barrier);
        let b23 = sim12 * sim23 * c23;
        if density + b23 > 0.0 {
            if aq_trace_at(x, z) {
                eprintln!("AQD {x} {y} {z} d={density} null 23 sim={sim12:.4} s23={sim23:.4} c23={c23:.4} l0={:?} l1={:?} l2={:?}", l2, l3, l4);
            }
            return None;
        }
    }
    if aq_trace_at(x, z) {
        eprintln!("AQD {x} {y} {z} d={density} state {blk} sim={sim12:.4} c12={c12:.4} c13={c13:.4} c23={c23:.4} l0={:?} l1={:?} l2={:?}", l2, l3, l4);
    }
    Some(match blk {
        1 => water,
        2 => lava,
        _ => 0,
    })
    }
}

// ---------- biome multi-noise tree ----------

// vanilla 1.21.1 overworld biome registry order — matches the captured
// registry_data sent to clients in config phase (alphabetical, 64 entries).
const BIOME64: [&str; 64] = [
    "badlands", "bamboo_jungle", "basalt_deltas", "beach", "birch_forest",
    "cherry_grove", "cold_ocean", "crimson_forest", "dark_forest",
    "deep_cold_ocean", "deep_dark", "deep_frozen_ocean", "deep_lukewarm_ocean",
    "deep_ocean", "desert", "dripstone_caves", "end_barrens", "end_highlands",
    "end_midlands", "eroded_badlands", "flower_forest", "forest",
    "frozen_ocean", "frozen_peaks", "frozen_river", "grove", "ice_spikes",
    "jagged_peaks", "jungle", "lukewarm_ocean", "lush_caves", "mangrove_swamp",
    "meadow", "mushroom_fields", "nether_wastes", "ocean",
    "old_growth_birch_forest", "old_growth_pine_taiga", "old_growth_spruce_taiga",
    "plains", "river", "savanna", "savanna_plateau", "small_end_islands",
    "snowy_beach", "snowy_plains", "snowy_slopes", "snowy_taiga",
    "soul_sand_valley", "sparse_jungle", "stony_peaks", "stony_shore",
    "sunflower_plains", "swamp", "taiga", "the_end", "the_void", "warm_ocean",
    "warped_forest", "windswept_forest", "windswept_gravelly_hills",
    "windswept_hills", "windswept_savanna", "wooded_badlands",
];

fn biome_id(name: &str) -> i64 {
    let n = name.rsplit(':').next().unwrap_or(name);
    BIOME64.iter().position(|&b| b == n).map(|i| i as i64).unwrap_or(39) // plains
}

#[derive(Debug)]
enum BT {
    Leaf([[i32; 2]; 7], u16),
    Sub([[i32; 2]; 7], Vec<BT>),
}

fn bt_params(j: &J) -> [[i32; 2]; 7] {
    let mut p = [[0i32; 2]; 7];
    if let Some(a) = j.get("parameters").and_then(J::into_arr) {
        for (i, e) in a.iter().take(7).enumerate() {
            p[i][0] = e.get("min").map(J::num).unwrap_or(0.0) as i32;
            p[i][1] = e.get("max").map(J::num).unwrap_or(0.0) as i32;
        }
    }
    p
}

fn bt_parse(
    j: &J,
    names: &mut Vec<String>,
    ids: &mut HashMap<String, u16>,
    leaf_ps: &mut Vec<[[i32; 2]; 7]>,
) -> Option<BT> {
    let ps = bt_params(j);
    match j.get("_type").map(J::str) {
        Some("leaf") => {
            let b = j.get("biome").map(J::str)?.to_string();
            let id = match ids.get(&b) {
                Some(&i) => i,
                None => {
                    let i = names.len() as u16;
                    names.push(b.clone());
                    ids.insert(b, i);
                    leaf_ps.push(ps);
                    i
                }
            };
            Some(BT::Leaf(ps, id))
        }
        Some("branch") => {
            let ch = j
                .get("subTree")
                .and_then(J::into_arr)?
                .iter()
                .filter_map(|c| bt_parse(c, names, ids, leaf_ps))
                .collect();
            Some(BT::Sub(ps, ch))
        }
        _ => None,
    }
}

fn ps_dist(ps: &[[i32; 2]; 7], t: &[i64; 7]) -> i64 {
    let mut d = 0i64;
    for i in 0..7 {
        let (mn, mx) = (ps[i][0] as i64, ps[i][1] as i64);
        let dd = if t[i] < mn {
            mn - t[i]
        } else if t[i] > mx {
            t[i] - mx
        } else {
            0
        };
        d += dd * dd;
    }
    d
}

impl BT {
    fn dist(&self, t: &[i64; 7]) -> i64 {
        let ps = match self {
            BT::Leaf(p, _) | BT::Sub(p, _) => p,
        };
        let mut d = 0i64;
        for i in 0..7 {
            let (mn, mx) = (ps[i][0] as i64, ps[i][1] as i64);
            let dd = if t[i] < mn {
                mn - t[i]
            } else if t[i] > mx {
                t[i] - mx
            } else {
                0
            };
            d += dd * dd;
        }
        d
    }
    fn search(&self, t: &[i64; 7], best: &mut (i64, u16)) {
        let d = self.dist(t);
        if d > best.0 {
            return;
        }
        match self {
            BT::Leaf(_, id) => {
                if d < best.0 {
                    *best = (d, *id);
                }
            }
            BT::Sub(_, ch) => {
                // vanilla visits children in ascending distance order
                let mut ordd: Vec<(i64, &BT)> =
                    ch.iter().map(|c| (c.dist(t), c)).collect();
                ordd.sort_by_key(|&(dd, _)| dd);
                for (cd, c) in ordd {
                    if cd > best.0 {
                        break;
                    }
                    c.search(t, best);
                }
            }
        }
    }
}

// 5 y-independent climate params cached per quart column:
// [temperature, vegetation, continentalness, erosion, weirdness]


impl Gen {
    fn climate_col(&mut self, qx: i32, qz: i32) -> Option<([i64; 5], f64)> {
        if let Some(&v) = self.clim_c.get(&(qx, qz)) {
            return Some(v);
        }
        let cl = self.climate?;
        let (bx, bz) = (qx << 2, qz << 2);
        let mut memo = HashMap::new();
        let mut t = [0i64; 5];
        for (i, &id) in [cl[0], cl[1], cl[2], cl[3], cl[5]].iter().enumerate() {
            t[i] = (self.eval(id, bx, 0, bz, &mut memo) * 10000.0) as i64;
        }
        // depth offset term (y-independent part of depth) when splittable
        let off = match self.depth_split {
            Some((_, _, _, _, oid)) => self.eval(oid, bx, 0, bz, &mut memo),
            None => 0.0,
        };
        self.clim_c.insert((qx, qz), (t, off));
        self.clim_ord.push((qx, qz));
        if self.clim_ord.len() > 4096 {
            let old = self.clim_ord.remove(0);
            self.clim_c.remove(&old);
        }
        Some((t, off))
    }

    /// 7-param biome target at quart coords
    fn target7(&mut self, qx: i32, qy: i32, qz: i32) -> Option<[i64; 7]> {
        let (c5, off) = self.climate_col(qx, qz)?;
        let dq = match self.depth_split {
            Some((fc, fv, tc, tv, _)) => {
                let y = (qy << 2) as f64;
                let tt = ((y - fc) / (tc - fc)).clamp(0.0, 1.0);
                ((fv + tt * (tv - fv) + off) * 10000.0) as i64
            }
            None => (self.eval(
                self.climate?[4],
                qx << 2,
                qy << 2,
                qz << 2,
                &mut HashMap::new(),
            ) * 10000.0) as i64,
        };
        Some([c5[0], c5[1], c5[2], c5[3], dq, c5[4], 0])
    }

    /// `BiomeManager.getBiome(BlockPos)` — the *blended* biome vanilla's
    /// `LevelReader.getBiome` returns, which is what `BiomeFilter` (and anything
    /// else reading the level at a position) sees. It is not the chunk's biome
    /// container: the eight neighbouring noise samples are ranked by
    /// `getFiddledDistance`, so at a biome boundary the two disagree.
    pub(crate) fn biome_blended(&mut self, x: i32, y: i32, z: i32) -> Option<(i64, u16)> {
        let zoom = crate::generation_math::biome_zoom_seed(self.world_seed);
        let (x, y, z) = (x - 2, y - 2, z - 2);
        let (bx, by, bz) = (x >> 2, y >> 2, z >> 2);
        let (fx, fy, fz) = (
            (x & 3) as f64 / 4.0,
            (y & 3) as f64 / 4.0,
            (z & 3) as f64 / 4.0,
        );
        let mut best = (0usize, f64::INFINITY);
        for i in 0..8usize {
            let (px, py, pz) = ((i & 4) == 0, (i & 2) == 0, (i & 1) == 0);
            let (qx, qy, qz) = (
                if px { bx } else { bx + 1 },
                if py { by } else { by + 1 },
                if pz { bz } else { bz + 1 },
            );
            let (ox, oy, oz) = (
                if px { fx } else { fx - 1.0 },
                if py { fy } else { fy - 1.0 },
                if pz { fz } else { fz - 1.0 },
            );
            let d = crate::generation_math::fiddled_distance(zoom, qx, qy, qz, ox, oy, oz);
            if d < best.1 {
                best = (i, d);
            }
        }
        let i = best.0;
        let (px, py, pz) = ((i & 4) == 0, (i & 2) == 0, (i & 1) == 0);
        // the winner is looked up in the noise source at its own quart corner,
        // which is exactly what this port's biome tree answers
        self.biome_quart(
            if px { bx } else { bx + 1 },
            if py { by } else { by + 1 },
            if pz { bz } else { bz + 1 },
            None,
        )
    }

    pub(crate) fn biome_quart(
        &mut self,
        qx: i32,
        qy: i32,
        qz: i32,
        last: Option<u16>,
    ) -> Option<(i64, u16)> {
        let t = self.target7(qx, qy, qz)?;
        // vanilla seeds the search with the previous result
        let mut best = (i64::MAX, 0u16);
        if let Some(l) = last {
            if let Some(ps) = self.leaf_ps.get(l as usize) {
                best = (ps_dist(ps, &t), l);
            }
        }
        self.btree.as_ref()?.search(&t, &mut best);
        let name = self.biome_names.get(best.1 as usize)?;
        Some((biome_id(name), best.1))
    }
}

/// debug: (biome_id, [t,h,c,e,d,w,0]) at quart coords
pub fn biome_debug(qx: i32, qy: i32, qz: i32) -> Option<(i64, [i64; 7])> {
    let mut gg = GEN.lock().unwrap();
    let g = gg.as_mut()?;
    let t = g.target7(qx, qy, qz)?;
    let mut best = (i64::MAX, 0u16);
    g.btree.as_ref()?.search(&t, &mut best);
    Some((biome_id(&g.biome_names[best.1 as usize].clone()), t))
}

/// quart-space biome id (public for debug/tests)
pub fn biome_at(qx: i32, qy: i32, qz: i32) -> Option<i64> {
    let mut gg = GEN.lock().unwrap();
    gg.as_mut()?.biome_quart(qx, qy, qz, None).map(|(b, _)| b)
}

/// 4x4x4 biome ids for one chunk section, quart-space index (y<<4|z<<2|x).
/// `MC_SPINE_HM_DUMP=cx,cz`: print the OCEAN_FLOOR and WORLD_SURFACE height of
/// every column, to compare against a saved vanilla chunk's Heightmaps.
pub fn hm_dump(spec: &str) {
    let p: Vec<i32> = spec.split(',').filter_map(|n| n.parse().ok()).collect();
    if p.len() != 2 {
        return;
    }
    let mut gg = GEN.lock().unwrap();
    let Some(g) = gg.as_mut() else { return };
    let (cx, cz) = (p[0], p[1]);
    // the feature layer's height a *running* world would hold only exists once the
    // chunks around the column have been populated; reading the tally before that
    // returns terrain heights and silently "proves" a difference
    g.ensure_populated(cx, cz);
    let Some(c) = g.cols(cx, cz) else { return };
    for lz in 0..16usize {
        for lx in 0..16usize {
            let (x, z) = (cx * 16 + lx as i32, cz * 16 + lz as i32);
            // include the feature layer: this dump exists to compare against a saved
            // chunk's heightmaps, which a running server has already had raised by
            // the canopy its own placements grew
            let of = g.top_oc(x, z).max(g.feature_height(x, z, 1));
            let ws = g.top_ws(x, z).max(g.feature_height(x, z, 0));
            eprintln!("HM {} {} {} {}", lx, lz, of, ws);
        }
    }
}

/// `MC_SPINE_BIOME_AT=x,y,z;x,y,z;...`: print both biome notions at each position.
/// The container (what a saved chunk holds) and the blended lookup that
/// `LevelReader.getBiome` returns are different answers; a placement filter reads
/// the second, so seeing both at once is how a boundary disagreement is read.
pub fn biome_at_dump(spec: &str) {
    let mut gg = GEN.lock().unwrap();
    let Some(g) = gg.as_mut() else { return };
    for item in spec.split(';') {
        let p: Vec<i32> = item.split(',').filter_map(|n| n.parse().ok()).collect();
        if p.len() != 3 {
            continue;
        }
        let (x, y, z) = (p[0], p[1], p[2]);
        let blended = g.biome_blended(x, y, z).and_then(|(_, l)| g.biome_name_of_leaf(l));
        let container = match g.biome_quart(x >> 2, y >> 2, z >> 2, None) {
            Some((_, l)) => g.biome_name_of_leaf(l),
            None => None,
        };
        eprintln!(
            "BIOMEAT {x} {y} {z} blended={} container={}",
            blended.unwrap_or_else(|| "?".into()),
            container.unwrap_or_else(|| "?".into())
        );
    }
}

/// `MC_SPINE_BIOME_DUMP=cx,sy,cz`: print the quart-sampled biome name for every
/// 4x4x4 cell of one section, to compare against a saved vanilla chunk.
pub fn biome_dump(spec: &str) {
    let p: Vec<i32> = spec.split(',').filter_map(|n| n.parse().ok()).collect();
    if p.len() != 3 {
        return;
    }
    let mut gg = GEN.lock().unwrap();
    let Some(g) = gg.as_mut() else { return };
    let (cx, sy, cz) = (p[0], p[1], p[2]);
    let (qx0, qy0, qz0) = (cx * 4, sy >> 2, cz * 4);
    let mut last: Option<u16> = None;
    for ly in 0..4 {
        for lz in 0..4 {
            for lx in 0..4 {
                if let Some((_, lid)) = g.biome_quart(qx0 + lx, qy0 + ly, qz0 + lz, last) {
                    last = Some(lid);
                    let name = g
                        .biome_names
                        .get(lid as usize)
                        .cloned()
                        .unwrap_or_else(|| format!("#{lid}"));
                    eprintln!(
                        "BDUMP {} {} {} {}",
                        (qx0 + lx) * 4, (qy0 + ly) * 4, (qz0 + lz) * 4, name
                    );
                }
            }
        }
    }
}

pub fn biome_section(cx: i32, sy: i32, cz: i32) -> Option<Box<[i64; 64]>> {
    let mut gg = GEN.lock().unwrap();
    let g = gg.as_mut()?;
    let mut out = Box::new([39i64; 64]); // plains
    let (qx0, qy0, qz0) = (cx * 4, sy >> 2, cz * 4);
    let mut last: Option<u16> = None;
    for ly in 0..4 {
        for lz in 0..4 {
            for lx in 0..4 {
                if let Some((b, lid)) = g.biome_quart(qx0 + lx, qy0 + ly, qz0 + lz, last) {
                    out[((ly << 4) | (lz << 2) | lx) as usize] = b;
                    last = Some(lid);
                }
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod wg_tests {
    use super::*;

    // These tests mutate the shared `GEN` static, so they must not run in
    // parallel: one lock per test serializes the whole module.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn lock_gen() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap()
    }
    #[test]
    fn biome_vectors() {
        let _guard = lock_gen();
        // pumpkin multi_noise_sampler tests: seed, quart pos, params
        fn stub(_: &str) -> Option<u32> { Some(1) }
        assert!(init(123, stub));
        let (b, t) = biome_debug(123, 123, 123).unwrap();
        assert_eq!(t, [-5727, 55, 4996, 2371, -19774, 4421, 0]);
        assert_eq!(b, 47); // snowy_taiga
        assert!(init(13579, stub));
        let (b, t) = biome_debug(123, 123, 123).unwrap();
        assert_eq!(t, [7489, 3502, -2168, -3511, -21237, -5222, 0]);
        assert_eq!(b, 57); // warm_ocean
    }

    #[test]
    fn material_rules_load() {
        let _guard = lock_gen();
        fn stub(_: &str) -> Option<u32> { Some(1) }
        assert!(init(0, stub));
        let mut gg = GEN.lock().unwrap();
        let g = gg.as_mut().unwrap();
        println!("surface_rule={:?} conds={} rules={} mats_loaded",
            g.surface_rule, g.conds.len(), g.rules.len());
        assert!(g.surface_rule.is_some(), "material_rule not resolved");
        assert!(!g.conds.is_empty() && g.rules.len() > 5);
        drop(gg);
        // real state ids: grass/dirt/stone or sand should appear in a
        // plains-ish spawn column; "no match" (u32::MAX->default) is stone.
        let cc = chunk_cols(0, 0).unwrap();
        let mut kinds: HashMap<u32, u32> = HashMap::new();
        let mut nmat = 0;
        for lz in 0..16 {
            for lx in 0..16 {
                if let Some(m) = &cc.mats[lz * 16 + lx] {
                    nmat += 1;
                    for &v in m.vals.iter() {
                        *kinds.entry(v as u32).or_default() += 1;
                    }
                }
            }
        }
        println!("cols_with_mats={} kinds={:?}", nmat, kinds);
        assert!(nmat > 0, "no material mats built");
    }

    #[test]
    fn material_rules_real_ids() {
        let _guard = lock_gen();
        assert!(init(0, crate::name_to_state));
        let cc = chunk_cols(0, 0).unwrap();
        let mut kinds: HashMap<u32, u32> = HashMap::new();
        for lz in 0..16 {
            for lx in 0..16 {
                if let Some(m) = &cc.mats[lz * 16 + lx] {
                    for &v in m.vals.iter() {
                        *kinds.entry(v as u32).or_default() += 1;
                    }
                }
            }
        }
        println!("real-id kinds={:?}", kinds);
        // at least one real surface block resolved (grass=9, dirt=10,
        // sand=112, gravel=118) or all-default (ocean column)
        let surfaced = kinds.keys().any(|&k| [9u32, 10, 112, 118].contains(&k));
        assert!(surfaced || kinds.len() <= 2, "no surface mats resolved");
    }
// pumpkin aquifer_sampler.rs vectors (GlobalRandomConfig seed 0, chunk 7,4)
const AQ_NB: &[((i32,i32,i32),i32)] = &[((-100,-100,-100),-103), ((-100,-100,-50),-103), ((-100,-100,0),-103), ((-100,-100,50),-103), ((-100,-100,100),-103), ((-100,-50,-100),-63), ((-100,-50,-50),-63), ((-100,-50,0),-63), ((-100,-50,50),-63), ((-100,-50,100),-63), ((-100,0,-100),17), ((-100,0,-50),17), ((-100,0,0),17), ((-100,0,50),17), ((-100,0,100),17), ((-100,50,-100),57), ((-100,50,-50),57), ((-100,50,0),57), ((-100,50,50),57), ((-100,50,100),57), ((-100,100,-100),97), ((-100,100,-50),97), ((-100,100,0),97), ((-100,100,50),97), ((-100,100,100),97), ((-50,-100,-100),-103), ((-50,-100,-50),-103), ((-50,-100,0),-103), ((-50,-100,50),-103), ((-50,-100,100),-100), ((-50,-50,-100),-63), ((-50,-50,-50),-63), ((-50,-50,0),-63), ((-50,-50,50),-63), ((-50,-50,100),-60), ((-50,0,-100),17), ((-50,0,-50),17), ((-50,0,0),17), ((-50,0,50),17), ((-50,0,100),20), ((-50,50,-100),57), ((-50,50,-50),57), ((-50,50,0),57), ((-50,50,50),57), ((-50,50,100),60), ((-50,100,-100),97), ((-50,100,-50),97), ((-50,100,0),97), ((-50,100,50),97), ((-50,100,100),100), ((0,-100,-100),-103), ((0,-100,-50),-103), ((0,-100,0),-103), ((0,-100,50),-100), ((0,-100,100),-100), ((0,-50,-100),-63), ((0,-50,-50),-63), ((0,-50,0),-63), ((0,-50,50),-60), ((0,-50,100),-60), ((0,0,-100),17), ((0,0,-50),17), ((0,0,0),17), ((0,0,50),20), ((0,0,100),20), ((0,50,-100),57), ((0,50,-50),57), ((0,50,0),57), ((0,50,50),60), ((0,50,100),60), ((0,100,-100),97), ((0,100,-50),97), ((0,100,0),97), ((0,100,50),100), ((0,100,100),100), ((50,-100,-100),-103), ((50,-100,-50),-103), ((50,-100,0),-103), ((50,-100,50),-100), ((50,-100,100),-100), ((50,-50,-100),-63), ((50,-50,-50),-63), ((50,-50,0),-63), ((50,-50,50),-60), ((50,-50,100),-60), ((50,0,-100),17), ((50,0,-50),17), ((50,0,0),17), ((50,0,50),20), ((50,0,100),20), ((50,50,-100),57), ((50,50,-50),57), ((50,50,0),57), ((50,50,50),60), ((50,50,100),60), ((50,100,-100),97), ((50,100,-50),97), ((50,100,0),97), ((50,100,50),100), ((50,100,100),100), ((100,-100,-100),-103), ((100,-100,-50),-103), ((100,-100,0),-103), ((100,-100,50),-103), ((100,-100,100),-100), ((100,-50,-100),-63), ((100,-50,-50),-63), ((100,-50,0),-63), ((100,-50,50),-63), ((100,-50,100),-60), ((100,0,-100),17), ((100,0,-50),17), ((100,0,0),17), ((100,0,50),17), ((100,0,100),20), ((100,50,-100),57), ((100,50,-50),57), ((100,50,0),57), ((100,50,50),57), ((100,50,100),60), ((100,100,-100),97), ((100,100,-50),97), ((100,100,0),97), ((100,100,50),97), ((100,100,100),100)];
const AQ_FYT: &[((i32,i32,i32),i32)] = &[((-100,-100,-100),-32512), ((-100,-100,-50),-32512), ((-100,-100,0),-32512), ((-100,-100,50),-32512), ((-100,-100,100),-32512), ((-100,-50,-100),-32512), ((-100,-50,-50),-63), ((-100,-50,0),-63), ((-100,-50,50),-63), ((-100,-50,100),-32512), ((-100,0,-100),-32512), ((-100,0,-50),-32512), ((-100,0,0),-32512), ((-100,0,50),-32512), ((-100,0,100),-32512), ((-100,50,-100),57), ((-100,50,-50),57), ((-100,50,0),57), ((-100,50,50),57), ((-100,50,100),57), ((-100,100,-100),80), ((-100,100,-50),0), ((-100,100,0),0), ((-100,100,50),0), ((-100,100,100),0), ((-50,-100,-100),-32512), ((-50,-100,-50),-32512), ((-50,-100,0),-32512), ((-50,-100,50),-32512), ((-50,-100,100),-32512), ((-50,-50,-100),-32512), ((-50,-50,-50),-32512), ((-50,-50,0),-32512), ((-50,-50,50),-32512), ((-50,-50,100),-32512), ((-50,0,-100),-32512), ((-50,0,-50),-32512), ((-50,0,0),-32512), ((-50,0,50),-32512), ((-50,0,100),-32512), ((-50,50,-100),-32512), ((-50,50,-50),-32512), ((-50,50,0),57), ((-50,50,50),57), ((-50,50,100),60), ((-50,100,-100),80), ((-50,100,-50),0), ((-50,100,0),0), ((-50,100,50),0), ((-50,100,100),0), ((0,-100,-100),-32512), ((0,-100,-50),-32512), ((0,-100,0),-32512), ((0,-100,50),-32512), ((0,-100,100),-32512), ((0,-50,-100),-32512), ((0,-50,-50),-32512), ((0,-50,0),-32512), ((0,-50,50),-32512), ((0,-50,100),-32512), ((0,0,-100),-32512), ((0,0,-50),-32512), ((0,0,0),-32512), ((0,0,50),-32512), ((0,0,100),-32512), ((0,50,-100),-32512), ((0,50,-50),-32512), ((0,50,0),57), ((0,50,50),60), ((0,50,100),60), ((0,100,-100),0), ((0,100,-50),0), ((0,100,0),0), ((0,100,50),0), ((0,100,100),0), ((50,-100,-100),-32512), ((50,-100,-50),-32512), ((50,-100,0),-32512), ((50,-100,50),-32512), ((50,-100,100),-100), ((50,-50,-100),-32512), ((50,-50,-50),-32512), ((50,-50,0),-32512), ((50,-50,50),-32512), ((50,-50,100),-60), ((50,0,-100),-32512), ((50,0,-50),-32512), ((50,0,0),-32512), ((50,0,50),-32512), ((50,0,100),-32512), ((50,50,-100),-32512), ((50,50,-50),-32512), ((50,50,0),57), ((50,50,50),60), ((50,50,100),60), ((50,100,-100),0), ((50,100,-50),0), ((50,100,0),0), ((50,100,50),0), ((50,100,100),0), ((100,-100,-100),-32512), ((100,-100,-50),-32512), ((100,-100,0),-32512), ((100,-100,50),-103), ((100,-100,100),-100), ((100,-50,-100),-32512), ((100,-50,-50),-32512), ((100,-50,0),-32512), ((100,-50,50),-32512), ((100,-50,100),-60), ((100,0,-100),-32512), ((100,0,-50),-32512), ((100,0,0),-32512), ((100,0,50),-32512), ((100,0,100),-32512), ((100,50,-100),-32512), ((100,50,-50),-32512), ((100,50,0),57), ((100,50,50),57), ((100,50,100),60), ((100,100,-100),0), ((100,100,-50),0), ((100,100,0),0), ((100,100,50),0), ((100,100,100),0)];
const AQ_FYF: &[((i32,i32,i32),i32)] = &[((-100,-100,-100),-32512), ((-100,-100,-50),-32512), ((-100,-100,0),-32512), ((-100,-100,50),-32512), ((-100,-100,100),-32512), ((-100,-50,-100),-32512), ((-100,-50,-50),-63), ((-100,-50,0),-63), ((-100,-50,50),-63), ((-100,-50,100),-32512), ((-100,0,-100),-32512), ((-100,0,-50),-32512), ((-100,0,0),-32512), ((-100,0,50),-32512), ((-100,0,100),-32512), ((-100,50,-100),-32512), ((-100,50,-50),-32512), ((-100,50,0),-32512), ((-100,50,50),-32512), ((-100,50,100),-32512), ((-100,100,-100),-32512), ((-100,100,-50),-32512), ((-100,100,0),-32512), ((-100,100,50),-32512), ((-100,100,100),-32512), ((-50,-100,-100),-32512), ((-50,-100,-50),-32512), ((-50,-100,0),-32512), ((-50,-100,50),-32512), ((-50,-100,100),-32512), ((-50,-50,-100),-32512), ((-50,-50,-50),-32512), ((-50,-50,0),-32512), ((-50,-50,50),-32512), ((-50,-50,100),-32512), ((-50,0,-100),-32512), ((-50,0,-50),-32512), ((-50,0,0),-32512), ((-50,0,50),-32512), ((-50,0,100),-32512), ((-50,50,-100),-32512), ((-50,50,-50),-32512), ((-50,50,0),-32512), ((-50,50,50),-32512), ((-50,50,100),-32512), ((-50,100,-100),-32512), ((-50,100,-50),-32512), ((-50,100,0),80), ((-50,100,50),-32512), ((-50,100,100),-32512), ((0,-100,-100),-32512), ((0,-100,-50),-32512), ((0,-100,0),-32512), ((0,-100,50),-32512), ((0,-100,100),-32512), ((0,-50,-100),-32512), ((0,-50,-50),-32512), ((0,-50,0),-32512), ((0,-50,50),-32512), ((0,-50,100),-32512), ((0,0,-100),-32512), ((0,0,-50),-32512), ((0,0,0),-32512), ((0,0,50),-32512), ((0,0,100),-32512), ((0,50,-100),-32512), ((0,50,-50),-32512), ((0,50,0),-32512), ((0,50,50),-32512), ((0,50,100),-32512), ((0,100,-100),-32512), ((0,100,-50),-32512), ((0,100,0),80), ((0,100,50),-32512), ((0,100,100),-32512), ((50,-100,-100),-32512), ((50,-100,-50),-32512), ((50,-100,0),-32512), ((50,-100,50),-32512), ((50,-100,100),-100), ((50,-50,-100),-32512), ((50,-50,-50),-32512), ((50,-50,0),-32512), ((50,-50,50),-32512), ((50,-50,100),-60), ((50,0,-100),-32512), ((50,0,-50),-32512), ((50,0,0),-32512), ((50,0,50),-32512), ((50,0,100),-32512), ((50,50,-100),-32512), ((50,50,-50),-32512), ((50,50,0),-32512), ((50,50,50),-32512), ((50,50,100),-32512), ((50,100,-100),-32512), ((50,100,-50),-32512), ((50,100,0),80), ((50,100,50),-32512), ((50,100,100),-32512), ((100,-100,-100),-32512), ((100,-100,-50),-32512), ((100,-100,0),-32512), ((100,-100,50),-103), ((100,-100,100),-100), ((100,-50,-100),-32512), ((100,-50,-50),-32512), ((100,-50,0),-32512), ((100,-50,50),-32512), ((100,-50,100),-60), ((100,0,-100),-32512), ((100,0,-50),-32512), ((100,0,0),-32512), ((100,0,50),-32512), ((100,0,100),-32512), ((100,50,-100),-32512), ((100,50,-50),-32512), ((100,50,0),-32512), ((100,50,50),-32512), ((100,50,100),-32512), ((100,100,-100),-32512), ((100,100,-50),-32512), ((100,100,0),80), ((100,100,50),-32512), ((100,100,100),-32512)];
const AQ_FBS: &[((i32,i32,i32),u8)] = &[((-100,-100,-100),1), ((-100,-100,-50),2), ((-100,-100,0),1), ((-100,-100,50),1), ((-100,-100,100),1), ((-100,-50,-100),1), ((-100,-50,-50),2), ((-100,-50,0),2), ((-100,-50,50),2), ((-100,-50,100),1), ((-100,0,-100),1), ((-100,0,-50),1), ((-100,0,0),1), ((-100,0,50),1), ((-100,0,100),1), ((-100,50,-100),1), ((-100,50,-50),1), ((-100,50,0),1), ((-100,50,50),1), ((-100,50,100),1), ((-100,100,-100),1), ((-100,100,-50),1), ((-100,100,0),1), ((-100,100,50),1), ((-100,100,100),1), ((-50,-100,-100),1), ((-50,-100,-50),1), ((-50,-100,0),2), ((-50,-100,50),2), ((-50,-100,100),1), ((-50,-50,-100),1), ((-50,-50,-50),1), ((-50,-50,0),1), ((-50,-50,50),1), ((-50,-50,100),1), ((-50,0,-100),2), ((-50,0,-50),1), ((-50,0,0),1), ((-50,0,50),1), ((-50,0,100),1), ((-50,50,-100),1), ((-50,50,-50),1), ((-50,50,0),2), ((-50,50,50),2), ((-50,50,100),1), ((-50,100,-100),1), ((-50,100,-50),1), ((-50,100,0),2), ((-50,100,50),2), ((-50,100,100),2), ((0,-100,-100),1), ((0,-100,-50),2), ((0,-100,0),2), ((0,-100,50),2), ((0,-100,100),1), ((0,-50,-100),1), ((0,-50,-50),1), ((0,-50,0),1), ((0,-50,50),1), ((0,-50,100),1), ((0,0,-100),2), ((0,0,-50),2), ((0,0,0),1), ((0,0,50),1), ((0,0,100),1), ((0,50,-100),1), ((0,50,-50),1), ((0,50,0),1), ((0,50,50),1), ((0,50,100),1), ((0,100,-100),1), ((0,100,-50),2), ((0,100,0),1), ((0,100,50),1), ((0,100,100),1), ((50,-100,-100),1), ((50,-100,-50),2), ((50,-100,0),2), ((50,-100,50),2), ((50,-100,100),1), ((50,-50,-100),1), ((50,-50,-50),1), ((50,-50,0),1), ((50,-50,50),1), ((50,-50,100),1), ((50,0,-100),2), ((50,0,-50),2), ((50,0,0),1), ((50,0,50),1), ((50,0,100),1), ((50,50,-100),1), ((50,50,-50),1), ((50,50,0),1), ((50,50,50),1), ((50,50,100),1), ((50,100,-100),1), ((50,100,-50),2), ((50,100,0),1), ((50,100,50),1), ((50,100,100),1), ((100,-100,-100),1), ((100,-100,-50),2), ((100,-100,0),1), ((100,-100,50),1), ((100,-100,100),1), ((100,-50,-100),2), ((100,-50,-50),2), ((100,-50,0),2), ((100,-50,50),2), ((100,-50,100),2), ((100,0,-100),1), ((100,0,-50),2), ((100,0,0),1), ((100,0,50),1), ((100,0,100),2), ((100,50,-100),1), ((100,50,-50),1), ((100,50,0),1), ((100,50,50),1), ((100,50,100),1), ((100,100,-100),2), ((100,100,-50),2), ((100,100,0),1), ((100,100,50),1), ((100,100,100),1)];
const AQ_FL: &[((i32,i32,i32),(i32,u8))] = &[((-100,-100,-100),(-32512,2)), ((-100,-100,-50),(-32512,2)), ((-100,-100,0),(-32512,2)), ((-100,-100,50),(-32512,2)), ((-100,-100,100),(-32512,2)), ((-100,-50,-100),(-32512,1)), ((-100,-50,-50),(-63,2)), ((-100,-50,0),(-63,2)), ((-100,-50,50),(-63,2)), ((-100,-50,100),(-32512,1)), ((-100,0,-100),(-32512,1)), ((-100,0,-50),(-32512,1)), ((-100,0,0),(-32512,1)), ((-100,0,50),(-32512,1)), ((-100,0,100),(-32512,1)), ((-100,50,-100),(-32512,1)), ((-100,50,-50),(63,1)), ((-100,50,0),(63,1)), ((-100,50,50),(-32512,1)), ((-100,50,100),(63,1)), ((-100,100,-100),(63,1)), ((-100,100,-50),(63,1)), ((-100,100,0),(63,1)), ((-100,100,50),(63,1)), ((-100,100,100),(63,1)), ((-50,-100,-100),(-32512,2)), ((-50,-100,-50),(-32512,2)), ((-50,-100,0),(-32512,2)), ((-50,-100,50),(-32512,2)), ((-50,-100,100),(-32512,2)), ((-50,-50,-100),(-32512,1)), ((-50,-50,-50),(-32512,1)), ((-50,-50,0),(-32512,1)), ((-50,-50,50),(-32512,1)), ((-50,-50,100),(-32512,1)), ((-50,0,-100),(-32512,1)), ((-50,0,-50),(-32512,1)), ((-50,0,0),(-32512,1)), ((-50,0,50),(-32512,1)), ((-50,0,100),(-32512,1)), ((-50,50,-100),(-32512,1)), ((-50,50,-50),(63,1)), ((-50,50,0),(63,1)), ((-50,50,50),(-32512,1)), ((-50,50,100),(63,1)), ((-50,100,-100),(63,1)), ((-50,100,-50),(63,1)), ((-50,100,0),(63,1)), ((-50,100,50),(63,1)), ((-50,100,100),(63,1)), ((0,-100,-100),(-32512,2)), ((0,-100,-50),(-32512,2)), ((0,-100,0),(-32512,2)), ((0,-100,50),(-32512,2)), ((0,-100,100),(-32512,2)), ((0,-50,-100),(-32512,1)), ((0,-50,-50),(-32512,1)), ((0,-50,0),(-32512,1)), ((0,-50,50),(-32512,1)), ((0,-50,100),(-32512,1)), ((0,0,-100),(-32512,1)), ((0,0,-50),(-32512,1)), ((0,0,0),(-32512,1)), ((0,0,50),(-32512,1)), ((0,0,100),(-32512,1)), ((0,50,-100),(-32512,1)), ((0,50,-50),(63,1)), ((0,50,0),(63,1)), ((0,50,50),(-32512,1)), ((0,50,100),(-32512,1)), ((0,100,-100),(63,1)), ((0,100,-50),(63,1)), ((0,100,0),(63,1)), ((0,100,50),(63,1)), ((0,100,100),(63,1)), ((50,-100,-100),(-32512,2)), ((50,-100,-50),(-32512,2)), ((50,-100,0),(-32512,2)), ((50,-100,50),(-32512,2)), ((50,-100,100),(-100,2)), ((50,-50,-100),(-32512,1)), ((50,-50,-50),(-32512,1)), ((50,-50,0),(-32512,1)), ((50,-50,50),(-32512,1)), ((50,-50,100),(-60,1)), ((50,0,-100),(-32512,1)), ((50,0,-50),(-32512,1)), ((50,0,0),(-32512,1)), ((50,0,50),(-32512,1)), ((50,0,100),(-32512,1)), ((50,50,-100),(-32512,1)), ((50,50,-50),(63,1)), ((50,50,0),(63,1)), ((50,50,50),(63,1)), ((50,50,100),(-32512,1)), ((50,100,-100),(-32512,1)), ((50,100,-50),(63,1)), ((50,100,0),(63,1)), ((50,100,50),(63,1)), ((50,100,100),(63,1)), ((100,-100,-100),(-32512,2)), ((100,-100,-50),(-32512,2)), ((100,-100,0),(-32512,2)), ((100,-100,50),(-103,2)), ((100,-100,100),(-100,2)), ((100,-50,-100),(-32512,1)), ((100,-50,-50),(-32512,1)), ((100,-50,0),(-32512,1)), ((100,-50,50),(-32512,1)), ((100,-50,100),(-60,2)), ((100,0,-100),(-32512,1)), ((100,0,-50),(-32512,1)), ((100,0,0),(-32512,1)), ((100,0,50),(-32512,1)), ((100,0,100),(-32512,1)), ((100,50,-100),(63,1)), ((100,50,-50),(63,1)), ((100,50,0),(63,1)), ((100,50,50),(63,1)), ((100,50,100),(-32512,1)), ((100,100,-100),(63,1)), ((100,100,-50),(63,1)), ((100,100,0),(63,1)), ((100,100,50),(63,1)), ((100,100,100),(63,1))];
const AQ_CD: &[((i32,i32,i32,i32,i32),f64)] = &[((-100,-100,-100,0,0),0.0), ((-100,-100,-50,50,0),-19.3), ((-100,-100,0,0,0),0.0), ((-100,-100,50,50,0),-19.3), ((-100,-100,100,0,0),0.0), ((-100,-50,-100,0,0),0.0), ((-100,-50,-50,50,0),-9.3), ((-100,-50,0,0,0),0.0), ((-100,-50,50,50,0),-9.3), ((-100,-50,100,0,0),0.0), ((-100,0,-100,0,0),0.0), ((-100,0,-50,-50,0),0.20838508), ((-100,0,0,0,0),0.0), ((-100,0,50,50,0),2.069189), ((-100,0,100,0,0),0.0), ((-100,50,-100,0,0),0.0), ((-100,50,-50,-50,0),-40.4), ((-100,50,0,0,0),0.0), ((-100,50,50,-50,0),-40.4), ((-100,50,100,0,0),0.0), ((-100,100,-100,0,0),0.0), ((-100,100,-50,-50,0),-80.4), ((-100,100,0,0,0),0.0), ((-100,100,50,-50,0),-80.4), ((-100,100,100,0,0),0.0), ((-50,-100,-100,0,50),-19.3), ((-50,-100,-50,50,50),0.0), ((-50,-100,0,0,-50),-9.3), ((-50,-100,50,50,-50),-9.3), ((-50,-100,100,0,-50),-9.3), ((-50,-50,-100,0,50),-9.3), ((-50,-50,-50,50,50),0.0), ((-50,-50,0,0,-50),2.2042949518442185), ((-50,-50,50,50,-50),1.8767275908406176), ((-50,-50,100,0,-50),2.3399656359995133), ((-50,0,-100,0,-50),-0.08405951), ((-50,0,-50,-50,-50),0.0), ((-50,0,0,0,-50),0.3902410585192353), ((-50,0,50,50,-50),66.0), ((-50,0,100,0,-50),-0.7930165090675787), ((-50,50,-100,0,-50),-40.4), ((-50,50,-50,-50,-50),0.0), ((-50,50,0,0,-50),-40.4), ((-50,50,50,-50,50),-0.35570822400215646), ((-50,50,100,0,50),-0.16770224497207317), ((-50,100,-100,0,-50),-80.4), ((-50,100,-50,-50,-50),0.0), ((-50,100,0,0,-50),-80.4), ((-50,100,50,-50,50),-40.4), ((-50,100,100,0,50),-40.4), ((0,-100,-100,0,0),0.0), ((0,-100,-50,-50,0),-9.3), ((0,-100,0,0,0),0.0), ((0,-100,50,50,0),-19.3), ((0,-100,100,0,0),0.0), ((0,-50,-100,0,0),0.0), ((0,-50,-50,-50,0),2.857141340264507), ((0,-50,0,0,0),0.0), ((0,-50,50,50,0),-9.3), ((0,-50,100,0,0),0.0), ((0,0,-100,0,0),0.0), ((0,0,-50,-50,0),-0.1361016501707068), ((0,0,0,0,0),0.0), ((0,0,50,50,0),1.9841279541408636), ((0,0,100,0,0),0.0), ((0,50,-100,0,0),0.0), ((0,50,-50,-50,0),-40.4), ((0,50,0,0,0),0.0), ((0,50,50,50,0),-0.36331007530382964), ((0,50,100,0,0),0.0), ((0,100,-100,0,0),0.0), ((0,100,-50,-50,0),-80.4), ((0,100,0,0,0),0.0), ((0,100,50,50,0),-40.4), ((0,100,100,0,0),0.0), ((50,-100,-100,0,50),-19.3), ((50,-100,-50,-50,50),-9.3), ((50,-100,0,0,50),-19.3), ((50,-100,50,-50,-50),0.0), ((50,-100,100,0,-50),-9.3), ((50,-50,-100,0,50),-9.3), ((50,-50,-50,-50,50),1.619242225388449), ((50,-50,0,0,50),-9.3), ((50,-50,50,-50,-50),0.0), ((50,-50,100,0,-50),2.1561171703198188), ((50,0,-100,0,50),2.6298865590685954), ((50,0,-50,-50,50),66.0), ((50,0,0,0,50),2.572198917833846), ((50,0,50,50,50),0.0), ((50,0,100,0,50),2.082884998883258), ((50,50,-100,0,-50),-40.4), ((50,50,-50,50,-50),-0.1894344852785401), ((50,50,0,0,50),-0.7155260733519367), ((50,50,50,50,50),0.0), ((50,50,100,0,50),-0.4132183490530098), ((50,100,-100,0,-50),-80.4), ((50,100,-50,50,-50),-40.4), ((50,100,0,0,50),-40.4), ((50,100,50,50,50),0.0), ((50,100,100,0,50),-40.4), ((100,-100,-100,0,0),0.0), ((100,-100,-50,-50,0),-9.3), ((100,-100,0,0,0),0.0), ((100,-100,50,-50,0),-9.3), ((100,-100,100,0,0),0.0), ((100,-50,-100,0,0),0.0), ((100,-50,-50,-50,0),1.6711026207576742), ((100,-50,0,0,0),0.0), ((100,-50,50,-50,0),2.042353012197518), ((100,-50,100,0,0),0.0), ((100,0,-100,0,0),0.0), ((100,0,-50,-50,0),0.3145492757856567), ((100,0,0,0,0),0.0), ((100,0,50,50,0),2.27260703684609), ((100,0,100,0,0),0.0), ((100,50,-100,0,0),0.0), ((100,50,-50,50,0),-0.16949328993376553), ((100,50,0,0,0),0.0), ((100,50,50,50,0),0.5196380801381327), ((100,50,100,0,0),0.0), ((100,100,-100,0,0),0.0), ((100,100,-50,50,0),-40.4), ((100,100,0,0,0),0.0), ((100,100,50,50,0),-40.4), ((100,100,100,0,0),0.0)];
const AQ_AP: &[((i32,i32,i32,f64),&str)] = &[((112,-100,64,0.037482421875),"none"), ((112,-100,66,0.037482421875),"none"), ((112,-100,68,0.037482421875),"none"), ((112,-100,70,0.037482421875),"none"), ((112,-100,72,0.037482421875),"none"), ((112,-100,74,0.037482421875),"none"), ((112,-80,64,0.037482421875),"none"), ((112,-80,66,0.037482421875),"none"), ((112,-80,68,0.037482421875),"none"), ((112,-80,70,0.037482421875),"none"), ((112,-80,72,0.037482421875),"none"), ((112,-80,74,0.037482421875),"none"), ((112,-60,64,0.04861063447117113),"none"), ((112,-60,66,0.04924175767418443),"none"), ((112,-60,68,0.04989611436947345),"none"), ((112,-60,70,0.0505756649980066),"none"), ((112,-60,72,0.051280297037227154),"none"), ((112,-60,74,0.05200473277817172),"none"), ((112,-40,64,0.10768778489063564),"none"), ((112,-40,66,0.11167582884587932),"none"), ((112,-40,68,0.1150242394949102),"none"), ((112,-40,70,0.11846243756482717),"none"), ((112,-40,72,0.12198087391634192),"none"), ((112,-40,74,0.12558796524209848),"none"), ((112,-20,64,0.10797485850904144),"none"), ((112,-20,66,0.10725848123542484),"none"), ((112,-20,68,0.1061187765379769),"none"), ((112,-20,70,0.10630527141203541),"none"), ((112,-20,72,0.10955530075188523),"none"), ((112,-20,74,0.11296050502041746),"none"), ((112,0,64,-0.0016242211141160837),"none"), ((112,0,68,0.0013878562706830444),"none"), ((112,0,70,0.0030297756784707407),"none"), ((112,0,72,0.004722310162617811),"none"), ((112,0,74,0.0064699315064971315),"none"), ((112,20,64,0.042774410930765096),"none"), ((112,20,66,0.040191327400772414),"none"), ((112,20,68,0.0375637494138927),"none"), ((112,20,70,0.03485919313914573),"none"), ((112,20,72,0.032058901444915404),"none"), ((112,20,74,0.029170651813548235),"none"), ((112,40,64,0.011596925999172775),"none"), ((112,40,66,0.014957019593043965),"none"), ((112,40,68,0.01827627643898028),"none"), ((112,40,70,0.021477214738188897),"none"), ((112,40,72,0.02448508577325224),"none"), ((112,40,74,0.027262647782191486),"none"), ((112,60,64,0.14338446306313316),"none"), ((112,60,66,0.16772904485726645),"none"), ((112,60,68,0.1756309873589998),"none"), ((112,60,70,0.1782686032102433),"none"), ((112,60,72,0.18822148746793055),"none"), ((112,60,74,0.20387997189913717),"none"), ((112,80,64,-0.28931054817132484),"air"), ((112,80,66,-0.2808098154769529),"air"), ((112,80,68,-0.2806908647477032),"air"), ((112,80,70,-0.28068300576359284),"air"), ((112,80,72,-0.2805878392398348),"air"), ((112,80,74,-0.27824504138444317),"air"), ((112,100,64,-0.4583333333333333),"air"), ((112,100,66,-0.4583333333333333),"air"), ((112,100,68,-0.4583333333333333),"air"), ((112,100,70,-0.4583333333333333),"air"), ((112,100,72,-0.4583333333333333),"air"), ((112,100,74,-0.4583333333333333),"air"), ((114,-100,64,0.037482421875),"none"), ((114,-100,66,0.037482421875),"none"), ((114,-100,68,0.037482421875),"none"), ((114,-100,70,0.037482421875),"none"), ((114,-100,72,0.037482421875),"none"), ((114,-100,74,0.037482421875),"none"), ((114,-80,64,0.037482421875),"none"), ((114,-80,66,0.037482421875),"none"), ((114,-80,68,0.037482421875),"none"), ((114,-80,70,0.037482421875),"none"), ((114,-80,72,0.037482421875),"none"), ((114,-80,74,0.037482421875),"none"), ((114,-60,64,0.0491033911468506),"none"), ((114,-60,66,0.04974948234981454),"none"), ((114,-60,68,0.05041864489941496),"none"), ((114,-60,70,0.051112999802414315),"none"), ((114,-60,72,0.051832684146704056),"none"), ((114,-60,74,0.05257231446254198),"none"), ((114,-40,64,0.1122919611041932),"none"), ((114,-40,66,0.11636247433302638),"none"), ((114,-40,68,0.11979640774332366),"none"), ((114,-40,70,0.12330055500715384),"none"), ((114,-40,72,0.1268619579414046),"none"), ((114,-40,74,0.13048474950215),"none"), ((114,-20,64,0.10435296791344484),"none"), ((114,-20,66,0.10558672714675042),"none"), ((114,-20,68,0.10831868013423275),"none"), ((114,-20,70,0.11121282163190734),"none"), ((114,-20,72,0.11433776346079558),"none"), ((114,-20,74,0.11770444723497474),"none"), ((114,0,64,-0.0026209759846139574),"water"), ((114,0,66,-0.0011869543056835608),"water"), ((114,0,70,0.002068623223791626),"none"), ((114,0,72,0.0038193250297024243),"none"), ((114,0,74,0.005646396361630968),"none"), ((114,20,64,0.04183884597405475),"none"), ((114,20,66,0.039334239558802865),"none"), ((114,20,68,0.0367933735261107),"none"), ((114,20,70,0.03417828296442246),"none"), ((114,20,72,0.03146119496538579),"none"), ((114,20,74,0.028640178813057193),"none"), ((114,40,64,0.0011727557986376189),"none"), ((114,40,66,0.004478197250912497),"none"), ((114,40,68,0.007807656734373648),"none"), ((114,40,70,0.0110982528501824),"none"), ((114,40,72,0.014287968294347897),"none"), ((114,40,74,0.017340060385442255),"none"), ((114,60,64,0.057307692379898946),"none"), ((114,60,66,0.07909878599088975),"none"), ((114,60,68,0.09103769264897049),"none"), ((114,60,70,0.10529675531043547),"none"), ((114,60,72,0.1261191394093652),"none"), ((114,60,74,0.15323465023530602),"none"), ((114,80,64,-0.3135251519473628),"air"), ((114,80,66,-0.3092766951165722),"air"), ((114,80,68,-0.3063751991759311),"air"), ((114,80,70,-0.3004342091280733),"air"), ((114,80,72,-0.29703745590700253),"air"), ((114,80,74,-0.2920638815250855),"air"), ((114,100,64,-0.4583333333333333),"air"), ((114,100,66,-0.4583333333333333),"air"), ((114,100,68,-0.4583333333333333),"air"), ((114,100,70,-0.4583333333333333),"air"), ((114,100,72,-0.4583333333333333),"air"), ((114,100,74,-0.4583333333333333),"air"), ((116,-100,64,0.037482421875),"none"), ((116,-100,66,0.037482421875),"none"), ((116,-100,68,0.037482421875),"none"), ((116,-100,70,0.037482421875),"none"), ((116,-100,72,0.037482421875),"none"), ((116,-100,74,0.037482421875),"none"), ((116,-80,64,0.037482421875),"none"), ((116,-80,66,0.037482421875),"none"), ((116,-80,68,0.037482421875),"none"), ((116,-80,70,0.037482421875),"none"), ((116,-80,72,0.037482421875),"none"), ((116,-80,74,0.037482421875),"none"), ((116,-60,64,0.049510031628008565),"none"), ((116,-60,66,0.0501688864114582),"none"), ((116,-60,68,0.05085081029508635),"none"), ((116,-60,70,0.051557962654552196),"none"), ((116,-60,72,0.05229073376331405),"none"), ((116,-60,74,0.05304386684600273),"none"), ((116,-40,64,0.11647696380354154),"none"), ((116,-40,66,0.12065670598690036),"none"), ((116,-40,68,0.124186198423662),"none"), ((116,-40,70,0.12776934001653206),"none"), ((116,-40,72,0.1313900411197126),"none"), ((116,-40,74,0.13504793394752057),"none"), ((116,-20,64,0.10797310440989095),"none"), ((116,-20,66,0.11052803071968675),"none"), ((116,-20,68,0.11313075983384659),"none"), ((116,-20,70,0.11589560860382501),"none"), ((116,-20,72,0.11889599517563405),"none"), ((116,-20,74,0.12214992807094607),"none"), ((116,0,64,-0.003764380972543319),"water"), ((116,0,66,-0.002339168705169207),"water"), ((116,0,72,0.0027605740566328273),"none"), ((116,0,74,0.0046712919320928475),"none"), ((116,20,64,0.04027812111674997),"none"), ((116,20,66,0.03846080745866824),"none"), ((116,20,68,0.03602535830962611),"none"), ((116,20,70,0.033477031827460015),"none"), ((116,20,72,0.03082854239914883),"none"), ((116,20,74,0.028967404819271153),"none"), ((116,40,64,-0.009355588931802767),"none"), ((116,40,66,-0.006094366713842806),"air"), ((116,40,68,-0.0027537988904787606),"air"), ((116,40,72,0.00396682662711753),"none"), ((116,40,74,0.007260950530417173),"none"), ((116,60,64,0.022624582978071506),"none"), ((116,60,66,0.023942871216082746),"none"), ((116,60,68,0.037192323513527165),"none"), ((116,60,70,0.053305618429865455),"none"), ((116,60,72,0.06694547220363958),"none"), ((116,60,74,0.08711813973093903),"none"), ((116,80,64,-0.3326652310213258),"air"), ((116,80,66,-0.32962834810938174),"air"), ((116,80,68,-0.32236370014057947),"air"), ((116,80,70,-0.31670491006554574),"air"), ((116,80,72,-0.3130639601887072),"air"), ((116,80,74,-0.3124769234268471),"air"), ((116,100,64,-0.4583333333333333),"air"), ((116,100,66,-0.4583333333333333),"air"), ((116,100,68,-0.4583333333333333),"air"), ((116,100,70,-0.4583333333333333),"air"), ((116,100,72,-0.4583333333333333),"air"), ((116,100,74,-0.4583333333333333),"air"), ((118,-100,64,0.037482421875),"none"), ((118,-100,66,0.037482421875),"none"), ((118,-100,68,0.037482421875),"none"), ((118,-100,70,0.037482421875),"none"), ((118,-100,72,0.037482421875),"none"), ((118,-100,74,0.037482421875),"none"), ((118,-80,64,0.037482421875),"none"), ((118,-80,66,0.037482421875),"none"), ((118,-80,68,0.037482421875),"none"), ((118,-80,70,0.037482421875),"none"), ((118,-80,72,0.037482421875),"none"), ((118,-80,74,0.037482421875),"none"), ((118,-60,64,0.04983026397203373),"none"), ((118,-60,66,0.05049926028532013),"none"), ((118,-60,68,0.05119129093033625),"none"), ((118,-60,70,0.05190836104462707),"none"), ((118,-60,72,0.0526510917415344),"none"), ((118,-60,74,0.05341462434689053),"none"), ((118,-40,64,0.12023506214440967),"none"), ((118,-40,66,0.12453642919562771),"none"), ((118,-40,68,0.1281709824736104),"none"), ((118,-40,70,0.13184553502467083),"none"), ((118,-40,72,0.13554113610267055),"none"), ((118,-40,74,0.13925273367632113),"none"), ((118,-20,64,0.11282016461369758),"none"), ((118,-20,66,0.11526093589883914),"none"), ((118,-20,68,0.11774539644635261),"none"), ((118,-20,70,0.12038440430256446),"none"), ((118,-20,72,0.12325317705242089),"none"), ((118,-20,74,0.12637678353248477),"none"), ((118,0,64,-0.00501589634392619),"water"), ((118,0,66,-0.003601631485605401),"water"), ((118,0,68,-0.0020166185756455924),"water"), ((118,0,72,0.0015704124446037308),"none"), ((118,0,74,0.0035605198020311826),"none"), ((118,20,64,0.040232966220497414),"none"), ((118,20,66,0.039486572716430204),"none"), ((118,20,68,0.03885242305023035),"none"), ((118,20,70,0.03837601204655802),"none"), ((118,20,72,0.03770430407795084),"none"), ((118,20,74,0.03707763999605109),"none"), ((118,40,64,-0.016298811685686653),"none"), ((118,40,66,-0.016656636719901533),"air"), ((118,40,68,-0.01330299024830442),"air"), ((118,40,70,-0.009864486324034218),"air"), ((118,40,72,-0.006380723268648157),"air"), ((118,40,74,-0.002886835272463701),"air"), ((118,60,64,0.006086790713922152),"none"), ((118,60,66,0.006014479808113486),"none"), ((118,60,68,0.008953321476532182),"none"), ((118,60,70,0.011915636473415587),"none"), ((118,60,72,0.01001192490238903),"none"), ((118,60,74,0.0075500927486281426),"none"), ((118,80,64,-0.3462118919745469),"air"), ((118,80,66,-0.34419241078645835),"air"), ((118,80,68,-0.33580861045450133),"air"), ((118,80,70,-0.33008534054566163),"air"), ((118,80,72,-0.333649815109498),"air"), ((118,80,74,-0.33771329428807284),"air"), ((118,100,64,-0.4583333333333333),"air"), ((118,100,66,-0.4583333333333333),"air"), ((118,100,68,-0.4583333333333333),"air"), ((118,100,70,-0.4583333333333333),"air"), ((118,100,72,-0.4583333333333333),"air"), ((118,100,74,-0.4583333333333333),"air"), ((120,-100,64,0.037482421875),"none"), ((120,-100,66,0.037482421875),"none"), ((120,-100,68,0.037482421875),"none"), ((120,-100,70,0.037482421875),"none"), ((120,-100,72,0.037482421875),"none"), ((120,-100,74,0.037482421875),"none"), ((120,-80,64,0.037482421875),"none"), ((120,-80,66,0.037482421875),"none"), ((120,-80,68,0.037482421875),"none"), ((120,-80,70,0.037482421875),"none"), ((120,-80,72,0.037482421875),"none"), ((120,-80,74,0.037482421875),"none"), ((120,-60,64,0.05006400032487905),"none"), ((120,-60,66,0.05074065517392799),"none"), ((120,-60,68,0.05144011744518559),"none"), ((120,-60,70,0.05216399757431933),"none"), ((120,-60,72,0.052913091382129525),"none"), ((120,-60,74,0.053683198493088384),"none"), ((120,-40,64,0.1235607292468892),"none"), ((120,-40,66,0.12798336289135304),"none"), ((120,-40,68,0.13173164094126777),"none"), ((120,-40,70,0.13550880522456066),"none"), ((120,-40,72,0.1392932761010718),"none"), ((120,-40,74,0.14307520910884483),"none"), ((120,-20,64,0.11746888767713162),"none"), ((120,-20,66,0.1198086187834423),"none"), ((120,-20,68,0.12218509691580316),"none"), ((120,-20,70,0.12469970986670785),"none"), ((120,-20,72,0.1274266324562779),"none"), ((120,-20,74,0.13039812171785095),"none"), ((120,0,64,-0.006329576321547214),"water"), ((120,0,66,-0.004930192503238298),"water"), ((120,0,68,-0.003355964670343278),"water"), ((120,0,70,-0.0016206542469077872),"water"), ((120,0,74,0.002332419553726638),"none"), ((120,20,64,0.04783863627983937),"none"), ((120,20,66,0.04699098011102891),"none"), ((120,20,68,0.046231913852781324),"none"), ((120,20,70,0.04560004002476374),"none"), ((120,20,72,0.04512419653798615),"none"), ((120,20,74,0.04482086018104904),"none"), ((120,40,64,-0.017456523705167773),"none"), ((120,40,66,-0.020044623482270124),"air"), ((120,40,68,-0.022372181411266172),"air"), ((120,40,70,-0.020228945291907708),"air"), ((120,40,72,-0.01664436674077766),"air"), ((120,40,74,-0.013001583733654043),"air"), ((120,60,64,-0.010805185122555435),"water"), ((120,60,66,-0.011684313707812422),"water"), ((120,60,68,-0.007705484690135335),"water"), ((120,60,70,-0.012326309226980426),"water"), ((120,60,72,-0.019043795741958334),"water"), ((120,60,74,-0.023185441889689514),"water"), ((120,80,64,-0.3611328625547435),"air"), ((120,80,66,-0.3586517592327399),"air"), ((120,80,68,-0.3524534485283812),"air"), ((120,80,70,-0.35323218454039057),"air"), ((120,80,72,-0.36213549677301105),"air"), ((120,80,74,-0.3684474143996314),"air"), ((120,100,64,-0.4583333333333333),"air"), ((120,100,66,-0.4583333333333333),"air"), ((120,100,68,-0.4583333333333333),"air"), ((120,100,70,-0.4583333333333333),"air"), ((120,100,72,-0.4583333333333333),"air"), ((120,100,74,-0.4583333333333333),"air"), ((122,-100,64,0.037482421875),"none"), ((122,-100,66,0.037482421875),"none"), ((122,-100,68,0.037482421875),"none"), ((122,-100,70,0.037482421875),"none"), ((122,-100,72,0.037482421875),"none"), ((122,-100,74,0.037482421875),"none"), ((122,-80,64,0.037482421875),"none"), ((122,-80,66,0.037482421875),"none"), ((122,-80,68,0.037482421875),"none"), ((122,-80,70,0.037482421875),"none"), ((122,-80,72,0.037482421875),"none"), ((122,-80,74,0.037482421875),"none"), ((122,-60,64,0.050211152183993704),"none"), ((122,-60,66,0.05089355899866971),"none"), ((122,-60,68,0.05159826596516877),"none"), ((122,-60,70,0.05232622949491639),"none"), ((122,-60,72,0.053078329171471185),"none"), ((122,-60,74,0.05385122904837061),"none"), ((122,-40,64,0.12644943947294945),"none"), ((122,-40,66,0.13095960991588632),"none"), ((122,-40,68,0.13485451601704876),"none"), ((122,-40,70,0.13874405974643175),"none"), ((122,-40,72,0.1426292409194739),"none"), ((122,-40,74,0.14649538659711875),"none"), ((122,-20,64,0.12191638116406471),"none"), ((122,-20,66,0.12416901766463509),"none"), ((122,-20,68,0.1264480340172126),"none"), ((122,-20,70,0.12884021810202248),"none"), ((122,-20,72,0.13141636494496137),"none"), ((122,-20,74,0.13421609155559988),"none"), ((122,0,64,-0.007667256303541582),"water"), ((122,0,66,-0.006288822820341533),"water"), ((122,0,68,-0.004737470102527975),"water"), ((122,0,70,-0.0030099389619020873),"water"), ((122,0,72,-0.0010942861750551764),"water"), ((122,0,74,0.001001992078975999),"none"), ((122,20,64,0.05535892661135848),"none"), ((122,20,66,0.05444413322973901),"none"), ((122,20,68,0.05359719969130291),"none"), ((122,20,70,0.05284624631887433),"none"), ((122,20,72,0.052209127741325245),"none"), ((122,20,74,0.05170112238940831),"none"), ((122,40,64,-0.013498316953033454),"none"), ((122,40,66,-0.016896390550754353),"air"), ((122,40,68,-0.01994683889106233),"air"), ((122,40,70,-0.022658183924480487),"air"), ((122,40,72,-0.02460705550987633),"air"), ((122,40,74,-0.02133677750482264),"none"), ((122,60,64,-0.02580014098083049),"water"), ((122,60,66,-0.027410062228040422),"water"), ((122,60,68,-0.02425659570836858),"water"), ((122,60,70,-0.03261718168256943),"water"), ((122,60,72,-0.04369665936638442),"water"), ((122,60,74,-0.04490159197647781),"water"), ((122,80,64,-0.37247525946547166),"air"), ((122,80,66,-0.3727266378002749),"air"), ((122,80,68,-0.36804742745663505),"air"), ((122,80,70,-0.3736723706537362),"air"), ((122,80,72,-0.3860951288334311),"air"), ((122,80,74,-0.3923721309133264),"air"), ((122,100,64,-0.4583333333333333),"air"), ((122,100,66,-0.4583333333333333),"air"), ((122,100,68,-0.4583333333333333),"air"), ((122,100,70,-0.4583333333333333),"air"), ((122,100,72,-0.4583333333333333),"air"), ((122,100,74,-0.4583333333333333),"air"), ((124,-100,64,0.037482421875),"none"), ((124,-100,66,0.037482421875),"none"), ((124,-100,68,0.037482421875),"none"), ((124,-100,70,0.037482421875),"none"), ((124,-100,72,0.037482421875),"none"), ((124,-100,74,0.037482421875),"none"), ((124,-80,64,0.037482421875),"none"), ((124,-80,66,0.037482421875),"none"), ((124,-80,68,0.037482421875),"none"), ((124,-80,70,0.037482421875),"none"), ((124,-80,72,0.037482421875),"none"), ((124,-80,74,0.037482421875),"none"), ((124,-60,64,0.05027131800090962),"none"), ((124,-60,66,0.0509584844305151),"none"), ((124,-60,68,0.05166719216129481),"none"), ((124,-60,70,0.05239749814810798),"none"), ((124,-60,72,0.053150241639753654),"none"), ((124,-60,74,0.053923063702221205),"none"), ((124,-40,64,0.1288961855391929),"none"), ((124,-40,66,0.13348604127036617),"none"), ((124,-40,68,0.13753360917037172),"none"), ((124,-40,70,0.1415440368478666),"none"), ((124,-40,72,0.1455395599824845),"none"), ((124,-40,74,0.1495006614184435),"none"), ((124,-20,64,0.12612367959767024),"none"), ((124,-20,66,0.12830319402402945),"none"), ((124,-20,68,0.1304960062341348),"none"), ((124,-20,70,0.1327707947453995),"none"), ((124,-20,72,0.13519345838131658),"none"), ((124,-20,74,0.13781110986582973),"none"), ((124,0,64,-0.009009809178163559),"water"), ((124,0,66,-0.007660237459532607),"water"), ((124,0,68,-0.0061446463166489424),"water"), ((124,0,70,-0.004442459854201204),"water"), ((124,0,72,-0.0025318494505197223),"water"), ((124,20,64,0.06277697412858188),"none"), ((124,20,66,0.06182400117210263),"none"), ((124,20,68,0.060920470163534336),"none"), ((124,20,70,0.06008166953195572),"none"), ((124,20,72,0.05931163578048011),"none"), ((124,20,74,0.05862261977236657),"none"), ((124,40,64,-0.004596793013348681),"none"), ((124,40,66,-0.007881293688553023),"none"), ((124,40,68,-0.010851313478373932),"none"), ((124,40,70,-0.013513605008223933),"water"), ((124,40,72,-0.015880866729427373),"water"), ((124,40,74,-0.017978317799117856),"water"), ((124,60,64,-0.033729060298063454),"water"), ((124,60,66,-0.04062740064249005),"water"), ((124,60,68,-0.03660634922712756),"water"), ((124,60,70,-0.04106936165998065),"water"), ((124,60,72,-0.048715160337165046),"water"), ((124,60,74,-0.053817378732386144),"water"), ((124,80,64,-0.378513110274629),"air"), ((124,80,66,-0.37887533037235366),"air"), ((124,80,68,-0.3755672366866089),"air"), ((124,80,70,-0.3806264904596738),"air"), ((124,80,72,-0.39139114552312176),"air"), ((124,80,74,-0.39905004304932734),"air"), ((124,100,64,-0.4583333333333333),"air"), ((124,100,66,-0.4583333333333333),"air"), ((124,100,68,-0.4583333333333333),"air"), ((124,100,70,-0.4583333333333333),"air"), ((124,100,72,-0.4583333333333333),"air"), ((124,100,74,-0.4583333333333333),"air"), ((126,-100,64,0.037482421875),"none"), ((126,-100,66,0.037482421875),"none"), ((126,-100,68,0.037482421875),"none"), ((126,-100,70,0.037482421875),"none"), ((126,-100,72,0.037482421875),"none"), ((126,-100,74,0.037482421875),"none"), ((126,-80,64,0.037482421875),"none"), ((126,-80,66,0.037482421875),"none"), ((126,-80,68,0.037482421875),"none"), ((126,-80,70,0.037482421875),"none"), ((126,-80,72,0.037482421875),"none"), ((126,-80,74,0.037482421875),"none"), ((126,-60,64,0.05024330978368198),"none"), ((126,-60,66,0.05093542123713047),"none"), ((126,-60,68,0.05164825745773993),"none"), ((126,-60,70,0.052380776473173525),"none"), ((126,-60,72,0.053133618951577574),"none"), ((126,-60,74,0.05390538149459787),"none"), ((126,-40,64,0.1308942633558083),"none"), ((126,-40,66,0.1355722846035891),"none"), ((126,-40,68,0.13977234300629068),"none"), ((126,-40,70,0.14391148475400958),"none"), ((126,-40,72,0.1480252139578508),"none"), ((126,-40,74,0.15208907413316627),"none"), ((126,-20,64,0.13001056656809973),"none"), ((126,-20,66,0.13213006749070327),"none"), ((126,-20,68,0.1342494739710899),"none"), ((126,-20,70,0.13641807828631622),"none"), ((126,-20,72,0.13869641277763253),"none"), ((126,-20,74,0.1411390651002113),"none"), ((126,0,64,-0.010355206926252296),"water"), ((126,0,66,-0.009043874021560911),"water"), ((126,0,68,-0.007576245457331987),"water"), ((126,0,70,-0.005915269354878528),"water"), ((126,0,72,-0.0040306175772153365),"water"), ((126,0,74,-0.0019328609472880898),"water"), ((126,20,64,0.0700499260773044),"none"), ((126,20,66,0.06908003164862005),"none"), ((126,20,68,0.0681425614589177),"none"), ((126,20,70,0.06723915989832648),"none"), ((126,20,72,0.066359174495176),"none"), ((126,20,74,0.06551116407146489),"none"), ((126,40,64,0.004497597038868666),"none"), ((126,40,66,0.0013502912778844962),"none"), ((126,40,68,-0.0015191728313191184),"water"), ((126,40,70,-0.0041188588354404134),"water"), ((126,40,72,-0.006463772144671846),"water"), ((126,40,74,-0.008581034519921562),"water"), ((126,60,64,-0.03471424652823008),"water"), ((126,60,66,-0.04732045558891548),"water"), ((126,60,68,-0.04568337003176991),"water"), ((126,60,70,-0.0428377824231183),"water"), ((126,60,72,-0.04738820166968918),"water"), ((126,60,74,-0.05663750895047857),"water"), ((126,80,64,-0.37931742687180287),"air"), ((126,80,66,-0.38265481838544235),"air"), ((126,80,68,-0.3808041835281554),"air"), ((126,80,70,-0.38160238129796925),"air"), ((126,80,72,-0.387746448733821),"air"), ((126,80,74,-0.3990668807989283),"air"), ((126,100,64,-0.4583333333333333),"air"), ((126,100,66,-0.4583333333333333),"air"), ((126,100,68,-0.4583333333333333),"air"), ((126,100,70,-0.4583333333333333),"air"), ((126,100,72,-0.4583333333333333),"air"), ((126,100,74,-0.4583333333333333),"air")];

    #[test]
    fn aquifer_vectors() {
        let _guard = lock_gen();
        let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
        let mut miss = 0u32;
        {
            let g = &mut g;
            for &((x, y, z), want) in AQ_NB {
                let got = g.aq_noise_level(x, y, z, 200);
                if got != want { miss += 1; eprintln!("nb {x},{y},{z}: {got} != {want}"); }
            }
            for (tbl, my) in [(AQ_FYT, true), (AQ_FYF, false)] {
                for &((x, y, z), want) in tbl {
                    let want = want;
                    let got = g.aq_fluid_y(x, y, z, (0, 1), 80, my);
                    if got != want { miss += 1; eprintln!("fy[{my}] {x},{y},{z}: {got} != {want}"); }
                }
            }
            for &((x, y, z), want) in AQ_FBS {
                let got = g.aq_fluid_kind(x, y, z, (0, 1), -10);
                if got != want { miss += 1; eprintln!("fbs {x},{y},{z}: {got} != {want}"); }
            }
            for &((x, y, z), (wy, wk)) in AQ_FL {
                let wy = wy;
                let got = g.aq_fluid_level_at(x, y, z);
                if got != (wy, wk) { miss += 1; eprintln!("fl {x},{y},{z}: {got:?} != ({wy},{wk})"); }
            }
            for &((x, y, z, h1, h2), want) in AQ_CD {
                let mut b = None;
                let got = g.aq_calc_density(x, y, z, (h1, 1), (h2, 1), &mut b);
                if (got - want).abs() > 1e-4 { miss += 1; eprintln!("cd {x},{y},{z},{h1},{h2}: {got} != {want}"); }
            }
        }
        let water = crate::name_to_state("minecraft:water").unwrap();
        let lava = crate::name_to_state("minecraft:lava").unwrap();
        for &((x, y, z, d), blk) in AQ_AP {
            let got = g.aq_state(x, y, z, d);
            let want = match blk {
                "none" => None,
                "air" => Some(0),
                "water" => Some(water),
                "lava" => Some(lava),
                _ => unreachable!(),
            };
            if got != want { miss += 1; eprintln!("ap {x},{y},{z}: {got:?} != {want:?}"); }
        }
        assert_eq!(miss, 0, "aquifer vector mismatches");
    }



    #[test]
    fn aqdist() {
        let _guard = lock_gen();
        if std::env::var("WGAQ").is_err() { return; }
        assert!(init(0, crate::name_to_state));
        let (mut air, mut wat, mut lav, mut sol, mut cave) = (0,0,0,0,0);
        for cx in 0..4 { for cz in 0..4 {
            let cc = chunk_cols(cx, cz).unwrap();
            for lx in 0..16 { for lz in 0..16 {
                for y in (-60..60).step_by(2) {
                    let d = density_at(cx*16+lx, y, cz*16+lz).unwrap_or(0.0);
                    if d <= 0.0 {
                        cave += 1;
                        match aquifer_state(cx*16+lx, y, cz*16+lz, d) {
                            Some(0) => air += 1,
                            Some(80) => wat += 1,
                            Some(96) => lav += 1,
                            Some(_) => sol += 1,
                            None => sol += 1,
                        }
                    }
                }
            } }
        } }
        eprintln!("AQDIST caves={cave} air={air} water={wat} lava={lav} solid={sol}");
    }

    #[test]
    fn perf_probe() {
        let _guard = lock_gen();
        if std::env::var("WGPERF").is_err() { return; }
        assert!(init(0, crate::name_to_state));
        let t = std::time::Instant::now();
        let mut n = 0;
        for cx in 0..8 { for cz in 0..8 {
            chunk_cols(cx, cz).unwrap(); n += 1;
        } }
        let el = t.elapsed().as_secs_f64() * 1000.0 / n as f64;
        eprintln!("PERF chunk_cols: {el:.2} ms/chunk ({n} chunks)");
        // aquifer+surface cell evaluation cost via deep_state on a few chunks
        let t = std::time::Instant::now();
        let mut m = 0;
        for cx in 0..4 { for cz in 0..4 {
            let cc = chunk_cols(cx, cz).unwrap();
            for y in (-64..64).step_by(2) {
                for lx in 0..16 { for lz in 0..16 {
                    deep_state(cx*16+lx, y, cz*16+lz, &cc); m += 1;
                } }
            }
        } }
        eprintln!("PERF deep_state: {:.4} ms/call", t.elapsed().as_secs_f64() * 1000.0 / m as f64);
    }

    #[test]
    fn feat_populate_perf() {
        let _guard = lock_gen();
        let seed = 0u64;
        init(seed, crate::name_to_state);
        let t = std::time::Instant::now();
        // one query triggers populate of the 3x3 neighborhood
        feature_state(8, 64, 8, crate::feature::populate);
        eprintln!("PERF populate9: {:?}", t.elapsed());
        let t = std::time::Instant::now();
        for cx in -1..=1 {
            for cz in -1..=1 {
                feature_state(cx * 16 + 8, 64, cz * 16 + 8, crate::feature::populate);
            }
        }
        eprintln!("PERF +8 centers (cached): {:?}", t.elapsed());
        let mut n = 0;
        for cx in -1..=1 {
            for cz in -1..=1 {
                if let Some(m) = GEN.lock().unwrap().as_ref().unwrap().feat_map(cx, cz) {
                    n += m.len();
                }
            }
        }
        eprintln!("PERF overrides/chunk-9x9: {}", n);
        crate::feature::feat_prof_dump();
    }

    #[test]
    fn lazy_population_keeps_neighborhood_order_and_cached_overrides() {
        let _guard = lock_gen();
        let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
        let mut visited = Vec::new();
        let mut populate = |g: &mut Gen, sx: i32, sz: i32| {
            visited.push((sx, sz));
            // Cross-chunk writes make invocation order and repeated population
            // observable in returned values, not just in the call trace.
            g.feat_write(sx * 16 + 16, 64, sz * 16, 100 + visited.len() as u32);
        };
        assert_eq!(g.feat_state(-16, 64, -16, &mut populate), Some(102));
        assert_eq!(g.feat_state(-16, 64, -16, &mut populate), Some(102));
        assert_eq!(g.feat_state(0, 64, 0, &mut populate), Some(106));
        let mut expected: Vec<_> = (-2..=0).flat_map(|sx| (-2..=0).map(move |sz| (sx, sz))).collect();
        expected.extend([(-1, 1), (0, 1), (1, -1), (1, 0), (1, 1)]);
        assert_eq!(visited, expected);
    }

    #[test]
    fn lazy_population_preserves_direct_feature_outputs() {
        let _guard = lock_gen();
        // Preserve population results through the lazy lookup, including writes
        // outside the queried chunk. This is a refactor control, not a vanilla oracle.
        for (x, z) in [(-1i32, -1i32), (16, 16)] {
            let mut lazy = Gen::load(0, crate::name_to_state).expect("datapack");
            let mut direct = Gen::load(0, crate::name_to_state).expect("datapack");
            let first = lazy.feat_state(x, 64, z, crate::feature::populate);
            let (cx, cz) = (x.div_euclid(16), z.div_euclid(16));
            for sx in cx - 1..=cx + 1 {
                for sz in cz - 1..=cz + 1 {
                    crate::feature::populate(&mut direct, sx, sz);
                }
            }
            let expected: usize = direct.feat_c.values().map(HashMap::len).sum();
            assert!(expected > 0, "fixture must exercise real feature writes");
            assert!(lazy.feat_c == direct.feat_c,
                "feature values differ at center ({cx},{cz}): actual={} expected={expected}",
                lazy.feat_c.values().map(HashMap::len).sum::<usize>());
            assert_eq!(lazy.feat_state(x, 64, z, crate::feature::populate), first);
            assert!(lazy.feat_c == direct.feat_c, "cached lookup changed feature values");
        }
    }

    /// The full, sorted feature population output for a fixed seed and query
    /// sequence, as raw bytes. The digest below was taken after switching the
    /// feature order to vanilla's decompiled `FeatureSorter` algorithm (DFS
    /// post-order reversed, cross-step edges); later shared provider/predicate/
    /// context edits must reproduce it exactly.
    fn feature_population_capture(g: &mut Gen) -> Vec<u8> {
        let mut bytes = b"GEN-OUTPUT-1\0".to_vec();
        for (x, y, z) in [(-1, 64, -1), (0, 64, 0), (-17, 64, -1), (0, 80, 0)] {
            let result = g.feat_state(x, y, z, crate::feature::populate);
            for n in [x, y, z] { bytes.extend_from_slice(&n.to_le_bytes()); }
            bytes.push(u8::from(result.is_some()));
            bytes.extend_from_slice(&result.unwrap_or(0).to_le_bytes());
            let mut rows: Vec<_> = g.feat_c.iter().flat_map(|(&(cx, cz), map)| {
                map.iter().map(move |(&index, &state)| (cx, cz, index, state))
            }).collect();
            rows.sort_unstable();
            assert!(!rows.is_empty(), "the fixture must exercise feature writes");
            bytes.extend_from_slice(&(rows.len() as u64).to_le_bytes());
            for (cx, cz, index, state) in rows {
                bytes.extend_from_slice(&cx.to_le_bytes());
                bytes.extend_from_slice(&cz.to_le_bytes());
                bytes.extend_from_slice(&index.to_le_bytes());
                bytes.extend_from_slice(&state.to_le_bytes());
            }
            let mut done: Vec<_> = g.pop_done.iter().copied().collect();
            done.sort_unstable();
            bytes.extend_from_slice(&(done.len() as u64).to_le_bytes());
            for (cx, cz) in done {
                bytes.extend_from_slice(&cx.to_le_bytes());
                bytes.extend_from_slice(&cz.to_le_bytes());
            }
        }
        bytes
    }

    #[test]
    fn vertical_gradient_seed_reads_the_name_hash_big_endian() {
        let _guard = lock_gen();
        // vanilla StringHash.fromString = Guava Longs.fromBytes, i.e. big-endian,
        // then PositionalRandomFactory.fromHashOf XORs those words with the
        // world's forkPositional seed. Reading the md5 words little-endian put
        // deepslate/bedrock in the wrong cells (1221 vs 194 block diffs against
        // a vanilla server with features disabled).
        let g = Gen::load(0, crate::name_to_state).expect("datapack");
        let h = crate::generation_math::md5(b"minecraft:deepslate");
        let want_lo = u64::from_be_bytes(h[0..8].try_into().unwrap());
        let want_hi = u64::from_be_bytes(h[8..16].try_into().unwrap());
        let be_pair = g.conds.iter().any(|c| {
            if let Cond::VGrad { lo_w, hi_w, .. } = c {
                *lo_w == want_lo && *hi_w == want_hi
            } else {
                false
            }
        });
        assert!(
            be_pair,
            "vertical_gradient must read random_name's md5 big-endian"
        );
    }

    #[test]
    fn not_underwater_is_grass_only_above_the_water_surface() {
        let _guard = lock_gen();
        // vanilla's WaterCondition (SurfaceRules$WaterConditionSource$1WaterCondition)
        // is already "not underwater": waterHeight == MIN (no fluid), or
        // blockY + stoneDepthAbove >= the water surface. The grass rule uses it
        // directly. Negating it put grass on the submerged floors of the spawn
        // ocean and dirt on dry ground.
        assert!(init(0, crate::name_to_state));
        let grass = crate::name_to_state("minecraft:grass_block").unwrap();
        let dirt = crate::name_to_state("minecraft:dirt").unwrap();
        let wet = chunk_cols(0, 0).unwrap();
        assert_eq!(wet.surface(2, 0), 54, "top solid of the spawn ocean column");
        assert_eq!(wet.wtop(2, 0), 62, "its water surface");
        assert_eq!(wet.mat(2, 0, 54), Some(dirt), "a submerged floor is dirt");
        let dry = chunk_cols(0, 2).unwrap();
        assert_eq!(dry.surface(4, 0), 63, "top solid of a dry column");
        assert_eq!(dry.mat(4, 0, 63), Some(grass), "a dry surface is grass");
    }

    #[test]
    fn surface_density_follows_vanillas_interpolated_order() {
        let _guard = lock_gen();
        // vanilla samples `interpolated` over its input at 4x8x4 cell corners and
        // interpolates that, then applies squeeze/min/add per block. Interpolating
        // final_density as a whole is lerp(squeeze(X)); squeeze is cubic, so the
        // surface of this column sat one block low (y=62 instead of 63).
        assert!(init(0, crate::name_to_state));
        let cc = chunk_cols(0, 1).unwrap();
        assert_eq!(cc.surface(4, 0), 63, "surface of the check column");
    }

    #[test]
    fn feature_population_output_matches_the_pre_refactor_capture() {
        let _guard = lock_gen();
        // Unlike the lazy-vs-direct comparison, this pins the values against a
        // digest recorded before the shared contracts moved, so a change made on
        // both sides at once cannot hide behind a passing local comparison.
        let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
        let bytes = feature_population_capture(&mut g);
        let digest: String = crate::generation_math::md5(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        // 2026-10-20: the surface materials changed because the
        // vertical_gradient name hash is now read big-endian like vanilla's
        // Guava Longs.fromBytes (see vertical_gradient_seed_reads_the_name_hash_big_endian).
        // Vanilla block diff with features and carvers disabled on both sides
        // went 1221 -> 194 cells at seed 0 chunk (0,0); the deepslate and
        // bedrock bands match, so this capture records the corrected input.
        // 2026-10-20: the terrain now matches vanilla exactly, and the biome
        // feature lists were replaced with the 1.21.1 server jar's (the bundled
        // data had 1.21.5-ish lists: 48/64 biomes differed). With features and
        // carvers disabled the block diff is 0 in (0,0), (0,1), (-1,-1), (-2,0);
        // with features on it is 10939 -> 10874 after the list swap.
        // 2026-10-20: placement modifiers now run as vanilla's stream
        // (`flatMap` per position, feature placed before the next position is
        // pulled) instead of one modifier over all positions. Same seed, same
        // step index, different draw order: `ore_andesite_lower` in chunk (0,0)
        // used to place at y=15 and y=18, vanilla's order gives y=49 and y=18.
        // Full-chunk block diff for (0,0) against the replayed vanilla world:
        // 11017 -> 10302 cells (11.21% -> 10.48%).
        // 2026-10-21: `Lcg::next_i64` now sign-extends its low word like Java's
        // `nextLong` (see `world_rng`'s pinned vector), which is what every
        // population / large-feature seed is built from. Half of all draws were
        // 2^32 off. Measured cost in this layer: the 2x2-chunk feature-on diff
        // against a fresh 1.21.1 world went 44187 -> 45226 cells, so the feature
        // layer still has a compensating defect (R1a); the carver seeds need the
        // correct value, and two wrong seeds are not a calibration to preserve.
        // 2026-10-22: `minecraft:uniform` height providers draw once (`eia` ->
        // `Mth.randomBetweenInclusive`), not twice like the trapezoid `ehv` the
        // port had copied. Every cave tunnel start was shifted by one draw.
        // Cave-only and all-carver 2x2 fixtures at (0,0) went 617 / 617+341 ->
        // 0 differing cells against vanilla, so this capture records the
        // corrected (vanilla-matching) carving output.
        // 2026-10-22: feature placement now draws from a `WorldgenRandom`, which
        // wraps the Xoroshiro source but implements `dyn`'s *legacy* draws
        // (`nextInt(bound)` = multiply-shift on `next(31)`, `nextInt()` = the top
        // word, `nextDouble()` = two draws). The modern `RandomGenerator` forms
        // put every ore blob somewhere else: 242/242 origins differed. With the
        // legacy flavour the first origins match vanilla exactly and the 2x2
        // feature-on diff at (0,0) drops 40975 -> 34679.
        // 2026-10-22: the feature height providers now match vanilla's classes
        // (uniform `eia` one draw, trapezoid `ehz` plateau 0, biased `ehv`,
        // very-biased `eib`) and `below_top` resolves to minY + height - 1 - value.
        // With those fixed every one of the 22 ore placed features in chunk (0,0)
        // has an identical origin list to the live server, and the 2x2 feature-on
        // diff drops 33853 cells.
        // 2026-10-22: the decoration seed is now computed the way vanilla does it
        // (`WorldgenRandom.setDecorationSeed` on the Xoroshiro source, with XOR
        // instead of the LCG and addition the port had). Only chunk (0,0) was
        // unaffected, because its seed is `0 ^ 0 ^ seed`. With the fix the ore
        // origins of neighbour chunks match too and the 2x2 feature-on diff
        // drops 33853 -> 15820.
        // 2026-10-22: `underwater_magma` now runs `UnderwaterMagmaFeature`'s
        // Column.scan (the origin must be water) and enclosure test, so a land
        // chunk no longer receives magma at all. 2x2 feature-on diff: 3472 ->
        // 2188 cells.
        // 2026-10-22: the placed-feature definitions are now the 1.21.1 jar's
        // (the bundled set was 1.21.2+ and, for example, referenced
        // `trees_birch_and_oak_leaf_litter` while the 1.21.1 biomes reference
        // `trees_birch_and_oak`, so every forest tree was silently skipped).
        // 2x2 feature-on diff: 2188 -> 2062.
        // 2026-10-22: the feature-order tie-break list is now the biome list
        // `FeatureSorter.buildFeaturesPerStep` actually receives (53 overworld
        // biomes, measured from a live server) instead of a 64-entry registry
        // order that also contained the nether and the end. That shifted the
        // per-feature index - and therefore the RNG offset - of every step whose
        // biome lists differ, so trees and vegetation landed in the wrong places
        // while the ore step (identical lists in every biome) was unaffected.
        // 2026-10-22: the disk features now place. Their 1.21.1 state provider has
        // no "type" field and their fluid predicate names a single fluid, so both
        // parsed to nothing and the feature silently did no work.
        // 2x2 feature-on diff: 2510 -> 1856 (chunk (0,0): 540 -> 140).
        // 2026-10-22: a nested *placed* feature reference now runs the referenced
        // feature's own placement modifiers (the configured-feature contract holds
        // Holder<PlacedFeature>). Dropping them hid
        // block_predicate_filter(would_survive) on the random_selector children, so
        // every tree candidate grew a tree. 2x2 feature-on diff: 1856 -> 1035.
        // 2026-10-22: the biome the placement chain reads is now
        // `BiomeManager.getBiome` (the blended lookup that `LevelReader.getBiome`
        // returns) instead of the chunk's quart container. BiomeFilter uses it, so
        // the container path accepted candidates vanilla rejects at a biome
        // boundary. 2x2 feature-on diff: 1035 -> 940; trees 45 -> 42 (vanilla 38).
        // 2026-10-22: the feature-layer heightmap is now owned by `Gen` and updated
        // on every write, so a tree placed for one chunk raises the height a
        // neighbouring chunk's candidates read (vanilla updates the *owning* chunk's
        // heightmap on setBlock, and OCEAN_FLOOR counts canopy leaves). Keeping the
        // tally inside one placement pass made candidate (0,17) read y=64 instead of
        // vanilla's 70 and grow a tree vanilla rejects on the leaf above it.
        // 2x2 feature-on diff: 940 -> 704; trees 42 -> 39 (vanilla 38).
        // 2026-10-23: `random_patch` is implemented and `matching_blocks` accepts a
        // scalar `blocks`, so the vegetation that was inert now places. The capture
        // moves with it; the fixture to read next is `tools/ore_fixture.sh`
        // (938 -> 42939 cells over 8 chunks, short_grass 0 -> 362 against vanilla's 102,
        // i.e. an over-placement to explain).
        // 2026-10-23: `SimpleBlockFeature`'s `canSurvive` gate is applied, so a patch no
        // longer places its block on any air cell in the box (short_grass 362 -> 44
        // against vanilla's 102, with the draw streams identical - the next item is the
        // attempt/acceptance count, not the placement).
        // 2026-10-23: `seagrass` is implemented (`edk` + `SeagrassBlock.mayPlaceOn`), which
        // places 27 cells against vanilla's 28 in the isolated fixture - the capture
        // moves with it, and that feature's residue is now the size of the reference's
        // own noise floor.
        // 2026-10-23: `minecraft:flower` (the same patch config shape as random_patch, a
        // different type name) is implemented, so the flower patches in the pack place.
        assert_eq!(digest, "549920689f0e8bc55538ad9aada6bc45",
            "feature population output changed from the vanilla-order capture");
    }

    #[test]
    fn bundled_carver_configs_are_the_1_21_1_shapes() {
        let _guard = lock_gen();
        // The four configured carvers the overworld's biomes reference. They were missing
        // entirely, so no cave could ever be carved; this keeps them present and shaped like
        // the 1.21.1 jar's files now that the carving code is the next piece.
        for (name, ty) in [
            ("cave", "minecraft:cave"),
            ("cave_extra_underground", "minecraft:cave"),
            ("canyon", "minecraft:canyon"),
            ("nether_cave", "minecraft:nether_cave"),
        ] {
            let raw = std::fs::read(format!(
                "datapacks/vanilla/data/minecraft/worldgen/configured_carver/{name}.json"
            ))
            .unwrap_or_else(|e| panic!("{name}: {e}"));
            let j = crate::json::jparse(&raw).unwrap_or_else(|| panic!("{name}: not JSON"));
            assert_eq!(crate::json::jstr(&j, "type").as_deref(), Some(ty), "{name}: type");
            let cfg = crate::json::js(&j, "config").unwrap_or_else(|| panic!("{name}: no config"));
            for key in ["probability", "y", "yScale", "lava_level", "replaceable"] {
                assert!(crate::json::js(cfg, key).is_some(), "{name}: config lacks {key}");
            }
        }
    }

    #[test]
    fn the_ore_vein_geometry_matches_vanillas_own_output() {
        let _guard = lock_gen();
        // Vanilla's OreFeature.place (called through tools/vanilla-probe) writes these 666
        // cells for seed 60007, position (7,49,13) and size 64. Pinning them means the Rust
        // generator is compared against the game's own output, not only against the Python
        // port in tools/ore_oracle.py.
        let text = include_str!("../evidence/gameplay/feature-parity/ore_andesite_666.txt");
        let mut want: Vec<(i32, i32, i32)> = text
            .lines()
            .filter_map(|l| {
                let mut it = l.split_whitespace();
                Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?, it.next()?.parse().ok()?))
            })
            .collect();
        want.sort();
        let got = crate::feature::ore_cells_for_test(60007, (7, 49, 13), 64);
        assert_eq!(got.len(), want.len(), "cell count differs from vanilla's 666");
        assert_eq!(got, want, "vein geometry differs from vanilla's own output");
    }

    #[test]
    fn the_shared_biome_cache_returns_the_same_leaf_for_the_same_quart() {
        let _guard = lock_gen();
        // The deep material rule asks for the biome once per block; the answer only depends on the
        // quart, so one shared cache replaced millions of B-tree searches. Pin that a repeat query
        // returns what the first one computed and what a direct lookup gives.
        let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
        let mut c = SC::new();
        for (qx, qy, qz) in [(0i32, 24, 0), (1, 25, 2), (-3, -8, 5), (7, 60, -8)] {
            c.x = qx << 2;
            c.y = qy << 2;
            c.z = qz << 2;
            let first = g.leaf_at(&mut c);
            let second = g.leaf_at(&mut c);
            assert_eq!(first, second, "the shared cache changed the leaf for {qx},{qy},{qz}");
            let direct = g
                .biome_quart(qx, qy, qz, None)
                .map(|(_, l)| l)
                .unwrap_or(u16::MAX);
            assert_eq!(second, direct, "the shared cache disagrees with a direct lookup at {qx},{qy},{qz}");
        }
    }

    #[test]
    fn feat_determinism() {
        let _guard = lock_gen();
        // same seed -> identical override maps; and feature writes must cross
        // chunk boundaries (e.g. a tree rooted in a neighbor must appear here)
        let g1 = Gen::load(0, crate::name_to_state).expect("datapack");
        let g2 = Gen::load(0, crate::name_to_state).expect("datapack");
        let (mut g1, mut g2) = (g1, g2);
        crate::feature::populate(&mut g1, 0, 0);
        crate::feature::populate(&mut g2, 0, 0);
        let m1 = g1.feat_map(0, 0).cloned().unwrap_or_default();
        let m2 = g2.feat_map(0, 0).cloned().unwrap_or_default();
        if m1 != m2 {
            let mut diffs = 0;
            for (k, v) in &m1 {
                match m2.get(k) {
                    Some(v2) if v2 == v => {}
                    other => {
                        if diffs < 20 {
                            eprintln!(
                                "  diff key={} (y={} x={} z={}) {} vs {:?}",
                                k, (k >> 8) as i32 - 64, k & 15, (k >> 4) & 15, v, other
                            );
                        }
                        diffs += 1;
                    }
                }
            }
            for (k, v) in &m2 {
                if !m1.contains_key(k) {
                    if diffs < 40 {
                        eprintln!("  m2-only key={} {}", k, v);
                    }
                    diffs += 1;
                }
            }
            panic!("populate is not deterministic: {} diffs", diffs);
        }
        // cross-chunk: collect writes from chunk (0,0) into neighbor maps
        let neigh: usize = [-1i32, 1]
            .iter()
            .flat_map(|&dx| [-1i32, 1].iter().map(move |&dz| (dx, dz)))
            .map(|(cx, cz)| g1.feat_map(cx, cz).map(|m| m.len()).unwrap_or(0))
            .sum();
        eprintln!("feat_determinism: own={} neighbor-overrides={}", m1.len(), neigh);
        assert!(m1.len() + neigh > 0, "no features generated at all");
    }
}

#[cfg(test)]
mod bedrock_bench {
    /// Diagnostic: is a flat bedrock layer worth its parity cost?
    /// The bedrock band is ~7us per chunk against ~5.7ms for the terrain layer
    /// (about 0.12%), so flattening it does not make the generator materially
    /// lighter and the per-cell vanilla pattern is kept. Run with
    /// `WGBEDROCK=1 cargo test --release time_deep_state_band -- --nocapture`.
    /// Env-gated like the other probes: a parked ignored test counts against
    /// the debt ratchet, and this is a benchmark rather than a skipped assertion.
    #[test]
    fn time_deep_state_band() {
        if std::env::var("WGBEDROCK").is_err() {
            return;
        }
        let mut g = super::Gen::load(0, crate::name_to_state).expect("datapack");
        let cc = g.cols(0, 0).expect("cols");
        let start = std::time::Instant::now();
        let mut n = 0u64;
        let mut acc = 0u64;
        for _ in 0..20 {
            for y in -63..=-60i32 {
                for z in 0..16 {
                    for x in 0..16 {
                        acc = acc.wrapping_add(super::deep_state(x, y, z, &cc) as u64);
                        n += 1;
                    }
                }
            }
        }
        let elapsed = start.elapsed();
        eprintln!("BEDROCK_BENCH cells={n} elapsed={elapsed:?} per_cell={:?} acc={acc}", elapsed / n as u32);
        // Whole-chunk context: how much is the bedrock band against full generation?
        let mut cg = super::Gen::load(0, crate::name_to_state).expect("datapack");
        let t = std::time::Instant::now();
        let mut chunks = 0u32;
        for cx in 0..8i32 {
            for cz in 0..8i32 {
                if cg.cols(cx, cz).is_some() { chunks += 1; }
            }
        }
        eprintln!("CHUNK_BENCH chunks={chunks} elapsed={:?} per_chunk={:?}", t.elapsed(), t.elapsed() / chunks);
    }
}
