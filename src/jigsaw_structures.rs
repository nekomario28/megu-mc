// Vanilla 1.21.1 `JigsawStructure` / `JigsawPlacement` pool expansion and
// `PoolElementStructurePiece` placement kernel, matched against the native
// 1.21.1 server oracle across all 10 `minecraft:jigsaw` structures.

use crate::nbt::Tag;
use crate::structure_template::{BlockInfo, Template};
use crate::structure_transform::{block as transform_block, bounds as transform_bounds, state as transform_state, Mirror, Rotation};
use std::collections::{BTreeMap, VecDeque};
#[cfg(test)]
use std::path::Path;

type Pos = (i32, i32, i32);
type BBox = [i32; 6]; // [min_x, min_y, min_z, max_x, max_y, max_z], inclusive

const MIN_Y: i32 = -64;
const WORLD_HEIGHT: i32 = 384;
const MAX_Y_EXCL: i32 = MIN_Y + WORLD_HEIGHT; // 320

#[cfg(test)]
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn j_num(j: &J) -> Option<f64> {
    match j {
        J::Num(n) => Some(*n),
        _ => None,
    }
}

fn j_bool(j: &J) -> Option<bool> {
    match j {
        J::Num(n) => Some(*n != 0.0),
        _ => None,
    }
}

fn j_obj(j: &J) -> Option<&J> {
    match j {
        J::Obj(_) => Some(j),
        _ => None,
    }
}

// ---------- Datapack schemas & strict parsers ----------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LiquidSettings {
    IgnoreWaterlogging = 0,
    ApplyWaterlogging = 1,
}

impl LiquidSettings {
    fn parse(raw: &str) -> Result<Self, String> {
        match raw {
            "ignore_waterlogging" => Ok(Self::IgnoreWaterlogging),
            "apply_waterlogging" => Ok(Self::ApplyWaterlogging),
            other => Err(format!("unknown liquid_settings: {other}")),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Projection {
    Rigid,
    TerrainMatching,
}

impl Projection {
    fn parse(raw: &str) -> Result<Self, String> {
        match raw {
            "rigid" => Ok(Self::Rigid),
            "terrain_matching" => Ok(Self::TerrainMatching),
            other => Err(format!("unknown projection: {other}")),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum HeightProvider {
    Constant(i32),
    Uniform { min: i32, max: i32 },
}

impl HeightProvider {
    fn parse_vertical_anchor(v: &J) -> Result<i32, String> {
        let obj = j_obj(v).ok_or("vertical anchor must be object")?;
        if let Some(val) = obj.get("absolute") {
            let n = j_num(val).ok_or("absolute anchor must be number")?;
            return Ok(n as i32);
        }
        if let Some(val) = obj.get("above_bottom") {
            let n = j_num(val).ok_or("above_bottom anchor must be number")?;
            return Ok(MIN_Y + n as i32);
        }
        if let Some(val) = obj.get("below_top") {
            let n = j_num(val).ok_or("below_top anchor must be number")?;
            return Ok(MAX_Y_EXCL - 1 - n as i32);
        }
        Err("unsupported vertical anchor".into())
    }

    fn parse(v: &J) -> Result<Self, String> {
        let obj = j_obj(v).ok_or("start_height must be object")?;
        if let Some(ty) = obj.get("type").map(J::str) {
            match ty {
                "minecraft:constant" | "constant" => {
                    let val = obj.get("value").ok_or("missing constant start_height.value")?;
                    Ok(Self::Constant(Self::parse_vertical_anchor(val)?))
                }
                "minecraft:uniform" | "uniform" => {
                    let min = Self::parse_vertical_anchor(
                        obj.get("min_inclusive").ok_or("missing uniform min_inclusive")?,
                    )?;
                    let max = Self::parse_vertical_anchor(
                        obj.get("max_inclusive").ok_or("missing uniform max_inclusive")?,
                    )?;
                    Ok(Self::Uniform { min, max })
                }
                other => Err(format!("unsupported height_provider type: {other}")),
            }
        } else {
            Ok(Self::Constant(Self::parse_vertical_anchor(v)?))
        }
    }

    fn sample(&self, rng: &mut Lcg) -> i32 {
        match *self {
            Self::Constant(v) => v,
            Self::Uniform { min, max } => {
                if min >= max {
                    min
                } else {
                    min + rng.next_bound(max - min + 1)
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum PoolAliasBinding {
    Direct {
        alias: String,
        target: String,
    },
    Random {
        alias: String,
        targets: Vec<(i32, String)>,
    },
    RandomGroup {
        groups: Vec<(i32, Vec<PoolAliasBinding>)>,
    },
}

impl PoolAliasBinding {
    fn parse(v: &J) -> Result<Self, String> {
        let ty = v.get("type").map(J::str).ok_or("missing pool alias type")?;
        match ty {
            "minecraft:direct" => {
                let alias = ns_name(v.get("alias").and_then(|j| match j {
                    J::Str(s) => Some(s.as_str()),
                    _ => None,
                }).ok_or("direct alias missing alias")?);
                let target = ns_name(v.get("target").and_then(|j| match j {
                    J::Str(s) => Some(s.as_str()),
                    _ => None,
                }).ok_or("direct alias missing target")?);
                Ok(Self::Direct { alias, target })
            }
            "minecraft:random" => {
                let alias = ns_name(v.get("alias").and_then(|j| match j {
                    J::Str(s) => Some(s.as_str()),
                    _ => None,
                }).ok_or("random alias missing alias")?);
                let arr = v
                    .get("targets")
                    .and_then(J::as_arr)
                    .ok_or("random alias missing targets")?;
                let mut targets = Vec::with_capacity(arr.len());
                for entry in arr {
                    let weight = entry
                        .get("weight")
                        .and_then(j_num)
                        .ok_or("random alias entry missing weight")? as i32;
                    let data = entry
                        .get("data")
                        .and_then(|j| match j {
                            J::Str(s) => Some(ns_name(s)),
                            _ => None,
                        })
                        .ok_or("random alias entry missing data")?;
                    targets.push((weight, data));
                }
                Ok(Self::Random { alias, targets })
            }
            "minecraft:random_group" => {
                let arr = v
                    .get("groups")
                    .and_then(J::as_arr)
                    .ok_or("random_group missing groups")?;
                let mut groups = Vec::with_capacity(arr.len());
                for entry in arr {
                    let weight = entry
                        .get("weight")
                        .and_then(j_num)
                        .ok_or("random_group entry missing weight")? as i32;
                    let data_arr = entry
                        .get("data")
                        .and_then(J::as_arr)
                        .ok_or("random_group entry missing data")?;
                    let mut bindings = Vec::with_capacity(data_arr.len());
                    for b in data_arr {
                        bindings.push(Self::parse(b)?);
                    }
                    groups.push((weight, bindings));
                }
                Ok(Self::RandomGroup { groups })
            }
            other => Err(format!("unsupported pool alias type: {other}")),
        }
    }

    fn apply(&self, rng: &mut Lcg, out: &mut HashMap<String, String>) {
        match self {
            Self::Direct { alias, target } => {
                out.insert(alias.clone(), target.clone());
            }
            Self::Random { alias, targets } => {
                if let Some(chosen) = weighted_pick(targets, rng) {
                    out.insert(alias.clone(), chosen.clone());
                }
            }
            Self::RandomGroup { groups } => {
                if let Some(group) = weighted_pick(groups, rng) {
                    for binding in group {
                        binding.apply(rng, out);
                    }
                }
            }
        }
    }
}

fn weighted_pick<'a, T>(items: &'a [(i32, T)], rng: &mut Lcg) -> Option<&'a T> {
    let total: i32 = items.iter().map(|(w, _)| *w).sum();
    if total <= 0 {
        return None;
    }
    let mut roll = rng.next_bound(total);
    for (w, item) in items {
        roll -= *w;
        if roll < 0 {
            return Some(item);
        }
    }
    None
}

#[derive(Clone, Debug)]
pub(crate) struct JigsawStructure {
    pub(crate) id: String,
    pub(crate) start_pool: String,
    pub(crate) start_jigsaw_name: Option<String>,
    pub(crate) max_depth: i32,
    pub(crate) start_height: HeightProvider,
    pub(crate) project_start_to_heightmap: Option<HeightmapKind>,
    pub(crate) max_distance_from_center: i32,
    pub(crate) use_expansion_hack: bool,
    pub(crate) pool_aliases: Vec<PoolAliasBinding>,
    pub(crate) dimension_padding: (i32, i32), // (bottom, top)
    pub(crate) liquid_settings: LiquidSettings,
}

impl JigsawStructure {
    fn parse(id: &str, v: &J) -> Result<Self, String> {
        let start_pool = ns_name(
            v.get("start_pool")
                .and_then(|j| match j {
                    J::Str(s) => Some(s.as_str()),
                    _ => None,
                })
                .ok_or("missing start_pool")?,
        );
        let start_jigsaw_name = match v.get("start_jigsaw_name") {
            None => None,
            Some(J::Str(s)) => Some(ns_name(s)),
            _ => return Err("start_jigsaw_name must be string".into()),
        };
        let max_depth = v
            .get("size")
            .and_then(j_num)
            .ok_or("missing size")? as i32;
        let start_height = HeightProvider::parse(
            v.get("start_height").ok_or("missing start_height")?,
        )?;
        let project_start_to_heightmap = match v.get("project_start_to_heightmap") {
            None => None,
            Some(J::Str(s)) => Some(
                HeightmapKind::parse(s)
                    .ok_or_else(|| format!("unknown heightmap: {s}"))?,
            ),
            _ => return Err("project_start_to_heightmap must be string".into()),
        };
        let max_distance_from_center = match v.get("max_distance_from_center") {
            Some(J::Num(n)) => *n as i32,
            Some(o @ J::Obj(_)) => o
                .get("horizontal")
                .and_then(j_num)
                .ok_or("missing max_distance_from_center.horizontal")? as i32,
            _ => return Err("missing max_distance_from_center".into()),
        };
        let use_expansion_hack = v
            .get("use_expansion_hack")
            .and_then(j_bool)
            .ok_or("missing use_expansion_hack")?;
        let pool_aliases = match v.get("pool_aliases") {
            None => Vec::new(),
            Some(J::Arr(arr)) => arr
                .iter()
                .map(PoolAliasBinding::parse)
                .collect::<Result<Vec<_>, _>>()?,
            _ => return Err("pool_aliases must be array".into()),
        };
        let dimension_padding = match v.get("dimension_padding") {
            None => (0, 0),
            Some(J::Num(n)) => (*n as i32, *n as i32),
            Some(o @ J::Obj(_)) => {
                let bottom = o.get("bottom").and_then(j_num).unwrap_or(0.0) as i32;
                let top = o.get("top").and_then(j_num).unwrap_or(0.0) as i32;
                (bottom, top)
            }
            _ => return Err("invalid dimension_padding".into()),
        };
        let liquid_settings = match v.get("liquid_settings") {
            None => LiquidSettings::ApplyWaterlogging,
            Some(J::Str(s)) => LiquidSettings::parse(s)?,
            _ => return Err("invalid liquid_settings".into()),
        };
        Ok(Self {
            id: id.to_owned(),
            start_pool,
            start_jigsaw_name,
            max_depth,
            start_height,
            project_start_to_heightmap,
            max_distance_from_center,
            use_expansion_hack,
            pool_aliases,
            dimension_padding,
            liquid_settings,
        })
    }
}

// ---------- Processor AST (strict parity with 1.21.1) ----------

fn state_from_json(value: &J, reg: &BReg) -> Result<u32, String> {
    let J::Str(name) = value.get("Name").ok_or("missing Name in block_state")? else {
        return Err("block_state Name must be string".into());
    };
    let mut properties = Vec::new();
    if let Some(props) = value.get("Properties") {
        let J::Obj(map) = props else {
            return Err("block_state Properties must be object".into());
        };
        for (k, v) in map {
            let J::Str(vs) = v else {
                return Err("block_state property value must be string".into());
            };
            properties.push((k.clone(), vs.clone()));
        }
    }
    reg.checked_state(name, &properties)
        .ok_or_else(|| format!("unknown block state: {name}"))
}

fn parse_tag_id(raw: &str, require_hash: bool) -> Result<String, String> {
    let s = if require_hash {
        raw.strip_prefix('#')
            .filter(|t| !t.is_empty())
            .ok_or_else(|| format!("expected #tag, got {raw}"))?
    } else {
        if raw.is_empty() || raw.starts_with('#') {
            return Err(format!("invalid tag id: {raw}"));
        }
        raw
    };
    Ok(ns_name(s))
}

#[derive(Clone, Debug)]
enum RuleTest {
    Always,
    Block(String),
    RandomBlock { name: String, probability: f32 },
    Tag(String),
    BlockState(u32),
    RandomBlockState { state: u32, probability: f32 },
}

impl RuleTest {
    fn parse(value: &J, reg: &BReg) -> Result<Self, String> {
        match value.get("predicate_type").map(J::str) {
            Some("minecraft:always_true") => Ok(Self::Always),
            Some("minecraft:block_match") => {
                let name = value.get("block").ok_or("missing rule block")?.str();
                let state = reg
                    .checked_state(name, &[])
                    .ok_or_else(|| format!("unknown rule block {name}"))?;
                Ok(Self::Block(reg.name_of(state).to_owned()))
            }
            Some("minecraft:random_block_match") => {
                let name = value.get("block").ok_or("missing rule block")?.str();
                let state = reg
                    .checked_state(name, &[])
                    .ok_or_else(|| format!("unknown rule block {name}"))?;
                let prob = value
                    .get("probability")
                    .and_then(j_num)
                    .ok_or("missing probability")? as f32;
                Ok(Self::RandomBlock {
                    name: reg.name_of(state).to_owned(),
                    probability: prob,
                })
            }
            Some("minecraft:tag_match") => {
                let J::Str(tag) = value.get("tag").ok_or("missing rule tag")? else {
                    return Err("tag_match tag must be string".into());
                };
                Ok(Self::Tag(parse_tag_id(tag, false)?))
            }
            Some("minecraft:blockstate_match") => {
                let bs = value.get("block_state").ok_or("missing block_state")?;
                Ok(Self::BlockState(state_from_json(bs, reg)?))
            }
            Some("minecraft:random_blockstate_match") => {
                let bs = value.get("block_state").ok_or("missing block_state")?;
                let prob = value
                    .get("probability")
                    .and_then(j_num)
                    .ok_or("missing probability")? as f32;
                Ok(Self::RandomBlockState {
                    state: state_from_json(bs, reg)?,
                    probability: prob,
                })
            }
            other => Err(format!("unsupported rule predicate_type: {other:?}")),
        }
    }

    fn matches(&self, state: u32, reg: &BReg) -> bool {
        match self {
            Self::Always => true,
            Self::Block(name) | Self::RandomBlock { name, .. } => reg.name_of(state) == name,
            Self::Tag(tag) => reg.in_tag(state, tag),
            Self::BlockState(target) | Self::RandomBlockState { state: target, .. } => {
                state == *target
            }
        }
    }

    fn roll(&self, rng: &mut Lcg) -> bool {
        match self {
            Self::RandomBlock { probability, .. }
            | Self::RandomBlockState { probability, .. } => rng.next_f32() < *probability,
            _ => true,
        }
    }
}

#[derive(Clone, Debug)]
enum PosRuleTest {
    Always,
    AxisAlignedLinearPos {
        min_chance: f32,
        max_chance: f32,
        min_dist: i32,
        max_dist: i32,
        axis: u8,
    },
    LinearPos {
        min_chance: f32,
        max_chance: f32,
        min_dist: i32,
        max_dist: i32,
    },
}

impl PosRuleTest {
    fn opt_f32(v: &J, k: &str, def: f32) -> Result<f32, String> {
        match v.get(k) {
            None => Ok(def),
            Some(J::Num(n)) => Ok(*n as f32),
            _ => Err(format!("invalid float for {k}")),
        }
    }
    fn opt_i32(v: &J, k: &str, def: i32) -> Result<i32, String> {
        match v.get(k) {
            None => Ok(def),
            Some(J::Num(n)) => Ok(*n as i32),
            _ => Err(format!("invalid int for {k}")),
        }
    }
    fn parse(v: &J) -> Result<Self, String> {
        match v.get("predicate_type").map(J::str) {
            Some("minecraft:always_true") => Ok(Self::Always),
            Some("minecraft:axis_aligned_linear_pos") => {
                let min_chance = Self::opt_f32(v, "min_chance", 0.0)?;
                let max_chance = Self::opt_f32(v, "max_chance", 0.0)?;
                let min_dist = Self::opt_i32(v, "min_dist", 0)?;
                let max_dist = Self::opt_i32(v, "max_dist", 0)?;
                let axis = match v.get("axis") {
                    None => 1,
                    Some(J::Str(a)) => match a.as_str() {
                        "x" => 0,
                        "y" => 1,
                        "z" => 2,
                        other => return Err(format!("unknown axis: {other}")),
                    },
                    _ => return Err("axis must be string".into()),
                };
                Ok(Self::AxisAlignedLinearPos {
                    min_chance,
                    max_chance,
                    min_dist,
                    max_dist,
                    axis,
                })
            }
            Some("minecraft:linear_pos") => Ok(Self::LinearPos {
                min_chance: Self::opt_f32(v, "min_chance", 0.0)?,
                max_chance: Self::opt_f32(v, "max_chance", 0.0)?,
                min_dist: Self::opt_i32(v, "min_dist", 0)?,
                max_dist: Self::opt_i32(v, "max_dist", 0)?,
            }),
            other => Err(format!("unsupported position_predicate: {other:?}")),
        }
    }

    fn clamped_chance(dist: i32, min_dist: i32, max_dist: i32, min_chance: f32, max_chance: f32) -> f32 {
        if min_dist >= max_dist {
            return min_chance;
        }
        let t = (dist as f32 - min_dist as f32) / (max_dist as f32 - min_dist as f32);
        let t = t.clamp(0.0, 1.0);
        min_chance + t * (max_chance - min_chance)
    }

    fn matches(&self, world_pos: Pos, pivot: Pos, rng: &mut Lcg) -> bool {
        match *self {
            Self::Always => true,
            Self::AxisAlignedLinearPos {
                min_chance,
                max_chance,
                min_dist,
                max_dist,
                axis,
            } => {
                let dist = match axis {
                    0 => world_pos.0.wrapping_sub(pivot.0).abs(),
                    1 => world_pos.1.wrapping_sub(pivot.1).abs(),
                    _ => world_pos.2.wrapping_sub(pivot.2).abs(),
                };
                rng.next_f32()
                    <= Self::clamped_chance(dist, min_dist, max_dist, min_chance, max_chance)
            }
            Self::LinearPos {
                min_chance,
                max_chance,
                min_dist,
                max_dist,
            } => {
                let dx = world_pos.0.wrapping_sub(pivot.0) as f32;
                let dy = world_pos.1.wrapping_sub(pivot.1) as f32;
                let dz = world_pos.2.wrapping_sub(pivot.2) as f32;
                let dist = (dx * dx + dy * dy + dz * dz).sqrt() as i32;
                rng.next_f32()
                    <= Self::clamped_chance(dist, min_dist, max_dist, min_chance, max_chance)
            }
        }
    }
}

#[derive(Clone, Debug)]
enum BlockEntityModifier {
    Passthrough,
    AppendLoot(String),
}

impl BlockEntityModifier {
    fn parse(v: &J) -> Result<Self, String> {
        match v.get("type").map(J::str) {
            Some("minecraft:passthrough") => Ok(Self::Passthrough),
            Some("minecraft:append_loot") => {
                let J::Str(lt) = v.get("loot_table").ok_or("append_loot missing loot_table")? else {
                    return Err("append_loot loot_table must be string".into());
                };
                Ok(Self::AppendLoot(ns_name(lt)))
            }
            other => Err(format!("unsupported block_entity_modifier: {other:?}")),
        }
    }

    fn apply(&self, rng: &mut Lcg, current: Option<Tag>) -> Option<Tag> {
        match self {
            Self::Passthrough => current,
            Self::AppendLoot(loot_table) => {
                let mut map = match current {
                    Some(Tag::Compound(m)) => m,
                    _ => BTreeMap::new(),
                };
                map.insert(
                    crate::nbt::NbtString::text("LootTable"),
                    Tag::String(crate::nbt::NbtString::text(loot_table)),
                );
                map.insert(
                    crate::nbt::NbtString::text("LootTableSeed"),
                    Tag::Long(rng.next_i64()),
                );
                Some(Tag::Compound(map))
            }
        }
    }
}

#[derive(Clone, Debug)]
struct BlockRule {
    input: RuleTest,
    location: RuleTest,
    position: PosRuleTest,
    output: u32,
    output_nbt: Option<Tag>,
    modifier: BlockEntityModifier,
}

#[derive(Clone, Debug)]
enum RottableBlocks {
    Names(Vec<String>),
    Tag(String),
}

#[derive(Clone, Debug)]
enum IntProvider {
    Const(i32),
    Uniform(i32, i32),
    BiasBottom(i32, i32),
    VeryBias(i32, i32),
    Clamped(Box<IntProvider>, i32, i32),
    Trap(i32, i32, i32),
    Norm(f64, f64, i32, i32),
    Weighted(Vec<(i32, IntProvider)>),
}

impl IntProvider {
    fn parse(j: &J) -> Result<Self, String> {
        if let J::Num(n) = j {
            return Ok(Self::Const(*n as i32));
        }
        let ty = j.get("type").map(J::str).ok_or("missing int provider type")?;
        let ty = ty.strip_prefix("minecraft:").unwrap_or(ty);
        let get_i = |k: &str, k2: &str, def: i32| -> i32 {
            j.get(k)
                .or_else(|| j.get(k2))
                .and_then(j_num)
                .map(|n| n as i32)
                .unwrap_or(def)
        };
        match ty {
            "constant" | "literal" => Ok(Self::Const(get_i("value", "constant", 0))),
            "uniform" => Ok(Self::Uniform(
                get_i("min_inclusive", "min", 0),
                get_i("max_inclusive", "max", 0),
            )),
            "biased_to_bottom" => Ok(Self::BiasBottom(
                get_i("min_inclusive", "min", 0),
                get_i("max_inclusive", "max", 0),
            )),
            "very_biased_to_bottom" => Ok(Self::VeryBias(
                get_i("min_inclusive", "min", 0),
                get_i("max_inclusive", "max", 0),
            )),
            "clamped" => Ok(Self::Clamped(
                Box::new(Self::parse(j.get("source").ok_or("clamped missing source")?)?),
                get_i("min_inclusive", "min", 0),
                get_i("max_inclusive", "max", 0),
            )),
            "trapezoid" => Ok(Self::Trap(
                get_i("min_inclusive", "min", 0),
                get_i("max_inclusive", "max", 0),
                get_i("plateau", "plateau", 0),
            )),
            "clamped_normal" => Ok(Self::Norm(
                j.get("mean").and_then(j_num).unwrap_or(0.0),
                j.get("deviation").and_then(j_num).unwrap_or(1.0),
                get_i("min_inclusive", "min", 0),
                get_i("max_inclusive", "max", 0),
            )),
            "weighted_list" => {
                let arr = j
                    .get("distribution")
                    .and_then(J::as_arr)
                    .ok_or("weighted_list missing distribution")?;
                let mut v = Vec::with_capacity(arr.len());
                for e in arr {
                    let w = e.get("weight").and_then(j_num).unwrap_or(1.0) as i32;
                    let d = e.get("data").unwrap_or(e);
                    v.push((w, Self::parse(d)?));
                }
                Ok(Self::Weighted(v))
            }
            other => Err(format!("unsupported int provider: {other}")),
        }
    }

    fn max_inclusive(&self) -> i32 {
        match self {
            Self::Const(v) => *v,
            Self::Uniform(_, b) | Self::BiasBottom(_, b) | Self::VeryBias(_, b) | Self::Trap(_, b, _) => *b,
            Self::Clamped(_, _, b) | Self::Norm(_, _, _, b) => *b,
            Self::Weighted(l) => l.iter().map(|(_, p)| p.max_inclusive()).max().unwrap_or(0),
        }
    }

    fn sample(&self, rng: &mut Lcg) -> i32 {
        match self {
            Self::Const(v) => *v,
            Self::Uniform(a, b) => {
                if a >= b {
                    *a
                } else {
                    a + rng.next_bound(b - a + 1)
                }
            }
            Self::BiasBottom(a, b) => {
                if a >= b {
                    *a
                } else {
                    let bound = rng.next_bound(b - a + 1) + 1;
                    a + rng.next_bound(bound)
                }
            }
            Self::VeryBias(a, b) => {
                if a >= b {
                    *a
                } else {
                    let u1 = a + rng.next_bound(b - a + 1);
                    let u2 = a + rng.next_bound(b - a + 1);
                    let u3 = a + rng.next_bound(b - a + 1);
                    u1.min(u2).min(u3)
                }
            }
            Self::Clamped(inner, lo, hi) => inner.sample(rng).clamp(*lo, *hi),
            Self::Trap(a, b, pl) => {
                if b - a - pl < 0 {
                    return *a;
                }
                let lo = a + (b - a - pl) / 2;
                let hi = b - (b - a - pl) / 2;
                let v = if a >= b { *a } else { a + rng.next_bound(b - a + 1) };
                if v > lo && v < hi {
                    hi - (v - lo)
                } else {
                    v
                }
            }
            Self::Norm(mean, dev, lo, hi) => {
                let gauss = loop {
                    let v1 = 2.0 * rng.next_f64() - 1.0;
                    let v2 = 2.0 * rng.next_f64() - 1.0;
                    let s = v1 * v1 + v2 * v2;
                    if s < 1.0 && s != 0.0 {
                        break v1 * (-2.0 * s.ln() / s).sqrt();
                    }
                };
                (mean + gauss * dev).round().clamp(*lo as f64, *hi as f64) as i32
            }
            Self::Weighted(list) => {
                let total: i32 = list.iter().map(|(w, _)| (*w).max(0)).sum();
                if total <= 0 {
                    return 0;
                }
                let mut roll = rng.next_bound(total);
                for (w, p) in list {
                    roll -= (*w).max(0);
                    if roll < 0 {
                        return p.sample(rng);
                    }
                }
                list.last().map(|(_, p)| p.sample(rng)).unwrap_or(0)
            }
        }
    }
}

#[derive(Clone, Debug)]
enum Processor {
    BlockIgnore(Vec<String>),
    JigsawReplacement,
    BlockRot {
        integrity: f32,
        rottable: Option<RottableBlocks>,
    },
    Protected(String),
    Rules(Vec<BlockRule>),
    Gravity {
        heightmap: HeightmapKind,
        offset: i32,
    },
    Capped {
        delegate: Box<Processor>,
        limit: IntProvider,
    },
}

impl Processor {
    fn parse_list(value: &J, reg: &BReg) -> Result<Vec<Self>, String> {
        let arr = value
            .get("processors")
            .and_then(J::as_arr)
            .ok_or("missing processors array")?;
        arr.iter().map(|p| Self::parse(p, reg)).collect()
    }

    fn parse(value: &J, reg: &BReg) -> Result<Self, String> {
        match value.get("processor_type").map(J::str) {
            Some("minecraft:block_ignore") => {
                let blocks = value
                    .get("blocks")
                    .and_then(J::as_arr)
                    .ok_or("missing block_ignore.blocks")?;
                let names = blocks
                    .iter()
                    .map(|b| Ok(reg.name_of(state_from_json(b, reg)?).to_owned()))
                    .collect::<Result<Vec<_>, String>>()?;
                Ok(Self::BlockIgnore(names))
            }
            Some("minecraft:block_rot") => {
                let integrity = value
                    .get("integrity")
                    .and_then(j_num)
                    .ok_or("missing block_rot integrity")? as f32;
                let rottable = match value.get("rottable_blocks") {
                    None => None,
                    Some(J::Arr(vals)) => Some(RottableBlocks::Names(
                        vals.iter()
                            .map(|v| {
                                let J::Str(name) = v else {
                                    return Err("rottable block must be string".into());
                                };
                                let st = reg
                                    .checked_state(name, &[])
                                    .ok_or_else(|| format!("unknown rottable block {name}"))?;
                                Ok(reg.name_of(st).to_owned())
                            })
                            .collect::<Result<Vec<_>, String>>()?,
                    )),
                    Some(J::Str(tag)) => Some(RottableBlocks::Tag(parse_tag_id(tag, true)?)),
                    _ => return Err("unsupported rottable_blocks".into()),
                };
                Ok(Self::BlockRot {
                    integrity,
                    rottable,
                })
            }
            Some("minecraft:protected_blocks") => {
                let J::Str(tag) = value.get("value").ok_or("missing protected_blocks value")?
                else {
                    return Err("protected_blocks value must be string".into());
                };
                Ok(Self::Protected(parse_tag_id(tag, true)?))
            }
            Some("minecraft:rule") => {
                let arr = value
                    .get("rules")
                    .and_then(J::as_arr)
                    .ok_or("missing rules array")?;
                let mut rules = Vec::with_capacity(arr.len());
                for r in arr {
                    let position = match r.get("position_predicate") {
                        None => PosRuleTest::Always,
                        Some(p) => PosRuleTest::parse(p)?,
                    };
                    let modifier = match r.get("block_entity_modifier") {
                        None => BlockEntityModifier::Passthrough,
                        Some(m) => BlockEntityModifier::parse(m)?,
                    };
                    let output_j = r.get("output_state").ok_or("missing output_state")?;
                    rules.push(BlockRule {
                        input: RuleTest::parse(
                            r.get("input_predicate").ok_or("missing input_predicate")?,
                            reg,
                        )?,
                        location: RuleTest::parse(
                            r.get("location_predicate")
                                .ok_or("missing location_predicate")?,
                            reg,
                        )?,
                        position,
                        output: state_from_json(output_j, reg)?,
                        output_nbt: None,
                        modifier,
                    });
                }
                Ok(Self::Rules(rules))
            }
            Some("minecraft:gravity") => {
                let heightmap = match value.get("heightmap") {
                    None => HeightmapKind::WorldSurfaceWg,
                    Some(J::Str(s)) => HeightmapKind::parse(s)
                        .ok_or_else(|| format!("unknown gravity heightmap: {s}"))?,
                    _ => return Err("invalid gravity heightmap".into()),
                };
                let offset = value
                    .get("offset")
                    .and_then(j_num)
                    .map(|n| n as i32)
                    .unwrap_or(0);
                Ok(Self::Gravity { heightmap, offset })
            }
            Some("minecraft:capped") => {
                let delegate = Self::parse(
                    value.get("delegate").ok_or("missing capped delegate")?,
                    reg,
                )?;
                let limit = IntProvider::parse(value.get("limit").ok_or("missing capped limit")?)?;
                Ok(Self::Capped {
                    delegate: Box::new(delegate),
                    limit,
                })
            }
            other => Err(format!("unsupported processor_type: {other:?}")),
        }
    }
}

#[derive(Clone, Debug)]
enum ProcessorRef {
    Named(String),
    Inline(Vec<Processor>),
}

impl ProcessorRef {
    fn parse(v: &J, reg: &BReg) -> Result<Self, String> {
        match v {
            J::Str(s) => Ok(Self::Named(ns_name(s))),
            J::Obj(_) => Ok(Self::Inline(Processor::parse_list(v, reg)?)),
            J::Arr(arr) => {
                let procs = arr
                    .iter()
                    .map(|p| Processor::parse(p, reg))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(Self::Inline(procs))
            }
            _ => Err("processors must be string, object, or array".into()),
        }
    }
}

#[derive(Clone, Debug)]
enum PoolElement {
    Empty,
    Single {
        location: String,
        processors: ProcessorRef,
        projection: Projection,
        override_liquid_settings: Option<LiquidSettings>,
    },
    LegacySingle {
        location: String,
        processors: ProcessorRef,
        projection: Projection,
        override_liquid_settings: Option<LiquidSettings>,
    },
    Feature {
        feature: String,
        projection: Projection,
    },
    List {
        elements: Vec<PoolElement>,
        projection: Projection,
    },
}

impl PoolElement {
    fn parse(v: &J, reg: &BReg) -> Result<Self, String> {
        let ty = v
            .get("element_type")
            .map(J::str)
            .ok_or("missing element_type")?;
        match ty {
            "minecraft:empty_pool_element" => Ok(Self::Empty),
            "minecraft:single_pool_element" | "minecraft:legacy_single_pool_element" => {
                let loc = match v.get("location") {
                    Some(J::Str(s)) => ns_name(s),
                    _ => return Err("single_pool_element missing location".into()),
                };
                let processors = ProcessorRef::parse(
                    v.get("processors").ok_or("missing element processors")?,
                    reg,
                )?;
                let projection = Projection::parse(
                    v.get("projection")
                        .map(J::str)
                        .ok_or("missing element projection")?,
                )?;
                let override_liquid_settings = match v.get("override_liquid_settings") {
                    None => None,
                    Some(J::Str(s)) => Some(LiquidSettings::parse(s)?),
                    _ => return Err("invalid override_liquid_settings".into()),
                };
                if ty == "minecraft:legacy_single_pool_element" {
                    Ok(Self::LegacySingle {
                        location: loc,
                        processors,
                        projection,
                        override_liquid_settings,
                    })
                } else {
                    Ok(Self::Single {
                        location: loc,
                        processors,
                        projection,
                        override_liquid_settings,
                    })
                }
            }
            "minecraft:feature_pool_element" => {
                let feature = match v.get("feature") {
                    Some(J::Str(s)) => ns_name(s),
                    _ => return Err("feature_pool_element missing feature id".into()),
                };
                let projection = Projection::parse(
                    v.get("projection")
                        .map(J::str)
                        .ok_or("missing feature_pool_element projection")?,
                )?;
                Ok(Self::Feature {
                    feature,
                    projection,
                })
            }
            "minecraft:list_pool_element" => {
                let arr = v
                    .get("elements")
                    .and_then(J::as_arr)
                    .ok_or("list_pool_element missing elements")?;
                let mut elements = Vec::with_capacity(arr.len());
                for item in arr {
                    elements.push(Self::parse(item, reg)?);
                }
                let projection = Projection::parse(
                    v.get("projection")
                        .map(J::str)
                        .ok_or("missing list_pool_element projection")?,
                )?;
                Ok(Self::List {
                    elements,
                    projection,
                })
            }
            other => Err(format!("unsupported element_type: {other}")),
        }
    }

    fn projection(&self) -> Projection {
        match self {
            Self::Empty => Projection::Rigid,
            Self::Single { projection, .. }
            | Self::LegacySingle { projection, .. }
            | Self::Feature { projection, .. }
            | Self::List { projection, .. } => *projection,
        }
    }

    fn ground_level_delta(&self) -> i32 {
        match self {
            Self::Empty => 0,
            _ => 1,
        }
    }

    fn describe(&self) -> String {
        match self {
            Self::Empty => "empty".to_owned(),
            Self::Single { location, .. } => format!("single:{location}"),
            Self::LegacySingle { location, .. } => format!("legacy:{location}"),
            Self::Feature { feature, .. } => format!("feature:{feature}"),
            Self::List { elements, .. } => {
                let inner: Vec<String> = elements.iter().map(Self::describe).collect();
                format!("list:[{}]", inner.join(","))
            }
        }
    }
}

#[derive(Clone, Debug)]
struct TemplatePool {
    fallback: String,
    raw_elements: Vec<(i32, PoolElement)>,
    templates: Vec<PoolElement>, // weighted expansion
}

impl TemplatePool {
    fn parse(v: &J, reg: &BReg) -> Result<Self, String> {
        let fallback = ns_name(
            v.get("fallback")
                .and_then(|j| match j {
                    J::Str(s) => Some(s.as_str()),
                    _ => None,
                })
                .ok_or("missing fallback")?,
        );
        let arr = v
            .get("elements")
            .and_then(J::as_arr)
            .ok_or("missing elements")?;
        let mut raw_elements = Vec::with_capacity(arr.len());
        let mut templates = Vec::new();
        for pair in arr {
            let weight = pair
                .get("weight")
                .and_then(j_num)
                .ok_or("pool entry missing weight")? as i32;
            let el = PoolElement::parse(pair.get("element").ok_or("pool entry missing element")?, reg)?;
            for _ in 0..weight {
                templates.push(el.clone());
            }
            raw_elements.push((weight, el));
        }
        Ok(Self {
            fallback,
            raw_elements,
            templates,
        })
    }

    fn get_random_template(&self, rng: &mut Lcg) -> PoolElement {
        if self.templates.is_empty() {
            PoolElement::Empty
        } else {
            let idx = rng.next_bound(self.templates.len() as i32) as usize;
            self.templates[idx].clone()
        }
    }

    fn get_shuffled_templates(&self, rng: &mut Lcg) -> Vec<PoolElement> {
        let mut out = self.templates.clone();
        util_shuffle(&mut out, rng);
        out
    }
}

trait JigsawData {
    fn pool(&self, id: &str) -> Option<&TemplatePool>;
    fn resolve_processors(&self, pref: &ProcessorRef) -> Result<Vec<Processor>, String>;
    fn template(&mut self, id: &str, reg: &BReg) -> Result<std::sync::Arc<Template>, String>;
}

struct DatapackRepo {
    #[cfg(test)]
    structures: HashMap<String, JigsawStructure>,
    pools: HashMap<String, TemplatePool>,
    processor_lists: HashMap<String, Vec<Processor>>,
    templates: HashMap<String, std::sync::Arc<Template>>,
}

struct LiveJigsawDefs {
    structures: HashMap<String, JigsawStructure>,
    pools: HashMap<String, TemplatePool>,
    processor_lists: HashMap<String, Vec<Processor>>,
}

fn load_template_arc(id: &str, reg: &BReg) -> Result<std::sync::Arc<Template>, String> {
    let key = ns_name(id);
    let tpl = if let Some(bytes) = Gen::dp_resource("structure", &key, "nbt") {
        let (_, root) = crate::nbt::read_gzip(&bytes, 16 * 1024 * 1024)
            .map_err(|e| format!("nbt {key}: {e}"))?;
        Template::load(&root, reg).map_err(|e| format!("template {key}: {e}"))?
    } else {
        Template {
            size: (0, 0, 0),
            palettes: Vec::new(),
            entities: Vec::new(),
        }
    };
    Ok(std::sync::Arc::new(tpl))
}

static LIVE_JIGSAW_DEFS: std::sync::OnceLock<Option<LiveJigsawDefs>> = std::sync::OnceLock::new();

thread_local! {
    static LIVE_JIGSAW_TEMPLATES: std::cell::RefCell<HashMap<String, std::sync::Arc<Template>>> =
        std::cell::RefCell::new(HashMap::new());
}

fn live_jigsaw_defs(reg: &BReg) -> Option<&'static LiveJigsawDefs> {
    LIVE_JIGSAW_DEFS
        .get_or_init(|| match DatapackRepo::load_dp(reg) {
            Ok(defs) => Some(defs),
            Err(e) => {
                eprintln!("SPIPE jigsaw definitions failed to load: {e}");
                None
            }
        })
        .as_ref()
}

struct LiveJigsawRepo<'a>(&'a LiveJigsawDefs);

impl JigsawData for LiveJigsawRepo<'_> {
    fn pool(&self, id: &str) -> Option<&TemplatePool> {
        self.0.pools.get(id)
    }

    fn resolve_processors(&self, pref: &ProcessorRef) -> Result<Vec<Processor>, String> {
        match pref {
            ProcessorRef::Named(id) => self
                .0
                .processor_lists
                .get(id)
                .cloned()
                .ok_or_else(|| format!("missing processor_list: {id}")),
            ProcessorRef::Inline(list) => Ok(list.clone()),
        }
    }

    fn template(&mut self, id: &str, reg: &BReg) -> Result<std::sync::Arc<Template>, String> {
        let key = ns_name(id);
        if let Some(hit) = LIVE_JIGSAW_TEMPLATES.with(|slot| slot.borrow().get(&key).cloned()) {
            return Ok(hit);
        }
        let tpl = load_template_arc(&key, reg)?;
        LIVE_JIGSAW_TEMPLATES.with(|slot| {
            slot.borrow_mut().insert(key, tpl.clone());
        });
        Ok(tpl)
    }
}

#[cfg(test)]
fn collect_json_files(dir: &Path, prefix: &str, out: &mut Vec<(String, std::path::PathBuf)>) -> Result<(), String> {
    if !dir.exists() {
        return Ok(());
    }
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| format!("read_dir {}: {e}", dir.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("dir entry: {e}"))?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            let sub = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            collect_json_files(&path, &sub, out)?;
        } else if let Some(stem) = name.strip_suffix(".json") {
            let id = if prefix.is_empty() {
                format!("minecraft:{stem}")
            } else {
                format!("minecraft:{prefix}/{stem}")
            };
            out.push((id, path));
        }
    }
    Ok(())
}

impl DatapackRepo {
    fn load_dp(reg: &BReg) -> Result<LiveJigsawDefs, String> {
        let mut struct_files: Vec<(String, Vec<u8>)> = Gen::dp_files("structure").into_iter().collect();
        struct_files.sort_by(|a, b| a.0.cmp(&b.0));
        let mut structures = HashMap::new();
        for (id, bytes) in struct_files {
            let json = jparse(&bytes)
                .ok_or_else(|| format!("invalid JSON in structure {id}"))?;
            if json.get("type").map(J::str) == Some("minecraft:jigsaw") {
                structures.insert(id.clone(), JigsawStructure::parse(&id, &json)?);
            }
        }

        let mut pool_files: Vec<(String, Vec<u8>)> = Gen::dp_files("template_pool").into_iter().collect();
        pool_files.sort_by(|a, b| a.0.cmp(&b.0));
        let mut pools = HashMap::new();
        for (id, bytes) in pool_files {
            let json = jparse(&bytes)
                .ok_or_else(|| format!("invalid JSON in template_pool {id}"))?;
            pools.insert(id, TemplatePool::parse(&json, reg)?);
        }

        let mut proc_files: Vec<(String, Vec<u8>)> = Gen::dp_files("processor_list").into_iter().collect();
        proc_files.sort_by(|a, b| a.0.cmp(&b.0));
        let mut processor_lists = HashMap::new();
        processor_lists.insert("minecraft:empty".to_owned(), Vec::new());
        for (id, bytes) in proc_files {
            let json = jparse(&bytes)
                .ok_or_else(|| format!("invalid JSON in processor_list {id}"))?;
            let list = Processor::parse_list(&json, reg)
                .map_err(|e| format!("{id}: {e}"))?;
            processor_lists.insert(id, list);
        }

        Ok(LiveJigsawDefs {
            structures,
            pools,
            processor_lists,
        })
    }

    #[cfg(test)]
    fn load(reg: &BReg) -> Result<Self, String> {
        let base = Path::new("datapacks/vanilla/data/minecraft/worldgen");

        let mut struct_files = Vec::new();
        collect_json_files(&base.join("structure"), "", &mut struct_files)?;
        let mut structures = HashMap::new();
        for (id, path) in struct_files {
            let bytes = std::fs::read(&path)
                .map_err(|e| format!("read {}: {e}", path.display()))?;
            let json = jparse(&bytes)
                .ok_or_else(|| format!("invalid JSON in {}", path.display()))?;
            if json.get("type").map(J::str) == Some("minecraft:jigsaw") {
                structures.insert(id.clone(), JigsawStructure::parse(&id, &json)?);
            }
        }

        let mut pool_files = Vec::new();
        collect_json_files(&base.join("template_pool"), "", &mut pool_files)?;
        let mut pools = HashMap::new();
        for (id, path) in pool_files {
            let bytes = std::fs::read(&path)
                .map_err(|e| format!("read {}: {e}", path.display()))?;
            let json = jparse(&bytes)
                .ok_or_else(|| format!("invalid JSON in {}", path.display()))?;
            pools.insert(id, TemplatePool::parse(&json, reg)?);
        }

        let mut proc_files = Vec::new();
        collect_json_files(&base.join("processor_list"), "", &mut proc_files)?;
        let mut processor_lists = HashMap::new();
        processor_lists.insert("minecraft:empty".to_owned(), Vec::new());
        for (id, path) in proc_files {
            let bytes = std::fs::read(&path)
                .map_err(|e| format!("read {}: {e}", path.display()))?;
            let json = jparse(&bytes)
                .ok_or_else(|| format!("invalid JSON in {}", path.display()))?;
            let list = Processor::parse_list(&json, reg)
                .map_err(|e| format!("{}: {e}", path.display()))?;
            processor_lists.insert(id, list);
        }

        Ok(Self {
            structures,
            pools,
            processor_lists,
            templates: HashMap::new(),
        })
    }
}

impl JigsawData for DatapackRepo {
    fn pool(&self, id: &str) -> Option<&TemplatePool> {
        self.pools.get(id)
    }

    fn resolve_processors(&self, pref: &ProcessorRef) -> Result<Vec<Processor>, String> {
        match pref {
            ProcessorRef::Named(id) => self
                .processor_lists
                .get(id)
                .cloned()
                .ok_or_else(|| format!("missing processor_list: {id}")),
            ProcessorRef::Inline(list) => Ok(list.clone()),
        }
    }

    fn template(&mut self, id: &str, reg: &BReg) -> Result<std::sync::Arc<Template>, String> {
        let key = ns_name(id);
        if !self.templates.contains_key(&key) {
            let tpl = load_template_arc(&key, reg)?;
            self.templates.insert(key.clone(), tpl);
        }
        Ok(self.templates.get(&key).unwrap().clone())
    }
}

// ---------- Jigsaw helpers & geometry ----------

fn util_shuffle<T>(slice: &mut [T], rng: &mut Lcg) {
    let mut i = slice.len();
    while i > 1 {
        let j = rng.next_bound(i as i32) as usize;
        slice.swap(i - 1, j);
        i -= 1;
    }
}

fn dir_normal(dir: &str) -> Pos {
    match dir {
        "down" => (0, -1, 0),
        "up" => (0, 1, 0),
        "north" => (0, 0, -1),
        "south" => (0, 0, 1),
        "west" => (-1, 0, 0),
        "east" => (1, 0, 0),
        _ => (0, 0, 0),
    }
}

fn dir_opposite(dir: &str) -> &'static str {
    match dir {
        "down" => "up",
        "up" => "down",
        "north" => "south",
        "south" => "north",
        "west" => "east",
        "east" => "west",
        _ => "down",
    }
}

fn dir_is_horizontal(dir: &str) -> bool {
    matches!(dir, "north" | "south" | "west" | "east")
}

fn jigsaw_front_top(state: u32, reg: &BReg) -> (&'static str, &'static str) {
    let orient = reg.get_prop(state, "orientation");
    let (front, top) = orient
        .as_deref()
        .and_then(|s| s.split_once('_'))
        .unwrap_or(("north", "up"));
    let norm = |d: &str| -> &'static str {
        match d {
            "down" => "down",
            "up" => "up",
            "north" => "north",
            "south" => "south",
            "west" => "west",
            "east" => "east",
            _ => "north",
        }
    };
    (norm(front), norm(top))
}

fn nbt_str<'a>(tag: &'a Tag, key: &str) -> Option<String> {
    tag.get(key)?.string()?.to_utf8().ok()
}

fn nbt_int(tag: &Tag, key: &str) -> Option<i32> {
    tag.get(key)?.int()
}

fn jigsaws_can_attach(parent: &BlockInfo, cand: &BlockInfo, reg: &BReg) -> bool {
    let (p_front, p_top) = jigsaw_front_top(parent.state, reg);
    let (c_front, c_top) = jigsaw_front_top(cand.state, reg);
    if p_front != dir_opposite(c_front) {
        return false;
    }
    let Some(p_nbt) = parent.nbt.as_ref() else {
        return false;
    };
    let Some(c_nbt) = cand.nbt.as_ref() else {
        return false;
    };
    let p_target = nbt_str(p_nbt, "target").map(|s| ns_name(&s)).unwrap_or_default();
    let c_name = nbt_str(c_nbt, "name").map(|s| ns_name(&s)).unwrap_or_default();
    if p_target != c_name {
        return false;
    }
    let default_joint = if dir_is_horizontal(p_front) {
        "aligned"
    } else {
        "rollable"
    };
    let joint = nbt_str(p_nbt, "joint").unwrap_or_else(|| default_joint.to_owned());
    let rollable = match joint.as_str() {
        "rollable" => true,
        "aligned" => false,
        _ => dir_is_horizontal(p_front) == false,
    };
    rollable || p_top == c_top
}

fn bb_contains_pos(bb: BBox, p: Pos) -> bool {
    p.0 >= bb[0] && p.0 <= bb[3] && p.1 >= bb[1] && p.1 <= bb[4] && p.2 >= bb[2] && p.2 <= bb[5]
}

fn jigsaw_bb_intersects(a: BBox, b: BBox) -> bool {
    a[3] >= b[0] && a[0] <= b[3] && a[5] >= b[2] && a[2] <= b[5] && a[4] >= b[1] && a[1] <= b[4]
}

fn bb_move(bb: BBox, dx: i32, dy: i32, dz: i32) -> BBox {
    [
        bb[0] + dx,
        bb[1] + dy,
        bb[2] + dz,
        bb[3] + dx,
        bb[4] + dy,
        bb[5] + dz,
    ]
}

fn jigsaw_bb_encapsulate(a: BBox, b: BBox) -> BBox {
    [
        a[0].min(b[0]),
        a[1].min(b[1]),
        a[2].min(b[2]),
        a[3].max(b[3]),
        a[4].max(b[4]),
        a[5].max(b[5]),
    ]
}

fn bb_yspan(bb: BBox) -> i32 {
    bb[4] - bb[1] + 1
}

#[derive(Clone, Copy, Debug)]
struct Aabb {
    min_x: f64,
    min_y: f64,
    min_z: f64,
    max_x: f64,
    max_y: f64,
    max_z: f64,
}

impl Aabb {
    fn of_bb(bb: BBox) -> Self {
        Self {
            min_x: bb[0] as f64,
            min_y: bb[1] as f64,
            min_z: bb[2] as f64,
            max_x: (bb[3] + 1) as f64,
            max_y: (bb[4] + 1) as f64,
            max_z: (bb[5] + 1) as f64,
        }
    }

    fn deflate(self, d: f64) -> Self {
        Self {
            min_x: self.min_x + d,
            min_y: self.min_y + d,
            min_z: self.min_z + d,
            max_x: self.max_x - d,
            max_y: self.max_y - d,
            max_z: self.max_z - d,
        }
    }

    fn is_empty(&self) -> bool {
        self.min_x >= self.max_x || self.min_y >= self.max_y || self.min_z >= self.max_z
    }

    fn contained_in(&self, outer: &Aabb) -> bool {
        self.min_x >= outer.min_x
            && self.min_y >= outer.min_y
            && self.min_z >= outer.min_z
            && self.max_x <= outer.max_x
            && self.max_y <= outer.max_y
            && self.max_z <= outer.max_z
    }

    fn intersects_strict(&self, other: &Aabb) -> bool {
        self.min_x < other.max_x
            && self.max_x > other.min_x
            && self.min_y < other.max_y
            && self.max_y > other.min_y
            && self.min_z < other.max_z
            && self.max_z > other.min_z
    }
}

#[derive(Clone, Debug)]
struct FreeShape {
    outer: Aabb,
    obstacles: Vec<Aabb>,
}

impl FreeShape {
    fn from_aabb(outer: Aabb) -> Self {
        Self {
            outer,
            obstacles: Vec::new(),
        }
    }

    /// Mirrors `Shapes.joinIsNotEmpty(freeShape, Shapes.create(candBox), BooleanOp.ONLY_SECOND)`.
    fn blocks_cand(&self, cand: &Aabb) -> bool {
        if cand.is_empty() {
            return false;
        }
        !cand.contained_in(&self.outer)
            || self.obstacles.iter().any(|obs| cand.intersects_strict(obs))
    }

    fn cut_out(&mut self, cand_full: Aabb) {
        if !cand_full.is_empty() {
            self.obstacles.push(cand_full);
        }
    }
}

#[allow(dead_code)]
fn element_size<D: JigsawData>(
    repo: &mut D,
    el: &PoolElement,
    rot: Rotation,
    reg: &BReg,
) -> Result<Pos, String> {
    match el {
        PoolElement::Empty | PoolElement::Feature { .. } => Ok((0, 0, 0)),
        PoolElement::Single { location, .. } | PoolElement::LegacySingle { location, .. } => {
            let tpl = repo.template(location, reg)?;
            let s = tpl.size;
            Ok(match rot {
                Rotation::Clockwise90 | Rotation::Counterclockwise90 => (s.2, s.1, s.0),
                Rotation::None | Rotation::Clockwise180 => s,
            })
        }
        PoolElement::List { elements, .. } => {
            let mut mx = 0;
            let mut my = 0;
            let mut mz = 0;
            for sub in elements {
                let s = element_size(repo, sub, rot, reg)?;
                mx = mx.max(s.0);
                my = my.max(s.1);
                mz = mz.max(s.2);
            }
            Ok((mx, my, mz))
        }
    }
}

fn element_bounding_box<D: JigsawData>(
    repo: &mut D,
    el: &PoolElement,
    pos: Pos,
    rot: Rotation,
    reg: &BReg,
) -> Result<BBox, String> {
    match el {
        PoolElement::Empty => Err("empty pool element has no bounding box".into()),
        PoolElement::Feature { .. } => Ok([pos.0, pos.1, pos.2, pos.0, pos.1, pos.2]),
        PoolElement::Single { location, .. } | PoolElement::LegacySingle { location, .. } => {
            let tpl = repo.template(location, reg)?;
            if tpl.size.0 < 1 || tpl.size.1 < 1 || tpl.size.2 < 1 {
                Ok([pos.0, pos.1, pos.2, pos.0, pos.1, pos.2])
            } else {
                Ok(transform_bounds(tpl.size, pos, (0, 0, 0), Mirror::None, rot))
            }
        }
        PoolElement::List { elements, .. } => {
            let mut combined: Option<BBox> = None;
            for sub in elements {
                if matches!(sub, PoolElement::Empty) {
                    continue;
                }
                let sub_bb = element_bounding_box(repo, sub, pos, rot, reg)?;
                combined = Some(match combined {
                    None => sub_bb,
                    Some(cur) => jigsaw_bb_encapsulate(cur, sub_bb),
                });
            }
            combined.ok_or_else(|| "list pool element has only empty elements".into())
        }
    }
}

fn element_shuffled_jigsaws<D: JigsawData>(
    repo: &mut D,
    el: &PoolElement,
    pos: Pos,
    rot: Rotation,
    rng: &mut Lcg,
    reg: &BReg,
) -> Result<Vec<BlockInfo>, String> {
    match el {
        PoolElement::Empty => Ok(Vec::new()),
        PoolElement::Feature { .. } => {
            let state = reg
                .checked_state("minecraft:jigsaw", &[("orientation".into(), "down_south".into())])
                .ok_or("missing jigsaw down_south state")?;
            let nbt = Tag::Compound(BTreeMap::from([
                (
                    crate::nbt::NbtString::text("name"),
                    Tag::String(crate::nbt::NbtString::text("minecraft:bottom")),
                ),
                (
                    crate::nbt::NbtString::text("final_state"),
                    Tag::String(crate::nbt::NbtString::text("minecraft:air")),
                ),
                (
                    crate::nbt::NbtString::text("pool"),
                    Tag::String(crate::nbt::NbtString::text("minecraft:empty")),
                ),
                (
                    crate::nbt::NbtString::text("target"),
                    Tag::String(crate::nbt::NbtString::text("minecraft:empty")),
                ),
                (
                    crate::nbt::NbtString::text("joint"),
                    Tag::String(crate::nbt::NbtString::text("rollable")),
                ),
            ]));
            Ok(vec![BlockInfo {
                pos,
                state,
                nbt: Some(nbt),
            }])
        }
        PoolElement::Single { location, .. } | PoolElement::LegacySingle { location, .. } => {
            let tpl = repo.template(location, reg)?;
            let mut list: Vec<BlockInfo> = match tpl.palettes.first() {
                None => Vec::new(),
                Some(pal) => pal
                    .iter()
                    .filter(|b| reg.name_of(b.state) == "minecraft:jigsaw")
                    .map(|b| {
                        let tp = transform_block(b.pos, Mirror::None, rot, (0, 0, 0));
                        BlockInfo {
                            pos: (
                                tp.0.wrapping_add(pos.0),
                                tp.1.wrapping_add(pos.1),
                                tp.2.wrapping_add(pos.2),
                            ),
                            state: transform_state(b.state, Mirror::None, rot)
                                .unwrap_or(b.state),
                            nbt: b.nbt.clone(),
                        }
                    })
                    .collect(),
            };
            util_shuffle(&mut list, rng);
            // Stable sort descending by selection_priority (default 0)
            list.sort_by(|a, b| {
                let pa = a
                    .nbt
                    .as_ref()
                    .and_then(|t| nbt_int(t, "selection_priority"))
                    .unwrap_or(0);
                let pb = b
                    .nbt
                    .as_ref()
                    .and_then(|t| nbt_int(t, "selection_priority"))
                    .unwrap_or(0);
                pb.cmp(&pa)
            });
            Ok(list)
        }
        PoolElement::List { elements, .. } => {
            if let Some(first) = elements.first() {
                element_shuffled_jigsaws(repo, first, pos, rot, rng, reg)
            } else {
                Ok(Vec::new())
            }
        }
    }
}

fn pool_max_size<D: JigsawData>(
    repo: &mut D,
    pool_id: &str,
    reg: &BReg,
) -> Result<i32, String> {
    let Some(pool) = repo.pool(pool_id).cloned() else {
        return Ok(0);
    };
    let mut best = 0i32;
    for (_, el) in &pool.raw_elements {
        if matches!(el, PoolElement::Empty) {
            continue;
        }
        let bb = element_bounding_box(repo, el, (0, 0, 0), Rotation::None, reg)?;
        best = best.max(bb_yspan(bb));
    }
    Ok(best)
}

// ---------- JigsawPlacement ----------

#[derive(Clone, Debug)]
pub(crate) struct JigsawPiece {
    element: PoolElement,
    pos: Pos,
    gld: i32,
    rot: Rotation,
    pub(crate) bb: BBox,
    liquid_settings: LiquidSettings,
    pub(crate) junctions: Vec<crate::worldgen::BeardJunction>,
}

impl JigsawPiece {
    fn move_by(&mut self, dx: i32, dy: i32, dz: i32) {
        self.pos = (self.pos.0 + dx, self.pos.1 + dy, self.pos.2 + dz);
        self.bb = bb_move(self.bb, dx, dy, dz);
    }

    pub(crate) fn is_rigid(&self) -> bool {
        self.element.projection() == Projection::Rigid
    }

    pub(crate) fn ground_level_delta(&self) -> i32 {
        self.gld
    }
}

struct PriorityQueue<T> {
    queues: BTreeMap<i32, VecDeque<T>>,
}

impl<T> PriorityQueue<T> {
    fn new() -> Self {
        Self {
            queues: BTreeMap::new(),
        }
    }
    fn push(&mut self, item: T, priority: i32) {
        self.queues.entry(priority).or_default().push_back(item);
    }
    fn pop(&mut self) -> Option<T> {
        let (&max_prio, _) = self.queues.iter().next_back()?;
        let q = self.queues.get_mut(&max_prio)?;
        let item = q.pop_front();
        if q.is_empty() {
            self.queues.remove(&max_prio);
        }
        item
    }
}

trait JigsawWorld {
    fn heightmap(&mut self, x: i32, z: i32, kind: HeightmapKind) -> i32;
    fn valid_biome(&mut self, x: i32, y: i32, z: i32) -> bool;
}

#[cfg(test)]
struct TestJigsawWorld {
    flat_top: i32,
}

#[cfg(test)]
impl JigsawWorld for TestJigsawWorld {
    fn heightmap(&mut self, _x: i32, _z: i32, _kind: HeightmapKind) -> i32 {
        self.flat_top
    }

    fn valid_biome(&mut self, _x: i32, _y: i32, _z: i32) -> bool {
        true
    }
}

struct LiveJigsawWorld<'a> {
    g: &'a mut Gen,
    allowed_biomes: &'a HashSet<String>,
}

impl JigsawWorld for LiveJigsawWorld<'_> {
    fn heightmap(&mut self, x: i32, z: i32, kind: HeightmapKind) -> i32 {
        match kind {
            HeightmapKind::OceanFloor | HeightmapKind::OceanFloorWg => self.g.top_oc(x, z),
            _ => self.g.top_ws(x, z),
        }
    }

    fn valid_biome(&mut self, x: i32, y: i32, z: i32) -> bool {
        if self.allowed_biomes.is_empty() {
            return true;
        }
        self.g
            .biome_name_at(x, y, z)
            .is_some_and(|b| self.allowed_biomes.contains(&b))
    }
}

fn generate_pieces_with_hm<D: JigsawData, W: JigsawWorld>(
    repo: &mut D,
    world: &mut W,
    structure: &JigsawStructure,
    seed: i64,
    cx: i32,
    cz: i32,
    dim_min_y: i32,
    dim_max_y_excl: i32,
    reg: &BReg,
) -> Result<Option<(Pos, Vec<JigsawPiece>)>, String> {
    let mut gen_rng = Lcg::for_large_feature(seed as u64, cx, cz);
    let sampled_y = structure.start_height.sample(&mut gen_rng);
    let start_pos = (cx << 4, sampled_y, cz << 4);

    let mut alias_map = HashMap::new();
    if !structure.pool_aliases.is_empty() {
        let factory_seed = Lcg::new(seed as u64).next_i64();
        let mut alias_rng =
            Lcg::new((hash_block_pos(start_pos.0, start_pos.1, start_pos.2) ^ factory_seed) as u64);
        for binding in &structure.pool_aliases {
            binding.apply(&mut alias_rng, &mut alias_map);
        }
    }
    let lookup_pool = |raw: &str| -> String {
        let n = ns_name(raw);
        alias_map.get(&n).cloned().unwrap_or(n)
    };

    let start_pool_id = lookup_pool(&structure.start_pool);
    let start_pool = repo
        .pool(&start_pool_id)
        .ok_or_else(|| format!("missing start_pool: {start_pool_id}"))?
        .clone();

    let start_rot = Rotation::from_index(gen_rng.next_bound(4) as u8).unwrap();
    let start_el = start_pool.get_random_template(&mut gen_rng);
    if matches!(start_el, PoolElement::Empty) {
        return Ok(None);
    }

    let delta = if let Some(jigsaw_name) = &structure.start_jigsaw_name {
        let jigsaws = element_shuffled_jigsaws(repo, &start_el, start_pos, start_rot, &mut gen_rng, reg)?;
        let mut found = None;
        for j in jigsaws {
            if let Some(nbt) = &j.nbt {
                if nbt_str(nbt, "name").map(|s| ns_name(&s)).as_deref() == Some(jigsaw_name.as_str()) {
                    found = Some(j.pos);
                    break;
                }
            }
        }
        let found_pos = found.ok_or_else(|| format!("missing start_jigsaw_name: {jigsaw_name}"))?;
        (
            found_pos.0 - start_pos.0,
            found_pos.1 - start_pos.1,
            found_pos.2 - start_pos.2,
        )
    } else {
        (0, 0, 0)
    };

    let piece_pos = (
        start_pos.0 - delta.0,
        start_pos.1 - delta.1,
        start_pos.2 - delta.2,
    );
    let piece_bb = element_bounding_box(repo, &start_el, piece_pos, start_rot, reg)?;
    let mut start_piece = JigsawPiece {
        element: start_el.clone(),
        pos: piece_pos,
        gld: start_el.ground_level_delta(),
        rot: start_rot,
        bb: piece_bb,
        liquid_settings: structure.liquid_settings,
        junctions: Vec::new(),
    };

    let center_x = (piece_bb[3] + piece_bb[0]) / 2;
    let center_z = (piece_bb[5] + piece_bb[2]) / 2;
    let target_y = if let Some(hm_kind) = structure.project_start_to_heightmap {
        start_pos.1 + world.heightmap(center_x, center_z, hm_kind)
    } else {
        piece_pos.1
    };
    let anchor_y = piece_bb[1] + start_piece.gld;
    start_piece.move_by(0, target_y - anchor_y, 0);
    let center_y = target_y + delta.1;
    let stub = (center_x, center_y, center_z);

    if !world.valid_biome(center_x, center_y, center_z) {
        return Ok(None);
    }

    let mut pieces = vec![start_piece];
    if structure.max_depth <= 0 {
        return Ok(Some((stub, pieces)));
    }

    let max_d = structure.max_distance_from_center;
    let (pad_bottom, pad_top) = structure.dimension_padding;
    let outer_aabb = Aabb {
        min_x: (center_x - max_d) as f64,
        min_y: (center_y - max_d).max(dim_min_y + pad_bottom) as f64,
        min_z: (center_z - max_d) as f64,
        max_x: (center_x + max_d + 1) as f64,
        max_y: (center_y + max_d + 1).min(dim_max_y_excl - pad_top) as f64,
        max_z: (center_z + max_d + 1) as f64,
    };
    let mut root_shape = FreeShape::from_aabb(outer_aabb);
    root_shape.cut_out(Aabb::of_bb(pieces[0].bb));

    // Placer::tryPlacingChildren runs on root piece then queued pieces
    #[derive(Clone)]
    enum ShapeSlot {
        Root,
        Local(usize), // index into local_shapes
    }
    let mut local_shapes: Vec<Option<FreeShape>> = Vec::new();
    let mut queue: PriorityQueue<(usize, ShapeSlot, i32)> = PriorityQueue::new();

    let mut try_place_children =
        |parent_idx: usize,
         parent_shape_slot: ShapeSlot,
         depth: i32,
         pieces: &mut Vec<JigsawPiece>,
         root_shape: &mut FreeShape,
         local_shapes: &mut Vec<Option<FreeShape>>,
         queue: &mut PriorityQueue<(usize, ShapeSlot, i32)>,
         world: &mut W|
         -> Result<(), String> {
            let (parent_element, parent_pos, parent_bb, parent_gld, parent_rot) = {
                let p = &pieces[parent_idx];
                (p.element.clone(), p.pos, p.bb, p.gld, p.rot)
            };
            let parent_rigid = parent_element.projection() == Projection::Rigid;
            let parent_min_y = parent_bb[1];
            let parent_jigsaws =
                element_shuffled_jigsaws(repo, &parent_element, parent_pos, parent_rot, &mut gen_rng, reg)?;
            let parent_children_idx = local_shapes.len();
            local_shapes.push(None);

            for p_jig in parent_jigsaws {
                let (p_front, _) = jigsaw_front_top(p_jig.state, reg);
                let p_jig_pos = p_jig.pos;
                let n = dir_normal(p_front);
                let target_pos = (p_jig_pos.0 + n.0, p_jig_pos.1 + n.1, p_jig_pos.2 + n.2);
                let parent_jig_rel_y = p_jig_pos.1 - parent_min_y;
                let mut cached_hm_k: i32 = -1;

                let raw_pool = p_jig
                    .nbt
                    .as_ref()
                    .and_then(|t| nbt_str(t, "pool"))
                    .unwrap_or_else(|| "minecraft:empty".to_owned());
                let pool_id = lookup_pool(&raw_pool);
                let Some(pool) = repo.pool(&pool_id).cloned() else {
                    continue;
                };
                if pool.templates.is_empty() && pool_id != "minecraft:empty" {
                    continue;
                }
                let fallback_id = pool.fallback.clone();
                let Some(fallback) = repo.pool(&fallback_id).cloned() else {
                    continue;
                };
                if fallback.templates.is_empty() && fallback_id != "minecraft:empty" {
                    continue;
                }

                let use_local = bb_contains_pos(parent_bb, target_pos);
                let mut candidates = Vec::new();
                if depth != structure.max_depth {
                    candidates.extend(pool.get_shuffled_templates(&mut gen_rng));
                }
                candidates.extend(fallback.get_shuffled_templates(&mut gen_rng));

                let placement_priority = p_jig
                    .nbt
                    .as_ref()
                    .and_then(|t| nbt_int(t, "placement_priority"))
                    .unwrap_or(0);

                'cand_loop: for cand_el in candidates {
                    if matches!(cand_el, PoolElement::Empty) {
                        break 'cand_loop;
                    }
                    let mut rots = [
                        Rotation::None,
                        Rotation::Clockwise90,
                        Rotation::Clockwise180,
                        Rotation::Counterclockwise90,
                    ];
                    util_shuffle(&mut rots, &mut gen_rng);
                    for cand_rot in rots {
                        let cand_jigs = element_shuffled_jigsaws(
                            repo,
                            &cand_el,
                            (0, 0, 0),
                            cand_rot,
                            &mut gen_rng,
                            reg,
                        )?;
                        let cand_zero_bb =
                            element_bounding_box(repo, &cand_el, (0, 0, 0), cand_rot, reg)?;
                        let expansion =
                            if structure.use_expansion_hack && bb_yspan(cand_zero_bb) <= 16 {
                                let mut best = 0i32;
                                for cj in &cand_jigs {
                                    let (cf, _) = jigsaw_front_top(cj.state, reg);
                                    let cn = dir_normal(cf);
                                    let adj = (cj.pos.0 + cn.0, cj.pos.1 + cn.1, cj.pos.2 + cn.2);
                                    if !bb_contains_pos(cand_zero_bb, adj) {
                                        continue;
                                    }
                                    let cj_pool_id = lookup_pool(
                                        &cj.nbt
                                            .as_ref()
                                            .and_then(|t| nbt_str(t, "pool"))
                                            .unwrap_or_else(|| "minecraft:empty".to_owned()),
                                    );
                                    let s1 = pool_max_size(repo, &cj_pool_id, reg)?;
                                    let s2 = if let Some(p) = repo.pool(&cj_pool_id) {
                                        let fb = p.fallback.clone();
                                        pool_max_size(repo, &fb, reg)?
                                    } else {
                                        0
                                    };
                                    best = best.max(s1.max(s2));
                                }
                                best
                            } else {
                                0
                            };

                        for cand_jig in &cand_jigs {
                            if !jigsaws_can_attach(&p_jig, cand_jig, reg) {
                                continue;
                            }
                            let cj_pos = cand_jig.pos;
                            let cand_offset = (
                                target_pos.0 - cj_pos.0,
                                target_pos.1 - cj_pos.1,
                                target_pos.2 - cj_pos.2,
                            );
                            let cand_bb_at_off =
                                element_bounding_box(repo, &cand_el, cand_offset, cand_rot, reg)?;
                            let cand_bb_min_y = cand_bb_at_off[1];
                            let cand_rigid = cand_el.projection() == Projection::Rigid;
                            let cj_y = cj_pos.1;
                            let y_diff = parent_jig_rel_y - cj_y + n.1;
                            let child_anchor_y = if parent_rigid && cand_rigid {
                                parent_min_y + y_diff
                            } else {
                                if cached_hm_k == -1 {
                                    cached_hm_k = world.heightmap(
                                        p_jig_pos.0,
                                        p_jig_pos.2,
                                        HeightmapKind::WorldSurfaceWg,
                                    );
                                }
                                cached_hm_k - cj_y
                            };
                            let move_y = child_anchor_y - cand_bb_min_y;
                            let mut final_bb = bb_move(cand_bb_at_off, 0, move_y, 0);
                            let final_pos = (
                                cand_offset.0,
                                cand_offset.1 + move_y,
                                cand_offset.2,
                            );
                            if expansion > 0 {
                                let expand_by =
                                    (expansion + 1).max(final_bb[4] - final_bb[1]);
                                final_bb[4] = final_bb[4].max(final_bb[1] + expand_by);
                            }

                            let deflated = Aabb::of_bb(final_bb).deflate(0.25);
                            let blocked = if use_local {
                                let slot = local_shapes[parent_children_idx]
                                    .get_or_insert_with(|| FreeShape::from_aabb(Aabb::of_bb(parent_bb)));
                                slot.blocks_cand(&deflated)
                            } else {
                                match &parent_shape_slot {
                                    ShapeSlot::Root => root_shape.blocks_cand(&deflated),
                                    ShapeSlot::Local(idx) => local_shapes[*idx]
                                        .as_ref()
                                        .unwrap()
                                        .blocks_cand(&deflated),
                                }
                            };
                            if blocked {
                                continue;
                            }

                            let active_slot = if use_local {
                                local_shapes[parent_children_idx]
                                    .as_mut()
                                    .unwrap()
                                    .cut_out(Aabb::of_bb(final_bb));
                                ShapeSlot::Local(parent_children_idx)
                            } else {
                                match &parent_shape_slot {
                                    ShapeSlot::Root => root_shape.cut_out(Aabb::of_bb(final_bb)),
                                    ShapeSlot::Local(idx) => {
                                        local_shapes[*idx].as_mut().unwrap().cut_out(Aabb::of_bb(final_bb));
                                    }
                                }
                                parent_shape_slot.clone()
                            };

                            let child_gld = if cand_rigid {
                                parent_gld - y_diff
                            } else {
                                cand_el.ground_level_delta()
                            };

                            let junc_y = if parent_rigid {
                                parent_min_y + parent_jig_rel_y
                            } else if cand_rigid {
                                child_anchor_y + cj_y
                            } else {
                                if cached_hm_k == -1 {
                                    cached_hm_k = world.heightmap(
                                        p_jig_pos.0,
                                        p_jig_pos.2,
                                        HeightmapKind::WorldSurfaceWg,
                                    );
                                }
                                cached_hm_k + y_diff / 2
                            };

                            pieces[parent_idx]
                                .junctions
                                .push(crate::worldgen::BeardJunction {
                                    source_x: target_pos.0,
                                    source_ground_y: junc_y - parent_jig_rel_y + parent_gld,
                                    source_z: target_pos.2,
                                    delta_y: y_diff,
                                    rigid: cand_rigid,
                                });

                            let child_piece = JigsawPiece {
                                element: cand_el.clone(),
                                pos: final_pos,
                                gld: child_gld,
                                rot: cand_rot,
                                bb: final_bb,
                                liquid_settings: structure.liquid_settings,
                                junctions: vec![crate::worldgen::BeardJunction {
                                    source_x: p_jig_pos.0,
                                    source_ground_y: junc_y - cj_y + child_gld,
                                    source_z: p_jig_pos.2,
                                    delta_y: -y_diff,
                                    rigid: parent_rigid,
                                }],
                            };
                            let child_idx = pieces.len();
                            pieces.push(child_piece);
                            if depth + 1 <= structure.max_depth {
                                queue.push((child_idx, active_slot, depth + 1), placement_priority);
                            }
                            break 'cand_loop;
                        }
                    }
                }
            }
            Ok(())
        };

    try_place_children(
        0,
        ShapeSlot::Root,
        0,
        &mut pieces,
        &mut root_shape,
        &mut local_shapes,
        &mut queue,
        world,
    )?;
    while let Some((piece_idx, slot, depth)) = queue.pop() {
        try_place_children(
            piece_idx,
            slot,
            depth,
            &mut pieces,
            &mut root_shape,
            &mut local_shapes,
            &mut queue,
            world,
        )?;
    }

    Ok(Some((stub, pieces)))
}

#[cfg(test)]
fn generate_pieces(
    repo: &mut DatapackRepo,
    structure: &JigsawStructure,
    seed: i64,
    cx: i32,
    cz: i32,
    flat_top: i32,
    reg: &BReg,
) -> Result<Vec<JigsawPiece>, String> {
    let mut world = TestJigsawWorld { flat_top };
    generate_pieces_with_hm(
        repo,
        &mut world,
        structure,
        seed,
        cx,
        cz,
        MIN_Y,
        MAX_Y_EXCL,
        reg,
    )?
    .map(|(_, pieces)| pieces)
    .ok_or_else(|| "start pool element was empty".into())
}

pub(crate) fn generate_jigsaw_pieces_live(
    g: &mut Gen,
    struct_id: &str,
    allowed_biomes: &HashSet<String>,
    seed: u64,
    cx: i32,
    cz: i32,
    reg: &BReg,
) -> Option<(Pos, Vec<JigsawPiece>)> {
    let defs = live_jigsaw_defs(reg)?;
    let structure = defs.structures.get(struct_id)?;
    let mut repo = LiveJigsawRepo(defs);
    let dim_min_y = g.min_y;
    let dim_max_y_excl = g.min_y + g.height;
    let mut world = LiveJigsawWorld { g, allowed_biomes };
    match generate_pieces_with_hm(
        &mut repo,
        &mut world,
        structure,
        seed as i64,
        cx,
        cz,
        dim_min_y,
        dim_max_y_excl,
        reg,
    ) {
        Ok(res) => res,
        Err(e) => {
            if crate::trace_env::env_flag!("SPIPE_TRACE") {
                eprintln!("SPIPE jigsaw {struct_id} ({cx},{cz}) error: {e}");
            }
            None
        }
    }
}

// ---------- Chunk placement (`StructureStart::placeInChunk`) ----------

#[cfg(test)]
fn scene_height(scene: &str, x: i32, z: i32) -> i32 {
    match scene {
        "sloped" => 60 + (x * 3 + z * 7).rem_euclid(9),
        "nether" => 120,
        _ => 64,
    }
}

#[cfg(test)]
fn scene_state(scene: &str, x: i32, y: i32, z: i32, reg: &BReg) -> u32 {
    let top = scene_height(scene, x, z);
    let stone = reg.state_of("minecraft:stone", None);
    let grass = reg.state_of("minecraft:grass_block", None);
    let water = reg.state_of("minecraft:water", None);
    let deepslate = reg.state_of("minecraft:deepslate", None);
    let netherrack = reg.state_of("minecraft:netherrack", None);
    let air = reg.state_of("minecraft:air", None);
    match scene {
        "flat" => if y < top { stone } else { air },
        "grass" | "sloped" => {
            if y < top - 1 {
                stone
            } else if y == top - 1 {
                grass
            } else {
                air
            }
        }
        "flooded" => {
            if y < 52 {
                stone
            } else if y < 64 {
                water
            } else {
                air
            }
        }
        "underground" => if y < 64 { deepslate } else { air },
        "nether" => if y < 120 { netherrack } else { air },
        other => panic!("unknown scene {other}"),
    }
}

enum JigsawTarget<'a, 'g> {
    #[cfg(test)]
    Scene {
        scene: &'a str,
        writes: &'a mut HashMap<Pos, u32>,
        gen: &'a mut crate::worldgen::Gen,
    },
    Live(&'a mut FC<'g>),
}

impl JigsawTarget<'_, '_> {
    fn get_world(&mut self, p: Pos, reg: &BReg) -> u32 {
        match self {
            #[cfg(test)]
            Self::Scene { scene, writes, .. } => writes
                .get(&p)
                .copied()
                .unwrap_or_else(|| scene_state(scene, p.0, p.1, p.2, reg)),
            Self::Live(ctx) => ctx.get(p.0, p.1, p.2),
        }
    }

    fn set_world(&mut self, p: Pos, st: u32) {
        match self {
            #[cfg(test)]
            Self::Scene { writes, .. } => {
                writes.insert(p, st);
            }
            Self::Live(ctx) => {
                ctx.set(p.0, p.1, p.2, st);
            }
        }
    }

    fn gravity_surface(&mut self, kind: HeightmapKind, x: i32, z: i32) -> i32 {
        match self {
            #[cfg(test)]
            Self::Scene { scene, .. } => scene_height(scene, x, z),
            Self::Live(ctx) => ctx.heightmap(kind, x, z),
        }
    }
}

fn parse_final_state(raw: &str, reg: &BReg) -> Option<u32> {
    let raw = raw.trim();
    if let Some((name, rest)) = raw.split_once('[') {
        // `BlockStateParser.parse` (`JigsawReplacementProcessor` -> `egb.a` reads the block
        // name, then skips to the closing `]` and never looks at the rest of the string), so
        // a template property list terminated early still resolves. Measured:
        // `village/savanna/savanna_lamp_post_01.nbt` carries
        // `minecraft:acacia_fence[...west=false]]` (doubled bracket) and native places the
        // fence, while a strict `strip_suffix(']')` dropped it - village_savanna_s1 lost the
        // two lamp-post fences in chunk (0,0) and one each in (2,1)/(3,1).
        let props_str = rest.split_once(']').map(|(p, _)| p)?;
        let mut props = Vec::new();
        for pair in props_str.split(',').filter(|s| !s.is_empty()) {
            let (k, v) = pair.split_once('=')?;
            props.push((k.trim().to_owned(), v.trim().to_owned()));
        }
        reg.checked_state(name.trim(), &props)
    } else {
        reg.checked_state(raw, &[])
    }
}

fn is_randomizable_container_be(name: &str) -> bool {
    matches!(
        name,
        "minecraft:chest"
            | "minecraft:trapped_chest"
            | "minecraft:barrel"
            | "minecraft:dispenser"
            | "minecraft:dropper"
            | "minecraft:hopper"
            | "minecraft:crafter"
            | "minecraft:decorated_pot"
            | "minecraft:shulker_box"
    ) || name.ends_with("_shulker_box")
}

fn can_place_water_liquid(reg: &BReg, st: u32) -> bool {
    let name = reg.name_of(st);
    if !reg.has_prop(name, "waterlogged") {
        return false;
    }
    if name.ends_with("_slab") && reg.get_prop(st, "type").as_deref() == Some("double") {
        return false;
    }
    true
}

fn apply_processor(
    proc: &Processor,
    original: &BlockInfo,
    current: &mut BlockInfo,
    pivot: Pos,
    reg: &BReg,
    target: &mut JigsawTarget<'_, '_>,
) -> bool {
    match proc {
        Processor::BlockIgnore(names) => {
            let n = reg.name_of(current.state);
            if std::env::var("JIGSAW_DBG_PROC").is_ok() {
                eprintln!("  BI name={n} hit={}", names.iter().any(|x| x == n));
            }
            if names.iter().any(|x| x == n) {
                return false;
            }
        }
        Processor::JigsawReplacement => {
            if reg.name_of(current.state) == "minecraft:jigsaw" {
                if let Some(nbt) = &current.nbt {
                    let final_str = nbt_str(nbt, "final_state").unwrap_or_default();
                    let Some(st) = parse_final_state(&final_str, reg) else {
                        return false;
                    };
                    if reg.name_of(st) == "minecraft:structure_void" {
                        return false;
                    }
                    current.state = st;
                    current.nbt = None;
                }
            }
        }
        Processor::BlockRot { integrity, rottable } => {
            let eligible = match rottable {
                None => true,
                Some(RottableBlocks::Names(names)) => {
                    names.iter().any(|n| n == reg.name_of(original.state))
                }
                Some(RottableBlocks::Tag(tag)) => reg.in_tag(original.state, tag),
            };
            if eligible {
                let mut r = Lcg::new(hash_block_pos(current.pos.0, current.pos.1, current.pos.2) as u64);
                if r.next_f32() > *integrity {
                    return false;
                }
            }
        }
        Processor::Protected(tag) => {
            if reg.in_tag(target.get_world(current.pos, reg), tag) {
                return false;
            }
        }
        Processor::Rules(rules) => {
            let mut r = Lcg::new(hash_block_pos(current.pos.0, current.pos.1, current.pos.2) as u64);
            let existing = target.get_world(current.pos, reg);
            if let Some(rule) = rules.iter().find(|rule| {
                rule.input.matches(current.state, reg)
                    && rule.input.roll(&mut r)
                    && rule.location.matches(existing, reg)
                    && rule.location.roll(&mut r)
                    && rule.position.matches(current.pos, pivot, &mut r)
            }) {
                current.state = rule.output;
                let base_nbt = rule.output_nbt.clone().or_else(|| current.nbt.take());
                current.nbt = rule.modifier.apply(&mut r, base_nbt);
            }
        }
        Processor::Gravity { heightmap, offset } => {
            let surface = target.gravity_surface(*heightmap, current.pos.0, current.pos.2);
            current.pos.1 = surface.wrapping_add(*offset).wrapping_add(original.pos.1);
        }
        Processor::Capped { .. } => {}
    }
    true
}

#[allow(clippy::too_many_arguments)]
fn place_single_or_legacy<D: JigsawData>(
    repo: &mut D,
    location: &str,
    processors_ref: &ProcessorRef,
    projection: Projection,
    override_ls: Option<LiquidSettings>,
    legacy: bool,
    piece_pos: Pos,
    start_center: Pos,
    rot: Rotation,
    clip: BBox,
    piece_ls: LiquidSettings,
    world_seed: i64,
    target: &mut JigsawTarget<'_, '_>,
    rng: &mut Xor,
    reg: &BReg,
) -> Result<(), String> {
    let list_procs = repo.resolve_processors(processors_ref)?;
    let tpl = repo.template(location, reg)?;
    if tpl.palettes.is_empty() || tpl.size.0 < 1 || tpl.size.1 < 1 || tpl.size.2 < 1 {
        return Ok(());
    }
    // `SinglePoolElement.getSettings` / `LegacySinglePoolElement.getSettings`, read from the
    // pinned 1.21.1 bytecode (`eky.a(dmm,ejj,end,Z)` + `ekw.a(...)`):
    //   single: addProcessor(BlockIgnoreProcessor.STRUCTURE_BLOCK)          -> `emu.b`
    //   legacy: removeProcessor(STRUCTURE_BLOCK); addProcessor(STRUCTURE_AND_AIR)  -> `emu.d`
    // `enp.a(enq)` appends and `enp.b(enq)` removes (`java/util/List.add` / `.remove`), so the
    // legacy air filter is appended **after** `JigsawReplacementProcessor` — a jigsaw block
    // whose `final_state` is `minecraft:air` is dropped rather than written. `emu.b` ignores
    // `minecraft:structure_block` only; `emu.d` ignores `minecraft:air` and
    // `minecraft:structure_block`.
    let mut processors = Vec::new();
    if !legacy {
        processors.push(Processor::BlockIgnore(vec![
            "minecraft:structure_block".to_owned(),
        ]));
    }
    processors.push(Processor::JigsawReplacement);
    processors.extend(list_procs);
    if projection == Projection::TerrainMatching {
        processors.push(Processor::Gravity {
            heightmap: HeightmapKind::WorldSurfaceWg,
            offset: -1,
        });
    }
    if legacy {
        // `Projection.getProcessors()` is appended before this, and `ekw.a(...)` appends
        // `STRUCTURE_AND_AIR` last: after `JigsawReplacement` has turned jigsaw blocks into
        // their `final_state` (often `minecraft:air`), every air cell is dropped.
        processors.push(Processor::BlockIgnore(vec![
            "minecraft:air".to_owned(),
            "minecraft:structure_block".to_owned(),
        ]));
    }

    // StructurePlaceSettings.getRandomPalette uses getRandom(piece_pos) when palettes.len() > 1
    let palette_idx = if tpl.palettes.len() > 1 {
        let mut r = Lcg::new(hash_block_pos(piece_pos.0, piece_pos.1, piece_pos.2) as u64);
        r.next_bound(tpl.palettes.len() as i32) as usize
    } else {
        0
    };
    let raw_blocks = &tpl.palettes[palette_idx];

    let mut kept_originals = Vec::new();
    let mut processed = Vec::new();
    for original in raw_blocks {
        let tp = transform_block(original.pos, Mirror::None, rot, (0, 0, 0));
        let mut current = original.clone();
        current.pos = (
            tp.0.wrapping_add(piece_pos.0),
            tp.1.wrapping_add(piece_pos.1),
            tp.2.wrapping_add(piece_pos.2),
        );
        let mut keep = true;
        for proc in &processors {
            if std::env::var("JIGSAW_DBG_PROC").is_ok() {
                if let Processor::BlockIgnore(names) = proc {
                    eprintln!(
                        "PROC legacy={legacy} state={} name={} ignores={:?}",
                        original.state,
                        reg.name_of(original.state),
                        names
                    );
                }
            }
            if !apply_processor(proc, original, &mut current, start_center, reg, target) {
                keep = false;
                break;
            }
        }
        if keep {
            kept_originals.push(original);
            processed.push(current);
        }
    }

    // CappedProcessor second pass
    for proc in &processors {
        if let Processor::Capped { delegate, limit } = proc {
            if limit.max_inclusive() == 0 || processed.is_empty() {
                continue;
            }
            let factory_seed = Lcg::new(world_seed as u64).next_i64();
            let mut cap_rng = Lcg::new(
                (hash_block_pos(piece_pos.0, piece_pos.1, piece_pos.2) ^ factory_seed) as u64,
            );
            let cap = limit.sample(&mut cap_rng).min(processed.len() as i32);
            if cap < 1 {
                continue;
            }
            let mut indices: Vec<usize> = (0..processed.len()).collect();
            util_shuffle(&mut indices, &mut cap_rng);
            let mut replaced = 0i32;
            for idx in indices {
                if replaced >= cap {
                    break;
                }
                let curr = &processed[idx];
                let mut cand = curr.clone();
                if apply_processor(
                    delegate,
                    kept_originals[idx],
                    &mut cand,
                    start_center,
                    reg,
                    target,
                ) && (cand.pos != curr.pos || cand.state != curr.state || cand.nbt != curr.nbt)
                {
                    replaced += 1;
                    processed[idx] = cand;
                }
            }
        }
    }

    let effective_ls = override_ls.unwrap_or(piece_ls);
    let apply_waterlogging = effective_ls == LiquidSettings::ApplyWaterlogging;
    let mut fluid_sources: HashSet<Pos> = HashSet::new();
    let mut to_waterlog: Vec<Pos> = Vec::new();

    for info in processed {
        let p = info.pos;
        if !bb_contains_pos(clip, p) {
            continue;
        }
        if p.1 < clip[1] || p.1 > clip[4] {
            continue;
        }
        let existing_fluid_is_water_source = if apply_waterlogging {
            let ex = target.get_world(p, reg);
            reg.fluid_name(ex) == "minecraft:water"
        } else {
            false
        };
        let placed = transform_state(info.state, Mirror::None, rot).unwrap_or(info.state);
        if reg.name_of(placed) == "minecraft:air" && std::env::var("JIGSAW_DBG_AIR").is_ok() {
            eprintln!(
                "AIRWRITE {},{},{} from {location} legacy={legacy} orig={} cur={} placed={}",
                p.0,
                p.1,
                p.2,
                info.state,
                info.state,
                placed
            );
        }
        target.set_world(p, placed);

        if let Some(nbt) = info.nbt.as_ref() {
            let bname = reg.name_of(placed);
            if is_randomizable_container_be(bname) {
                let loot_seed = rng.next_i64_legacy();
                if let JigsawTarget::Live(ctx) = &mut *target {
                    if let Some(table) = nbt_str(nbt, "LootTable") {
                        let table = ns_name(&table);
                        let is_furnace = match bname {
                            "minecraft:chest" | "minecraft:trapped_chest" | "minecraft:barrel" => {
                                Some(false)
                            }
                            "minecraft:furnace" | "minecraft:blast_furnace" | "minecraft:smoker" => {
                                Some(true)
                            }
                            _ => None,
                        };
                        if let Some(is_furnace) = is_furnace {
                            ctx.push_loot_container(p, is_furnace, table, Some(loot_seed));
                        } else if crate::trace_env::env_flag!("SPIPE_PAYLOAD") {
                            eprintln!("SPIPE_PAYLOAD jigsaw {bname} @ {p:?} table={table}");
                        }
                    }
                }
            } else if bname == "minecraft:spawner" {
                if let JigsawTarget::Live(ctx) = &mut *target {
                    let entity_id = nbt
                        .get("SpawnData")
                        .and_then(|d| d.get("entity"))
                        .and_then(|e| nbt_str(e, "id"))
                        .map(|s| ns_name(&s));
                    if let Some(entity_id) = entity_id {
                        ctx.push_spawner(p, &entity_id);
                    }
                }
            }
        }

        if apply_waterlogging {
            if reg.fluid_name(placed) == "minecraft:water" {
                fluid_sources.insert(p);
            } else if can_place_water_liquid(reg, placed) {
                if existing_fluid_is_water_source {
                    let wl = reg.with_prop(placed, "waterlogged", "true");
                    target.set_world(p, wl);
                } else {
                    to_waterlog.push(p);
                }
            }
        }
    }

    if apply_waterlogging && !to_waterlog.is_empty() {
        const DIRS: [Pos; 5] = [(0, 1, 0), (0, 0, -1), (1, 0, 0), (0, 0, 1), (-1, 0, 0)];
        let mut changed = true;
        while changed {
            changed = false;
            let mut next_pending = Vec::with_capacity(to_waterlog.len());
            for p in to_waterlog {
                let st = target.get_world(p, reg);
                if reg.fluid_name(st) == "minecraft:water" {
                    continue;
                }
                let mut found_source = false;
                for d in DIRS {
                    let nb = (p.0 + d.0, p.1 + d.1, p.2 + d.2);
                    if fluid_sources.contains(&nb) {
                        continue;
                    }
                    let nb_st = target.get_world(nb, reg);
                    if reg.fluid_name(nb_st) == "minecraft:water" {
                        found_source = true;
                        break;
                    }
                }
                if found_source && can_place_water_liquid(reg, st) {
                    let wl = reg.with_prop(st, "waterlogged", "true");
                    target.set_world(p, wl);
                    changed = true;
                } else {
                    next_pending.push(p);
                }
            }
            to_waterlog = next_pending;
        }
    }

    if let JigsawTarget::Live(ctx) = &mut *target {
        for info in &tpl.entities {
            let btp = transform_block(info.block_pos, Mirror::None, rot, (0, 0, 0));
            let block_pos = (
                btp.0.wrapping_add(piece_pos.0),
                btp.1.wrapping_add(piece_pos.1),
                btp.2.wrapping_add(piece_pos.2),
            );
            if !bb_contains_pos(clip, block_pos) {
                continue;
            }
            let Some(id) = nbt_str(&info.nbt, "id") else {
                continue;
            };
            let id = ns_name(&id);
            if matches!(
                id.as_str(),
                "minecraft:armor_stand"
                    | "minecraft:item_frame"
                    | "minecraft:glow_item_frame"
                    | "minecraft:painting"
            ) {
                if crate::trace_env::env_flag!("SPIPE_PAYLOAD") {
                    eprintln!("SPIPE_PAYLOAD unrouted jigsaw entity {id} at {block_pos:?}");
                }
                continue;
            }
            let tp = crate::structure_transform::entity(info.pos, Mirror::None, rot, (0, 0, 0));
            let pos = [
                tp[0] + f64::from(piece_pos.0),
                tp[1] + f64::from(piece_pos.1),
                tp[2] + f64::from(piece_pos.2),
            ];
            ctx.push_entity(&id, pos, 0.0);
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn place_element<D: JigsawData>(
    repo: &mut D,
    element: &PoolElement,
    piece_pos: Pos,
    start_center: Pos,
    rot: Rotation,
    clip: BBox,
    piece_ls: LiquidSettings,
    world_seed: i64,
    target: &mut JigsawTarget<'_, '_>,
    rng: &mut Xor,
    reg: &BReg,
) -> Result<(), String> {
    match element {
        PoolElement::Empty => Ok(()),
        PoolElement::Single {
            location,
            processors,
            projection,
            override_liquid_settings,
        } => place_single_or_legacy(
            repo,
            location,
            processors,
            *projection,
            *override_liquid_settings,
            false,
            piece_pos,
            start_center,
            rot,
            clip,
            piece_ls,
            world_seed,
            target,
            rng,
            reg,
        ),
        PoolElement::LegacySingle {
            location,
            processors,
            projection,
            override_liquid_settings,
        } => place_single_or_legacy(
            repo,
            location,
            processors,
            *projection,
            *override_liquid_settings,
            true,
            piece_pos,
            start_center,
            rot,
            clip,
            piece_ls,
            world_seed,
            target,
            rng,
            reg,
        ),
        PoolElement::Feature { feature, .. } => match target {
            #[cfg(test)]
            JigsawTarget::Scene { scene, writes, gen } => {
                let mut ctx = FC::new(*gen);
                ctx.test_default_air = true;
                ctx.test_biome("minecraft:plains");
                let r = 16;
                for x in (piece_pos.0 - r)..=(piece_pos.0 + r) {
                    for z in (piece_pos.2 - r)..=(piece_pos.2 + r) {
                        let top = scene_height(scene, x, z);
                        ctx.test_frozen_height(HeightmapKind::WorldSurfaceWg, x, z, top);
                        ctx.test_frozen_height(HeightmapKind::OceanFloorWg, x, z, top);
                        let y_lo = (top - 8).min(piece_pos.1 - 8).max(MIN_Y);
                        let y_hi = (top + 2).max(piece_pos.1 + 8).min(MAX_Y_EXCL - 1);
                        for y in y_lo..=y_hi {
                            let st = scene_state(scene, x, y, z, reg);
                            if st != 0 {
                                ctx.test_base_cell((x, y, z), st);
                            }
                        }
                    }
                }
                for (&p, &st) in writes.iter() {
                    if (p.0 - piece_pos.0).abs() <= r && (p.2 - piece_pos.2).abs() <= r {
                        ctx.wr.insert(p, st);
                    }
                }
                let ok = gen_placed_feature_by_name(feature, &mut ctx, rng, piece_pos);
                if !ok {
                    return Err(format!("unknown placed feature in FeaturePoolElement: {feature}"));
                }
                for (p, st) in ctx.wr {
                    writes.insert(p, st);
                }
                Ok(())
            }
            JigsawTarget::Live(ctx) => {
                let ok = gen_placed_feature_by_name(feature, *ctx, rng, piece_pos);
                if !ok {
                    return Err(format!("unknown placed feature in FeaturePoolElement: {feature}"));
                }
                Ok(())
            }
        },
        PoolElement::List { elements, .. } => {
            for sub in elements {
                place_element(
                    repo,
                    sub,
                    piece_pos,
                    start_center,
                    rot,
                    clip,
                    piece_ls,
                    world_seed,
                    target,
                    rng,
                    reg,
                )?;
            }
            Ok(())
        }
    }
}

pub(crate) fn place_jigsaw_chunk_live(
    ctx: &mut FC<'_>,
    pieces: &[JigsawPiece],
    world_seed: i64,
    rng: &mut Xor,
    clip: BBox,
    reg: &BReg,
) -> Result<(), String> {
    if pieces.is_empty() {
        return Ok(());
    }
    let Some(defs) = live_jigsaw_defs(reg) else {
        return Err("jigsaw definitions failed to load".into());
    };
    let mut repo = LiveJigsawRepo(defs);
    let first_bb = pieces[0].bb;
    let start_center = (
        first_bb[0] + (first_bb[3] - first_bb[0] + 1) / 2,
        first_bb[1],
        first_bb[2] + (first_bb[5] - first_bb[2] + 1) / 2,
    );
    let mut target = JigsawTarget::Live(ctx);
    for piece in pieces {
        if jigsaw_bb_intersects(piece.bb, clip) {
            place_element(
                &mut repo,
                &piece.element,
                piece.pos,
                start_center,
                piece.rot,
                clip,
                piece.liquid_settings,
                world_seed,
                &mut target,
                rng,
                reg,
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
fn decoration_rng(seed: i64, cx: i32, cz: i32) -> Xor {
    let mut r = Xor::seed_worldgen(seed as u64);
    let l = r.next_i64_legacy() | 1;
    let m = r.next_i64_legacy() | 1;
    let bx = (cx << 4) as i64;
    let bz = (cz << 4) as i64;
    let deco = bx.wrapping_mul(l).wrapping_add(bz.wrapping_mul(m)) ^ seed;
    Xor::seed_worldgen(deco as u64)
}

#[cfg(test)]
fn place_chunk(
    repo: &mut DatapackRepo,
    pieces: &[JigsawPiece],
    world_seed: i64,
    cx: i32,
    cz: i32,
    scene: &str,
    gen: &mut crate::worldgen::Gen,
    reg: &BReg,
) -> Result<(usize, String, i64, String), String> {
    let clip: BBox = [cx << 4, MIN_Y, cz << 4, (cx << 4) + 15, MAX_Y_EXCL - 1, (cz << 4) + 15];
    let first_bb = pieces[0].bb;
    let start_center = (
        first_bb[0] + (first_bb[3] - first_bb[0] + 1) / 2,
        first_bb[1],
        first_bb[2] + (first_bb[5] - first_bb[2] + 1) / 2,
    );
    let mut rng = decoration_rng(world_seed, cx, cz);
    let mut writes: HashMap<Pos, u32> = HashMap::new();
    let mut target = JigsawTarget::Scene {
        scene,
        writes: &mut writes,
        gen,
    };

    for piece in pieces {
        if jigsaw_bb_intersects(piece.bb, clip) {
            place_element(
                repo,
                &piece.element,
                piece.pos,
                start_center,
                piece.rot,
                clip,
                piece.liquid_settings,
                world_seed,
                &mut target,
                &mut rng,
                reg,
            )?;
        }
    }

    let mut entries: Vec<(Pos, u32)> = writes.into_iter().collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let mut buf = String::new();
    for ((x, y, z), st) in &entries {
        buf.push_str(&format!("{x},{y},{z}={}\n", reg.canonical_state(*st)));
    }
    let w_sha = hex(&sha256(buf.as_bytes()));
    let rng_after = rng.next_i64_legacy();
    Ok((entries.len(), w_sha, rng_after, buf))
}

#[cfg(test)]
pub(crate) mod jigsaw_structures {
    pub(crate) mod tests {
        use super::super::*;

    #[test]
    fn jigsaw_datapacks_parse_without_losses() {
        let reg = breg().expect("block registry");
        let repo = DatapackRepo::load(reg).expect("all jigsaw datapacks parse");
        assert_eq!(repo.structures.len(), 10, "expected 10 jigsaw structures");
        assert_eq!(repo.pools.len(), 186, "expected 186 template pools");
        assert_eq!(
            repo.processor_lists.len(),
            40,
            "expected 40 vanilla processor lists"
        );
        for expected in [
            "minecraft:village_plains",
            "minecraft:village_desert",
            "minecraft:village_savanna",
            "minecraft:village_snowy",
            "minecraft:village_taiga",
            "minecraft:pillager_outpost",
            "minecraft:ancient_city",
            "minecraft:bastion_remnant",
            "minecraft:trail_ruins",
            "minecraft:trial_chambers",
        ] {
            assert!(
                repo.structures.contains_key(expected),
                "missing structure {expected}"
            );
        }
        // Fail-closed checks for unknown types
        let bad_el = jparse(br#"{"element_type":"minecraft:bogus"}"#).unwrap();
        assert!(PoolElement::parse(&bad_el, reg).is_err());
        let bad_proc = jparse(br#"{"processor_type":"minecraft:bogus"}"#).unwrap();
        assert!(Processor::parse(&bad_proc, reg).is_err());
        let bad_alias = jparse(br#"{"type":"minecraft:bogus"}"#).unwrap();
        assert!(PoolAliasBinding::parse(&bad_alias).is_err());
    }

    #[test]
    fn jigsaw_structures_match_1211_native_fixture() {
        let reg = breg().expect("block registry");
        let mut repo = DatapackRepo::load(reg).expect("load datapacks");
        let mut gen = crate::worldgen::Gen::load(0, crate::name_to_state).expect("gen load");

        let tsv = include_str!("../tests/data/jigsaw_structures_1211.tsv");
        let mut current_case: Option<(String, i64, String, Vec<JigsawPiece>)> = None;
        let mut rows = 0usize;
        let mut mismatches = Vec::new();

        for line in tsv.lines() {
            let f: Vec<&str> = line.split('\t').collect();
            match f[0] {
                "CASE" => {
                    assert_eq!(f.len(), 12);
                    let id = f[1];
                    let struct_id = f[2];
                    let seed: i64 = f[3].parse().unwrap();
                    let cx: i32 = f[4].parse().unwrap();
                    let cz: i32 = f[5].parse().unwrap();
                    let scene = f[6];
                    let flat_top: i32 = f[7].parse().unwrap();
                    let exp_count: usize = f[8].parse().unwrap();
                    let exp_sha = f[9];
                    let exp_first_el = f[10];
                    let exp_first_rot: u8 = f[11].parse().unwrap();

                    let structure = repo
                        .structures
                        .get(struct_id)
                        .unwrap_or_else(|| panic!("missing {struct_id}"))
                        .clone();
                    let pieces =
                        generate_pieces(&mut repo, &structure, seed, cx, cz, flat_top, reg)
                            .unwrap_or_else(|e| panic!("{id}: {e}"));

                    let mut desc = String::new();
                    for (i, p) in pieces.iter().enumerate() {
                        desc.push_str(&format!(
                            "{i}\t{}\t{},{},{}\t{}\t{}\t{},{},{},{},{},{}\t{}\n",
                            p.element.describe(),
                            p.pos.0,
                            p.pos.1,
                            p.pos.2,
                            p.rot as u8,
                            p.gld,
                            p.bb[0],
                            p.bb[1],
                            p.bb[2],
                            p.bb[3],
                            p.bb[4],
                            p.bb[5],
                            p.liquid_settings as u8,
                        ));
                    }
                    let got_sha = hex(&sha256(desc.as_bytes()));
                    if pieces.len() != exp_count
                        || pieces[0].element.describe() != exp_first_el
                        || pieces[0].rot as u8 != exp_first_rot
                        || got_sha != exp_sha
                    {
                        mismatches.push(format!(
                            "CASE {id}: count={}/{} first={}/{} rot={}/{} sha={}/{}",
                            pieces.len(),
                            exp_count,
                            pieces[0].element.describe(),
                            exp_first_el,
                            pieces[0].rot as u8,
                            exp_first_rot,
                            &got_sha[..12],
                            &exp_sha[..12],
                        ));
                    }

                    current_case = Some((id.to_owned(), seed, scene.to_owned(), pieces));
                    rows += 1;
                }
                "CHUNK" => {
                    assert_eq!(f.len(), 7);
                    let id = f[1];
                    let cx: i32 = f[2].parse().unwrap();
                    let cz: i32 = f[3].parse().unwrap();
                    let exp_writes: usize = f[4].parse().unwrap();
                    let exp_sha = f[5];
                    let exp_rng: i64 = f[6].parse().unwrap();

                    let (case_id, seed, scene, pieces) =
                        current_case.as_ref().expect("CHUNK before CASE");
                    assert_eq!(case_id, id);

                    let (writes_len, w_sha, rng_after, _buf) =
                        place_chunk(&mut repo, pieces, *seed, cx, cz, scene, &mut gen, reg)
                            .unwrap_or_else(|e| panic!("{id} chunk {cx},{cz}: {e}"));
                    if writes_len != exp_writes || w_sha != exp_sha || rng_after != exp_rng {
                        mismatches.push(format!(
                            "CHUNK {id} ({cx},{cz}): writes={writes_len}/{exp_writes} rng={rng_after}/{exp_rng} sha={}/{}",
                            &w_sha[..12],
                            &exp_sha[..12],
                        ));
                    }
                    rows += 1;
                }
                other => panic!("unexpected row kind {other}"),
            }
        }
        assert_eq!(rows, 140, "expected all 140 rows in fixture");
        assert!(
            mismatches.is_empty(),
            "{} mismatches:\n{}",
            mismatches.len(),
            mismatches.join("\n")
        );
    }
    }
}
