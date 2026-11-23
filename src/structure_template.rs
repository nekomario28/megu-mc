//! StructureTemplate load, processing and bounded full-cube terrain placement.
//! Load/processing remain read-only; solid placement writes through the generation context.
use crate::block_registry::BReg;
use crate::nbt::{NbtString, Tag};
use std::collections::BTreeMap;

type Pos = (i32, i32, i32);
#[derive(Clone, Debug)]
pub(crate) struct BlockInfo {
    pub pos: Pos,
    pub state: u32,
    pub nbt: Option<Tag>,
}
#[derive(Clone, Debug)]
pub(crate) struct EntityInfo {
    pub pos: [f64; 3],
    pub block_pos: Pos,
    pub nbt: Tag,
}
/// One `placeEntities` record after the mirror/rotation transform: `pos` is the
/// stored doubles `transform(Vec3)` produces plus the placement origin — the
/// exact triple `Entity.moveTo` reads — `block_pos` is the anchor the clip-box
/// filter tests, `id` is the entity type the constructor resolves, and
/// `yaw`/`pitch` are what `moveTo` applies (`pitch` is never transformed).
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(not(test), allow(dead_code))]
#[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
pub(crate) struct PlacedEntity {
    pub pos: [f64; 3],
    pub block_pos: Pos,
    pub yaw: f32,
    pub pitch: f32,
    pub id: String,
}
#[derive(Clone, Debug)]
pub(crate) struct Template {
    pub size: Pos,
    pub palettes: Vec<Vec<BlockInfo>>,
    pub entities: Vec<EntityInfo>,
}

fn integer_pos(value: &Tag) -> Result<Pos, &'static str> {
    let values = value
        .list()
        .ok_or("template position must be an int list")?;
    if values.len() != 3 {
        return Err("template position must have three coordinates");
    }
    Ok((
        values[0].int().ok_or("template coordinate must be int")?,
        values[1].int().ok_or("template coordinate must be int")?,
        values[2].int().ok_or("template coordinate must be int")?,
    ))
}
fn field<'a>(value: &'a Tag, key: &str) -> Result<&'a Tag, &'static str> {
    value.get(key).ok_or("missing template field")
}
fn list<'a>(value: &'a Tag, key: &str) -> Result<&'a [Tag], &'static str> {
    field(value, key)?
        .list()
        .ok_or("template field must be list")
}
fn state_from_tag(value: &Tag, reg: &BReg) -> Result<u32, &'static str> {
    let name = field(value, "Name")?
        .string()
        .ok_or("palette name must be string")?
        .to_utf8()
        .map_err(|_| "invalid palette name UTF-16")?;
    let mut properties = Vec::new();
    if let Some(value) = value.get("Properties") {
        let Tag::Compound(values) = value else {
            return Err("palette properties must be compound");
        };
        for (key, value) in values {
            properties.push((
                key.to_utf8().map_err(|_| "invalid property key")?,
                value
                    .string()
                    .ok_or("property value must be string")?
                    .to_utf8()
                    .map_err(|_| "invalid property value")?,
            ));
        }
    }
    reg.checked_state(&name, &properties)
        .ok_or("unknown palette block or property")
}

/// Coordinates are transformed before processing, states only during placement.
#[derive(Clone, Copy)]
pub(crate) struct Placement {
    pub origin: Pos,
    pub pivot: Pos,
    pub mirror: crate::structure_transform::Mirror,
    pub rotation: crate::structure_transform::Rotation,
}

fn state_from_json(value: &crate::json::J, reg: &BReg) -> Result<u32, &'static str> {
    use crate::json::J;
    let Some(J::Str(name)) = value.get("Name") else {
        return Err("missing template state name");
    };
    let mut properties = Vec::new();
    if let Some(props) = value.get("Properties") {
        let J::Obj(props) = props else {
            return Err("template state properties must be an object");
        };
        for (key, val) in props {
            let J::Str(val) = val else {
                return Err("template state property must be a string");
            };
            properties.push((key.clone(), val.clone()));
        }
    }
    reg.checked_state(name, &properties)
        .ok_or("unknown template state")
}

/// `Entity.load` reads yaw/pitch from the `Rotation` list, which native writes
/// as exactly two floats. A missing list defaults both to `0.0`; a present one
/// that is not a float list fails closed — the spawn path would otherwise lose
/// the entity to `loadEntityRecursive`'s empty `Optional`.
#[cfg_attr(not(test), allow(dead_code))]
#[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
fn entity_rotation(nbt: &Tag) -> Result<(f32, f32), &'static str> {
    let Some(tag) = nbt.get("Rotation") else {
        return Ok((0.0, 0.0));
    };
    let items = tag
        .list()
        .ok_or("template entity Rotation must be a float list")?;
    let at = |i: usize| -> Result<f32, &'static str> {
        match items.get(i) {
            None => Ok(0.0),
            Some(Tag::Float(bits)) => Ok(f32::from_bits(*bits)),
            Some(_) => Err("template entity Rotation must be a float list"),
        }
    };
    let (yaw, pitch) = (at(0)?, at(1)?);
    if !yaw.is_finite() || !pitch.is_finite() {
        return Err("template entity Rotation is not finite");
    }
    Ok((yaw, pitch))
}

/// `Mth.wrapDegrees`: fold an angle into `[-180, 180)`.
#[cfg_attr(not(test), allow(dead_code))]
#[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
fn wrap_degrees(angle: f32) -> f32 {
    let wrapped = angle % 360.0;
    if wrapped >= 180.0 {
        wrapped - 360.0
    } else if wrapped < -180.0 {
        wrapped + 360.0
    } else {
        wrapped
    }
}

/// The yaw `placeEntities` computes: `f = entity.mirror(mirror)` then
/// `f += entity.rotate(rotation) - entity.getYRot()`.
///
/// `Entity.mirror` (`bsr.a(dkv)`) is `wrapDegrees(yaw)`, then `-w` for
/// `FRONT_BACK` (inverts x — this port's `FrontBack`) and `180 - w` for
/// `LEFT_RIGHT` (inverts z — this port's `LeftRight`); `Entity.rotate`
/// (`bsr.a(dmm)`) is `wrapDegrees(yaw)` plus 0/90/180/270 for
/// none/CW90/CW180/CCW90. `getYRot()` in the subtraction is the stored yaw
/// un-updated, so `w` is used twice rather than the wrapped result replacing it.
#[cfg_attr(not(test), allow(dead_code))]
#[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
fn transformed_yaw(
    raw_yaw: f32,
    mirror: crate::structure_transform::Mirror,
    rotation: crate::structure_transform::Rotation,
) -> f32 {
    use crate::structure_transform::{Mirror, Rotation};
    let wrapped = wrap_degrees(raw_yaw);
    let mirrored = match mirror {
        Mirror::None => wrapped,
        Mirror::LeftRight => 180.0 - wrapped,
        Mirror::FrontBack => -wrapped,
    };
    let rotated = wrapped
        + match rotation {
            Rotation::None => 0.0,
            Rotation::Clockwise90 => 90.0,
            Rotation::Clockwise180 => 180.0,
            Rotation::Counterclockwise90 => 270.0,
        };
    mirrored + rotated - raw_yaw
}

fn parse_tag_identity(raw: &str, require_hash: bool) -> Result<String, &'static str> {
    let tag = if require_hash {
        raw.strip_prefix('#')
            .filter(|tag| !tag.is_empty())
            .ok_or("protected blocks requires a tag identity")?
    } else {
        if raw.is_empty() || raw.starts_with('#') {
            return Err("invalid tag identity");
        }
        raw
    };
    let tag = crate::block_registry::ns_name(tag);
    let (namespace, path) = tag
        .split_once(':')
        .ok_or("invalid tag identity")?;
    if namespace.is_empty()
        || !namespace.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || b"_.-".contains(&b)
        })
        || path.is_empty()
        || !path.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || b"/_.-".contains(&b)
        })
    {
        return Err("invalid tag identity");
    }
    Ok(tag)
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
    fn parse(value: &crate::json::J, reg: &BReg) -> Result<Self, &'static str> {
        use crate::json::J;
        match value.get("predicate_type").map(J::str) {
            Some("minecraft:always_true") => Ok(Self::Always),
            Some("minecraft:block_match") => {
                let name = value.get("block").ok_or("missing rule block")?.str();
                let state = reg.checked_state(name, &[]).ok_or("unknown rule block")?;
                Ok(Self::Block(reg.name_of(state).to_owned()))
            }
            Some("minecraft:random_block_match") => {
                let name = value.get("block").ok_or("missing rule block")?.str();
                let state = reg.checked_state(name, &[]).ok_or("unknown rule block")?;
                let Some(J::Num(prob)) = value.get("probability") else {
                    return Err("missing random block match probability");
                };
                let probability = *prob as f32;
                if !probability.is_finite() {
                    return Err("invalid random block match probability");
                }
                Ok(Self::RandomBlock {
                    name: reg.name_of(state).to_owned(),
                    probability,
                })
            }
            Some("minecraft:tag_match") => {
                let Some(J::Str(tag)) = value.get("tag") else {
                    return Err("missing rule tag");
                };
                Ok(Self::Tag(parse_tag_identity(tag, false)?))
            }
            Some("minecraft:blockstate_match") => {
                let bs = value
                    .get("block_state")
                    .ok_or("missing rule block_state")?;
                Ok(Self::BlockState(state_from_json(bs, reg)?))
            }
            Some("minecraft:random_blockstate_match") => {
                let bs = value
                    .get("block_state")
                    .ok_or("missing rule block_state")?;
                let Some(J::Num(prob)) = value.get("probability") else {
                    return Err("missing random blockstate match probability");
                };
                let probability = *prob as f32;
                if !probability.is_finite() {
                    return Err("invalid random blockstate match probability");
                }
                Ok(Self::RandomBlockState {
                    state: state_from_json(bs, reg)?,
                    probability,
                })
            }
            _ => Err("unsupported template rule test"),
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
    fn roll(&self, rng: &mut crate::generation_math::Lcg) -> bool {
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
    fn opt_f32(value: &crate::json::J, key: &str, default: f32) -> Result<f32, &'static str> {
        match value.get(key) {
            None => Ok(default),
            Some(crate::json::J::Num(v)) if (*v as f32).is_finite() => Ok(*v as f32),
            _ => Err("invalid position predicate float field"),
        }
    }
    fn opt_i32(value: &crate::json::J, key: &str, default: i32) -> Result<i32, &'static str> {
        match value.get(key) {
            None => Ok(default),
            Some(crate::json::J::Num(v)) if v.is_finite() && v.fract() == 0.0 => Ok(*v as i32),
            _ => Err("invalid position predicate int field"),
        }
    }
    fn parse(value: &crate::json::J) -> Result<Self, &'static str> {
        use crate::json::J;
        match value.get("predicate_type").map(J::str) {
            Some("minecraft:always_true") => Ok(Self::Always),
            Some("minecraft:axis_aligned_linear_pos") => {
                let min_chance = Self::opt_f32(value, "min_chance", 0.0)?;
                let max_chance = Self::opt_f32(value, "max_chance", 0.0)?;
                let min_dist = Self::opt_i32(value, "min_dist", 0)?;
                let max_dist = Self::opt_i32(value, "max_dist", 0)?;
                if min_dist >= max_dist {
                    return Err("axis_aligned_linear_pos requires min_dist < max_dist");
                }
                let axis = match value.get("axis") {
                    None => 1,
                    Some(J::Str(a)) => match a.as_str() {
                        "x" => 0,
                        "y" => 1,
                        "z" => 2,
                        _ => return Err("unknown axis in axis_aligned_linear_pos"),
                    },
                    _ => return Err("axis must be a string"),
                };
                Ok(Self::AxisAlignedLinearPos {
                    min_chance,
                    max_chance,
                    min_dist,
                    max_dist,
                    axis,
                })
            }
            Some("minecraft:linear_pos") => {
                let min_chance = Self::opt_f32(value, "min_chance", 0.0)?;
                let max_chance = Self::opt_f32(value, "max_chance", 0.0)?;
                let min_dist = Self::opt_i32(value, "min_dist", 0)?;
                let max_dist = Self::opt_i32(value, "max_dist", 0)?;
                if min_dist >= max_dist {
                    return Err("linear_pos requires min_dist < max_dist");
                }
                Ok(Self::LinearPos {
                    min_chance,
                    max_chance,
                    min_dist,
                    max_dist,
                })
            }
            _ => Err("unsupported template rule position test"),
        }
    }
    fn clamped_chance(dist: i32, min_dist: i32, max_dist: i32, min_chance: f32, max_chance: f32) -> f32 {
        let t = (dist as f32 - min_dist as f32) / (max_dist as f32 - min_dist as f32);
        if t < 0.0 {
            min_chance
        } else if t > 1.0 {
            max_chance
        } else {
            min_chance + t * (max_chance - min_chance)
        }
    }
    fn matches(
        &self,
        world_pos: Pos,
        pivot: Pos,
        rng: &mut crate::generation_math::Lcg,
    ) -> bool {
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
                let dx = (world_pos.0.wrapping_sub(pivot.0)) as f32;
                let dy = (world_pos.1.wrapping_sub(pivot.1)) as f32;
                let dz = (world_pos.2.wrapping_sub(pivot.2)) as f32;
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
    fn parse(value: &crate::json::J) -> Result<Self, &'static str> {
        use crate::json::J;
        match value.get("type").map(J::str) {
            Some("minecraft:passthrough") => Ok(Self::Passthrough),
            Some("minecraft:append_loot") => {
                let Some(J::Str(loot_table)) = value.get("loot_table") else {
                    return Err("append_loot requires loot_table string");
                };
                if loot_table.is_empty() {
                    return Err("append_loot loot_table cannot be empty");
                }
                Ok(Self::AppendLoot(crate::block_registry::ns_name(loot_table)))
            }
            _ => Err("unsupported template rule block entity modifier"),
        }
    }
    fn apply(&self, rng: &mut crate::generation_math::Lcg, current: Option<Tag>) -> Option<Tag> {
        match self {
            Self::Passthrough => current,
            Self::AppendLoot(loot_table) => {
                let mut map = match current {
                    Some(Tag::Compound(map)) => map,
                    _ => BTreeMap::new(),
                };
                map.insert(
                    NbtString::text("LootTable"),
                    Tag::String(NbtString::text(loot_table)),
                );
                map.insert(NbtString::text("LootTableSeed"), Tag::Long(rng.next_i64()));
                Some(Tag::Compound(map))
            }
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct BlockRule {
    input: RuleTest,
    location: RuleTest,
    position: PosRuleTest,
    output: u32,
    modifier: BlockEntityModifier,
}

#[derive(Clone, Debug)]
pub(crate) enum RottableBlocks {
    Names(Vec<String>),
    Tag(String),
}

/// Supported processor shapes mapped to native 1.21.1 `StructureProcessor` semantics.
#[derive(Clone, Debug)]
pub(crate) enum Processor {
    BlockIgnore(Vec<String>),
    BlockRot {
        integrity: f32,
        rottable: Option<RottableBlocks>,
    },
    Protected(String),
    Rules(Vec<BlockRule>),
    Gravity {
        heightmap: crate::block_registry::HeightmapKind,
        offset: i32,
    },
    BlackstoneReplace,
    LavaSubmergedBlock,
    Capped {
        delegate: Box<Processor>,
        limit: CappedLimit,
    },
}

#[derive(Clone, Debug)]
pub(crate) enum CappedLimit {
    Const(i32),
    Uniform(i32, i32),
    BiasBottom(i32, i32),
    VeryBias(i32, i32),
    Clamped(Box<CappedLimit>, i32, i32),
    Trap(i32, i32, i32),
    Norm(f64, f64, i32, i32),
    Weighted(Vec<(i32, CappedLimit)>),
}

impl CappedLimit {
    fn min_inclusive(&self) -> i32 {
        match self {
            Self::Const(v) => *v,
            Self::Uniform(a, _)
            | Self::BiasBottom(a, _)
            | Self::VeryBias(a, _)
            | Self::Trap(a, _, _) => *a,
            Self::Clamped(_, a, _) => *a,
            Self::Norm(_, _, a, _) => *a,
            Self::Weighted(l) => l.iter().map(|(_, p)| p.min_inclusive()).min().unwrap_or(0),
        }
    }

    fn max_inclusive(&self) -> i32 {
        match self {
            Self::Const(v) => *v,
            Self::Uniform(_, b)
            | Self::BiasBottom(_, b)
            | Self::VeryBias(_, b)
            | Self::Trap(_, b, _) => *b,
            Self::Clamped(_, _, b) => *b,
            Self::Norm(_, _, _, b) => *b,
            Self::Weighted(l) => l.iter().map(|(_, p)| p.max_inclusive()).max().unwrap_or(0),
        }
    }

    fn parse(j: &crate::json::J) -> Option<Self> {
        use crate::json::{J, jarr, jint, jnum, js, jstr};
        if let J::Num(n) = j {
            return Some(Self::Const(*n as i32));
        }
        let ty = jstr(j, "type")?;
        let ty = ty.strip_prefix("minecraft:").unwrap_or(&ty);
        Some(match ty {
            "constant" | "literal" => Self::Const(jint(j, "value", jint(j, "constant", 0))),
            "uniform" => Self::Uniform(
                jint(j, "min_inclusive", jint(j, "min", 0)),
                jint(j, "max_inclusive", jint(j, "max", 0)),
            ),
            "biased_to_bottom" => Self::BiasBottom(
                jint(j, "min_inclusive", jint(j, "min", 0)),
                jint(j, "max_inclusive", jint(j, "max", 0)),
            ),
            "very_biased_to_bottom" => Self::VeryBias(
                jint(j, "min_inclusive", jint(j, "min", 0)),
                jint(j, "max_inclusive", jint(j, "max", 0)),
            ),
            "clamped" => Self::Clamped(
                Box::new(Self::parse(js(j, "source")?)?),
                jint(j, "min_inclusive", 0),
                jint(j, "max_inclusive", 0),
            ),
            "trapezoid" => Self::Trap(
                jint(j, "min_inclusive", jint(j, "min", 0)),
                jint(j, "max_inclusive", jint(j, "max", 0)),
                jint(j, "plateau", 0),
            ),
            "clamped_normal" => Self::Norm(
                jnum(j, "mean").unwrap_or(0.0),
                jnum(j, "deviation").unwrap_or(1.0),
                jint(j, "min_inclusive", 0),
                jint(j, "max_inclusive", 0),
            ),
            "weighted_list" => {
                let mut v = vec![];
                for e in jarr(j, "distribution")? {
                    let w = jint(e, "weight", 1);
                    let d = js(e, "data").unwrap_or(e);
                    v.push((w, Self::parse(d)?));
                }
                Self::Weighted(v)
            }
            _ => return None,
        })
    }
}

fn blackstone_replacement(name: &str) -> Option<&'static str> {
    Some(match name {
        "minecraft:cobblestone" | "minecraft:mossy_cobblestone" => "minecraft:blackstone",
        "minecraft:stone" => "minecraft:polished_blackstone",
        "minecraft:stone_bricks" | "minecraft:mossy_stone_bricks" => {
            "minecraft:polished_blackstone_bricks"
        }
        "minecraft:cobblestone_stairs" | "minecraft:mossy_cobblestone_stairs" => {
            "minecraft:blackstone_stairs"
        }
        "minecraft:stone_stairs" => "minecraft:polished_blackstone_stairs",
        "minecraft:stone_brick_stairs" | "minecraft:mossy_stone_brick_stairs" => {
            "minecraft:polished_blackstone_brick_stairs"
        }
        "minecraft:cobblestone_slab" | "minecraft:mossy_cobblestone_slab" => {
            "minecraft:blackstone_slab"
        }
        "minecraft:smooth_stone_slab" | "minecraft:stone_slab" => {
            "minecraft:polished_blackstone_slab"
        }
        "minecraft:stone_brick_slab" | "minecraft:mossy_stone_brick_slab" => {
            "minecraft:polished_blackstone_brick_slab"
        }
        "minecraft:stone_brick_wall" | "minecraft:mossy_stone_brick_wall" => {
            "minecraft:polished_blackstone_brick_wall"
        }
        "minecraft:cobblestone_wall" | "minecraft:mossy_cobblestone_wall" => {
            "minecraft:blackstone_wall"
        }
        "minecraft:chiseled_stone_bricks" => "minecraft:chiseled_polished_blackstone",
        "minecraft:cracked_stone_bricks" => "minecraft:cracked_polished_blackstone_bricks",
        "minecraft:iron_bars" => "minecraft:chain",
        _ => return None,
    })
}

fn sample_ip_lcg(
    ip: &CappedLimit,
    rng: &mut crate::generation_math::Lcg,
) -> i32 {
    match ip {
        CappedLimit::Const(v) => *v,
        CappedLimit::Uniform(a, b) => {
            if a >= b {
                *a
            } else {
                a + rng.next_bound(b - a + 1)
            }
        }
        CappedLimit::BiasBottom(a, b) => {
            if a >= b {
                *a
            } else {
                let bound = rng.next_bound(b - a + 1) + 1;
                a + rng.next_bound(bound)
            }
        }
        CappedLimit::VeryBias(a, b) => {
            if a >= b {
                *a
            } else {
                let u1 = a + rng.next_bound(b - a + 1);
                let u2 = a + rng.next_bound(b - a + 1);
                let u3 = a + rng.next_bound(b - a + 1);
                u1.min(u2).min(u3)
            }
        }
        CappedLimit::Clamped(inner, lo, hi) => sample_ip_lcg(inner, rng).clamp(*lo, *hi),
        CappedLimit::Trap(a, b, pl) => {
            if b - a - pl < 0 {
                return *a;
            }
            let lo = a + (b - a - pl) / 2;
            let hi = b - (b - a - pl) / 2;
            let v = if a >= b {
                *a
            } else {
                a + rng.next_bound(b - a + 1)
            };
            if v > lo && v < hi {
                hi - (v - lo)
            } else {
                v
            }
        }
        CappedLimit::Norm(mean, dev, lo, hi) => {
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
        CappedLimit::Weighted(list) => {
            let total: i32 = list.iter().map(|(w, _)| (*w).max(0)).sum();
            if total <= 0 {
                return 0;
            }
            let mut roll = rng.next_bound(total);
            for (w, p) in list {
                roll -= (*w).max(0);
                if roll < 0 {
                    return sample_ip_lcg(p, rng);
                }
            }
            sample_ip_lcg(&list.last().unwrap().1, rng)
        }
    }
}

impl Processor {
    /// Reject an unsupported required child rather than shorten a processor list.
    pub(crate) fn list(value: &crate::json::J, reg: &BReg) -> Result<Vec<Self>, &'static str> {
        let processors = value
            .get("processors")
            .and_then(crate::json::J::as_arr)
            .ok_or("missing template processor list")?;
        processors.iter().map(|p| Self::parse(p, reg)).collect()
    }
    #[cfg(test)]
    #[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
    pub(crate) fn from_json(value: &crate::json::J, reg: &BReg) -> Result<Self, &'static str> {
        Self::parse(value, reg)
    }
    fn parse(value: &crate::json::J, reg: &BReg) -> Result<Self, &'static str> {
        use crate::json::J;
        match value.get("processor_type").map(J::str) {
            Some("minecraft:block_ignore") => {
                let blocks = value
                    .get("blocks")
                    .and_then(J::as_arr)
                    .ok_or("missing block_ignore blocks")?;
                let names = blocks
                    .iter()
                    .map(|b| Ok(reg.name_of(state_from_json(b, reg)?).to_owned()))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(Self::BlockIgnore(names))
            }
            Some("minecraft:block_rot") => {
                let Some(J::Num(integrity)) = value.get("integrity") else {
                    return Err("missing block rot integrity");
                };
                let integrity = *integrity as f32;
                // Native floatRange compares boxed floats: -0.0 sorts below +0.0.
                if !integrity.is_finite() || integrity.is_sign_negative() || integrity > 1.0 {
                    return Err("block rot integrity outside 0..=1");
                }
                let rottable = match value.get("rottable_blocks") {
                    None => None,
                    Some(J::Arr(values)) => Some(RottableBlocks::Names(
                        values
                            .iter()
                            .map(|v| {
                                let J::Str(name) = v else {
                                    return Err("rottable block must be an identity");
                                };
                                let state = reg
                                    .checked_state(name, &[])
                                    .ok_or("unknown rottable block")?;
                                Ok(reg.name_of(state).to_owned())
                            })
                            .collect::<Result<Vec<_>, _>>()?,
                    )),
                    Some(J::Str(tag)) => {
                        Some(RottableBlocks::Tag(parse_tag_identity(tag, true)?))
                    }
                    _ => return Err("unsupported rottable holder set"),
                };
                Ok(Self::BlockRot {
                    integrity,
                    rottable,
                })
            }
            Some("minecraft:protected_blocks") => {
                let Some(J::Str(name)) = value.get("value") else {
                    return Err("missing protected block identity");
                };
                Ok(Self::Protected(parse_tag_identity(name, true)?))
            }
            Some("minecraft:rule") => {
                let rules = value
                    .get("rules")
                    .and_then(J::as_arr)
                    .ok_or("missing template rules")?;
                Ok(Self::Rules(
                    rules
                        .iter()
                        .map(|r| {
                            let position = match r.get("position_predicate") {
                                None => PosRuleTest::Always,
                                Some(p) => PosRuleTest::parse(p)?,
                            };
                            let modifier = match r.get("block_entity_modifier") {
                                None => BlockEntityModifier::Passthrough,
                                Some(m) => BlockEntityModifier::parse(m)?,
                            };
                            let output =
                                r.get("output_state").ok_or("missing rule output state")?;
                            Ok(BlockRule {
                                input: RuleTest::parse(
                                    r.get("input_predicate").ok_or("missing rule input test")?,
                                    reg,
                                )?,
                                location: RuleTest::parse(
                                    r.get("location_predicate")
                                        .ok_or("missing rule location test")?,
                                    reg,
                                )?,
                                position,
                                output: state_from_json(output, reg)?,
                                modifier,
                            })
                        })
                        .collect::<Result<Vec<_>, _>>()?,
                ))
            }
            Some("minecraft:gravity") => {
                let heightmap = match value.get("heightmap") {
                    None => crate::block_registry::HeightmapKind::WorldSurfaceWg,
                    Some(J::Str(name)) => crate::block_registry::HeightmapKind::parse(name)
                        .ok_or("unknown gravity heightmap")?,
                    _ => return Err("gravity heightmap must be string"),
                };
                let offset = match value.get("offset") {
                    None => 0,
                    Some(J::Num(v)) if v.is_finite() && v.fract() == 0.0 => *v as i32,
                    _ => return Err("gravity offset must be int"),
                };
                Ok(Self::Gravity { heightmap, offset })
            }
            Some("minecraft:blackstone_replace") => Ok(Self::BlackstoneReplace),
            Some("minecraft:lava_submerged_block") => Ok(Self::LavaSubmergedBlock),
            Some("minecraft:capped") => {
                let delegate = value
                    .get("delegate")
                    .ok_or("missing capped processor delegate")?;
                let limit_json = value
                    .get("limit")
                    .ok_or("missing capped processor limit")?;
                let limit = CappedLimit::parse(limit_json)
                    .ok_or("unsupported capped processor limit")?;
                if limit.min_inclusive() < 1 {
                    return Err("capped processor limit must be positive");
                }
                Ok(Self::Capped {
                    delegate: Box::new(Self::parse(delegate, reg)?),
                    limit,
                })
            }
            _ => Err("unsupported template processor type"),
        }
    }
}

#[expect(clippy::too_many_arguments, reason = "preserve the established placement and simulation call signature")]
fn apply_single_processor(
    processor: &Processor,
    original: &BlockInfo,
    current: &mut BlockInfo,
    pivot: Pos,
    reg: &BReg,
    world: &mut impl FnMut(Pos) -> u32,
    draw: &mut impl FnMut(Pos) -> f32,
    heightmap: &mut impl FnMut(crate::block_registry::HeightmapKind, i32, i32) -> i32,
) -> bool {
    match processor {
        Processor::BlockIgnore(names) => {
            let cur_name = reg.name_of(current.state);
            if names.iter().any(|n| n == cur_name) {
                return false;
            }
        }
        Processor::BlockRot {
            integrity,
            rottable,
        } => {
            // Membership uses the source snapshot, not preceding rule output.
            let rot_eligible = match rottable {
                None => true,
                Some(RottableBlocks::Names(names)) => {
                    names.iter().any(|n| n == reg.name_of(original.state))
                }
                Some(RottableBlocks::Tag(tag)) => reg.in_tag(original.state, tag),
            };
            if rot_eligible && draw(current.pos) > *integrity {
                return false;
            }
        }
        Processor::Protected(tag) => {
            if reg.in_tag(world(current.pos), tag) {
                return false;
            }
        }
        Processor::Rules(rules) => {
            let mut rule_rng = crate::generation_math::Lcg::new(
                crate::generation_math::hash_block_pos(current.pos.0, current.pos.1, current.pos.2)
                    as u64,
            );
            let existing = world(current.pos);
            if let Some(rule) = rules.iter().find(|r| {
                r.input.matches(current.state, reg)
                    && r.input.roll(&mut rule_rng)
                    && r.location.matches(existing, reg)
                    && r.location.roll(&mut rule_rng)
                    && r.position.matches(current.pos, pivot, &mut rule_rng)
            }) {
                current.state = rule.output;
                current.nbt = rule.modifier.apply(&mut rule_rng, current.nbt.take());
            }
        }
        Processor::Gravity {
            heightmap: kind,
            offset,
        } => {
            let surface = heightmap(*kind, current.pos.0, current.pos.2);
            current.pos.1 = surface.wrapping_add(*offset).wrapping_add(original.pos.1);
        }
        Processor::BlackstoneReplace => {
            if let Some(replacement) = blackstone_replacement(reg.name_of(current.state)) {
                let mut next = reg.state_of(replacement, None);
                for prop in ["facing", "half", "type"] {
                    if let Some(val) = reg.get_prop(current.state, prop) {
                        next = reg.with_prop(next, prop, &val);
                    }
                }
                current.state = next;
            }
        }
        Processor::LavaSubmergedBlock => {
            if reg.name_of(world(current.pos)) == "minecraft:lava"
                && !reg.is_collision_shape_full_block(current.state)
            {
                current.state = reg.state_of("minecraft:lava", None);
            }
        }
        Processor::Capped { .. } => {}
    }
    true
}

impl Template {
    /// Native processBlockInfos stage: no writes, clipping, state rotation,
    /// palette selection, entities, fluids, or shape updates. The caller supplies
    /// settings.getRandom(current_world_pos).nextFloat, preserving shared versus
    /// position-seeded RNG ownership.
    pub(crate) fn process_blocks(
        &self,
        palette: usize,
        placement: Placement,
        processors: &[Processor],
        reg: &BReg,
        world: impl FnMut(Pos) -> u32,
        draw: impl FnMut(Pos) -> f32,
    ) -> Result<Vec<BlockInfo>, &'static str> {
        self.process_blocks_with_context(
            palette,
            placement,
            processors,
            reg,
            world,
            draw,
            |_, _, _| 0,
            0,
        )
    }

    #[expect(clippy::too_many_arguments, reason = "preserve the established placement and simulation call signature")]
    pub(crate) fn process_blocks_with_context(
        &self,
        palette: usize,
        placement: Placement,
        processors: &[Processor],
        reg: &BReg,
        mut world: impl FnMut(Pos) -> u32,
        mut draw: impl FnMut(Pos) -> f32,
        mut heightmap: impl FnMut(crate::block_registry::HeightmapKind, i32, i32) -> i32,
        world_seed: u64,
    ) -> Result<Vec<BlockInfo>, &'static str> {
        let mut kept_originals = Vec::new();
        let mut result = Vec::new();
        for original in self
            .palettes
            .get(palette)
            .ok_or("palette outside template")?
        {
            let p = crate::structure_transform::block(
                original.pos,
                placement.mirror,
                placement.rotation,
                placement.pivot,
            );
            let mut current = original.clone();
            current.pos = (
                p.0.wrapping_add(placement.origin.0),
                p.1.wrapping_add(placement.origin.1),
                p.2.wrapping_add(placement.origin.2),
            );
            let mut keep = true;
            for processor in processors {
                if !apply_single_processor(
                    processor,
                    original,
                    &mut current,
                    placement.origin,
                    reg,
                    &mut world,
                    &mut draw,
                    &mut heightmap,
                ) {
                    keep = false;
                    break;
                }
            }
            if keep {
                kept_originals.push(original);
                result.push(current);
            }
        }
        for processor in processors {
            if let Processor::Capped { delegate, limit } = processor {
                if limit.max_inclusive() == 0 || result.is_empty() {
                    continue;
                }
                let factory_seed = crate::generation_math::Lcg::new(world_seed).next_i64();
                let mut cap_rng = crate::generation_math::Lcg::new(
                    (crate::generation_math::hash_block_pos(
                        placement.origin.0,
                        placement.origin.1,
                        placement.origin.2,
                    ) ^ factory_seed) as u64,
                );
                let cap = sample_ip_lcg(limit, &mut cap_rng).min(result.len() as i32);
                if cap < 1 {
                    continue;
                }
                let mut indices: Vec<usize> = (0..result.len()).collect();
                for i in (2..=indices.len()).rev() {
                    let j = cap_rng.next_bound(i as i32) as usize;
                    indices.swap(i - 1, j);
                }
                let mut replaced = 0i32;
                for idx in indices {
                    if replaced >= cap {
                        break;
                    }
                    let curr = &result[idx];
                    let mut candidate = curr.clone();
                    if apply_single_processor(
                        delegate,
                        kept_originals[idx],
                        &mut candidate,
                        placement.origin,
                        reg,
                        &mut world,
                        &mut draw,
                        &mut heightmap,
                    ) && (candidate.pos != curr.pos
                        || candidate.state != curr.state
                        || candidate.nbt != curr.nbt)
                    {
                        replaced += 1;
                        result[idx] = candidate;
                    }
                }
            }
        }
        Ok(result)
    }

    /// The one template NBT contract this crate accepts today: a container's `Items`.
    ///
    /// Native loads the tag through the live block entity with the level's registry
    /// access, and a container that also holds a `LootTable` saves that table *instead
    /// of* its items (`tests/data/template_init_1211.tsv` has both cases). Anything
    /// else is refused rather than stored and silently shadowed later.
    pub(crate) fn container_items(tag: &Tag) -> Result<Vec<(i16, i64, u8)>, &'static str> {
        let Tag::Compound(values) = tag else {
            return Err("template block NBT must be a compound");
        };
        for key in values.keys() {
            let key = key.to_utf8().map_err(|_| "template NBT key is not UTF-8")?;
            if key != "Items" {
                return Err("unsupported template block NBT: only container Items are handled");
            }
        }
        let Some(Tag::List(_, entries)) = tag.get("Items") else {
            return Err("template container Items must be a list");
        };
        let mut out = Vec::with_capacity(entries.len());
        for entry in entries {
            let Tag::Compound(_) = entry else {
                return Err("template container item must be a compound");
            };
            let slot = match entry.get("Slot") {
                Some(Tag::Byte(v)) => *v as i16,
                _ => return Err("template container item needs a byte Slot"),
            };
            if !(0..=255).contains(&slot) {
                return Err("template container slot is out of range");
            }
            let count = match entry.get("count") {
                Some(Tag::Int(v)) => *v,
                _ => return Err("template container item needs an int count"),
            };
            if !(1..=64).contains(&count) {
                return Err("template container count is out of range");
            }
            let id = match entry.get("id") {
                Some(Tag::String(v)) => v.to_utf8().map_err(|_| "item id is not UTF-8")?,
                _ => return Err("template container item needs a string id"),
            };
            let item = crate::name_to_item(&id).ok_or("unknown item id in template NBT")?;
            out.push((slot, item, count as u8));
        }
        Ok(out)
    }

    /// A sign side's four lines, decoded from the game's own message form.
    ///
    /// Native stores each message as a JSON string component (`'"Hi"'`); older or edited
    /// data may carry a text object. Anything else is refused rather than shown blank.
    pub(crate) fn sign_lines(tag: &Tag) -> Result<[String; 4], &'static str> {
        let Tag::Compound(_) = tag else {
            return Err("template block NBT must be a compound");
        };
        let front = tag
            .get("front_text")
            .ok_or("a sign template needs front_text")?;
        let lines = Self::sign_side(front)?;
        // The Sign payload only carries the front's four lines, but a malformed
        // back side must not silently survive placement either.
        if let Some(back) = tag.get("back_text") {
            Self::sign_side(back)?;
        }
        Ok(lines)
    }

    /// One sign side's four lines, decoded from the game's own message form.
    fn sign_side(side: &Tag) -> Result<[String; 4], &'static str> {
        let Tag::Compound(_) = side else {
            return Err("a sign side must be a compound");
        };
        let Some(Tag::List(_, messages)) = side.get("messages") else {
            return Err("a sign side's messages must be a list");
        };
        if messages.len() != 4 {
            return Err("a sign side has exactly four messages");
        }
        let mut lines: [String; 4] = Default::default();
        for (slot, message) in messages.iter().enumerate() {
            let Tag::String(text) = message else {
                return Err("a sign message must be a string component");
            };
            let raw = text.to_utf8().map_err(|_| "sign message is not UTF-8")?;
            let parsed = crate::json::jparse(raw.as_bytes()).ok_or("sign message is not JSON")?;
            lines[slot] = match &parsed {
                crate::json::J::Str(s) => s.clone(),
                crate::json::J::Obj(_) => match parsed.get("text") {
                    Some(crate::json::J::Str(s)) => s.clone(),
                    _ => return Err("unsupported sign component"),
                },
                _ => return Err("unsupported sign component"),
            };
        }
        Ok(lines)
    }

    /// Structural or decorative block entities that carry no active store in this port
    /// and should pass through template placement without failing closed. Anything a
    /// store or the payload seam owns — containers, signs, a spawner's `SpawnData`,
    /// a decorated pot's `LootTable`/`Items` — routes through `staged_payload`
    /// instead, which stages the supported fields and refuses the rest. Content keys
    /// a live store reads but the seam cannot carry yet (`Book`, a bookshelf's
    /// `Items`) fail closed the same way.
    fn is_passthrough_block_entity(state: u32, tag: &Tag, reg: &BReg) -> bool {
        let Tag::Compound(values) = tag else {
            return false;
        };
        let name = reg.name_of(state);
        let bare = name.strip_prefix("minecraft:").unwrap_or(name);
        match bare {
            // A marker with no config stays structural; `SpawnData` (and the
            // config around it) routes through `staged_payload`.
            "spawner" => {
                return values
                    .keys()
                    .all(|k| k.to_utf8().map(|k| k == "id").unwrap_or(false));
            }
            // A decorated pot's `sherds`/`item` have no payload seam yet; its
            // `LootTable`/`Items` do.
            "decorated_pot" => {
                return tag.get("sherds").is_some() || tag.get("item").is_some();
            }
            _ => {}
        }
        if crate::generation_payload::ContainerKind::for_block(name).is_some()
            || tag.get("front_text").is_some()
            || name.ends_with("_sign")
            || name.ends_with("_hanging_sign")
        {
            return false;
        }
        // Entities with a live store but no payload route yet fail closed when
        // the tag carries the content that store would read.
        if (bare == "lectern" && tag.get("Book").is_some())
            || (bare == "chiseled_bookshelf" && tag.get("Items").is_some())
            || (matches!(bare, "suspicious_sand" | "suspicious_gravel")
                && (tag.get("item").is_some()
                    || tag.get("LootTable").is_some()
                    || tag.get("LootTableSeed").is_some()))
        {
            return false;
        }
        matches!(
            name,
            "minecraft:jigsaw"
                | "minecraft:structure_block"
                | "minecraft:trial_spawner"
                | "minecraft:vault"
                | "minecraft:suspicious_sand"
                | "minecraft:suspicious_gravel"
                | "minecraft:brewing_stand"
                | "minecraft:campfire"
                | "minecraft:soul_campfire"
                | "minecraft:lectern"
                | "minecraft:bell"
                | "minecraft:comparator"
                | "minecraft:ender_chest"
                | "minecraft:sculk_sensor"
                | "minecraft:calibrated_sculk_sensor"
                | "minecraft:sculk_shrieker"
                | "minecraft:sculk_catalyst"
                | "minecraft:chiseled_bookshelf"
                | "minecraft:beacon"
                | "minecraft:conduit"
                | "minecraft:daylight_detector"
                | "minecraft:enchanting_table"
                | "minecraft:end_gateway"
                | "minecraft:end_portal"
                | "minecraft:flower_pot"
                | "minecraft:piston"
                | "minecraft:moving_piston"
                | "minecraft:command_block"
                | "minecraft:chain_command_block"
                | "minecraft:repeating_command_block"
                | "minecraft:beehive"
                | "minecraft:bee_nest"
        ) || name.ends_with("_bed")
            || name.ends_with("_banner")
            || name.ends_with("_wall_banner")
            || name.ends_with("_skull")
            || name.ends_with("_wall_skull")
            || name.ends_with("_head")
            || name.ends_with("_wall_head")
    }

    /// Which of the supported block-entity payloads a tag carries: a sign's
    /// `front_text` (with `back_text` validated), a spawner's `SpawnData.entity`,
    /// a container's `LootTable` (+ optional `LootTableSeed`), or a container's
    /// `Items`.
    pub(crate) fn staged_payload(
        tag: &Tag,
        state: u32,
        reg: &BReg,
    ) -> Result<crate::generation_payload::GenerationPayload, &'static str> {
        use crate::generation_payload::{ContainerKind, GenerationPayload};
        if tag.get("front_text").is_some() {
            return Ok(GenerationPayload::Sign(Self::sign_lines(tag)?));
        }
        if reg.name_of(state) == "minecraft:spawner" {
            return Self::spawner_payload(tag);
        }
        let kind = match ContainerKind::for_block(reg.name_of(state)) {
            Some(kind) => kind,
            None => {
                return Err("template container NBT on a block with no supported store");
            }
        };
        let Tag::Compound(values) = tag else {
            return Err("template block NBT must be a compound");
        };
        let is_container_meta = |k: &str| {
            matches!(
                k,
                "id"
                    | "CustomName"
                    | "Lock"
                    | "BurnTime"
                    | "CookTime"
                    | "CookTimeTotal"
                    | "RecipesUsed"
            )
        };
        if let Some(loot_tag) = tag.get("LootTable") {
            for key in values.keys() {
                let key = key.to_utf8().map_err(|_| "template NBT key is not UTF-8")?;
                if key != "LootTable"
                    && key != "LootTableSeed"
                    && key != "Items"
                    && !is_container_meta(&key)
                {
                    return Err("unsupported template block NBT: unexpected loot container field");
                }
            }
            let loot_table = loot_tag
                .string()
                .ok_or("LootTable must be a string")?
                .to_utf8()
                .map_err(|_| "LootTable is not UTF-8")?;
            let loot_seed = match tag.get("LootTableSeed") {
                None => None,
                Some(Tag::Long(v)) => Some(*v),
                Some(_) => return Err("LootTableSeed must be a long"),
            };
            return Ok(GenerationPayload::LootContainer {
                kind,
                loot_table,
                loot_seed,
            });
        }
        let mut has_items = false;
        let mut has_only_items_and_meta = true;
        for key in values.keys() {
            let key = key.to_utf8().map_err(|_| "template NBT key is not UTF-8")?;
            if key == "Items" {
                has_items = true;
            } else if !is_container_meta(&key) {
                has_only_items_and_meta = false;
            }
        }
        let slots = if has_items && has_only_items_and_meta && values.len() > 1 {
            let items_only = Tag::Compound(BTreeMap::from([(
                NbtString::text("Items"),
                tag.get("Items").unwrap().clone(),
            )]));
            Self::container_items(&items_only)?
        } else {
            Self::container_items(tag)?
        };
        Ok(GenerationPayload::Container { kind, slots })
    }

    /// A spawner's saved config. The runtime spawner model keeps the serving
    /// entity id (`SpawnData.entity.id`) and authors its own delay/range
    /// defaults — the same fields the jigsaw path extracts — so the rest of the
    /// vanilla `BaseSpawner` layout is accepted by name but not carried. A tag
    /// without `SpawnData`, or with keys outside the vanilla layout, is refused.
    fn spawner_payload(
        tag: &Tag,
    ) -> Result<crate::generation_payload::GenerationPayload, &'static str> {
        let Tag::Compound(values) = tag else {
            return Err("template block NBT must be a compound");
        };
        for key in values.keys() {
            let key = key.to_utf8().map_err(|_| "template NBT key is not UTF-8")?;
            if !matches!(
                key.as_str(),
                "id"
                    | "Delay"
                    | "MinSpawnDelay"
                    | "MaxSpawnDelay"
                    | "SpawnCount"
                    | "MaxNearbyEntities"
                    | "RequiredPlayerRange"
                    | "SpawnRange"
                    | "SpawnData"
                    | "SpawnPotentials"
                    | "custom_spawn_rules"
            ) {
                return Err("unsupported template spawner NBT field");
            }
        }
        if let Some(potentials) = tag.get("SpawnPotentials") {
            let Tag::List(_, entries) = potentials else {
                return Err("template spawner SpawnPotentials must be a list");
            };
            if entries
                .iter()
                .any(|entry| !matches!(entry, Tag::Compound(_)))
            {
                return Err("template spawner SpawnPotentials entries must be compounds");
            }
        }
        let Some(Tag::Compound(_)) = tag.get("SpawnData") else {
            return Err("template spawner needs SpawnData");
        };
        let entity = tag
            .get("SpawnData")
            .and_then(|d| d.get("entity"))
            .ok_or("template spawner SpawnData needs an entity")?;
        let Tag::Compound(_) = entity else {
            return Err("template spawner SpawnData.entity must be a compound");
        };
        let id = entity
            .get("id")
            .and_then(Tag::string)
            .ok_or("template spawner entity needs a string id")?
            .to_utf8()
            .map_err(|_| "spawner entity id is not UTF-8")?;
        Ok(crate::generation_payload::GenerationPayload::SpawnerEntity(id))
    }

    /// The general template contract: no template entities yet, and NBT limited to
    /// container `Items`/`LootTable`, signs, a spawner's `SpawnData`, or passthrough
    /// structural block entities. Callers add their own restrictions on top (the fossil path
    /// requires full cubes), so admission is not baked into placement and one owner
    /// serves every caller.
    #[cfg(test)]
    #[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
    pub(crate) fn validate_placement(&self) -> Result<(), &'static str> {
        let reg = crate::block_registry::breg().ok_or("missing block registry")?;
        if !self.entities.is_empty() {
            return Err("template placement does not handle template entities");
        }
        for info in self.palettes.iter().flatten() {
            if let Some(tag) = &info.nbt {
                if !Self::is_passthrough_block_entity(info.state, tag, reg) {
                    Self::staged_payload(tag, info.state, reg)?;
                }
            }
        }
        Ok(())
    }

    /// The fossil palette contract: no entities/NBT, and only full-cube outputs.
    /// This is not the general template entity/fluid/shape-finalization pipeline.
    pub(crate) fn validate_solid_placement(
        &self,
        processors: &[Processor],
        reg: &BReg,
    ) -> Result<(), &'static str> {
        if !self.entities.is_empty()
            || self
                .palettes
                .iter()
                .flatten()
                .any(|b| b.nbt.is_some() || reg.is_template_solid(b.state) != Some(true))
        {
            return Err("solid template placement requires full cubes without entities or NBT");
        }
        for processor in processors {
            if let Processor::Rules(rules) = processor {
                if rules
                    .iter()
                    .any(|r| reg.is_template_solid(r.output) != Some(true))
                {
                    return Err("solid template rule has a non-full output");
                }
            }
        }
        Ok(())
    }

    /// The fossil entry point. It keeps the full-cube/no-NBT admission that
    /// `validate_solid_placement` states, then runs the shared placement owner, so
    /// loosening placement for other callers cannot loosen the fossil contract.
    pub(crate) fn place_solid_blocks(
        &self,
        ctx: &mut crate::generation_context::FC,
        rng: &mut crate::generation_math::Xor,
        placement: Placement,
        processors: &[Processor],
        clip: [i32; 6],
    ) -> Result<bool, &'static str> {
        let reg = crate::block_registry::breg().ok_or("missing block registry")?;
        self.validate_solid_placement(processors, reg)?;
        self.place_blocks(ctx, rng, placement, processors, clip)
    }

    /// Shared-RNG placement for any admitted template: processors, the coordinate
    /// transform, clipping after processing, writes, the bounded edge pass and container
    /// staging. A whole processor pass reads the world before this template's writes;
    /// the next template sees those writes. Clipping is AFTER processing, including
    /// draws for discarded coordinates.
    pub(crate) fn place_blocks(
        &self,
        ctx: &mut crate::generation_context::FC,
        rng: &mut crate::generation_math::Xor,
        placement: Placement,
        processors: &[Processor],
        clip: [i32; 6],
    ) -> Result<bool, &'static str> {
        // `StructureTemplate.placeInWorld` also places the entity list through
        // `placeEntities`; this entry point has no entity seam, so a template
        // carrying one is refused rather than silently dropping the spawn.
        if !self.entities.is_empty() {
            return Err("template placement does not handle template entities");
        }
        if self.palettes.is_empty() {
            return Ok(false);
        }
        let palette = rng.next_bound(self.palettes.len() as i32) as usize;
        if self.palettes[palette].is_empty()
            || self.size.0 < 1
            || self.size.1 < 1
            || self.size.2 < 1
        {
            return Ok(false);
        }
        let reg = crate::block_registry::breg().ok_or("missing block registry")?;
        let world_seed = ctx.g.wseed();
        let blocks = {
            let ctx_cell = std::cell::RefCell::new(&mut *ctx);
            self.process_blocks_with_context(
                palette,
                placement,
                processors,
                reg,
                |p| ctx_cell.borrow_mut().get(p.0, p.1, p.2),
                |_| rng.next_f32(),
                |kind, x, z| ctx_cell.borrow_mut().heightmap(kind, x, z),
                world_seed,
            )?
        };
        let mut written = Vec::new();
        for block in blocks {
            let p = block.pos;
            if p.0 < clip[0]
                || p.1 < clip[1]
                || p.2 < clip[2]
                || p.0 > clip[3]
                || p.1 > clip[4]
                || p.2 > clip[5]
            {
                continue;
            }
            let state = crate::structure_transform::state(
                block.state,
                placement.mirror,
                placement.rotation,
            )
            .ok_or("unknown template state transform")?;
            // `WorldGenRegion.setBlock`: the write is refused by the level's build height
            // (`dimension_type`), which is what the clip above uses as well.
            if p.1 >= ctx.g.world_min_y && p.1 < ctx.g.world_min_y + ctx.g.world_height {
                ctx.set(p.0, p.1, p.2, state);
                written.push(p);
                // The `StructurePiece.placeBlock` mark: states in the pinned
                // `SHAPE_CHECK_BLOCKS` set are filed for `postProcessGeneration`,
                // which re-derives their support once the chunk settles. `FC::set`
                // already files the separate `hasPostProcess` gate; the two sets
                // are disjoint. Full-cube-only callers (the fossil path) can never
                // reach this mark, so their mark set is unchanged.
                if reg.needs_postprocess_shape(state) {
                    ctx.mark_shape(p.0, p.1, p.2);
                }
                if let Some(tag) = block
                    .nbt
                    .as_ref()
                    .filter(|tag| !Self::is_passthrough_block_entity(state, tag, reg))
                {
                    // Admission checked this tag; the write itself still fails closed
                    // rather than staging data the caller never validated.
                    ctx.payloads
                        .push((p, Self::staged_payload(tag, state, reg)?));
                }
            }
        }
        let edge_shape = |ctx: &mut crate::generation_context::FC,
                          pos: Pos,
                          state: u32,
                          neighbor: u32,
                          direction: u8|
         -> Option<u32> {
            // The registry already owns fragile attachment direction. Native
            // canSupportCenter distinguishes top and bottom slab support.
            if reg.fragile_loses_support(state, direction, reg.state_of("minecraft:air", None))
                && !reg.is_face_sturdy(
                    neighbor, direction ^ 1, crate::block_registry::Support::Center,
                )
            {
                return Some(reg.state_of("minecraft:air", None));
            }
            let name = reg.name_of(state);
            let bare = name.strip_prefix("minecraft:").unwrap_or(name);
            // `CarpetBlock.updateShape` (`dgp.a`): `!state.canSurvive -> AIR` on
            // EVERY direction — the direction does not gate it — and
            // `canSurvive` is `!level.isEmptyBlock(below)`, so a liquid floor
            // still counts as occupied.
            if bare.ends_with("_carpet")
                && reg.is_air(ctx.get(pos.0, pos.1.wrapping_sub(1), pos.2))
            {
                return Some(reg.state_of("minecraft:air", None));
            }
            // `RedStoneWireBlock.updateShape` (`dmb.a`): only the DOWN direction
            // carries a survival check (`canSurviveOn` = the block below is
            // `isFaceSturdy(_, UP, FULL)` or a hopper). The UP and horizontal
            // arms recompute the wire's own connection graph, which this path
            // deliberately does not own; the placed side properties are kept
            // rather than a second wire state machine being invented here.
            if name == "minecraft:redstone_wire"
                && direction == 0
                && !reg.is_face_sturdy(neighbor, 1, crate::block_registry::Support::Full)
                && reg.name_of(neighbor) != "minecraft:hopper"
            {
                return Some(reg.state_of("minecraft:air", None));
            }
            // `BaseRailBlock.updateShape` (`dfn.a`) only schedules the waterlogged
            // state's fluid tick and never rewrites the rail — rails fall out
            // through `neighborChanged`, not `updateShape` — so there is nothing
            // to add for them here; the fluid reservation below already covers
            // the tick. `FallingBlock.updateShape` (`diu.a`) likewise only
            // reserves a block tick, and the generation-side queue for those is
            // owned by the chunk scheduler, not this entry point.
            let above = if name.ends_with("_wall") {
                ctx.get(pos.0, pos.1.wrapping_add(1), pos.2)
            } else {
                0
            };
            // Native slabs split into single slabs without a connecting side
            // face and double slabs with full connecting faces. The registry
            // already owns the corresponding air/full-cube transition rules.
            let slab_type = reg.name_of(neighbor).ends_with("_slab")
                .then(|| reg.get_prop(neighbor, "type"))
                .flatten();
            let connecting_neighbor = match slab_type.as_deref() {
                Some("top" | "bottom") => reg.state_of("minecraft:air", None),
                Some("double") => reg.state_of("minecraft:bone_block", None),
                _ if reg.is_liquid_block(neighbor) => reg.state_of("minecraft:air", None),
                _ => neighbor,
            };
            // The registry's generic fallback detaches an isolated low wall
            // without checking its non-air neighbor. A full face must retain
            // that side even if the dedicated wall rule has no state change.
            if name.ends_with("_wall") && !reg.is_air(connecting_neighbor) {
                return reg.wall_after_full_neighbor(state, connecting_neighbor, above, direction);
            }
            reg.connection_after_neighbor(state, connecting_neighbor, above, direction)
                .or_else(|| {
                    if name.ends_with("_stairs") {
                        reg.stair_after_neighbors(state, pos, |cell| ctx.get(cell.0, cell.1, cell.2))
                    } else {
                        None
                    }
                })
        };
        // The bounded edge rule above owns the cases the scripted worlds pin. The
        // shared `update_shape` (the template-structure owner, same module) covers
        // the remaining `BlockBehaviour.updateShape` families — wall banners,
        // snowy dirt, doors, tall seagrass, chest pairing, crops, mushrooms,
        // multiface faces, ladders and buttons — so an edge touching one of them
        // finalizes instead of freezing.
        let shape_at = |ctx: &mut crate::generation_context::FC,
                            pos: Pos,
                            state: u32,
                            neighbor: u32,
                            direction: u8|
         -> Option<u32> {
            edge_shape(ctx, pos, state, neighbor, direction).or_else(|| {
                let next = update_shape(reg, state, direction, neighbor, ctx, pos);
                (next != state).then_some(next)
            })
        };
        // Native updateShapeAtEdge visits exposed voxel faces after ALL writes,
        // updating inside (`p`) first and passing the resulting state to outside (`q`).
        for (direction, p, q) in crate::voxel_faces::exposed_faces(written.iter().copied()) {
            let mut inside = ctx.get(p.0, p.1, p.2);
            let outside = ctx.get(q.0, q.1, q.2);
            crate::generation_context::schedule_leaf_edge_state(ctx, reg, p, inside, outside);
            crate::generation_context::schedule_leaf_edge_state(ctx, reg, q, outside, inside);
            if let Some(state) = shape_at(ctx, p, inside, outside, direction) {
                ctx.set(p.0, p.1, p.2, state);
                inside = state;
            }
            if let Some(state) = shape_at(ctx, q, outside, inside, direction ^ 1) {
                ctx.set(q.0, q.1, q.2, state);
            }
            match reg.fluid_name(inside) {
                "minecraft:water" => ctx.g.schedule_generated_fluid(p, 0),
                "minecraft:lava" => ctx.g.schedule_generated_fluid(p, 2),
                _ => {}
            }
            // A waterlogged pane/stair/fence reports the same source
            // FluidState as a water block. Native ProtoChunkTicks preserves
            // identity/position, not the scheduled delay; unpack happens on
            // chunk activation. This is NOT the 5-GT live-world update path.
            match reg.fluid_name(outside) {
                "minecraft:water" => ctx.g.schedule_generated_fluid(q, 0),
                "minecraft:lava" => ctx.g.schedule_generated_fluid(q, 2),
                _ => {}
            }
        }
        // Native `Block.updateFromNeighbourShapes` runs once per written cell after
        // the exposed-face pass: the same `updateShape` chain applied across
        // `UPDATE_SHAPE_ORDER` (WEST, EAST, NORTH, SOUTH, DOWN, UP), each result
        // cascading into the next direction and the chain stopping at air. This is
        // what connects a cell every one of whose neighbors the template also
        // wrote (it has no exposed face to visit) and what breaks a fragile block
        // placed on a support that is not center-sturdy.
        let air = reg.state_of("minecraft:air", None);
        for &p in &written {
            let mut current = ctx.get(p.0, p.1, p.2);
            let start = current;
            for direction in [4u8, 5, 2, 3, 0, 1] {
                let (dx, dy, dz) = direction_delta(direction);
                let neighbor = ctx.get(
                    p.0.wrapping_add(dx),
                    p.1.wrapping_add(dy),
                    p.2.wrapping_add(dz),
                );
                if let Some(next) = shape_at(ctx, p, current, neighbor, direction) {
                    current = next;
                }
                if current == air {
                    break;
                }
            }
            if current != start {
                ctx.set(p.0, p.1, p.2, current);
            }
        }
        Ok(true)
    }

    /// Transformation-only preview. Processors and world placement side effects
    /// must be applied by the structure pipeline, not inferred from this list.
    pub(crate) fn transformed_blocks(
        &self,
        palette: usize,
        mirror: crate::structure_transform::Mirror,
        rotation: crate::structure_transform::Rotation,
        pivot: Pos,
        origin: Pos,
    ) -> Result<Vec<BlockInfo>, &'static str> {
        self.palettes
            .get(palette)
            .ok_or("palette outside template")?
            .iter()
            .map(|b| {
                let p = crate::structure_transform::block(b.pos, mirror, rotation, pivot);
                Ok(BlockInfo {
                    pos: (
                        p.0.wrapping_add(origin.0),
                        p.1.wrapping_add(origin.1),
                        p.2.wrapping_add(origin.2),
                    ),
                    state: crate::structure_transform::state(b.state, mirror, rotation)
                        .ok_or("unknown template state transform")?,
                    nbt: b.nbt.clone(),
                })
            })
            .collect()
    }

    /// The transform half of `StructureTemplate.placeEntities`: `transform(Vec3)`
    /// plus `Vec3.atLowerCornerOf(origin)` for the doubles `moveTo` reads,
    /// `transform(blockPos)` plus `origin` for the clip-filter anchor, and the
    /// yaw `entity.mirror(mirror) + entity.rotate(rotation) - entity.getYRot()`,
    /// where `mirror`/`rotate` both wrap `getYRot()` BEFORE the raw value is
    /// subtracted, so an out-of-range yaw is wrapped twice, not once. `UUID`
    /// removal and the `Pos` rewrite are the spawn path's job, as native splits
    /// them; the yaw/pitch `Rotation` list and the entity `id` are validated
    /// here because a malformed entity silently vanishes in native
    /// (`loadEntityRecursive` answers `Optional.empty`) and this port's contract
    /// is to refuse it instead.
    #[cfg_attr(not(test), allow(dead_code))]
    #[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
    pub(crate) fn transformed_entities(
        &self,
        placement: Placement,
    ) -> Result<Vec<PlacedEntity>, &'static str> {
        self.entities
            .iter()
            .map(|info| {
                if !info.pos.iter().all(|v| v.is_finite()) {
                    return Err("template entity position is not finite");
                }
                let Some(Tag::String(id)) = info.nbt.get("id") else {
                    return Err("template entity needs a string id");
                };
                let id = id.to_utf8().map_err(|_| "template entity id is not UTF-8")?;
                let (raw_yaw, pitch) = entity_rotation(&info.nbt)?;
                let block = crate::structure_transform::block(
                    info.block_pos,
                    placement.mirror,
                    placement.rotation,
                    placement.pivot,
                );
                let pos = crate::structure_transform::entity(
                    info.pos,
                    placement.mirror,
                    placement.rotation,
                    placement.pivot,
                );
                Ok(PlacedEntity {
                    pos: [
                        pos[0] + f64::from(placement.origin.0),
                        pos[1] + f64::from(placement.origin.1),
                        pos[2] + f64::from(placement.origin.2),
                    ],
                    block_pos: (
                        block.0.wrapping_add(placement.origin.0),
                        block.1.wrapping_add(placement.origin.1),
                        block.2.wrapping_add(placement.origin.2),
                    ),
                    yaw: transformed_yaw(raw_yaw, placement.mirror, placement.rotation),
                    pitch,
                    id,
                })
            })
            .collect()
    }

    pub(crate) fn load(root: &Tag, reg: &BReg) -> Result<Self, &'static str> {
        let size = integer_pos(field(root, "size")?)?;
        if size.0 < 0 || size.1 < 0 || size.2 < 0 {
            return Err("negative template size");
        }
        let raw_blocks = list(root, "blocks")?;
        let raw_palettes: Vec<&[Tag]> = if let Some(palettes) = root.get("palettes") {
            palettes
                .list()
                .ok_or("palettes must be list")?
                .iter()
                .map(|p| p.list().ok_or("palette must be list"))
                .collect::<Result<_, _>>()?
        } else {
            vec![list(root, "palette")?]
        };
        let mut palettes = Vec::new();
        for raw_palette in raw_palettes {
            let states = raw_palette
                .iter()
                .map(|p| state_from_tag(p, reg))
                .collect::<Result<Vec<_>, _>>()?;
            let mut blocks = Vec::new();
            for info in raw_blocks {
                let pos = integer_pos(field(info, "pos")?)?;
                let index = field(info, "state")?
                    .int()
                    .ok_or("palette index must be int")?;
                let state = *states
                    .get(usize::try_from(index).map_err(|_| "negative palette index")?)
                    .ok_or("palette index outside template palette")?;
                let nbt = match info.get("nbt") {
                    None => None,
                    Some(v @ Tag::Compound(_)) => Some(v.clone()),
                    _ => return Err("block entity data must be compound"),
                };
                let category = if nbt.is_some() {
                    2
                } else if reg
                    .is_template_solid(state)
                    .ok_or("missing native template-solid predicate")?
                {
                    0
                } else {
                    1
                };
                blocks.push((category, BlockInfo { pos, state, nbt }));
            }
            // Native addToLists/buildInfoList: full collision blocks first,
            // non-full next, then block entities; stable Y/X/Z order per group.
            blocks.sort_by_key(|(category, b)| (*category, b.pos.1, b.pos.0, b.pos.2));
            palettes.push(blocks.into_iter().map(|(_, block)| block).collect());
        }
        let mut entities = Vec::new();
        for info in list(root, "entities")? {
            let Some(nbt) = info.get("nbt") else {
                continue;
            };
            if !matches!(nbt, Tag::Compound(_)) {
                return Err("template entity NBT must be compound");
            }
            let raw = list(info, "pos")?;
            if raw.len() != 3 {
                return Err("entity position requires three doubles");
            }
            let mut pos = [0.0; 3];
            for (i, value) in raw.iter().enumerate() {
                let Tag::Double(bits) = value else {
                    return Err("entity position requires doubles");
                };
                pos[i] = f64::from_bits(*bits);
            }
            let block_pos = integer_pos(field(info, "blockPos")?)?;
            entities.push(EntityInfo {
                pos,
                block_pos,
                nbt: nbt.clone(),
            });
        }
        Ok(Self {
            size,
            palettes,
            entities,
        })
    }

    /// Exact canonical state/block-entity representation used by the native
    /// loader oracle. This covers every loaded palette's placement order.
    pub(crate) fn block_layout(&self, reg: &BReg) -> Result<Vec<u8>, &'static str> {
        fn state_tag(state: u32, reg: &BReg) -> Result<Tag, &'static str> {
            let mut out = BTreeMap::from([(
                NbtString::text("Name"),
                Tag::String(NbtString::text(reg.name_of(state))),
            )]);
            let properties = reg
                .properties_of(state)
                .ok_or("unknown state in template")?;
            if !properties.is_empty() {
                out.insert(
                    NbtString::text("Properties"),
                    Tag::Compound(
                        properties
                            .into_iter()
                            .map(|(k, v)| (NbtString::text(&k), Tag::String(NbtString::text(&v))))
                            .collect(),
                    ),
                );
            }
            Ok(Tag::Compound(out))
        }
        let mut out = Vec::new();
        out.extend_from_slice(&(self.palettes.len() as i32).to_be_bytes());
        for palette in &self.palettes {
            out.extend_from_slice(&(palette.len() as i32).to_be_bytes());
            for info in palette {
                for v in [info.pos.0, info.pos.1, info.pos.2] {
                    out.extend_from_slice(&v.to_be_bytes());
                }
                out.extend(crate::nbt::write_named(
                    &NbtString::text(""),
                    &state_tag(info.state, reg)?,
                )?);
                out.push(u8::from(info.nbt.is_some()));
                if let Some(nbt) = &info.nbt {
                    out.extend(crate::nbt::write_named(&NbtString::text(""), nbt)?);
                }
            }
        }
        Ok(out)
    }
}

#[cfg(all(test, megu_vanilla_data))]
mod tests {
    use super::*;
    use crate::generation_math::sha256;
    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|v| format!("{v:02x}")).collect()
    }
    #[test]
    fn fossil_template_admission_rejects_unhandled_data() {
        let reg = crate::block_registry::breg().unwrap();
        let bone = reg.state_of("minecraft:bone_block", None);
        for kind in 0..4 {
            let mut template = Template {
                size: (1, 1, 1),
                palettes: vec![vec![BlockInfo {
                    pos: (0, 0, 0),
                    state: bone,
                    nbt: None,
                }]],
                entities: vec![],
            };
            let mut processors = Vec::new();
            match kind {
                0=>template.palettes[0][0].nbt=Some(Tag::Compound(BTreeMap::new())),
                1=>template.entities.push(EntityInfo {pos:[0.0;3],block_pos:(0,0,0),nbt:Tag::Compound(BTreeMap::new())}),
                2=>template.palettes[0][0].state=reg.state_of("minecraft:torch",None),
                _=>processors=Processor::list(&crate::json::jparse(br#"{"processors":[{"processor_type":"minecraft:rule","rules":[{"input_predicate":{"predicate_type":"minecraft:always_true"},"location_predicate":{"predicate_type":"minecraft:always_true"},"output_state":{"Name":"minecraft:torch"}}]}]}"#).unwrap(),reg).unwrap(),
            }
            assert!(
                template.validate_solid_placement(&processors, reg).is_err(),
                "unhandled template input {kind} passed fossil admission"
            );
        }
    }

    /// The native fixture is the expectation. The same two-cell template (a chest with
    /// explicit contents and a stone block) placed at 10,80,10 with no rotation and with
    /// 90 degrees must write the same cells, the same rotated state and the same staged
    /// contents as the captured native rows. Reading the fixture instead of restating it
    /// is the point: drift fails against what the game actually did.
    #[test]
    fn template_container_items_match_native_placement() {
        let reg = crate::block_registry::breg().unwrap();
        let chest = reg.state_of("minecraft:chest", None);
        let stone = reg.state_of("minecraft:stone", None);
        let mut items = std::collections::BTreeMap::new();
        items.insert(
            NbtString::text("Items"),
            Tag::List(
                10,
                vec![
                    container_entry(0, "minecraft:redstone", 3),
                    container_entry(2, "minecraft:diamond", 1),
                ],
            ),
        );
        let template = Template {
            // Same two cells as the fixture rows compared below: the items chest at the
            // template origin and the stone block at (1,0,1).
            size: (2, 1, 2),
            palettes: vec![vec![
                BlockInfo { pos: (0, 0, 0), state: chest, nbt: Some(Tag::Compound(items)) },
                BlockInfo { pos: (1, 0, 1), state: stone, nbt: None },
            ]],
            entities: Vec::new(),
        };
        assert!(template.validate_placement().is_ok());

        for (case, rotation) in [
            ("none", crate::structure_transform::Rotation::None),
            ("clockwise_90", crate::structure_transform::Rotation::Clockwise90),
        ] {
            let rows: Vec<Vec<String>> = include_str!("../tests/data/template_init_1211.tsv")
                .lines()
                .map(|line| line.split('\t').map(str::to_owned).collect())
                .filter(|f: &Vec<String>| f[0] == case)
                // This template is the items chest and the stone block. The fixture's other
                // rows are the loot-table chest (refused) and the sign (not staged yet).
                .filter(|f: &Vec<String>| f[4] == "absent" || f[5] != "-")
                .collect();
            let cells: Vec<(i32, i32, i32)> = rows.iter().map(|f| parse_cell(&f[2])).collect();
            let mut g = crate::worldgen::Gen::load(0, crate::name_to_state).unwrap();
            let mut ctx = crate::generation_context::FC::new(&mut g);
            ctx.test_no_terrain = true;
            let air = reg.state_of("minecraft:air", None);
            // The bounded edge pass reads the six neighbours of every written cell, and
            // a scripted world panics rather than inventing terrain for an unprovided one.
            for cell in &cells {
                ctx.test_base_cell(*cell, air);
                for (dx, dy, dz) in [(1, 0, 0), (-1, 0, 0), (0, 1, 0), (0, -1, 0), (0, 0, 1), (0, 0, -1)] {
                    ctx.test_base_cell((cell.0 + dx, cell.1 + dy, cell.2 + dz), air);
                }
            }
            let mut rng = crate::generation_math::Xor::seed_worldgen(7);
            let placement = Placement {
                origin: (10, 80, 10),
                pivot: (0, 0, 0),
                mirror: crate::structure_transform::Mirror::None,
                rotation,
            };
            let clip = [0, 0, 0, 31, 255, 31];
            assert!(template
                .place_blocks(&mut ctx, &mut rng, placement, &[], clip)
                .is_ok());

            for field in &rows {
                let cell = parse_cell(&field[2]);
                let placed = ctx.get(cell.0, cell.1, cell.2);
                // A state with no properties is written without brackets.
                let (name, props) = match field[3].split_once('[') {
                    Some((name, props)) => (name, props.trim_end_matches(']')),
                    None => (field[3].as_str(), ""),
                };
                assert_eq!(reg.name_of(placed), name, "{case} {} state", field[2]);
                for property in props.split(',').filter(|p| !p.is_empty()) {
                    let (key, value) = property.split_once('=').unwrap();
                    assert_eq!(
                        reg.get_prop(placed, key).as_deref(),
                        Some(value),
                        "{case} {} property {key}",
                        field[2]
                    );
                }
                let staged: Vec<_> = ctx
                    .payloads
                    .iter()
                    .filter(|(p, _)| *p == cell)
                    .filter_map(|(_, v)| match v {
                        crate::generation_payload::GenerationPayload::Container { slots, .. } => {
                            Some(slots.clone())
                        }
                        _ => None,
                    })
                    .next()
                    .unwrap_or_default();
                assert_eq!(
                    staged,
                    parse_native_contents(&field[5]),
                    "{case} {} contents",
                    field[2]
                );
            }
            assert_eq!(ctx.payloads.len(), 1, "{case}: one NBT cell stages contents");
        }
    }

    /// The handoff the fixture cannot show: staged contents must reach the one live
    /// container store, and survive a save/reload, or a placed chest is right in the
    /// generation layer and empty in game.
    #[test]
    fn staged_container_contents_reach_the_store_and_survive_a_save() {
        let reg = crate::block_registry::breg().unwrap();
        let chest = reg.state_of("minecraft:chest", None);
        let mut items = std::collections::BTreeMap::new();
        items.insert(
            NbtString::text("Items"),
            Tag::List(10, vec![container_entry(0, "minecraft:redstone", 3)]),
        );
        let template = Template {
            size: (1, 1, 1),
            palettes: vec![vec![BlockInfo { pos: (0, 0, 0), state: chest, nbt: Some(Tag::Compound(items)) }]],
            entities: Vec::new(),
        };
        let mut g = crate::worldgen::Gen::load(0, crate::name_to_state).unwrap();
        {
            let mut ctx = crate::generation_context::FC::new(&mut g);
            ctx.test_no_terrain = true;
            let cell = (2, 70, 2);
            ctx.test_base_cell(cell, reg.state_of("minecraft:air", None));
            for (dx, dy, dz) in [(1, 0, 0), (-1, 0, 0), (0, 1, 0), (0, -1, 0), (0, 0, 1), (0, 0, -1)] {
                ctx.test_base_cell((cell.0 + dx, cell.1 + dy, cell.2 + dz), reg.state_of("minecraft:air", None));
            }
            let mut rng = crate::generation_math::Xor::seed_worldgen(3);
            let placement = Placement {
                origin: cell,
                pivot: (0, 0, 0),
                mirror: crate::structure_transform::Mirror::None,
                rotation: crate::structure_transform::Rotation::None,
            };
            assert!(template
                .place_blocks(&mut ctx, &mut rng, placement, &[], [0, 0, 0, 31, 255, 31])
                .is_ok());
            assert_eq!(ctx.payloads.len(), 1, "placement stages the contents");
            let mut staged = std::mem::take(&mut ctx.payloads);
            g.stage_payloads((0, 0), &mut staged);
        }
        let staged = g.take_staged_payloads();
        assert_eq!(staged.len(), 1, "the generator holds the staged contents once");
        assert!(g.take_staged_payloads().is_empty(), "taking them twice yields nothing");
        crate::materialize_generation_payloads(staged);
        let redstone = crate::name_to_item("minecraft:redstone").unwrap();
        assert_eq!(
            crate::chests().lock().unwrap_or_else(|e| e.into_inner()).get(&crate::ContainerKey::ow((2, 70, 2))).cloned(),
            Some([(0i16, (redstone, 3u8, Vec::new()))].into_iter().collect()),
            "the live store holds what the template staged"
        );
        // Save and reload: the contents must come back, not just the block.
        let mut buffer = Vec::new();
        crate::save_world_into(&mut buffer);
        crate::chests().lock().unwrap_or_else(|e| e.into_inner()).clear();
        crate::load_world_from(&buffer).unwrap();
        assert_eq!(
            crate::chests().lock().unwrap_or_else(|e| e.into_inner()).get(&crate::ContainerKey::ow((2, 70, 2))).cloned(),
            Some([(0i16, (redstone, 3u8, Vec::new()))].into_iter().collect()),
            "a saved container comes back with its contents"
        );
        crate::chests().lock().unwrap_or_else(|e| e.into_inner()).remove(&crate::ContainerKey::ow((2, 70, 2)));
    }

    /// The window a player opens reads the same store the handoff writes, so the packet
    /// carries what the template staged. This is the claim "packet NBT" made concrete for
    /// a container; if a later change copies the contents somewhere else, this fails.
    #[test]
    fn a_placed_chest_shows_its_staged_contents_in_the_open_window() {
        use crate::effect_tests::client_pair;
        let reg = crate::block_registry::breg().unwrap();
        let redstone = crate::name_to_item("minecraft:redstone").unwrap();
        let pos = crate::ContainerKey::ow((2, 70, 2));
        crate::chests().lock().unwrap_or_else(|e| e.into_inner()).insert(
            pos,
            [(0i16, (redstone, 3u8, Vec::new()))].into_iter().collect(),
        );
        let (client, mut receiver) = client_pair();
        let inv = client.inv.lock().unwrap_or_else(|e| e.into_inner()).clone();
        crate::open_chest_window(
            &mut client.sock.lock().unwrap_or_else(|e| e.into_inner()),
            pos,
            None,
            &inv,
            "Chest",
        );
        // 0x33 opens the window, 0x13 carries the slots.
        let (id, _) = crate::read_packet(&mut receiver, std::time::Instant::now() + std::time::Duration::from_secs(2))
            .expect("the open-window packet");
        assert_eq!(id, 0x33, "an open-window packet precedes the items");
        let (id, payload) = crate::read_packet(&mut receiver, std::time::Instant::now() + std::time::Duration::from_secs(2))
            .expect("the window-items packet");
        assert_eq!(id, 0x13, "window items come after the screen");
        let mut at = 0usize;
        let mut varint = || {
            let mut value = 0u32;
            let mut shift = 0;
            loop {
                let byte = payload[at];
                at += 1;
                value |= ((byte & 0x7f) as u32) << shift;
                if byte & 0x80 == 0 {
                    return value;
                }
                shift += 7;
            }
        };
        assert_eq!(varint(), 1, "window id");
        let _state = varint();
        assert_eq!(varint(), 27 + 36, "chest slots plus the player inventory");
        assert_eq!(varint(), 3, "slot 0 holds the staged count");
        assert_eq!(varint(), redstone as u32, "slot 0 holds the staged item");
        assert_eq!(varint(), 0, "the staged stack carries no components");

        // The same window over an empty store: the packet follows the store, not a copy.
        crate::chests().lock().unwrap_or_else(|e| e.into_inner()).remove(&pos);
        crate::open_chest_window(
            &mut client.sock.lock().unwrap_or_else(|e| e.into_inner()),
            pos,
            None,
            &inv,
            "Chest",
        );
        let _ = crate::read_packet(&mut receiver, std::time::Instant::now() + std::time::Duration::from_secs(2));
        let (id, payload) = crate::read_packet(&mut receiver, std::time::Instant::now() + std::time::Duration::from_secs(2))
            .expect("the second window-items packet");
        assert_eq!(id, 0x13);
        let mut at = 0usize;
        let mut varint = || {
            let mut value = 0u32;
            let mut shift = 0;
            loop {
                let byte = payload[at];
                at += 1;
                value |= ((byte & 0x7f) as u32) << shift;
                if byte & 0x80 == 0 {
                    return value;
                }
                shift += 7;
            }
        };
        let _ = varint();
        let _ = varint();
        assert_eq!(varint(), 27 + 36);
        assert_eq!(
            varint(),
            0,
            "an empty store must show an empty slot, so the window is not a stale copy"
        );
        let _ = reg;
    }

    /// The fixture's sign row, end to end: the template's `front_text` decodes to four
    /// lines, placement stages them, and the handoff writes them into the sign store so
    /// the chunk packet carries the text.
    #[test]
    fn template_sign_text_matches_native_and_reaches_the_sign_store() {
        // The run writes the process-global `signs()` store.
        let _guard = crate::test_support::global_world_test_guard();
        use crate::generation_payload::GenerationPayload;
        let reg = crate::block_registry::breg().unwrap();
        let sign_state = reg.state_of("minecraft:oak_sign", None);
        let stone = reg.state_of("minecraft:stone", None);
        // The native fixture's raw form for the first line, so this test starts from the
        // captured shape rather than from what the port happens to emit.
        let rows: Vec<Vec<String>> = include_str!("../tests/data/template_init_1211.tsv")
            .lines()
            .map(|l| l.split('\t').map(str::to_owned).collect())
            .filter(|f: &Vec<String>| f[0] == "none" && f[6] != "-")
            .collect();
        assert_eq!(rows.len(), 1, "the fixture carries exactly one sign row");
        let native_text = rows[0][6].clone();
        let first_message = native_text.split('|').next().unwrap().to_string();
        let mut front = std::collections::BTreeMap::new();
        front.insert(
            NbtString::text("messages"),
            Tag::List(
                8,
                vec![
                    Tag::String(NbtString::text(&first_message)),
                    Tag::String(NbtString::text("\"\"")),
                    Tag::String(NbtString::text("\"\"")),
                    Tag::String(NbtString::text("\"\"")),
                ],
            ),
        );
        let mut tag = std::collections::BTreeMap::new();
        tag.insert(
            NbtString::text("front_text"),
            Tag::Compound(front),
        );
        let template = Template {
            size: (2, 2, 2),
            palettes: vec![vec![
                BlockInfo { pos: (1, 0, 1), state: stone, nbt: None },
                BlockInfo { pos: (1, 1, 1), state: sign_state, nbt: Some(Tag::Compound(tag)) },
            ]],
            entities: Vec::new(),
        };
        assert!(template.validate_placement().is_ok());

        let cell = parse_cell(&rows[0][2]);
        let mut g = crate::worldgen::Gen::load(0, crate::name_to_state).unwrap();
        {
            let mut ctx = crate::generation_context::FC::new(&mut g);
            ctx.test_no_terrain = true;
            let air = reg.state_of("minecraft:air", None);
            for base in [cell, (cell.0, cell.1 - 1, cell.2)] {
                ctx.test_base_cell(base, air);
                for (dx, dy, dz) in
                    [(1, 0, 0), (-1, 0, 0), (0, 1, 0), (0, -1, 0), (0, 0, 1), (0, 0, -1)]
                {
                    ctx.test_base_cell((base.0 + dx, base.1 + dy, base.2 + dz), air);
                }
            }
            let mut rng = crate::generation_math::Xor::seed_worldgen(11);
            let placement = Placement {
                origin: (10, 80, 10),
                pivot: (0, 0, 0),
                mirror: crate::structure_transform::Mirror::None,
                rotation: crate::structure_transform::Rotation::None,
            };
            assert!(template
                .place_blocks(&mut ctx, &mut rng, placement, &[], [0, 0, 0, 31, 255, 31])
                .is_ok());
            let staged = ctx
                .payloads
                .iter()
                .find(|(p, _)| *p == cell)
                .map(|(_, v)| v.clone())
                .expect("the sign cell stages a payload");
            assert_eq!(
                staged,
                GenerationPayload::Sign(["Hi".into(), String::new(), String::new(), String::new()]),
                "the template's front_text decodes to four lines"
            );
            assert_eq!(
                reg.name_of(ctx.get(cell.0, cell.1, cell.2)),
                "minecraft:oak_sign"
            );
            let mut staged_all = std::mem::take(&mut ctx.payloads);
            g.stage_payloads((0, 0), &mut staged_all);
        }
        crate::materialize_generation_payloads(g.take_staged_payloads());
        assert_eq!(
            crate::signs()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&(crate::worldgen::DimId::Overworld, cell.0, cell.1, cell.2, 1))
                .cloned(),
            Some(["Hi".to_string(), String::new(), String::new(), String::new()]),
            "the sign store holds the front lines the template carried"
        );
        crate::signs()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&(crate::worldgen::DimId::Overworld, cell.0, cell.1, cell.2, 1));
    }

    /// A furnace's `Items` belong in `furnaces()`, not `chests()`: the furnace window reads
    /// that store, so a misplaced staging would look like an empty furnace forever.
    #[test]
    fn template_furnace_contents_reach_the_furnace_store() {
        use crate::generation_payload::{ContainerKind, GenerationPayload};
        let reg = crate::block_registry::breg().unwrap();
        let furnace = reg.state_of("minecraft:furnace", None);
        let mut items = std::collections::BTreeMap::new();
        items.insert(
            NbtString::text("Items"),
            Tag::List(
                10,
                vec![
                    container_entry(0, "minecraft:iron_ore", 2),
                    container_entry(1, "minecraft:coal", 5),
                ],
            ),
        );
        let template = Template {
            size: (1, 1, 1),
            palettes: vec![vec![BlockInfo {
                pos: (0, 0, 0),
                state: furnace,
                nbt: Some(Tag::Compound(items)),
            }]],
            entities: Vec::new(),
        };
        assert!(template.validate_placement().is_ok());
        let cell = (3, 71, 3);
        let mut g = crate::worldgen::Gen::load(0, crate::name_to_state).unwrap();
        {
            let mut ctx = crate::generation_context::FC::new(&mut g);
            ctx.test_no_terrain = true;
            let air = reg.state_of("minecraft:air", None);
            ctx.test_base_cell(cell, air);
            for (dx, dy, dz) in [(1, 0, 0), (-1, 0, 0), (0, 1, 0), (0, -1, 0), (0, 0, 1), (0, 0, -1)] {
                ctx.test_base_cell((cell.0 + dx, cell.1 + dy, cell.2 + dz), air);
            }
            let mut rng = crate::generation_math::Xor::seed_worldgen(13);
            let placement = Placement {
                origin: cell,
                pivot: (0, 0, 0),
                mirror: crate::structure_transform::Mirror::None,
                rotation: crate::structure_transform::Rotation::None,
            };
            assert!(template
                .place_blocks(&mut ctx, &mut rng, placement, &[], [0, 0, 0, 31, 255, 31])
                .is_ok());
            assert_eq!(
                ctx.payloads.first().map(|(_, v)| v.clone()),
                Some(GenerationPayload::Container {
                    kind: ContainerKind::Furnace,
                    slots: vec![
                        (0, crate::name_to_item("minecraft:iron_ore").unwrap(), 2),
                        (1, crate::name_to_item("minecraft:coal").unwrap(), 5),
                    ],
                }),
                "a furnace is staged with the furnace kind"
            );
            let mut staged = std::mem::take(&mut ctx.payloads);
            g.stage_payloads((0, 0), &mut staged);
        }
        crate::materialize_generation_payloads(g.take_staged_payloads());
        let ore = crate::name_to_item("minecraft:iron_ore").unwrap();
        let coal = crate::name_to_item("minecraft:coal").unwrap();
        {
            let store = crate::furnaces().lock().unwrap_or_else(|e| e.into_inner());
            let f = store.get(&crate::ow_block(cell)).expect("the furnace store holds the placed furnace");
            assert_eq!(f.input, (ore, 2, Vec::new()), "slot 0 is the input");
            assert_eq!(f.fuel, (coal, 5, Vec::new()), "slot 1 is the fuel");
            assert_eq!(f.output, (0, 0, Vec::new()), "slot 2 is empty");
            assert!(f.dirty, "a placed furnace needs a resync");
        }
        assert!(
            crate::chests().lock().unwrap_or_else(|e| e.into_inner()).get(&crate::ContainerKey::ow(cell)).is_none(),
            "a furnace must not be staged into the chest store"
        );
        crate::furnaces().lock().unwrap_or_else(|e| e.into_inner()).remove(&crate::ow_block(cell));
    }

    /// A container the port has no store for is refused rather than staged where nothing
    /// would read it.
    ///
    /// An ender chest is the case: its contents belong to the per-player store that
    /// `ender_key` derives from a UUID, not to the block position, so a template that carried
    /// `Items` for one cannot be honoured by writing the position-keyed store.
    #[test]
    fn unsupported_container_states_are_refused() {
        let reg = crate::block_registry::breg().unwrap();
        let ender_chest = reg.state_of("minecraft:ender_chest", None);
        let mut items = std::collections::BTreeMap::new();
        items.insert(
            NbtString::text("Items"),
            Tag::List(10, vec![container_entry(0, "minecraft:arrow", 1)]),
        );
        assert_eq!(
            Template::staged_payload(&Tag::Compound(items), ender_chest, reg),
            Err("template container NBT on a block with no supported store")
        );
    }

    /// The states the pinned corpus actually carries contents for are admitted with the size
    /// of their own menu, so the loot roll and the stored slots are bounded by the block.
    #[test]
    fn container_states_are_admitted_with_their_own_size() {
        use crate::generation_payload::{ContainerKind, GenerationPayload};
        let reg = crate::block_registry::breg().unwrap();
        let mut items = std::collections::BTreeMap::new();
        items.insert(
            NbtString::text("Items"),
            Tag::List(10, vec![container_entry(0, "minecraft:arrow", 1)]),
        );
        let arrow = crate::name_to_item("minecraft:arrow").unwrap();
        for (name, expected) in [
            ("minecraft:chest", ContainerKind::Chest),
            ("minecraft:barrel", ContainerKind::Chest),
            ("minecraft:shulker_box", ContainerKind::Chest),
            ("minecraft:orange_shulker_box", ContainerKind::Chest),
            ("minecraft:furnace", ContainerKind::Furnace),
            ("minecraft:blast_furnace", ContainerKind::Furnace),
            ("minecraft:smoker", ContainerKind::Furnace),
            ("minecraft:dispenser", ContainerKind::BlockContainer(9)),
            ("minecraft:dropper", ContainerKind::BlockContainer(9)),
            ("minecraft:crafter", ContainerKind::BlockContainer(9)),
            ("minecraft:hopper", ContainerKind::BlockContainer(5)),
            ("minecraft:decorated_pot", ContainerKind::BlockContainer(1)),
        ] {
            assert_eq!(
                Template::staged_payload(
                    &Tag::Compound(items.clone()),
                    reg.state_of(name, None),
                    reg
                ),
                Ok(GenerationPayload::Container {
                    kind: expected,
                    slots: vec![(0, arrow, 1)],
                }),
                "{name} carries contents this port has a store for"
            );
        }
    }

    /// A sign tag with a shape the port cannot represent is refused, not blanked.
    #[test]
    fn unsupported_sign_components_are_refused() {
        let reg = crate::block_registry::breg().unwrap();
        let sign_state = reg.state_of("minecraft:oak_sign", None);
        let mut front = std::collections::BTreeMap::new();
        front.insert(
            NbtString::text("messages"),
            Tag::List(1, vec![Tag::Byte(1), Tag::Byte(1), Tag::Byte(1), Tag::Byte(1)]),
        );
        let mut tag = std::collections::BTreeMap::new();
        tag.insert(NbtString::text("front_text"), Tag::Compound(front));
        assert_eq!(
            Template::staged_payload(&Tag::Compound(tag), sign_state, reg),
            Err("a sign message must be a string component")
        );
    }

    /// The sign NBT the port emits must be the shape the game writes: `messages`
    /// are JSON string components and `filtered_messages` is *absent* — the codec
    /// field is optional and the game only emits it for chat-filtered lines. The
    /// contract is the live-1.21.1 differential oracle (`be_after_edit`), which
    /// shows the key omitted for an unfiltered sign.
    #[test]
    fn sign_side_nbt_matches_the_shape_the_game_writes() {
        let lines = [
            "Hi".to_string(),
            String::new(),
            String::new(),
            String::new(),
        ];
        let mut out = Vec::new();
        crate::sign_side_nbt_meta(&mut out, "front_text", &lines, "black", false);
        // `sign_side_nbt_meta` writes a named compound, so read it as one.
        let (name, tag) = crate::nbt::read_named(&out).expect("sign side parses");
        assert_eq!(name.to_utf8().unwrap(), "front_text");
        assert_eq!(tag.get("color").cloned(), Some(crate::nbt::Tag::String(crate::nbt::NbtString::text("black"))));
        assert_eq!(tag.get("has_glowing_text").cloned(), Some(crate::nbt::Tag::Byte(0)));
        let messages = match tag.get("messages") {
            Some(crate::nbt::Tag::List(kind, items)) => {
                assert_eq!(*kind, 8, "messages are a string list");
                items.clone()
            }
            other => panic!("messages must be a string list, got {other:?}"),
        };
        assert_eq!(messages.len(), 4);
        assert_eq!(
            messages[0],
            crate::nbt::Tag::String(crate::nbt::NbtString::text("\"Hi\"")),
            "a literal is a JSON string component, not a text object"
        );
        assert!(
            tag.get("filtered_messages").is_none(),
            "filtered_messages is omitted unless lines were chat-filtered"
        );
    }

    /// The fossil contract must not move: a chest is still refused there, even though the
    /// shared placement owner now handles one for other callers.
    #[test]
    fn the_fossil_contract_still_refuses_a_container() {
        let reg = crate::block_registry::breg().unwrap();
        let mut items = std::collections::BTreeMap::new();
        items.insert(NbtString::text("Items"), Tag::List(10, vec![container_entry(0, "minecraft:redstone", 3)]));
        let mut plain = std::collections::BTreeMap::new();
        plain.insert(
            NbtString::text("LootTable"),
            Tag::String(NbtString::text("minecraft:chests/simple_dungeon")),
        );
        let chest = reg.state_of("minecraft:chest", None);
        let with_items = Template {
            size: (1, 1, 1),
            palettes: vec![vec![BlockInfo { pos: (0, 0, 0), state: chest, nbt: Some(Tag::Compound(items)) }]],
            entities: Vec::new(),
        };
        assert_eq!(
            with_items.validate_placement(),
            Ok(()),
            "the general contract admits container contents"
        );
        assert_eq!(
            with_items.validate_solid_placement(&[], reg),
            Err("solid template placement requires full cubes without entities or NBT"),
            "the fossil contract still refuses a container"
        );
        let with_loot = Template {
            size: (1, 1, 1),
            palettes: vec![vec![BlockInfo { pos: (0, 0, 0), state: chest, nbt: Some(Tag::Compound(plain)) }]],
            entities: Vec::new(),
        };
        assert_eq!(
            with_loot.validate_placement(),
            Ok(()),
            "the general contract admits LootTable containers"
        );
        assert_eq!(
            with_loot.validate_solid_placement(&[], reg),
            Err("solid template placement requires full cubes without entities or NBT"),
            "the fossil contract still refuses a LootTable container"
        );
        let mut unknown = std::collections::BTreeMap::new();
        unknown.insert(NbtString::text("Items"), Tag::List(10, Vec::new()));
        unknown.insert(NbtString::text("UnknownField"), Tag::Int(1));
        let with_unknown = Template {
            size: (1, 1, 1),
            palettes: vec![vec![BlockInfo { pos: (0, 0, 0), state: chest, nbt: Some(Tag::Compound(unknown)) }]],
            entities: Vec::new(),
        };
        assert_eq!(
            with_unknown.validate_placement(),
            Err("unsupported template block NBT: only container Items are handled"),
            "unexpected container NBT keys are still refused"
        );
    }

    fn container_entry(slot: i8, id: &str, count: i32) -> Tag {
        let mut entry = std::collections::BTreeMap::new();
        entry.insert(NbtString::text("Slot"), Tag::Byte(slot));
        entry.insert(NbtString::text("id"), Tag::String(NbtString::text(id)));
        entry.insert(NbtString::text("count"), Tag::Int(count));
        Tag::Compound(entry)
    }

    fn parse_cell(text: &str) -> (i32, i32, i32) {
        let mut it = text.split(',');
        (
            it.next().unwrap().parse().unwrap(),
            it.next().unwrap().parse().unwrap(),
            it.next().unwrap().parse().unwrap(),
        )
    }

    /// `0=3 minecraft:redstone;2=1 minecraft:diamond` -> [(0, redstone, 3), (2, diamond, 1)]
    fn parse_native_contents(text: &str) -> Vec<(i16, i64, u8)> {
        if text == "-" {
            return Vec::new();
        }
        text.split(';')
            .map(|entry| {
                let (slot, stack) = entry.split_once('=').unwrap();
                let (count, id) = stack.split_once(' ').unwrap();
                (
                    slot.parse().unwrap(),
                    crate::name_to_item(id).unwrap(),
                    count.parse().unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn fence_full_cube_edge_changes_only_the_facing_connection() {
        let reg = crate::block_registry::breg().unwrap();
        let bone = reg.state_of("minecraft:bone_block", None);
        let fence = reg.state_of("minecraft:oak_fence", None);
        assert_eq!(reg.is_template_solid(bone), Some(true));
        for (direction, property) in [
            (2, "north"),
            (3, "south"),
            (4, "west"),
            (5, "east"),
        ] {
            let updated = reg
                .fence_after_full_neighbor(fence, bone, direction)
                .expect("full bone-block face must connect an oak fence");
            assert_eq!(reg.get_prop(updated, property).as_deref(), Some("true"));
            for other in ["north", "south", "west", "east"] {
                if other != property {
                    assert_eq!(
                        reg.get_prop(updated, other),
                        reg.get_prop(fence, other),
                        "neighbor update changed the wrong fence side"
                    );
                }
            }
            assert!(
                reg.fence_after_full_neighbor(updated, bone, direction).is_none(),
                "already-connected fence must not be written again"
            );
        }
        assert!(reg.fence_after_full_neighbor(fence, bone, 0).is_none());
        assert!(reg.fence_after_full_neighbor(fence, bone, 1).is_none());
        assert!(reg.fence_after_full_neighbor(fence, reg.state_of("minecraft:air", None), 4).is_none());
        assert!(reg.fence_after_full_neighbor(bone, bone, 4).is_none());
    }

    #[test]
    fn solid_template_edges_update_fences_and_reserve_native_leaf_ticks() {
        use crate::generation_context::FC;
        use crate::generation_math::Xor;
        use crate::structure_transform::{Mirror, Rotation};
        let reg = crate::block_registry::breg().unwrap();
        let bone = reg.state_of("minecraft:bone_block", None);
        let fence = reg.state_of("minecraft:oak_fence", None);
        let leaves = reg.with_prop(
            reg.state_of("minecraft:oak_leaves", None),
            "distance",
            "7",
        );
        let air = reg.state_of("minecraft:air", None);
        let center = (8, 72, 8);
        let east = (9, 72, 8);
        let north = (8, 72, 7);
        let template = Template {
            size: (1, 1, 1),
            palettes: vec![vec![BlockInfo {
                pos: (0, 0, 0),
                state: bone,
                nbt: None,
            }]],
            entities: Vec::new(),
        };
        assert!(template.validate_solid_placement(&[], reg).is_ok());
        let mut g = crate::worldgen::Gen::load(0, crate::name_to_state).unwrap();
        let mut ctx = FC::new(&mut g);
        ctx.test_no_terrain = true;
        ctx.test_writes = Some(Vec::new());
        for (pos, state) in [
            (center, air),
            ((7, 72, 8), air),
            (east, fence),
            (north, leaves),
            ((8, 72, 9), air),
            ((8, 71, 8), air),
            ((8, 73, 8), air),
        ] {
            ctx.test_base_cell(pos, state);
        }
        let mut rng = Xor::seed_worldgen(91);
        assert!(
            template
                .place_solid_blocks(
                    &mut ctx,
                    &mut rng,
                    Placement {
                        origin: center,
                        pivot: (0, 0, 0),
                        mirror: Mirror::None,
                        rotation: Rotation::None,
                    },
                    &[],
                    [8, 72, 8, 8, 72, 8],
                )
                .unwrap()
        );
        assert_eq!(ctx.get(center.0, center.1, center.2), bone);
        let updated_fence = ctx.get(east.0, east.1, east.2);
        assert_eq!(reg.get_prop(updated_fence, "west").as_deref(), Some("true"));
        assert_eq!(
            ctx.g.generated_leaf_ticks(),
            vec![(north, reg.state_of("minecraft:oak_leaves", None))],
            "template finalization must use tree's native tick owner"
        );
        assert!(ctx.g.generated_fluid_ticks().is_empty());
        let writes = ctx.test_writes.as_ref().unwrap();
        assert_eq!(writes.len(), 2);
        assert_eq!(writes[0], (center, bone));
        assert_eq!(writes[1], (east, updated_fence));
    }

    #[test]
    fn pane_wall_and_waterlogged_connections_preserve_unrelated_properties() {
        use crate::generation_context::FC;
        use crate::generation_math::Xor;
        use crate::structure_transform::{Mirror, Rotation};
        let reg = crate::block_registry::breg().unwrap();
        let bone = reg.state_of("minecraft:bone_block", None);
        let air = reg.state_of("minecraft:air", None);
        let pane = reg.state_of("minecraft:glass_pane", None);
        let bars = reg.with_prop(reg.state_of("minecraft:iron_bars", None), "waterlogged", "true");
        let wall = reg.state_of("minecraft:cobblestone_wall", None);
        let stair = reg.state_of("minecraft:oak_stairs", None);
        let p = (8, 72, 8);
        let north = (8, 72, 7);
        let south = (8, 72, 9);
        let west = (7, 72, 8);
        let east = (9, 72, 8);
        let template = Template {
            size: (1, 1, 1),
            palettes: vec![vec![BlockInfo { pos: (0, 0, 0), state: bone, nbt: None }]],
            entities: Vec::new(),
        };
        let mut g = crate::worldgen::Gen::load(0, crate::name_to_state).unwrap();
        let mut ctx = FC::new(&mut g);
        ctx.test_no_terrain = true;
        ctx.test_writes = Some(Vec::new());
        for (at, st) in [
            (p, air), (north, wall), (south, stair), (west, bars),
            (east, pane), ((8, 71, 8), air), ((8, 73, 8), air),
            ((8, 73, 7), air), ((8, 72, 10), air),
        ] {
            ctx.test_base_cell(at, st);
        }
        let mut rng = Xor::seed_worldgen(0);
        assert!(template.place_solid_blocks(
            &mut ctx,
            &mut rng,
            Placement {
                origin: p, pivot: (0, 0, 0),
                mirror: Mirror::None, rotation: Rotation::None,
            },
            &[], [8, 72, 8, 8, 72, 8],
        ).unwrap());
        assert_eq!(reg.get_prop(ctx.get(north.0, north.1, north.2), "south").as_deref(), Some("low"));
        assert_eq!(reg.get_prop(ctx.get(north.0, north.1, north.2), "up").as_deref(), Some("true"));
        assert_eq!(reg.get_prop(ctx.get(east.0, east.1, east.2), "west").as_deref(), Some("true"));
        assert_eq!(reg.get_prop(ctx.get(west.0, west.1, west.2), "east").as_deref(), Some("true"));
        assert_eq!(
            reg.get_prop(ctx.get(west.0, west.1, west.2), "waterlogged").as_deref(),
            Some("true"),
            "shape updates must not lose the waterlogged state"
        );
        assert_eq!(ctx.get(south.0, south.1, south.2), stair,
            "an ordinary cube cannot change the corner of a straight stair");
        let writes = ctx.test_writes.as_ref().unwrap();
        assert_eq!(writes.first(), Some(&(p, bone)));
        assert_eq!(writes.len(), 4, "only the center and three connectors change");
        // The existing fluid engine cannot execute waterlogged block-state
        // water. Do not enqueue a phantom ordinary water block tick here.
        assert_eq!(
            ctx.g.generated_fluid_ticks(),
            vec![(west, 0)],
            "waterlogged bars reserve native water source via the proto queue"
        );
    }

    #[test]
    fn template_air_and_water_edges_detach_connectors_and_break_fragile_supports() {
        use crate::generation_context::FC;
        use crate::generation_math::Xor;
        use crate::structure_transform::{Mirror, Rotation};

        let reg = crate::block_registry::breg().unwrap();
        let air = reg.state_of("minecraft:air", None);
        let water = reg.state_of("minecraft:water", None);
        let bone = reg.state_of("minecraft:bone_block", None);
        let center = (8, 72, 8);
        let north = (8, 72, 7);
        let south = (8, 72, 9);
        let east = (9, 72, 8);
        let west = (7, 72, 8);
        let above = (8, 73, 8);
        let below = (8, 71, 8);

        let wall = reg.with_prop(
            reg.state_of("minecraft:cobblestone_wall", None), "south", "low",
        );
        let fence = reg.with_prop(reg.state_of("minecraft:oak_fence", None), "north", "true");
        let bars = reg.with_prop(
            reg.with_prop(reg.state_of("minecraft:iron_bars", None), "west", "true"),
            "waterlogged", "true",
        );
        let pane = reg.with_prop(reg.state_of("minecraft:glass_pane", None), "east", "true");
        let torch = reg.state_of("minecraft:torch", None);
        let lantern = reg.with_prop(
            reg.state_of("minecraft:lantern", None), "hanging", "true",
        );

        for replacement in [air, water] {
            let template = Template {
                size: (1, 1, 1),
                palettes: vec![vec![BlockInfo {
                    pos: (0, 0, 0), state: replacement, nbt: None,
                }]],
                entities: Vec::new(),
            };
            let mut g = crate::worldgen::Gen::load(0, crate::name_to_state).unwrap();
            let mut ctx = FC::new(&mut g);
            ctx.test_no_terrain = true;
            ctx.test_default_air = true;
            ctx.test_writes = Some(Vec::new());
            for (pos, state) in [
                (center, bone), (north, wall), (south, fence),
                (east, bars), (west, pane), (above, torch), (below, lantern),
            ] {
                ctx.test_base_cell(pos, state);
            }
            let mut rng = Xor::seed_worldgen(7);
            assert!(template.place_blocks(
                &mut ctx, &mut rng,
                Placement {
                    origin: center, pivot: (0, 0, 0),
                    mirror: Mirror::None, rotation: Rotation::None,
                },
                &[], [8, 72, 8, 8, 72, 8],
            ).unwrap());

            assert_eq!(ctx.get(center.0, center.1, center.2), replacement);
            assert_eq!(reg.get_prop(ctx.get(north.0, north.1, north.2), "south").as_deref(), Some("none"));
            assert_eq!(reg.get_prop(ctx.get(south.0, south.1, south.2), "north").as_deref(), Some("false"));
            assert_eq!(reg.get_prop(ctx.get(east.0, east.1, east.2), "west").as_deref(), Some("false"));
            assert_eq!(reg.get_prop(ctx.get(east.0, east.1, east.2), "waterlogged").as_deref(), Some("true"));
            assert_eq!(reg.get_prop(ctx.get(west.0, west.1, west.2), "east").as_deref(), Some("false"));
            assert_eq!(ctx.get(above.0, above.1, above.2), air);
            assert_eq!(ctx.get(below.0, below.1, below.2), air);
            assert_eq!(ctx.test_writes.as_ref().unwrap().len(), 7,
                "one template write and exactly six neighboring shape updates");

            let fluid_ticks = ctx.g.generated_fluid_ticks();
            assert!(fluid_ticks.contains(&(east, 0)),
                "the surviving waterlogged bars reserve native proto source water");
            assert_eq!(fluid_ticks.contains(&(center, 0)), replacement == water,
                "only an actual liquid center reserves its own generated fluid tick");
        }
    }

    #[test]
    fn template_slab_edges_follow_native_center_support_and_connection_faces() {
        use crate::generation_context::FC;
        use crate::generation_math::Xor;
        use crate::structure_transform::{Mirror, Rotation};

        let reg = crate::block_registry::breg().unwrap();
        let air = reg.state_of("minecraft:air", None);
        let bone = reg.state_of("minecraft:bone_block", None);
        let center = (8, 72, 8);
        let north = (8, 72, 7);
        let south = (8, 72, 9);
        let east = (9, 72, 8);
        let west = (7, 72, 8);
        let above = (8, 73, 8);
        let below = (8, 71, 8);
        let wall = reg.with_prop(
            reg.state_of("minecraft:cobblestone_wall", None), "south", "low",
        );
        let fence = reg.with_prop(reg.state_of("minecraft:oak_fence", None), "north", "true");
        let bars = reg.with_prop(
            reg.with_prop(reg.state_of("minecraft:iron_bars", None), "west", "true"),
            "waterlogged", "true",
        );
        let pane = reg.with_prop(reg.state_of("minecraft:glass_pane", None), "east", "true");
        let torch = reg.state_of("minecraft:torch", None);
        let lantern = reg.with_prop(
            reg.state_of("minecraft:lantern", None), "hanging", "true",
        );

        for (slab_type, torch_survives, lantern_survives) in [
            ("bottom", false, true),
            ("top", true, false),
            ("double", true, true),
        ] {
            let slab = reg.with_prop(reg.state_of("minecraft:oak_slab", None), "type", slab_type);
            let template = Template {
                size: (1, 1, 1),
                palettes: vec![vec![BlockInfo {
                    pos: (0, 0, 0), state: slab, nbt: None,
                }]],
                entities: Vec::new(),
            };
            let mut g = crate::worldgen::Gen::load(0, crate::name_to_state).unwrap();
            let mut ctx = FC::new(&mut g);
            ctx.test_no_terrain = true;
            ctx.test_default_air = true;
            ctx.test_writes = Some(Vec::new());
            for (pos, state) in [
                (center, bone), (north, wall), (south, fence),
                (east, bars), (west, pane), (above, torch), (below, lantern),
            ] {
                ctx.test_base_cell(pos, state);
            }
            let mut rng = Xor::seed_worldgen(7);
            assert!(template.place_blocks(
                &mut ctx, &mut rng,
                Placement {
                    origin: center, pivot: (0, 0, 0),
                    mirror: Mirror::None, rotation: Rotation::None,
                },
                &[], [8, 72, 8, 8, 72, 8],
            ).unwrap());

            assert_eq!(ctx.get(center.0, center.1, center.2), slab);
            let connected = slab_type == "double";
            assert_eq!(reg.get_prop(ctx.get(north.0, north.1, north.2), "south").as_deref(),
                Some(if connected { "low" } else { "none" }), "{slab_type}");
            assert_eq!(reg.get_prop(ctx.get(south.0, south.1, south.2), "north").as_deref(),
                Some(if connected { "true" } else { "false" }), "{slab_type}");
            assert_eq!(reg.get_prop(ctx.get(east.0, east.1, east.2), "west").as_deref(),
                Some(if connected { "true" } else { "false" }), "{slab_type}");
            assert_eq!(reg.get_prop(ctx.get(east.0, east.1, east.2), "waterlogged").as_deref(),
                Some("true"), "{slab_type}");
            assert_eq!(reg.get_prop(ctx.get(west.0, west.1, west.2), "east").as_deref(),
                Some(if connected { "true" } else { "false" }), "{slab_type}");
            assert_eq!(ctx.get(above.0, above.1, above.2),
                if torch_survives { torch } else { air }, "{slab_type}");
            assert_eq!(ctx.get(below.0, below.1, below.2),
                if lantern_survives { lantern } else { air }, "{slab_type}");
            assert_eq!(ctx.test_writes.as_ref().unwrap().len(),
                if connected { 1 } else { 6 }, "{slab_type}");
            assert_eq!(ctx.g.generated_fluid_ticks(), vec![(east, 0)], "{slab_type}");
        }
    }

    #[test]
    fn template_double_slab_connects_disconnected_neighbors() {
        use crate::generation_context::FC;
        use crate::generation_math::Xor;
        use crate::structure_transform::{Mirror, Rotation};

        let reg = crate::block_registry::breg().unwrap();
        let slab = reg.with_prop(reg.state_of("minecraft:oak_slab", None), "type", "double");
        let template = Template {
            size: (1, 1, 1),
            palettes: vec![vec![BlockInfo {
                pos: (0, 0, 0), state: slab, nbt: None,
            }]],
            entities: Vec::new(),
        };
        let center = (8, 72, 8);
        let north = (8, 72, 7);
        let south = (8, 72, 9);
        let east = (9, 72, 8);
        let west = (7, 72, 8);
        let wall = reg.with_prop(
            reg.state_of("minecraft:cobblestone_wall", None), "north", "low",
        );
        let fence = reg.state_of("minecraft:oak_fence", None);
        let bars = reg.with_prop(reg.state_of("minecraft:iron_bars", None),
            "waterlogged", "true");
        let pane = reg.state_of("minecraft:glass_pane", None);
        let mut g = crate::worldgen::Gen::load(0, crate::name_to_state).unwrap();
        let mut ctx = FC::new(&mut g);
        ctx.test_no_terrain = true;
        ctx.test_default_air = true;
        ctx.test_writes = Some(Vec::new());
        for (pos, state) in [
            (center, reg.state_of("minecraft:bone_block", None)),
            (north, wall), (south, fence), (east, bars), (west, pane),
        ] {
            ctx.test_base_cell(pos, state);
        }
        let mut rng = Xor::seed_worldgen(7);
        assert!(template.place_blocks(
            &mut ctx, &mut rng,
            Placement {
                origin: center, pivot: (0, 0, 0),
                mirror: Mirror::None, rotation: Rotation::None,
            },
            &[], [8, 72, 8, 8, 72, 8],
        ).unwrap());

        let wall_after = ctx.get(north.0, north.1, north.2);
        assert_eq!(reg.get_prop(wall_after, "south").as_deref(), Some("low"));
        assert_eq!(reg.get_prop(wall_after, "up").as_deref(), Some("false"));
        assert_eq!(reg.get_prop(ctx.get(south.0, south.1, south.2), "north").as_deref(), Some("true"));
        assert_eq!(reg.get_prop(ctx.get(east.0, east.1, east.2), "west").as_deref(), Some("true"));
        assert_eq!(reg.get_prop(ctx.get(east.0, east.1, east.2), "waterlogged").as_deref(), Some("true"));
        assert_eq!(reg.get_prop(ctx.get(west.0, west.1, west.2), "east").as_deref(), Some("true"));
        assert_eq!(ctx.test_writes.as_ref().unwrap().len(), 5);
        assert_eq!(ctx.g.generated_fluid_ticks(), vec![(east, 0)]);
    }

    #[test]
    fn stairs_recompute_outer_inner_and_preserve_waterlogged_on_neighbor_changes() {
        use std::collections::HashMap;
        let reg = crate::block_registry::breg().unwrap();
        let stair = reg.with_prop(
            reg.state_of("minecraft:oak_stairs", None), "waterlogged", "true",
        );
        let west_stair = reg.with_prop(stair, "facing", "west");
        let top_stair = reg.with_prop(west_stair, "half", "top");
        let p = (0, 70, 0);
        let air = reg.state_of("minecraft:air", None);
        let eval = |neighbors: HashMap<(i32, i32, i32), u32>| {
            reg.stair_after_neighbors(stair, p, |q| neighbors.get(&q).copied().unwrap_or(air))
        };
        let outer = eval(HashMap::from([((0, 70, -1), west_stair)]))
            .expect("front turn");
        assert_eq!(reg.get_prop(outer, "shape").as_deref(), Some("outer_left"));
        assert_eq!(reg.get_prop(outer, "waterlogged").as_deref(), Some("true"));
        let inner = eval(HashMap::from([((0, 70, 1), west_stair)]))
            .expect("rear turn");
        assert_eq!(reg.get_prop(inner, "shape").as_deref(), Some("inner_left"));
        assert!(eval(HashMap::from([((0, 70, -1), top_stair)])).is_none(),
            "different stair halves do not make a corner");
        assert!(eval(HashMap::from([
            ((0, 70, -1), west_stair), ((1, 70, 0), stair),
        ])).is_none(), "aligned flank blocks the outer corner");
        assert!(eval(HashMap::new()).is_none(), "already-straight state is stable");
    }

    #[test]
    fn every_pinned_template_matches_native_typed_nbt_and_loaded_order() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let reg = crate::block_registry::breg().unwrap();
        let rows: Vec<&str> = include_str!("../tests/data/structure_templates_1211.tsv")
            .lines()
            .collect();
        assert_eq!(rows.len(), 1180);
        let workers = std::thread::available_parallelism()
            .map(|n| n.get().clamp(1, 8))
            .unwrap_or(1);
        let next = AtomicUsize::new(0);
        let count = std::thread::scope(|s| {
            let handles: Vec<_> = (0..workers)
                .map(|_| {
                    s.spawn(|| {
                        let mut count = 0usize;
                        loop {
                            let idx = next.fetch_add(1, Ordering::Relaxed);
                            let Some(row) = rows.get(idx) else {
                                break;
                            };
                            let f: Vec<_> = row.split('\t').collect();
                            assert_eq!(f.len(), 5);
                            let path =
                                std::path::Path::new("datapacks/vanilla/data/minecraft/structure")
                                    .join(f[0]);
                            let bytes = std::fs::read(&path).unwrap();
                            let (name, root) =
                                crate::nbt::read_gzip(&bytes, 16 * 1024 * 1024).unwrap();
                            let canonical = crate::nbt::write_named(&name, &root).unwrap();
                            assert_eq!(hex(&sha256(&canonical)), f[1], "native typed NBT: {}", f[0]);
                            let template =
                                Template::load(&root, reg).unwrap_or_else(|e| panic!("{}: {e}", f[0]));
                            assert_eq!(
                                template.palettes.len(),
                                f[2].parse::<usize>().unwrap(),
                                "{}",
                                f[0]
                            );
                            assert_eq!(
                                template.entities.len(),
                                f[3].parse::<usize>().unwrap(),
                                "{}",
                                f[0]
                            );
                            assert_eq!(
                                hex(&sha256(&template.block_layout(reg).unwrap())),
                                f[4],
                                "native template load: {}",
                                f[0]
                            );
                            count += 1;
                        }
                        count
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).sum::<usize>()
        });
        assert_eq!(count, 1180);
    }

    #[test]
    fn processor_stage_matches_native_cells_and_rng() {
        use crate::generation_math::{hash_block_pos, Lcg, Xor};
        use crate::json::{jparse, J};
        use crate::structure_transform::{Mirror, Rotation};
        let reg = crate::block_registry::breg().unwrap();
        let cases = jparse(include_bytes!(
            "../tests/data/template_processor_cases_1211.json"
        ))
        .unwrap();
        let cases = cases.as_arr().unwrap();
        let expected: Vec<_> = include_str!("../tests/data/template_processors_1211.tsv")
            .lines()
            .collect();
        assert_eq!(cases.len(), expected.len());
        assert_eq!(cases.len(), 270);
        let pos = |j: &J| {
            let a = j.as_arr().unwrap();
            (a[0].num() as i32, a[1].num() as i32, a[2].num() as i32)
        };
        for (case, row) in cases.iter().zip(expected) {
            let f: Vec<_> = row.split('\t').collect();
            let id = case.get("id").unwrap().str();
            assert_eq!(f[0], id);
            let empty_blocks = J::Arr(Vec::new());
            let blocks = case
                .get("blocks")
                .unwrap_or(&empty_blocks)
                .as_arr()
                .unwrap()
                .iter()
                .map(|b| BlockInfo {
                    pos: pos(b.get("pos").unwrap()),
                    state: state_from_json(b.get("state").unwrap(), reg).unwrap(),
                    nbt: b.get("marker").map(|m| {
                        Tag::Compound(BTreeMap::from([(
                            NbtString::text("marker"),
                            Tag::Int(m.num() as i32),
                        )]))
                    }),
                })
                .collect();
            let mut template = if let Some(path) = case.get("template") {
                let data = std::fs::read(path.str()).unwrap();
                let (_, root) = crate::nbt::read_gzip(&data, 16 * 1024 * 1024).unwrap();
                Template::load(&root, reg).unwrap()
            } else {
                Template {
                    size: (8, 8, 8),
                    palettes: vec![blocks],
                    entities: Vec::new(),
                }
            };
            let original = template.block_layout(reg).unwrap();
            let processors = Processor::list(case, reg).unwrap();
            let world: Vec<_> = case
                .get("world")
                .unwrap()
                .as_arr()
                .unwrap()
                .iter()
                .map(|s| state_from_json(s, reg).unwrap())
                .collect();
            let mut rng = Xor::seed_worldgen(case.get("seed").unwrap().num() as i64 as u64);
            let mode = case.get("random").unwrap().str();
            let mut shared_draws = 0;
            let mut reads = 0;
            let processed = template
                .process_blocks(
                    0,
                    Placement {
                        origin: pos(case.get("origin").unwrap()),
                        pivot: pos(case.get("pivot").unwrap()),
                        mirror: Mirror::from_index(case.get("mirror").unwrap().num() as u8)
                            .unwrap(),
                        rotation: Rotation::from_index(case.get("rotation").unwrap().num() as u8)
                            .unwrap(),
                    },
                    &processors,
                    reg,
                    |p| {
                        reads += 1;
                        let key =
                            p.0.wrapping_mul(31)
                                .wrapping_add(p.1.wrapping_mul(7))
                                .wrapping_add(p.2.wrapping_mul(13));
                        world[key.rem_euclid(world.len() as i32) as usize]
                    },
                    |p| {
                        if mode == "positional" {
                            Lcg::new(hash_block_pos(p.0, p.1, p.2) as u64).next_f32()
                        } else {
                            shared_draws += 1;
                            if mode == "zero" {
                                0.0
                            } else {
                                rng.next_f32()
                            }
                        }
                    },
                )
                .unwrap();
            assert_eq!(
                template.block_layout(reg).unwrap(),
                original,
                "source mutated: {id}"
            );
            assert_eq!(
                processed.len(),
                f[1].parse::<usize>().unwrap(),
                "count: {id}"
            );
            template.palettes = vec![processed];
            assert_eq!(
                hex(&sha256(&template.block_layout(reg).unwrap())),
                f[2],
                "full typed cells: {id}"
            );
            assert_eq!(
                rng.next_bound(1000003),
                f[3].parse::<i32>().unwrap(),
                "following RNG: {id}"
            );
            assert_eq!(
                shared_draws,
                f[4].parse::<usize>().unwrap(),
                "shared float calls: {id}"
            );
            assert_eq!(reads, f[5].parse::<usize>().unwrap(), "world reads: {id}");
        }
    }

    #[test]
    fn processor_lists_reject_unsupported_required_children() {
        use crate::json::jparse;
        let reg = crate::block_registry::breg().unwrap();
        let reject = |body: &str| {
            let json = format!("{{\"processors\":[{body}]}}");
            assert!(
                Processor::list(&jparse(json.as_bytes()).unwrap(), reg).is_err(),
                "accepted {body}"
            );
        };
        for bad in [
            r#"{"processor_type":"minecraft:unknown_processor"}"#,
            r#"{"processor_type":"minecraft:gravity","heightmap":"BOGUS"}"#,
            r#"{"processor_type":"minecraft:capped","delegate":{"processor_type":"minecraft:blackstone_replace"},"limit":0}"#,
            r#"{"processor_type":"minecraft:protected_blocks","value":"bedrock"}"#,
            r##"{"processor_type":"minecraft:protected_blocks","value":"#BadTag"}"##,
            r#"{"processor_type":"minecraft:block_rot","integrity":-0.0}"#,
            r#"{"processor_type":"minecraft:block_rot","integrity":-1e-60}"#,
            r#"{"processor_type":"minecraft:block_rot","integrity":1.1}"#,
            r#"{"processor_type":"minecraft:block_rot","integrity":0.1,"rottable_blocks":["no_such_block"]}"#,
            r#"{"processor_type":"minecraft:rule","rules":[{}]}"#,
        ] {
            reject(bad);
        }
        // A supported sibling does not make a required unsupported child optional.
        reject(
            r#"{"processor_type":"minecraft:block_rot","integrity":1},{"processor_type":"minecraft:unknown_processor"}"#,
        );
        for extra in [
            r#""position_predicate":{"predicate_type":"minecraft:unknown_pos"},"#,
            r#""block_entity_modifier":{"type":"minecraft:unknown_modifier"},"#,
        ] {
            reject(&format!(
                r#"{{"processor_type":"minecraft:rule","rules":[{{{extra}"input_predicate":{{"predicate_type":"minecraft:always_true"}},"location_predicate":{{"predicate_type":"minecraft:always_true"}},"output_state":{{"Name":"minecraft:stone"}}}}]}}"#
            ));
        }
    }

    #[test]
    fn all_vanilla_processor_lists_parse_cleanly() {
        let reg = crate::block_registry::breg().unwrap();
        let dir = std::path::Path::new("datapacks/vanilla/data/minecraft/worldgen/processor_list");
        let mut count = 0usize;
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let bytes = std::fs::read(&path).unwrap();
            let json = crate::json::jparse(&bytes).unwrap_or_else(|| panic!("parse {}", path.display()));
            Processor::list(&json, reg)
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            count += 1;
        }
        assert_eq!(count, 40, "all 40 vanilla processor lists must parse");
    }

    #[test]
    fn gravity_processor_shifts_y_by_heightmap_offset_and_original_y() {
        use crate::block_registry::HeightmapKind;
        use crate::structure_transform::{Mirror, Rotation};
        let reg = crate::block_registry::breg().unwrap();
        let stone = reg.state_of("minecraft:stone", None);
        let air = reg.state_of("minecraft:air", None);
        let template = Template {
            size: (2, 4, 2),
            palettes: vec![vec![
                BlockInfo { pos: (0, 0, 0), state: stone, nbt: None },
                BlockInfo { pos: (1, 3, 0), state: stone, nbt: None },
            ]],
            entities: Vec::new(),
        };
        let proc_json = crate::json::jparse(
            br#"{"processor_type":"minecraft:gravity","heightmap":"OCEAN_FLOOR_WG","offset":-2}"#,
        )
        .unwrap();
        let processor = Processor::from_json(&proc_json, reg).unwrap();
        let processed = template
            .process_blocks_with_context(
                0,
                Placement {
                    origin: (10, 50, 20),
                    pivot: (0, 0, 0),
                    mirror: Mirror::None,
                    rotation: Rotation::None,
                },
                &[processor],
                reg,
                |_| air,
                |_| 0.0,
                |kind, x, z| {
                    assert_eq!(kind, HeightmapKind::OceanFloorWg);
                    if (x, z) == (10, 20) {
                        64
                    } else {
                        70
                    }
                },
                0,
            )
            .unwrap();
        assert_eq!(processed[0].pos, (10, (64 - 2), 20));
        assert_eq!(processed[1].pos, (11, 70 - 2 + 3, 20));
    }

    #[test]
    fn blackstone_replace_processor_preserves_facing_half_type_and_drops_unrelated_props() {
        use crate::structure_transform::{Mirror, Rotation};
        let reg = crate::block_registry::breg().unwrap();
        let cobble = reg.state_of("minecraft:cobblestone", None);
        let slab = reg.with_prop(
            reg.with_prop(reg.state_of("minecraft:stone_brick_slab", None), "type", "top"),
            "waterlogged",
            "true",
        );
        let stairs = reg.with_prop(
            reg.with_prop(
                reg.with_prop(
                    reg.with_prop(reg.state_of("minecraft:stone_brick_stairs", None), "facing", "east"),
                    "half",
                    "top",
                ),
                "shape",
                "inner_left",
            ),
            "waterlogged",
            "true",
        );
        let bars = reg.state_of("minecraft:iron_bars", None);
        let gold = reg.state_of("minecraft:gold_block", None);
        let template = Template {
            size: (5, 1, 1),
            palettes: vec![vec![
                BlockInfo { pos: (0, 0, 0), state: cobble, nbt: None },
                BlockInfo { pos: (1, 0, 0), state: slab, nbt: None },
                BlockInfo { pos: (2, 0, 0), state: stairs, nbt: None },
                BlockInfo { pos: (3, 0, 0), state: bars, nbt: None },
                BlockInfo { pos: (4, 0, 0), state: gold, nbt: None },
            ]],
            entities: Vec::new(),
        };
        let proc_json =
            crate::json::jparse(br#"{"processor_type":"minecraft:blackstone_replace"}"#).unwrap();
        let processor = Processor::from_json(&proc_json, reg).unwrap();
        let processed = template
            .process_blocks(
                0,
                Placement {
                    origin: (0, 64, 0),
                    pivot: (0, 0, 0),
                    mirror: Mirror::None,
                    rotation: Rotation::None,
                },
                &[processor],
                reg,
                |_| reg.state_of("minecraft:air", None),
                |_| 0.0,
            )
            .unwrap();
        assert_eq!(reg.name_of(processed[0].state), "minecraft:blackstone");
        assert_eq!(
            reg.name_of(processed[1].state),
            "minecraft:polished_blackstone_brick_slab"
        );
        assert_eq!(reg.get_prop(processed[1].state, "type").as_deref(), Some("top"));
        assert_eq!(
            reg.get_prop(processed[1].state, "waterlogged").as_deref(),
            Some("false"),
            "BlackstoneReplace starts from defaultBlockState and only copies facing/half/type"
        );
        assert_eq!(
            reg.name_of(processed[2].state),
            "minecraft:polished_blackstone_brick_stairs"
        );
        assert_eq!(reg.get_prop(processed[2].state, "facing").as_deref(), Some("east"));
        assert_eq!(reg.get_prop(processed[2].state, "half").as_deref(), Some("top"));
        assert_eq!(reg.get_prop(processed[2].state, "shape").as_deref(), Some("straight"));
        assert_eq!(reg.name_of(processed[3].state), "minecraft:chain");
        assert_eq!(reg.name_of(processed[4].state), "minecraft:gold_block");
    }

    #[test]
    fn lava_submerged_block_processor_replaces_non_full_blocks_in_lava() {
        use crate::structure_transform::{Mirror, Rotation};
        let reg = crate::block_registry::breg().unwrap();
        let slab = reg.state_of("minecraft:stone_slab", None);
        let stone = reg.state_of("minecraft:stone", None);
        let lava = reg.state_of("minecraft:lava", None);
        let air = reg.state_of("minecraft:air", None);
        let template = Template {
            size: (3, 1, 1),
            palettes: vec![vec![
                BlockInfo { pos: (0, 0, 0), state: slab, nbt: None },
                BlockInfo { pos: (1, 0, 0), state: stone, nbt: None },
                BlockInfo { pos: (2, 0, 0), state: slab, nbt: None },
            ]],
            entities: Vec::new(),
        };
        let proc_json =
            crate::json::jparse(br#"{"processor_type":"minecraft:lava_submerged_block"}"#).unwrap();
        let processor = Processor::from_json(&proc_json, reg).unwrap();
        let processed = template
            .process_blocks(
                0,
                Placement {
                    origin: (0, 30, 0),
                    pivot: (0, 0, 0),
                    mirror: Mirror::None,
                    rotation: Rotation::None,
                },
                &[processor],
                reg,
                |p| if p.0 < 2 { lava } else { air },
                |_| 0.0,
            )
            .unwrap();
        assert_eq!(reg.name_of(processed[0].state), "minecraft:lava");
        assert_eq!(reg.name_of(processed[1].state), "minecraft:stone");
        assert_eq!(reg.name_of(processed[2].state), "minecraft:stone_slab");
    }

    #[test]
    fn capped_processor_uses_world_seed_positional_rng_and_shuffled_limit() {
        use crate::generation_math::{hash_block_pos, Lcg};
        use crate::structure_transform::{Mirror, Rotation};
        let reg = crate::block_registry::breg().unwrap();
        let gravel = reg.state_of("minecraft:gravel", None);
        let stone = reg.state_of("minecraft:stone", None);
        let air = reg.state_of("minecraft:air", None);
        let blocks = (0..16)
            .map(|i| BlockInfo {
                pos: (i, 0, 0),
                state: if i % 2 == 0 { gravel } else { stone },
                nbt: None,
            })
            .collect();
        let template = Template {
            size: (16, 1, 1),
            palettes: vec![blocks],
            entities: Vec::new(),
        };
        let proc_json = crate::json::jparse(
            br#"{
                "processor_type":"minecraft:capped",
                "limit":2,
                "delegate":{
                    "processor_type":"minecraft:rule",
                    "rules":[{
                        "input_predicate":{"predicate_type":"minecraft:block_match","block":"minecraft:gravel"},
                        "location_predicate":{"predicate_type":"minecraft:always_true"},
                        "output_state":{"Name":"minecraft:suspicious_gravel","Properties":{"dusted":"0"}},
                        "block_entity_modifier":{"type":"minecraft:append_loot","loot_table":"minecraft:archaeology/trail_ruins_common"}
                    }]
                }
            }"#,
        )
        .unwrap();
        let processor = Processor::from_json(&proc_json, reg).unwrap();
        let origin = (100, 60, -40);
        let world_seed = 0x1234_5678_9abc_def0u64;
        let processed = template
            .process_blocks_with_context(
                0,
                Placement {
                    origin,
                    pivot: (0, 0, 0),
                    mirror: Mirror::None,
                    rotation: Rotation::None,
                },
                &[processor],
                reg,
                |_| air,
                |_| 0.0,
                |_, _, _| 0,
                world_seed,
            )
            .unwrap();
        // Verify exact native index shuffle and cap=2 replacement count.
        let factory_seed = Lcg::new(world_seed).next_i64();
        let mut rng = Lcg::new((hash_block_pos(origin.0, origin.1, origin.2) ^ factory_seed) as u64);
        let mut expected_indices: Vec<usize> = (0..16).collect();
        for i in (2..=16).rev() {
            let j = rng.next_bound(i as i32) as usize;
            expected_indices.swap(i - 1, j);
        }
        let chosen: Vec<usize> = expected_indices
            .into_iter()
            .filter(|&idx| idx % 2 == 0)
            .take(2)
            .collect();
        for (idx, block) in processed.iter().enumerate() {
            if chosen.contains(&idx) {
                assert_eq!(reg.name_of(block.state), "minecraft:suspicious_gravel");
                let nbt = block.nbt.as_ref().expect("append_loot attaches NBT");
                assert_eq!(
                    nbt.get("LootTable").and_then(Tag::string).map(|s| s.to_utf8().unwrap()),
                    Some("minecraft:archaeology/trail_ruins_common".to_owned())
                );
                assert!(matches!(nbt.get("LootTableSeed"), Some(Tag::Long(_))));
            } else if idx % 2 == 0 {
                assert_eq!(reg.name_of(block.state), "minecraft:gravel");
                assert!(block.nbt.is_none());
            } else {
                assert_eq!(reg.name_of(block.state), "minecraft:stone");
            }
        }
    }

    #[test]
    fn rule_processor_supports_tag_blockstate_random_axis_pos_and_rottable_tag() {
        use crate::structure_transform::{Mirror, Rotation};
        let reg = crate::block_registry::breg().unwrap();
        let slab_bottom = reg.state_of("minecraft:stone_brick_slab", None);
        let slab_top = reg.with_prop(slab_bottom, "type", "top");
        let air = reg.state_of("minecraft:air", None);
        let template = Template {
            size: (1, 3, 1),
            palettes: vec![vec![
                BlockInfo { pos: (0, 0, 0), state: slab_bottom, nbt: None },
                BlockInfo { pos: (0, 1, 0), state: slab_top, nbt: None },
                BlockInfo { pos: (0, 2, 0), state: reg.state_of("minecraft:deepslate", None), nbt: None },
            ]],
            entities: Vec::new(),
        };
        let list_json = crate::json::jparse(
            br##"{
                "processors":[
                    {
                        "processor_type":"minecraft:block_rot",
                        "integrity":0.0,
                        "rottable_blocks":"#minecraft:ancient_city_replaceable"
                    },
                    {
                        "processor_type":"minecraft:rule",
                        "rules":[
                            {
                                "input_predicate":{
                                    "predicate_type":"minecraft:blockstate_match",
                                    "block_state":{"Name":"minecraft:stone_brick_slab","Properties":{"type":"top","waterlogged":"false"}}
                                },
                                "location_predicate":{"predicate_type":"minecraft:always_true"},
                                "position_predicate":{
                                    "predicate_type":"minecraft:axis_aligned_linear_pos",
                                    "axis":"y",
                                    "min_dist":0,
                                    "max_dist":10,
                                    "min_chance":1.0,
                                    "max_chance":1.0
                                },
                                "output_state":{"Name":"minecraft:mossy_stone_brick_slab","Properties":{"type":"top","waterlogged":"false"}}
                            },
                            {
                                "input_predicate":{"predicate_type":"minecraft:tag_match","tag":"minecraft:slabs"},
                                "location_predicate":{"predicate_type":"minecraft:always_true"},
                                "output_state":{"Name":"minecraft:cobblestone_slab","Properties":{"type":"bottom","waterlogged":"false"}}
                            }
                        ]
                    }
                ]
            }"##,
        )
        .unwrap();
        let processors = Processor::list(&list_json, reg).unwrap();
        let processed = template
            .process_blocks(
                0,
                Placement {
                    origin: (0, 64, 0),
                    pivot: (0, 0, 0),
                    mirror: Mirror::None,
                    rotation: Rotation::None,
                },
                &processors,
                reg,
                |_| air,
                |_| 0.5,
            )
            .unwrap();
        // deepslate is in #minecraft:ancient_city_replaceable and dropped at integrity=0.0, draw=0.5.
        assert_eq!(processed.len(), 2);
        assert_eq!(reg.name_of(processed[0].state), "minecraft:cobblestone_slab");
        assert_eq!(reg.name_of(processed[1].state), "minecraft:mossy_stone_brick_slab");
    }

    #[test]
    fn loot_table_chest_and_decorative_block_entities_place_and_materialize() {
        use crate::generation_context::FC;
        use crate::generation_math::Xor;
        use crate::generation_payload::{ContainerKind, GenerationPayload};
        use crate::structure_transform::{Mirror, Rotation};
        let reg = crate::block_registry::breg().unwrap();
        let chest = reg.state_of("minecraft:chest", None);
        let furnace = reg.state_of("minecraft:furnace", None);
        let jigsaw = reg.state_of("minecraft:jigsaw", None);
        let pot = reg.state_of("minecraft:decorated_pot", None);
        let spawner = reg.state_of("minecraft:spawner", None);
        let air = reg.state_of("minecraft:air", None);

        let mut loot_nbt = BTreeMap::new();
        loot_nbt.insert(
            NbtString::text("LootTable"),
            Tag::String(NbtString::text("minecraft:chests/abandoned_mineshaft")),
        );
        loot_nbt.insert(NbtString::text("LootTableSeed"), Tag::Long(424242));
        loot_nbt.insert(NbtString::text("id"), Tag::String(NbtString::text("minecraft:chest")));

        let mut furnace_nbt = BTreeMap::new();
        furnace_nbt.insert(NbtString::text("id"), Tag::String(NbtString::text("minecraft:furnace")));
        furnace_nbt.insert(NbtString::text("BurnTime"), Tag::Short(0));
        furnace_nbt.insert(NbtString::text("CookTime"), Tag::Short(0));
        furnace_nbt.insert(NbtString::text("CookTimeTotal"), Tag::Short(200));
        furnace_nbt.insert(NbtString::text("RecipesUsed"), Tag::Compound(BTreeMap::new()));
        furnace_nbt.insert(
            NbtString::text("Items"),
            Tag::List(10, vec![container_entry(0, "minecraft:iron_ore", 1)]),
        );

        let mut jigsaw_nbt = BTreeMap::new();
        jigsaw_nbt.insert(NbtString::text("id"), Tag::String(NbtString::text("minecraft:jigsaw")));
        jigsaw_nbt.insert(NbtString::text("pool"), Tag::String(NbtString::text("minecraft:empty")));

        let mut pot_nbt = BTreeMap::new();
        pot_nbt.insert(NbtString::text("id"), Tag::String(NbtString::text("minecraft:decorated_pot")));
        pot_nbt.insert(NbtString::text("LootTable"), Tag::String(NbtString::text("minecraft:pots/trial_chambers/corridor")));

        let mut spawner_nbt = BTreeMap::new();
        spawner_nbt.insert(NbtString::text("id"), Tag::String(NbtString::text("minecraft:spawner")));

        let template = Template {
            size: (5, 1, 1),
            palettes: vec![vec![
                BlockInfo { pos: (0, 0, 0), state: chest, nbt: Some(Tag::Compound(loot_nbt)) },
                BlockInfo { pos: (1, 0, 0), state: furnace, nbt: Some(Tag::Compound(furnace_nbt)) },
                BlockInfo { pos: (2, 0, 0), state: jigsaw, nbt: Some(Tag::Compound(jigsaw_nbt)) },
                BlockInfo { pos: (3, 0, 0), state: pot, nbt: Some(Tag::Compound(pot_nbt)) },
                BlockInfo { pos: (4, 0, 0), state: spawner, nbt: Some(Tag::Compound(spawner_nbt)) },
            ]],
            entities: Vec::new(),
        };
        assert!(template.validate_placement().is_ok());

        let origin = (4, 70, 4);
        let mut g = crate::worldgen::Gen::load(0, crate::name_to_state).unwrap();
        {
            let mut ctx = FC::new(&mut g);
            ctx.test_no_terrain = true;
            ctx.test_default_air = true;
            let mut rng = Xor::seed_worldgen(9);
            assert!(template
                .place_blocks(
                    &mut ctx,
                    &mut rng,
                    Placement {
                        origin,
                        pivot: (0, 0, 0),
                        mirror: Mirror::None,
                        rotation: Rotation::None,
                    },
                    &[],
                    [0, 0, 0, 31, 255, 31],
                )
                .is_ok());
            assert_eq!(ctx.payloads.len(), 3, "chest loot + furnace + pot loot stage payloads; structural BEs pass through");
            assert_eq!(
                ctx.payloads[0],
                (
                    (4, 70, 4),
                    GenerationPayload::LootContainer {
                        kind: ContainerKind::Chest,
                        loot_table: "minecraft:chests/abandoned_mineshaft".to_owned(),
                        loot_seed: Some(424242),
                    }
                )
            );
            assert!(matches!(
                &ctx.payloads[1],
                ((5, 70, 4), GenerationPayload::Container { kind: ContainerKind::Furnace, slots })
                if slots.len() == 1
            ));
            assert_eq!(
                ctx.payloads[2],
                (
                    (7, 70, 4),
                    GenerationPayload::LootContainer {
                        kind: ContainerKind::BlockContainer(1),
                        loot_table: "minecraft:pots/trial_chambers/corridor".to_owned(),
                        loot_seed: None,
                    }
                ),
                "a decorated pot's LootTable now stages; only sherds/item stay passthrough"
            );
            let _ = air;
            let mut staged = std::mem::take(&mut ctx.payloads);
            g.stage_payloads((0, 0), &mut staged);
        }
        crate::materialize_generation_payloads(g.take_staged_payloads());
        assert!(
            !crate::chests().lock().unwrap_or_else(|e| e.into_inner()).contains_key(&crate::ContainerKey::ow((4, 70, 4))),
            "placed loot chest remains unexpanded until first open/unpack"
        );
        assert!(crate::unpack_block_loot_if_needed(crate::ow_block((4, 70, 4))));
        let expected_loot = crate::roll_cart_loot(424242);
        assert_eq!(
            crate::chests().lock().unwrap_or_else(|e| e.into_inner()).get(&crate::ContainerKey::ow((4, 70, 4))).cloned(),
            Some(expected_loot)
        );
        assert!(crate::furnaces().lock().unwrap_or_else(|e| e.into_inner()).contains_key(&crate::ow_block((5, 70, 4))));
        crate::chests().lock().unwrap_or_else(|e| e.into_inner()).remove(&crate::ContainerKey::ow((4, 70, 4)));
        crate::pending_block_loot().lock().unwrap_or_else(|e| e.into_inner()).remove(&crate::ow_block((4, 70, 4)));
        crate::pending_block_loot().lock().unwrap_or_else(|e| e.into_inner()).remove(&crate::ow_block((7, 70, 4)));
        crate::furnaces().lock().unwrap_or_else(|e| e.into_inner()).remove(&crate::ow_block((5, 70, 4)));
    }

    #[test]
    fn place_blocks_updates_inside_boundary_shapes_and_schedules_inside_fluids() {
        use crate::generation_context::FC;
        use crate::generation_math::Xor;
        use crate::structure_transform::{Mirror, Rotation};
        let reg = crate::block_registry::breg().unwrap();
        let fence = reg.state_of("minecraft:oak_fence", None);
        let stone = reg.state_of("minecraft:stone", None);
        let water_stair = reg.with_prop(
            reg.state_of("minecraft:oak_stairs", None),
            "waterlogged",
            "true",
        );
        let air = reg.state_of("minecraft:air", None);
        let p_fence = (8, 72, 8);
        let p_stair = (9, 72, 8);
        let template = Template {
            size: (2, 1, 1),
            palettes: vec![vec![
                BlockInfo { pos: (0, 0, 0), state: fence, nbt: None },
                BlockInfo { pos: (1, 0, 0), state: water_stair, nbt: None },
            ]],
            entities: Vec::new(),
        };
        let mut g = crate::worldgen::Gen::load(0, crate::name_to_state).unwrap();
        let mut ctx = FC::new(&mut g);
        ctx.test_no_terrain = true;
        ctx.test_default_air = true;
        ctx.test_base_cell((7, 72, 8), stone);
        ctx.test_base_cell((8, 72, 8), air);
        ctx.test_base_cell((9, 72, 8), air);
        let mut rng = Xor::seed_worldgen(1);
        assert!(template
            .place_blocks(
                &mut ctx,
                &mut rng,
                Placement {
                    origin: p_fence,
                    pivot: (0, 0, 0),
                    mirror: Mirror::None,
                    rotation: Rotation::None,
                },
                &[],
                [0, 0, 0, 31, 255, 31],
            )
            .unwrap());
        assert_eq!(
            reg.get_prop(ctx.get(p_fence.0, p_fence.1, p_fence.2), "west").as_deref(),
            Some("true"),
            "placed inside fence must connect to adjacent world full cube"
        );
        assert_eq!(
            ctx.g.generated_fluid_ticks(),
            vec![(p_stair, 0)],
            "placed inside waterlogged stair must reserve fluid tick"
        );
    }

    /// `updateFromNeighbourShapes`: a written cell whose neighbors are all written
    /// too has no exposed face, so only the per-cell pass can connect it.
    #[test]
    fn neighbour_shapes_pass_connects_a_fully_surrounded_written_fence() {
        use crate::generation_context::FC;
        use crate::generation_math::Xor;
        use crate::structure_transform::{Mirror, Rotation};
        let reg = crate::block_registry::breg().unwrap();
        let bone = reg.state_of("minecraft:bone_block", None);
        let fence = reg.state_of("minecraft:oak_fence", None);
        let mut blocks = vec![BlockInfo { pos: (0, 0, 0), state: fence, nbt: None }];
        for pos in [(1, 0, 0), (-1, 0, 0), (0, 1, 0), (0, -1, 0), (0, 0, 1), (0, 0, -1)] {
            blocks.push(BlockInfo { pos, state: bone, nbt: None });
        }
        let template = Template {
            size: (3, 3, 3),
            palettes: vec![blocks],
            entities: Vec::new(),
        };
        let mut g = crate::worldgen::Gen::load(0, crate::name_to_state).unwrap();
        let mut ctx = FC::new(&mut g);
        ctx.test_no_terrain = true;
        ctx.test_default_air = true;
        ctx.test_writes = Some(Vec::new());
        let mut rng = Xor::seed_worldgen(3);
        assert!(template
            .place_blocks(
                &mut ctx,
                &mut rng,
                Placement {
                    origin: (8, 72, 8), pivot: (0, 0, 0),
                    mirror: Mirror::None, rotation: Rotation::None,
                },
                &[], [0, 0, 0, 31, 255, 31],
            )
            .unwrap());
        let placed = ctx.get(8, 72, 8);
        for side in ["north", "south", "west", "east"] {
            assert_eq!(
                reg.get_prop(placed, side).as_deref(),
                Some("true"),
                "a written fence inside written cubes connects on {side}"
            );
        }
        assert_eq!(
            ctx.test_writes.as_ref().unwrap().len(),
            8,
            "seven template writes plus the one neighbor-shape fix"
        );
    }

    /// `updateFromNeighbourShapes` applies the fragile-support check to a written
    /// cell, not only to world cells at the template edge: a torch the template
    /// puts on a single slab must break the way `canSupportCenter` breaks it.
    #[test]
    fn neighbour_shapes_pass_breaks_fragile_block_on_written_partial_support() {
        use crate::generation_context::FC;
        use crate::generation_math::Xor;
        use crate::structure_transform::{Mirror, Rotation};
        let reg = crate::block_registry::breg().unwrap();
        let air = reg.state_of("minecraft:air", None);
        let slab = reg.with_prop(reg.state_of("minecraft:oak_slab", None), "type", "bottom");
        let torch = reg.state_of("minecraft:torch", None);
        let template = Template {
            size: (1, 2, 1),
            palettes: vec![vec![
                BlockInfo { pos: (0, 0, 0), state: slab, nbt: None },
                BlockInfo { pos: (0, 1, 0), state: torch, nbt: None },
            ]],
            entities: Vec::new(),
        };
        let mut g = crate::worldgen::Gen::load(0, crate::name_to_state).unwrap();
        let mut ctx = FC::new(&mut g);
        ctx.test_no_terrain = true;
        ctx.test_default_air = true;
        ctx.test_writes = Some(Vec::new());
        let mut rng = Xor::seed_worldgen(5);
        assert!(template
            .place_blocks(
                &mut ctx,
                &mut rng,
                Placement {
                    origin: (8, 72, 8), pivot: (0, 0, 0),
                    mirror: Mirror::None, rotation: Rotation::None,
                },
                &[], [0, 0, 0, 31, 255, 31],
            )
            .unwrap());
        assert_eq!(ctx.get(8, 72, 8), slab);
        assert_eq!(
            ctx.get(8, 73, 8),
            air,
            "a single slab's top face is not center-sturdy, so the torch breaks"
        );
        assert_eq!(ctx.test_writes.as_ref().unwrap().len(), 3);
    }

    /// The shared `update_shape` families the bounded edge rule does not name
    /// still finalize at template edges: `SnowyDirtBlock.updateShape` marks a
    /// written grass block under snow, and `WallBannerBlock.updateShape` drops
    /// a banner whose written support leaves (the end city's tower edge case).
    #[test]
    fn shared_update_families_finalize_at_template_edges() {
        use crate::generation_context::FC;
        use crate::generation_math::Xor;
        use crate::structure_transform::{Mirror, Rotation};
        let reg = crate::block_registry::breg().unwrap();
        let air = reg.state_of("minecraft:air", None);
        let bone = reg.state_of("minecraft:bone_block", None);
        let grass = reg.state_of("minecraft:grass_block", None);
        let snow = reg.state_of("minecraft:snow_block", None);
        let banner = reg.with_prop(
            reg.state_of("minecraft:magenta_wall_banner", None),
            "facing",
            "south",
        );
        let placement = |origin| Placement {
            origin,
            pivot: (0, 0, 0),
            mirror: Mirror::None,
            rotation: Rotation::None,
        };

        // A written grass block with a snow block above its edge: the inside
        // side of the exposed face gets `snowy=true`.
        let grassy = Template {
            size: (1, 1, 1),
            palettes: vec![vec![BlockInfo { pos: (0, 0, 0), state: grass, nbt: None }]],
            entities: Vec::new(),
        };
        let mut g = crate::worldgen::Gen::load(0, crate::name_to_state).unwrap();
        let mut ctx = FC::new(&mut g);
        ctx.test_no_terrain = true;
        ctx.test_default_air = true;
        ctx.test_base_cell((8, 73, 8), snow);
        let mut rng = Xor::seed_worldgen(11);
        assert!(grassy
            .place_blocks(&mut ctx, &mut rng, placement((8, 72, 8)), &[], [0, 0, 0, 31, 255, 31])
            .unwrap());
        assert_eq!(
            reg.get_prop(ctx.get(8, 72, 8), "snowy").as_deref(),
            Some("true"),
            "grass under an outside snow block must turn snowy"
        );

        // A banner whose support the template erases: facing "south" means the
        // support is the cell north of the banner; writing air there drops it.
        let erase = Template {
            size: (1, 1, 1),
            palettes: vec![vec![BlockInfo { pos: (0, 0, 0), state: air, nbt: None }]],
            entities: Vec::new(),
        };
        let mut g = crate::worldgen::Gen::load(0, crate::name_to_state).unwrap();
        let mut ctx = FC::new(&mut g);
        ctx.test_no_terrain = true;
        ctx.test_default_air = true;
        ctx.test_base_cell((8, 72, 7), bone);
        ctx.test_base_cell((8, 72, 8), banner);
        let mut rng = Xor::seed_worldgen(11);
        assert!(erase
            .place_blocks(&mut ctx, &mut rng, placement((8, 72, 7)), &[], [0, 0, 0, 31, 255, 31])
            .unwrap());
        assert_eq!(ctx.get(8, 72, 7), air, "the template erased the support cell");
        assert_eq!(
            ctx.get(8, 72, 8),
            air,
            "the wall banner loses its written support and drops"
        );
    }

    /// Extended container staging: dispenser/hopper/shulker-box `Items` and a
    /// dispenser `LootTable` are staged instead of silently dropped, while
    /// unknown fields still fail closed.
    #[test]
    fn extended_containers_stage_and_unknown_fields_fail_closed() {
        use crate::generation_payload::{ContainerKind, GenerationPayload};
        let reg = crate::block_registry::breg().unwrap();
        let tag = |entries: Vec<(&str, Tag)>| {
            Tag::Compound(
                entries
                    .into_iter()
                    .map(|(k, v)| (NbtString::text(k), v))
                    .collect(),
            )
        };
        let id = |name: &str| ("id", Tag::String(NbtString::text(name)));
        let items = |slot: i8, item: &str, count: i32| {
            ("Items", Tag::List(10, vec![container_entry(slot, item, count)]))
        };

        for (block, kind) in [
            ("minecraft:dispenser", ContainerKind::BlockContainer(9)),
            ("minecraft:dropper", ContainerKind::BlockContainer(9)),
            ("minecraft:crafter", ContainerKind::BlockContainer(9)),
            ("minecraft:hopper", ContainerKind::BlockContainer(5)),
            ("minecraft:white_shulker_box", ContainerKind::Chest),
        ] {
            let state = reg.state_of(block, None);
            let nbt = tag(vec![id(block), items(0, "minecraft:iron_ore", 3)]);
            assert!(
                !Template::is_passthrough_block_entity(state, &nbt, reg),
                "{block} must route through the payload seam, not passthrough"
            );
            assert_eq!(
                Template::staged_payload(&nbt, state, reg),
                Ok(GenerationPayload::Container {
                    kind,
                    slots: vec![(0, crate::name_to_item("minecraft:iron_ore").unwrap(), 3)],
                }),
                "{block} Items stage into the owning store"
            );
        }

        let dispenser = reg.state_of("minecraft:dispenser", None);
        let loot = tag(vec![
            id("minecraft:dispenser"),
            ("LootTable", Tag::String(NbtString::text("minecraft:chests/simple_dungeon"))),
            ("LootTableSeed", Tag::Long(7)),
        ]);
        assert_eq!(
            Template::staged_payload(&loot, dispenser, reg),
            Ok(GenerationPayload::LootContainer {
                kind: ContainerKind::BlockContainer(9),
                loot_table: "minecraft:chests/simple_dungeon".to_owned(),
                loot_seed: Some(7),
            }),
            "a dispenser LootTable now stages instead of being dropped"
        );

        let bad = tag(vec![
            id("minecraft:dispenser"),
            items(0, "minecraft:iron_ore", 1),
            ("Failure", Tag::Int(1)),
        ]);
        assert!(Template::staged_payload(&bad, dispenser, reg).is_err(),
            "an unhandled field on a storable container is refused");

        // Content keys a live store reads but the seam cannot carry yet are
        // refused rather than silently dropped.
        let lectern = reg.state_of("minecraft:lectern", None);
        assert!(Template::is_passthrough_block_entity(
            lectern,
            &tag(vec![id("minecraft:lectern")]),
            reg
        ));
        assert!(!Template::is_passthrough_block_entity(
            lectern,
            &tag(vec![id("minecraft:lectern"), ("Book", Tag::Compound(BTreeMap::new()))]),
            reg
        ));
        assert!(Template::staged_payload(
            &tag(vec![id("minecraft:lectern"), ("Book", Tag::Compound(BTreeMap::new()))]),
            lectern,
            reg
        )
        .is_err());
        let bookshelf = reg.state_of("minecraft:chiseled_bookshelf", None);
        assert!(!Template::is_passthrough_block_entity(
            bookshelf,
            &tag(vec![id("minecraft:chiseled_bookshelf"), items(0, "minecraft:book", 1)]),
            reg
        ));

        // Brewing stands and campfires carry `Items` in the corpus but have no
        // payload route; pinning that bound so a later seam must move them.
        let brewing = reg.state_of("minecraft:brewing_stand", None);
        assert!(Template::is_passthrough_block_entity(
            brewing,
            &tag(vec![id("minecraft:brewing_stand"), items(0, "minecraft:iron_ore", 1)]),
            reg
        ));
    }

    /// A decorated pot's `sherds`/`item` stay passthrough (no payload seam), but
    /// its `LootTable`/`Items` stage like every other randomizable container.
    #[test]
    fn decorated_pot_sherds_pass_through_but_loot_stages() {
        use crate::generation_payload::{ContainerKind, GenerationPayload};
        let reg = crate::block_registry::breg().unwrap();
        let pot = reg.state_of("minecraft:decorated_pot", None);
        let tag = |entries: Vec<(&str, Tag)>| {
            Tag::Compound(
                entries
                    .into_iter()
                    .map(|(k, v)| (NbtString::text(k), v))
                    .collect(),
            )
        };
        let sherd_pot = tag(vec![
            ("id", Tag::String(NbtString::text("minecraft:decorated_pot"))),
            ("LootTable", Tag::String(NbtString::text("minecraft:pots/trial_chambers/corridor"))),
            ("sherds", Tag::List(8, vec![])),
        ]);
        assert!(
            Template::is_passthrough_block_entity(pot, &sherd_pot, reg),
            "a pot carrying sherds stays passthrough (no seam for them)"
        );
        let loot_pot = tag(vec![
            ("id", Tag::String(NbtString::text("minecraft:decorated_pot"))),
            ("LootTable", Tag::String(NbtString::text("minecraft:pots/trial_chambers/corridor"))),
        ]);
        assert!(!Template::is_passthrough_block_entity(pot, &loot_pot, reg));
        assert_eq!(
            Template::staged_payload(&loot_pot, pot, reg),
            Ok(GenerationPayload::LootContainer {
                kind: ContainerKind::BlockContainer(1),
                loot_table: "minecraft:pots/trial_chambers/corridor".to_owned(),
                loot_seed: None,
            })
        );
    }

    /// `SpawnData.entity.id` is the one spawner field the runtime model keeps;
    /// the rest of the vanilla `BaseSpawner` layout is accepted by name, and
    /// anything outside it — or a malformed `SpawnData` — fails closed.
    #[test]
    fn spawner_spawn_data_stages_entity_and_fails_closed() {
        use crate::generation_payload::GenerationPayload;
        let reg = crate::block_registry::breg().unwrap();
        let spawner = reg.state_of("minecraft:spawner", None);
        let tag = |entries: Vec<(&str, Tag)>| {
            Tag::Compound(
                entries
                    .into_iter()
                    .map(|(k, v)| (NbtString::text(k), v))
                    .collect(),
            )
        };
        let spawn_data = tag(vec![(
            "entity",
            tag(vec![("id", Tag::String(NbtString::text("minecraft:cave_spider")))]),
        )]);
        let full = tag(vec![
            ("id", Tag::String(NbtString::text("minecraft:spawner"))),
            ("Delay", Tag::Short(0)),
            ("MinSpawnDelay", Tag::Short(200)),
            ("MaxSpawnDelay", Tag::Short(800)),
            ("SpawnCount", Tag::Short(4)),
            ("MaxNearbyEntities", Tag::Short(6)),
            ("RequiredPlayerRange", Tag::Short(16)),
            ("SpawnRange", Tag::Short(4)),
            ("SpawnData", spawn_data.clone()),
            (
                "SpawnPotentials",
                Tag::List(10, vec![tag(vec![
                    ("weight", Tag::Int(1)),
                    ("data", tag(vec![(
                        "entity",
                        tag(vec![("id", Tag::String(NbtString::text("minecraft:cave_spider")))]),
                    )])),
                ])]),
            ),
        ]);
        assert!(!Template::is_passthrough_block_entity(spawner, &full, reg),
            "a configured spawner must not pass through");
        assert_eq!(
            Template::staged_payload(&full, spawner, reg),
            Ok(GenerationPayload::SpawnerEntity(
                "minecraft:cave_spider".to_owned()
            )),
            "the serving entity id survives staging"
        );

        let id_only = tag(vec![("id", Tag::String(NbtString::text("minecraft:spawner")))]);
        assert!(
            Template::is_passthrough_block_entity(spawner, &id_only, reg),
            "a bare spawner marker stays structural"
        );

        for (case, nbt) in [
            ("unknown field", tag(vec![
                ("id", Tag::String(NbtString::text("minecraft:spawner"))),
                ("SpawnData", spawn_data.clone()),
                ("PlayerRange", Tag::Short(4)),
            ])),
            ("SpawnData is not a compound", tag(vec![
                ("SpawnData", Tag::Int(3)),
            ])),
            ("SpawnPotentials not a list", tag(vec![
                ("SpawnData", spawn_data.clone()),
                ("SpawnPotentials", Tag::Compound(BTreeMap::new())),
            ])),
            ("entity id missing", tag(vec![(
                "SpawnData",
                tag(vec![("entity", tag(vec![]))]),
            )])),
        ] {
            assert!(
                Template::staged_payload(&nbt, spawner, reg).is_err(),
                "malformed spawner NBT must fail closed: {case}"
            );
        }
    }

    /// `SpawnData` on a placed spawner block reaches the payload seam end to end.
    #[test]
    fn place_blocks_stages_the_spawner_entity() {
        use crate::generation_context::FC;
        use crate::generation_math::Xor;
        use crate::generation_payload::GenerationPayload;
        use crate::structure_transform::{Mirror, Rotation};
        let reg = crate::block_registry::breg().unwrap();
        let spawner = reg.state_of("minecraft:spawner", None);
        let mut inner = BTreeMap::new();
        inner.insert(NbtString::text("id"), Tag::String(NbtString::text("minecraft:cave_spider")));
        let mut spawn_data = BTreeMap::new();
        spawn_data.insert(NbtString::text("entity"), Tag::Compound(inner));
        let mut nbt = BTreeMap::new();
        nbt.insert(NbtString::text("id"), Tag::String(NbtString::text("minecraft:spawner")));
        nbt.insert(NbtString::text("SpawnData"), Tag::Compound(spawn_data));
        let template = Template {
            size: (1, 1, 1),
            palettes: vec![vec![BlockInfo {
                pos: (0, 0, 0),
                state: spawner,
                nbt: Some(Tag::Compound(nbt)),
            }]],
            entities: Vec::new(),
        };
        assert!(template.validate_placement().is_ok());
        let mut g = crate::worldgen::Gen::load(0, crate::name_to_state).unwrap();
        let mut ctx = FC::new(&mut g);
        ctx.test_no_terrain = true;
        ctx.test_default_air = true;
        let mut rng = Xor::seed_worldgen(13);
        assert!(template
            .place_blocks(
                &mut ctx,
                &mut rng,
                Placement {
                    origin: (8, 72, 8), pivot: (0, 0, 0),
                    mirror: Mirror::None, rotation: Rotation::None,
                },
                &[], [0, 0, 0, 31, 255, 31],
            )
            .unwrap());
        assert_eq!(
            ctx.payloads,
            vec![(
                (8, 72, 8),
                GenerationPayload::SpawnerEntity("minecraft:cave_spider".to_owned())
            )]
        );
    }

    /// A sign's `back_text` is decoded with the same rules as the front: a
    /// malformed back side fails closed even though the payload only carries
    /// the front's four lines today.
    #[test]
    fn sign_back_text_is_validated_and_front_text_stages() {
        use crate::generation_payload::GenerationPayload;
        let reg = crate::block_registry::breg().unwrap();
        let sign = reg.state_of("minecraft:oak_sign", None);
        let side = |lines: [&str; 4]| {
            Tag::Compound(BTreeMap::from([
                (
                    NbtString::text("messages"),
                    Tag::List(
                        8,
                        lines
                            .iter()
                            .map(|l| {
                                Tag::String(NbtString::text(format!("\"{l}\"").as_str()))
                            })
                            .collect(),
                    ),
                ),
                (NbtString::text("color"), Tag::String(NbtString::text("white"))),
                (NbtString::text("has_glowing_text"), Tag::Byte(0)),
            ]))
        };
        let mut nbt = BTreeMap::new();
        nbt.insert(NbtString::text("front_text"), side(["Front", "", "", ""]));
        nbt.insert(NbtString::text("back_text"), side(["Back", "", "", ""]));
        assert_eq!(
            Template::staged_payload(&Tag::Compound(nbt), sign, reg),
            Ok(GenerationPayload::Sign([
                "Front".to_owned(),
                String::new(),
                String::new(),
                String::new()
            ])),
            "a valid back_text validates and the front still stages"
        );
        let mut nbt = BTreeMap::new();
        nbt.insert(NbtString::text("front_text"), side(["Front", "", "", ""]));
        nbt.insert(
            NbtString::text("back_text"),
            Tag::Compound(BTreeMap::from([(
                NbtString::text("messages"),
                Tag::List(8, vec![Tag::String(NbtString::text("\"x\""))]),
            )])),
        );
        assert_eq!(
            Template::staged_payload(&Tag::Compound(nbt), sign, reg),
            Err("a sign side has exactly four messages"),
            "a malformed back_text is refused, not silently dropped"
        );
    }

    /// This entry point has no entity seam, so a template whose `entities` list
    /// is populated is refused rather than silently dropping the spawns.
    #[test]
    fn place_blocks_refuses_a_template_carrying_entities() {
        let reg = crate::block_registry::breg().unwrap();
        let template = Template {
            size: (1, 1, 1),
            palettes: vec![vec![BlockInfo {
                pos: (0, 0, 0),
                state: reg.state_of("minecraft:bone_block", None),
                nbt: None,
            }]],
            entities: vec![EntityInfo {
                pos: [0.5, 0.0, 0.5],
                block_pos: (0, 0, 0),
                nbt: Tag::Compound(BTreeMap::from([(
                    NbtString::text("id"),
                    Tag::String(NbtString::text("minecraft:pig")),
                )])),
            }],
        };
        let mut g = crate::worldgen::Gen::load(0, crate::name_to_state).unwrap();
        let mut ctx = crate::generation_context::FC::new(&mut g);
        ctx.test_no_terrain = true;
        ctx.test_default_air = true;
        let mut rng = Xor::seed_worldgen(5);
        assert_eq!(
            template.place_blocks(
                &mut ctx,
                &mut rng,
                Placement {
                    origin: (8, 72, 8), pivot: (0, 0, 0),
                    mirror: Mirror::None, rotation: Rotation::None,
                },
                &[],
                [0, 0, 0, 31, 255, 31],
            ),
            Err("template placement does not handle template entities"),
        );
        assert_eq!(
            ctx.get(8, 72, 8),
            reg.state_of("minecraft:air", None),
            "a refused template wrote nothing"
        );
    }

    /// `CarpetBlock.updateShape` checks `!level.isEmptyBlock(below)` on every
    /// direction: a written cell that empties the floor drops the carpet, and a
    /// liquid floor keeps it (the floor is not air even though it is fluid).
    /// `RedStoneWireBlock.updateShape` carries its survival check only on DOWN:
    /// a written neighbour that leaves the below cell non-`isFaceSturdy(UP, FULL)`
    /// drops the wire, while a hopper or a sturdy top keeps it, and a horizontal
    /// touch leaves the wire's side graph to the placed state.
    #[test]
    fn place_blocks_breaks_carpets_and_redstone_wire_on_lost_support() {
        use crate::generation_context::FC;
        use crate::generation_math::Xor;
        use crate::structure_transform::{Mirror, Rotation};
        let reg = crate::block_registry::breg().unwrap();
        let air = reg.state_of("minecraft:air", None);
        let stone = reg.state_of("minecraft:stone", None);
        let carpet = reg.state_of("minecraft:white_carpet", None);
        let wire = reg.state_of("minecraft:redstone_wire", None);
        let hopper = reg.state_of("minecraft:hopper", None);
        let bottom_slab = reg.with_prop(reg.state_of("minecraft:oak_slab", None), "type", "bottom");
        let run = |world: &[((i32, i32, i32), u32)],
                   writes: &[((i32, i32, i32), u32)],
                   read: (i32, i32, i32)| {
            let template = Template {
                size: (1, 1, 1),
                palettes: vec![writes
                    .iter()
                    .map(|&(_, state)| BlockInfo { pos: (0, 0, 0), state, nbt: None })
                    .collect()],
                entities: Vec::new(),
            };
            let mut g = crate::worldgen::Gen::load(0, crate::name_to_state).unwrap();
            let mut ctx = FC::new(&mut g);
            ctx.test_no_terrain = true;
            ctx.test_default_air = true;
            for &(p, st) in world {
                ctx.test_base_cell(p, st);
            }
            let mut rng = Xor::seed_worldgen(7);
            template
                .place_blocks(
                    &mut ctx,
                    &mut rng,
                    Placement {
                        origin: writes[0].0,
                        pivot: (0, 0, 0),
                        mirror: Mirror::None,
                        rotation: Rotation::None,
                    },
                    &[],
                    [0, 0, 0, 31, 255, 31],
                )
                .unwrap();
            ctx.get(read.0, read.1, read.2)
        };
        // Carpet over a floor the template erases: DOWN-touch -> air.
        assert_eq!(
            run(
                &[((8, 72, 8), stone), ((8, 73, 8), carpet)],
                &[((8, 72, 8), air)],
                (8, 73, 8)
            ),
            air,
            "a carpet whose floor becomes empty drops, on any direction"
        );
        // Carpet over water: `isEmptyBlock` is the air predicate, not a dry one.
        assert_eq!(
            run(
                &[((8, 72, 8), reg.state_of("minecraft:water", None)), ((8, 73, 8), carpet), ((9, 73, 8), air)],
                &[((9, 73, 8), stone)],
                (8, 73, 8)
            ),
            carpet,
            "a carpet over water stays: water is not an empty block"
        );
        // Wire above a written half slab: bottom slab's UP face is not FULL.
        assert_eq!(
            run(
                &[((8, 73, 8), wire)],
                &[((8, 72, 8), bottom_slab)],
                (8, 73, 8)
            ),
            air,
            "wire above a bottom slab loses isFaceSturdy(_, UP, FULL) and drops"
        );
        // Wire above a written hopper: `canSurviveOn` accepts hoppers.
        assert_eq!(
            run(
                &[((8, 73, 8), wire)],
                &[((8, 72, 8), hopper)],
                (8, 73, 8)
            ),
            wire,
            "wire above a hopper stays"
        );
        // Wire above a written full cube stays too.
        assert_eq!(
            run(
                &[((8, 73, 8), wire)],
                &[((8, 72, 8), stone)],
                (8, 73, 8)
            ),
            wire,
            "wire above a full cube stays"
        );
        // A horizontal touch does not rewrite the wire: the horizontal arms
        // recompute the wire graph, which this path does not own.
        assert_eq!(
            run(
                &[((8, 73, 8), wire), ((9, 73, 8), air)],
                &[((9, 73, 8), stone)],
                (8, 73, 8)
            ),
            wire,
            "a horizontal edge leaves the wire's placed side properties alone"
        );
    }

    /// `BaseRailBlock.updateShape` (`dfn.a`) only schedules the waterlogged
    /// fluid tick — it never rewrites the rail — and `FallingBlock.updateShape`
    /// (`diu.a`) only reserves a block tick. Both keep their state at a template
    /// edge; the waterlogged rail's fluid reservation still files.
    #[test]
    fn place_blocks_keeps_rails_and_falling_blocks_but_reserves_rail_water_tick() {
        use crate::generation_context::FC;
        use crate::generation_math::Xor;
        use crate::structure_transform::{Mirror, Rotation};
        let reg = crate::block_registry::breg().unwrap();
        let stone = reg.state_of("minecraft:stone", None);
        let rail = reg.state_of("minecraft:rail", None);
        let sand = reg.state_of("minecraft:sand", None);
        let water_rail = reg.with_prop(rail, "waterlogged", "true");
        let template = Template {
            size: (1, 1, 1),
            palettes: vec![vec![BlockInfo { pos: (0, 0, 0), state: stone, nbt: None }]],
            entities: Vec::new(),
        };
        let mut g = crate::worldgen::Gen::load(0, crate::name_to_state).unwrap();
        let mut ctx = FC::new(&mut g);
        ctx.test_no_terrain = true;
        ctx.test_default_air = true;
        ctx.test_base_cell((8, 73, 8), rail);
        ctx.test_base_cell((9, 72, 8), water_rail);
        ctx.test_base_cell((8, 71, 8), sand);
        let mut rng = Xor::seed_worldgen(9);
        template
            .place_blocks(
                &mut ctx,
                &mut rng,
                Placement {
                    origin: (8, 72, 8), pivot: (0, 0, 0),
                    mirror: Mirror::None, rotation: Rotation::None,
                },
                &[],
                [0, 0, 0, 31, 255, 31],
            )
            .unwrap();
        assert_eq!(ctx.get(8, 73, 8), rail, "rails ignore updateShape writes");
        assert_eq!(
            ctx.get(9, 72, 8),
            water_rail,
            "a waterlogged rail keeps its state too"
        );
        assert_eq!(
            ctx.get(8, 71, 8),
            sand,
            "falling blocks only reserve a tick; there is no shape write"
        );
        assert_eq!(
            ctx.g.generated_fluid_ticks(),
            vec![((9, 72, 8), 0)],
            "the waterlogged rail's fluid tick reservation still files"
        );
    }

    /// `StructurePiece.placeBlock` files `SHAPE_CHECK_BLOCKS` writes for
    /// `postProcessGeneration`; the bounded template path files the same mark
    /// for its writes, and only for them.
    #[test]
    fn place_blocks_files_shape_check_marks_only_for_marked_states() {
        use crate::generation_context::FC;
        use crate::generation_math::Xor;
        use crate::structure_transform::{Mirror, Rotation};
        let reg = crate::block_registry::breg().unwrap();
        let template = Template {
            size: (2, 1, 1),
            palettes: vec![vec![
                BlockInfo {
                    pos: (0, 0, 0),
                    state: reg.state_of("minecraft:oak_fence", None),
                    nbt: None,
                },
                BlockInfo {
                    pos: (1, 0, 0),
                    state: reg.state_of("minecraft:stone", None),
                    nbt: None,
                },
            ]],
            entities: Vec::new(),
        };
        let mut g = crate::worldgen::Gen::load(0, crate::name_to_state).unwrap();
        let mut ctx = FC::new(&mut g);
        ctx.test_no_terrain = true;
        ctx.test_default_air = true;
        let mut rng = Xor::seed_worldgen(11);
        template
            .place_blocks(
                &mut ctx,
                &mut rng,
                Placement {
                    origin: (8, 72, 8), pivot: (0, 0, 0),
                    mirror: Mirror::None, rotation: Rotation::None,
                },
                &[],
                [0, 0, 0, 31, 255, 31],
            )
            .unwrap();
        assert_eq!(
            ctx.shape_marks,
            vec![(8, 72, 8)],
            "the fence write files its SHAPE_CHECK mark; the full cube does not"
        );
    }

    /// `Entity.mirror`/`Entity.rotate` exactly as `placeEntities` composes them:
    /// `f = mirror(m)(w) + rotate(r)(w) - raw` with `w = wrapDegrees(raw)`, so
    /// the stored yaw is wrapped twice — once inside each call — and an
    /// out-of-range stored yaw keeps its residue. `pitch` passes through.
    #[test]
    fn transformed_entities_applies_native_yaw_mirror_and_rotation() {
        use crate::structure_transform::{Mirror, Rotation};
        let entity = |yaw: f32, pitch: f32| EntityInfo {
            pos: [0.25, 0.5, 0.75],
            block_pos: (0, 0, 0),
            nbt: Tag::Compound(BTreeMap::from([
                (
                    NbtString::text("id"),
                    Tag::String(NbtString::text("minecraft:pig")),
                ),
                (
                    NbtString::text("Rotation"),
                    Tag::List(
                        5,
                        vec![Tag::Float(yaw.to_bits()), Tag::Float(pitch.to_bits())],
                    ),
                ),
            ])),
        };
        let place = |mirror, rotation| Placement {
            origin: (0, 0, 0),
            pivot: (0, 0, 0),
            mirror,
            rotation,
        };
        // (yaw, mirror, rotation) -> f. Values are the literal native formula,
        // not a wrapped yaw: e.g. north (180) mirrored left_right yields 0.
        for (yaw, mirror, rotation, want) in [
            (0.0f32, Mirror::None, Rotation::None, 0.0f32),
            (0.0, Mirror::None, Rotation::Clockwise90, 90.0),
            (0.0, Mirror::None, Rotation::Clockwise180, 180.0),
            (0.0, Mirror::None, Rotation::Counterclockwise90, 270.0),
            (90.0, Mirror::LeftRight, Rotation::None, 90.0),
            (90.0, Mirror::FrontBack, Rotation::None, -90.0),
            (180.0, Mirror::LeftRight, Rotation::None, 0.0),
            (45.0, Mirror::LeftRight, Rotation::Clockwise90, 225.0),
            (370.0, Mirror::None, Rotation::None, -350.0),
        ] {
            let template = Template {
                size: (1, 1, 1),
                palettes: vec![Vec::new()],
                entities: vec![entity(yaw, 30.0)],
            };
            let got = template.transformed_entities(place(mirror, rotation)).unwrap();
            assert_eq!(
                got[0].yaw, want,
                "yaw {yaw} under {mirror:?}/{rotation:?}"
            );
            assert_eq!(got[0].pitch, 30.0, "pitch is never transformed");
        }
    }

    /// `placeEntities` transforms the stored doubles AND the `blockPos` anchor:
    /// `pos` adds the origin after the mirror/rotation, `block_pos` goes through
    /// the integer `transform` and then adds the origin.
    #[test]
    fn transformed_entities_transforms_pos_and_block_pos() {
        use crate::structure_transform::{Mirror, Rotation};
        let template = Template {
            size: (4, 1, 4),
            palettes: vec![Vec::new()],
            entities: vec![EntityInfo {
                pos: [0.25, 0.5, 0.75],
                block_pos: (3, 0, 1),
                nbt: Tag::Compound(BTreeMap::from([(
                    NbtString::text("id"),
                    Tag::String(NbtString::text("minecraft:cat")),
                )])),
            }],
        };
        let placement = Placement {
            origin: (64, 80, -32),
            pivot: (0, 0, 0),
            mirror: Mirror::FrontBack,
            rotation: Rotation::Clockwise90,
        };
        let got = template.transformed_entities(placement).unwrap();
        assert_eq!(got.len(), 1);
        let e = &got[0];
        // FrontBack flips x (1-x for the doubles, -x for the block anchor),
        // then CW90 maps (x,y,z) -> (-z, y, x) around the zero pivot.
        assert_eq!(e.pos, [0.25 + 64.0, 0.5 + 80.0, 0.75 - 32.0]);
        assert_eq!(e.block_pos, (63, 80, -35));
        assert_eq!(e.id, "minecraft:cat");
        assert_eq!(
            e.yaw, 90.0,
            "a missing Rotation list defaults the stored yaw to 0; CW90 still adds +90"
        );
    }

    /// Malformed entity records fail closed instead of reaching the spawn path:
    /// a missing or non-string `id`, a non-list/non-float `Rotation`, a
    /// non-finite `Rotation`, or a non-finite `pos` all refuse.
    #[test]
    fn transformed_entities_rejects_malformed_records() {
        use crate::structure_transform::{Mirror, Rotation};
        let placement = Placement {
            origin: (0, 0, 0),
            pivot: (0, 0, 0),
            mirror: Mirror::None,
            rotation: Rotation::None,
        };
        let good_nbt = || {
            Tag::Compound(BTreeMap::from([(
                NbtString::text("id"),
                Tag::String(NbtString::text("minecraft:pig")),
            )]))
        };
        let template_with = |info: EntityInfo| Template {
            size: (1, 1, 1),
            palettes: vec![Vec::new()],
            entities: vec![info],
        };
        let base = || EntityInfo {
            pos: [0.5, 0.0, 0.5],
            block_pos: (0, 0, 0),
            nbt: good_nbt(),
        };
        // Missing id.
        let mut e = base();
        e.nbt = Tag::Compound(BTreeMap::new());
        assert!(template_with(e).transformed_entities(placement).is_err());
        // Non-string id.
        let mut e = base();
        e.nbt = Tag::Compound(BTreeMap::from([(
            NbtString::text("id"),
            Tag::Int(7),
        )]));
        assert!(template_with(e).transformed_entities(placement).is_err());
        // Rotation that is not a list.
        let mut e = base();
        let Tag::Compound(ref mut values) = e.nbt else { unreachable!() };
        values.insert(NbtString::text("Rotation"), Tag::Int(0));
        assert!(template_with(e).transformed_entities(placement).is_err());
        // Rotation with non-float elements.
        let mut e = base();
        let Tag::Compound(ref mut values) = e.nbt else { unreachable!() };
        values.insert(
            NbtString::text("Rotation"),
            Tag::List(6, vec![Tag::Double(0u64)]),
        );
        assert!(template_with(e).transformed_entities(placement).is_err());
        // One float is legal (native defaults the missing pitch to 0).
        let mut e = base();
        let Tag::Compound(ref mut values) = e.nbt else { unreachable!() };
        values.insert(
            NbtString::text("Rotation"),
            Tag::List(5, vec![Tag::Float(45f32.to_bits())]),
        );
        let got = template_with(e).transformed_entities(placement).unwrap();
        assert_eq!(got[0].yaw, 45.0);
        assert_eq!(got[0].pitch, 0.0);
        // Non-finite stored pos.
        let mut e = base();
        e.pos = [f64::NAN, 0.0, 0.0];
        assert!(template_with(e).transformed_entities(placement).is_err());
    }

    /// Directional block-entity states transform with the same table the rest
    /// of the template uses — a rotated chest/barrel and a rotated/mirrored
    /// standing sign land with facing/rotation already rewritten — and their
    /// NBT still stages. `loot` keeps `LootTableSeed`; a block entity the store
    /// cannot carry (a jukebox record) still fails closed.
    #[test]
    fn place_blocks_transforms_directional_block_entities_and_stages_them() {
        use crate::generation_context::FC;
        use crate::generation_math::Xor;
        use crate::generation_payload::GenerationPayload;
        use crate::structure_transform::{Mirror, Rotation};
        let reg = crate::block_registry::breg().unwrap();
        let chest = reg.with_prop(
            reg.state_of("minecraft:chest", None),
            "facing",
            "north",
        );
        let sign = reg.with_prop(reg.state_of("minecraft:oak_sign", None), "rotation", "4");
        let mut nbt = BTreeMap::new();
        nbt.insert(
            NbtString::text("id"),
            Tag::String(NbtString::text("minecraft:chest")),
        );
        nbt.insert(
            NbtString::text("LootTable"),
            Tag::String(NbtString::text("minecraft:chests/simple_dungeon")),
        );
        nbt.insert(NbtString::text("LootTableSeed"), Tag::Long(99));
        let front = Tag::Compound(BTreeMap::from([
            (
                NbtString::text("messages"),
                Tag::List(8, (0..4).map(|_| Tag::String(NbtString::text("\"\""))).collect()),
            ),
        ]));
        let mut sign_nbt = BTreeMap::new();
        sign_nbt.insert(NbtString::text("front_text"), front);
        let template = Template {
            size: (2, 1, 1),
            palettes: vec![vec![
                BlockInfo { pos: (0, 0, 0), state: chest, nbt: Some(Tag::Compound(nbt)) },
                BlockInfo { pos: (1, 0, 0), state: sign, nbt: Some(Tag::Compound(sign_nbt)) },
            ]],
            entities: Vec::new(),
        };
        let mut g = crate::worldgen::Gen::load(0, crate::name_to_state).unwrap();
        let mut ctx = FC::new(&mut g);
        ctx.test_no_terrain = true;
        ctx.test_default_air = true;
        let mut rng = Xor::seed_worldgen(17);
        template
            .place_blocks(
                &mut ctx,
                &mut rng,
                Placement {
                    origin: (8, 72, 8), pivot: (0, 0, 0),
                    mirror: Mirror::LeftRight, rotation: Rotation::Clockwise90,
                },
                &[],
                [0, 0, 0, 31, 255, 31],
            )
            .unwrap();
        // Mirror LeftRight first flips z: pos (1,0,0) stays, then CW90 maps
        // (x,y,z) -> (pivot.x + pivot.z - z, y, pivot.z - pivot.x + x); with
        // pivot (0,0,0): (x,y,z) -> (-z, y, x).
        assert_eq!(
            reg.get_prop(ctx.get(8, 72, 8), "facing").as_deref(),
            Some("west"),
            "chest[facing=north]: mirror flips north->south, CW90 turns it to west"
        );
        assert_eq!(
            reg.get_prop(ctx.get(8, 72, 9), "rotation").as_deref(),
            Some("8"),
            "the pinned state-transform table maps sign rotation 4 under LEFT_RIGHT+CW90 to 8"
        );
        assert_eq!(
            ctx.payloads[0].0,
            (8, 72, 8),
            "the chest payload is staged at the transformed position"
        );
        assert_eq!(
            ctx.payloads[0].1,
            GenerationPayload::LootContainer {
                kind: crate::generation_payload::ContainerKind::Chest,
                loot_table: "minecraft:chests/simple_dungeon".to_owned(),
                loot_seed: Some(99),
            },
            "LootTable + LootTableSeed survive the transform"
        );
        assert!(
            matches!(ctx.payloads[1].1, GenerationPayload::Sign(_)),
            "the sign's front_text stages under the transformed state"
        );

        // The jukebox is directional-free but its NBT has no store: fail closed.
        let jukebox = Template {
            size: (1, 1, 1),
            palettes: vec![vec![BlockInfo {
                pos: (0, 0, 0),
                state: reg.state_of("minecraft:jukebox", None),
                nbt: Some(Tag::Compound(BTreeMap::from([(
                    NbtString::text("RecordItem"),
                    Tag::Compound(BTreeMap::new()),
                )]))),
            }]],
            entities: Vec::new(),
        };
        let mut g = crate::worldgen::Gen::load(0, crate::name_to_state).unwrap();
        let mut ctx = FC::new(&mut g);
        ctx.test_no_terrain = true;
        ctx.test_default_air = true;
        let mut rng = Xor::seed_worldgen(17);
        assert!(
            jukebox
                .place_blocks(
                    &mut ctx,
                    &mut rng,
                    Placement {
                        origin: (8, 72, 8), pivot: (0, 0, 0),
                        mirror: Mirror::None, rotation: Rotation::None,
                    },
                    &[],
                    [0, 0, 0, 31, 255, 31],
                )
                .is_err(),
            "a jukebox record has no store and must refuse, not drop"
        );
    }

    /// `fluid_name` distinguishes source and flowing states: a written source
    /// water cell reserves `(pos, 0)` and a written source lava cell `(pos, 2)`,
    /// while an outside flowing-water neighbour reserves nothing.
    #[test]
    fn place_blocks_reserves_source_fluids_and_ignores_flowing_neighbours() {
        use crate::generation_context::FC;
        use crate::generation_math::Xor;
        use crate::structure_transform::{Mirror, Rotation};
        let reg = crate::block_registry::breg().unwrap();
        let water = reg.state_of("minecraft:water", None);
        let lava = reg.state_of("minecraft:lava", None);
        let flowing = reg.with_prop(water, "level", "5");
        let template = Template {
            size: (2, 1, 1),
            palettes: vec![vec![
                BlockInfo { pos: (0, 0, 0), state: water, nbt: None },
                BlockInfo { pos: (1, 0, 0), state: lava, nbt: None },
            ]],
            entities: Vec::new(),
        };
        let mut g = crate::worldgen::Gen::load(0, crate::name_to_state).unwrap();
        let mut ctx = FC::new(&mut g);
        ctx.test_no_terrain = true;
        ctx.test_default_air = true;
        ctx.test_base_cell((8, 73, 8), flowing);
        let mut rng = Xor::seed_worldgen(19);
        template
            .place_blocks(
                &mut ctx,
                &mut rng,
                Placement {
                    origin: (8, 72, 8), pivot: (0, 0, 0),
                    mirror: Mirror::None, rotation: Rotation::None,
                },
                &[],
                [0, 0, 0, 31, 255, 31],
            )
            .unwrap();
        let ticks = ctx.g.generated_fluid_ticks();
        assert!(
            ticks.contains(&((8, 72, 8), 0)) && ticks.contains(&((9, 72, 8), 2)),
            "source water and source lava reserve their own ticks: {ticks:?}"
        );
        assert!(
            !ticks.iter().any(|&(p, _)| p == (8, 73, 8)),
            "an outside flowing-water neighbour reserves nothing"
        );
    }
}

include!("template_structures.rs");

include!("end_city_mansion.rs");
