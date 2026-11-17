// worldgen.rs — datapack-driven vanilla worldgen slice for mc-spine.
// Loads vanilla worldgen JSONs (noise_settings / density_function / noise)
// from datapacks/<pack>/data/minecraft/worldgen/ and evaluates the vanilla
// density-function pipeline: per-chunk cell grid + trilinear interp.
// Algorithm referenced from vanilla/Pumpkin; implementation is independent.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use crate::block_registry::{breg, bs_parts};
use crate::json::{jbool, jparse, J};

use crate::generation_math::*;
use crate::trace_env::{env_flag, env_str};

// ---------- density functions ----------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum NoisePreset {
    Overworld,
    Nether,
    End,
}
impl NoisePreset {
    pub(crate) fn parse(name: &str) -> Option<Self> {
        match name {
            "overworld" => Some(Self::Overworld),
            "nether" => Some(Self::Nether),
            "end" => Some(Self::End),
            _ => None,
        }
    }
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Overworld => "overworld",
            Self::Nether => "nether",
            Self::End => "end",
        }
    }
}

pub(crate) fn parse_world_seed(value: &str) -> Result<u64, std::num::ParseIntError> {
    value
        .parse::<i64>()
        .map(|seed| seed as u64)
        .or_else(|_| value.parse())
}

/// The `dimension_type` file a preset's chunks belong to. Note the two names that are
/// not the preset: the Nether and the End are `the_nether` / `the_end` on disk.
///
/// One preset, one dimension: the overworld noise settings also cover its caves
/// (`overworld_caves` is a separate dimension_type this port does not load), and it
/// declares the same -64/384 window.
fn dimension_type_file(preset: NoisePreset) -> &'static str {
    match preset {
        NoisePreset::Overworld => "overworld",
        NoisePreset::Nether => "the_nether",
        NoisePreset::End => "the_end",
    }
}

/// The world window one `dimension_type` object declares, or `None` when it is not a
/// window this generator can live in.
///
/// Three refusals, each of which would otherwise silently cut terrain or produce a dump
/// whose length is not a whole number of sections:
/// * `min_y`/`height` missing, non-positive, or not multiples of 16;
/// * a window that starts above the generator's own `min_y`;
/// * a window that ends below the generator's top.
///
/// `None` means "keep `world_* = gen_*`" and the loader traces the reason.
fn dimension_window(j: &J, gen_min_y: i32, gen_height: i32) -> Option<(i32, i32)> {
    let min_y = j.get("min_y").map(J::num)? as i32;
    let height = j.get("height").map(J::num)? as i32;
    if height <= 0 || height % 16 != 0 || min_y % 16 != 0 {
        return None;
    }
    if min_y > gen_min_y || min_y + height < gen_min_y + gen_height {
        return None;
    }
    Some((min_y, height))
}

// ---------- beardifier (vanilla `Beardifier`) ----------
//
// The vanilla `beardifier` density-function node is not in the datapack: `NoiseChunk`
// wraps the router with `add(final_density, BeardifierMarker)` and substitutes the
// chunk's `Beardifier` for the marker (`NoiseChunk.wrap`), so this module inserts the
// same node (`load_preset`) and evaluates it against the chunk's beard data.
//
// Bytecode authority (pinned jar, `tools/bytecode_probe.sh`):
// `Beardifier.compute(dyr$b)`, `Beardifier.a(IIII)` (kernel lookup),
// `Beardifier.a(IDI)` (kernel value = `pow(E, -(x^2+y^2+z^2)/16)`),
// `Beardifier.a(DDD)` = `clampedMap(length(x,y,z), 0, 6, 1, 0)` (the burying value),
// `Mth.fastInvSqrt(double)` (the bit-hack, not `1/sqrt`).
// `TerrainAdjustment` ordinals: NONE, BURY, BEARD_THIN, BEARD_BOX, ENCAPSULATE.

/// `Beardifier.TerrainAdjustment` — the structure's `terrain_adaptation`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BeardAdjustment {
    None,
    Bury,
    BeardThin,
    BeardBox,
    Encapsulate,
}

impl BeardAdjustment {
    pub(crate) fn parse(name: &str) -> Option<Self> {
        match name {
            "none" => Some(Self::None),
            "bury" => Some(Self::Bury),
            "beard_thin" => Some(Self::BeardThin),
            "beard_box" => Some(Self::BeardBox),
            "encapsulate" => Some(Self::Encapsulate),
            _ => None,
        }
    }
}

/// `Beardifier.Rigid`: one piece's bounding box, the structure's terrain
/// adjustment and the piece's ground-level delta.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BeardRigid {
    pub(crate) min: [i32; 3],
    pub(crate) max: [i32; 3],
    pub(crate) adjustment: BeardAdjustment,
    pub(crate) ground_level_delta: i32,
}

/// `JigsawJunction`: a jigsaw connection point. `rigid` records the source
/// piece's projection (RIGID vs TERRAIN_MATCHING); `compute` does not read it,
/// but the collection filter and diagnostics do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BeardJunction {
    pub(crate) source_x: i32,
    pub(crate) source_ground_y: i32,
    pub(crate) source_z: i32,
    pub(crate) delta_y: i32,
    pub(crate) rigid: bool,
}

/// `Beardifier.forStructuresInChunk`: the rigids and junctions of every structure
/// piece that is close to one chunk.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct BeardChunkData {
    pub(crate) rigids: Vec<BeardRigid>,
    pub(crate) junctions: Vec<BeardJunction>,
}

impl BeardChunkData {
    pub(crate) fn is_empty(&self) -> bool {
        self.rigids.is_empty() && self.junctions.is_empty()
    }
}

/// `Beardifier.BEARD_KERNEL_RADIUS`.
const BEARD_KERNEL_RADIUS: i32 = 12;
/// `Beardifier.BEARD_KERNEL_SIZE`.
const BEARD_KERNEL_SIZE: usize = 24;

/// `Beardifier.BEARD_KERNEL`: 24^3 f32 values, `BEARD_KERNEL[z*576 + x*24 + y]`
/// = `kernel(x, y, z)` for offsets relative to the kernel centre.
fn beard_kernel() -> &'static [f32] {
    static KERNEL: std::sync::OnceLock<Box<[f32]>> = std::sync::OnceLock::new();
    KERNEL.get_or_init(|| {
        let mut v = vec![0f32; BEARD_KERNEL_SIZE * BEARD_KERNEL_SIZE * BEARD_KERNEL_SIZE];
        for i in 0..BEARD_KERNEL_SIZE {
            for j in 0..BEARD_KERNEL_SIZE {
                for k in 0..BEARD_KERNEL_SIZE {
                    v[i * 576 + j * 24 + k] = beard_kernel_value(
                        j as i32 - BEARD_KERNEL_RADIUS,
                        k as i32 - BEARD_KERNEL_RADIUS,
                        i as i32 - BEARD_KERNEL_RADIUS,
                    );
                }
            }
        }
        v.into_boxed_slice()
    })
}

/// `Beardifier.a(int, int, int)` → `Beardifier.a(int, double, int)`: the table
/// value `pow(E, -(x^2 + (y+0.5)^2 + z^2)/16)`.
fn beard_kernel_value(x: i32, y: i32, z: i32) -> f32 {
    let y = f64::from(y) + 0.5;
    let xs = f64::from(x);
    let zs = f64::from(z);
    let squared = xs * xs + y * y + zs * zs;
    // Java `Math.pow(Math.E, -squared / 16)`: the value is stored as f32 in the table.
    std::f64::consts::E.powf(-squared / 16.0) as f32
}

/// `Mth.fastInvSqrt(double)`: the seeded bit-hack, *not* `1.0 / sqrt(d)`.
fn fast_inv_sqrt(d: f64) -> f64 {
    let half = 0.5 * d;
    let bits = 6_910_469_410_427_058_090u64.wrapping_sub((d.to_bits() as i64 >> 1) as u64);
    let d = f64::from_bits(bits);
    d * (1.5 - half * d * d)
}

/// `Beardifier.a(int, int, int, int)`: the exp kernel times the ground-plane term.
/// `ground_level_delta` is the y distance whose half-plane the kernel is centred on.
fn beard_contribution(x: i32, y: i32, z: i32, ground_level_delta: i32) -> f64 {
    let (ix, iy, iz) = (x + 12, y + 12, z + 12);
    if !(0..BEARD_KERNEL_SIZE as i32).contains(&ix)
        || !(0..BEARD_KERNEL_SIZE as i32).contains(&iy)
        || !(0..BEARD_KERNEL_SIZE as i32).contains(&iz)
    {
        return 0.0;
    }
    let d = f64::from(ground_level_delta) + 0.5;
    let (xs, zs) = (f64::from(x), f64::from(z));
    let squared = xs * xs + d * d + zs * zs;
    let f = -d * fast_inv_sqrt(squared / 2.0) / 2.0;
    let kernel = beard_kernel()[iz as usize * 576 + ix as usize * 24 + iy as usize];
    f * f64::from(kernel)
}

/// `Beardifier.a(double, double, double)` — `clampedMap(length(x, y, z), 0, 6, 1, 0)`.
fn beard_burying_value(x: f64, y: f64, z: f64) -> f64 {
    let len = (x * x + y * y + z * z).sqrt();
    clamped_map(len, 0.0, 6.0, 1.0, 0.0)
}

/// `Mth.clampedMap(value, min, max, newMin, newMax)`.
fn clamped_map(value: f64, min: f64, max: f64, new_min: f64, new_max: f64) -> f64 {
    let delta = (value - min) / (max - min);
    let t = if delta < 0.0 {
        0.0
    } else if delta > 1.0 {
        1.0
    } else {
        delta
    };
    new_min + t * (new_max - new_min)
}

/// `Beardifier.compute(FunctionContext)`: the density this chunk's structures add to
/// the terrain density at one block position.
pub(crate) fn compute_beardifier(data: &BeardChunkData, x: i32, y: i32, z: i32) -> f64 {
    let mut density = 0.0f64;
    for rigid in &data.rigids {
        let dx = (rigid.min[0] - x).max(x - rigid.max[0]).max(0);
        let dz = (rigid.min[2] - z).max(z - rigid.max[2]).max(0);
        let ground = rigid.min[1] + rigid.ground_level_delta;
        let dy = y - ground;
        let boxed = match rigid.adjustment {
            BeardAdjustment::None => 0,
            BeardAdjustment::Bury | BeardAdjustment::BeardThin => dy,
            BeardAdjustment::BeardBox => (ground - y).max(y - rigid.max[1]).max(0),
            BeardAdjustment::Encapsulate => (rigid.min[1] - y).max(y - rigid.max[1]).max(0),
        };
        density += match rigid.adjustment {
            BeardAdjustment::None => 0.0,
            BeardAdjustment::Bury => {
                beard_burying_value(f64::from(dx), f64::from(boxed) / 2.0, f64::from(dz))
            }
            BeardAdjustment::BeardThin | BeardAdjustment::BeardBox => {
                beard_contribution(dx, boxed, dz, dy) * 0.8
            }
            BeardAdjustment::Encapsulate => {
                beard_burying_value(
                    f64::from(dx) / 2.0,
                    f64::from(boxed) / 2.0,
                    f64::from(dz) / 2.0,
                ) * 0.8
            }
        };
    }
    for junction in &data.junctions {
        let q = x - junction.source_x;
        let r = y - junction.source_ground_y;
        let s = z - junction.source_z;
        density += beard_contribution(q, r, s, r) * 0.4;
    }
    density
}

/// Resolves one chunk's beardifier input. `worldgen` may not import the structure
/// pipeline (`feature` sits above it in `tools/layers_check.sh`), so the pipeline
/// registers this function instead.
pub(crate) type BeardResolver = fn(&mut Gen, i32, i32) -> BeardChunkData;

static GLOBAL_BEARD_RESOLVER: Mutex<Option<BeardResolver>> = Mutex::new(None);

/// Register the process-wide beardifier resolver (`main.rs` and the structure tests).
pub(crate) fn register_beard_resolver(resolver: BeardResolver) {
    *GLOBAL_BEARD_RESOLVER.lock().unwrap() = Some(resolver);
}

fn global_beard_resolver() -> Option<BeardResolver> {
    *GLOBAL_BEARD_RESOLVER.lock().unwrap()
}

#[derive(Clone)]
enum SV {
    V(f32),
    S(Box<Spl>),
}
#[derive(Clone)]
struct Spl {
    coord: usize,
    pts: Vec<(f32, SV, f32)>,
}

/// `dys$z$a`: the rarity mapper of a `weird_scaled_sampler`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RarityValueMapper {
    Type1,
    Type2,
}

impl RarityValueMapper {
    fn parse(name: &str) -> Option<Self> {
        match name {
            "type_1" => Some(Self::Type1),
            "type_2" => Some(Self::Type2),
            _ => None,
        }
    }

    /// `dzf$a.b` (TYPE1) / `dzf$a.a` (TYPE2). Strict comparisons: a value equal
    /// to a threshold takes the next branch, and NaN falls through to the last.
    fn rarity(self, input: f64) -> f64 {
        match self {
            Self::Type1 => {
                if input < -0.5 {
                    0.75
                } else if input < 0.0 {
                    1.0
                } else if input < 0.5 {
                    1.5
                } else {
                    2.0
                }
            }
            Self::Type2 => {
                if input < -0.75 {
                    0.5
                } else if input < -0.5 {
                    0.75
                } else if input < 0.5 {
                    1.0
                } else if input < 0.75 {
                    2.0
                } else {
                    3.0
                }
            }
        }
    }
}

#[derive(Clone)]
enum DF {
    C(f64),
    EndIslands(Box<EndIslands>),
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
    // vanilla `interpolated`: sampled at the preset's cell corners, trilinearly
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
    WeirdScaledSampler {
        input: usize,
        ni: usize,
        mapper: RarityValueMapper,
    },
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
    Blended(Box<BlendedNoise>),
    BlendD(usize),
    Zero,
    /// Vanilla `BeardifierOrMarker`: the chunk's `BeardChunkData` decides the value.
    Beardifier,
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
        legacy_seed: u64,
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
    Temperature,
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

// Adapted from Horizon's worldgen/biome_temperature.rs (0564ab0), but 1.21.1
// fixes the altitude boundary at 80, not the newer sea_level + 17. Keep its
// PerlinSimplex intermediates in f64 and narrow only where Biome's bytecode does.
#[derive(Clone, Copy)]
struct BiomeWeather {
    temperature: f32,
    frozen: bool,
}
impl BiomeWeather {
    fn parse(j: &J) -> Option<Self> {
        let J::Num(temperature) = j.get("temperature")? else {
            return None;
        };
        let frozen = match j.get("temperature_modifier") {
            None => false,
            Some(J::Str(s)) if s == "none" => false,
            Some(J::Str(s)) if s == "frozen" => true,
            _ => return None,
        };
        Some(Self {
            temperature: *temperature as f32,
            frozen,
        })
    }

    fn at(self, x: i32, y: i32, z: i32) -> f32 {
        static NOISES: std::sync::OnceLock<(OSimp, OSimp, OSimp)> = std::sync::OnceLock::new();
        let mut base = self.temperature;
        if self.frozen || y > 80 {
            let (height, frozen, info) = NOISES.get_or_init(|| {
                (
                    OSimp::new_l(&mut Lcg::new(1234), &[0]),
                    OSimp::new_l(&mut Lcg::new(3456), &[-2, -1, 0]),
                    OSimp::new_l(&mut Lcg::new(2345), &[0]),
                )
            });
            let (dx, dz) = (x as f64, z as f64);
            if self.frozen
                && frozen.sample(dx * 0.05, dz * 0.05) * 7.0 + info.sample(dx * 0.2, dz * 0.2) < 0.3
                && info.sample(dx * 0.09, dz * 0.09) < 0.8
            {
                base = 0.2;
            }
            if y > 80 {
                let variation =
                    (height.sample((x as f32 / 8.0) as f64, (z as f32 / 8.0) as f64) * 8.0) as f32;
                base -= (variation + y as f32 - 80.0) * 0.05 / 40.0;
            }
        }
        base
    }

    fn cold(self, x: i32, y: i32, z: i32) -> bool {
        let warm = self.at(x, y, z) >= 0.15;
        !warm
    }
}

#[derive(Clone)]
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

/// One terrain cell as the surface scan sees it before the rule runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SurfaceCell {
    Air,
    Fluid,
    Solid,
}

/// Classify one pre-surface cell. Vanilla scans the chunk's actual block states;
/// an aquifer barrier (`None`) is solid, and a non-fluid aquifer state is solid
/// even when the density is non-positive. Such cells can still receive a
/// surface material and must stay in the surface window.
/// The four-block material window below the actual pre-surface top. A surface
/// material may be applied to an aquifer/default solid above the density top,
/// but not to deep solids far below the saved heightmap.
fn surface_material_applies(y: i32, surface_top: i32) -> bool {
    y >= surface_top.saturating_sub(4)
}

fn classify_surface_cell(
    density: f64,
    aquifer: Option<u32>,
    aquifer_air: bool,
    aquifer_fluid: bool,
) -> SurfaceCell {
    if density > 0.0 || aquifer.is_none() {
        SurfaceCell::Solid
    } else if aquifer_air {
        SurfaceCell::Air
    } else if aquifer_fluid {
        SurfaceCell::Fluid
    } else {
        SurfaceCell::Solid
    }
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
    nt: HashMap<usize, bool>, // per-column memo for 2d noise_threshold
    memo: HashMap<(usize, i32, i32, i32), f64>,
}
impl SC {
    fn new() -> SC {
        SC {
            x: 0,
            y: 0,
            z: 0,
            run_depth: 0,
            sec: 0.0,
            sec_ok: false,
            steep: None,
            sda: 0,
            sdb: 0,
            fluid: i32::MIN,
            est_min: i32::MAX,
            nt: HashMap::new(),
            memo: HashMap::new(),
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

/// The immutable half of the generator: everything read from the datapack and the
/// noise settings at `init`, plus the resolved block states and the seed.
///
/// `Clone` exists only so `Arc::make_mut` keeps the construction-time writes
/// working; nothing mutates it after `load_preset` returns. A worker gets an
/// `Arc<GenData>`/`&GenData`, which is the point of the split in
/// `specs/chunk-parallelism`: the read-only inputs need no world lock.
#[derive(Clone)]
pub struct GenData {
    pub(crate) preset: NoisePreset,
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
    btree: Option<BT>,
    leaf_ps: Vec<[[i32; 2]; 7]>,
    biome_names: Vec<String>,
    biome_weather: Vec<BiomeWeather>,
    pub sea: i32,
    /// The **generator** window from `worldgen/noise_settings/<preset>.json`'s `noise`
    /// object. Terrain, density, aquifers, carvers, surface rules and the biome quart
    /// sweep are defined over exactly this range.
    pub min_y: i32,
    pub height: i32,
    /// The **world** window from `dimension_type/<dimension>.json`. Chunk sections,
    /// heightmaps, the offline dump length and structure/furniture bounds are defined
    /// over this range. 1.21.1 makes them different: the Nether and the End generate
    /// 0..127 but store 0..255 (see `dim_height_256_design.md`). It equals
    /// `min_y`/`height` whenever the pack has no usable `dimension_type` object, so a
    /// fixture that predates this split keeps its exact meaning.
    pub world_min_y: i32,
    pub world_height: i32,
    // NoiseSettings.getCellHeight() = size_vertical * 4
    noise_cell_h: i32,
    noise_cell_w: i32,
    legacy_random_source: bool,
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
    aq: Option<Aqd>,
    world_seed: u64,
}
pub struct Gen {
    /// The immutable inputs. `Deref` keeps every existing field name working;
    /// `DerefMut` exists only for construction (see below).
    pub(crate) data: std::sync::Arc<GenData>,
    // scratch memo for the vein sampler's density evaluations
    memo: HashMap<(usize, i32, i32, i32), f64>,
    aq_lv: HashMap<(i32, i32, i32), (i32, u8)>,
    aq_lvo: Vec<(i32, i32, i32)>,
    aq_near: HashMap<(i32, i32, i32), (i32, i32, i32)>,
    aq_neo: Vec<(i32, i32, i32)>,
    prelim_c: HashMap<(i32, i32), i32>,
    /// per chunk: the distinct biomes of its section palettes, i.e. what vanilla's
    /// `applyBiomeDecoration` unions over the 3x3 before deciding which features run
    chunk_biomes: HashMap<(i32, i32), std::sync::Arc<Vec<String>>>,
    chunk_biomes_ord: std::collections::VecDeque<(i32, i32)>,
    prelim_ord: Vec<(i32, i32)>,
    clim_c: HashMap<(i32, i32), ([i64; 5], f64)>,
    clim_ord: Vec<(i32, i32)>,
    // Surface-rule biome leaf cache; Nether's fuzzy source keys actual blocks.
    bioq: HashMap<(i32, i32, i32), u16>,
    bioq_ord: std::collections::VecDeque<(i32, i32, i32)>,
    col_c: HashMap<(i32, i32), std::sync::Arc<ChunkCols>>,
    col_ord: Vec<(i32, i32)>,
    /// `ChunkGenerator.getBaseColumn` columns: the noise-only answer a structure's
    /// `findGenerationPoint` scans. Keyed like `col_c` but *not* interchangeable with
    /// it: a base column has no beardifier and no surface rule, so it is pure in
    /// (seed, preset, chunk) and may be cached even while a beard is resolving.
    base_c: HashMap<(i32, i32), std::sync::Arc<ChunkCols>>,
    base_ord: std::collections::VecDeque<(i32, i32)>,
    /// Per-chunk beardifier input, keyed like `col_c`. Pure in (seed, cx, cz): the
    /// resolver reads structure starts, which are functions of the seed only.
    beard_c: HashMap<(i32, i32), std::sync::Arc<BeardChunkData>>,
    beard_ord: std::collections::VecDeque<(i32, i32)>,
    /// Per-generator resolver override (tests); the world registers a global one.
    beard_resolver: Option<BeardResolver>,
    /// Non-zero while a beardifier resolution is in progress: a nested `cols` (a
    /// structure's heightmap query) must not resolve again, or it would describe
    /// terrain shaped by the structure it is trying to place.
    beard_depth: u32,
    // feature layer: chunk -> {(y-min)<<8|z<<4|x -> state}
    feat_c: HashMap<(i32, i32), HashMap<u32, u32>>,
    /// Container contents staged by template placement during feature population.
    /// Generation-owned: they are moved into the runtime container store as soon as
    /// this generator's lock is released (see `block_state`), never kept as a second
    /// live store.
    staged_payloads: Vec<((i32, i32, i32), crate::generation_payload::GenerationPayload)>,
    feat_ord: Vec<(i32, i32)>,
    deep_c: HashMap<(i32, i32, i32), u32>,
    deep_ord: std::collections::VecDeque<(i32, i32, i32)>,
    pop_done: HashSet<(i32, i32)>,
    /// `ChunkAccess.postProcessing` seen across chunks: the positions
    /// `StructurePiece.placeBlock` marked, keyed by the chunk that owns them and
    /// kept in mark order. A chunk's bucket is consumed by
    /// `LevelChunk.postProcessGeneration` once the chunk is promoted, which native
    /// only allows after the whole 3x3 around it is `FULL`.
    shape_marks: HashMap<(i32, i32), Vec<(i32, i32, i32)>>,
    /// The chunk whose FEATURES pass is running (`None` outside one): the centre of the
    /// step's `blockStateWriteRadius` window. See `feature_write_allowed`.
    pub(crate) pop_active: Option<(i32, i32)>,
    // Sources reconstructed after loading must recreate terrain but not enqueue
    // the generation ticks a second time.
    restored_tick_sources: HashSet<(i32, i32)>,
    suppress_generated_ticks: bool,
    /// `ChunkAccess`'s carve stage: one record per chunk, holding the latch `begin_carve`
    /// reads and both outputs the stage writes (see `CarveStage`). **Nothing else may
    /// hold a carve claim or a post-carve WG value** — three containers with three
    /// lifetimes let a claim outlive the output it stood for.
    carve_c: HashMap<(i32, i32), CarveStage>,
    /// The `carve_c` records that carry a WG value, oldest first: the retention bound
    /// lives in exactly one place (`set_wg_after_carve`).
    carve_ord: std::collections::VecDeque<(i32, i32)>,
    /// Generation metadata persists independently of evictable block caches.
    pub(crate) mineshaft_spawners: HashSet<(i32, i32, usize)>,
    pub(crate) payload_sources: HashSet<(i32, i32)>,
    /// Decoration region whose lexicographic scan order to follow (`MC_SPINE_POP_REGION`).
    pop_region: Option<(i32, i32, i32, i32)>,
    // per column: (highest non-air + 1, highest solid + 1) written by the feature layer
    feat_hm: HashMap<(i32, i32), (i32, i32)>,
    // Each generator owns its pending ticks; only the active world's handles are published.
    generated_ticks: GeneratedTickQueues,
}

impl std::ops::Deref for Gen {
    type Target = GenData;
    fn deref(&self) -> &Self::Target {
        &self.data
    }
}

impl std::ops::DerefMut for Gen {
    /// Construction-time only: `load_preset` fills the immutable fields after the
    /// literal. `Arc::make_mut` clones when a snapshot is shared, so a write after
    /// the generator is published would be a silent copy rather than an error —
    /// keep mutation out of this path.
    fn deref_mut(&mut self) -> &mut Self::Target {
        std::sync::Arc::make_mut(&mut self.data)
    }
}

#[derive(Clone)]
struct Aqd {
    barrier: usize,
    flood: usize,
    spread: usize,
    lava: usize,
    split: Split,
    water: u32,
    lava_st: u32,
}

/// Chunks whose post-carve WG heightmaps stay resident. A value is a deterministic scan
/// of the carved terrain, so re-running the carver pass reproduces it, but it is *not*
/// re-readable from a dropped block bucket: that is why this bound belongs to the record
/// and not to `feat_c`. Approved retention for the first chunk-state repair — do not
/// shrink it to the feature bucket's bound.
const CARVE_WG_BOUND: usize = 4096;

/// Chunks whose FEATURES-layer bucket (`feat_c` + `pop_done`) stays resident. Unchanged
/// by the chunk-state repair: it is a cache of block writes, not of stage identity.
const FEATURE_BUCKET_BOUND: usize = 384;

/// One chunk's carve stage — the record that replaces the three containers that used to
/// describe it (`carved`, `wg_after_carve`, `wg_after_carve_ord`).
///
/// `blocks_live` is the carve claim; there is no separate "this chunk is carved" set.
/// A record can retain a WG value after its block bucket and claim have been dropped.
/// The two outputs deliberately have different lifetimes:
///
/// * `wg` — `ChunkAccess`'s WORLD_SURFACE_WG `[0..256)` / OCEAN_FLOOR_WG `[256..512)` as
///   of the end of the carver pass, index `lz * 16 + lx`, `topSolid + 1`. Vanilla creates
///   a chunk's WG maps while it still carries the SURFACE status and updates them per
///   write, so the carver pass lowers them and the FEATURES status afterwards updates
///   only the POST_FEATURES maps. Retained up to `CARVE_WG_BOUND` chunks.
/// * `blocks_live` — whether the pass' block writes are still materialised in `feat_c`.
///   That map is a `FEATURE_BUCKET_BOUND`-chunk LRU shared with the feature layer, so it
///   drops carve writes while `wg` may still be resident; the claim has to fall with the
///   writes or the next FEATURES pass reads uncarved stone (`apply_carvers` would return
///   at `begin_carve`) against a post-carve WG value.
///
/// `wg` owns its own lifetime precisely so the 384-chunk feature bound cannot silently
/// truncate the 4096-chunk WG retention, and `blocks_live` owns the claim precisely so
/// the reverse cannot happen either. Neither field is written by anything but the
/// lifecycle methods below.
pub(crate) struct CarveStage {
    wg: Option<Box<[i32; 512]>>,
    blocks_live: bool,
}

/// `AQ_TRACE="<cx>,<cz>"` prints the aquifer's decision terms for that chunk.
fn aq_trace_at(x: i32, z: i32) -> bool {
    use std::sync::OnceLock;
    static SPEC: OnceLock<Option<(i32, i32)>> = std::sync::OnceLock::new();
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
            let name = p
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            (name != "vanilla", name)
        });
        dirs
    }

    /// Read one resource with the same later-pack-wins ordering as JSON inventories.
    pub(crate) fn dp_resource(kind: &str, id: &str, extension: &str) -> Option<Vec<u8>> {
        Self::dp_resource_in(&Self::packs(), kind, id, extension)
    }
    fn dp_resource_in(
        packs: &[std::path::PathBuf],
        kind: &str,
        id: &str,
        extension: &str,
    ) -> Option<Vec<u8>> {
        let (namespace, path) = id.split_once(':').unwrap_or(("minecraft", id));
        let valid = |s: &str, slash: bool| {
            !s.is_empty()
                && s.bytes().all(|b| {
                    b.is_ascii_lowercase()
                        || b.is_ascii_digit()
                        || b"._-".contains(&b)
                        || (slash && b == b'/')
                })
        };
        if !valid(namespace, false)
            || matches!(namespace, "." | "..")
            || !valid(path, true)
            || std::path::Path::new(path)
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return None;
        }
        for pack in packs.iter().rev() {
            let file = pack
                .join("data")
                .join(namespace)
                .join(kind)
                .join(format!("{path}.{extension}"));
            match std::fs::read(file) {
                Ok(bytes) => return Some(bytes),
                Err(e)
                    if e.kind() == std::io::ErrorKind::NotFound
                        || e.kind() == std::io::ErrorKind::NotADirectory => {}
                Err(_) => return None,
            }
        }
        None
    }

    pub(crate) fn dp_files(subdir: &str) -> HashMap<String, Vec<u8>> {
        Self::dp_files_any(&[subdir])
    }

    /// Merge several candidate directory names, later *names* winning within a pack.
    /// The reference build's own data uses 1.21.1's layout (`worldgen/configured_feature`)
    /// while a converted pack may use the newer `worldgen/feature`; both have to load,
    /// because the data is the reference and the conversion is not.
    pub(crate) fn dp_files_any(subdirs: &[&str]) -> HashMap<String, Vec<u8>> {
        let mut m = HashMap::new();
        for subdir in subdirs {
            for (k, v) in Self::dp_files_one(subdir) {
                m.insert(k, v);
            }
        }
        m
    }

    fn dp_files_one(subdir: &str) -> HashMap<String, Vec<u8>> {
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

    /// `LOAD_TRACE=1` names the step at which loading gave up. Without it a missing
    /// piece of the worldgen data looks exactly like a wrong world (CV2).
    fn load_trace(why: &str) {
        if std::env::var("LOAD_TRACE").is_ok() {
            eprintln!("[load] {why}");
        }
    }

    pub fn load(seed: u64, res: fn(&str) -> Option<u32>) -> Option<Gen> {
        Self::load_preset(seed, res, NoisePreset::Overworld)
    }

    pub(crate) fn load_preset(
        seed: u64,
        res: fn(&str) -> Option<u32>,
        preset: NoisePreset,
    ) -> Option<Gen> {
        #[cfg(test)]
        {
            if std::env::var_os("LOAD_TRACE").is_none()
                && std::env::var_os("MC_SPINE_VEINDBG").is_none()
            {
                use std::sync::{Mutex, OnceLock};
                type Key = (u64, u8, usize, bool, Vec<std::path::PathBuf>);
                static CACHE: OnceLock<Mutex<HashMap<Key, Option<std::sync::Arc<GenData>>>>> =
                    OnceLock::new();
                let preset_id = match preset {
                    NoisePreset::Overworld => 0,
                    NoisePreset::Nether => 1,
                    NoisePreset::End => 2,
                };
                let noaq = std::env::var_os("MC_SPINE_NOAQ").is_some();
                let key = (seed, preset_id, res as usize, noaq, Self::packs());
                let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
                if let Some(hit) = cache.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
                    return hit
                        .as_ref()
                        .map(|d| Self::fresh_scratch(std::sync::Arc::clone(d)));
                }
                let built = Self::load_preset_uncached(seed, res, preset);
                let entry = built.as_ref().map(|g| std::sync::Arc::clone(&g.data));
                cache
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(key, entry);
                return built;
            }
        }
        Self::load_preset_uncached(seed, res, preset)
    }

    fn load_preset_uncached(
        seed: u64,
        res: fn(&str) -> Option<u32>,
        preset: NoisePreset,
    ) -> Option<Gen> {
        let files = Self::df_files();
        let mut g = Gen {
            data: std::sync::Arc::new(GenData {
                preset,
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
                biome_weather: Vec::new(),
                sea: 63,
                min_y: -64,
                height: 384,
                world_min_y: -64,
                world_height: 384,
                noise_cell_h: 8,
                noise_cell_w: 4,
                legacy_random_source: false,
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
                vein: None,
                world_seed: seed,
            }),
            aq_lv: HashMap::new(),
            aq_lvo: Vec::new(),
            aq_near: HashMap::new(),
            aq_neo: Vec::new(),
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
            base_c: HashMap::new(),
            base_ord: std::collections::VecDeque::new(),
            beard_c: HashMap::new(),
            beard_ord: std::collections::VecDeque::new(),
            beard_resolver: None,
            beard_depth: 0,
            feat_c: HashMap::new(),
            staged_payloads: Vec::new(),
            feat_ord: Vec::new(),
            deep_c: HashMap::new(),
            deep_ord: std::collections::VecDeque::new(),
            pop_done: HashSet::new(),
            shape_marks: HashMap::new(),
            pop_active: None,
            restored_tick_sources: HashSet::new(),
            suppress_generated_ticks: false,
            carve_c: HashMap::new(),
            carve_ord: std::collections::VecDeque::new(),
            pop_region: std::env::var("MC_SPINE_POP_REGION").ok().and_then(|v| {
                let p: Vec<i32> = v.split(',').filter_map(|n| n.parse().ok()).collect();
                if p.len() == 4 {
                    Some((p[0], p[1], p[2], p[3]))
                } else {
                    None
                }
            }),
            memo: HashMap::new(),
            mineshaft_spawners: HashSet::new(),
            payload_sources: HashSet::new(),
            feat_hm: HashMap::new(),
            generated_ticks: GeneratedTickQueues::default(),
        };
        // noise settings (sea level, bounds, router names)
        let mut ns_json: Option<J> = None;
        'outer: for p in Gen::packs() {
            let f = p.join(format!(
                "data/minecraft/worldgen/noise_settings/{}.json",
                preset.name()
            ));
            if let Ok(b) = std::fs::read(&f) {
                ns_json = jparse(&b);
                g.base = p.to_string_lossy().to_string();
                break 'outer;
            }
        }
        let ns = match ns_json {
            Some(j) => j,
            None => {
                Self::load_trace(&format!(
                    "noise_settings/{}.json was not found or is not JSON",
                    preset.name()
                ));
                return None;
            }
        };
        g.sea = ns.get("sea_level").map(J::num).unwrap_or(63.0) as i32;
        g.legacy_random_source = jbool(&ns, "legacy_random_source", false);
        if let Some(n) = ns.get("noise") {
            g.min_y = n.get("min_y").map(J::num).unwrap_or(-64.0) as i32;
            g.height = n.get("height").map(J::num).unwrap_or(384.0) as i32;
            g.noise_cell_h = (n.get("size_vertical").map(J::num).unwrap_or(2.0) as i32 * 4).max(1);
            g.noise_cell_w = n.get("size_horizontal").map(J::num).unwrap_or(1.0) as i32 * 4;
            if g.noise_cell_w <= 0
                || g.height <= 0
                || 16 % g.noise_cell_w != 0
                || g.height % g.noise_cell_h != 0
            {
                Self::load_trace("noise cell sizes do not divide the chunk dimensions");
                return None;
            }
        }
        let router = ns.get("noise_router");
        // The **world** window: `dimension_type/<dimension>.json`'s `min_y`/`height`.
        // It is a different fact from the noise settings above and 1.21.1 differs by 2x
        // for both non-overworld dimensions (noise 0/128, dimension 0/256). Only the
        // storage-side readers use it - chunk sections, heightmaps, the offline dump
        // length, structure/furniture bounds - never the density/aquifer/carver/surface
        // path, which is generator height in vanilla too. Absent or unusable, the world
        // window *is* the generator window, which keeps every pre-split fixture exact.
        g.world_min_y = g.min_y;
        g.world_height = g.height;
        {
            let file = dimension_type_file(preset);
            let mut candidates: Vec<std::path::PathBuf> = Vec::new();
            if !g.base.is_empty() {
                // the pack the noise settings came from answers first, so one pack cannot
                // contribute a generator and a different pack its world
                candidates.push(std::path::PathBuf::from(&g.base));
            }
            candidates.extend(Gen::packs());
            for p in candidates {
                let f = p.join(format!("data/minecraft/dimension_type/{file}.json"));
                let Ok(b) = std::fs::read(&f) else { continue };
                let Some(j) = jparse(&b) else {
                    Self::load_trace(&format!(
                        "dimension_type/{file}.json is not JSON; the world window stays {}/{}",
                        g.min_y, g.height
                    ));
                    break;
                };
                match dimension_window(&j, g.min_y, g.height) {
                    Some((min_y, height)) => {
                        g.world_min_y = min_y;
                        g.world_height = height;
                    }
                    // Fail closed: a window that is not whole sections or that does not
                    // contain the terrain the generator makes would truncate it.
                    None => Self::load_trace(&format!(
                        "dimension_type/{file}.json is not a window containing the generator's \
                         {}/{}",
                        g.min_y, g.height
                    )),
                }
                break;
            }
        }
        // 1.21.1 writes the router entries as inline density functions (the newer
        // dialect writes references), so this has to go through `router_df`, which
        // accepts either.
        let Some(fd) = router.and_then(|r| r.get("final_density")) else {
            Self::load_trace("noise_settings has no noise_router.final_density");
            return None;
        };
        let Some(idx) = router_df(&mut g, &files, fd) else {
            Self::load_trace("noise_router.final_density did not resolve");
            return None;
        };
        // Vanilla's chunk pipeline never reads `final_density` bare: `NoiseChunk`
        // builds `cacheAllInCell(add(router.finalDensity, BeardifierMarker))` and
        // `wrap` substitutes the chunk's `Beardifier` for the marker. The datapack has
        // no such node, so the same edge is added here once, at load.
        let base = idx;
        let beard = g.dfs.len();
        g.dfs.push(DF::Beardifier);
        let sum = g.dfs.len();
        g.dfs.push(DF::Add(base, beard));
        g.final_density = sum;
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
                    // the resolver `Gen::load` was handed, not the root binding: a
                    // generation module may not reach `crate::` for names.
                    let st = |n: &str| res(n).unwrap_or(0);
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
                    .and_then(|j| router_df(&mut g, &files, j))
                {
                    Some(v) => ids[i] = v,
                    None => {
                        Self::load_trace(&format!("climate router field {k:?} did not resolve"));
                        ok = false;
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
        // Vanilla's built-in climate tree, exported by tools/biome_tree_from_jar.py.
        // The preset JSON only names the builder; it does not contain its intervals.
        if preset == NoisePreset::End {
            for name in [
                "minecraft:the_end",
                "minecraft:end_highlands",
                "minecraft:end_midlands",
                "minecraft:small_end_islands",
                "minecraft:end_barrens",
            ] {
                let leaf = g.biome_names.len() as u16;
                g.biome_names.push(name.to_string());
                g.bleaf.insert(name.to_string(), leaf);
            }
        } else {
            let tp = std::path::Path::new(&g.base).join("multi_noise_biome_tree.json");
            if let Ok(b) = std::fs::read(&tp) {
                if let Some(j) = jparse(&b) {
                    if let Some(ow) = j.get(preset.name()) {
                        let d = std::sync::Arc::make_mut(&mut g.data);
                        d.btree = bt_parse(ow, &mut d.biome_names, &mut d.bleaf, &mut d.leaf_ps);
                    }
                }
            }
            if g.btree.is_none() {
                Self::load_trace(&format!(
                    "no valid {} biome tree at {}",
                    preset.name(),
                    tp.display()
                ));
            }
        }
        // SurfaceRules.Temperature reads biome weather, not the noise-router
        // temperature coordinate. Bind the settings to the same leaf identities.
        let biome_files = Self::dp_files("biome");
        for name in g.biome_names.clone() {
            let Some(weather) = biome_files
                .get(&name)
                .and_then(|b| jparse(b))
                .and_then(|j| BiomeWeather::parse(&j))
            else {
                Self::load_trace(&format!("biome weather did not parse: {name}"));
                return None;
            };
            g.biome_weather.push(weather);
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
            g.def_st = Self::block_state(ns.get("default_block")?, res)?;
            g.def_fl = Self::block_state(ns.get("default_fluid")?, res)?;
            g.bands = create_bands(&mut g.factory.of_str("minecraft:clay_bands"), res);
            // Vanilla NoiseChunk computes the preliminary surface from
            // `noise_router.initial_density_without_jaggedness` in code (threshold
            // 0.390625, quart-aligned column, cell-height steps). Older converted
            // packs carry an explicit `aquifers.surface_level` find_top_surface or a
            // `chunk_surface_level` router entry; keep those as legacy fallbacks.
            let prelim = router
                .and_then(|r| r.get("initial_density_without_jaggedness"))
                .or_else(|| ns.get("aquifers").and_then(|a| a.get("surface_level")))
                .or_else(|| router.and_then(|r| r.get("chunk_surface_level")));
            if let Some(j) = prelim {
                g.prelim_df = router_df(&mut g, &files, j);
                if g.prelim_df.is_none() {
                    Self::load_trace("preliminary surface density did not resolve");
                }
            }
            if let Some(rule) = ns.get("surface_rule").or_else(|| ns.get("material_rule")) {
                g.surface_rule = g.rule_node(rule, &files, &rfiles, &cfiles, res);
                if g.surface_rule.is_none() {
                    Self::load_trace("noise_settings surface_rule/material_rule did not resolve");
                    return None;
                }
            }
        }
        // aquifer config: noise DF ids + positional deriver (minecraft:aquifer)
        let noaq = std::env::var("MC_SPINE_NOAQ").is_ok() || !jbool(&ns, "aquifers_enabled", true);
        // 1.21.1 has no top-level `aquifers`: the same four functions sit in
        // `noise_router` under their own names. 1.21.1's deep-aquifer test reads
        // `erosion` and `depth` (OverworldBiomeBuilder.isDeepDark; NoiseRouter's
        // h()/i() accessors), not an `exclusion` node; a converted pack's
        // `exclusion` is not applied.
        let aq = ns.get("aquifers");
        if !noaq && (aq.is_some() || router.is_some()) {
            let get = |g: &mut Gen, k: &str| -> Option<usize> {
                let j = match aq
                    .and_then(|a| a.get(k))
                    .or_else(|| router.and_then(|r| r.get(k)))
                {
                    Some(j) => j,
                    None => {
                        Self::load_trace(&format!(
                            "no aquifer function {k:?} in aquifers or noise_router"
                        ));
                        return None;
                    }
                };
                router_df(g, &files, j)
            };
            let (b, f, s, l) = (
                get(&mut g, "barrier"),
                get(&mut g, "fluid_level_floodedness"),
                get(&mut g, "fluid_level_spread"),
                get(&mut g, "lava"),
            );
            if let (Some(barrier), Some(flood), Some(spread), Some(lava)) = (b, f, s, l) {
                g.aq = Some(Aqd {
                    barrier,
                    flood,
                    spread,
                    lava,
                    split: g.factory.of_str("minecraft:aquifer").splitter(),
                    water: res("minecraft:water").unwrap_or(80),
                    lava_st: res("minecraft:lava").unwrap_or(1),
                });
            }
        }
        Some(g)
    }

    // ---- material rule parsing ----

    fn block_state(j: &J, res: fn(&str) -> Option<u32>) -> Option<u32> {
        let Some((name, props)) = bs_parts(j) else {
            Self::load_trace(&format!("invalid surface block state {j:?}"));
            return None;
        };
        let Some(default) = res(&name) else {
            Self::load_trace(&format!("unknown surface block {name:?}"));
            return None;
        };
        // Keep state/property decoding with the registry, rather than a second
        // surface-only list of state offsets. The callback still resolves names.
        match props {
            Some(props) => Some(breg()?.state_of(&name, Some(props))),
            None => Some(default),
        }
    }

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
        let Some(j) = rf.get(name).and_then(|b| jparse(b)) else {
            Self::load_trace(&format!(
                "surface rule reference {name:?}: missing or invalid JSON"
            ));
            return None;
        };
        let idx = self.rules.len();
        self.rule_ids.insert(name.to_string(), idx);
        self.rules.push(Rule::Block(0)); // placeholder for recursion
        let Some(r) = self.parse_rule(&j, files, rf, cf, res) else {
            self.rule_ids.remove(name);
            Self::load_trace(&format!("surface rule reference {name:?}: did not parse"));
            return None;
        };
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
                let Some(r) = self.parse_rule(j, files, rf, cf, res) else {
                    Self::load_trace(&format!("surface rule did not parse: {j:?}"));
                    return None;
                };
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
                    .as_arr()?
                    .iter()
                    .map(|c| self.rule_node(c, files, rf, cf, res))
                    .collect::<Option<_>>()?,
            ),
            "condition" => Rule::Cond(
                self.cond_node(j.get("if_true")?, cf)?,
                self.rule_node(j.get("then_run")?, files, rf, cf, res)?,
            ),
            "block" => Rule::Block(Self::block_state(j.get("result_state")?, res)?),
            "bandlands" => Rule::Bandlands,
            "ore_vein" => {
                // 1.21.1 has no `material_rule`; when the 1.21.1 sampler is
                // active (ore_veins_enabled), the 1.21.2+ rule would place a
                // second vein on top of it.
                if self.vein.is_some() {
                    return Some(Rule::Seq(Box::new([])));
                }
                let density = self.node(j.get("density")?, files)?;
                // bounds check fast-path: density root is range_choice on minecraft:y
                let (mut ymin, mut ymax) = (i32::MIN, i32::MAX);
                let yid = self.names.get("minecraft:y").copied();
                if let (Some(yid), DF::RangeChoice { i, mi, ma, .. }) = (yid, &self.dfs[density]) {
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
            _ => {
                Self::load_trace(&format!("unknown surface rule type {ty:?}"));
                return None;
            }
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
        let Some(j) = cf.get(name).and_then(|b| jparse(b)) else {
            Self::load_trace(&format!(
                "surface condition reference {name:?}: missing or invalid JSON"
            ));
            return None;
        };
        let idx = self.conds.len();
        self.cond_ids.insert(name.to_string(), idx);
        self.conds.push(Cond::False);
        let Some(c) = self.parse_cond(&j, cf) else {
            self.cond_ids.remove(name);
            Self::load_trace(&format!(
                "surface condition reference {name:?}: did not parse"
            ));
            return None;
        };
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
                    legacy_seed: legacy_named_factory_seed(
                        self.world_seed,
                        j.get("random_name")?.str(),
                    ),
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
            "temperature" => Cond::Temperature,
            _ => {
                Self::load_trace(&format!("unknown surface condition type {ty:?}"));
                return None;
            }
        })
    }

    fn resolve_ref(&mut self, files: &HashMap<String, Vec<u8>>, name: &str) -> Option<usize> {
        if let Some(&i) = self.names.get(name) {
            return Some(i);
        }
        let trace = |why: &str| {
            if std::env::var("LOAD_TRACE").is_ok() {
                eprintln!("[load] density reference {name}: {why}");
            }
        };
        let b = match files.get(name) {
            Some(b) => b,
            None => {
                trace("no such file in the pack");
                return None;
            }
        };
        let j = match jparse(b) {
            Some(j) => j,
            None => {
                trace("the file is not valid JSON");
                return None;
            }
        };
        let idx = self.dfs.len();
        self.names.insert(name.to_string(), idx);
        self.dfs.push(DF::Zero); // placeholder for recursion safety
        let d = match self.parse_df(&j, files) {
            Some(d) => d,
            None => {
                // A failed parse must not leave this name pointing at the
                // recursion placeholder: a retry has to reach `parse_df` again
                // instead of silently resolving to `DF::Zero`.
                self.names.remove(name);
                trace("its own definition did not parse");
                return None;
            }
        };
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
                let d = match self.parse_df(j, files) {
                    Some(d) => d,
                    None => {
                        Self::load_trace(&format!(
                            "density node type {:?} did not parse",
                            j.get("type").map(|t| t.str()).unwrap_or("<none>")
                        ));
                        return None;
                    }
                };
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
        if self.legacy_random_source {
            let noise = match name {
                "minecraft:temperature" => Some(normal_legacy_nether(self.world_seed)),
                "minecraft:vegetation" => {
                    Some(normal_legacy_nether(self.world_seed.wrapping_add(1)))
                }
                "minecraft:offset" => Some(normal_new_amps(&mut Xor::seed(0), 0, &[0.0])),
                _ => None,
            };
            if let Some(noise) = noise {
                let id = self.noises.len();
                self.noises.push(noise);
                self.noise_names.insert(name.to_owned(), id);
                return Some(id);
            }
        }
        let path = format!(
            "{}/data/minecraft/worldgen/noise/{}.json",
            self.base,
            name.rsplit(':').next().unwrap_or(name)
        );
        let b = std::fs::read(&path).ok()?;
        let j = jparse(&b)?;
        // 1.21.1 noise files: `firstOctave` + `amplitudes`. The converted pack re-encoded
        // the same octaves as `base_octave`/`base_amplitude`/`octave_count`; both load,
        // and both construct the noise the modern way (see `normal_new_amps`).
        if let Some(amps) = j.get("amplitudes").and_then(|a| a.as_arr()) {
            let first = j
                .get("firstOctave")
                .or_else(|| j.get("first_octave"))
                .map(|v| v.num())
                .unwrap_or(-4.0) as i32;
            let amps: Vec<f64> = amps.iter().map(|v| v.num()).collect();
            let mut rng = self.factory.of_str(name);
            let n = if self.legacy_random_source {
                normal_legacy_named(self.world_seed, name, first, &amps)
            } else {
                crate::generation_math::normal_new_amps(&mut rng, first, &amps)
            };
            self.noises.push(n);
            let id = self.noises.len() - 1;
            self.noise_names.insert(name.to_string(), id);
            return Some(id);
        }
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
        let id = self.noises.len() - 1;
        self.noise_names.insert(name.to_string(), id);
        Some(id)
    }

    fn parse_spline(&mut self, j: &J, files: &HashMap<String, Vec<u8>>) -> Option<Spl> {
        let coord = self.node(j.get("coordinate")?, files)?;
        let mut pts = Vec::new();
        for p in j.get("points")?.as_arr()? {
            let loc = p.get("location")?.num() as f32;
            let der = p.get("derivative").map(J::num).unwrap_or(0.0) as f32;
            let val = match p.get("value")? {
                J::Num(n) => SV::V(*n as f32),
                x @ J::Obj(_) => SV::S(Box::new(self.parse_spline(x, files)?)),
                _ => return None,
            };
            pts.push((loc, val, der));
        }
        Some(Spl { coord, pts })
    }

    fn parse_df(&mut self, j: &J, files: &HashMap<String, Vec<u8>>) -> Option<DF> {
        let ty = j.get("type")?.str().strip_prefix("minecraft:")?.to_string();
        // The loader silently dropping a node looks exactly like a wrong world, so
        // `LOAD_TRACE=1` names the type it could not build (CV2: print what was skipped).
        let trace = |ty: &str, ok: bool| {
            if !ok && std::env::var("LOAD_TRACE").is_ok() {
                eprintln!("[load] density function type not built: {ty}");
            }
        };
        let n = |k: &str| j.get(k).map(J::num).unwrap_or(0.0);
        let sub = |g: &mut Gen, k: &str| -> Option<usize> { g.node(j.get(k)?, files) };
        // 1.21.1 names the two operands `argument1`/`argument2`, the newer dialect
        // `left`/`right`. Same operation, same order, so accept either.
        let either = |g: &mut Gen, k: &str, alt: &str| -> Option<usize> {
            g.node(j.get(k).or_else(|| j.get(alt))?, files)
        };
        let out = Some(match ty.as_str() {
            "add" => DF::Add(
                either(self, "left", "argument1")?,
                either(self, "right", "argument2")?,
            ),
            "sub" => DF::Sub(
                either(self, "left", "argument1")?,
                either(self, "right", "argument2")?,
            ),
            "mul" => DF::Mul(
                either(self, "left", "argument1")?,
                either(self, "right", "argument2")?,
            ),
            "min" => DF::Min(
                either(self, "left", "argument1")?,
                either(self, "right", "argument2")?,
            ),
            "max" => DF::Max(
                either(self, "left", "argument1")?,
                either(self, "right", "argument2")?,
            ),
            "div" => DF::Div(
                either(self, "left", "argument1")?,
                either(self, "right", "argument2")?,
            ),
            "negate" => DF::Neg(either(self, "input", "argument")?),
            "abs" => DF::Abs(either(self, "input", "argument")?),
            "square" => DF::Sq(either(self, "input", "argument")?),
            "cube" => DF::Cube(either(self, "input", "argument")?),
            "half_negative" => DF::HNeg(either(self, "input", "argument")?),
            "quarter_negative" => DF::QNeg(either(self, "input", "argument")?),
            "squeeze" => DF::Squeeze(either(self, "input", "argument")?),
            "clamp" => DF::Clamp {
                i: either(self, "input", "argument")?,
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
            // 1.21.1's name for the y gradient, with `from_y`/`to_y`. Both clamp.
            "y_clamped_gradient" => DF::Grad {
                fc: n("from_y"),
                fv: n("from_value"),
                tc: n("to_y"),
                tv: n("to_value"),
            },
            "interpolated" => DF::Interp(either(self, "input", "argument")?),
            "cache" | "flat_cache" | "cache_2d" | "cache_once" | "cache_all_in_cell" => {
                DF::Cache(either(self, "input", "argument")?)
            }
            "noise" => {
                let zero = {
                    self.dfs.push(DF::C(0.0));
                    self.dfs.len() - 1
                };
                // For this dialect, omitted shifts mean zero, but a present
                // unparseable shift is not equivalent to an omitted one.
                let shift = |g: &mut Gen, k: &str| -> Option<usize> {
                    match j.get(k) {
                        Some(value) => g.node(value, files),
                        None => Some(zero),
                    }
                };
                DF::Noise {
                    ni: self.noise_id(j.get("noise")?.str())?,
                    sx: shift(self, "shift_x")?,
                    sy: shift(self, "shift_y")?,
                    sz: shift(self, "shift_z")?,
                    xzs: n("xz_scale"),
                    ys: n("y_scale"),
                }
            }
            // Native 1.21.1 shifted_noise specifies all three density shifts.
            // Reading shift_y as constant zero was only correct by coincidence
            // for the currently pinned vanilla data (whose shift_y is zero).
            "shifted_noise" => DF::Noise {
                ni: self.noise_id(j.get("noise")?.str())?,
                sx: self.node(j.get("shift_x")?, files)?,
                sy: self.node(j.get("shift_y")?, files)?,
                sz: self.node(j.get("shift_z")?, files)?,
                xzs: n("xz_scale"),
                ys: n("y_scale"),
            },
            // 1.21.1 names it in `argument`, the newer dialect in `noise`; either way it
            // is a *noise* name (`minecraft:offset`), not a density-function reference.
            "shift_a" | "shift_x" => {
                DF::ShiftA(self.noise_id(j.get("noise").or_else(|| j.get("argument"))?.str())?)
            }
            "shift_b" | "shift_z" => {
                DF::ShiftB(self.noise_id(j.get("noise").or_else(|| j.get("argument"))?.str())?)
            }
            "weird_scaled_sampler" => {
                let name = j
                    .get("rarity_value_mapper")
                    .map(J::str)
                    .unwrap_or("<missing>");
                let mapper = RarityValueMapper::parse(name).or_else(|| {
                    Self::load_trace(&format!(
                        "weird_scaled_sampler invalid rarity_value_mapper {name:?}"
                    ));
                    None
                })?;
                let name = j.get("noise").map(J::str).unwrap_or("<missing>");
                let ni = self.noise_id(name).or_else(|| {
                    Self::load_trace(&format!("weird_scaled_sampler unresolved noise {name:?}"));
                    None
                })?;
                DF::WeirdScaledSampler {
                    input: either(self, "input", "argument")?,
                    ni,
                    mapper,
                }
            }
            "range_choice" => DF::RangeChoice {
                i: either(self, "input", "argument")?,
                mi: n("min_inclusive"),
                ma: n("max_exclusive"),
                wr: sub(self, "when_in_range")?,
                wo: sub(self, "when_out_of_range")?,
            },
            "interval_select" => {
                let mut th = vec![];
                let mut fns = vec![];
                for f in j.get("functions")?.as_arr()? {
                    fns.push(self.node(f, files)?);
                }
                for t in j.get("thresholds")?.as_arr()? {
                    th.push(t.num());
                }
                DF::IntervalSelect {
                    i: either(self, "input", "argument")?,
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
                DF::Blended(Box::new(if self.legacy_random_source {
                    BlendedNoise::legacy(self.world_seed, xzs, ys, xzf, yf, smear)
                } else {
                    let mut rng = self.factory.of_str("minecraft:terrain");
                    BlendedNoise::modern(&mut rng, xzs, ys, xzf, yf, smear)
                }))
            }
            "blend_density" => DF::BlendD(either(self, "input", "argument")?),
            "blend_alpha" => DF::C(1.0),
            "end_islands" => DF::EndIslands(Box::new(EndIslands::new(self.world_seed))),
            "beardifier" => DF::Beardifier,
            "blend_offset" | "end_outer_islands" | "distance_to_point" | "slice" => DF::Zero,
            "find_top_surface" => DF::FindTopSurface {
                density: sub(self, "density")?,
                lower: n("lower_bound") as i32,
                upper: sub(self, "upper_bound")?,
                cell_h: n("cell_height") as i32,
            },
            _ => {
                // An unrecognized required density function is not a constant zero.
                // Returning None propagates through node/resolve_ref into the
                // noise-router loader instead of silently changing the terrain.
                Self::load_trace(&format!("unsupported density function type {ty:?}"));
                return None;
            }
        });
        trace(&ty, out.is_some());
        out
    }

    pub(crate) fn eval(
        &self,
        id: usize,
        x: i32,
        y: i32,
        z: i32,
        memo: &mut HashMap<(usize, i32, i32, i32), f64>,
    ) -> f64 {
        self.eval_c(id, x, y, z, memo, None)
    }

    /// Evaluate a density function. `cc` supplies the per-chunk interpolation
    /// grids: with it, an `interpolated` node returns the trilinear value of its
    /// grid instead of evaluating its input at block resolution (vanilla
    /// NoiseChunk). While the grids are being filled `cc` is None, which is a
    /// corner sample.
    fn eval_c(
        &self,
        id: usize,
        x: i32,
        y: i32,
        z: i32,
        memo: &mut HashMap<(usize, i32, i32, i32), f64>,
        cc: Option<&ChunkCols>,
    ) -> f64 {
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
            | DF::EndIslands(_)
            | DF::Zero
            | DF::Beardifier
            | DF::Grad { .. }
            | DF::ShiftA(_)
            | DF::ShiftB(_)
            | DF::Blended(_) => {}
            DF::WeirdScaledSampler { input, .. } => out.push(*input),
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

    fn eval_spline_c(
        &self,
        s: &Spl,
        x: i32,
        y: i32,
        z: i32,
        memo: &mut HashMap<(usize, i32, i32, i32), f64>,
        cc: Option<&ChunkCols>,
    ) -> f64 {
        // CubicSpline stores locations/derivatives/constants as f32 and casts
        // the coordinate to f32; interpolation is float arithmetic (Mth.lerp).
        let loc = self.eval_c(s.coord, x, y, z, memo, cc) as f32;
        let n = s.pts.len();
        if n == 0 {
            return 0.0;
        }
        let mut sv = |sv: &SV| -> f32 {
            match sv {
                SV::V(v) => *v,
                SV::S(sp) => self.eval_spline_c(sp, x, y, z, memo, cc) as f32,
            }
        };
        let idx = s.pts.partition_point(|p| loc >= p.0) as isize - 1;
        if idx < 0 || idx == n as isize - 1 {
            let p = if idx < 0 { &s.pts[0] } else { &s.pts[n - 1] };
            let v = sv(&p.1);
            let out = if p.2 == 0.0 { v } else { v + p.2 * (loc - p.0) };
            return out as f64;
        }
        let i = idx as usize;
        let (x0, v0, d0) = (s.pts[i].0, sv(&s.pts[i].1), s.pts[i].2);
        let (x1, v1, d1) = (s.pts[i + 1].0, sv(&s.pts[i + 1].1), s.pts[i + 1].2);
        let t = (loc - x0) / (x1 - x0);
        let dx = x1 - x0;
        let d0t = d0 * dx - (v1 - v0);
        let d1t = -d1 * dx + (v1 - v0);
        let lerp = |delta: f32, start: f32, end: f32| start + delta * (end - start);
        (lerp(t, v0, v1) + t * (1.0 - t) * lerp(t, d0t, d1t)) as f64
    }

    fn eval0_c(
        &self,
        id: usize,
        x: i32,
        y: i32,
        z: i32,
        memo: &mut HashMap<(usize, i32, i32, i32), f64>,
        cc: Option<&ChunkCols>,
    ) -> f64 {
        match &self.dfs[id] {
            DF::C(v) => *v,
            DF::EndIslands(field) => field.density(x, z),
            DF::Zero => 0.0,
            // Vanilla substitutes the chunk's `Beardifier` for the marker in
            // `NoiseChunk.wrap`. Without columns (the `interpolated` corner samples,
            // and `getBaseHeight`/preliminary-surface queries) the marker itself is
            // what vanilla evaluates, and it is zero.
            DF::Beardifier => match cc.and_then(|cc| cc.beard.as_deref()) {
                Some(beard) => compute_beardifier(beard, x, y, z),
                None => 0.0,
            },
            DF::Add(a, b) => {
                self.eval_c(*a, x, y, z, memo, cc) + self.eval_c(*b, x, y, z, memo, cc)
            }
            DF::Sub(a, b) => {
                self.eval_c(*a, x, y, z, memo, cc) - self.eval_c(*b, x, y, z, memo, cc)
            }
            DF::Mul(a, b) => {
                self.eval_c(*a, x, y, z, memo, cc) * self.eval_c(*b, x, y, z, memo, cc)
            }
            DF::Min(a, b) => self
                .eval_c(*a, x, y, z, memo, cc)
                .min(self.eval_c(*b, x, y, z, memo, cc)),
            DF::Max(a, b) => self
                .eval_c(*a, x, y, z, memo, cc)
                .max(self.eval_c(*b, x, y, z, memo, cc)),
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
            DF::Noise {
                ni,
                sx,
                sy,
                sz,
                xzs,
                ys,
            } => {
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
            DF::WeirdScaledSampler { input, ni, mapper } => {
                let rarity = mapper.rarity(self.eval_c(*input, x, y, z, memo, cc));
                let xd = x as f64 / rarity;
                let yd = y as f64 / rarity;
                let zd = z as f64 / rarity;
                rarity * self.noises[*ni].get(xd, yd, zd).abs()
            }
            DF::RangeChoice { i, mi, ma, wr, wo } => {
                let v = self.eval_c(*i, x, y, z, memo, cc);
                self.eval_c(
                    if v >= *mi && v < *ma { *wr } else { *wo },
                    x,
                    y,
                    z,
                    memo,
                    cc,
                )
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
            DF::Blended(noise) => noise.get(x, y, z),
            DF::BlendD(a) => self.eval_c(*a, x, y, z, memo, cc),
            DF::FindTopSurface {
                density,
                lower,
                upper,
                cell_h,
            } => {
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
        let random = if self.legacy_random_source {
            Lcg::new(legacy_world_factory_seed(self.world_seed) ^ hash_block_pos(x, 0, z) as u64)
                .next_f64()
        } else {
            r.next_f64()
        };
        (n * 2.75 + 3.0 + random * 0.25) as i32
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
        // SurfaceRules.Context uses BiomeManager at the actual block position.
        // Fuzzy biome boundaries are per block, not uniform inside a quart.
        let key = if self.preset == NoisePreset::Nether {
            (c.x, c.y, c.z)
        } else {
            (qx, qy, qz)
        };
        // Share the lookup across surface contexts. The Nether cache must retain
        // block coordinates because fuzzy selection can vary inside one quart.
        if let Some(&l) = self.bioq.get(&key) {
            return l;
        }
        let l = if self.preset == NoisePreset::Nether {
            self.biome_blended(c.x, c.y, c.z)
        } else {
            self.biome_quart(qx, qy, qz, None)
        }
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
        if let Some(spec) = env_str!("PRELIM_DUMP") {
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
        // NoiseChunk.preliminarySurfaceLevel keys by QuartPos (x >> 2, z >> 2).
        let k = (x >> 2, z >> 2);
        if let Some(&v) = self.prelim_c.get(&k) {
            return v;
        }
        let v = match self.prelim_df {
            Some(id) => {
                let ax = (x >> 2) << 2;
                let az = (z >> 2) << 2;
                match self.dfs.get(id) {
                    // Converted dialect only: its node carries explicit bounds.
                    Some(DF::FindTopSurface {
                        density, cell_h, ..
                    }) => {
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
                    // 1.21.1: scan initial_density_without_jaggedness from
                    // minY + height down to minY by the noise cell height and
                    // take the first value above 0.390625. The column is a
                    // QuartPos column, not a section column.
                    _ => {
                        let step = self.noise_cell_h.max(1);
                        let mut y = self.min_y + self.height;
                        let mut found = i32::MAX;
                        while y >= self.min_y {
                            if self.eval(id, ax, y, az, &mut HashMap::new()) > 0.390625 {
                                found = y;
                                break;
                            }
                            y -= step;
                        }
                        found
                    }
                }
            }
            None => i32::MAX,
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
        est.wrapping_add(self.run_depth(x, z)).wrapping_sub(8)
    }

    fn test_cond(&mut self, cid: usize, c: &mut SC, heightmap: &[i16; 256]) -> bool {
        let cd = self.conds[cid];
        match cd {
            Cond::False => false,
            Cond::Temperature => {
                let leaf = self.leaf_at(c);
                self.biome_weather
                    .get(leaf as usize)
                    .is_some_and(|weather| weather.cold(c.x, c.y, c.z))
            }
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
            Cond::VGrad {
                legacy_seed,
                lo_w,
                hi_w,
                t,
                f,
            } => {
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
                if self.legacy_random_source {
                    Lcg::new(legacy_seed ^ hash_block_pos(c.x, c.y, c.z) as u64).next_f32() < m
                } else {
                    r.next_f32() < m
                }
            }
            Cond::YAbove { a, mult, sd } => {
                let top = self.min_y + self.height - 1;
                c.y + if sd { c.sda } else { 0 } >= a.get(self.min_y, top) + c.run_depth * mult
            }
            Cond::Water { off, mult, sd } => {
                c.fluid == i32::MIN
                    || c.y + if sd { c.sda } else { 0 } >= c.fluid + off + c.run_depth * mult
            }
            Cond::Steep => {
                if let Some(s) = c.steep {
                    return s;
                }
                let lx = c.x & 15;
                let lz = c.z & 15;
                let t = |lx: i32, lz: i32| -> i32 {
                    heightmap[(lz.clamp(0, 15) as usize) * 16 + (lx.clamp(0, 15) as usize)] as i32
                };
                // pumpkin steep: north-facing (z+1 >= z-1 + 4) or west-facing (x-1 >= x+1 + 4)
                let s = t(lx, lz + 1) >= t(lx, lz - 1) + 4 || t(lx - 1, lz) >= t(lx + 1, lz) + 4;
                c.steep = Some(s);
                s
            }
            Cond::Not(i) => !self.test_cond(i, c, heightmap),
            Cond::Hole => c.run_depth <= 0,
            Cond::Prelim => c.y >= c.est_min,
            Cond::StoneD {
                off,
                sdep,
                sec,
                ceil,
            } => {
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

    fn eval_rule(&mut self, rid: usize, c: &mut SC, heightmap: &[i16; 256]) -> Option<u32> {
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
                    {
                        let sub = next?;
                        i += 1;
                        if let Some(st) = self.eval_rule(sub, c, heightmap) {
                            return Some(st);
                        }
                    }
                }
            }
            Rule::Cond(ci, ri) => {
                let (ci, ri) = (*ci, *ri);
                if self.test_cond(ci, c, heightmap) {
                    self.eval_rule(ri, c, heightmap)
                } else {
                    None
                }
            }
            Rule::OreVein {
                density,
                rich,
                gap,
                ore,
                raw,
                fill,
                rawc,
                ymin,
                ymax,
            } => {
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
    // Preset-sized horizontal/vertical corner grid per `interpolated` node reachable from
    // final_density, indexed by density-function id. Vanilla interpolates only
    // the input of `interpolated`; squeeze/min/add stay per block.
    grids: Vec<Option<Box<[f64]>>>,
    surf: [i16; 256],
    ny: usize, // vertical corner count, including both edges
    cell_w: i32,
    cell_h: i32,
    min_y: i32,
    mats: Vec<Option<SurfMat>>, // per-column material-rule results (surface window)
    rd: [i16; 256],             // per-column run_depth (surface noise)
    wtop: [i16; 256],           // per-column top of solid-or-fluid (heightmap basis)
    /// The beardifier input of the chunk that owns these columns, resolved by
    /// `Gen::resolve_beard` before `surf`, `wtop` and `mats` are built (vanilla's
    /// `NoiseChunk.fillFromNoise` applies `Beardifier` before
    /// `SurfaceSystem.buildSurface`). `None` during nested structure-heightmap
    /// queries (`beard_depth > 0`) and for chunks with no structures.
    beard: Option<std::sync::Arc<BeardChunkData>>,
}

type GeneratedTick = ((i32, i32, i32), u32);
type PendingGeneratedTicks = (Vec<GeneratedTick>, HashSet<GeneratedTick>);

/// Per-world pending ticks. Sharing these handles with runtime avoids taking GEN
/// to drain, without mixing independent generators or reinitialized worlds.
#[derive(Clone, Default)]
struct GeneratedTickQueues {
    leaves: std::sync::Arc<Mutex<PendingGeneratedTicks>>,
    fluids: std::sync::Arc<Mutex<PendingGeneratedTicks>>,
}

static GEN_TICK_QUEUES: Mutex<Option<GeneratedTickQueues>> = Mutex::new(None);

fn drain_generated_ticks(queue: &Mutex<PendingGeneratedTicks>) -> Vec<GeneratedTick> {
    let mut q = queue.lock().unwrap();
    q.1.clear();
    std::mem::take(&mut q.0)
}

/// Drain the active world's leaf queue without waiting for the worldgen lock.
pub fn take_generated_leaf_ticks() -> Vec<GeneratedTick> {
    let queues = GEN_TICK_QUEUES.lock().unwrap().clone();
    queues.map_or_else(Vec::new, |q| drain_generated_ticks(&q.leaves))
}

pub fn take_generated_fluid_ticks() -> Vec<GeneratedTick> {
    let queues = GEN_TICK_QUEUES.lock().unwrap().clone();
    queues.map_or_else(Vec::new, |q| drain_generated_ticks(&q.fluids))
}

static GEN: Mutex<Option<Gen>> = Mutex::new(None);

/// Bumped by every `init`. The caches above the generator (`main.rs`'s state band and
/// section store) describe *this* generator, so they must not answer for a replacement:
/// a band left over from a previous `init` fed the anti-xray exposure test states from
/// another world, which changed one chunk's delivered bytes between two sweeps in the same
/// process (seed 60007, chunk 2,-1 — found by the digest guard).
static GEN_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub fn generation_seq() -> u64 {
    GEN_SEQ.load(std::sync::atomic::Ordering::Relaxed)
}

/// Serializes tests that mutate the process-global generator, including ones in other
/// test modules (`worldgen::wg_tests`' own `lock_gen` still uses it too).
static GEN_TEST_LOCK: Mutex<()> = Mutex::new(());

/// `GEN`'s lock, timing how long a caller waited for it and how long it held it.
///
/// The parallelization decision needs this split: `FEAT_PROFILE` can say how much
/// work a populate does, but not how much of a server's wall time is spent queued
/// behind the one world lock. Off unless `FEAT_PROFILE` is set, in which case the
/// two `Instant` calls are paid on every acquisition.
pub(crate) struct GenLock {
    guard: std::sync::MutexGuard<'static, Option<Gen>>,
    /// `Some` when profiling: the time spent inside `GEN.lock()`.
    waited_ns: Option<u64>,
    held_since: Option<std::time::Instant>,
}
static GEN_WAIT_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static GEN_HOLD_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static GEN_LOCK_N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl Drop for GenLock {
    fn drop(&mut self) {
        let (Some(waited), Some(since)) = (self.waited_ns, self.held_since) else {
            return;
        };
        use std::sync::atomic::Ordering;
        GEN_HOLD_NS.fetch_add(since.elapsed().as_nanos() as u64, Ordering::Relaxed);
        GEN_WAIT_NS.fetch_add(waited, Ordering::Relaxed);
        GEN_LOCK_N.fetch_add(1, Ordering::Relaxed);
    }
}

impl std::ops::Deref for GenLock {
    type Target = Option<Gen>;
    fn deref(&self) -> &Self::Target {
        &self.guard
    }
}
impl std::ops::DerefMut for GenLock {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.guard
    }
}

/// The part of `Gen` that never changes after `init`: the dimension facts every reader
/// needs (`sea`, build height, the default block states).
///
/// Published separately from `GEN` so a reader does not queue behind a 13 ms generation
/// hold to learn the build height. It is the first piece of the `GenData`/`Gen` split in
/// `specs/chunk-parallelism`: immutable inputs readable without the world lock. Anything
/// added here must be immutable after construction — the snapshot is taken once, and a
/// later writer would silently diverge from the generator.
#[derive(Clone, Copy, Debug)]
pub(crate) struct GenFacts {
    pub preset: NoisePreset,
    pub sea: i32,
    /// generator window (`noise_settings`)
    pub min_y: i32,
    pub height: i32,
    /// world window (`dimension_type`); see `GenData::world_min_y`
    pub world_min_y: i32,
    pub world_height: i32,
    pub def_st: u32,
    pub def_fl: u32,
    pub bedrock_st: u32,
}

static GEN_FACTS: Mutex<Option<GenFacts>> = Mutex::new(None);

/// The immutable half as a shareable snapshot, published at `init`. A worker takes a
/// clone of the `Arc` once and then reads the world's inputs without touching `GEN`:
/// this lock is held for a clone, not for a generation step.
static GEN_DATA: Mutex<Option<std::sync::Arc<GenData>>> = Mutex::new(None);

/// `Arc<GenData>` without waiting for a generation step to finish. A job can keep the
/// snapshot for its lifetime; the caches it must not share live on `Gen`, not here.
pub fn data_snapshot() -> Option<std::sync::Arc<GenData>> {
    GEN_DATA.lock().unwrap().clone()
}

/// The immutable dimension facts, without the `GEN` lock. `None` before `init`.
pub(crate) fn facts() -> Option<GenFacts> {
    *GEN_FACTS.lock().unwrap()
}

fn gen_lock() -> GenLock {
    // Either switch turns the timing on: `FEAT_PROFILE` for generation-only runs,
    // `MC_SPINE_TICK_PROFILE` for a live server, where the tick thread is the caller
    // that queues behind a section pass.
    let prof = env_flag!("FEAT_PROFILE") || env_flag!("MC_SPINE_TICK_PROFILE");
    let started = prof.then(std::time::Instant::now);
    let guard = GEN.lock().unwrap();
    GenLock {
        guard,
        waited_ns: started.map(|t| t.elapsed().as_nanos() as u64),
        held_since: started,
    }
}

/// `GENLOCK wait=<ms> hold=<ms> n=<acquisitions>` under `FEAT_PROFILE`.
///
/// `wait / (wait + hold)` is the share of lock-attributed time spent queued; a
/// worker-per-chunk design removes the queue, not the hold.
pub fn report_gen_lock() {
    if !env_flag!("FEAT_PROFILE") && !env_flag!("MC_SPINE_TICK_PROFILE") {
        return;
    }
    use std::sync::atomic::Ordering;
    let wait = GEN_WAIT_NS.swap(0, Ordering::Relaxed) as f64 / 1e6;
    let hold = GEN_HOLD_NS.swap(0, Ordering::Relaxed) as f64 / 1e6;
    let n = GEN_LOCK_N.swap(0, Ordering::Relaxed);
    eprintln!(
        "GENLOCK wait={wait:.1}ms hold={hold:.1}ms n={n} threads={}",
        gen_threads_probe()
    );
}

fn gen_threads_probe() -> usize {
    std::thread::available_parallelism()
        .map(|v| v.get())
        .unwrap_or(1)
}

/// The one instruction for a tree that has no game data committed next to it. Both the
/// startup fallbacks and the tests that read the data print it, so a missing file names the
/// command that writes it instead of the `Option` that came back empty.
pub(crate) const VANILLA_DATA_MISSING: &str =
    "Vanilla data missing. Regenerate it from your own 1.21.1 jar: tools/vanilla_data/gen_vanilla_data.sh --jar /path/to/server.jar --fetch-fabric-mappings --accept-eula";

pub fn init(seed: u64, res: fn(&str) -> Option<u32>) -> bool {
    init_preset(seed, res, NoisePreset::Overworld)
}
pub(crate) fn init_preset(seed: u64, res: fn(&str) -> Option<u32>, preset: NoisePreset) -> bool {
    let generator = match preset {
        NoisePreset::Overworld => Gen::load(seed, res),
        NoisePreset::End | NoisePreset::Nether => Gen::load_preset(seed, res, preset),
    };
    match generator {
        Some(g) => {
            *GEN_FACTS.lock().unwrap() = Some(GenFacts {
                preset: g.preset,
                sea: g.sea,
                min_y: g.min_y,
                height: g.height,
                world_min_y: g.world_min_y,
                world_height: g.world_height,
                def_st: g.def_st,
                def_fl: g.def_fl,
                bedrock_st: g.bedrock_st,
            });
            *GEN_DATA.lock().unwrap() = Some(g.data.clone());
            *GEN_TICK_QUEUES.lock().unwrap() = Some(g.generated_ticks.clone());
            *gen_lock() = Some(g);
            GEN_READY.store(true, std::sync::atomic::Ordering::Relaxed);
            GEN_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
    if let Some(g) = gen_lock().as_mut() {
        g.dump_dfs(p[0], p[1], p[2]);
    }
}
/// Is a generator loaded?
///
/// An atomic rather than `gen_lock().is_some()`: this is asked once per cell read on the
/// light and block-read paths, and taking the worldgen lock there made every read queue
/// behind whatever generation is in flight. `init_preset` is the only writer, and the
/// generator is never dropped once loaded (tests that re-init only replace it).
static GEN_READY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn enabled() -> bool {
    GEN_READY.load(std::sync::atomic::Ordering::Relaxed)
}

/// `ChunkLevel.byStatus(FullChunkStatus.ENTITY_TICKING)`: a `forceload` request enters the
/// scheduler as a ticket at level 31 (`ChunkMap.c`), not as a position in a list.
pub(crate) const POP_REQUEST_LEVEL: i32 = 31;
/// The status whose task decorates a chunk (`ChunkStatus.FEATURES`) answers to level 34 in this
/// build (`ChunkLevel.getStatusForLevel`), so a request set is mirrored by three rings of
/// *demanders* and the decorated set is the request set plus three rings (`FEATURES_RING`).
pub(crate) const POP_DECORATION_LEVEL: i32 = 34;
/// `ChunkStep.blockStateWriteRadius` for the `FEATURES` step: one chunk, so a decoration walks
/// its own 3x3 and everything it writes outside that square is dropped (`arb.f_`).
pub(crate) const POP_WRITE_RADIUS: i32 = 1;

/// One pop of the population priority discipline: the demander that reached the head of the
/// queue, the level it was queued at, and the chunks its `FEATURES` step decorated, in the order
/// the step walked them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PopBatch {
    pub level: i32,
    pub demander: (i32, i32),
    pub decorated: Vec<(i32, i32)>,
}

/// The request set in the box shape `MC_SPINE_POP_REGION` already uses, walked x-then-z.
///
/// This walk is the *declared* half of the default order and the reason the whole default is
/// named: vanilla fixes the priority discipline and leaves the within-level sequence to the wall
/// clock (two identical-setting native runs agree on 9 of 49 chunks), so any single default is one
/// member of that family. Here the member is: the first demander is the box's min corner, and a
/// demander's window is walked x-outer / z-inner.
pub(crate) fn request_box(x0: i32, z0: i32, x1: i32, z1: i32) -> Vec<(i32, i32)> {
    let (x0, x1) = (x0.min(x1), x0.max(x1));
    let (z0, z1) = (z0.min(z1), z0.max(z1));
    (x0..=x1)
        .flat_map(|x| (z0..=z1).map(move |z| (x, z)))
        .collect()
}

/// The default population order: **the serialized demand scan**, a named member of vanilla's
/// order family.
///
/// Vanilla's population order is a priority discipline, not a sequence:
///
/// * a request is a ticket at level 31 (`POP_REQUEST_LEVEL`), and `DistanceManager` propagates it
///   at +1 per Chebyshev step, so ring *d* sits at level 31+d;
/// * `ChunkTaskPriorityQueue` holds one insertion-ordered FIFO bucket per level and pops the
///   lowest non-empty level (`aqd`), with the task's priority being the demander's queue level;
/// * a popped task walks statuses upward and, at `FEATURES`, walks its `2r+1` window x-outer /
///   z-inner, running each demanded chunk's step inline (`apy`, `dwc`);
/// * `FEATURES` answers to level 34, so demanders exist at levels 31..=33 and the decorated set
///   is the request set plus three rings (measured on seven native captures: 1 chunk -> 7x7,
///   3x3 -> 9x9).
///
/// The rule is fixed; the within-level order is wall clock, and identical-setting native runs
/// differ (9/49 common prefix). This function therefore implements *one named member* and does not
/// claim bit-parity with any capture — for a comparison against a recorded order the order is an
/// input (`MC_SPINE_POP_ORDER`), which is what the finite acceptance set compares with.
///
/// The member, exactly:
///
/// 1. request chunks enter level 31's bucket in `request_box` order;
/// 2. pop the head of the lowest non-empty level;
/// 3. walk that demander's 3x3 window x-outer / z-inner, skipping chunks already decorated, and
///    append every newly decorated chunk that is itself a demander (level < 34) to the tail of
///    its own level's bucket — that inline demand is what makes the scan serial;
/// 4. repeat until every bucket is empty.
///
/// Steps 2-3 give the two measured shape properties for free: the first batch is the x-then-z 3x3
/// at the request box's min corner, and batches pop in non-decreasing level (the decorated list is
/// not level-monotone — a batch's window reaches one ring further — exactly as measured).
pub(crate) fn population_discipline(requests: &[(i32, i32)]) -> Vec<PopBatch> {
    if requests.is_empty() {
        return Vec::new();
    }
    let level_of = |c: (i32, i32)| {
        let ring = requests
            .iter()
            .map(|r| (c.0 - r.0).abs().max((c.1 - r.1).abs()))
            .min()
            .unwrap_or(0);
        POP_REQUEST_LEVEL + ring
    };
    // One FIFO bucket per level, and the pop order is by level: a `BTreeMap` is the whole queue.
    let mut buckets: std::collections::BTreeMap<i32, std::collections::VecDeque<(i32, i32)>> =
        std::collections::BTreeMap::new();
    let mut queued: HashSet<(i32, i32)> = HashSet::new();
    let mut decorated: HashSet<(i32, i32)> = HashSet::new();
    for &request in requests {
        if queued.insert(request) {
            buckets
                .entry(level_of(request))
                .or_default()
                .push_back(request);
        }
    }
    let mut batches = Vec::new();
    loop {
        let Some(level) = buckets
            .iter()
            .find(|(_, bucket)| !bucket.is_empty())
            .map(|(level, _)| *level)
        else {
            break;
        };
        let demander = buckets
            .get_mut(&level)
            .and_then(|bucket| bucket.pop_front())
            .expect("the level was found non-empty");
        let mut window = Vec::with_capacity(9);
        for dx in -POP_WRITE_RADIUS..=POP_WRITE_RADIUS {
            for dz in -POP_WRITE_RADIUS..=POP_WRITE_RADIUS {
                let chunk = (demander.0 + dx, demander.1 + dz);
                if !decorated.insert(chunk) {
                    continue;
                }
                window.push(chunk);
                let chunk_level = level_of(chunk);
                // A chunk whose own target *is* `FEATURES` has a demand radius of 0 for it, so
                // ring 3 is decorated but never demands: that is where the set stops.
                if chunk_level < POP_DECORATION_LEVEL && queued.insert(chunk) {
                    buckets.entry(chunk_level).or_default().push_back(chunk);
                }
            }
        }
        batches.push(PopBatch {
            level,
            demander,
            decorated: window,
        });
    }
    batches
}

/// The decorated order of [`population_discipline`], flattened: the execution order.
pub(crate) fn population_discipline_order(requests: &[(i32, i32)]) -> Vec<(i32, i32)> {
    population_discipline(requests)
        .into_iter()
        .flat_map(|batch| batch.decorated)
        .collect()
}

/// Validate an explicit FEATURES plan covering a dump's one-chunk decoration halo.
/// Recorded plans may include additional observed sources; exact plans may not.
/// Neither mode silently drops or reorders entries.
pub(crate) fn parse_population_order(
    text: &str,
    ox: i32,
    oz: i32,
    n: i32,
    allow_extra: bool,
) -> Result<Vec<(i32, i32)>, String> {
    if n <= 0 {
        return Err("dump size must be positive".into());
    }
    let width = n.checked_add(2).ok_or("population footprint overflow")? as usize;
    let expected = width
        .checked_mul(width)
        .ok_or("population footprint overflow")?;
    let x0 = ox.checked_sub(1).ok_or("population coordinate overflow")?;
    let z0 = oz.checked_sub(1).ok_or("population coordinate overflow")?;
    let x1 = ox.checked_add(n).ok_or("population coordinate overflow")?;
    let z1 = oz.checked_add(n).ok_or("population coordinate overflow")?;
    let mut order = Vec::new();
    let mut seen = HashSet::new();
    for (i, line) in text.lines().enumerate() {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() != 2 {
            return Err(format!("population line {} needs x z: {line:?}", i + 1));
        }
        let x = fields[0]
            .parse::<i32>()
            .map_err(|_| format!("invalid population x on line {}: {line:?}", i + 1))?;
        let z = fields[1]
            .parse::<i32>()
            .map_err(|_| format!("invalid population z on line {}: {line:?}", i + 1))?;
        if !allow_extra && (x < x0 || x > x1 || z < z0 || z > z1) {
            return Err(format!(
                "population chunk ({x},{z}) is outside ({x0},{z0})..({x1},{z1})"
            ));
        }
        if !seen.insert((x, z)) {
            return Err(format!(
                "duplicate population chunk ({x},{z}) on line {}",
                i + 1
            ));
        }
        order.push((x, z));
    }
    let covered = seen
        .iter()
        .filter(|&&(x, z)| x0 <= x && x <= x1 && z0 <= z && z <= z1)
        .count();
    if covered != expected {
        return Err(format!("incomplete population footprint: {covered} of {expected} source chunks in ({x0},{z0})..({x1},{z1})"));
    }
    Ok(order)
}

/// Run a diagnostic against the generator under GEN. Feature-facing accessors
/// stay in the caller's layer to avoid worldgen depending on generation_context.
pub fn with_gen<R>(f: impl FnOnce(&mut Gen) -> R) -> Option<R> {
    gen_lock().as_mut().map(f)
}

/// A stable fingerprint of one chunk's generated columns, for determinism tests.
/// Every field is part of the terrain answer, including the interpolated grids.
pub(crate) fn cols_fingerprint(cc: &ChunkCols) -> u64 {
    fn mix(h: &mut u64, v: u64) {
        *h ^= v;
        *h = h.wrapping_mul(0x100_0000_01b3);
    }
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for &s in &cc.surf {
        mix(&mut h, s as u64);
    }
    for &s in &cc.wtop {
        mix(&mut h, s as u64);
    }
    for &s in &cc.rd {
        mix(&mut h, s as u64);
    }
    for m in &cc.mats {
        match m {
            None => mix(&mut h, u64::MAX),
            Some(m) => {
                mix(&mut h, m.lo as u64);
                for v in &m.vals {
                    mix(&mut h, *v as u64);
                }
            }
        }
    }
    for grid in &cc.grids {
        match grid {
            None => mix(&mut h, u64::MAX - 1),
            Some(cells) => {
                for v in cells.iter() {
                    mix(&mut h, v.to_bits());
                }
            }
        }
    }
    mix(&mut h, cc.ny as u64);
    mix(&mut h, cc.cell_w as u64);
    mix(&mut h, cc.cell_h as u64);
    mix(&mut h, cc.min_y as u64);
    h
}

/// Generate one chunk's columns on a scratch generator, without the world lock.
///
/// Stage 2A of `specs/chunk-parallelism`: terrain is a pure function of the immutable
/// data and the seed — `cols` reads no feature write — so a worker can compute it while
/// the owner keeps `GEN`. Features stay sequential: they read other chunks' writes, and
/// their order is pinned to the native capture.
pub fn chunk_cols_scratch(
    data: &std::sync::Arc<GenData>,
    cx: i32,
    cz: i32,
) -> Option<std::sync::Arc<ChunkCols>> {
    Gen::fresh_scratch(data.clone()).cols(cx, cz)
}

/// Compute `keys` with up to `workers` scratch generators, returned in the caller's order.
/// `workers <= 1` runs inline, which is the reference path; every worker count must give
/// the same `ChunkCols` because `cols` is pure (the determinism test pins that).
pub fn prefetch_cols(
    data: &std::sync::Arc<GenData>,
    keys: &[(i32, i32)],
    workers: usize,
) -> Vec<((i32, i32), Option<std::sync::Arc<ChunkCols>>)> {
    if workers <= 1 || keys.len() <= 1 {
        return keys
            .iter()
            .map(|&(cx, cz)| ((cx, cz), chunk_cols_scratch(data, cx, cz)))
            .collect();
    }
    let mut out: Vec<((i32, i32), Option<std::sync::Arc<ChunkCols>>)> =
        keys.iter().map(|&k| (k, None)).collect();
    let per = keys.len().div_ceil(workers);
    std::thread::scope(|s| {
        for slice in out.chunks_mut(per) {
            s.spawn(move || {
                for (key, slot) in slice.iter_mut() {
                    *slot = chunk_cols_scratch(data, key.0, key.1);
                }
            });
        }
    });
    out
}

/// Is this chunk's `ChunkCols` already in the generator's cache? Diagnostic only: it
/// tells whether a caller that asks for columns also pays for their generation.
/// Does this chunk's column set carry beardifier data? Diagnostic for the cache rule
/// that keeps preliminary (structure-free) columns out of `col_c`.
pub(crate) fn cols_have_beard(cc: &ChunkCols) -> bool {
    cc.beard.is_some()
}

pub fn cols_cached(cx: i32, cz: i32) -> bool {
    gen_lock()
        .as_ref()
        .is_some_and(|g| g.col_c.contains_key(&(cx, cz)))
}

pub fn chunk_cols(cx: i32, cz: i32) -> Option<std::sync::Arc<ChunkCols>> {
    gen_lock().as_mut()?.cols(cx, cz)
}

impl Gen {
    /// Queue a leaf block tick.
    ///
    /// Each generator owns its queue, while runtime shares the active world's handle.
    /// The separate lock lets the tick thread drain without waiting for generation;
    /// process-global contents would mix independent generators and their tick order.
    pub(crate) fn schedule_generated_leaf(&mut self, pos: (i32, i32, i32), block: u32) {
        if self.suppress_generated_ticks {
            return;
        }
        let mut q = self.generated_ticks.leaves.lock().unwrap();
        if q.1.insert((pos, block)) {
            q.0.push((pos, block));
        }
    }

    pub(crate) fn generated_leaf_ticks(&self) -> Vec<((i32, i32, i32), u32)> {
        self.generated_ticks.leaves.lock().unwrap().0.clone()
    }

    /// Queue a generated fluid tick: same reasoning as the leaf queue above — the tick
    /// thread drains it every tick, so it must not be behind the worldgen lock.
    pub(crate) fn schedule_generated_fluid(&mut self, pos: (i32, i32, i32), fluid: u32) {
        if self.suppress_generated_ticks {
            return;
        }
        let mut q = self.generated_ticks.fluids.lock().unwrap();
        if q.1.insert((pos, fluid)) {
            q.0.push((pos, fluid));
        }
    }

    #[cfg(test)]
    pub(crate) fn generated_fluid_ticks(&self) -> Vec<GeneratedTick> {
        self.generated_ticks.fluids.lock().unwrap().0.clone()
    }

    pub(crate) fn tick_sources(&self) -> HashSet<(i32, i32)> {
        self.pop_done
            .union(&self.restored_tick_sources)
            .copied()
            .collect()
    }

    pub(crate) fn restore_tick_sources(&mut self, sources: HashSet<(i32, i32)>) {
        self.restored_tick_sources = sources;
    }

    pub(crate) fn restore_generated_ticks(
        &mut self,
        blocks: Vec<((i32, i32, i32), u32)>,
        fluids: Vec<((i32, i32, i32), u32)>,
    ) {
        // Restore only this world's queues, never another generator's pending work.
        {
            let mut q = self.generated_ticks.leaves.lock().unwrap();
            q.1 = blocks.iter().copied().collect();
            q.0 = blocks;
        }
        let mut fq = self.generated_ticks.fluids.lock().unwrap();
        fq.1 = fluids.iter().copied().collect();
        fq.0 = fluids;
    }

    /// Per-Gen cached chunk column builder (terrain layer only — no features).
    /// A generator over the same immutable inputs with empty caches: the per-job scratch
    /// for terrain work (`specs/chunk-parallelism`, stage 2A). Every cache on `Gen` is a
    /// memoization of a pure function of `GenData` and the seed, so a cold copy produces
    /// the same `ChunkCols`; it only re-does work the shared generator may already have
    /// done. Nothing in `cols` reads another chunk's feature writes.
    pub(crate) fn fresh_scratch(data: std::sync::Arc<GenData>) -> Gen {
        Gen {
            data,
            memo: HashMap::new(),
            aq_lv: HashMap::new(),
            aq_lvo: Vec::new(),
            aq_near: HashMap::new(),
            aq_neo: Vec::new(),
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
            base_c: HashMap::new(),
            base_ord: std::collections::VecDeque::new(),
            beard_c: HashMap::new(),
            beard_ord: std::collections::VecDeque::new(),
            beard_resolver: None,
            beard_depth: 0,
            feat_c: HashMap::new(),
            staged_payloads: Vec::new(),
            feat_ord: Vec::new(),
            deep_c: HashMap::new(),
            deep_ord: std::collections::VecDeque::new(),
            pop_done: HashSet::new(),
            shape_marks: HashMap::new(),
            pop_active: None,
            restored_tick_sources: HashSet::new(),
            suppress_generated_ticks: false,
            carve_c: HashMap::new(),
            carve_ord: std::collections::VecDeque::new(),
            pop_region: std::env::var("MC_SPINE_POP_REGION").ok().and_then(|v| {
                let p: Vec<i32> = v.split(',').filter_map(|n| n.parse().ok()).collect();
                if p.len() == 4 {
                    Some((p[0], p[1], p[2], p[3]))
                } else {
                    None
                }
            }),
            mineshaft_spawners: HashSet::new(),
            payload_sources: HashSet::new(),
            feat_hm: HashMap::new(),
            generated_ticks: GeneratedTickQueues::default(),
        }
    }

    /// Install prefetched columns with the same FIFO eviction `cols` uses, so a caller
    /// never grows `col_c` past its bound behind the cache's back.
    pub(crate) fn install_cols(&mut self, cols: Vec<((i32, i32), std::sync::Arc<ChunkCols>)>) {
        for ((cx, cz), arc) in cols {
            if self.col_c.contains_key(&(cx, cz)) {
                continue;
            }
            self.col_ord.push((cx, cz));
            if self.col_ord.len() > 128 {
                let old = self.col_ord.remove(0);
                self.col_c.remove(&old);
            }
            self.col_c.insert((cx, cz), arc);
        }
    }

    /// Set this generator's beardifier resolver.
    pub(crate) fn set_beard_resolver(&mut self, resolver: BeardResolver) {
        self.beard_resolver = Some(resolver);
    }

    /// The chunk's beardifier input, resolving (and caching) it on first use.
    ///
    /// `None` while another resolution is in progress: a structure's own heightmap
    /// query must describe the terrain without that structure, which is exactly why
    /// vanilla's `getBaseHeight` samples the BeardifierMarker and not the start.
    pub(crate) fn resolve_beard(
        &mut self,
        cx: i32,
        cz: i32,
    ) -> Option<std::sync::Arc<BeardChunkData>> {
        if let Some(v) = self.beard_c.get(&(cx, cz)) {
            return Some(v.clone());
        }
        if self.beard_depth > 0 {
            return None;
        }
        // `MC_SPINE_NOFEAT=1` is the terrain-only comparison switch: it skips the whole
        // feature layer (`feature::populate`), and a structure-shaped terrain is part of
        // that layer. Applying structures to a terrain fixture would compare the port's
        // beardified terrain with a native capture that had none.
        if env_flag!("MC_SPINE_NOFEAT") {
            return None;
        }
        let Some(resolver) = self.beard_resolver.or_else(global_beard_resolver) else {
            return None;
        };
        self.beard_depth += 1;
        let data = resolver(self, cx, cz);
        self.beard_depth -= 1;
        let arc = std::sync::Arc::new(data);
        // A structure generation may have populated the cache for this chunk already
        // (nested resolution is refused, so only the outermost call inserts).
        if let Some(v) = self.beard_c.get(&(cx, cz)) {
            return Some(v.clone());
        }
        self.beard_ord.push_back((cx, cz));
        if self.beard_ord.len() > 128 {
            if let Some(old) = self.beard_ord.pop_front() {
                self.beard_c.remove(&old);
            }
        }
        self.beard_c.insert((cx, cz), arc.clone());
        Some(arc)
    }

    pub(crate) fn cols(&mut self, cx: i32, cz: i32) -> Option<std::sync::Arc<ChunkCols>> {
        if let Some(v) = self.col_c.get(&(cx, cz)) {
            return Some(v.clone());
        }
        let cc = self.build_cols(cx, cz, true)?;
        let arc = std::sync::Arc::new(cc);
        // Columns built while a beardifier resolution was in flight are the preliminary
        // (structure-free) answer on purpose, and must not be cached: the same chunk's
        // final columns need the structure data this build could not ask for.
        if self.beard_depth == 0 {
            self.col_ord.push((cx, cz));
            if self.col_ord.len() > 128 {
                let old = self.col_ord.remove(0);
                self.col_c.remove(&old);
            }
            self.col_c.insert((cx, cz), arc.clone());
        }
        Some(arc)
    }

    /// `ChunkGenerator.getBaseColumn(x, z, level, randomState)`: the column a
    /// structure's `findGenerationPoint` may scan, with no beardifier and no surface
    /// rule. Vanilla builds a fresh 1x1 `NoiseChunk` here, so the answer must not
    /// depend on the carvers, the features, or the chunk's own beardifier: a
    /// `nether_fossil` placed from the live column lands on top of the mound its own
    /// beard raises instead of inside the terrain the noise says is there.
    ///
    /// Cached separately from `col_c` because it is pure in (seed, preset, chunk);
    /// unlike `cols`, it is safe to cache while `beard_depth > 0`.
    pub(crate) fn base_cols(&mut self, cx: i32, cz: i32) -> Option<std::sync::Arc<ChunkCols>> {
        if let Some(v) = self.base_c.get(&(cx, cz)) {
            return Some(v.clone());
        }
        let arc = std::sync::Arc::new(self.build_cols(cx, cz, false)?);
        self.base_ord.push_back((cx, cz));
        if self.base_ord.len() > 64 {
            if let Some(old) = self.base_ord.pop_front() {
                self.base_c.remove(&old);
            }
        }
        self.base_c.insert((cx, cz), arc.clone());
        Some(arc)
    }

    /// One cell of `ChunkGenerator.getBaseColumn`: `NoiseChunk.getBlockState` with the
    /// beardifier node answering zero, i.e. density -> default block, otherwise the
    /// aquifer's substance, falling back to the default block where the aquifer
    /// refuses the cell (vanilla's `$$27 == null ? settings.defaultBlock() : $$27`).
    ///
    /// `None` outside the noise window, where vanilla's `NoiseColumn` has no cell.
    pub(crate) fn base_column_state(&mut self, x: i32, y: i32, z: i32) -> Option<u32> {
        if y < self.min_y || y >= self.min_y + self.height {
            return None;
        }
        let cc = self.base_cols(x >> 4, z >> 4)?;
        let d = self.block_density(&cc, x, y, z);
        if d > 0.0 {
            return Some(self.def_st);
        }
        Some(self.aq_state(x, y, z, d).unwrap_or(self.def_st))
    }

    /// `cols` (`final_columns`) or `base_cols` (`!final_columns`): one chunk's columns.
    fn build_cols(&mut self, cx: i32, cz: i32, final_columns: bool) -> Option<ChunkCols> {
        let g = self;
        let ny = ((g.height / g.noise_cell_h) + 1) as usize;
        let nx = (16 / g.noise_cell_w + 1) as usize;
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
            let mut cells = vec![0.0f64; nx * nx * ny];
            for zi in 0..nx {
                for xi in 0..nx {
                    for yi in 0..ny {
                        let bx = cx * 16 + (xi as i32) * g.noise_cell_w;
                        let by = g.min_y + (yi as i32) * g.noise_cell_h;
                        let bz = cz * 16 + (zi as i32) * g.noise_cell_w;
                        cells[(zi * nx + xi) * ny + yi] = g.eval(inner, bx, by, bz, &mut memo);
                    }
                }
            }
            grids[iid] = Some(cells.into_boxed_slice());
        }
        // Resolve the chunk's beardifier before `surf`, `wtop` and `mats`: vanilla's
        // `fillFromNoise` evaluates `final_density` with `Beardifier` active and only
        // then calls `SurfaceSystem.buildSurface`. `preliminarySurfaceLevel` is the
        // unsmoothed estimate and is not affected; a nested heightmap query during
        // resolution runs with `beard_depth > 0`, gets `None` here, and is therefore
        // excluded from `col_c` (see the cache rule at the end of this function).
        let beard = if final_columns {
            g.resolve_beard(cx, cz)
        } else {
            None
        };
        let mut cc = ChunkCols {
            grids,
            surf: [0i16; 256],
            ny,
            cell_w: g.noise_cell_w,
            cell_h: g.noise_cell_h,
            min_y: g.min_y,
            mats: Vec::new(),
            rd: [0i16; 256],
            wtop: [0i16; 256],
            beard,
        };
        // A base column is the noise answer only: vanilla's `getBaseColumn` runs neither
        // `SurfaceSystem.buildSurface` nor the heightmap pass, and `block_density` never
        // reads `surf`/`wtop`/`mats`, so skip all three instead of paying for them.
        if !final_columns {
            return Some(cc);
        }
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
                    // Nether surface rules decorate every cave floor/ceiling;
                    // they have no above_preliminary_surface guard.
                    let est_min = if g.preset == NoisePreset::Nether {
                        g.min_y
                    } else {
                        g.prelim_min(x, z)
                    };
                    c.est_min = est_min;
                    let cells: Vec<SurfaceCell> = (g.min_y..=scan_top)
                        .map(|y| g.surface_cell(&cc, x, y, z, surf_y, water_surface))
                        .collect();
                    let column =
                        g.scan_surface_column(root, &mut c, est_min, &cells, &cc.wtop);
                    mats[lz * 16 + lx] = column;
                }
            }
        }
        cc.mats = mats;
        cc.rd = rd;
        Some(cc)
    }
}

impl Gen {
    fn surface_cell(
        &mut self,
        cc: &ChunkCols,
        x: i32,
        y: i32,
        z: i32,
        _surf_y: i32,
        _water_surface: i32,
    ) -> SurfaceCell {
        let d = self.block_density(cc, x, y, z);
        if d > 0.0 {
            return SurfaceCell::Solid;
        }
        let aq = self.aq_state(x, y, z, d);
        let air = aq.is_some_and(|st| st == 0 || breg().is_some_and(|r| r.is_air(st)));
        let fluid = aq.is_some_and(|st| self.is_fluid_state(st));
        classify_surface_cell(d, aq, air, fluid)
    }

    /// The top-down surface scan, separated from terrain sampling so a synthetic
    /// column can be compared with the actual `SurfaceSystem.buildSurface`.
    /// `cells` is indexed by `y - min_y`; the scan starts at its top cell.
    fn scan_surface_column(
        &mut self,
        root: usize,
        c: &mut SC,
        est_min: i32,
        cells: &[SurfaceCell],
        heightmap: &[i16; 256],
    ) -> Option<SurfMat> {
        let min_y = self.min_y;
        let scan_top = min_y + cells.len() as i32 - 1;
        if scan_top < est_min {
            return None;
        }
        let mut vals = vec![u16::MAX; (scan_top - est_min + 1) as usize];
        let mut min = i32::MAX;
        let mut sda = 0i32;
        let mut fluid = i32::MIN;
        for y in (est_min..=scan_top).rev() {
            match cells[(y - min_y) as usize] {
                SurfaceCell::Air => {
                    sda = 0;
                    fluid = i32::MIN;
                }
                SurfaceCell::Fluid => {
                    if fluid == i32::MIN {
                        fluid = y + 1;
                    }
                }
                SurfaceCell::Solid => {
                    if min >= y {
                        min = min_y;
                        for sy in (min_y..y).rev() {
                            if cells[(sy - min_y) as usize] != SurfaceCell::Solid {
                                min = sy + 1;
                                break;
                            }
                        }
                    }
                    sda += 1;
                    c.y = y;
                    c.sda = sda;
                    c.sdb = y - min + 1;
                    c.fluid = fluid;
                    if let Some(st) = self.eval_rule(root, c, heightmap) {
                        vals[(y - est_min) as usize] = st.min(u16::MAX as u32 - 1) as u16;
                    }
                }
            }
        }
        Some(SurfMat {
            lo: est_min as i16,
            vals: vals.into_boxed_slice(),
        })
    }

    /// Full block decision (feature layer, material window, aquifer, deep rules)
    /// without taking the GEN lock. `terrain_section` and the free `block_state`
    /// use it so a chunk fill takes one lock instead of five per block.
    pub(crate) fn block_state_at(
        &mut self,
        x: i32,
        y: i32,
        z: i32,
        populate: impl FnMut(&mut Gen, i32, i32),
    ) -> u32 {
        if let Some(st) = self.feat_state(x, y, z, populate) {
            return st;
        }
        // The generator is defined over its own window: a cell above (or below) it is
        // not terrain, it is an empty section, and vanilla answers air there
        // (`NoiseChunk` has no cell, `ChunkAccess` has no palette entry). Without this
        // the density function is *evaluated* outside the window it was built for and
        // invents blocks: measured on the Nether preset, all 32 768 cells of y 128..255
        // in chunk (0,0) came back non-air. `feat_state` above is consulted first
        // because a feature may legitimately write there (the End City does).
        if y < self.min_y || y >= self.min_y + self.height {
            return 0;
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
        let aq = self.aq_state(x, y, z, d);
        if let Some(st) = aq {
            if st == 0 || breg().is_some_and(|r| r.is_air(st)) {
                return 0;
            }
            return st;
        }
        let lx = (x & 15) as usize;
        let lz = (z & 15) as usize;
        let surface_top = cc.wtop[lz * 16 + lx] as i32;
        if surface_material_applies(y, surface_top) {
            if let Some(st) = cc.mat(lx, lz, y) {
                if st != u32::MAX {
                    return self.vein_override(&cc, x, y, z, st);
                }
            }
        }
        match aq {
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
                static ONCE: std::sync::atomic::AtomicBool =
                    std::sync::atomic::AtomicBool::new(true);
                if env_flag!("MC_SPINE_VEINDBG")
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
        // `MC_SPINE_VEINTRACE="x,y,z"`: parsed once.
        static VEIN_TRACE_AT: std::sync::OnceLock<Option<[i32; 3]>> = std::sync::OnceLock::new();
        let dbg = *VEIN_TRACE_AT.get_or_init(|| {
            let v = std::env::var("MC_SPINE_VEINTRACE").ok()?;
            let p: Vec<i32> = v.split(',').filter_map(|n| n.parse().ok()).collect();
            <[i32; 3]>::try_from(p).ok()
        }) == Some([x, y, z]);
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
                    eprintln!(
                        "VEIN   up={up} down={down} at={at:.4} round={roundoff:.4} r1={r1:.4}"
                    );
                }
                if r1 > 0.7 {
                    None
                } else if self.eval_c(v.ridged, x, y, z, &mut memo, Some(cc)) >= 0.0 {
                    if dbg {
                        eprintln!("VEIN   ridged reject");
                    }
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
    ///
    /// Only the generation probes and the contract tests read through this: the
    /// generator itself goes to the material rules directly.
    #[cfg(test)]
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
        let aq = self.aq_state(x, y, z, d);
        if let Some(st) = aq {
            if st == 0 || breg().is_some_and(|r| r.is_air(st)) {
                return 0;
            }
            return st;
        }
        let lx = (x & 15) as usize;
        let lz = (z & 15) as usize;
        let surface_top = cc.wtop[lz * 16 + lx] as i32;
        if surface_material_applies(y, surface_top) {
            if let Some(st) = cc.mat(lx, lz, y) {
                if st != u32::MAX {
                    return self.vein_override(cc, x, y, z, st);
                }
            }
        }
        match aq {
            Some(st) => st,
            None => self.deep_state_cached(x, y, z, cc),
        }
    }
    /// `DimensionType.hasSkyLight()` for this preset. Only the Overworld has a sky
    /// light layer, which is what decides the brightness a feature sees before
    /// `INITIALIZE_LIGHT` (see `generation_providers::uninitialized_raw_brightness`).
    pub(crate) fn has_skylight(&self) -> bool {
        self.preset == NoisePreset::Overworld
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
    pub(crate) fn biome_leaf_of_name(&self, name: &str) -> Option<u16> {
        self.bleaf.get(name).copied()
    }
    pub(crate) fn biome_cold_at_leaf(&self, leaf: u16, x: i32, y: i32, z: i32) -> bool {
        self.biome_weather
            .get(leaf as usize)
            .is_some_and(|weather| weather.cold(x, y, z))
    }
    /// Feature/carver override already recorded for this cell, without populating.
    /// Features run after the carver pass and must see those air/fluid writes.
    pub(crate) fn feature_override(&self, x: i32, y: i32, z: i32) -> Option<u32> {
        let k = ((y - self.min_y) as u32) << 8 | (((z & 15) as u32) << 4) | (x & 15) as u32;
        self.feat_c.get(&(x >> 4, z >> 4))?.get(&k).copied()
    }

    /// Every FEATURES-layer write this generator still holds for one chunk, decoded back to
    /// positions and states: `(pos, state)` for each entry of the chunk's bucket, in the
    /// bucket's own order.
    ///
    /// The promotion-boundary light sample (`generation_context::FC::begin_promotion_pass`) is
    /// the one caller. It needs the emitters a chunk can see, and the write layers are the only
    /// place they can be enumerated without a full terrain scan; the cell *reads* during the
    /// flood still go through `FC::get`, so the terrain is not skipped, only not scanned.
    /// A chunk with no bucket yields nothing, which is the right answer for a chunk nothing
    /// wrote to.
    pub(crate) fn feature_bucket_writes(
        &self,
        cx: i32,
        cz: i32,
    ) -> impl Iterator<Item = ((i32, i32, i32), u32)> + '_ {
        let min_y = self.min_y;
        self.feat_c
            .get(&(cx, cz))
            .into_iter()
            .flat_map(move |bucket| {
                bucket.iter().map(move |(&k, &st)| {
                    let y = min_y + (k >> 8) as i32;
                    let z = cz * 16 + ((k >> 4) & 15) as i32;
                    let x = cx * 16 + (k & 15) as i32;
                    ((x, y, z), st)
                })
            })
    }

    /// feature-layer lookup: runs the default population dispatch on demand, then
    /// returns the override state for (x,y,z) if a feature wrote one.
    pub(crate) fn feat_state(
        &mut self,
        x: i32,
        y: i32,
        z: i32,
        mut populate: impl FnMut(&mut Gen, i32, i32),
    ) -> Option<u32> {
        let (cx, cz) = (x >> 4, z >> 4);
        // Order is an input: vanilla's ticket scheduler is not a coordinate
        // sort. POP_REGION is a legacy lexicographic scan; the default is the
        // named member of vanilla's discipline (`population_discipline`), never
        // a claim of bit-parity with a capture. Offline callers can prepopulate
        // an observed plan through the same execution owner, and a chunk the plan
        // already covers needs no dispatch at all.
        if let Some((px0, pz0, px1, pz1)) = self.pop_region {
            for sx in px0..=px1 {
                for sz in pz0..=pz1 {
                    if (sx, sz) > (cx, cz) {
                        break;
                    }
                    self.populate_in_order([(sx, sz)], &mut populate);
                }
            }
            self.populate_in_order([(cx, cz)], &mut populate);
        } else {
            let nbr = env_str!("MC_SPINE_FEAT_NBR")
                .map(|v| v != "0")
                .unwrap_or(true);
            if !self.pop_done.contains(&(cx, cz)) {
                if nbr {
                    self.populate_request_box(cx, cz, cx, cz, &mut populate);
                } else {
                    self.populate_in_order([(cx, cz)], &mut populate);
                }
            }
        }
        // bound the override maps (LRU over chunks)
        while self.feat_ord.len() > FEATURE_BUCKET_BOUND {
            let old = self.feat_ord.remove(0);
            self.forget_feature_bucket(old);
        }
        let k = ((y - self.min_y) as u32) << 8 | (((z & 15) as u32) << 4) | (x & 15) as u32;
        self.feat_c.get(&(cx, cz))?.get(&k).copied()
    }
    /// Drop a chunk's FEATURES-layer bucket: its override map, its `pop_done` mark and
    /// the `feat_ord` slot that lists it. Both bounds go through here, so the queue and
    /// the map cannot disagree (a stale slot would make the next `feat_write` push a
    /// duplicate and grow the queue without a bound).
    ///
    /// A bucket also holds the carver's writes for that chunk, so the carve claim that
    /// stood for them falls here too — otherwise `apply_carvers` returns at `begin_carve`
    /// and the next FEATURES pass reads uncarved stone. `CarveStage::wg` is *not* touched:
    /// it is a scan of the carved terrain with its own 4096-chunk retention.
    fn forget_feature_bucket(&mut self, key: (i32, i32)) {
        self.feat_c.remove(&key);
        self.pop_done.remove(&key);
        if let Some(i) = self.feat_ord.iter().position(|k| *k == key) {
            self.feat_ord.remove(i);
        }
        if let Some(rec) = self.carve_c.get_mut(&key) {
            rec.blocks_live = false;
        }
        if env_flag!("CHUNK_STAGE_TRACE") {
            eprintln!("[stage] evict bucket {key:?}");
        }
    }
    /// The one place a carve record dies. Dropping a WG value also invalidates the
    /// claim and block bucket, so the next generation can re-derive the whole stage.
    /// Unlike feature-bucket eviction, this removes both retained outputs.
    fn evict_carve_record(&mut self, key: (i32, i32)) {
        self.carve_c.remove(&key);
        self.forget_feature_bucket(key);
        if env_flag!("CHUNK_STAGE_TRACE") {
            eprintln!("[stage] evict record {key:?}");
        }
    }
    /// The feature layer's heightmap: for each column, the highest non-air and the
    /// highest solid block a *feature* has written. Vanilla keeps this per chunk and
    /// updates it on every `setBlock`, so a tree placed for one chunk raises the
    /// heightmap a neighbouring chunk's placements read - `heightmap(OCEAN_FLOOR)`
    /// counts canopy leaves, and a later candidate then fails `would_survive` on the
    /// leaf above it. Keeping the tally inside one chunk's placement pass loses that
    /// (measured: vanilla's candidate (0,70,17) against this port's (0,64,17)).
    ///
    /// The caller classifies the state: the block knowledge lives below this layer
    /// and a generation module may not reach for it (see module-layers/policy.json).
    /// The live feature layer's per-column maxima: highest non-air (`solid` 0) and
    /// highest solid (`solid` 1) block a *feature* has written, +1.
    ///
    /// These are scan **bounds**, not heightmap answers. `FC::heightmap_uncached` starts a
    /// column scan here and walks down with the native predicate
    /// (`BReg::heightmap_counts`) because a later writer can remove the cell these maxima
    /// name, and because the marks are max-only: they never record a removal, they miss
    /// every state the predicate rejects (a leaf above the floor), and they know nothing
    /// about carver writes (`FC::feature_override` is the carver's channel, not this
    /// tally). `MC_SPINE_HM_DUMP` used to print `max(top_oc, tally)` and that is exactly
    /// the bug this comment exists to stop: the printed answer was the bound.
    pub(crate) fn note_feature_write(&mut self, x: i32, y: i32, z: i32, solid: bool, air: bool) {
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
        // The quart y index is the *block* y divided by four, so the sweep has to start at
        // `min_y / 4` (-16 in the overworld), not at 0. Starting at 0 covered y = 0..383 and
        // silently omitted every biome that lives only below y = 0: the chunk's biome set had
        // no `deep_dark`, so `sculk_patch_deep_dark` and `sculk_vein` never ran for a deep-dark
        // chunk (measured: 0 of the 397 sculk cells vanilla places), and the same hole applies
        // to any cave biome confined to the negative range.
        let qy_min = self.min_y / 4;
        let qy_max = (self.min_y + self.height) / 4;
        for qy in qy_min..qy_max {
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

    /// Claim before executing each source chunk, exactly once in caller order.
    pub(crate) fn populate_in_order(
        &mut self,
        order: impl IntoIterator<Item = (i32, i32)>,
        mut populate: impl FnMut(&mut Gen, i32, i32),
    ) -> usize {
        let mut executed = 0;
        for (x, z) in order {
            if self.pop_done.insert((x, z)) {
                self.suppress_generated_ticks = self.restored_tick_sources.contains(&(x, z));
                populate(self, x, z);
                self.suppress_generated_ticks = false;
                executed += 1;
            }
        }
        executed
    }

    /// `ChunkAccess.markPosForPostprocessing`, filed under the chunk that owns the cell
    /// and kept in mark order.
    pub(crate) fn store_shape_mark(&mut self, x: i32, y: i32, z: i32) {
        self.shape_marks
            .entry((x >> 4, z >> 4))
            .or_default()
            .push((x, y, z));
    }

    /// `ChunkMap.protoChunkToFullChunk` promotes a chunk only once the whole 3x3 is
    /// `ChunkStatus.FULL` — the promotion future asks for that status over a radius of
    /// one — so a chunk's `postProcessGeneration` runs after every chunk of its 3x3 has
    /// finished generating. `pop_done` records exactly one entry per generated chunk.
    pub(crate) fn shape_marks_ready(&self, cx: i32, cz: i32) -> bool {
        (-1..=1).all(|dx| (-1..=1).all(|dz| self.pop_done.contains(&(cx + dx, cz + dz))))
    }

    /// The marks of a promoted chunk, in mark order. Consumed once.
    pub(crate) fn take_shape_marks(&mut self, cx: i32, cz: i32) -> Vec<(i32, i32, i32)> {
        self.shape_marks.remove(&(cx, cz)).unwrap_or_default()
    }

    /// Execute the default population order for a request box: the named member of vanilla's
    /// order family documented on [`population_discipline`], over the box shape `MC_SPINE_POP_REGION`
    /// already uses. `.0` is the number of chunks this call decorated.
    pub(crate) fn populate_request_box(
        &mut self,
        x0: i32,
        z0: i32,
        x1: i32,
        z1: i32,
        populate: impl FnMut(&mut Gen, i32, i32),
    ) -> usize {
        let order = population_discipline_order(&request_box(x0, z0, x1, z1));
        self.populate_in_order(order, populate)
    }

    /// Populate the request a diagnostic makes, so it reads the feature-layer state a generation
    /// loop would hold for that chunk. The driver is passed in, as at every other call site here:
    /// the choice of placement implementation belongs to the caller's boundary, not to worldgen.
    ///
    /// A chunk that is already decorated needs nothing: everything that can write into its cells
    /// either decorated them or was decorated itself when the demand that reached it was served
    /// (a writer sits within `POP_WRITE_RADIUS` of it, so within the same three rings).
    pub(crate) fn ensure_populated(
        &mut self,
        cx: i32,
        cz: i32,
        mut populate: impl FnMut(&mut Gen, i32, i32),
    ) {
        if self.pop_done.contains(&(cx, cz)) {
            return;
        }
        self.populate_request_box(cx, cz, cx, cz, &mut populate);
    }

    /// Highest non-air (`0`) / solid (`1`) feature-written block + 1 in a column.
    pub(crate) fn feature_height(&self, x: i32, z: i32, solid: usize) -> i32 {
        match self.feat_hm.get(&(x, z)) {
            Some(e) => {
                if solid == 0 {
                    e.0
                } else {
                    e.1
                }
            }
            None => i32::MIN,
        }
    }

    /// Whether a feature write may land in the chunk that owns `(x, z)`.
    ///
    /// This models `WorldGenRegion.ensureCanWrite` (`arb.f_`), which rejects a chunk
    /// further than the FEATURES step's `blockStateWriteRadius` from the decorated
    /// chunk: the 1.21.1 `ChunkPyramid` (`dvy`) builds that step as
    /// `features -> carvers@1 + struct_starts@8 + .a(1)`, and `arb.f_` compares the
    /// chunk offset with that radius (Chebyshev, in chunks). Writes outside it are
    /// dropped with only a log line, so the feature sees its own cell unchanged.
    ///
    /// The other vanilla refusal is *not* modelled, deliberately. Once a chunk is
    /// promoted to a `LevelChunk`, `ChunkMap` installs
    /// `new ImposterProtoChunk(levelChunk, false)` into every proto-status slot and
    /// `ImposterProtoChunk.setBlockState` (`dvh.a(jd,dtc,Z)`) then returns `null`
    /// without touching the chunk's blocks or heightmaps - while `arb.a(...)` still
    /// returns true, which is why a `FCELL ... wrote=true` trace line proves nothing
    /// about persistence (see `tools/carve-agent/README.md`). That flag flips at
    /// promotion, not when the chunk's own FEATURES pass returns: measured on the
    /// pinned Nether fixture (`/tmp/nat-4242-nether-fossil/world`, grid (-25,19)+3x3
    /// with the recorded pop order), treating "its pass finished" as read-only moved
    /// 34 diffs to 1183, because native accepted a later pass' writes into an
    /// already-decorated neighbour that was still a writable proto chunk (blackstone
    /// at (-385,10,304) from pop (-24,19)). The offline generator has no promotion
    /// stage, so modeling that half would refuse writes vanilla kept.
    pub(crate) fn feature_write_allowed(&self, x: i32, z: i32) -> bool {
        let Some((px, pz)) = self.pop_active else {
            // Not inside a FEATURES pass (unit tests, template post-processing):
            // there is no region and therefore no window to enforce.
            return true;
        };
        let (cx, cz) = (x >> 4, z >> 4);
        (cx - px).abs() <= 1 && (cz - pz).abs() <= 1
    }

    pub(crate) fn feat_write(&mut self, x: i32, y: i32, z: i32, st: u32) {
        // The FEATURES-layer storage gate is the **world** window: 1.21.1's End City
        // builds above the End's generator top (measured: 273 cells at y 128..143 in
        // chunk (65,45) of the pinned capture), and vanilla's
        // `WorldGenRegion.ensureCanWrite` bound is the level's build height.
        if y < self.world_min_y || y >= self.world_min_y + self.world_height {
            return;
        }
        let key = (x >> 4, z >> 4);
        let k = ((y - self.min_y) as u32) << 8 | (((z & 15) as u32) << 4) | (x & 15) as u32;
        if !self.feat_c.contains_key(&key) {
            self.feat_ord.push(key);
        }
        self.feat_c.entry(key).or_default().insert(k, st);
    }
    pub(crate) fn stage_payloads(
        &mut self,
        source: (i32, i32),
        items: &mut Vec<((i32, i32, i32), crate::generation_payload::GenerationPayload)>,
    ) {
        if self.payload_sources.insert(source) {
            self.staged_payloads.append(items);
        } else {
            // Recomputing evicted blocks must not reset NBT or recreate entities.
            items.clear();
        }
    }

    /// Hand the staged contents to the caller. Taken by value so a second call cannot
    /// double-apply them.
    pub(crate) fn take_staged_payloads(
        &mut self,
    ) -> Vec<((i32, i32, i32), crate::generation_payload::GenerationPayload)> {
        std::mem::take(&mut self.staged_payloads)
    }

    pub(crate) fn wseed(&self) -> u64 {
        self.world_seed
    }

    /// Claim the one carver pass for a chunk. Vanilla writes CARVERS before
    /// FEATURES for every chunk in the decoration area; claiming here lets the
    /// feature context pre-carve its neighbours without carving them twice.
    pub(crate) fn begin_carve(&mut self, cx: i32, cz: i32) -> bool {
        let key = (cx, cz);
        if let Some(rec) = self.carve_c.get_mut(&key) {
            if rec.blocks_live {
                return false;
            }
            // The bucket that held this pass' writes is gone (`forget_feature_bucket`),
            // so re-running is the only way to get them back. `wg` stays: it is a scan of
            // the carved terrain, not a read of `feat_c`, and the re-run reproduces it.
            if env_flag!("CHUNK_STAGE_TRACE") {
                eprintln!("[stage] reclaim carve {cx},{cz} (block output evicted)");
            }
            rec.blocks_live = true;
            return true;
        }
        self.carve_c.insert(
            key,
            CarveStage {
                wg: None,
                blocks_live: true,
            },
        );
        true
    }

    /// Record a chunk's WG heightmaps as the carver pass leaves them (see
    /// `CarveStage::wg` for why the carver, and only the carver, moves them).
    pub(crate) fn set_wg_after_carve(&mut self, cx: i32, cz: i32, tops: Box<[i32; 512]>) {
        let key = (cx, cz);
        // A caller that hands over a carved scan without claiming the pass first (the
        // `wg_heightmap_keeps_the_carved_top` fixture) records the value and no block
        // output: the same state `forget_feature_bucket` leaves behind, so the next
        // `begin_carve` claims and re-runs instead of suppressing the pass.
        let had_wg = {
            let rec = self.carve_c.entry(key).or_insert(CarveStage {
                wg: None,
                blocks_live: false,
            });
            rec.wg.replace(tops).is_some()
        };
        if !had_wg {
            self.carve_ord.push_back(key);
        }
        // Decoration reaches the 17x17 region plus the structure reach, so a value must
        // outlive the pass that wrote it; `CARVE_WG_BOUND` chunks is the approved bound
        // and it is *this* record's bound — not the 384-chunk `feat_c` one. Dropping the
        // record drops its block output with it (`evict_carve_record`), so the chunk
        // returns to the not-yet-carved stage and the next generation re-derives both;
        // an evicted value never leaves a claim behind that could not recover it.
        while self.carve_ord.len() > CARVE_WG_BOUND {
            let Some(old) = self.carve_ord.pop_front() else {
                break;
            };
            if old == key {
                // The record being written is the oldest one: keep it, evict the next.
                self.carve_ord.push_back(old);
                continue;
            }
            self.evict_carve_record(old);
        }
    }

    /// The stored post-carve WG height for a column, first-available form
    /// (`topSolid + 1`), or `None` when this generator holds no post-carve value for that
    /// chunk. `None` means no WG value is retained; a newly claimed pass also has no
    /// value until `set_wg_after_carve` hands over its scan. Evicting a completed WG
    /// value invalidates the claim too, allowing the next generation to re-derive it.
    pub(crate) fn wg_after_carve_height(&self, ocean_floor: bool, x: i32, z: i32) -> Option<i32> {
        let tops = self.carve_c.get(&(x >> 4, z >> 4))?.wg.as_ref()?;
        let base = if ocean_floor { 256 } else { 0 };
        Some(tops[base + (z & 15) as usize * 16 + (x & 15) as usize])
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
        Some(trilinear(cells, self, x, y, z))
    }
}

#[inline]
fn trilinear(cells: &[f64], columns: &ChunkCols, x: i32, y: i32, z: i32) -> f64 {
    let (ny, min_y) = (columns.ny, columns.min_y);
    let nx = (16 / columns.cell_w + 1) as usize;
    let (width, height) = (f64::from(columns.cell_w), f64::from(columns.cell_h));
    let lx = (x & 15) as f64;
    let lz = (z & 15) as f64;
    let ly = (y - min_y) as f64;
    let xi = (lx / width).floor().min((nx - 2) as f64) as usize;
    let zi = (lz / width).floor().min((nx - 2) as f64) as usize;
    let yi = (ly / height).floor().min((ny - 2) as f64) as usize;
    let fx = lx / width - xi as f64;
    let fz = lz / width - zi as f64;
    let fy = ly / height - yi as f64;
    let g = |xi: usize, yi: usize, zi: usize| cells[(zi * nx + xi) * ny + yi];
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
    match gen_lock().as_ref() {
        Some(g) => g.block_density(cc, x, y, z),
        None => 0.0,
    }
}

pub fn debug_eval(name: &str, x: i32, y: i32, z: i32) -> Option<f64> {
    let mut g = gen_lock();
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

pub fn sea_level() -> i32 {
    facts().map(|f| f.sea).unwrap_or(63)
}

/// material-rule result for blocks below the surface window (bedrock,
/// ore veins, deepslate gradient, underground fallback -> default block).
/// stone_depth/fluid context is pinned "deep" — vanilla underground rules
/// do not read them.
#[cfg(test)]
pub fn deep_state(x: i32, y: i32, z: i32, cc: &ChunkCols) -> u32 {
    let mut gg = gen_lock();
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
        if let Some(&v) = self.deep_c.get(&(x, y, z)) {
            return v;
        }
        let v = self.deep_state_i(x, y, z, cc);
        self.deep_c.insert((x, y, z), v);
        self.deep_ord.push_back((x, y, z));
        while self.deep_ord.len() > 500_000 {
            if let Some(old) = self.deep_ord.pop_front() {
                self.deep_c.remove(&old);
            }
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
        &mut self,
        x: i32,
        y: i32,
        z: i32,
        is_fluid: bool,
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
        self.eval_rule(root, &mut c, &cc.wtop)
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
        self.eval_rule(root, &mut c, &cc.wtop)
            .unwrap_or(self.def_st)
    }
}
pub fn min_y() -> i32 {
    facts().map(|f| f.min_y).unwrap_or(-64)
}
pub fn max_y() -> i32 {
    facts().map(|f| f.min_y + f.height - 1).unwrap_or(319)
}
/// The **world** window's floor (`dimension_type` `min_y`): the bottom of the chunk a
/// player stands in, which is not necessarily the bottom of the terrain.
///
/// `min_y()`/`max_y()` above are the generator's window and stay that way: the density
/// lattice, the surface scans and the biome quart sweep are defined over it, so moving
/// them would rewrite terrain. Use these for storage and bounds: chunk sections,
/// heightmaps, the offline dump length, feature/structure write bounds and structure
/// placement boxes.
pub fn world_min_y() -> i32 {
    facts().map(|f| f.world_min_y).unwrap_or(-64)
}
pub fn world_height() -> i32 {
    facts().map(|f| f.world_height).unwrap_or(384)
}
pub fn world_max_y() -> i32 {
    world_min_y() + world_height() - 1
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
    (0, 0),
    (-2, -1),
    (-1, -1),
    (0, -1),
    (1, -1),
    (-3, 0),
    (-2, 0),
    (-1, 0),
    (1, 0),
    (-2, 1),
    (-1, 1),
    (0, 1),
    (1, 1),
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
                        sx + dx,
                        sy + dy,
                        sz + dz,
                        px,
                        py,
                        pz
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
            let o = n.wrapping_add(8);
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
        // OverworldBiomeBuilder.isDeepDark: erosion < -0.225F && depth > 0.9F.
        // NoiseRouter.h()/i() are erosion/depth accessors (the fields are named
        // continents/erosion/depth); keep the promoted f32 literals.
        let deep = match self.climate {
            Some(c) => {
                self.eval(c[3], x, y, z, &mut memo) < -0.225f32 as f64
                    && self.eval(c[4], x, y, z, &mut memo) > 0.9f32 as f64
            }
            None => false,
        };
        let (d, e) = if deep {
            (-1.0, -1.0)
        } else {
            let top = min_est.wrapping_add(8).wrapping_sub(y);
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
        let s = self.eval(
            self.aq.as_ref().unwrap().spread,
            gx,
            gy,
            gz,
            &mut HashMap::new(),
        ) * 10.0;
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
        // Vanilla: !(q < -2.0) && !(q > 2.0), which is true for NaN.
        let r = if q.is_nan() || (-2.0..=2.0).contains(&q) {
            match *barrier {
                Some(v) => v,
                None => {
                    let v = self.eval(
                        self.aq.as_ref().unwrap().barrier,
                        x,
                        y,
                        z,
                        &mut HashMap::new(),
                    );
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
#[cfg(test)]
pub fn aquifer_state(x: i32, y: i32, z: i32, density: f64) -> Option<u32> {
    gen_lock().as_mut()?.aq_state(x, y, z, density)
}

/// The block state at a world position, taking the GEN lock once.
/// One cell. Payloads stay generator-owned until a safe runtime boundary or
/// save drains them under save -> generator -> runtime lock order. A cell read
/// can hold a mob/player lock, so it must never acquire the save lock or create
/// a take-before-save gap between a source claim and its payload.
pub fn block_state(
    x: i32,
    y: i32,
    z: i32,
    populate: impl FnMut(&mut Gen, i32, i32),
) -> Option<u32> {
    let mut guard = gen_lock();
    Some(guard.as_mut()?.block_state_at(x, y, z, populate))
}

/// Base terrain states for one 16x16x16 section, in the send layout
/// `(ly << 8) | (lz << 4) | lx`, with one GEN lock for the whole section.
pub fn terrain_section(
    cx: i32,
    sy: i32,
    cz: i32,
    mut populate: impl FnMut(&mut Gen, i32, i32),
) -> Option<Box<[u32; 4096]>> {
    let mut gg = gen_lock();
    let g = gg.as_mut()?;
    let mut arr = Box::new([0u32; 4096]);
    for ly in 0..16i32 {
        let y = sy + ly;
        for lz in 0..16i32 {
            for lx in 0..16i32 {
                let x = cx * 16 + lx;
                let z = cz * 16 + lz;
                arr[((ly << 8) | (lz << 4) | lx) as usize] =
                    g.block_state_at(x, y, z, &mut populate);
            }
        }
    }
    Some(arr)
}

/// feature-layer override lookup (populates 3x3 chunks lazily on first touch)
pub fn feature_state(
    x: i32,
    y: i32,
    z: i32,
    populate: impl FnMut(&mut Gen, i32, i32),
) -> Option<u32> {
    gen_lock().as_mut()?.feat_state(x, y, z, populate)
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
        if def_blk == 2 {
            if aq_trace_at(x, z) {
                eprintln!("Aqd {x} {y} {z} d={density} lava");
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
                eprintln!("Aqd {x} {y} {z} d={density} sim {blk} sim12={sim12:.4} l0={l2:?}");
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
                eprintln!("Aqd {x} {y} {z} d={density} lavaunder {blk} sim12={sim12:.4} l0={l2:?}");
            }
            return Some(water);
        }
        let mut barrier = None;
        let l3 = g.aq_water_level(near[1].0, near[1].1, near[1].2);
        let c12 = g.aq_calc_density(x, y, z, l2, l3, &mut barrier);
        let (mut c13, mut c23) = (0.0f64, 0.0f64);
        if density + sim12 * c12 > 0.0 {
            if aq_trace_at(x, z) {
                eprintln!("Aqd {x} {y} {z} d={density} null 12 sim={sim12:.4} c12={c12:.4} c13=0 c23=0 l0={:?} l1={:?} l2={:?}", l2, l3, (0, 0));
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
                    eprintln!("Aqd {x} {y} {z} d={density} null 13 sim={sim12:.4} c12={c12:.4} s13={sim13:.4} c13={c13:.4} l0={:?} l1={:?} l2={:?}", l2, l3, l4);
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
                    eprintln!("Aqd {x} {y} {z} d={density} null 23 sim={sim12:.4} s23={sim23:.4} c23={c23:.4} l0={:?} l1={:?} l2={:?}", l2, l3, l4);
                }
                return None;
            }
        }
        if aq_trace_at(x, z) {
            eprintln!("Aqd {x} {y} {z} d={density} state {blk} sim={sim12:.4} c12={c12:.4} c13={c13:.4} c23={c23:.4} l0={:?} l1={:?} l2={:?}", l2, l3, l4);
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
    "badlands",
    "bamboo_jungle",
    "basalt_deltas",
    "beach",
    "birch_forest",
    "cherry_grove",
    "cold_ocean",
    "crimson_forest",
    "dark_forest",
    "deep_cold_ocean",
    "deep_dark",
    "deep_frozen_ocean",
    "deep_lukewarm_ocean",
    "deep_ocean",
    "desert",
    "dripstone_caves",
    "end_barrens",
    "end_highlands",
    "end_midlands",
    "eroded_badlands",
    "flower_forest",
    "forest",
    "frozen_ocean",
    "frozen_peaks",
    "frozen_river",
    "grove",
    "ice_spikes",
    "jagged_peaks",
    "jungle",
    "lukewarm_ocean",
    "lush_caves",
    "mangrove_swamp",
    "meadow",
    "mushroom_fields",
    "nether_wastes",
    "ocean",
    "old_growth_birch_forest",
    "old_growth_pine_taiga",
    "old_growth_spruce_taiga",
    "plains",
    "river",
    "savanna",
    "savanna_plateau",
    "small_end_islands",
    "snowy_beach",
    "snowy_plains",
    "snowy_slopes",
    "snowy_taiga",
    "soul_sand_valley",
    "sparse_jungle",
    "stony_peaks",
    "stony_shore",
    "sunflower_plains",
    "swamp",
    "taiga",
    "the_end",
    "the_void",
    "warm_ocean",
    "warped_forest",
    "windswept_forest",
    "windswept_gravelly_hills",
    "windswept_hills",
    "windswept_savanna",
    "wooded_badlands",
];

fn biome_id(name: &str) -> i64 {
    let n = name.rsplit(':').next().unwrap_or(name);
    BIOME64
        .iter()
        .position(|&b| b == n)
        .map(|i| i as i64)
        .unwrap_or(39) // plains
}

#[derive(Debug, Clone)]
enum BT {
    Leaf([[i32; 2]; 7], u16),
    Sub([[i32; 2]; 7], Vec<BT>),
}

fn bt_params(j: &J) -> [[i32; 2]; 7] {
    let mut p = [[0i32; 2]; 7];
    if let Some(a) = j.get("parameters").and_then(J::as_arr) {
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
                .and_then(J::as_arr)?
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
    /// `SearchTree.TreeNode.getResultingNode`. `best` is the incumbent (distance, leaf): the
    /// previous result when the caller has one, else `(i64::MAX, _)`. A branch scans its
    /// children **in array order**, descends into a child only when the child's box is
    /// strictly nearer than the incumbent (`if (l > m)`), and a leaf replaces the incumbent
    /// only when strictly nearer (`if (l > n)`). So among equally near leaves the incumbent
    /// wins, then the one reached first in array order - which is why the order must not be
    /// changed (sorting the children by distance reaches a different leaf on ties, and also
    /// prunes differently).
    fn search(&self, t: &[i64; 7], best: &mut (i64, u16)) {
        match self {
            BT::Leaf(_, id) => {
                let d = self.dist(t);
                if d < best.0 {
                    *best = (d, *id);
                }
            }
            BT::Sub(_, ch) => {
                for c in ch {
                    if best.0 > c.dist(t) {
                        c.search(t, best);
                    }
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
            None => {
                (self.eval(
                    self.climate?[4],
                    qx << 2,
                    qy << 2,
                    qz << 2,
                    &mut HashMap::new(),
                ) * 10000.0) as i64
            }
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
        if self.preset == NoisePreset::End {
            let (cx, cz) = (qx.wrapping_mul(4) >> 4, qz.wrapping_mul(4) >> 4);
            let leaf = if i64::from(cx) * i64::from(cx) + i64::from(cz) * i64::from(cz) <= 4096 {
                0
            } else {
                let x = cx.wrapping_mul(2).wrapping_add(1).wrapping_mul(8);
                let z = cz.wrapping_mul(2).wrapping_add(1).wrapping_mul(8);
                let erosion = self.eval(
                    self.climate?[3],
                    x,
                    qy.wrapping_mul(4),
                    z,
                    &mut HashMap::new(),
                );
                if erosion > 0.25 {
                    1
                } else if erosion >= -0.0625 {
                    2
                } else if erosion < -0.21875 {
                    3
                } else {
                    4
                }
            };
            return Some((biome_id(self.biome_names.get(leaf)?), leaf as u16));
        }
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
    let mut gg = gen_lock();
    let g = gg.as_mut()?;
    let t = g.target7(qx, qy, qz)?;
    let mut best = (i64::MAX, 0u16);
    g.btree.as_ref()?.search(&t, &mut best);
    Some((biome_id(&g.biome_names[best.1 as usize].clone()), t))
}

/// Block-space biome name through the same quart lookup as the feature layer.
pub fn biome_name_at_block(x: i32, y: i32, z: i32) -> Option<String> {
    gen_lock().as_mut()?.biome_name_at(x, y, z)
}

/// quart-space biome id (public for debug/tests)
pub fn biome_at(qx: i32, qy: i32, qz: i32) -> Option<i64> {
    let mut gg = gen_lock();
    gg.as_mut()?.biome_quart(qx, qy, qz, None).map(|(b, _)| b)
}

/// 4x4x4 biome ids for one chunk section, quart-space index (y<<4|z<<2|x).
/// `MC_SPINE_HM_DUMP=cx,cz` lives in `generation_context::hm_dump`: the live maps are
/// what a feature reads (`FC::hm_oc`/`hm_ws`), and `worldgen` may not reach for the
/// generation context (module layers).


/// `MC_SPINE_BIOME_AT=x,y,z;x,y,z;...`: print both biome notions at each position.
/// The container (what a saved chunk holds) and the blended lookup that
/// `LevelReader.getBiome` returns are different answers; a placement filter reads
/// the second, so seeing both at once is how a boundary disagreement is read.
pub fn biome_at_dump(spec: &str) {
    let mut gg = gen_lock();
    let Some(g) = gg.as_mut() else { return };
    for item in spec.split(';') {
        let p: Vec<i32> = item.split(',').filter_map(|n| n.parse().ok()).collect();
        if p.len() != 3 {
            continue;
        }
        let (x, y, z) = (p[0], p[1], p[2]);
        let blended = g
            .biome_blended(x, y, z)
            .and_then(|(_, l)| g.biome_name_of_leaf(l));
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
    let mut gg = gen_lock();
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
                        (qx0 + lx) * 4,
                        (qy0 + ly) * 4,
                        (qz0 + lz) * 4,
                        name
                    );
                }
            }
        }
    }
}

pub fn biome_section(cx: i32, sy: i32, cz: i32) -> Option<Box<[i64; 64]>> {
    let mut gg = gen_lock();
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

/// The generator lock, for tests outside `wg_tests` that drive `feature::populate`.
///
/// `wg_tests` is private, so the reachable path is this wrapper: a parent module can see a
/// child's private items, and the lock has to be the *same* mutex the generator tests use.
#[cfg(test)]
pub(crate) fn lock_gen_for_tests() -> std::sync::MutexGuard<'static, ()> {
    wg_tests::lock_gen()
}

#[cfg(test)]
mod wg_tests {
    use super::*;

    #[test]
    fn datapack_resources_respect_priority_and_confine_id_paths() {
        let root = std::env::temp_dir().join(format!("mc-resource-paths-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let low = root.join("low");
        let high = root.join("high");
        for pack in [&low, &high] {
            std::fs::create_dir_all(pack.join("data/minecraft/structure/fossil")).unwrap();
        }
        std::fs::write(low.join("data/minecraft/structure/fossil/test.nbt"), b"low").unwrap();
        std::fs::write(
            high.join("data/minecraft/structure/fossil/test.nbt"),
            b"high",
        )
        .unwrap();
        let packs = [low, high.clone()];
        assert_eq!(
            Gen::dp_resource_in(&packs, "structure", "minecraft:fossil/test", "nbt"),
            Some(b"high".to_vec())
        );
        let readme = root.join("README.txt");
        std::fs::write(&readme, b"not a pack directory").unwrap();
        let entries = [packs[0].clone(), packs[1].clone(), readme];
        assert_eq!(
            Gen::dp_resource_in(&entries, "structure", "minecraft:fossil/test", "nbt"),
            Some(b"high".to_vec()),
            "ordinary datapacks/ entries must not mask real resources"
        );
        std::fs::create_dir_all(packs[0].join("data/minecraft/structure/nested")).unwrap();
        std::fs::write(
            packs[0].join("data/minecraft/structure/nested/test.nbt"),
            b"nested-low",
        )
        .unwrap();
        std::fs::write(
            high.join("data/minecraft/structure/nested"),
            b"not a directory",
        )
        .unwrap();
        assert_eq!(
            Gen::dp_resource_in(&packs, "structure", "minecraft:nested/test", "nbt"),
            Some(b"nested-low".to_vec()),
            "a non-directory prefix is an absent resource, not a shadowing resource"
        );
        std::fs::create_dir_all(high.join("structure")).unwrap();
        std::fs::write(high.join("structure/escape.nbt"), b"outside namespace").unwrap();
        std::fs::write(
            high.join("data/minecraft/escape.nbt"),
            b"outside resource kind",
        )
        .unwrap();
        // The targets EXIST: removing either path guard must really expose a file.
        for id in ["..:escape", "minecraft:../escape"] {
            assert!(
                Gen::dp_resource_in(&packs, "structure", id, "nbt").is_none(),
                "unsafe resource id resolved: {id}"
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn generated_ticks_belong_to_their_generator() {
        let mut first = Gen::load(0, crate::name_to_state).unwrap();
        let mut second = Gen::load(1, crate::name_to_state).unwrap();
        let leaf = ((1, 80, 2), 264);
        first.schedule_generated_leaf(leaf.0, leaf.1);
        assert!(
            second.generated_leaf_ticks().is_empty(),
            "another generator's ticks leaked"
        );
        second.restore_generated_ticks(Vec::new(), Vec::new());
        assert_eq!(
            first.generated_leaf_ticks(),
            vec![leaf],
            "restore cleared another world"
        );
    }

    #[test]
    fn runtime_drains_only_published_ticks_without_the_gen_lock() {
        let _guard = lock_gen();
        assert!(init(0, crate::name_to_state));
        let _ = take_generated_leaf_ticks();
        let _ = take_generated_fluid_ticks();
        let leaf = ((1, 80, 2), 264);
        let fluid = ((3, 60, 4), 1);
        with_gen(|g| {
            g.schedule_generated_leaf(leaf.0, leaf.1);
            g.schedule_generated_leaf(leaf.0, leaf.1);
            g.schedule_generated_fluid(fluid.0, fluid.1);
        })
        .unwrap();
        let mut local = Gen::load(42, crate::name_to_state).unwrap();
        local.schedule_generated_leaf((9, 80, 9), 264);
        local.schedule_generated_fluid((9, 60, 9), 1);
        // Hold GEN while another thread drains: reintroducing a GEN acquisition
        // here must fail promptly rather than deadlock the test process.
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = with_gen(|_| {
            let worker = std::thread::spawn(move || {
                tx.send((take_generated_leaf_ticks(), take_generated_fluid_ticks()))
                    .unwrap();
            });
            let got = rx.recv_timeout(std::time::Duration::from_secs(2));
            (worker, got)
        })
        .unwrap();
        worker.0.join().unwrap();
        let (leaves, fluids) = worker.1.expect("runtime drain waited on GEN");
        assert_eq!(
            leaves,
            vec![leaf],
            "unpublished generator leaked into runtime"
        );
        assert_eq!(
            fluids,
            vec![fluid],
            "unpublished generator leaked into runtime"
        );
        assert_eq!(local.generated_leaf_ticks(), vec![((9, 80, 9), 264)]);
        with_gen(|g| g.schedule_generated_leaf(leaf.0, leaf.1)).unwrap();
        assert_eq!(
            take_generated_leaf_ticks(),
            vec![leaf],
            "drain retained the dedup set"
        );
        with_gen(|g| g.restore_generated_ticks(vec![leaf], vec![fluid])).unwrap();
        local.restore_generated_ticks(Vec::new(), Vec::new());
        assert!(init(1, crate::name_to_state));
        assert!(take_generated_leaf_ticks().is_empty());
        assert!(take_generated_fluid_ticks().is_empty());
    }

    #[test]
    fn interpolation_uses_noise_preset_cell_dimensions() {
        for (cell_w, cell_h, min_y, world_height) in
            [(4, 8, -64, 384), (8, 4, 0, 128), (4, 8, 0, 128)]
        {
            let nx = (16 / cell_w + 1) as usize;
            let ny = (world_height / cell_h + 1) as usize;
            let columns = ChunkCols {
                grids: Vec::new(),
                surf: [0; 256],
                ny,
                cell_w,
                cell_h,
                min_y,
                mats: Vec::new(),
                rd: [0; 256],
                wtop: [0; 256],
                beard: None,
            };
            let (cx, cz) = (-2, 3);
            let field = |x, y, z| f64::from(x) * 3.0 + f64::from(y) * 0.5 + f64::from(z) * 7.0;
            let mut grid = vec![0.0; nx * nx * ny];
            for zi in 0..nx {
                for xi in 0..nx {
                    for yi in 0..ny {
                        grid[(zi * nx + xi) * ny + yi] = field(
                            cx * 16 + xi as i32 * cell_w,
                            min_y + yi as i32 * cell_h,
                            cz * 16 + zi as i32 * cell_w,
                        );
                    }
                }
            }
            for z in cz * 16..(cz + 1) * 16 {
                for x in cx * 16..(cx + 1) * 16 {
                    for y in min_y..min_y + world_height {
                        assert_eq!(
                            trilinear(&grid, &columns, x, y, z).to_bits(),
                            field(x, y, z).to_bits(),
                            "cell {cell_w}x{cell_h}: {x},{y},{z}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn end_features_match_native_full_feature_chunks() {
        compare_end_features(
            -116,
            1262,
            "minecraft:end_stone",
            include_bytes!("../tests/data/end_small_features_1211.bin.gz"),
            include_str!("../tests/data/end_small_features_1211.order.tsv"),
        );
        compare_end_features(
            128,
            -128,
            "minecraft:chorus_plant",
            include_bytes!("../tests/data/end_highlands_features_1211.bin.gz"),
            include_str!("../tests/data/end_highlands_features_1211.order.tsv"),
        );
    }

    fn compare_end_features(
        cx0: i32,
        cz0: i32,
        positive_name: &str,
        compressed: &[u8],
        order: &str,
    ) {
        use std::io::Read;
        let mut expected = Vec::new();
        flate2::read::GzDecoder::new(compressed)
            .read_to_end(&mut expected)
            .unwrap();
        assert_eq!(expected.len(), 9 * 32768 * 4);
        let mut g = Gen::load_preset(0, crate::name_to_state, NoisePreset::End).unwrap();
        let order = order.lines().map(|line| {
            let (x, z) = line.split_once('\t').unwrap();
            (x.parse().unwrap(), z.parse().unwrap())
        });
        assert_eq!(g.populate_in_order(order, crate::feature::populate), 25);
        let mut states = expected.as_chunks::<4>().0.iter();
        let mut positive_features = 0;
        for cz in cz0..cz0 + 3 {
            for cx in cx0..cx0 + 3 {
                for y in 0..128 {
                    for z in cz * 16..(cz + 1) * 16 {
                        for x in cx * 16..(cx + 1) * 16 {
                            let want = u32::from_le_bytes(*states.next().unwrap());
                            let got = g.block_state_at(x, y, z, |_, _, _| {});
                            assert_eq!(got, want, "native End features: {x},{y},{z}");
                            positive_features += usize::from(
                                g.feature_override(x, y, z)
                                    .is_some_and(|s| breg().unwrap().name_of(s) == positive_name),
                            );
                        }
                    }
                }
            }
        }
        assert!(states.next().is_none());
        assert!(positive_features > 0, "positive decorated cells required");
    }

    #[test]
    fn nether_carvers_match_all_native_fixture_states() {
        use std::io::Read;
        let mut expected = Vec::new();
        flate2::read::GzDecoder::new(
            include_bytes!("../tests/data/nether_carvers_1211.bin.gz").as_slice(),
        )
        .read_to_end(&mut expected)
        .unwrap();
        assert_eq!(expected.len(), 9 * 32768 * 4);
        let mut g = Gen::load_preset(0, crate::name_to_state, NoisePreset::Nether).unwrap();
        for cz in 0..3 {
            for cx in 0..3 {
                crate::feature::apply_carvers(&mut g, cx, cz);
            }
        }
        let mut states = expected.as_chunks::<4>().0.iter();
        let mut cave_air = 0;
        for cz in 0..3 {
            for cx in 0..3 {
                for y in 0..128 {
                    for z in cz * 16..(cz + 1) * 16 {
                        for x in cx * 16..(cx + 1) * 16 {
                            let want = u32::from_le_bytes(*states.next().unwrap());
                            let got = g.block_state_at(x, y, z, |_, _, _| {});
                            assert_eq!(got, want, "native Nether carvers: {x},{y},{z}");
                            cave_air +=
                                usize::from(breg().unwrap().name_of(got) == "minecraft:cave_air");
                        }
                    }
                }
            }
        }
        assert!(states.next().is_none());
        assert!(cave_air > 0, "positive carved cells required");
    }

    #[test]
    fn dimension_terrain_matches_all_native_fixture_states() {
        use std::io::Read;
        type TerrainCase = (NoisePreset, u64, i32, i32, i32, &'static [u8]);
        let cases: &[TerrainCase] = &[
            (
                NoisePreset::End,
                0,
                0,
                0,
                1,
                include_bytes!("../tests/data/dimension_terrain_1211/0-end.bin.gz"),
            ),
            (
                NoisePreset::End,
                u64::MAX,
                100,
                -103,
                3,
                include_bytes!("../tests/data/dimension_terrain_1211/1-end.bin.gz"),
            ),
            (
                NoisePreset::Nether,
                0,
                0,
                0,
                1,
                include_bytes!("../tests/data/dimension_terrain_1211/2-nether.bin.gz"),
            ),
            (
                NoisePreset::Nether,
                u64::MAX,
                -65,
                32,
                3,
                include_bytes!("../tests/data/dimension_terrain_1211/3-nether.bin.gz"),
            ),
        ];
        let mut compared = 0;
        for &(preset, seed, cx, cz, n, bytes) in cases {
            let mut expected = Vec::new();
            flate2::read::GzDecoder::new(bytes)
                .read_to_end(&mut expected)
                .unwrap();
            assert_eq!(expected.len(), (n * n * 128 * 256 * 4) as usize);
            let mut g = Gen::load_preset(seed, crate::name_to_state, preset).unwrap();
            assert_eq!((g.min_y, g.height), (0, 128));
            let mut states = expected.as_chunks::<4>().0.iter();
            for dz in 0..n {
                for dx in 0..n {
                    let columns = g.cols(cx + dx, cz + dz).unwrap();
                    for y in 0..128 {
                        for z in (cz + dz) * 16..(cz + dz + 1) * 16 {
                            for x in (cx + dx) * 16..(cx + dx + 1) * 16 {
                                let want = u32::from_le_bytes(*states.next().unwrap());
                                assert_eq!(
                                    g.terrain_cell(&columns, x, y, z),
                                    want,
                                    "{preset:?} seed={seed} at {x},{y},{z}"
                                );
                                compared += 1;
                            }
                        }
                    }
                }
            }
            assert!(states.next().is_none());
        }
        assert_eq!(compared, 655_360);
    }

    /// The whole Overworld chunk the pinned server produced, at canonical-state level.
    ///
    /// The capture is a real 1.21.1 server run (`tests/data/overworld_chunk_0_0_1211.tsv.gz`,
    /// seed 0, chunk 0,0, features on, no players). Two runs of that input differed in four
    /// cells, so the port is compared against the run it matches; the four noisy cells are
    /// named in the provenance rather than tolerated here.
    #[test]
    fn overworld_chunk_matches_native_capture_state_for_state() {
        use std::io::Read;
        let mut expected_text = String::new();
        flate2::read::GzDecoder::new(&include_bytes!(
            "../tests/data/overworld_chunk_0_0_1211.tsv.gz"
        )[..])
        .read_to_string(&mut expected_text)
        .unwrap();
        let reg = crate::block_registry::breg().unwrap();
        let mut want: std::collections::HashMap<(i32, i32, i32), String> =
            std::collections::HashMap::new();
        for line in expected_text.lines() {
            if line.starts_with('#') {
                continue;
            }
            let f: Vec<&str> = line.split('\t').collect();
            assert_eq!(f.len(), 4, "{line}");
            want.insert(
                (
                    f[0].parse().unwrap(),
                    f[1].parse().unwrap(),
                    f[2].parse().unwrap(),
                ),
                f[3].to_owned(),
            );
        }
        assert_eq!(want.len(), 32_518, "the capture carries its full cell count");

        // `terrain_section` reads the process-global generator, so hold its lock (the
        // parallel rounds run this beside other world tests) and initialise seed 0.
        let _guard = lock_gen();
        assert!(init(0, crate::name_to_state), "generator for the captured seed");
        let (lo, hi) = (min_y(), max_y());
        let mut got: std::collections::HashMap<(i32, i32, i32), String> =
            std::collections::HashMap::new();
        let mut sy = lo;
        while sy <= hi {
            let arr = terrain_section(0, sy, 0, crate::feature::populate).unwrap();
            for ly in 0..16 {
                for lz in 0..16 {
                    for lx in 0..16 {
                        let st = arr[((ly << 8) | (lz << 4) | lx) as usize];
                        if st == 0 {
                            continue;
                        }
                        got.insert(
                            (lx as i32, sy + ly, lz as i32),
                            reg.canonical_state(st),
                        );
                    }
                }
            }
            sy += 16;
        }
        let mut diffs: Vec<((i32, i32, i32), Option<&String>, Option<&String>)> = Vec::new();
        for key in want.keys().chain(got.keys()) {
            if want.get(key) != got.get(key) && !diffs.iter().any(|d| d.0 == *key) {
                diffs.push((*key, want.get(key), got.get(key)));
            }
        }
        assert!(
            diffs.is_empty(),
            "{} cells differ from the native capture, first few: {:?}",
            diffs.len(),
            &diffs[..diffs.len().min(5)]
        );
    }

    /// The state-for-state gate for a chunk whose content is a *structure*, not terrain.
    ///
    /// Seed 4242 chunk (-96,35) carries a mineshaft corridor, a stronghold and a trial
    /// chamber. Four of its cells are the residual this fixes: the `oak_fence` pair at
    /// x = -1536, z = 565 keeps `east+north+west` (two `oak_log` pillars and a fence on
    /// its west side) and the pair at z = 562 keeps *no* connection bit, because native's
    /// `StructurePiece.placeBlock` marks those cells with `markPosForPostprocessing` and
    /// `LevelChunk.postProcessGeneration` re-derives them from the world as it stands when
    /// the chunk is promoted — which `ChunkMap.protoChunkToFullChunk` only allows once the
    /// whole 3x3 around it is `FULL`.
    ///
    /// It also pins the other half of that pass: the two `glow_lichen` cells at
    /// (-1521,-30,573..574) must be absent. The trial chamber template writes them with
    /// `east=true`, their support cell lies outside the placement box, and
    /// `MultifaceBlock.updateShape` clears the face and then the cell.
    #[test]
    fn structure_chunk_matches_native_capture_state_for_state() {
        use std::io::Read;
        let mut expected_text = String::new();
        flate2::read::GzDecoder::new(
            &include_bytes!("../tests/data/stronghold_chunk_-96_35_1211.tsv.gz")[..],
        )
        .read_to_string(&mut expected_text)
        .unwrap();
        let reg = crate::block_registry::breg().unwrap();
        let mut want: std::collections::HashMap<(i32, i32, i32), String> =
            std::collections::HashMap::new();
        for line in expected_text.lines() {
            if line.starts_with('#') {
                continue;
            }
            let f: Vec<&str> = line.split('\t').collect();
            assert_eq!(f.len(), 4, "{line}");
            want.insert(
                (
                    f[0].parse().unwrap(),
                    f[1].parse().unwrap(),
                    f[2].parse().unwrap(),
                ),
                f[3].to_owned(),
            );
        }
        assert_eq!(want.len(), 31_721, "the capture carries its full cell count");

        let _guard = lock_gen();
        // The generator reaches the structure pipeline's beardifier through a
        // registered function pointer (`worldgen` sits below `feature`), and the
        // process entry point registers it before any dump. Without it the terrain is
        // unbearded, which moves the trial chamber's `encapsulate` shell and every block
        // the surface then sees.
        crate::feature::register_beard_resolver();
        assert!(init(4242, crate::name_to_state), "generator for the captured seed");
        let (lo, hi) = (min_y(), max_y());
        let mut got: std::collections::HashMap<(i32, i32, i32), String> =
            std::collections::HashMap::new();
        let mut sy = lo;
        while sy <= hi {
            let arr = terrain_section(-96, sy, 35, crate::feature::populate).unwrap();
            for ly in 0..16 {
                for lz in 0..16 {
                    for lx in 0..16 {
                        let st = arr[((ly << 8) | (lz << 4) | lx) as usize];
                        if st == 0 {
                            continue;
                        }
                        got.insert(
                            (-96 * 16 + lx as i32, sy + ly, 35 * 16 + lz as i32),
                            reg.canonical_state(st),
                        );
                    }
                }
            }
            sy += 16;
        }
        let mut diffs: Vec<((i32, i32, i32), Option<&String>, Option<&String>)> = Vec::new();
        for key in want.keys().chain(got.keys()) {
            if want.get(key) != got.get(key) && !diffs.iter().any(|d| d.0 == *key) {
                diffs.push((*key, want.get(key), got.get(key)));
            }
        }
        assert!(
            diffs.is_empty(),
            "{} cells differ from the native capture, first few: {:?}",
            diffs.len(),
            &diffs[..diffs.len().min(5)]
        );
    }

    #[test]
    fn nether_cave_surfaces_and_lava_match_native_negative_seed() {
        let mut g = Gen::load_preset(u64::MAX, crate::name_to_state, NoisePreset::Nether).unwrap();
        assert_eq!(g.def_fl, crate::name_to_state("minecraft:lava").unwrap());
        assert!(g.aq.is_none());
        for (y, name) in [(27, "minecraft:soul_sand"), (30, "minecraft:lava")] {
            assert_eq!(
                g.terrain_state(-995, y, 512),
                crate::name_to_state(name).unwrap(),
                "native cell -995,{y},512"
            );
        }
    }

    #[test]
    fn nether_surface_depth_and_named_random_match_native() {
        let mut current = None;
        let mut generator = None;
        for row in include_str!("../tests/data/nether_surface_1211.tsv").lines() {
            let p: Vec<_> = row.split('\t').collect();
            let seed = parse_world_seed(p[0]).unwrap();
            if current != Some(seed) {
                generator = Some(
                    Gen::load_preset(seed, crate::name_to_state, NoisePreset::Nether).unwrap(),
                );
                current = Some(seed);
            }
            let x = p[1].parse().unwrap();
            let z = p[2].parse().unwrap();
            assert_eq!(
                generator.as_ref().unwrap().run_depth(x, z),
                p[3].parse::<i32>().unwrap(),
                "{row}"
            );
            let factory = legacy_named_factory_seed(seed, p[4]);
            let y = p[5].parse().unwrap();
            let value = Lcg::new(factory ^ hash_block_pos(x, y, z) as u64).next_f32();
            assert_eq!(
                value.to_bits(),
                u32::from_str_radix(p[6], 16).unwrap(),
                "{row}"
            );
        }
    }

    #[test]
    fn nether_biome_source_matches_native_nether_random_state() {
        let mut current = None;
        let mut generator = None;
        let mut names = std::collections::BTreeSet::new();
        for row in include_str!("../tests/data/nether_biome_1211.tsv").lines() {
            let p: Vec<_> = row.split('\t').collect();
            let seed = parse_world_seed(p[0]).unwrap();
            if current != Some(seed) {
                generator = Some(
                    Gen::load_preset(seed, crate::name_to_state, NoisePreset::Nether)
                        .expect("Nether datapack"),
                );
                current = Some(seed);
            }
            let g = generator.as_mut().unwrap();
            let (_, leaf) = g
                .biome_quart(
                    p[1].parse().unwrap(),
                    p[2].parse().unwrap(),
                    p[3].parse().unwrap(),
                    None,
                )
                .expect("Nether biome");
            let name = &g.biome_names[usize::from(leaf)];
            assert_eq!(name, p[4], "{row}");
            names.insert(name.clone());
        }
        assert_eq!(names.len(), 5);
    }

    #[test]
    fn signed_world_seed_preserves_java_long_bits() {
        assert_eq!(parse_world_seed("-1").unwrap(), u64::MAX);
        assert_eq!(parse_world_seed("-9223372036854775808").unwrap(), 1 << 63);
        assert_eq!(parse_world_seed("18446744073709551615").unwrap(), u64::MAX);
        assert!(parse_world_seed("unknown").is_err());
    }

    #[test]
    fn end_biome_source_matches_native_end_random_state() {
        let mut current = None;
        let mut generator = None;
        let mut names = std::collections::BTreeSet::new();
        let mut count = 0;
        for row in include_str!("../tests/data/end_biome_1211.tsv").lines() {
            let f: Vec<_> = row.split('\t').collect();
            assert_eq!(f.len(), 5);
            let seed = f[0].parse::<i64>().unwrap() as u64;
            if current != Some(seed) {
                generator = Some(
                    Gen::load_preset(seed, crate::name_to_state, NoisePreset::End)
                        .expect("End datapack"),
                );
                current = Some(seed);
            }
            let g = generator.as_mut().unwrap();
            let (_, leaf) = g
                .biome_quart(
                    f[1].parse().unwrap(),
                    f[2].parse().unwrap(),
                    f[3].parse().unwrap(),
                    None,
                )
                .unwrap();
            assert_eq!(
                g.biome_name_of_leaf(leaf).as_deref(),
                Some(f[4]),
                "native End biome: {row}"
            );
            names.insert(f[4]);
            count += 1;
        }
        assert_eq!(count, 2352);
        assert_eq!(
            names,
            std::collections::BTreeSet::from([
                "minecraft:the_end",
                "minecraft:end_highlands",
                "minecraft:end_midlands",
                "minecraft:small_end_islands",
                "minecraft:end_barrens"
            ])
        );
    }

    #[test]
    fn end_island_density_node_uses_the_generators_seed() {
        for seed in [0u64, 42] {
            let mut g = Gen::load(seed, crate::name_to_state).expect("pinned datapack");
            let config = jparse(br#"{"type":"minecraft:end_islands"}"#).unwrap();
            let index = g.node(&config, &HashMap::new()).expect("End density node");
            let mut checked = 0;
            for row in include_str!("../tests/data/end_islands_1211.tsv").lines() {
                let f: Vec<_> = row.split('\t').collect();
                if f[0].parse::<i64>().unwrap() as u64 != seed {
                    continue;
                }
                let (x, z) = (f[1].parse::<i32>().unwrap(), f[2].parse::<i32>().unwrap());
                let actual = g.eval(index, x, 64, z, &mut HashMap::new());
                assert_eq!(
                    actual.to_bits(),
                    u64::from_str_radix(f[4], 16).unwrap(),
                    "production End density: {row}"
                );
                checked += 1;
            }
            assert_eq!(checked, 80);
        }
    }

    #[test]
    fn unsupported_density_types_fail_closed_at_every_reference_boundary() {
        let mut g = Gen::load(0, crate::name_to_state).expect("pinned datapack");
        let empty = HashMap::new();

        // Unknown nodes must never masquerade as a legitimate zero function.
        let unknown = jparse(br#"{"type":"minecraft:unsupported_density"}"#).unwrap();
        assert!(g.node(&unknown, &empty).is_none());

        // A required child inside a valid operator must propagate the failure.
        let nested = jparse(
            br#"{"type":"minecraft:add","argument1":1,"argument2":{"type":"minecraft:unsupported_density"}}"#,
        )
        .unwrap();
        assert!(g.node(&nested, &empty).is_none());

        // A failing named definition must not leave the recursion placeholder
        // cached as a valid reference on subsequent loads.
        let mut files = HashMap::new();
        files.insert(
            "minecraft:broken".to_string(),
            br#"{"type":"minecraft:unsupported_density"}"#.to_vec(),
        );
        for _ in 0..2 {
            assert!(g.resolve_ref(&files, "minecraft:broken").is_none());
            assert!(!g.names.contains_key("minecraft:broken"));
        }

        // An intentional numeric zero remains valid and must still evaluate.
        let valid = jparse(br#"{"type":"minecraft:add","argument1":2,"argument2":0}"#).unwrap();
        let id = g.node(&valid, &empty).expect("known density operators");
        assert_eq!(g.eval(id, 0, 0, 0, &mut HashMap::new()), 2.0);
    }

    #[test]
    fn shifted_noise_preserves_y_and_rejects_invalid_required_shifts() {
        let mut g = Gen::load(0, crate::name_to_state).expect("pinned datapack");
        let empty = HashMap::new();
        let good = jparse(
            br#"{"type":"minecraft:shifted_noise","noise":"minecraft:temperature","shift_x":0,"shift_y":3,"shift_z":0,"xz_scale":1,"y_scale":1}"#,
        )
        .unwrap();
        let id = g.node(&good, &empty).expect("valid shifted noise");
        let DF::Noise { sy, .. } = &g.dfs[id] else {
            panic!("shifted_noise did not use the noise evaluator");
        };
        match &g.dfs[*sy] {
            DF::C(y) => assert_eq!(*y, 3.0, "nonzero shift_y must not be dropped"),
            _ => panic!("expected literal y shift"),
        }

        // The native codec requires all three shifts. Missing or unparseable
        // children must not be silently replaced by a zero density.
        for invalid in [
            r#"{"type":"minecraft:shifted_noise","noise":"minecraft:temperature","shift_x":0,"shift_z":0,"xz_scale":1,"y_scale":1}"#,
            r#"{"type":"minecraft:shifted_noise","noise":"minecraft:temperature","shift_x":0,"shift_y":{"type":"minecraft:unsupported_density"},"shift_z":0,"xz_scale":1,"y_scale":1}"#,
            r#"{"type":"minecraft:noise","noise":"minecraft:temperature","shift_x":{"type":"minecraft:unsupported_density"},"xz_scale":1,"y_scale":1}"#,
        ] {
            assert!(g.node(&jparse(invalid.as_bytes()).unwrap(), &empty).is_none(), "{invalid}");
        }
    }

    #[test]
    fn the_surface_material_window_stops_at_deep_solids() {
        assert!(surface_material_applies(62, 62));
        assert!(surface_material_applies(59, 62));
        assert!(surface_material_applies(58, 62));
        assert!(!surface_material_applies(57, 62));
        assert!(!surface_material_applies(33, 62));
    }

    #[test]
    fn a_nonpositive_density_with_an_aquifer_barrier_is_still_solid() {
        assert_eq!(
            classify_surface_cell(-0.01, None, false, false),
            SurfaceCell::Solid
        );
        assert_eq!(
            classify_surface_cell(-0.01, Some(1), false, false),
            SurfaceCell::Solid
        );
        assert_eq!(
            classify_surface_cell(-0.01, Some(0), true, false),
            SurfaceCell::Air
        );
        assert_eq!(
            classify_surface_cell(-0.01, Some(2), false, true),
            SurfaceCell::Fluid
        );
        assert_eq!(
            classify_surface_cell(0.01, Some(0), true, false),
            SurfaceCell::Solid
        );
    }

    // These tests mutate the shared `GEN` static, so they must not run in parallel. The
    // lock is the module-level one, also used by tests in other modules (the delivery-path
    // bench) and visible to them: one lock for every test that touches the generator.
    pub(super) fn lock_gen() -> std::sync::MutexGuard<'static, ()> {
        super::GEN_TEST_LOCK.lock().unwrap_or_else(|poison| {
            // This mutex only serializes tests; it protects no recoverable data.
            super::GEN_TEST_LOCK.clear_poison();
            poison.into_inner()
        })
    }
    #[test]
    fn poisoned_test_lock_does_not_cascade() {
        let failed = std::panic::catch_unwind(|| {
            let _guard = lock_gen();
            panic!("intentional test failure");
        });
        assert!(failed.is_err());
        let _guard = lock_gen();
        assert!(!super::GEN_TEST_LOCK.is_poisoned());
    }
    #[test]
    fn pinned_biome_tree_uses_only_biomes_from_its_jar() {
        let g = Gen::load(0, crate::name_to_state).expect("pinned pack must load");
        let biomes = Gen::dp_files("biome");
        assert!(!g.biome_names.is_empty(), "empty biome tree");
        let unknown: Vec<_> = g
            .biome_names
            .iter()
            .filter(|name| !biomes.contains_key(*name))
            .collect();
        assert!(
            unknown.is_empty(),
            "biome tree contains foreign biomes: {unknown:?}"
        );
    }

    /// The chunk biome set must cover the whole build height, not y >= 0.
    ///
    /// `biome_quart` takes the *block* y divided by four, so a sweep of `0..height/4` reads
    /// y = 0..383 and drops every biome confined below y = 0. Measured consequence: a deep-dark
    /// chunk had no `deep_dark` in its set, so `sculk_patch_deep_dark` and `sculk_vein` never
    /// ran there (0 of the 397 sculk cells vanilla places), while `pointed_dripstone` ran
    /// because `dripstone_caves` also occurs above y = 0.
    #[test]
    fn chunk_biome_set_covers_the_negative_y_range() {
        let mut g = Gen::load(0, crate::name_to_state).expect("pinned pack must load");
        let (cx, cz) = (-16, 112);
        let set = g.chunk_biome_set(cx, cz);
        assert!(
            set.iter().any(|b| b == "minecraft:deep_dark"),
            "deep-dark chunk lost its below-zero biome: {set:?}"
        );
        // and the set must be the union over *every* quart layer of the chunk
        let mut union = std::collections::HashSet::new();
        for qy in g.min_y / 4..(g.min_y + g.height) / 4 {
            for lx in 0..4 {
                for lz in 0..4 {
                    if let Some((_, leaf)) = g.biome_quart(cx * 4 + lx, qy, cz * 4 + lz, None) {
                        if let Some(name) = g.biome_name_of_leaf(leaf) {
                            union.insert(name);
                        }
                    }
                }
            }
        }
        let from_set: std::collections::HashSet<String> = set.iter().cloned().collect();
        assert_eq!(
            from_set, union,
            "chunk biome set is not the union of its own quart layers"
        );
    }

    /// Actual `SurfaceSystem.buildSurface` scan on synthetic stone/water columns.
    /// The Java oracle fills a ProtoChunk, runs the real buildSurface with a
    /// stone-depth rule, and records the gold cells. The test feeds the same
    /// column states through the production `scan_surface_column` path.
    #[test]
    fn surface_build_scan_matches_the_pinned_classes_on_synthetic_columns() {
        let fixture = jparse(include_bytes!(
            "../evidence/foundation/surface-context/buildsurface.json"
        ))
        .unwrap();
        let rows = fixture.get("columns").unwrap().as_arr().unwrap();
        let gold = crate::name_to_state("minecraft:gold_block").expect("gold block");
        let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
        let mut seen = 0usize;
        for row in rows {
            let offset = row.get("offset").unwrap().num() as i32;
            let x = row.get("x").unwrap().num() as i32;
            let z = row.get("z").unwrap().num() as i32;
            let top = row.get("top").unwrap().num() as i32;
            let sea = row.get("sea").unwrap().num() as i32;
            g.conds.push(Cond::StoneD {
                off: offset,
                sdep: false,
                sec: 0,
                ceil: false,
            });
            let cid = g.conds.len() - 1;
            g.rules.push(Rule::Block(gold));
            let block = g.rules.len() - 1;
            g.rules.push(Rule::Cond(cid, block));
            let rid = g.rules.len() - 1;
            let scan_top = top.max(sea);
            let cells: Vec<SurfaceCell> = (g.min_y..=scan_top)
                .map(|y| {
                    if y <= top {
                        SurfaceCell::Solid
                    } else if y <= sea {
                        SurfaceCell::Fluid
                    } else {
                        SurfaceCell::Air
                    }
                })
                .collect();
            let mut c = SC::new();
            c.x = x;
            c.z = z;
            c.run_depth = 0;
            c.sec_ok = true;
            c.est_min = g.min_y;
            let want: Vec<i32> = row
                .get("gold")
                .unwrap()
                .as_arr()
                .unwrap()
                .iter()
                .map(|v| v.num() as i32)
                .collect();
            let mat = g
                .scan_surface_column(rid, &mut c, g.min_y, &cells, &[0i16; 256])
                .expect("surface column");
            let got: Vec<i32> = mat
                .vals
                .iter()
                .enumerate()
                .filter(|(_, v)| **v != u16::MAX)
                .map(|(i, _)| mat.lo as i32 + i as i32)
                .collect();
            assert_eq!(got, want, "scan offset {offset} column {x},{z} top {top}");
            let _ = cid;
            seen += 1;
        }
        assert_eq!(seen, 768, "buildSurface synthetic coverage changed");
    }

    /// Actual `SurfaceRules` condition formulas on synthetic context fields:
    /// stone depth above/below, surface depth, water height, surface secondary,
    /// hole and the preliminary-surface comparison. The fixture is generated by
    /// `tools/surface_condition_oracle.py` from the pinned classes.
    #[test]
    fn surface_conditions_match_the_pinned_context_formulas() {
        let fixture = jparse(include_bytes!(
            "../evidence/foundation/surface-context/conditions.json"
        ))
        .unwrap();
        let rows = fixture.get("rows").unwrap().as_arr().unwrap();
        let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
        let mut seen = 0usize;
        for row in rows {
            let kind = row.get("kind").unwrap().str();
            let mut c = SC::new();
            c.x = 5;
            c.z = 7;
            c.y = row.get("y").map(J::num).unwrap_or(0.0) as i32;
            c.run_depth = row.get("surface_depth").map(J::num).unwrap_or(0.0) as i32;
            c.sda = row.get("stone_above").map(J::num).unwrap_or(0.0) as i32;
            c.sdb = row.get("stone_below").map(J::num).unwrap_or(0.0) as i32;
            c.fluid = row
                .get("water_height")
                .map(J::num)
                .unwrap_or(i32::MIN as f64) as i32;
            c.est_min = row.get("min_surface").map(J::num).unwrap_or(0.0) as i32;
            c.sec = row.get("surface_secondary").map(J::num).unwrap_or(0.0) as f32;
            c.sec_ok = true;
            let cond = match kind {
                "stone" => Cond::StoneD {
                    off: row.get("offset").unwrap().num() as i32,
                    sdep: row.get("add").unwrap().num() != 0.0,
                    sec: row.get("secondary").unwrap().num() as i32,
                    ceil: row.get("ceiling").unwrap().num() != 0.0,
                },
                "water" => Cond::Water {
                    off: row.get("offset").unwrap().num() as i32,
                    mult: row.get("mult").unwrap().num() as i32,
                    sd: row.get("start").unwrap().num() != 0.0,
                },
                "hole" => Cond::Hole,
                "prelim" => Cond::Prelim,
                other => panic!("unexpected condition kind {other}"),
            };
            g.conds.push(cond);
            let cid = g.conds.len() - 1;
            let want = row.get("expected").unwrap().num() != 0.0;
            assert_eq!(
                g.test_cond(cid, &mut c, &[0i16; 256]),
                want,
                "{} {}",
                row.get("id").unwrap().str(),
                kind
            );
            seen += 1;
        }
        assert_eq!(seen, 4037, "surface condition coverage changed");
    }

    /// `Context$SteepMaterialCondition` compares WORLD_SURFACE_WG heights at
    /// z+1/z-1 and x-1/x+1 with a four-block threshold. The scan must pass the
    /// surface/fluid heightmap, not the stone-only density top.
    #[test]
    fn surface_steep_reads_the_world_surface_heightmap() {
        let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
        g.conds.push(Cond::Steep);
        let cid = g.conds.len() - 1;
        let mut c = SC::new();
        c.x = 5;
        c.z = 5;
        let mut hm = [64i16; 256];
        hm[6 * 16 + 5] = 68;
        assert!(g.test_cond(cid, &mut c, &hm), "z+1 slope");
        let mut c = SC::new();
        c.x = 5;
        c.z = 5;
        let mut hm = [64i16; 256];
        hm[6 * 16 + 5] = 67;
        assert!(
            !g.test_cond(cid, &mut c, &hm),
            "three-block slope is not steep"
        );
        let mut c = SC::new();
        c.x = 5;
        c.z = 5;
        let mut hm = [64i16; 256];
        hm[5 * 16 + 4] = 68;
        assert!(g.test_cond(cid, &mut c, &hm), "x-1 slope");
        let mut c = SC::new();
        c.x = 5;
        c.z = 5;
        assert!(!g.test_cond(cid, &mut c, &[64i16; 256]), "flat");
    }

    #[test]
    fn pinned_surface_rule_places_bedrock_and_deepslate() {
        let mut g = Gen::load(0, crate::name_to_state).expect("pinned pack must load");
        let root = g.surface_rule.expect("inline surface_rule did not resolve");
        let surface = [64; 256];
        for (y, expected) in [(-64, "minecraft:bedrock"), (-8, "minecraft:deepslate")] {
            let mut c = SC::new();
            c.y = y;
            c.est_min = i32::MAX;
            c.sda = 32000;
            c.sdb = 32000;
            assert_eq!(
                g.eval_rule(root, &mut c, &surface),
                crate::name_to_state(expected),
                "pinned surface rule at y={y}"
            );
        }
    }

    #[test]
    fn pinned_surface_block_states_match_the_native_codec() {
        let fixture = jparse(include_bytes!(
            "../evidence/foundation/surface-loader/vanilla.json"
        ))
        .expect("surface reference JSON");
        assert_eq!(fixture.get("minecraft").unwrap().str(), "1.21.1");
        assert_eq!(
            fixture.get("server_sha1").unwrap().str(),
            "59353fb40c36d304f2035d51e7d6e6baa98dc05c"
        );
        let mut g = Gen::load(0, crate::name_to_state).expect("pinned pack");
        let empty = HashMap::new();
        let mut checked = 0;
        let mut ids = HashSet::new();
        for row in fixture.get("cases").unwrap().as_arr().unwrap() {
            assert!(ids.insert(row.get("id").unwrap().str()));
            if row.get("op").unwrap().str() != "state" {
                continue;
            }
            let state = row.get("state").unwrap();
            let rule = J::Obj(vec![
                ("type".into(), J::Str("minecraft:block".into())),
                ("result_state".into(), state.clone()),
            ]);
            let Some(Rule::Block(actual)) =
                g.parse_rule(&rule, &empty, &empty, &empty, crate::name_to_state)
            else {
                panic!(
                    "native state did not decode: {}",
                    row.get("id").unwrap().str()
                );
            };
            let expected = row.get("expected").unwrap().get("state_id").unwrap().num() as u32;
            assert_eq!(actual, expected, "{}", row.get("id").unwrap().str());
            // The retained converted dialect must still use the same registry.
            let (name, props) = bs_parts(state).unwrap();
            let mut converted = vec![("id".into(), J::Str(name))];
            if let Some(props) = props {
                converted.push(("properties".into(), props.clone()));
            }
            assert_eq!(
                Gen::block_state(&J::Obj(converted), crate::name_to_state),
                Some(expected)
            );
            checked += 1;
        }
        assert!(checked > 0, "no native state cases compared");
    }

    #[test]
    fn pinned_surface_temperature_matches_the_native_biome() {
        let fixture = jparse(include_bytes!(
            "../evidence/foundation/surface-loader/vanilla.json"
        ))
        .expect("surface reference JSON");
        let mut g = Gen::load(0, crate::name_to_state).expect("pinned pack");
        let empty = HashMap::new();
        let condition = jparse(br#"{"type":"minecraft:temperature"}"#).unwrap();
        let cid = g
            .cond_node(&condition, &empty)
            .expect("temperature condition");
        let mut checked = 0;
        let mut cold = 0;
        let mut nonzero_noise = 0;
        for row in fixture.get("cases").unwrap().as_arr().unwrap() {
            if row.get("op").unwrap().str() == "simplex" {
                let (seed, octaves): (u64, &[i32]) = match row.get("source").unwrap().str() {
                    "height" => (1234, &[0]),
                    "frozen" => (3456, &[-2, -1, 0]),
                    "info" => (2345, &[0]),
                    other => panic!("unknown native Biome noise field {other}"),
                };
                let pos = row.get("pos").unwrap().as_arr().unwrap();
                let noise = OSimp::new_l(&mut Lcg::new(seed), octaves);
                let actual = noise.sample(pos[0].num(), pos[1].num());
                let bits = row
                    .get("expected")
                    .unwrap()
                    .get("value_bits")
                    .unwrap()
                    .str();
                assert_eq!(
                    actual.to_bits(),
                    u64::from_str_radix(bits, 16).unwrap(),
                    "native Biome noise: {}",
                    row.get("id").unwrap().str()
                );
                nonzero_noise += usize::from(actual != 0.0);
                continue;
            }
            if row.get("op").unwrap().str() != "temperature" {
                continue;
            }
            let p = row.get("pos").unwrap().as_arr().unwrap();
            let (x, y, z) = (p[0].num() as i32, p[1].num() as i32, p[2].num() as i32);
            let weather = BiomeWeather {
                temperature: row.get("base_temperature").unwrap().num() as f32,
                frozen: jbool(row, "frozen", false),
            };
            let expected = row.get("expected").unwrap();
            let id = row.get("id").unwrap().str();
            assert_eq!(
                weather.at(x, y, z).to_bits(),
                expected.get("temperature_bits").unwrap().num() as i32 as u32,
                "{id}"
            );
            let is_cold = jbool(expected, "cold", false);
            // Exercise the actual condition dispatch, with an explicit biome
            // leaf. This does not claim that the world biome lookup is correct.
            g.biome_weather[0] = weather;
            g.bioq.insert((x >> 2, y >> 2, z >> 2), 0);
            let mut c = SC::new();
            (c.x, c.y, c.z) = (x, y, z);
            assert_eq!(g.test_cond(cid, &mut c, &[64; 256]), is_cold, "{id}");
            cold += usize::from(is_cold);
            checked += 1;
        }
        assert!(cold > 0 && cold < checked, "no cold/warm contrast");
        assert!(nonzero_noise > 0, "no nonzero native noise values compared");
    }

    #[test]
    fn surface_sequence_rejects_a_child_that_did_not_parse() {
        let mut g = Gen::load(0, crate::name_to_state).expect("pinned pack");
        let empty = HashMap::new();
        for child in [
            r#"{"type":"minecraft:block"}"#,
            r#"{"type":"minecraft:unknown_rule"}"#,
            r#""minecraft:missing_rule""#,
            r#"{"type":"minecraft:condition","if_true":{"type":"minecraft:unknown_condition"},"then_run":{"type":"minecraft:block","result_state":"minecraft:stone"}}"#,
        ] {
            let rule = jparse(format!(
                r#"{{"type":"minecraft:sequence","sequence":[{{"type":"minecraft:block","result_state":"minecraft:dirt"}},{child}]}}"#
            ).as_bytes()).unwrap();
            assert!(
                g.rule_node(&rule, &empty, &empty, &empty, crate::name_to_state)
                    .is_none(),
                "sequence silently dropped {child}"
            );
        }
        let bad_name = "minecraft:bad_named_rule";
        let named = HashMap::from([(
            bad_name.to_string(),
            br#"{"type":"minecraft:block"}"#.to_vec(),
        )]);
        for _ in 0..2 {
            assert!(
                g.resolve_rule(bad_name, &empty, &named, &empty, crate::name_to_state)
                    .is_none(),
                "failed named rule was cached as a valid placeholder"
            );
        }
    }

    #[test]
    fn pinned_climate_router_resolves_inline_fields() {
        let mut g = Gen::load(0, crate::name_to_state).expect("pinned pack must load");
        // The tree existed, but temperature/vegetation are inline objects in 1.21.1.
        // Treating them as reference strings silently disabled every biome query.
        assert!(g.climate.is_some(), "climate router did not resolve");
        for (qx, qy, qz) in [(0, 16, 0), (832, 16, -448), (-256, 16, 128)] {
            let (_, leaf) = g.biome_quart(qx, qy, qz, None).expect("biome query");
            assert!(!g.biome_names[leaf as usize].is_empty());
        }
    }

    #[test]
    fn biome_vectors() {
        let _guard = lock_gen();
        // pumpkin multi_noise_sampler tests: seed, quart pos, params
        fn stub(_: &str) -> Option<u32> {
            Some(1)
        }
        assert!(init(123, stub));
        let (b, t) = biome_debug(123, 123, 123).unwrap();
        assert_eq!(t, [-5727, 55, 4996, 2371, -19774, 4421, 0]);
        assert_eq!(b, 47); // snowy_taiga
        assert!(init(13579, stub));
        let (b, t) = biome_debug(123, 123, 123).unwrap();
        assert_eq!(t, [7489, 3502, -2168, -3511, -21237, -5222, 0]);
        assert_eq!(b, 57); // warm_ocean
    }

    /// The same lookup as the game's on the tie-heavy probe set
    /// `tests/data/biome_probe_ties_1211.tsv` (points on leaf range edges, almost all
    /// with two different biomes at the same minimum distance). `SearchTree` scans a
    /// branch's children in array order and replaces its best only on a strictly
    /// smaller distance, so equally near leaves resolve to the first one reached.
    #[test]
    fn biome_tree_breaks_ties_the_way_vanilla_scans_children() {
        let _guard = lock_gen();
        fn stub(_: &str) -> Option<u32> {
            Some(1)
        }
        assert!(init(0, stub));
        let mut gg = gen_lock();
        let g = gg.as_mut().unwrap();
        let tree = g.btree.as_ref().expect("biome tree must load");
        let (mut n, mut tied, mut wrong) = (0usize, 0usize, Vec::new());
        for file in ["tests/data/biome_probe_ties_1211.tsv"] {
            let probes = std::fs::read_to_string(file).expect("probe points must be readable");
            for line in probes.lines() {
                let f: Vec<&str> = line.split('\t').collect();
                assert_eq!(f.len(), 8, "bad probe line {line:?}");
                if !file.contains("ties") && f[7] != "1" {
                    continue; // unambiguous points are covered by the test above
                }
                let mut t = [0i64; 7];
                for i in 0..6 {
                    t[i] = f[i].parse().unwrap();
                }
                let mut best = (i64::MAX, 0u16);
                tree.search(&t, &mut best);
                let got = &g.biome_names[best.1 as usize];
                n += 1;
                tied += (f[7] == "1") as usize;
                if got != f[6] {
                    wrong.push(format!("{t:?}: game {} ours {got}", f[6]));
                }
            }
        }
        eprintln!(
            "tie-break points: {n} ({tied} with different biomes equally near), differing: {}",
            wrong.len()
        );
        assert!(tied >= 2000, "the tie set must be large, got {tied}");
        assert!(
            wrong.is_empty(),
            "{} of {n} points differ from vanilla, first: {:?}",
            wrong.len(),
            &wrong[..wrong.len().min(5)]
        );
    }

    #[test]
    fn material_rules_load() {
        let _guard = lock_gen();
        fn stub(_: &str) -> Option<u32> {
            Some(1)
        }
        assert!(init(0, stub));
        let mut gg = gen_lock();
        let g = gg.as_mut().unwrap();
        println!(
            "surface_rule={:?} conds={} rules={} mats_loaded",
            g.surface_rule,
            g.conds.len(),
            g.rules.len()
        );
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
    // Pumpkin-derived internal-method vectors, retained as regression pins. They
    // include coordinates a vanilla chunk-scoped aquifer never queries and a donor
    // GlobalRandomConfig; native behaviour is pinned separately by
    // `aquifer_matches_the_pinned_chunk_through_its_public_api`.
    const AQ_NB: &[((i32, i32, i32), i32)] = &[
        ((-100, -100, -100), -103),
        ((-100, -100, -50), -103),
        ((-100, -100, 0), -103),
        ((-100, -100, 50), -103),
        ((-100, -100, 100), -103),
        ((-100, -50, -100), -63),
        ((-100, -50, -50), -63),
        ((-100, -50, 0), -63),
        ((-100, -50, 50), -63),
        ((-100, -50, 100), -63),
        ((-100, 0, -100), 17),
        ((-100, 0, -50), 17),
        ((-100, 0, 0), 17),
        ((-100, 0, 50), 17),
        ((-100, 0, 100), 17),
        ((-100, 50, -100), 57),
        ((-100, 50, -50), 57),
        ((-100, 50, 0), 57),
        ((-100, 50, 50), 57),
        ((-100, 50, 100), 57),
        ((-100, 100, -100), 97),
        ((-100, 100, -50), 97),
        ((-100, 100, 0), 97),
        ((-100, 100, 50), 97),
        ((-100, 100, 100), 97),
        ((-50, -100, -100), -103),
        ((-50, -100, -50), -103),
        ((-50, -100, 0), -103),
        ((-50, -100, 50), -103),
        ((-50, -100, 100), -100),
        ((-50, -50, -100), -63),
        ((-50, -50, -50), -63),
        ((-50, -50, 0), -63),
        ((-50, -50, 50), -63),
        ((-50, -50, 100), -60),
        ((-50, 0, -100), 17),
        ((-50, 0, -50), 17),
        ((-50, 0, 0), 17),
        ((-50, 0, 50), 17),
        ((-50, 0, 100), 20),
        ((-50, 50, -100), 57),
        ((-50, 50, -50), 57),
        ((-50, 50, 0), 57),
        ((-50, 50, 50), 57),
        ((-50, 50, 100), 60),
        ((-50, 100, -100), 97),
        ((-50, 100, -50), 97),
        ((-50, 100, 0), 97),
        ((-50, 100, 50), 97),
        ((-50, 100, 100), 100),
        ((0, -100, -100), -103),
        ((0, -100, -50), -103),
        ((0, -100, 0), -103),
        ((0, -100, 50), -100),
        ((0, -100, 100), -100),
        ((0, -50, -100), -63),
        ((0, -50, -50), -63),
        ((0, -50, 0), -63),
        ((0, -50, 50), -60),
        ((0, -50, 100), -60),
        ((0, 0, -100), 17),
        ((0, 0, -50), 17),
        ((0, 0, 0), 17),
        ((0, 0, 50), 20),
        ((0, 0, 100), 20),
        ((0, 50, -100), 57),
        ((0, 50, -50), 57),
        ((0, 50, 0), 57),
        ((0, 50, 50), 60),
        ((0, 50, 100), 60),
        ((0, 100, -100), 97),
        ((0, 100, -50), 97),
        ((0, 100, 0), 97),
        ((0, 100, 50), 100),
        ((0, 100, 100), 100),
        ((50, -100, -100), -103),
        ((50, -100, -50), -103),
        ((50, -100, 0), -103),
        ((50, -100, 50), -100),
        ((50, -100, 100), -100),
        ((50, -50, -100), -63),
        ((50, -50, -50), -63),
        ((50, -50, 0), -63),
        ((50, -50, 50), -60),
        ((50, -50, 100), -60),
        ((50, 0, -100), 17),
        ((50, 0, -50), 17),
        ((50, 0, 0), 17),
        ((50, 0, 50), 20),
        ((50, 0, 100), 20),
        ((50, 50, -100), 57),
        ((50, 50, -50), 57),
        ((50, 50, 0), 57),
        ((50, 50, 50), 60),
        ((50, 50, 100), 60),
        ((50, 100, -100), 97),
        ((50, 100, -50), 97),
        ((50, 100, 0), 97),
        ((50, 100, 50), 100),
        ((50, 100, 100), 100),
        ((100, -100, -100), -103),
        ((100, -100, -50), -103),
        ((100, -100, 0), -103),
        ((100, -100, 50), -103),
        ((100, -100, 100), -100),
        ((100, -50, -100), -63),
        ((100, -50, -50), -63),
        ((100, -50, 0), -63),
        ((100, -50, 50), -63),
        ((100, -50, 100), -60),
        ((100, 0, -100), 17),
        ((100, 0, -50), 17),
        ((100, 0, 0), 17),
        ((100, 0, 50), 17),
        ((100, 0, 100), 20),
        ((100, 50, -100), 57),
        ((100, 50, -50), 57),
        ((100, 50, 0), 57),
        ((100, 50, 50), 57),
        ((100, 50, 100), 60),
        ((100, 100, -100), 97),
        ((100, 100, -50), 97),
        ((100, 100, 0), 97),
        ((100, 100, 50), 97),
        ((100, 100, 100), 100),
    ];
    const AQ_FYT: &[((i32, i32, i32), i32)] = &[
        ((-100, -100, -100), -32512),
        ((-100, -100, -50), -32512),
        ((-100, -100, 0), -32512),
        ((-100, -100, 50), -32512),
        ((-100, -100, 100), -32512),
        ((-100, -50, -100), -32512),
        ((-100, -50, -50), -63),
        ((-100, -50, 0), -63),
        ((-100, -50, 50), -63),
        ((-100, -50, 100), -32512),
        ((-100, 0, -100), -32512),
        ((-100, 0, -50), -32512),
        ((-100, 0, 0), -32512),
        ((-100, 0, 50), -32512),
        ((-100, 0, 100), -32512),
        ((-100, 50, -100), 57),
        ((-100, 50, -50), 57),
        ((-100, 50, 0), 57),
        ((-100, 50, 50), 57),
        ((-100, 50, 100), 57),
        ((-100, 100, -100), 80),
        ((-100, 100, -50), 0),
        ((-100, 100, 0), 0),
        ((-100, 100, 50), 0),
        ((-100, 100, 100), 0),
        ((-50, -100, -100), -32512),
        ((-50, -100, -50), -32512),
        ((-50, -100, 0), -32512),
        ((-50, -100, 50), -32512),
        ((-50, -100, 100), -32512),
        ((-50, -50, -100), -32512),
        ((-50, -50, -50), -32512),
        ((-50, -50, 0), -32512),
        ((-50, -50, 50), -32512),
        ((-50, -50, 100), -32512),
        ((-50, 0, -100), -32512),
        ((-50, 0, -50), -32512),
        ((-50, 0, 0), -32512),
        ((-50, 0, 50), -32512),
        ((-50, 0, 100), -32512),
        ((-50, 50, -100), -32512),
        ((-50, 50, -50), -32512),
        ((-50, 50, 0), 57),
        ((-50, 50, 50), 57),
        ((-50, 50, 100), 60),
        ((-50, 100, -100), 80),
        ((-50, 100, -50), 0),
        ((-50, 100, 0), 0),
        ((-50, 100, 50), 0),
        ((-50, 100, 100), 0),
        ((0, -100, -100), -32512),
        ((0, -100, -50), -32512),
        ((0, -100, 0), -32512),
        ((0, -100, 50), -32512),
        ((0, -100, 100), -32512),
        ((0, -50, -100), -32512),
        ((0, -50, -50), -32512),
        ((0, -50, 0), -32512),
        ((0, -50, 50), -32512),
        ((0, -50, 100), -32512),
        ((0, 0, -100), -32512),
        ((0, 0, -50), -32512),
        ((0, 0, 0), -32512),
        ((0, 0, 50), -32512),
        ((0, 0, 100), -32512),
        ((0, 50, -100), -32512),
        ((0, 50, -50), -32512),
        ((0, 50, 0), 57),
        ((0, 50, 50), 60),
        ((0, 50, 100), 60),
        ((0, 100, -100), 0),
        ((0, 100, -50), 0),
        ((0, 100, 0), 0),
        ((0, 100, 50), 0),
        ((0, 100, 100), 0),
        ((50, -100, -100), -32512),
        ((50, -100, -50), -32512),
        ((50, -100, 0), -32512),
        ((50, -100, 50), -32512),
        ((50, -100, 100), -100),
        ((50, -50, -100), -32512),
        ((50, -50, -50), -32512),
        ((50, -50, 0), -32512),
        ((50, -50, 50), -32512),
        ((50, -50, 100), -60),
        ((50, 0, -100), -32512),
        ((50, 0, -50), -32512),
        ((50, 0, 0), -32512),
        ((50, 0, 50), -32512),
        ((50, 0, 100), -32512),
        ((50, 50, -100), -32512),
        ((50, 50, -50), -32512),
        ((50, 50, 0), 57),
        ((50, 50, 50), 60),
        ((50, 50, 100), 60),
        ((50, 100, -100), 0),
        ((50, 100, -50), 0),
        ((50, 100, 0), 0),
        ((50, 100, 50), 0),
        ((50, 100, 100), 0),
        ((100, -100, -100), -32512),
        ((100, -100, -50), -32512),
        ((100, -100, 0), -32512),
        ((100, -100, 50), -103),
        ((100, -100, 100), -100),
        ((100, -50, -100), -32512),
        ((100, -50, -50), -32512),
        ((100, -50, 0), -32512),
        ((100, -50, 50), -32512),
        ((100, -50, 100), -60),
        ((100, 0, -100), -32512),
        ((100, 0, -50), -32512),
        ((100, 0, 0), -32512),
        ((100, 0, 50), -32512),
        ((100, 0, 100), -32512),
        ((100, 50, -100), -32512),
        ((100, 50, -50), -32512),
        ((100, 50, 0), 57),
        ((100, 50, 50), 57),
        ((100, 50, 100), 60),
        ((100, 100, -100), 0),
        ((100, 100, -50), 0),
        ((100, 100, 0), 0),
        ((100, 100, 50), 0),
        ((100, 100, 100), 0),
    ];
    const AQ_FYF: &[((i32, i32, i32), i32)] = &[
        ((-100, -100, -100), -32512),
        ((-100, -100, -50), -32512),
        ((-100, -100, 0), -32512),
        ((-100, -100, 50), -32512),
        ((-100, -100, 100), -32512),
        ((-100, -50, -100), -32512),
        ((-100, -50, -50), -63),
        ((-100, -50, 0), -63),
        ((-100, -50, 50), -63),
        ((-100, -50, 100), -32512),
        ((-100, 0, -100), -32512),
        ((-100, 0, -50), -32512),
        ((-100, 0, 0), -32512),
        ((-100, 0, 50), -32512),
        ((-100, 0, 100), -32512),
        ((-100, 50, -100), -32512),
        ((-100, 50, -50), -32512),
        ((-100, 50, 0), -32512),
        ((-100, 50, 50), -32512),
        ((-100, 50, 100), -32512),
        ((-100, 100, -100), -32512),
        ((-100, 100, -50), -32512),
        ((-100, 100, 0), -32512),
        ((-100, 100, 50), -32512),
        ((-100, 100, 100), -32512),
        ((-50, -100, -100), -32512),
        ((-50, -100, -50), -32512),
        ((-50, -100, 0), -32512),
        ((-50, -100, 50), -32512),
        ((-50, -100, 100), -32512),
        ((-50, -50, -100), -32512),
        ((-50, -50, -50), -32512),
        ((-50, -50, 0), -32512),
        ((-50, -50, 50), -32512),
        ((-50, -50, 100), -32512),
        ((-50, 0, -100), -32512),
        ((-50, 0, -50), -32512),
        ((-50, 0, 0), -32512),
        ((-50, 0, 50), -32512),
        ((-50, 0, 100), -32512),
        ((-50, 50, -100), -32512),
        ((-50, 50, -50), -32512),
        ((-50, 50, 0), -32512),
        ((-50, 50, 50), -32512),
        ((-50, 50, 100), -32512),
        ((-50, 100, -100), -32512),
        ((-50, 100, -50), -32512),
        ((-50, 100, 0), 80),
        ((-50, 100, 50), -32512),
        ((-50, 100, 100), -32512),
        ((0, -100, -100), -32512),
        ((0, -100, -50), -32512),
        ((0, -100, 0), -32512),
        ((0, -100, 50), -32512),
        ((0, -100, 100), -32512),
        ((0, -50, -100), -32512),
        ((0, -50, -50), -32512),
        ((0, -50, 0), -32512),
        ((0, -50, 50), -32512),
        ((0, -50, 100), -32512),
        ((0, 0, -100), -32512),
        ((0, 0, -50), -32512),
        ((0, 0, 0), -32512),
        ((0, 0, 50), -32512),
        ((0, 0, 100), -32512),
        ((0, 50, -100), -32512),
        ((0, 50, -50), -32512),
        ((0, 50, 0), -32512),
        ((0, 50, 50), -32512),
        ((0, 50, 100), -32512),
        ((0, 100, -100), -32512),
        ((0, 100, -50), -32512),
        ((0, 100, 0), 80),
        ((0, 100, 50), -32512),
        ((0, 100, 100), -32512),
        ((50, -100, -100), -32512),
        ((50, -100, -50), -32512),
        ((50, -100, 0), -32512),
        ((50, -100, 50), -32512),
        ((50, -100, 100), -100),
        ((50, -50, -100), -32512),
        ((50, -50, -50), -32512),
        ((50, -50, 0), -32512),
        ((50, -50, 50), -32512),
        ((50, -50, 100), -60),
        ((50, 0, -100), -32512),
        ((50, 0, -50), -32512),
        ((50, 0, 0), -32512),
        ((50, 0, 50), -32512),
        ((50, 0, 100), -32512),
        ((50, 50, -100), -32512),
        ((50, 50, -50), -32512),
        ((50, 50, 0), -32512),
        ((50, 50, 50), -32512),
        ((50, 50, 100), -32512),
        ((50, 100, -100), -32512),
        ((50, 100, -50), -32512),
        ((50, 100, 0), 80),
        ((50, 100, 50), -32512),
        ((50, 100, 100), -32512),
        ((100, -100, -100), -32512),
        ((100, -100, -50), -32512),
        ((100, -100, 0), -32512),
        ((100, -100, 50), -103),
        ((100, -100, 100), -100),
        ((100, -50, -100), -32512),
        ((100, -50, -50), -32512),
        ((100, -50, 0), -32512),
        ((100, -50, 50), -32512),
        ((100, -50, 100), -60),
        ((100, 0, -100), -32512),
        ((100, 0, -50), -32512),
        ((100, 0, 0), -32512),
        ((100, 0, 50), -32512),
        ((100, 0, 100), -32512),
        ((100, 50, -100), -32512),
        ((100, 50, -50), -32512),
        ((100, 50, 0), -32512),
        ((100, 50, 50), -32512),
        ((100, 50, 100), -32512),
        ((100, 100, -100), -32512),
        ((100, 100, -50), -32512),
        ((100, 100, 0), 80),
        ((100, 100, 50), -32512),
        ((100, 100, 100), -32512),
    ];
    const AQ_FBS: &[((i32, i32, i32), u8)] = &[
        ((-100, -100, -100), 1),
        ((-100, -100, -50), 2),
        ((-100, -100, 0), 1),
        ((-100, -100, 50), 1),
        ((-100, -100, 100), 1),
        ((-100, -50, -100), 1),
        ((-100, -50, -50), 2),
        ((-100, -50, 0), 2),
        ((-100, -50, 50), 2),
        ((-100, -50, 100), 1),
        ((-100, 0, -100), 1),
        ((-100, 0, -50), 1),
        ((-100, 0, 0), 1),
        ((-100, 0, 50), 1),
        ((-100, 0, 100), 1),
        ((-100, 50, -100), 1),
        ((-100, 50, -50), 1),
        ((-100, 50, 0), 1),
        ((-100, 50, 50), 1),
        ((-100, 50, 100), 1),
        ((-100, 100, -100), 1),
        ((-100, 100, -50), 1),
        ((-100, 100, 0), 1),
        ((-100, 100, 50), 1),
        ((-100, 100, 100), 1),
        ((-50, -100, -100), 1),
        ((-50, -100, -50), 1),
        ((-50, -100, 0), 2),
        ((-50, -100, 50), 2),
        ((-50, -100, 100), 1),
        ((-50, -50, -100), 1),
        ((-50, -50, -50), 1),
        ((-50, -50, 0), 1),
        ((-50, -50, 50), 1),
        ((-50, -50, 100), 1),
        ((-50, 0, -100), 2),
        ((-50, 0, -50), 1),
        ((-50, 0, 0), 1),
        ((-50, 0, 50), 1),
        ((-50, 0, 100), 1),
        ((-50, 50, -100), 1),
        ((-50, 50, -50), 1),
        ((-50, 50, 0), 2),
        ((-50, 50, 50), 2),
        ((-50, 50, 100), 1),
        ((-50, 100, -100), 1),
        ((-50, 100, -50), 1),
        ((-50, 100, 0), 2),
        ((-50, 100, 50), 2),
        ((-50, 100, 100), 2),
        ((0, -100, -100), 1),
        ((0, -100, -50), 2),
        ((0, -100, 0), 2),
        ((0, -100, 50), 2),
        ((0, -100, 100), 1),
        ((0, -50, -100), 1),
        ((0, -50, -50), 1),
        ((0, -50, 0), 1),
        ((0, -50, 50), 1),
        ((0, -50, 100), 1),
        ((0, 0, -100), 2),
        ((0, 0, -50), 2),
        ((0, 0, 0), 1),
        ((0, 0, 50), 1),
        ((0, 0, 100), 1),
        ((0, 50, -100), 1),
        ((0, 50, -50), 1),
        ((0, 50, 0), 1),
        ((0, 50, 50), 1),
        ((0, 50, 100), 1),
        ((0, 100, -100), 1),
        ((0, 100, -50), 2),
        ((0, 100, 0), 1),
        ((0, 100, 50), 1),
        ((0, 100, 100), 1),
        ((50, -100, -100), 1),
        ((50, -100, -50), 2),
        ((50, -100, 0), 2),
        ((50, -100, 50), 2),
        ((50, -100, 100), 1),
        ((50, -50, -100), 1),
        ((50, -50, -50), 1),
        ((50, -50, 0), 1),
        ((50, -50, 50), 1),
        ((50, -50, 100), 1),
        ((50, 0, -100), 2),
        ((50, 0, -50), 2),
        ((50, 0, 0), 1),
        ((50, 0, 50), 1),
        ((50, 0, 100), 1),
        ((50, 50, -100), 1),
        ((50, 50, -50), 1),
        ((50, 50, 0), 1),
        ((50, 50, 50), 1),
        ((50, 50, 100), 1),
        ((50, 100, -100), 1),
        ((50, 100, -50), 2),
        ((50, 100, 0), 1),
        ((50, 100, 50), 1),
        ((50, 100, 100), 1),
        ((100, -100, -100), 1),
        ((100, -100, -50), 2),
        ((100, -100, 0), 1),
        ((100, -100, 50), 1),
        ((100, -100, 100), 1),
        ((100, -50, -100), 2),
        ((100, -50, -50), 2),
        ((100, -50, 0), 2),
        ((100, -50, 50), 2),
        ((100, -50, 100), 2),
        ((100, 0, -100), 1),
        ((100, 0, -50), 2),
        ((100, 0, 0), 1),
        ((100, 0, 50), 1),
        ((100, 0, 100), 2),
        ((100, 50, -100), 1),
        ((100, 50, -50), 1),
        ((100, 50, 0), 1),
        ((100, 50, 50), 1),
        ((100, 50, 100), 1),
        ((100, 100, -100), 2),
        ((100, 100, -50), 2),
        ((100, 100, 0), 1),
        ((100, 100, 50), 1),
        ((100, 100, 100), 1),
    ];
    type AqDensityKey = (i32, i32, i32, i32, i32);
    type AqPointKey = (i32, i32, i32, f64);
    const AQ_FL: &[(crate::BlockPos, (i32, u8))] = &[
        ((-100, -100, -100), (-32512, 2)),
        ((-100, -100, -50), (-32512, 2)),
        ((-100, -100, 0), (-32512, 2)),
        ((-100, -100, 50), (-32512, 2)),
        ((-100, -100, 100), (-32512, 2)),
        ((-100, -50, -100), (-32512, 1)),
        ((-100, -50, -50), (-63, 2)),
        ((-100, -50, 0), (-63, 2)),
        ((-100, -50, 50), (-63, 2)),
        ((-100, -50, 100), (-32512, 1)),
        ((-100, 0, -100), (-32512, 1)),
        ((-100, 0, -50), (-32512, 1)),
        ((-100, 0, 0), (-32512, 1)),
        ((-100, 0, 50), (-32512, 1)),
        ((-100, 0, 100), (-32512, 1)),
        ((-100, 50, -100), (-32512, 1)),
        ((-100, 50, -50), (63, 1)),
        ((-100, 50, 0), (63, 1)),
        ((-100, 50, 50), (-32512, 1)),
        ((-100, 50, 100), (63, 1)),
        ((-100, 100, -100), (63, 1)),
        ((-100, 100, -50), (63, 1)),
        ((-100, 100, 0), (63, 1)),
        ((-100, 100, 50), (63, 1)),
        ((-100, 100, 100), (63, 1)),
        ((-50, -100, -100), (-32512, 2)),
        ((-50, -100, -50), (-32512, 2)),
        ((-50, -100, 0), (-32512, 2)),
        ((-50, -100, 50), (-32512, 2)),
        ((-50, -100, 100), (-32512, 2)),
        ((-50, -50, -100), (-32512, 1)),
        ((-50, -50, -50), (-32512, 1)),
        ((-50, -50, 0), (-32512, 1)),
        ((-50, -50, 50), (-32512, 1)),
        ((-50, -50, 100), (-32512, 1)),
        ((-50, 0, -100), (-32512, 1)),
        ((-50, 0, -50), (-32512, 1)),
        ((-50, 0, 0), (-32512, 1)),
        ((-50, 0, 50), (-32512, 1)),
        ((-50, 0, 100), (-32512, 1)),
        ((-50, 50, -100), (-32512, 1)),
        ((-50, 50, -50), (63, 1)),
        ((-50, 50, 0), (63, 1)),
        ((-50, 50, 50), (-32512, 1)),
        ((-50, 50, 100), (63, 1)),
        ((-50, 100, -100), (63, 1)),
        ((-50, 100, -50), (63, 1)),
        ((-50, 100, 0), (63, 1)),
        ((-50, 100, 50), (63, 1)),
        ((-50, 100, 100), (63, 1)),
        ((0, -100, -100), (-32512, 2)),
        ((0, -100, -50), (-32512, 2)),
        ((0, -100, 0), (-32512, 2)),
        ((0, -100, 50), (-32512, 2)),
        ((0, -100, 100), (-32512, 2)),
        ((0, -50, -100), (-32512, 1)),
        ((0, -50, -50), (-32512, 1)),
        ((0, -50, 0), (-32512, 1)),
        ((0, -50, 50), (-32512, 1)),
        ((0, -50, 100), (-32512, 1)),
        ((0, 0, -100), (-32512, 1)),
        ((0, 0, -50), (-32512, 1)),
        ((0, 0, 0), (-32512, 1)),
        ((0, 0, 50), (-32512, 1)),
        ((0, 0, 100), (-32512, 1)),
        ((0, 50, -100), (-32512, 1)),
        ((0, 50, -50), (63, 1)),
        ((0, 50, 0), (63, 1)),
        ((0, 50, 50), (-32512, 1)),
        ((0, 50, 100), (-32512, 1)),
        ((0, 100, -100), (63, 1)),
        ((0, 100, -50), (63, 1)),
        ((0, 100, 0), (63, 1)),
        ((0, 100, 50), (63, 1)),
        ((0, 100, 100), (63, 1)),
        ((50, -100, -100), (-32512, 2)),
        ((50, -100, -50), (-32512, 2)),
        ((50, -100, 0), (-32512, 2)),
        ((50, -100, 50), (-32512, 2)),
        ((50, -100, 100), (-100, 2)),
        ((50, -50, -100), (-32512, 1)),
        ((50, -50, -50), (-32512, 1)),
        ((50, -50, 0), (-32512, 1)),
        ((50, -50, 50), (-32512, 1)),
        ((50, -50, 100), (-60, 1)),
        ((50, 0, -100), (-32512, 1)),
        ((50, 0, -50), (-32512, 1)),
        ((50, 0, 0), (-32512, 1)),
        ((50, 0, 50), (-32512, 1)),
        ((50, 0, 100), (-32512, 1)),
        ((50, 50, -100), (-32512, 1)),
        ((50, 50, -50), (63, 1)),
        ((50, 50, 0), (63, 1)),
        ((50, 50, 50), (63, 1)),
        ((50, 50, 100), (-32512, 1)),
        ((50, 100, -100), (-32512, 1)),
        ((50, 100, -50), (63, 1)),
        ((50, 100, 0), (63, 1)),
        ((50, 100, 50), (63, 1)),
        ((50, 100, 100), (63, 1)),
        ((100, -100, -100), (-32512, 2)),
        ((100, -100, -50), (-32512, 2)),
        ((100, -100, 0), (-32512, 2)),
        ((100, -100, 50), (-103, 2)),
        ((100, -100, 100), (-100, 2)),
        ((100, -50, -100), (-32512, 1)),
        ((100, -50, -50), (-32512, 1)),
        ((100, -50, 0), (-32512, 1)),
        ((100, -50, 50), (-32512, 1)),
        ((100, -50, 100), (-60, 2)),
        ((100, 0, -100), (-32512, 1)),
        ((100, 0, -50), (-32512, 1)),
        ((100, 0, 0), (-32512, 1)),
        ((100, 0, 50), (-32512, 1)),
        ((100, 0, 100), (-32512, 1)),
        ((100, 50, -100), (63, 1)),
        ((100, 50, -50), (63, 1)),
        ((100, 50, 0), (63, 1)),
        ((100, 50, 50), (63, 1)),
        ((100, 50, 100), (-32512, 1)),
        ((100, 100, -100), (63, 1)),
        ((100, 100, -50), (63, 1)),
        ((100, 100, 0), (63, 1)),
        ((100, 100, 50), (63, 1)),
        ((100, 100, 100), (63, 1)),
    ];
    // (x, y, z, first height, second height) -> the density the reference reported
    const AQ_CD: &[(AqDensityKey, f64)] = &[
        ((-100, -100, -100, 0, 0), 0.0),
        ((-100, -100, -50, 50, 0), -19.3),
        ((-100, -100, 0, 0, 0), 0.0),
        ((-100, -100, 50, 50, 0), -19.3),
        ((-100, -100, 100, 0, 0), 0.0),
        ((-100, -50, -100, 0, 0), 0.0),
        ((-100, -50, -50, 50, 0), -9.3),
        ((-100, -50, 0, 0, 0), 0.0),
        ((-100, -50, 50, 50, 0), -9.3),
        ((-100, -50, 100, 0, 0), 0.0),
        ((-100, 0, -100, 0, 0), 0.0),
        ((-100, 0, -50, -50, 0), 0.20838508),
        ((-100, 0, 0, 0, 0), 0.0),
        ((-100, 0, 50, 50, 0), 2.069189),
        ((-100, 0, 100, 0, 0), 0.0),
        ((-100, 50, -100, 0, 0), 0.0),
        ((-100, 50, -50, -50, 0), -40.4),
        ((-100, 50, 0, 0, 0), 0.0),
        ((-100, 50, 50, -50, 0), -40.4),
        ((-100, 50, 100, 0, 0), 0.0),
        ((-100, 100, -100, 0, 0), 0.0),
        ((-100, 100, -50, -50, 0), -80.4),
        ((-100, 100, 0, 0, 0), 0.0),
        ((-100, 100, 50, -50, 0), -80.4),
        ((-100, 100, 100, 0, 0), 0.0),
        ((-50, -100, -100, 0, 50), -19.3),
        ((-50, -100, -50, 50, 50), 0.0),
        ((-50, -100, 0, 0, -50), -9.3),
        ((-50, -100, 50, 50, -50), -9.3),
        ((-50, -100, 100, 0, -50), -9.3),
        ((-50, -50, -100, 0, 50), -9.3),
        ((-50, -50, -50, 50, 50), 0.0),
        ((-50, -50, 0, 0, -50), 2.2042949518442185),
        ((-50, -50, 50, 50, -50), 1.8767275908406176),
        ((-50, -50, 100, 0, -50), 2.3399656359995133),
        ((-50, 0, -100, 0, -50), -0.08405951),
        ((-50, 0, -50, -50, -50), 0.0),
        ((-50, 0, 0, 0, -50), 0.3902410585192353),
        ((-50, 0, 50, 50, -50), 66.0),
        ((-50, 0, 100, 0, -50), -0.7930165090675787),
        ((-50, 50, -100, 0, -50), -40.4),
        ((-50, 50, -50, -50, -50), 0.0),
        ((-50, 50, 0, 0, -50), -40.4),
        ((-50, 50, 50, -50, 50), -0.35570822400215646),
        ((-50, 50, 100, 0, 50), -0.16770224497207317),
        ((-50, 100, -100, 0, -50), -80.4),
        ((-50, 100, -50, -50, -50), 0.0),
        ((-50, 100, 0, 0, -50), -80.4),
        ((-50, 100, 50, -50, 50), -40.4),
        ((-50, 100, 100, 0, 50), -40.4),
        ((0, -100, -100, 0, 0), 0.0),
        ((0, -100, -50, -50, 0), -9.3),
        ((0, -100, 0, 0, 0), 0.0),
        ((0, -100, 50, 50, 0), -19.3),
        ((0, -100, 100, 0, 0), 0.0),
        ((0, -50, -100, 0, 0), 0.0),
        ((0, -50, -50, -50, 0), 2.857141340264507),
        ((0, -50, 0, 0, 0), 0.0),
        ((0, -50, 50, 50, 0), -9.3),
        ((0, -50, 100, 0, 0), 0.0),
        ((0, 0, -100, 0, 0), 0.0),
        ((0, 0, -50, -50, 0), -0.1361016501707068),
        ((0, 0, 0, 0, 0), 0.0),
        ((0, 0, 50, 50, 0), 1.9841279541408636),
        ((0, 0, 100, 0, 0), 0.0),
        ((0, 50, -100, 0, 0), 0.0),
        ((0, 50, -50, -50, 0), -40.4),
        ((0, 50, 0, 0, 0), 0.0),
        ((0, 50, 50, 50, 0), -0.36331007530382964),
        ((0, 50, 100, 0, 0), 0.0),
        ((0, 100, -100, 0, 0), 0.0),
        ((0, 100, -50, -50, 0), -80.4),
        ((0, 100, 0, 0, 0), 0.0),
        ((0, 100, 50, 50, 0), -40.4),
        ((0, 100, 100, 0, 0), 0.0),
        ((50, -100, -100, 0, 50), -19.3),
        ((50, -100, -50, -50, 50), -9.3),
        ((50, -100, 0, 0, 50), -19.3),
        ((50, -100, 50, -50, -50), 0.0),
        ((50, -100, 100, 0, -50), -9.3),
        ((50, -50, -100, 0, 50), -9.3),
        ((50, -50, -50, -50, 50), 1.619242225388449),
        ((50, -50, 0, 0, 50), -9.3),
        ((50, -50, 50, -50, -50), 0.0),
        ((50, -50, 100, 0, -50), 2.1561171703198188),
        ((50, 0, -100, 0, 50), 2.6298865590685954),
        ((50, 0, -50, -50, 50), 66.0),
        ((50, 0, 0, 0, 50), 2.572198917833846),
        ((50, 0, 50, 50, 50), 0.0),
        ((50, 0, 100, 0, 50), 2.082884998883258),
        ((50, 50, -100, 0, -50), -40.4),
        ((50, 50, -50, 50, -50), -0.1894344852785401),
        ((50, 50, 0, 0, 50), -0.7155260733519367),
        ((50, 50, 50, 50, 50), 0.0),
        ((50, 50, 100, 0, 50), -0.4132183490530098),
        ((50, 100, -100, 0, -50), -80.4),
        ((50, 100, -50, 50, -50), -40.4),
        ((50, 100, 0, 0, 50), -40.4),
        ((50, 100, 50, 50, 50), 0.0),
        ((50, 100, 100, 0, 50), -40.4),
        ((100, -100, -100, 0, 0), 0.0),
        ((100, -100, -50, -50, 0), -9.3),
        ((100, -100, 0, 0, 0), 0.0),
        ((100, -100, 50, -50, 0), -9.3),
        ((100, -100, 100, 0, 0), 0.0),
        ((100, -50, -100, 0, 0), 0.0),
        ((100, -50, -50, -50, 0), 1.6711026207576742),
        ((100, -50, 0, 0, 0), 0.0),
        ((100, -50, 50, -50, 0), 2.042353012197518),
        ((100, -50, 100, 0, 0), 0.0),
        ((100, 0, -100, 0, 0), 0.0),
        ((100, 0, -50, -50, 0), 0.3145492757856567),
        ((100, 0, 0, 0, 0), 0.0),
        ((100, 0, 50, 50, 0), 2.27260703684609),
        ((100, 0, 100, 0, 0), 0.0),
        ((100, 50, -100, 0, 0), 0.0),
        ((100, 50, -50, 50, 0), -0.16949328993376553),
        ((100, 50, 0, 0, 0), 0.0),
        ((100, 50, 50, 50, 0), 0.5196380801381327),
        ((100, 50, 100, 0, 0), 0.0),
        ((100, 100, -100, 0, 0), 0.0),
        ((100, 100, -50, 50, 0), -40.4),
        ((100, 100, 0, 0, 0), 0.0),
        ((100, 100, 50, 50, 0), -40.4),
        ((100, 100, 100, 0, 0), 0.0),
    ];
    // (x, y, z, density) -> which fluid the reference put there
    const AQ_AP: &[(AqPointKey, &str)] = &[
        ((112, -100, 64, 0.037482421875), "none"),
        ((112, -100, 66, 0.037482421875), "none"),
        ((112, -100, 68, 0.037482421875), "none"),
        ((112, -100, 70, 0.037482421875), "none"),
        ((112, -100, 72, 0.037482421875), "none"),
        ((112, -100, 74, 0.037482421875), "none"),
        ((112, -80, 64, 0.037482421875), "none"),
        ((112, -80, 66, 0.037482421875), "none"),
        ((112, -80, 68, 0.037482421875), "none"),
        ((112, -80, 70, 0.037482421875), "none"),
        ((112, -80, 72, 0.037482421875), "none"),
        ((112, -80, 74, 0.037482421875), "none"),
        ((112, -60, 64, 0.04861063447117113), "none"),
        ((112, -60, 66, 0.04924175767418443), "none"),
        ((112, -60, 68, 0.04989611436947345), "none"),
        ((112, -60, 70, 0.0505756649980066), "none"),
        ((112, -60, 72, 0.051280297037227154), "none"),
        ((112, -60, 74, 0.05200473277817172), "none"),
        ((112, -40, 64, 0.10768778489063564), "none"),
        ((112, -40, 66, 0.11167582884587932), "none"),
        ((112, -40, 68, 0.1150242394949102), "none"),
        ((112, -40, 70, 0.11846243756482717), "none"),
        ((112, -40, 72, 0.12198087391634192), "none"),
        ((112, -40, 74, 0.12558796524209848), "none"),
        ((112, -20, 64, 0.10797485850904144), "none"),
        ((112, -20, 66, 0.10725848123542484), "none"),
        ((112, -20, 68, 0.1061187765379769), "none"),
        ((112, -20, 70, 0.10630527141203541), "none"),
        ((112, -20, 72, 0.10955530075188523), "none"),
        ((112, -20, 74, 0.11296050502041746), "none"),
        ((112, 0, 64, -0.0016242211141160837), "none"),
        ((112, 0, 68, 0.0013878562706830444), "none"),
        ((112, 0, 70, 0.0030297756784707407), "none"),
        ((112, 0, 72, 0.004722310162617811), "none"),
        ((112, 0, 74, 0.0064699315064971315), "none"),
        ((112, 20, 64, 0.042774410930765096), "none"),
        ((112, 20, 66, 0.040191327400772414), "none"),
        ((112, 20, 68, 0.0375637494138927), "none"),
        ((112, 20, 70, 0.03485919313914573), "none"),
        ((112, 20, 72, 0.032058901444915404), "none"),
        ((112, 20, 74, 0.029170651813548235), "none"),
        ((112, 40, 64, 0.011596925999172775), "none"),
        ((112, 40, 66, 0.014957019593043965), "none"),
        ((112, 40, 68, 0.01827627643898028), "none"),
        ((112, 40, 70, 0.021477214738188897), "none"),
        ((112, 40, 72, 0.02448508577325224), "none"),
        ((112, 40, 74, 0.027262647782191486), "none"),
        ((112, 60, 64, 0.14338446306313316), "none"),
        ((112, 60, 66, 0.16772904485726645), "none"),
        ((112, 60, 68, 0.1756309873589998), "none"),
        ((112, 60, 70, 0.1782686032102433), "none"),
        ((112, 60, 72, 0.18822148746793055), "none"),
        ((112, 60, 74, 0.20387997189913717), "none"),
        ((112, 80, 64, -0.28931054817132484), "air"),
        ((112, 80, 66, -0.2808098154769529), "air"),
        ((112, 80, 68, -0.2806908647477032), "air"),
        ((112, 80, 70, -0.28068300576359284), "air"),
        ((112, 80, 72, -0.2805878392398348), "air"),
        ((112, 80, 74, -0.27824504138444317), "air"),
        ((112, 100, 64, -0.4583333333333333), "air"),
        ((112, 100, 66, -0.4583333333333333), "air"),
        ((112, 100, 68, -0.4583333333333333), "air"),
        ((112, 100, 70, -0.4583333333333333), "air"),
        ((112, 100, 72, -0.4583333333333333), "air"),
        ((112, 100, 74, -0.4583333333333333), "air"),
        ((114, -100, 64, 0.037482421875), "none"),
        ((114, -100, 66, 0.037482421875), "none"),
        ((114, -100, 68, 0.037482421875), "none"),
        ((114, -100, 70, 0.037482421875), "none"),
        ((114, -100, 72, 0.037482421875), "none"),
        ((114, -100, 74, 0.037482421875), "none"),
        ((114, -80, 64, 0.037482421875), "none"),
        ((114, -80, 66, 0.037482421875), "none"),
        ((114, -80, 68, 0.037482421875), "none"),
        ((114, -80, 70, 0.037482421875), "none"),
        ((114, -80, 72, 0.037482421875), "none"),
        ((114, -80, 74, 0.037482421875), "none"),
        ((114, -60, 64, 0.0491033911468506), "none"),
        ((114, -60, 66, 0.04974948234981454), "none"),
        ((114, -60, 68, 0.05041864489941496), "none"),
        ((114, -60, 70, 0.051112999802414315), "none"),
        ((114, -60, 72, 0.051832684146704056), "none"),
        ((114, -60, 74, 0.05257231446254198), "none"),
        ((114, -40, 64, 0.1122919611041932), "none"),
        ((114, -40, 66, 0.11636247433302638), "none"),
        ((114, -40, 68, 0.11979640774332366), "none"),
        ((114, -40, 70, 0.12330055500715384), "none"),
        ((114, -40, 72, 0.1268619579414046), "none"),
        ((114, -40, 74, 0.13048474950215), "none"),
        ((114, -20, 64, 0.10435296791344484), "none"),
        ((114, -20, 66, 0.10558672714675042), "none"),
        ((114, -20, 68, 0.10831868013423275), "none"),
        ((114, -20, 70, 0.11121282163190734), "none"),
        ((114, -20, 72, 0.11433776346079558), "none"),
        ((114, -20, 74, 0.11770444723497474), "none"),
        ((114, 0, 64, -0.0026209759846139574), "water"),
        ((114, 0, 66, -0.0011869543056835608), "water"),
        ((114, 0, 70, 0.002068623223791626), "none"),
        ((114, 0, 72, 0.0038193250297024243), "none"),
        ((114, 0, 74, 0.005646396361630968), "none"),
        ((114, 20, 64, 0.04183884597405475), "none"),
        ((114, 20, 66, 0.039334239558802865), "none"),
        ((114, 20, 68, 0.0367933735261107), "none"),
        ((114, 20, 70, 0.03417828296442246), "none"),
        ((114, 20, 72, 0.03146119496538579), "none"),
        ((114, 20, 74, 0.028640178813057193), "none"),
        ((114, 40, 64, 0.0011727557986376189), "none"),
        ((114, 40, 66, 0.004478197250912497), "none"),
        ((114, 40, 68, 0.007807656734373648), "none"),
        ((114, 40, 70, 0.0110982528501824), "none"),
        ((114, 40, 72, 0.014287968294347897), "none"),
        ((114, 40, 74, 0.017340060385442255), "none"),
        ((114, 60, 64, 0.057307692379898946), "none"),
        ((114, 60, 66, 0.07909878599088975), "none"),
        ((114, 60, 68, 0.09103769264897049), "none"),
        ((114, 60, 70, 0.10529675531043547), "none"),
        ((114, 60, 72, 0.1261191394093652), "none"),
        ((114, 60, 74, 0.15323465023530602), "none"),
        ((114, 80, 64, -0.3135251519473628), "air"),
        ((114, 80, 66, -0.3092766951165722), "air"),
        ((114, 80, 68, -0.3063751991759311), "air"),
        ((114, 80, 70, -0.3004342091280733), "air"),
        ((114, 80, 72, -0.29703745590700253), "air"),
        ((114, 80, 74, -0.2920638815250855), "air"),
        ((114, 100, 64, -0.4583333333333333), "air"),
        ((114, 100, 66, -0.4583333333333333), "air"),
        ((114, 100, 68, -0.4583333333333333), "air"),
        ((114, 100, 70, -0.4583333333333333), "air"),
        ((114, 100, 72, -0.4583333333333333), "air"),
        ((114, 100, 74, -0.4583333333333333), "air"),
        ((116, -100, 64, 0.037482421875), "none"),
        ((116, -100, 66, 0.037482421875), "none"),
        ((116, -100, 68, 0.037482421875), "none"),
        ((116, -100, 70, 0.037482421875), "none"),
        ((116, -100, 72, 0.037482421875), "none"),
        ((116, -100, 74, 0.037482421875), "none"),
        ((116, -80, 64, 0.037482421875), "none"),
        ((116, -80, 66, 0.037482421875), "none"),
        ((116, -80, 68, 0.037482421875), "none"),
        ((116, -80, 70, 0.037482421875), "none"),
        ((116, -80, 72, 0.037482421875), "none"),
        ((116, -80, 74, 0.037482421875), "none"),
        ((116, -60, 64, 0.049510031628008565), "none"),
        ((116, -60, 66, 0.0501688864114582), "none"),
        ((116, -60, 68, 0.05085081029508635), "none"),
        ((116, -60, 70, 0.051557962654552196), "none"),
        ((116, -60, 72, 0.05229073376331405), "none"),
        ((116, -60, 74, 0.05304386684600273), "none"),
        ((116, -40, 64, 0.11647696380354154), "none"),
        ((116, -40, 66, 0.12065670598690036), "none"),
        ((116, -40, 68, 0.124186198423662), "none"),
        ((116, -40, 70, 0.12776934001653206), "none"),
        ((116, -40, 72, 0.1313900411197126), "none"),
        ((116, -40, 74, 0.13504793394752057), "none"),
        ((116, -20, 64, 0.10797310440989095), "none"),
        ((116, -20, 66, 0.11052803071968675), "none"),
        ((116, -20, 68, 0.11313075983384659), "none"),
        ((116, -20, 70, 0.11589560860382501), "none"),
        ((116, -20, 72, 0.11889599517563405), "none"),
        ((116, -20, 74, 0.12214992807094607), "none"),
        ((116, 0, 64, -0.003764380972543319), "water"),
        ((116, 0, 66, -0.002339168705169207), "water"),
        ((116, 0, 72, 0.0027605740566328273), "none"),
        ((116, 0, 74, 0.0046712919320928475), "none"),
        ((116, 20, 64, 0.04027812111674997), "none"),
        ((116, 20, 66, 0.03846080745866824), "none"),
        ((116, 20, 68, 0.03602535830962611), "none"),
        ((116, 20, 70, 0.033477031827460015), "none"),
        ((116, 20, 72, 0.03082854239914883), "none"),
        ((116, 20, 74, 0.028967404819271153), "none"),
        ((116, 40, 64, -0.009355588931802767), "none"),
        ((116, 40, 66, -0.006094366713842806), "air"),
        ((116, 40, 68, -0.0027537988904787606), "air"),
        ((116, 40, 72, 0.00396682662711753), "none"),
        ((116, 40, 74, 0.007260950530417173), "none"),
        ((116, 60, 64, 0.022624582978071506), "none"),
        ((116, 60, 66, 0.023942871216082746), "none"),
        ((116, 60, 68, 0.037192323513527165), "none"),
        ((116, 60, 70, 0.053305618429865455), "none"),
        ((116, 60, 72, 0.06694547220363958), "none"),
        ((116, 60, 74, 0.08711813973093903), "none"),
        ((116, 80, 64, -0.3326652310213258), "air"),
        ((116, 80, 66, -0.32962834810938174), "air"),
        ((116, 80, 68, -0.32236370014057947), "air"),
        ((116, 80, 70, -0.31670491006554574), "air"),
        ((116, 80, 72, -0.3130639601887072), "air"),
        ((116, 80, 74, -0.3124769234268471), "air"),
        ((116, 100, 64, -0.4583333333333333), "air"),
        ((116, 100, 66, -0.4583333333333333), "air"),
        ((116, 100, 68, -0.4583333333333333), "air"),
        ((116, 100, 70, -0.4583333333333333), "air"),
        ((116, 100, 72, -0.4583333333333333), "air"),
        ((116, 100, 74, -0.4583333333333333), "air"),
        ((118, -100, 64, 0.037482421875), "none"),
        ((118, -100, 66, 0.037482421875), "none"),
        ((118, -100, 68, 0.037482421875), "none"),
        ((118, -100, 70, 0.037482421875), "none"),
        ((118, -100, 72, 0.037482421875), "none"),
        ((118, -100, 74, 0.037482421875), "none"),
        ((118, -80, 64, 0.037482421875), "none"),
        ((118, -80, 66, 0.037482421875), "none"),
        ((118, -80, 68, 0.037482421875), "none"),
        ((118, -80, 70, 0.037482421875), "none"),
        ((118, -80, 72, 0.037482421875), "none"),
        ((118, -80, 74, 0.037482421875), "none"),
        ((118, -60, 64, 0.04983026397203373), "none"),
        ((118, -60, 66, 0.05049926028532013), "none"),
        ((118, -60, 68, 0.05119129093033625), "none"),
        ((118, -60, 70, 0.05190836104462707), "none"),
        ((118, -60, 72, 0.0526510917415344), "none"),
        ((118, -60, 74, 0.05341462434689053), "none"),
        ((118, -40, 64, 0.12023506214440967), "none"),
        ((118, -40, 66, 0.12453642919562771), "none"),
        ((118, -40, 68, 0.1281709824736104), "none"),
        ((118, -40, 70, 0.13184553502467083), "none"),
        ((118, -40, 72, 0.13554113610267055), "none"),
        ((118, -40, 74, 0.13925273367632113), "none"),
        ((118, -20, 64, 0.11282016461369758), "none"),
        ((118, -20, 66, 0.11526093589883914), "none"),
        ((118, -20, 68, 0.11774539644635261), "none"),
        ((118, -20, 70, 0.12038440430256446), "none"),
        ((118, -20, 72, 0.12325317705242089), "none"),
        ((118, -20, 74, 0.12637678353248477), "none"),
        ((118, 0, 64, -0.00501589634392619), "water"),
        ((118, 0, 66, -0.003601631485605401), "water"),
        ((118, 0, 68, -0.0020166185756455924), "water"),
        ((118, 0, 72, 0.0015704124446037308), "none"),
        ((118, 0, 74, 0.0035605198020311826), "none"),
        ((118, 20, 64, 0.040232966220497414), "none"),
        ((118, 20, 66, 0.039486572716430204), "none"),
        ((118, 20, 68, 0.03885242305023035), "none"),
        ((118, 20, 70, 0.03837601204655802), "none"),
        ((118, 20, 72, 0.03770430407795084), "none"),
        ((118, 20, 74, 0.03707763999605109), "none"),
        ((118, 40, 64, -0.016298811685686653), "none"),
        ((118, 40, 66, -0.016656636719901533), "air"),
        ((118, 40, 68, -0.01330299024830442), "air"),
        ((118, 40, 70, -0.009864486324034218), "air"),
        ((118, 40, 72, -0.006380723268648157), "air"),
        ((118, 40, 74, -0.002886835272463701), "air"),
        ((118, 60, 64, 0.006086790713922152), "none"),
        ((118, 60, 66, 0.006014479808113486), "none"),
        ((118, 60, 68, 0.008953321476532182), "none"),
        ((118, 60, 70, 0.011915636473415587), "none"),
        ((118, 60, 72, 0.01001192490238903), "none"),
        ((118, 60, 74, 0.0075500927486281426), "none"),
        ((118, 80, 64, -0.3462118919745469), "air"),
        ((118, 80, 66, -0.34419241078645835), "air"),
        ((118, 80, 68, -0.33580861045450133), "air"),
        ((118, 80, 70, -0.33008534054566163), "air"),
        ((118, 80, 72, -0.333649815109498), "air"),
        ((118, 80, 74, -0.33771329428807284), "air"),
        ((118, 100, 64, -0.4583333333333333), "air"),
        ((118, 100, 66, -0.4583333333333333), "air"),
        ((118, 100, 68, -0.4583333333333333), "air"),
        ((118, 100, 70, -0.4583333333333333), "air"),
        ((118, 100, 72, -0.4583333333333333), "air"),
        ((118, 100, 74, -0.4583333333333333), "air"),
        ((120, -100, 64, 0.037482421875), "none"),
        ((120, -100, 66, 0.037482421875), "none"),
        ((120, -100, 68, 0.037482421875), "none"),
        ((120, -100, 70, 0.037482421875), "none"),
        ((120, -100, 72, 0.037482421875), "none"),
        ((120, -100, 74, 0.037482421875), "none"),
        ((120, -80, 64, 0.037482421875), "none"),
        ((120, -80, 66, 0.037482421875), "none"),
        ((120, -80, 68, 0.037482421875), "none"),
        ((120, -80, 70, 0.037482421875), "none"),
        ((120, -80, 72, 0.037482421875), "none"),
        ((120, -80, 74, 0.037482421875), "none"),
        ((120, -60, 64, 0.05006400032487905), "none"),
        ((120, -60, 66, 0.05074065517392799), "none"),
        ((120, -60, 68, 0.05144011744518559), "none"),
        ((120, -60, 70, 0.05216399757431933), "none"),
        ((120, -60, 72, 0.052913091382129525), "none"),
        ((120, -60, 74, 0.053683198493088384), "none"),
        ((120, -40, 64, 0.1235607292468892), "none"),
        ((120, -40, 66, 0.12798336289135304), "none"),
        ((120, -40, 68, 0.13173164094126777), "none"),
        ((120, -40, 70, 0.13550880522456066), "none"),
        ((120, -40, 72, 0.1392932761010718), "none"),
        ((120, -40, 74, 0.14307520910884483), "none"),
        ((120, -20, 64, 0.11746888767713162), "none"),
        ((120, -20, 66, 0.1198086187834423), "none"),
        ((120, -20, 68, 0.12218509691580316), "none"),
        ((120, -20, 70, 0.12469970986670785), "none"),
        ((120, -20, 72, 0.1274266324562779), "none"),
        ((120, -20, 74, 0.13039812171785095), "none"),
        ((120, 0, 64, -0.006329576321547214), "water"),
        ((120, 0, 66, -0.004930192503238298), "water"),
        ((120, 0, 68, -0.003355964670343278), "water"),
        ((120, 0, 70, -0.0016206542469077872), "water"),
        ((120, 0, 74, 0.002332419553726638), "none"),
        ((120, 20, 64, 0.04783863627983937), "none"),
        ((120, 20, 66, 0.04699098011102891), "none"),
        ((120, 20, 68, 0.046231913852781324), "none"),
        ((120, 20, 70, 0.04560004002476374), "none"),
        ((120, 20, 72, 0.04512419653798615), "none"),
        ((120, 20, 74, 0.04482086018104904), "none"),
        ((120, 40, 64, -0.017456523705167773), "none"),
        ((120, 40, 66, -0.020044623482270124), "air"),
        ((120, 40, 68, -0.022372181411266172), "air"),
        ((120, 40, 70, -0.020228945291907708), "air"),
        ((120, 40, 72, -0.01664436674077766), "air"),
        ((120, 40, 74, -0.013001583733654043), "air"),
        ((120, 60, 64, -0.010805185122555435), "water"),
        ((120, 60, 66, -0.011684313707812422), "water"),
        ((120, 60, 68, -0.007705484690135335), "water"),
        ((120, 60, 70, -0.012326309226980426), "water"),
        ((120, 60, 72, -0.019043795741958334), "water"),
        ((120, 60, 74, -0.023185441889689514), "water"),
        ((120, 80, 64, -0.3611328625547435), "air"),
        ((120, 80, 66, -0.3586517592327399), "air"),
        ((120, 80, 68, -0.3524534485283812), "air"),
        ((120, 80, 70, -0.35323218454039057), "air"),
        ((120, 80, 72, -0.36213549677301105), "air"),
        ((120, 80, 74, -0.3684474143996314), "air"),
        ((120, 100, 64, -0.4583333333333333), "air"),
        ((120, 100, 66, -0.4583333333333333), "air"),
        ((120, 100, 68, -0.4583333333333333), "air"),
        ((120, 100, 70, -0.4583333333333333), "air"),
        ((120, 100, 72, -0.4583333333333333), "air"),
        ((120, 100, 74, -0.4583333333333333), "air"),
        ((122, -100, 64, 0.037482421875), "none"),
        ((122, -100, 66, 0.037482421875), "none"),
        ((122, -100, 68, 0.037482421875), "none"),
        ((122, -100, 70, 0.037482421875), "none"),
        ((122, -100, 72, 0.037482421875), "none"),
        ((122, -100, 74, 0.037482421875), "none"),
        ((122, -80, 64, 0.037482421875), "none"),
        ((122, -80, 66, 0.037482421875), "none"),
        ((122, -80, 68, 0.037482421875), "none"),
        ((122, -80, 70, 0.037482421875), "none"),
        ((122, -80, 72, 0.037482421875), "none"),
        ((122, -80, 74, 0.037482421875), "none"),
        ((122, -60, 64, 0.050211152183993704), "none"),
        ((122, -60, 66, 0.05089355899866971), "none"),
        ((122, -60, 68, 0.05159826596516877), "none"),
        ((122, -60, 70, 0.05232622949491639), "none"),
        ((122, -60, 72, 0.053078329171471185), "none"),
        ((122, -60, 74, 0.05385122904837061), "none"),
        ((122, -40, 64, 0.12644943947294945), "none"),
        ((122, -40, 66, 0.13095960991588632), "none"),
        ((122, -40, 68, 0.13485451601704876), "none"),
        ((122, -40, 70, 0.13874405974643175), "none"),
        ((122, -40, 72, 0.1426292409194739), "none"),
        ((122, -40, 74, 0.14649538659711875), "none"),
        ((122, -20, 64, 0.12191638116406471), "none"),
        ((122, -20, 66, 0.12416901766463509), "none"),
        ((122, -20, 68, 0.1264480340172126), "none"),
        ((122, -20, 70, 0.12884021810202248), "none"),
        ((122, -20, 72, 0.13141636494496137), "none"),
        ((122, -20, 74, 0.13421609155559988), "none"),
        ((122, 0, 64, -0.007667256303541582), "water"),
        ((122, 0, 66, -0.006288822820341533), "water"),
        ((122, 0, 68, -0.004737470102527975), "water"),
        ((122, 0, 70, -0.0030099389619020873), "water"),
        ((122, 0, 72, -0.0010942861750551764), "water"),
        ((122, 0, 74, 0.001001992078975999), "none"),
        ((122, 20, 64, 0.05535892661135848), "none"),
        ((122, 20, 66, 0.05444413322973901), "none"),
        ((122, 20, 68, 0.05359719969130291), "none"),
        ((122, 20, 70, 0.05284624631887433), "none"),
        ((122, 20, 72, 0.052209127741325245), "none"),
        ((122, 20, 74, 0.05170112238940831), "none"),
        ((122, 40, 64, -0.013498316953033454), "none"),
        ((122, 40, 66, -0.016896390550754353), "air"),
        ((122, 40, 68, -0.01994683889106233), "air"),
        ((122, 40, 70, -0.022658183924480487), "air"),
        ((122, 40, 72, -0.02460705550987633), "air"),
        ((122, 40, 74, -0.02133677750482264), "none"),
        ((122, 60, 64, -0.02580014098083049), "water"),
        ((122, 60, 66, -0.027410062228040422), "water"),
        ((122, 60, 68, -0.02425659570836858), "water"),
        ((122, 60, 70, -0.03261718168256943), "water"),
        ((122, 60, 72, -0.04369665936638442), "water"),
        ((122, 60, 74, -0.04490159197647781), "water"),
        ((122, 80, 64, -0.37247525946547166), "air"),
        ((122, 80, 66, -0.3727266378002749), "air"),
        ((122, 80, 68, -0.36804742745663505), "air"),
        ((122, 80, 70, -0.3736723706537362), "air"),
        ((122, 80, 72, -0.3860951288334311), "air"),
        ((122, 80, 74, -0.3923721309133264), "air"),
        ((122, 100, 64, -0.4583333333333333), "air"),
        ((122, 100, 66, -0.4583333333333333), "air"),
        ((122, 100, 68, -0.4583333333333333), "air"),
        ((122, 100, 70, -0.4583333333333333), "air"),
        ((122, 100, 72, -0.4583333333333333), "air"),
        ((122, 100, 74, -0.4583333333333333), "air"),
        ((124, -100, 64, 0.037482421875), "none"),
        ((124, -100, 66, 0.037482421875), "none"),
        ((124, -100, 68, 0.037482421875), "none"),
        ((124, -100, 70, 0.037482421875), "none"),
        ((124, -100, 72, 0.037482421875), "none"),
        ((124, -100, 74, 0.037482421875), "none"),
        ((124, -80, 64, 0.037482421875), "none"),
        ((124, -80, 66, 0.037482421875), "none"),
        ((124, -80, 68, 0.037482421875), "none"),
        ((124, -80, 70, 0.037482421875), "none"),
        ((124, -80, 72, 0.037482421875), "none"),
        ((124, -80, 74, 0.037482421875), "none"),
        ((124, -60, 64, 0.05027131800090962), "none"),
        ((124, -60, 66, 0.0509584844305151), "none"),
        ((124, -60, 68, 0.05166719216129481), "none"),
        ((124, -60, 70, 0.05239749814810798), "none"),
        ((124, -60, 72, 0.053150241639753654), "none"),
        ((124, -60, 74, 0.053923063702221205), "none"),
        ((124, -40, 64, 0.1288961855391929), "none"),
        ((124, -40, 66, 0.13348604127036617), "none"),
        ((124, -40, 68, 0.13753360917037172), "none"),
        ((124, -40, 70, 0.1415440368478666), "none"),
        ((124, -40, 72, 0.1455395599824845), "none"),
        ((124, -40, 74, 0.1495006614184435), "none"),
        ((124, -20, 64, 0.12612367959767024), "none"),
        ((124, -20, 66, 0.12830319402402945), "none"),
        ((124, -20, 68, 0.1304960062341348), "none"),
        ((124, -20, 70, 0.1327707947453995), "none"),
        ((124, -20, 72, 0.13519345838131658), "none"),
        ((124, -20, 74, 0.13781110986582973), "none"),
        ((124, 0, 64, -0.009009809178163559), "water"),
        ((124, 0, 66, -0.007660237459532607), "water"),
        ((124, 0, 68, -0.0061446463166489424), "water"),
        ((124, 0, 70, -0.004442459854201204), "water"),
        ((124, 0, 72, -0.0025318494505197223), "water"),
        ((124, 20, 64, 0.06277697412858188), "none"),
        ((124, 20, 66, 0.06182400117210263), "none"),
        ((124, 20, 68, 0.060920470163534336), "none"),
        ((124, 20, 70, 0.06008166953195572), "none"),
        ((124, 20, 72, 0.05931163578048011), "none"),
        ((124, 20, 74, 0.05862261977236657), "none"),
        ((124, 40, 64, -0.004596793013348681), "none"),
        ((124, 40, 66, -0.007881293688553023), "none"),
        ((124, 40, 68, -0.010851313478373932), "none"),
        ((124, 40, 70, -0.013513605008223933), "water"),
        ((124, 40, 72, -0.015880866729427373), "water"),
        ((124, 40, 74, -0.017978317799117856), "water"),
        ((124, 60, 64, -0.033729060298063454), "water"),
        ((124, 60, 66, -0.04062740064249005), "water"),
        ((124, 60, 68, -0.03660634922712756), "water"),
        ((124, 60, 70, -0.04106936165998065), "water"),
        ((124, 60, 72, -0.048715160337165046), "water"),
        ((124, 60, 74, -0.053817378732386144), "water"),
        ((124, 80, 64, -0.378513110274629), "air"),
        ((124, 80, 66, -0.37887533037235366), "air"),
        ((124, 80, 68, -0.3755672366866089), "air"),
        ((124, 80, 70, -0.3806264904596738), "air"),
        ((124, 80, 72, -0.39139114552312176), "air"),
        ((124, 80, 74, -0.39905004304932734), "air"),
        ((124, 100, 64, -0.4583333333333333), "air"),
        ((124, 100, 66, -0.4583333333333333), "air"),
        ((124, 100, 68, -0.4583333333333333), "air"),
        ((124, 100, 70, -0.4583333333333333), "air"),
        ((124, 100, 72, -0.4583333333333333), "air"),
        ((124, 100, 74, -0.4583333333333333), "air"),
        ((126, -100, 64, 0.037482421875), "none"),
        ((126, -100, 66, 0.037482421875), "none"),
        ((126, -100, 68, 0.037482421875), "none"),
        ((126, -100, 70, 0.037482421875), "none"),
        ((126, -100, 72, 0.037482421875), "none"),
        ((126, -100, 74, 0.037482421875), "none"),
        ((126, -80, 64, 0.037482421875), "none"),
        ((126, -80, 66, 0.037482421875), "none"),
        ((126, -80, 68, 0.037482421875), "none"),
        ((126, -80, 70, 0.037482421875), "none"),
        ((126, -80, 72, 0.037482421875), "none"),
        ((126, -80, 74, 0.037482421875), "none"),
        ((126, -60, 64, 0.05024330978368198), "none"),
        ((126, -60, 66, 0.05093542123713047), "none"),
        ((126, -60, 68, 0.05164825745773993), "none"),
        ((126, -60, 70, 0.052380776473173525), "none"),
        ((126, -60, 72, 0.053133618951577574), "none"),
        ((126, -60, 74, 0.05390538149459787), "none"),
        ((126, -40, 64, 0.1308942633558083), "none"),
        ((126, -40, 66, 0.1355722846035891), "none"),
        ((126, -40, 68, 0.13977234300629068), "none"),
        ((126, -40, 70, 0.14391148475400958), "none"),
        ((126, -40, 72, 0.1480252139578508), "none"),
        ((126, -40, 74, 0.15208907413316627), "none"),
        ((126, -20, 64, 0.13001056656809973), "none"),
        ((126, -20, 66, 0.13213006749070327), "none"),
        ((126, -20, 68, 0.1342494739710899), "none"),
        ((126, -20, 70, 0.13641807828631622), "none"),
        ((126, -20, 72, 0.13869641277763253), "none"),
        ((126, -20, 74, 0.1411390651002113), "none"),
        ((126, 0, 64, -0.010355206926252296), "water"),
        ((126, 0, 66, -0.009043874021560911), "water"),
        ((126, 0, 68, -0.007576245457331987), "water"),
        ((126, 0, 70, -0.005915269354878528), "water"),
        ((126, 0, 72, -0.0040306175772153365), "water"),
        ((126, 0, 74, -0.0019328609472880898), "water"),
        ((126, 20, 64, 0.0700499260773044), "none"),
        ((126, 20, 66, 0.06908003164862005), "none"),
        ((126, 20, 68, 0.0681425614589177), "none"),
        ((126, 20, 70, 0.06723915989832648), "none"),
        ((126, 20, 72, 0.066359174495176), "none"),
        ((126, 20, 74, 0.06551116407146489), "none"),
        ((126, 40, 64, 0.004497597038868666), "none"),
        ((126, 40, 66, 0.0013502912778844962), "none"),
        ((126, 40, 68, -0.0015191728313191184), "water"),
        ((126, 40, 70, -0.0041188588354404134), "water"),
        ((126, 40, 72, -0.006463772144671846), "water"),
        ((126, 40, 74, -0.008581034519921562), "water"),
        ((126, 60, 64, -0.03471424652823008), "water"),
        ((126, 60, 66, -0.04732045558891548), "water"),
        ((126, 60, 68, -0.04568337003176991), "water"),
        ((126, 60, 70, -0.0428377824231183), "water"),
        ((126, 60, 72, -0.04738820166968918), "water"),
        ((126, 60, 74, -0.05663750895047857), "water"),
        ((126, 80, 64, -0.37931742687180287), "air"),
        ((126, 80, 66, -0.38265481838544235), "air"),
        ((126, 80, 68, -0.3808041835281554), "air"),
        ((126, 80, 70, -0.38160238129796925), "air"),
        ((126, 80, 72, -0.387746448733821), "air"),
        ((126, 80, 74, -0.3990668807989283), "air"),
        ((126, 100, 64, -0.4583333333333333), "air"),
        ((126, 100, 66, -0.4583333333333333), "air"),
        ((126, 100, 68, -0.4583333333333333), "air"),
        ((126, 100, 70, -0.4583333333333333), "air"),
        ((126, 100, 72, -0.4583333333333333), "air"),
        ((126, 100, 74, -0.4583333333333333), "air"),
    ];

    #[test]
    fn aquifer_vectors() {
        let _guard = lock_gen();
        let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
        let mut miss = 0u32;
        {
            let g = &mut g;
            for &((x, y, z), want) in AQ_NB {
                let got = g.aq_noise_level(x, y, z, 200);
                if got != want {
                    miss += 1;
                    eprintln!("nb {x},{y},{z}: {got} != {want}");
                }
            }
            for (tbl, my) in [(AQ_FYT, true), (AQ_FYF, false)] {
                for &((x, y, z), want) in tbl {
                    let got = g.aq_fluid_y(x, y, z, (0, 1), 80, my);
                    if got != want {
                        miss += 1;
                        eprintln!("fy[{my}] {x},{y},{z}: {got} != {want}");
                    }
                }
            }
            for &((x, y, z), want) in AQ_FBS {
                let got = g.aq_fluid_kind(x, y, z, (0, 1), -10);
                if got != want {
                    miss += 1;
                    eprintln!("fbs {x},{y},{z}: {got} != {want}");
                }
            }
            for &((x, y, z), (wy, wk)) in AQ_FL {
                let got = g.aq_fluid_level_at(x, y, z);
                if got != (wy, wk) {
                    miss += 1;
                    eprintln!("fl {x},{y},{z}: {got:?} != ({wy},{wk})");
                }
            }
            for &((x, y, z, h1, h2), want) in AQ_CD {
                let mut b = None;
                let got = g.aq_calc_density(x, y, z, (h1, 1), (h2, 1), &mut b);
                if (got - want).abs() > 1e-4 {
                    miss += 1;
                    eprintln!("cd {x},{y},{z},{h1},{h2}: {got} != {want}");
                }
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
            if got != want {
                miss += 1;
                eprintln!("ap {x},{y},{z}: {got:?} != {want:?}");
            }
        }
        assert_eq!(miss, 0, "aquifer vector mismatches");
    }

    #[test]
    fn aqdist() {
        let _guard = lock_gen();
        if std::env::var("WGAQ").is_err() {
            return;
        }
        assert!(init(0, crate::name_to_state));
        let (mut air, mut wat, mut lav, mut sol, mut cave) = (0, 0, 0, 0, 0);
        for cx in 0..4 {
            for cz in 0..4 {
                let _cc = chunk_cols(cx, cz).unwrap();
                for lx in 0..16 {
                    for lz in 0..16 {
                        for y in (-60..60).step_by(2) {
                            let d = density_at(cx * 16 + lx, y, cz * 16 + lz).unwrap_or(0.0);
                            if d <= 0.0 {
                                cave += 1;
                                match aquifer_state(cx * 16 + lx, y, cz * 16 + lz, d) {
                                    Some(0) => air += 1,
                                    Some(80) => wat += 1,
                                    Some(96) => lav += 1,
                                    Some(_) => sol += 1,
                                    None => sol += 1,
                                }
                            }
                        }
                    }
                }
            }
        }
        eprintln!("AqdIST caves={cave} air={air} water={wat} lava={lav} solid={sol}");
    }

    #[test]
    fn perf_probe() {
        let _guard = lock_gen();
        if std::env::var("WGPERF").is_err() {
            return;
        }
        assert!(init(0, crate::name_to_state));
        let t = std::time::Instant::now();
        let mut n = 0;
        for cx in 0..8 {
            for cz in 0..8 {
                chunk_cols(cx, cz).unwrap();
                n += 1;
            }
        }
        let el = t.elapsed().as_secs_f64() * 1000.0 / n as f64;
        eprintln!("PERF chunk_cols: {el:.2} ms/chunk ({n} chunks)");
        // aquifer+surface cell evaluation cost via deep_state on a few chunks
        let t = std::time::Instant::now();
        let mut m = 0;
        for cx in 0..4 {
            for cz in 0..4 {
                let cc = chunk_cols(cx, cz).unwrap();
                for y in (-64..64).step_by(2) {
                    for lx in 0..16 {
                        for lz in 0..16 {
                            deep_state(cx * 16 + lx, y, cz * 16 + lz, &cc);
                            m += 1;
                        }
                    }
                }
            }
        }
        eprintln!(
            "PERF deep_state: {:.4} ms/call",
            t.elapsed().as_secs_f64() * 1000.0 / m as f64
        );
    }

    /// The published immutable facts must equal the generator's own values: they are
    /// read without the world lock, so a divergence would be silent (and every
    /// dimension-aware reader would be wrong).
    #[test]
    fn published_gen_facts_match_the_generator() {
        let _guard = lock_gen();
        assert!(init_preset(0, crate::name_to_state, NoisePreset::End));
        let f = facts().expect("facts published at init");
        let (g_sea, g_min_y, g_height, g_def_st, g_def_fl, g_bedrock, preset) = with_gen(|g| {
            (
                g.sea,
                g.min_y,
                g.height,
                g.def_st,
                g.def_fl,
                g.bedrock_st,
                g.preset,
            )
        })
        .expect("generator");
        assert_eq!(
            (f.sea, f.min_y, f.height, f.def_st, f.def_fl, f.bedrock_st),
            (g_sea, g_min_y, g_height, g_def_st, g_def_fl, g_bedrock)
        );
        assert_eq!(f.preset, preset);
        assert_eq!(min_y(), f.min_y);
        assert_eq!(max_y(), f.min_y + f.height - 1);
        assert_eq!(sea_level(), f.sea);
        // leave the shared fixture in the Overworld for the other tests
        assert!(init(0, crate::name_to_state));
        assert_eq!(facts().expect("facts").preset, NoisePreset::Overworld);
    }

    /// There are **two** heights, from two files, and they are not interchangeable.
    ///
    /// `worldgen/noise_settings/<preset>.json` is the generator: terrain, density,
    /// aquifers, carvers, surface rules and the biome quart sweep all live in it, and
    /// 1.21.1 gives the Nether and the End `min_y 0 / height 128` there. `dimension_type/
    /// <dimension>.json` is the world: chunk sections, heightmaps, the dump length and
    /// structure/furniture bounds, and 1.21.1 gives the Nether and the End `0 / 256` there.
    /// The overworld agrees at -64/384, which is why one number looked sufficient.
    ///
    /// The numbers are asserted against the pack files themselves, not against a Rust
    /// constant: if the data changes the test follows it, and if the loader stops reading
    /// the file the two sides disagree.
    #[test]
    fn dimension_type_declares_a_world_window_containing_the_generator() {
        let _guard = lock_gen();
        for (preset, file, gen, world) in [
            (NoisePreset::Overworld, "overworld.json", (-64, 384), (-64, 384)),
            (NoisePreset::Nether, "the_nether.json", (0, 128), (0, 256)),
            (NoisePreset::End, "the_end.json", (0, 128), (0, 256)),
        ] {
            assert!(init_preset(0, crate::name_to_state, preset));
            let f = facts().expect("facts published at init");
            assert_eq!(
                (f.min_y, f.height),
                gen,
                "{preset:?}: the generator window comes from noise_settings"
            );
            assert_eq!(
                (f.world_min_y, f.world_height),
                world,
                "{preset:?}: the world window comes from dimension_type"
            );
            assert_eq!(
                (world_min_y(), world_height(), world_max_y()),
                (world.0, world.1, world.0 + world.1 - 1),
                "{preset:?}: the free functions must agree with the published facts"
            );
            // A world window that does not contain the generator's would cut terrain.
            assert!(f.world_min_y <= f.min_y, "{preset:?}");
            assert!(
                f.world_min_y + f.world_height >= f.min_y + f.height,
                "{preset:?}"
            );
            assert!(f.world_height >= f.height, "{preset:?}");
            // And the numbers are the pack's own, read the way the loader reads them.
            let path = std::path::Path::new("datapacks/vanilla/data/minecraft/dimension_type")
                .join(file);
            let raw = std::fs::read(&path)
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            let j = jparse(&raw).unwrap_or_else(|| panic!("{}: invalid JSON", path.display()));
            assert_eq!(
                (
                    j.get("min_y").map(J::num).map(|v| v as i32),
                    j.get("height").map(J::num).map(|v| v as i32)
                ),
                (Some(f.world_min_y), Some(f.world_height)),
                "{}: the loaded world window must be this file's min_y/height",
                path.display()
            );
        }
        // leave the shared fixture in the Overworld for the other tests
        assert!(init(0, crate::name_to_state));
    }

    /// The fallback that keeps every existing fixture's meaning: a pack without a usable
    /// `dimension_type` object leaves `world_* = gen_*`. "Unusable" is not only "absent":
    /// a window that is not whole sections, or that does not contain what the generator
    /// produces, would truncate terrain - refuse it and say so (the loader traces it),
    /// rather than defining a world smaller than the ground.
    #[test]
    fn dimension_window_falls_back_to_the_generator_when_the_object_is_unusable() {
        let parse = |s: &str| jparse(s.as_bytes()).expect("json");
        // absent fields (the file exists but is not a dimension_type)
        assert_eq!(dimension_window(&parse("{}"), 0, 128), None);
        // not whole sections
        assert_eq!(
            dimension_window(&parse(r#"{"min_y": 0, "height": 120}"#), 0, 128),
            None
        );
        assert_eq!(
            dimension_window(&parse(r#"{"min_y": 5, "height": 256}"#), 0, 128),
            None
        );
        // shorter than the generator's own terrain
        assert_eq!(
            dimension_window(&parse(r#"{"min_y": 0, "height": 96}"#), 0, 128),
            None
        );
        // starts above the generator
        assert_eq!(
            dimension_window(&parse(r#"{"min_y": 16, "height": 256}"#), 0, 128),
            None
        );
        // the three shapes 1.21.1 actually ships
        assert_eq!(
            dimension_window(&parse(r#"{"min_y": 0, "height": 256}"#), 0, 128),
            Some((0, 256))
        );
        assert_eq!(
            dimension_window(&parse(r#"{"min_y": -64, "height": 384}"#), -64, 384),
            Some((-64, 384))
        );
        // and an exact match is still a valid world window
        assert_eq!(
            dimension_window(&parse(r#"{"min_y": 0, "height": 128}"#), 0, 128),
            Some((0, 128))
        );
    }

    /// The dump, not only the fetch, follows the **world** window.
    ///
    /// `main.rs` dumps `worldgen::world_min_y()..=world_max_y()` in 16-block sections, and
    /// `chunk_triage` is told the same numbers, so "the window the port can produce" and
    /// "the window the comparison reads" cannot disagree. Above the generator top the
    /// window must hold nothing but air: that is what makes the Nether's 256 a storage
    /// property and the End's a content property.
    #[test]
    fn nether_and_end_dumps_cover_the_world_window_with_air_above_the_generator() {
        let _guard = lock_gen();
        let reg = crate::block_registry::breg().expect("block registry");
        for preset in [NoisePreset::Nether, NoisePreset::End] {
            assert!(init_preset(4242, crate::name_to_state, preset));
            let f = facts().expect("facts");
            let (lo, hi) = (world_min_y(), world_max_y());
            assert_eq!(lo, f.world_min_y);
            assert_eq!(hi, f.world_min_y + f.world_height - 1);
            // The dump's own loop: one 4096-cell section per 16 blocks.
            let sections = ((hi - lo + 1) / 16) as usize;
            assert_eq!(sections, f.world_height as usize / 16, "{preset:?}");
            assert_eq!(sections, 16, "{preset:?}: 1.21.1 stores 16 sections");
            let mut cells = 0usize;
            let mut above_gen = 0usize;
            let mut sy = lo;
            while sy <= hi {
                let arr = terrain_section(0, sy, 0, crate::feature::populate)
                    .expect("terrain section");
                cells += arr.len();
                if sy >= f.min_y + f.height {
                    above_gen += arr
                        .iter()
                        .filter(|st| **st != 0 && !reg.is_air(**st))
                        .count();
                }
                sy += 16;
            }
            assert_eq!(
                cells,
                sections * 4096,
                "{preset:?}: the dump length must be the declared window"
            );
            assert_eq!(
                above_gen, 0,
                "{preset:?}: the generator window's ceiling is the top of the terrain, so \
                 128..255 must be air in chunk (0,0)"
            );
        }
        // leave the shared fixture in the Overworld for the other tests
        assert!(init(0, crate::name_to_state));
    }

    /// Stage 2A: terrain is pure, so a scratch generator on the same `GenData` must
    /// reproduce the shared generator's columns exactly, for every worker count.
    /// If this fails, the "no feature write is read by `cols`" premise is wrong and
    /// terrain cannot be parallelised without changing the parity contract.
    #[test]
    fn scratch_terrain_matches_the_shared_generator_at_any_worker_count() {
        let _guard = lock_gen();
        assert!(init(0, crate::name_to_state));
        let data = data_snapshot().expect("data published");
        let keys: Vec<(i32, i32)> = (-1..=2)
            .flat_map(|x| (-1..=2).map(move |z| (x * 3, z * 3)))
            .collect();
        // reference: the shared generator's own cache
        let reference: Vec<u64> = keys
            .iter()
            .map(|&(cx, cz)| cols_fingerprint(&chunk_cols(cx, cz).expect("cols")))
            .collect();
        for workers in [1usize, 2, 4, 8] {
            let got = prefetch_cols(&data, &keys, workers);
            assert_eq!(got.len(), keys.len());
            for (i, (key, cols)) in got.iter().enumerate() {
                assert_eq!(key, &keys[i]);
                let cc = cols.as_ref().expect("prefetched cols");
                assert_eq!(
                    cols_fingerprint(cc),
                    reference[i],
                    "worker count {workers} diverged at {key:?}"
                );
            }
        }
        // installing them must not change what the shared cache answers
        let mut fresh = Gen::fresh_scratch(data.clone());
        let installed = prefetch_cols(&data, &keys, 4)
            .into_iter()
            .filter_map(|(k, c)| c.map(|c| (k, c)))
            .collect();
        fresh.install_cols(installed);
        for (i, &(cx, cz)) in keys.iter().enumerate() {
            let cc = fresh.cols(cx, cz).expect("cached cols");
            assert_eq!(cols_fingerprint(&cc), reference[i]);
        }
    }

    /// Terrain throughput by worker count. `FEAT_PROFILE`-gated, and not a server
    /// measurement: it isolates `cols` (no populate, no delivery, no lock).
    #[test]
    fn terrain_parallel_throughput() {
        let _guard = lock_gen();
        if !env_flag!("FEAT_PROFILE") {
            return;
        }
        assert!(init(0, crate::name_to_state));
        let data = data_snapshot().expect("data published");
        let keys: Vec<(i32, i32)> = (0..6)
            .flat_map(|x| (0..6).map(move |z| (x * 5 - 10, z * 5 - 10)))
            .collect();
        for workers in [1usize, 2, 4, 8] {
            let t = std::time::Instant::now();
            let got = prefetch_cols(&data, &keys, workers);
            let ms = t.elapsed().as_secs_f64() * 1e3;
            assert!(got.iter().all(|(_, c)| c.is_some()));
            eprintln!(
                "TERRAIN workers={workers} chunks={} {ms:.1}ms ({:.2} ms/chunk)",
                keys.len(),
                ms / keys.len() as f64
            );
        }
    }

    /// The split's purpose: a worker's inputs are readable while the world lock is
    /// held by a long generation step. The snapshot lock is taken for an `Arc` clone,
    /// so the read must not wait for the 300 ms hold below.
    #[test]
    fn immutable_inputs_do_not_queue_on_the_world_lock() {
        let _guard = lock_gen();
        assert!(init(0, crate::name_to_state));
        let snap = data_snapshot().expect("data published at init");
        let held = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let released = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        std::thread::scope(|s| {
            let held2 = held.clone();
            let released2 = released.clone();
            s.spawn(move || {
                // take the world lock and keep it, as a 13 ms-or-longer generate does
                let g = gen_lock();
                held2.store(true, std::sync::atomic::Ordering::SeqCst);
                while !released2.load(std::sync::atomic::Ordering::SeqCst) {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                drop(g);
            });
            while !held.load(std::sync::atomic::Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            let t = std::time::Instant::now();
            let again = data_snapshot().expect("snapshot");
            let sea = again.sea;
            let elapsed = t.elapsed();
            released.store(true, std::sync::atomic::Ordering::SeqCst);
            assert_eq!(sea, snap.sea);
            assert!(
                elapsed < std::time::Duration::from_millis(50),
                "snapshot waited {elapsed:?} behind the world lock"
            );
        });
    }

    /// Lock-wait split under contention: three threads, each doing the work a
    /// server thread does on the shared world lock (column generation, populate,
    /// and a terrain/feature read). `GENLOCK` reports wait vs hold, so the
    /// parallelization decision can separate "serialized work" from "queueing".
    ///
    /// Not a throughput claim: one host, one process, work chosen to hit the lock
    /// rather than to match `load_compare.sh`'s movement column.
    #[test]
    fn gen_lock_contention() {
        let _guard = lock_gen();
        if !env_flag!("FEAT_PROFILE") {
            return;
        }
        assert!(init(0, crate::name_to_state));
        report_gen_lock(); // clear whatever the fixtures above accumulated
        let t0 = std::time::Instant::now();
        std::thread::scope(|s| {
            s.spawn(|| {
                for cx in 0..6 {
                    for cz in 0..6 {
                        let _ = chunk_cols(cx, cz);
                    }
                }
            });
            s.spawn(|| {
                for cx in -1..=2 {
                    for cz in -1..=2 {
                        let _ =
                            feature_state(cx * 16 + 8, 64, cz * 16 + 8, crate::feature::populate);
                    }
                }
            });
            s.spawn(|| {
                for i in 0..200 {
                    let _ = biome_name_at_block(i % 64, 64, (i / 64) % 64);
                }
            });
        });
        eprintln!("GENLOCK wall={:.1}ms", t0.elapsed().as_secs_f64() * 1e3);
        report_gen_lock();
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
                if let Some(m) = gen_lock().as_ref().unwrap().feat_map(cx, cz) {
                    n += m.len();
                }
            }
        }
        eprintln!("PERF overrides/chunk-9x9: {}", n);
        crate::feature::feat_prof_dump();
    }

    /// The default dispatch is a *named member* of vanilla's order family, not a capture: the
    /// level-major priority queue with the serialized demand scan tie-break documented on
    /// [`population_discipline`]. This pinned list is that member for a one-chunk request set, and
    /// it is the whole contract of the default — vanilla fixes the rule, not the sequence, so no
    /// captured run is reproduced bit-for-bit here (`MC_SPINE_POP_ORDER` is how a comparison
    /// against one recorded order is made).
    ///
    /// Before this discipline the default was a lazy x-then-z 3x3, so this list is 9 entries long
    /// and in a different order; the golden is what turns that into a failure instead of a story.
    #[test]
    fn default_dispatch_runs_the_named_population_order() {
        let _guard = lock_gen();
        let mut g = Gen::load(0, crate::name_to_state).expect("datapack");
        let mut visited = Vec::new();
        let _ = g.feat_state(8, 64, 8, |_, x, z| visited.push((x, z)));
        assert_eq!(
            render_population_order(&visited),
            squash(&GOLDEN_DEFAULT_ORDER_1X1),
            "the default dispatch must run the named member, not a coordinate sort"
        );
        // A chunk the run already decorated is served from the cache: the discipline is a queue,
        // not a re-scan.
        let mut again = Vec::new();
        let _ = g.feat_state(8 + 16, 64, 8, |_, x, z| again.push((x, z)));
        assert_eq!(again, Vec::new(), "an already decorated chunk re-populated");
    }

    /// The same order for a 3x3 request box: 81 decorated chunks (9x9), and the tie-break that
    /// makes the member reproducible — request chunks are queued at level 31 in box order, so the
    /// min corner's window is the first batch.
    #[test]
    fn named_population_order_scales_to_a_three_by_three_request_box() {
        let _guard = lock_gen();
        let order = population_discipline_order(&request_box(0, 0, 2, 2));
        assert_eq!(
            render_population_order(&order),
            squash(&GOLDEN_DEFAULT_ORDER_3X3),
            "the named member's 3x3 request box order"
        );
    }

    /// Invariance 1, measured on all seven native captures: the decorated set is the request set
    /// plus three rings (1 chunk -> 7x7, 3x3 -> 9x9). The rule comes from the status the decorating
    /// step answers to: `FEATURES` is level 34, so demanders sit at levels 31..=33.
    #[test]
    fn population_discipline_decorates_the_request_set_plus_three_rings() {
        let _guard = lock_gen();
        for (x0, z0, x1, z1, want) in [(0, 0, 0, 0, 49), (-3, 5, -3, 5, 49), (0, 0, 2, 2, 81)] {
            let requests = request_box(x0, z0, x1, z1);
            let order = population_discipline_order(&requests);
            assert_eq!(order.len(), want, "decorated count for ({x0},{z0})..({x1},{z1})");
            let (min_x, max_x) = (order.iter().map(|c| c.0).min(), order.iter().map(|c| c.0).max());
            let (min_z, max_z) = (order.iter().map(|c| c.1).min(), order.iter().map(|c| c.1).max());
            assert_eq!(
                (min_x, max_x, min_z, max_z),
                (Some(x0 - 3), Some(x1 + 3), Some(z0 - 3), Some(z1 + 3)),
                "decorated span for ({x0},{z0})..({x1},{z1}) must be the request box + 3 rings"
            );
            let unique: HashSet<_> = order.iter().copied().collect();
            assert_eq!(unique.len(), order.len(), "a chunk was decorated twice");
        }
    }

    /// Invariance 2, measured on all seven native captures: the first decoration block is the
    /// x-then-z 3x3 at the request box's (min x, min z) corner. That block is the first demander's
    /// write window, which is why it is the only part identical across runs.
    #[test]
    fn population_discipline_first_block_is_the_min_corner_window() {
        let _guard = lock_gen();
        for (x0, z0, x1, z1) in [(0, 0, 0, 0), (7, -4, 9, -2), (-25, 19, -25, 19)] {
            let requests = request_box(x0, z0, x1, z1);
            let batches = population_discipline(&requests);
            let first = &batches[0];
            assert_eq!(first.level, POP_REQUEST_LEVEL, "the first batch is a request chunk");
            assert_eq!(first.demander, (x0, z0), "the first demander is the min corner");
            let want: Vec<(i32, i32)> = (x0 - 1..=x0 + 1)
                .flat_map(|x| (z0 - 1..=z0 + 1).map(move |z| (x, z)))
                .collect();
            assert_eq!(first.decorated, want, "first block is the x-then-z 3x3 window");
            assert_eq!(
                population_discipline_order(&requests)[..9].to_vec(),
                want,
                "the first nine decorated chunks are that window"
            );
        }
    }

    /// Invariance 3: the pops are level-major (lowest non-empty level first) and every chunk's
    /// level is the request set's Chebyshev distance + 31. The *decorated* sequence is not
    /// level-monotone — a batch's window reaches one ring further — which is exactly the
    /// staircase native shows (`[1,0,1,2,3]` runs over levels {0..3} for one chunk).
    #[test]
    fn population_discipline_pops_in_level_order() {
        let _guard = lock_gen();
        for (x0, z0, x1, z1) in [(0, 0, 0, 0), (0, 0, 2, 2), (-2, 6, 1, 9)] {
            let requests = request_box(x0, z0, x1, z1);
            let batches = population_discipline(&requests);
            let levels: Vec<i32> = batches.iter().map(|batch| batch.level).collect();
            assert!(
                levels.windows(2).all(|w| w[0] <= w[1]),
                "pops must be level-major: {levels:?}"
            );
            assert_eq!(*levels.last().unwrap(), POP_DECORATION_LEVEL - 1);
            for batch in &batches {
                let ring = requests
                    .iter()
                    .map(|r| (batch.demander.0 - r.0).abs().max((batch.demander.1 - r.1).abs()))
                    .min()
                    .unwrap();
                assert_eq!(
                    batch.level,
                    POP_REQUEST_LEVEL + ring,
                    "{:?} must sit at 31 + its Chebyshev ring",
                    batch.demander
                );
                assert!(
                    batch.level < POP_DECORATION_LEVEL,
                    "a chunk whose own target is FEATURES never demands"
                );
                assert!(!batch.decorated.is_empty(), "a demander decorates its window");
            }
            // Level-major does not mean the decorated list is level-sorted: the window of a
            // level-31 pop already reaches ring 1.
            let decorated_levels: Vec<i32> = population_discipline_order(&requests)
                .into_iter()
                .map(|c| {
                    POP_REQUEST_LEVEL
                        + requests
                            .iter()
                            .map(|r| (c.0 - r.0).abs().max((c.1 - r.1).abs()))
                            .min()
                            .unwrap()
                })
                .collect();
            assert!(
                decorated_levels.windows(2).any(|w| w[0] > w[1]),
                "the decorated sequence is a staircase, not a sorted list"
            );
        }
    }

    /// The named member for a one-chunk request set, rendered as `x,z` in decoration order.
    const GOLDEN_DEFAULT_ORDER_1X1: &str = "
        -1,-1 -1,0 -1,1 0,-1 0,0 0,1 1,-1 1,0
        1,1 -2,-2 -2,-1 -2,0 -1,-2 0,-2 -2,1 -2,2
        -1,2 0,2 1,-2 1,2 2,-2 2,-1 2,0 2,1
        2,2 -3,-3 -3,-2 -3,-1 -2,-3 -1,-3 -3,0 -3,1
        0,-3 1,-3 -3,2 -3,3 -2,3 -1,3 0,3 1,3
        2,-3 2,3 3,-3 3,-2 3,-1 3,0 3,1 3,2
        3,3
    ";

    /// The same member for the 3x3 request box `(0,0)..(2,2)` (81 decorated chunks).
    const GOLDEN_DEFAULT_ORDER_3X3: &str = "
        -1,-1 -1,0 -1,1 0,-1 0,0 0,1 1,-1 1,0
        1,1 -1,2 0,2 1,2 -1,3 0,3 1,3 2,-1
        2,0 2,1 2,2 2,3 3,-1 3,0 3,1 3,2
        3,3 -2,-2 -2,-1 -2,0 -1,-2 0,-2 -2,1 -2,2
        1,-2 2,-2 -2,3 -2,4 -1,4 0,4 1,4 2,4
        3,-2 3,4 4,-2 4,-1 4,0 4,1 4,2 4,3
        4,4 -3,-3 -3,-2 -3,-1 -2,-3 -1,-3 -3,0 -3,1
        0,-3 1,-3 -3,2 -3,3 2,-3 3,-3 -3,4 -3,5
        -2,5 -1,5 0,5 1,5 2,5 3,5 4,-3 4,5
        5,-3 5,-2 5,-1 5,0 5,1 5,2 5,3 5,4
        5,5
    ";

    fn render_population_order(order: &[(i32, i32)]) -> String {
        order
            .iter()
            .map(|(x, z)| format!("{x},{z}"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Compare a golden against a render without caring where the literal broke its lines.
    fn squash(text: &str) -> String {
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn default_dispatch_keeps_discipline_order_and_cached_overrides() {
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
        // Chunk (-1,-1)'s own 3x3 is the first pop's window, so the values are the ones the lazy
        // scan produced; the difference is the 40 chunks the discipline also decorates.
        assert_eq!(visited, population_discipline_order(&request_box(-1, -1, -1, -1)));
        assert_eq!(visited.len(), 49);
    }

    #[test]
    fn lazy_population_preserves_direct_feature_outputs() {
        let _guard = lock_gen();
        // Preserve population results through the default dispatch, including writes
        // outside the queried chunk. This is a refactor control, not a vanilla oracle.
        //
        // The compared quantity is the *queried* chunk's bucket. A chunk's cells are written
        // only by its own 3x3 (writes outside `POP_WRITE_RADIUS` are dropped), that 3x3 is the
        // first pop's window in the discipline, and no later pop decorates those chunks again —
        // so the equal-value claim is about that chunk, not about the 40 extra ring-2/ring-3
        // buckets the discipline also fills.
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
            let want = direct.feat_map(cx, cz).cloned();
            assert!(
                want.as_ref().is_some_and(|bucket| !bucket.is_empty()),
                "fixture must exercise real feature writes"
            );
            assert_eq!(
                lazy.feat_map(cx, cz).cloned(),
                want,
                "feature values differ at the queried chunk ({cx},{cz})"
            );
            assert_eq!(lazy.feat_state(x, 64, z, crate::feature::populate), first);
            assert_eq!(
                lazy.feat_map(cx, cz).cloned(),
                want,
                "cached lookup changed feature values"
            );
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
            for n in [x, y, z] {
                bytes.extend_from_slice(&n.to_le_bytes());
            }
            bytes.push(u8::from(result.is_some()));
            bytes.extend_from_slice(&result.unwrap_or(0).to_le_bytes());
            let mut rows: Vec<_> = g
                .feat_c
                .iter()
                .flat_map(|(&(cx, cz), map)| {
                    map.iter()
                        .map(move |(&index, &state)| (cx, cz, index, state))
                })
                .collect();
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
    fn beardifier_applies_before_surface_rules() {
        let _guard = lock_gen();
        // vanilla 1.21.1 orders `NoiseChunk.fillFromNoise` (density *with* Beardifier)
        // before `SurfaceSystem.buildSurface`, and `preliminarySurfaceLevel` samples the
        // BeardifierMarker (density *without* it). So `surf`/`wtop`/`mats` must see the
        // beardified density, while `prelim_min` must not.
        fn no_beard(_: &mut Gen, _: i32, _: i32) -> BeardChunkData {
            BeardChunkData::default()
        }
        // A `beard_thin` rigid at ground y=68 on column (4, 32) adds density below its
        // ground plane (`compute_beardifier`: `dy < 0` is positive) and carves above it,
        // so the unbeardified surface of 63 is pushed up towards 68 (measured: 67).
        fn raise_beard(g: &mut Gen, cx: i32, cz: i32) -> BeardChunkData {
            if (cx, cz) != (0, 2) {
                return BeardChunkData::default();
            }
            // A nested heightmap query runs while the resolution is in flight: it must
            // describe the un-beardified terrain and must not poison `col_c`.
            assert_eq!(
                g.top_oc(4, 32),
                64,
                "nested top_oc during resolve_beard must be un-beardified"
            );
            BeardChunkData {
                rigids: vec![BeardRigid {
                    min: [2, 68, 30],
                    max: [6, 72, 34],
                    adjustment: BeardAdjustment::BeardThin,
                    ground_level_delta: 0,
                }],
                junctions: Vec::new(),
            }
        }
        // Ground plane y=59: the 63 surface is carved down to 58, which is below the
        // aquifer level, so the carved cells 59..=62 flood (measured `wtop` = 62).
        // `wtop > surf` is what makes the surface rules see a *submerged* floor, giving
        // `dirt` rather than `grass_block`. The same beard evaluated after the surface
        // scan leaves `surf` at 63 and the assertion below fails (63 vs 63).
        fn carve_beard(g: &mut Gen, cx: i32, cz: i32) -> BeardChunkData {
            if (cx, cz) != (0, 2) {
                return BeardChunkData::default();
            }
            assert_eq!(
                g.top_oc(4, 32),
                64,
                "nested top_oc during resolve_beard must be un-beardified"
            );
            BeardChunkData {
                rigids: vec![BeardRigid {
                    min: [2, 59, 30],
                    max: [6, 65, 34],
                    adjustment: BeardAdjustment::BeardThin,
                    ground_level_delta: 0,
                }],
                junctions: Vec::new(),
            }
        }

        let grass = crate::name_to_state("minecraft:grass_block").unwrap();
        let dirt = crate::name_to_state("minecraft:dirt").unwrap();

        let mut g_plain = Gen::load(0, crate::name_to_state).unwrap();
        g_plain.set_beard_resolver(no_beard);
        let plain_cc = g_plain.cols(0, 2).unwrap();
        let plain_surf = plain_cc.surface(4, 0);
        assert_eq!(plain_surf, 63, "unbeardified surface of the check column");
        assert_eq!(plain_cc.wtop(4, 0), 63, "unbeardified column is dry");
        assert_eq!(plain_cc.mat(4, 0, 63), Some(grass));
        let plain_prelim = g_plain.prelim_min(4, 32);

        // Positive beard: the surface rises above y=63, and the *material rules* have to
        // follow it. Resolving the beard last would leave `mats` describing y=63, so the
        // raised column would read as bare stone.
        let mut g_raise = Gen::load(0, crate::name_to_state).unwrap();
        g_raise.set_beard_resolver(raise_beard);
        let raise_cc = g_raise.cols(0, 2).unwrap();
        assert!(cols_have_beard(&raise_cc));
        let raised_surf = raise_cc.surface(4, 0);
        assert!(
            raised_surf > plain_surf,
            "positive beard should raise the surface ({raised_surf} vs {plain_surf})"
        );
        assert_eq!(raise_cc.mat(4, 0, raised_surf), Some(grass));
        assert_eq!(
            raise_cc.wtop(4, 0),
            raised_surf,
            "the raised column stays above the aquifer: no water on top of it"
        );
        assert_eq!(g_raise.terrain_state(4, raised_surf, 32), grass);
        assert_eq!(g_raise.terrain_state(4, raised_surf - 1, 32), dirt);
        assert_eq!(g_raise.terrain_state(4, raised_surf + 1, 32), 0);
        // `preliminarySurfaceLevel` samples the BeardifierMarker: unaffected.
        assert_eq!(g_raise.prelim_min(4, 32), plain_prelim);

        // Negative beard: the surface is carved below the aquifer level, the carved
        // cells become water/air, and `wtop > surf` makes the exposed floor a submerged
        // floor, so the rule that applies is `dirt`, not `grass_block`.
        let mut g_carve = Gen::load(0, crate::name_to_state).unwrap();
        g_carve.set_beard_resolver(carve_beard);
        let carve_cc = g_carve.cols(0, 2).unwrap();
        assert!(cols_have_beard(&carve_cc));
        let carved_surf = carve_cc.surface(4, 0);
        assert!(
            carved_surf < plain_surf,
            "negative beard should carve the surface ({carved_surf} vs {plain_surf})"
        );
        assert!(
            carve_cc.wtop(4, 0) > carved_surf,
            "a carved-below-water column floods above its floor (wtop {} vs surf {carved_surf})",
            carve_cc.wtop(4, 0)
        );
        assert_eq!(g_carve.terrain_state(4, carved_surf, 32), dirt);
        for y in carved_surf + 1..=plain_surf {
            let st = g_carve.terrain_state(4, y, 32);
            let solid = st != 0 && st != g_carve.def_fl;
            assert!(
                !solid,
                "carved cell y={y} must not stay solid (state {st})"
            );
        }
        assert_eq!(g_carve.prelim_min(4, 32), plain_prelim);
    }

    #[test]
    fn ordered_population_claims_once_without_reordering_callbacks() {
        let mut g = Gen::load(0, crate::name_to_state).unwrap();
        let mut seen = Vec::new();
        let count = g.populate_in_order([(2, 1), (-3, 4), (2, 1)], |g, x, z| {
            assert!(g.pop_done.contains(&(x, z)), "claim precedes callback");
            seen.push((x, z));
        });
        assert_eq!(seen, [(2, 1), (-3, 4)]);
        assert_eq!(count, 2);
        assert_eq!(
            g.populate_in_order([(2, 1), (-3, 4)], |_, _, _| panic!("already populated")),
            0
        );
    }

    /// `ChunkMap.protoChunkToFullChunk` promotes a chunk only once the whole 3x3 around
    /// it is `ChunkStatus.FULL`, and `LevelChunk.postProcessGeneration` consumes the
    /// chunk's shape marks right there. So a mark waits for every neighbour, and the list
    /// is consumed once — a mark that arrives afterwards is never applied, which is the
    /// stale connection bit native keeps on a fence written into an already-promoted chunk.
    #[test]
    fn shape_marks_wait_for_the_whole_three_by_three_and_are_consumed_once() {
        let mut g = Gen::load(4242, crate::name_to_state).unwrap();
        g.store_shape_mark(-1521, -21, 560);
        assert_eq!(g.take_shape_marks(-97, 35), Vec::new(), "filed under its own chunk");
        assert!(!g.shape_marks_ready(-96, 35), "nothing has generated yet");
        let mut order: Vec<(i32, i32)> = Vec::new();
        for dx in -1..=1 {
            for dz in -1..=1 {
                if (dx, dz) != (0, 0) {
                    order.push((-96 + dx, 35 + dz));
                }
            }
        }
        for (i, chunk) in order.iter().copied().enumerate() {
            assert_eq!(g.populate_in_order([chunk], |_, _, _| {}), 1);
            assert!(
                !g.shape_marks_ready(-96, 35),
                "still waiting for a chunk after {} of 8",
                i + 1
            );
        }
        assert_eq!(g.populate_in_order([(-96, 35)], |_, _, _| {}), 1);
        assert!(g.shape_marks_ready(-96, 35), "the whole 3x3 has generated");
        assert_eq!(g.take_shape_marks(-96, 35), vec![(-1521, -21, 560)]);
        assert!(g.take_shape_marks(-96, 35).is_empty(), "consumed once");
    }

    #[test]
    fn population_plan_preserves_order_and_rejects_incomplete_or_dropped_inputs() {
        let wanted: Vec<_> = (-1..=1)
            .rev()
            .flat_map(|x| (-1..=1).rev().map(move |z| (x, z)))
            .collect();
        let text: String = wanted.iter().map(|(x, z)| format!("{x}\t{z}\n")).collect();
        assert_eq!(
            parse_population_order(&text, 0, 0, 1, false).unwrap(),
            wanted
        );
        for input in ["", "0 0\n0 0\n", "0 0\n1\n", "unknown 0\n", "9 9\n"] {
            assert!(
                parse_population_order(input, 0, 0, 1, false).is_err(),
                "must reject {input:?}"
            );
        }
        assert!(parse_population_order(&text, 0, 0, 0, false).is_err());
        assert!(parse_population_order(&text, i32::MIN, 0, 1, false).is_err());
    }

    #[test]
    fn recorded_population_plan_preserves_extras_without_hiding_missing_sources() {
        let core: Vec<_> = (-1..=1)
            .flat_map(|x| (-1..=1).map(move |z| (x, z)))
            .collect();
        let mut wanted = vec![(9, 9)];
        wanted.extend_from_slice(&core);
        wanted.push((-20, 3));
        let text: String = wanted.iter().map(|(x, z)| format!("{x}\t{z}\n")).collect();
        assert_eq!(
            parse_population_order(&text, 0, 0, 1, true).unwrap(),
            wanted
        );
        assert!(parse_population_order(&text, 0, 0, 1, false).is_err());
        // Nine entries is not nine required sources: an extra cannot replace a core cell.
        let missing: String = wanted[..core.len()]
            .iter()
            .map(|(x, z)| format!("{x} {z}\n"))
            .collect();
        assert!(parse_population_order(&missing, 0, 0, 1, true).is_err());
        assert!(parse_population_order(&(text.clone() + "9 9\n"), 0, 0, 1, true).is_err());
        assert!(parse_population_order(&(text + "bad extra\n"), 0, 0, 1, true).is_err());
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
        // 2026-10-26: features now read carver writes from the feature layer.
        // Without that, an ore feature saw uncarved stone and placed lapis into
        // reference cave air; the isolated lapis fixture moved 28/23 -> 23/23.
        // The capture moves with the corrected stage order; the remaining forest
        // residual is still features/structures, not terrain or carvers.
        // 2026-10-26: `TreeFeature` now rejects a clipped tree without a
        // `min_clipped_height`, as the pinned class does. The one-feature forest
        // fixture moved from 47 port origins (26 shared) to 41/41 all shared, and
        // its feature draw stream matches the reference after the port's lazy
        // aquifer samples. The capture moves with that native stage order.
        // 2026-10-26: the tree parser reads `dirt_provider` (the pinned
        // TreeConfiguration field), so trunk placers set the base dirt; the
        // target chunk's last 5 cells (grass vs dirt) became 0.
        // 2026-10-26: fancy trunk attachments start at `start + (height - 3)`,
        // not `start + k`; the latter moved every fancy canopy up. The 3x3 diff
        // moved 728 -> 319 while the target chunk stayed at 0.
        // 2026-10-26: the centre fancy limb is `h2 * 0.5` (vanilla assigns h2
        // then returns half); the port returned h2, doubling the centre limb.
        // The 3x3 diff moved 72 -> 51; the remainder is leaf distance.
        // 2026-10-26: carvers for the 3x3 decoration area are claimed before
        // features, so a feature can read a neighbour's carver air. This is
        // vanilla's CARVERS -> FEATURES order; the dungeon that required a
        // hole in chunk z+1 now places, and the full-feature target moved
        // 382 -> 114.
        // OreFeature's discard=1 still places buried ore (no RNG draw before
        // checking neighbours). Native decision/RNG vectors pin the correction;
        // forest target no-tick differences fall 53 -> 30, all 23 changed cells
        // becoming matching diamond/lapis ore. See forest-r1/ore-air.md.
        // Frozen WG heights, normal noise, wrapped dungeon RNG, fluid predicates
        // and multiface spreading have independent native fixtures.
        // 2026-10-27: leaf distances now follow actual TreeFeature buckets,
        // including Java HashSet traversal, rather than global shortest paths.
        // 117 native leaf cases and 15 actual HashSet operation cases pin that
        // behavior. With explicit source order, the production dump matches all
        // 884736 cells of the paused native 3x3, without property exclusions.
        // Saved-world runtime differences remain separate: feature-contracts.md.
        // 2026-11-15: the WG heightmaps are now frozen at the end of the carver pass
        // instead of at the surface. `ProtoChunk.setBlockState` updates
        // `getStatus().heightmapsAfter()`, and the status is still SURFACE while the
        // carver runs (the new status is set when the stage body returns), so a carve
        // lowers WORLD_SURFACE_WG/OCEAN_FLOOR_WG and the FEATURES status afterwards
        // leaves them alone. Features and structures read that map
        // (`HeightmapFilter`/`world_surface_wg` placements, `WorldGenRegion.getHeight`),
        // so they now see the carved top. Seed 4242 chunk (6,7) against the native
        // chunk: `minecraft:ruined_portal`'s netherrack base read 110 at (107,122)
        // instead of vanilla's 100, which shifted the whole weighted base/column RNG
        // stream - 124 netherrack/dirt/stone cells of the portal differed (chunk total
        // 278 -> 154, the remainder the amethyst geode). The capture moves with the
        // corrected input.
        // 2026-11-16: a tree's trunk dirt now joins the tree's *log* list, and the three
        // decorator lists are handed over in `TreeDecorator.Context`'s order (HashSet
        // bucket order, then a stable sort by y). `TrunkPlacer.setDirtUnderneath` writes
        // through the same `BiConsumer` as `placeLog` (`ehm.a`), so vanilla's `logs` set
        // holds the dirt position too; `trunk_vine` draws four `nextInt(3)` per entry, so
        // the missing entry shortened the stream and shifted every later decorator. The
        // list order matters because `leave_vine` draws four `nextFloat` per leaf.
        // Measured on the pinned jungle fixture (`/tmp/nat-4242-jungle-temple/world`,
        // chunk (74,-41) against the recorded population order): 732 -> 489 differing
        // cells with the dirt entry, 489 -> 145 with the list order, and the live
        // OCEAN_FLOOR / WORLD_SURFACE columns against the saved chunk 77 -> 14 and
        // 89 -> 18. The visible blocks of this capture's own fixture are unchanged
        // ((-2,-2)+3x3 564 -> 564, (0,0)+2x2 11 -> 11, (-1,1)+2x2 0 -> 0), so the digest
        // moves with the captured *write set* in the chunks those windows do not cover.
        // 2026-11-18: the default population dispatch is now vanilla's order *discipline* — the
        // level-major priority queue with the serialized demand scan tie-break
        // (`population_discipline`) — instead of a lazy x-then-z 3x3. This capture digests every
        // `feat_c` bucket and the whole `pop_done` set, and the discipline decorates the request
        // set plus three rings (49 chunks for a one-chunk request instead of 9), so the digest
        // moves with the write set of the two extra rings. The *visible* blocks are unchanged:
        // a chunk's cells are written only by its own 3x3 (writes beyond `POP_WRITE_RADIUS` are
        // dropped), that square is the first pop's window in x-then-z — the same nine chunks the
        // lazy scan ran, in the same order — and no later pop decorates them again
        // (`lazy_population_preserves_direct_feature_outputs` and
        // `default_dispatch_keeps_discipline_order_and_cached_overrides` pin both halves).
        assert_eq!(
            digest, "dcfa9969316c5721ef37715af66240b4",
            "feature population output changed from the native feature-contract capture"
        );
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
            assert_eq!(
                crate::json::jstr(&j, "type").as_deref(),
                Some(ty),
                "{name}: type"
            );
            let cfg = crate::json::js(&j, "config").unwrap_or_else(|| panic!("{name}: no config"));
            for key in ["probability", "y", "yScale", "lava_level", "replaceable"] {
                assert!(
                    crate::json::js(cfg, key).is_some(),
                    "{name}: config lacks {key}"
                );
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
                Some((
                    it.next()?.parse().ok()?,
                    it.next()?.parse().ok()?,
                    it.next()?.parse().ok()?,
                ))
            })
            .collect();
        want.sort();
        let got = crate::feature::ore_cells_for_test(60007, (7, 49, 13), 64);
        assert_eq!(
            got.len(),
            want.len(),
            "cell count differs from vanilla's 666"
        );
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
            assert_eq!(
                first, second,
                "the shared cache changed the leaf for {qx},{qy},{qz}"
            );
            let direct = g
                .biome_quart(qx, qy, qz, None)
                .map(|(_, l)| l)
                .unwrap_or(u16::MAX);
            assert_eq!(
                second, direct,
                "the shared cache disagrees with a direct lookup at {qx},{qy},{qz}"
            );
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
                                k,
                                (k >> 8) as i32 - 64,
                                k & 15,
                                (k >> 4) & 15,
                                v,
                                other
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
        eprintln!(
            "feat_determinism: own={} neighbor-overrides={}",
            m1.len(),
            neigh
        );
        assert!(m1.len() + neigh > 0, "no features generated at all");
    }

    // ---- carve-stage lifecycle -----------------------------------------------------

    /// Everything the block layer holds for one chunk, in a stable order, so two runs of
    /// the same stage on the same inputs compare as data.
    fn chunk_override_digest(g: &Gen, cx: i32, cz: i32) -> (usize, u64) {
        let mut cells: Vec<(i32, i32, i32, u32)> = Vec::new();
        for lx in 0..16 {
            for lz in 0..16 {
                for y in g.min_y..g.min_y + g.height {
                    let (x, z) = (cx * 16 + lx, cz * 16 + lz);
                    if let Some(st) = g.feature_override(x, y, z) {
                        cells.push((x, y, z, st));
                    }
                }
            }
        }
        cells.sort();
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for (x, y, z, st) in &cells {
            for v in [*x as u64, *y as u64, *z as u64, *st as u64] {
                h ^= v;
                h = h.wrapping_mul(0x100_0000_01b3);
            }
        }
        (cells.len(), h)
    }

    /// Turn `feat_c`'s own bound far enough that an old bucket is dropped: `feat_write`
    /// is the entry both the carver and the feature layer flush through, and `feat_state`
    /// is where that bound runs. The driver is a no-op so this stays a cache test.
    fn evict_feature_buckets(g: &mut Gen, n: i32) {
        for i in 0..n {
            g.feat_write(1_000_000 + i * 16, 60, 1_000_000, 1);
        }
        let _ = g.feat_state(1_000_000, 60, 1_000_000, |_, _, _| {});
    }

    /// The carver's writes and the latch that says "this chunk is carved" are one stage.
    /// When the override cache drops the writes, the latch has to go with them, because
    /// the only other way to get them back is the pass the latch suppresses.
    #[test]
    fn carve_output_and_its_latch_share_one_lifetime() {
        let mut g = Gen::load(4242, crate::name_to_state).expect("datapack");
        // The real FEATURES entry point on the real schedule: chunk (6,7) alone. Its 3x3
        // is carved but not populated, exactly as `feature::populate` does it.
        assert_eq!(
            g.populate_in_order([(6, 7)], crate::feature::populate),
            1,
            "the first pass must run"
        );
        let first = chunk_override_digest(&g, 6, 7);
        let first_wg = g.wg_after_carve_height(false, 6 * 16, 7 * 16);
        assert!(first.0 > 0, "the first pass wrote no block output");
        evict_feature_buckets(&mut g, 400);
        assert_eq!(
            chunk_override_digest(&g, 6, 7),
            (0, 0xcbf2_9ce4_8422_2325),
            "the 384 bound really dropped this chunk's block output"
        );
        // Same entry point, same schedule, same inputs: the stage reproduces itself.
        let reran = g.populate_in_order([(6, 7)], crate::feature::populate);
        let second = chunk_override_digest(&g, 6, 7);
        let second_wg = g.wg_after_carve_height(false, 6 * 16, 7 * 16);
        eprintln!(
            "FACTS carve: first=(n={} h={:016x}) reran={reran} second=(n={} h={:016x}) \
             wg={first_wg:?}->{second_wg:?}",
            first.0, first.1, second.0, second.1
        );
        assert_eq!(reran, 1, "the chunk re-claimed its FEATURES pass");
        assert_eq!(
            second, first,
            "re-running the FEATURES stage under the same inputs and schedule must \
             reproduce it; the first pass's terrain included the carver's writes, so a \
             second pass that no longer sees them cannot"
        );
        assert_eq!(
            second_wg, first_wg,
            "the post-carve WG value is a scan of the same terrain, so the re-run must \
             derive the same heights"
        );
    }

    /// The approved retention is 4096 post-carve WG chunks and a 384-chunk feature
    /// cache. They are different outputs with different lifetimes: dropping the block
    /// bucket must not shrink the WG retention to 384 (the value is not re-readable from
    /// a dropped bucket), but it must drop the claim that stood for those blocks.
    #[test]
    fn post_carve_wg_outlives_the_feature_cache_but_not_its_claim() {
        let mut g = Gen::load(4242, crate::name_to_state).expect("datapack");
        assert!(g.begin_carve(0, 0), "the real order: claim, then record");
        let mut tops = Box::new([0i32; 512]);
        tops[0] = 71;
        g.set_wg_after_carve(0, 0, tops);
        // A carved cell in this chunk's own bucket, so the 384 bound has something of
        // this chunk's to drop.
        g.feat_write(0, 71, 0, 1);
        assert!(g.feature_override(0, 71, 0).is_some());
        evict_feature_buckets(&mut g, 400);
        assert_eq!(
            g.feature_override(0, 71, 0),
            None,
            "the 384 bound really dropped this chunk's block output"
        );
        assert_eq!(
            g.wg_after_carve_height(false, 0, 0),
            Some(71),
            "the 4096 WG retention must not shrink to the 384 feature bound"
        );
        assert!(
            g.begin_carve(0, 0),
            "a claim whose block output was evicted must not survive it: the next \
             generation has to re-run the pass"
        );
    }

    /// Past 4096 chunks the post-carve value is really evicted. The record holds the
    /// value and the claim, so the eviction takes both: an absent value never leaves a
    /// claim behind that could not re-derive it, and the stage can be established again.
    #[test]
    fn the_wg_retention_bound_drops_the_value_and_the_claim_together() {
        let mut g = Gen::load(4242, crate::name_to_state).expect("datapack");
        assert!(g.begin_carve(0, 0));
        let mut tops = Box::new([0i32; 512]);
        tops[0] = 71;
        g.set_wg_after_carve(0, 0, tops);
        g.feat_write(0, 71, 0, 1);
        assert_eq!(g.wg_after_carve_height(false, 0, 0), Some(71));
        for i in 1..=4100 {
            assert!(g.begin_carve(i, 0), "chunk {i} has its own pass");
            g.set_wg_after_carve(i, 0, Box::new([64i32; 512]));
        }
        assert_eq!(
            g.wg_after_carve_height(false, 0, 0),
            None,
            "the 4096 bound must actually evict, not grow"
        );
        assert!(
            g.begin_carve(0, 0),
            "an evicted value must not leave a claim behind: that claim is what made \
             the value unrecoverable"
        );
        assert_eq!(
            g.feature_override(0, 71, 0),
            None,
            "the record's block output goes with the record, not into a second owner"
        );
        // The stage can be re-established from the same inputs, which is the only
        // contract an evicted value leaves behind.
        let mut again = Box::new([0i32; 512]);
        again[0] = 71;
        g.set_wg_after_carve(0, 0, again);
        assert_eq!(g.wg_after_carve_height(false, 0, 0), Some(71));
    }

    /// Claiming, recording, evicting and re-inserting the same chunks must not grow a
    /// queue, duplicate a slot or let the queue and the map disagree.
    #[test]
    fn repeated_stage_updates_keep_one_slot_per_chunk() {
        let mut g = Gen::load(4242, crate::name_to_state).expect("datapack");
        for round in 0..600i32 {
            let (cx, cz) = (round % 7, round % 5);
            g.begin_carve(cx, cz);
            g.set_wg_after_carve(cx, cz, Box::new([60 + round % 3; 512]));
            g.feat_write(cx * 16 + 1, 60, cz * 16 + 1, 1);
            g.feat_write(1_000_000 + round * 16, 60, 1_000_000, 1);
            let _ = g.feat_state(1_000_000, 60, 1_000_000, |_, _, _| {});
            let mut ord = g.feat_ord.clone();
            ord.sort();
            assert!(
                ord.windows(2).all(|w| w[0] != w[1]),
                "round {round}: duplicate feat_ord slot"
            );
            assert_eq!(
                ord.len(),
                g.feat_c.len(),
                "round {round}: feat_ord must list exactly the buckets feat_c holds"
            );
            assert!(g.feat_ord.len() <= 384, "round {round}: bucket bound");
        }
        // Every subject chunk was written in the last rounds, so all of them are still
        // resident and keep their claim.
        let subjects: Vec<(i32, i32)> = (0..7).flat_map(|cx| (0..5).map(move |cz| (cx, cz))).collect();
        for &(cx, cz) in &subjects {
            assert!(
                g.feature_override(cx * 16 + 1, 60, cz * 16 + 1).is_some(),
                "{cx},{cz}: still resident"
            );
            assert!(!g.begin_carve(cx, cz), "a live bucket keeps its claim");
        }
        // Now turn every one of them over. A claim may not outlive the bucket it speaks
        // for: the next generation has to re-run the pass instead of reading the
        // uncarved terrain.
        evict_feature_buckets(&mut g, 500);
        for &(cx, cz) in &subjects {
            assert_eq!(
                g.feature_override(cx * 16 + 1, 60, cz * 16 + 1),
                None,
                "{cx},{cz}: the bucket turned over"
            );
            assert!(g.begin_carve(cx, cz), "a dropped bucket loses its claim");
        }
    }

    /// Evicting block output is not a reason to lose runtime work: a staged generation
    /// payload belongs to the runtime store, not to the cache that is being dropped.
    #[test]
    fn evicting_block_output_keeps_the_staged_payload() {
        let mut g = Gen::load(4242, crate::name_to_state).expect("datapack");
        let mut items = vec![(
            (0, 60, 0),
            crate::generation_payload::GenerationPayload::Sign(std::array::from_fn(|_| String::new())),
        )];
        g.stage_payloads((0, 0), &mut items);
        assert!(items.is_empty(), "the staging call takes the payloads");
        g.feat_write(0, 60, 0, 1);
        evict_feature_buckets(&mut g, 400);
        let taken = g.take_staged_payloads();
        assert_eq!(
            taken.len(),
            1,
            "dropping evicted blocks must not drop the pending runtime payload"
        );
        assert_eq!(taken[0].0, (0, 60, 0));
        // Recomputing the evicted source must not stage it a second time.
        let mut again = vec![(
            (0, 60, 0),
            crate::generation_payload::GenerationPayload::Sign(std::array::from_fn(|_| String::new())),
        )];
        g.stage_payloads((0, 0), &mut again);
        assert!(again.is_empty(), "a regenerated chunk keeps its payload source");
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
        eprintln!(
            "BEDROCK_BENCH cells={n} elapsed={elapsed:?} per_cell={:?} acc={acc}",
            elapsed / n as u32
        );
        // Whole-chunk context: how much is the bedrock band against full generation?
        let mut cg = super::Gen::load(0, crate::name_to_state).expect("datapack");
        let t = std::time::Instant::now();
        let mut chunks = 0u32;
        for cx in 0..8i32 {
            for cz in 0..8i32 {
                if cg.cols(cx, cz).is_some() {
                    chunks += 1;
                }
            }
        }
        eprintln!(
            "CHUNK_BENCH chunks={chunks} elapsed={:?} per_chunk={:?}",
            t.elapsed(),
            t.elapsed() / chunks
        );
    }
}

#[cfg(test)]
#[path = "weird_sampler_tests.rs"]
mod weird_sampler_tests;

#[cfg(test)]
#[path = "aquifer_native_tests.rs"]
mod aquifer_native_tests;

