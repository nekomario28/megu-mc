// block_registry.rs — block-state ids, properties, tags and the per-state
// values (hardness, drops, light, behaviour) read from assets/blocks_1211.json
// and assets/pblocks.json. Terrain and feature generation read it; it knows
// nothing about terrain, placement or features.

use crate::json::{jarr, jnum, jparse, js, jstr, J};
use std::collections::{HashMap, HashSet};

// ---------- block-state JSON helpers ----------

pub(crate) fn ns_name(s: &str) -> String {
    if s.contains(':') {
        s.to_string()
    } else {
        format!("minecraft:{}", s)
    }
}
// blockstate json (string or {id,properties}) -> (name, props)
pub(crate) fn bs_parts(j: &J) -> Option<(String, Option<&J>)> {
    match j {
        J::Str(s) => Some((ns_name(s), None)),
        // 1.21.1 writes a serialised block state as {"Name": ..., "Properties": ...};
        // 1.21.2+ (and this repository's copy of the data) as {"id": ...,
        // "properties": ...}. Both appear in the bundled data, so read either.
        J::Obj(_) => Some((
            ns_name(&jstr(j, "id").or_else(|| jstr(j, "Name"))?),
            js(j, "properties").or_else(|| js(j, "Properties")),
        )),
        _ => None,
    }
}

// ---------- block registry ----------

struct Prop {
    name: String,
    vals: Vec<String>,
}

fn metadata_properties(block: &J, definitions: &HashMap<i64, &J>) -> Option<Vec<Prop>> {
    jarr(block, "properties")?
        .iter()
        .map(|key| {
            let definition = definitions.get(&(key.num() as i64))?;
            let vals = match jstr(definition, "type")?.as_str() {
                "boolean" => vec!["true".into(), "false".into()],
                "enum" => jarr(definition, "values")?
                    .iter()
                    .map(|v| v.str().to_string())
                    .collect(),
                "int" => (jnum(definition, "min")? as i32..=jnum(definition, "max")? as i32)
                    .map(|v| v.to_string())
                    .collect(),
                _ => return None,
            };
            Some(Prop {
                name: jstr(definition, "serialized_name")?,
                vals,
            })
        })
        .collect()
}

/// State arrays are mixed-radix products, but each asset may order or extend the
/// property values differently. Translate names and values, never numeric state ids.
fn metadata_offset(wire: &[Prop], offset: usize, metadata: &[Prop]) -> Option<usize> {
    if wire.len() != metadata.len() {
        return None;
    }
    let mut out = 0usize;
    for prop in metadata {
        let index = wire.iter().position(|p| p.name == prop.name)?;
        let stride: usize = wire[index + 1..].iter().map(|p| p.vals.len()).product();
        if stride == 0 || wire[index].vals.is_empty() {
            return None;
        }
        let value = &wire[index].vals[(offset / stride) % wire[index].vals.len()];
        let digit = prop.vals.iter().position(|v| v == value)?;
        out = out.checked_mul(prop.vals.len())?.checked_add(digit)?;
    }
    Some(out)
}

#[cfg(test)]
mod state_mapping_tests {
    use super::*;
    #[test]
    fn property_names_and_values_determine_metadata_position() {
        let prop = |name: &str, vals: &[&str]| Prop {
            name: name.into(),
            vals: vals.iter().map(|v| v.to_string()).collect(),
        };
        let wire = [prop("axis", &["x", "y"]), prop("wet", &["true", "false"])];
        let metadata = [
            prop("wet", &["false", "true"]),
            prop("axis", &["z", "y", "x"]),
        ];
        for (offset, expected) in [5, 2, 4, 1].into_iter().enumerate() {
            assert_eq!(metadata_offset(&wire, offset, &metadata), Some(expected));
        }
    }
}

/// What a block does when a random tick lands on it. Zero is the common case and the
/// registry stores it for every block, so the dispatch is a compare against zero.
pub(crate) const B_NONE: u8 = 0;
pub(crate) const B_SUGAR_CANE: u8 = 1;
/// Blocks affected by gravity; scheduling and physics belong to the falling loop.
pub(crate) const B_FALLING: u8 = 2;

/// Native Heightmap.Types: WG variants share predicates, not update lifetimes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum HeightmapKind {
    WorldSurfaceWg,
    WorldSurface,
    OceanFloorWg,
    OceanFloor,
    MotionBlocking,
    MotionBlockingNoLeaves,
}
impl HeightmapKind {
    pub(crate) fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "WORLD_SURFACE_WG" => Self::WorldSurfaceWg,
            "WORLD_SURFACE" => Self::WorldSurface,
            "OCEAN_FLOOR_WG" => Self::OceanFloorWg,
            "OCEAN_FLOOR" => Self::OceanFloor,
            "MOTION_BLOCKING" => Self::MotionBlocking,
            "MOTION_BLOCKING_NO_LEAVES" => Self::MotionBlockingNoLeaves,
            _ => return None,
        })
    }
    pub(crate) fn is_wg(self) -> bool {
        matches!(self, Self::WorldSurfaceWg | Self::OceanFloorWg)
    }
    fn mask(self) -> u8 {
        match self {
            Self::WorldSurfaceWg | Self::WorldSurface => 2,
            Self::OceanFloorWg | Self::OceanFloor => 4,
            Self::MotionBlocking => 8,
            Self::MotionBlockingNoLeaves => 16,
        }
    }
}

pub(crate) struct BReg {
    bname: Vec<String>,
    idx: HashMap<String, u32>,
    props: Vec<Box<[Prop]>>,
    dfl: Vec<u32>,
    st_lo: Vec<u32>,
    n_states: Vec<u32>,
    st_blk: Vec<u32>,      // state id -> block idx
    st_fl: Vec<u16>,       // pumpkin state_flags
    st_side: Vec<u8>,      // pumpkin side_flags
    hardness: Vec<f32>,    // state id -> vanilla destroy time (-1 unbreakable)
    htools: Vec<Vec<i64>>, // block idx -> tool item ids that can harvest it (empty = any)
    tool_kind: Vec<i8>,    // block idx -> tool kind mined at full speed (-1 = none)
    /// Target-version states that could not obtain complete metadata by property value.
    /// The registry gate requires this to be empty, not merely the block names to match.
    pub(crate) unpaired: Vec<String>,
    drops: Vec<i64>, // state id -> item dropped with no enchantment (-1 = nothing)
    /// A few blocks drop twice: the potted plants drop the pot and the plant. Kept beside
    /// `drops` rather than folded into it so the single-drop path stays as it was.
    drops_second: Vec<i64>,
    stack_size: Vec<i64>, // state id -> max stack of the item, from the asset
    behaviour: Vec<u8>,   // block idx -> random-tick behaviour id
    luminance: Vec<u8>,   // state id -> light the state emits (0 = none)
    opacity: Vec<u8>,     // state id -> how much light it stops
    tag: HashMap<String, HashSet<u32>>, // tag name -> block idx set (expanded)
    /// tag name -> block idx list in the pack's own `values` order, nested tags expanded in
    /// place. `getRandomElement(tag, random)` indexes into that order, so a set would return
    /// a different member for the same draw.
    tag_order: HashMap<String, Vec<u32>>,
}

const FL_AIR: u16 = 1;
const FL_REPLACEABLE: u16 = 1 << 4;
const FL_LIQUID: u16 = 1 << 5;
const FL_SOLID: u16 = 1 << 6;

impl BReg {
    fn load() -> Option<BReg> {
        let md = jparse(&std::fs::read("assets/blocks_1211.json").ok()?)?
            .as_arr()?
            .clone();
        let pb = jparse(&std::fs::read("assets/pblocks.json").ok()?)?;
        let pb_blocks = pb.get("blocks")?.as_arr()?;
        let property_data = jparse(&std::fs::read("assets/properties.json").ok()?)?;
        let mut property_definitions = HashMap::new();
        for definition in property_data.as_arr()? {
            property_definitions.insert(jnum(definition, "hash_key")? as i64, definition);
        }
        let mut pb_by_name: HashMap<String, &J> = HashMap::new();
        for b in pb_blocks {
            pb_by_name.insert(format!("minecraft:{}", jstr(b, "name")?), b);
        }
        // The two assets are different Minecraft versions, so a block renamed between them has no
        // entry under the 1.21.1 name and silently loses every per-state value - solidity, opacity,
        // luminance, shapes. 1.21.1's chain became iron_chain when chains gained material variants.
        // This table is the only place that mapping lives, and `unpaired` below makes a new one
        // visible instead of quiet.
        const PB_ALIAS: [(&str, &str); 1] = [("minecraft:chain", "minecraft:iron_chain")];
        for (old, new) in PB_ALIAS {
            if let Some(b) = pb_by_name.get(new).copied() {
                pb_by_name.insert(old.to_string(), b);
            }
        }
        let state_count = md
            .iter()
            .filter_map(|b| jnum(b, "maxStateId"))
            .fold(0.0, f64::max) as usize
            + 1;
        let mut reg = BReg {
            bname: vec![],
            idx: HashMap::new(),
            props: vec![],
            dfl: vec![],
            st_lo: vec![],
            n_states: vec![],
            st_blk: vec![0; state_count],
            st_fl: vec![0; state_count],
            st_side: vec![0; state_count],
            hardness: vec![0.6; state_count],
            htools: vec![],
            tool_kind: vec![],
            unpaired: Vec::new(),
            drops: vec![-1; state_count],
            drops_second: vec![-1; state_count],
            stack_size: vec![64; state_count],
            behaviour: vec![],
            luminance: vec![0; state_count],
            opacity: vec![0; state_count],
            tag: HashMap::new(),
            tag_order: HashMap::new(),
        };
        for b in &md {
            let name = format!("minecraft:{}", jstr(b, "name")?);
            let bi = reg.bname.len() as u32;
            reg.idx.insert(name.clone(), bi);
            reg.bname.push(name.clone());
            let lo = jnum(b, "minStateId")? as u32;
            let hi = jnum(b, "maxStateId")? as u32;
            reg.st_lo.push(lo);
            reg.dfl
                .push(jnum(b, "defaultState").unwrap_or(lo as f64) as u32);
            reg.n_states.push(hi - lo + 1);
            let mut props = Vec::new();
            if let Some(ps) = jarr(b, "states") {
                for p in ps {
                    let pn = jstr(p, "name")?;
                    let ty = jstr(p, "type").unwrap_or_default();
                    let vals: Vec<String> = match jarr(p, "values") {
                        Some(vs) => vs.iter().map(|v| v.str().to_string()).collect(),
                        None if ty == "bool" => vec!["true".into(), "false".into()],
                        None => vec![],
                    };
                    props.push(Prop { name: pn, vals });
                }
            }
            reg.props.push(props.into_boxed_slice());
            let pbr = pb_by_name.get(&name);
            let metadata_props =
                pbr.and_then(|block| metadata_properties(block, &property_definitions));
            // Destroy time is a property of the block, so it is read here once and
            // held per state rather than re-derived by a hand table. A hand table
            // covered 356 of the 26684 states, which meant mining time was wrong for
            // almost every block in the world.
            let hardness = jnum(b, "hardness").unwrap_or(0.6) as f32;
            // Stack size is a block property in the asset (1 for tools and potions,
            // 16 for signs and buckets, 64 otherwise) and is read here once per state
            // rather than re-derived from item id ranges.
            let stack_size = jnum(b, "stackSize").unwrap_or(64.0) as i64;
            // What the block drops. The asset lists the default drop, with no
            // enchantment and the correct tool assumed, which is the question this
            // answers; silk touch and fortune are handled where the block is broken.
            // 34 blocks list two drops (the potted plants drop the pot and the plant). Both are
            // read: modelling only the first lost one item per broken pot.
            let (drop0, drop1) = match b.get("drops").and_then(|d| d.as_arr()) {
                Some(a) => (
                    a.first().map(|j| j.num() as i64).unwrap_or(-1),
                    a.get(1).map(|j| j.num() as i64).unwrap_or(-1),
                ),
                None => (-1, -1),
            };
            // Vanilla answers "can this tool harvest this block?" from the block's
            // harvestTools set: empty means any tool works, otherwise the held item
            // must be in it. Re-deriving that as (needs a pickaxe) AND (tier high
            // enough) got the multiplier wrong for every block that needs no tool.
            let ht: Vec<i64> = match b.get("harvestTools") {
                Some(J::Obj(entries)) => entries
                    .iter()
                    .filter_map(|(k, _)| k.parse::<i64>().ok())
                    .collect(),
                _ => Vec::new(),
            };
            // Which tool type mines this block at full speed. The asset states it as a
            // mineable/<kind> tag, or - for blocks gated by tier rather than by tool
            // type, like obsidian - only through the harvestTools set. A hand table of
            // bare id ranges answered this for 16% of the states.
            let from_material = match b.get("material") {
                Some(m) => m.str(),
                None => "",
            }
            .split(';')
            .find_map(|part| match part {
                "mineable/shovel" => Some(1i8),
                "mineable/pickaxe" => Some(2),
                "mineable/axe" => Some(3),
                "mineable/hoe" => Some(4),
                "mineable/sword" => Some(0),
                _ => None,
            });
            let from_tools = {
                let mut k: Option<i8> = None;
                let mut disagree = false;
                for t in &ht {
                    if (818..=847).contains(t) {
                        let kd = ((t - 818) % 5) as i8;
                        match k {
                            None => k = Some(kd),
                            Some(prev) if prev != kd => disagree = true,
                            _ => {}
                        }
                    }
                }
                if disagree {
                    None
                } else {
                    k
                }
            };
            // The asset has no behaviour field, so this is the one hand-written table in
            // the registry. It lives here, next to the data it accompanies, rather than at
            // the call site, so the dispatch is an integer compare and a behaviour is added
            // by one line instead of a new branch in the tick.
            reg.behaviour.push(match name.as_str() {
                "minecraft:sugar_cane" => B_SUGAR_CANE,
                "minecraft:sand" | "minecraft:red_sand" | "minecraft:gravel" => B_FALLING,
                n if n.ends_with("_concrete_powder") => B_FALLING,
                _ => B_NONE,
            });
            reg.tool_kind
                .push(from_material.or(from_tools).unwrap_or(-1));
            reg.htools.push(ht);
            let n = (hi - lo + 1) as usize;
            for i in 0..n {
                let st = (lo + i as u32) as usize;
                if st >= state_count {
                    break;
                }
                reg.st_blk[st] = bi;
                reg.hardness[st] = hardness;
                reg.drops[st] = drop0;
                reg.drops_second[st] = drop1;
                reg.stack_size[st] = stack_size;
                let values = metadata_props
                    .as_deref()
                    .and_then(|props| metadata_offset(&reg.props[bi as usize], i, props))
                    .and_then(|index| pbr.and_then(|block| jarr(block, "states")?.get(index)))
                    .and_then(|state| {
                        Some((
                            jnum(state, "state_flags")? as u16,
                            jnum(state, "side_flags")? as u8,
                            jnum(state, "luminance")? as u8,
                            jnum(state, "opacity")? as u8,
                        ))
                    });
                match values {
                    Some((flags, sides, light, opacity)) => {
                        reg.st_fl[st] = flags;
                        reg.st_side[st] = sides;
                        reg.luminance[st] = light;
                        reg.opacity[st] = opacity;
                    }
                    None => reg.unpaired.push(format!("{name} state {st}")),
                }
            }
        }
        // block tags: eagerly expand nested #tag refs
        let mut raw: HashMap<String, Vec<String>> = HashMap::new();
        for (name, bytes) in dp_tag_files("block") {
            if let Some(j) = jparse(&bytes) {
                if let Some(vals) = jarr(&j, "values") {
                    let mut members = Vec::new();
                    for v in vals {
                        match v {
                            J::Str(s) => members.push(ns_name(s)),
                            J::Obj(_) => {
                                if let Some(id) = jstr(v, "id") {
                                    members.push(ns_name(&id));
                                }
                            }
                            _ => {}
                        }
                    }
                    raw.insert(name, members);
                }
            }
        }
        fn expand(
            name: &str,
            raw: &HashMap<String, Vec<String>>,
            idx: &HashMap<String, u32>,
            done: &mut HashMap<String, HashSet<u32>>,
            stack: &mut HashSet<String>,
        ) -> HashSet<u32> {
            if let Some(s) = done.get(name) {
                return s.clone();
            }
            if stack.contains(name) {
                return HashSet::new();
            }
            stack.insert(name.to_string());
            let mut out = HashSet::new();
            if let Some(members) = raw.get(name) {
                for m in members {
                    if let Some(inner) = m.strip_prefix('#') {
                        for b in expand(&ns_name(inner), raw, idx, done, stack) {
                            out.insert(b);
                        }
                    } else if let Some(&bi) = idx.get(m) {
                        out.insert(bi);
                    }
                }
            }
            stack.remove(name);
            done.insert(name.to_string(), out.clone());
            out
        }
        fn expand_ordered(
            name: &str,
            raw: &HashMap<String, Vec<String>>,
            idx: &HashMap<String, u32>,
            done: &mut HashMap<String, Vec<u32>>,
            stack: &mut HashSet<String>,
        ) -> Vec<u32> {
            if let Some(v) = done.get(name) {
                return v.clone();
            }
            if stack.contains(name) {
                return Vec::new();
            }
            stack.insert(name.to_string());
            let mut out = Vec::new();
            if let Some(members) = raw.get(name) {
                for m in members {
                    if let Some(inner) = m.strip_prefix('#') {
                        out.extend(expand_ordered(&ns_name(inner), raw, idx, done, stack));
                    } else if let Some(&bi) = idx.get(m) {
                        out.push(bi);
                    }
                }
            }
            stack.remove(name);
            done.insert(name.to_string(), out.clone());
            out
        }
        let names: Vec<String> = raw.keys().cloned().collect();
        for n in names {
            let mut stack = HashSet::new();
            let set = expand(&n, &raw, &reg.idx, &mut reg.tag, &mut stack);
            reg.tag.insert(n.clone(), set);
            let mut stack = HashSet::new();
            let ordered = expand_ordered(&n, &raw, &reg.idx, &mut reg.tag_order, &mut stack);
            reg.tag_order.insert(n, ordered);
        }
        Some(reg)
    }
    /// resolve (name, props) to a 1.21.1 state id
    pub(crate) fn state_of(&self, name: &str, props: Option<&J>) -> u32 {
        let name = ns_name(name);
        let bi = match self.idx.get(&name) {
            Some(&b) => b,
            None => return 0,
        };
        let base = match props {
            Some(J::Obj(m)) => m,
            _ => return self.dfl[bi as usize],
        };
        let ps = &self.props[bi as usize];
        let mut off = 0u64;
        for (i, p) in ps.iter().enumerate() {
            let want = base
                .iter()
                .find(|(kk, _)| kk == &p.name)
                .map(|(_, v)| v.str().to_string());
            let mult: u64 = ps[i + 1..]
                .iter()
                .map(|q| q.vals.len().max(1) as u64)
                .product();
            if let Some(w) = want {
                match p.vals.iter().position(|v| *v == w) {
                    Some(d) => off += d as u64 * mult,
                    None => return self.dfl[bi as usize], // bad value -> default
                }
            }
        }
        let st = self.st_lo[bi as usize] + off as u32;
        if off >= self.n_states[bi as usize] as u64 {
            return self.dfl[bi as usize];
        }
        st
    }
    pub(crate) fn name_of(&self, st: u32) -> &str {
        self.st_blk
            .get(st as usize)
            .and_then(|&b| self.bname.get(b as usize))
            .map(|s| s.as_str())
            .unwrap_or("minecraft:air")
    }

    /// Structure palettes start from the block's default state. Invalid names
    /// or properties are errors for the template loader, not air substitutes.
    pub(crate) fn checked_state(&self, name: &str, properties: &[(String, String)]) -> Option<u32> {
        let bi = *self.idx.get(&ns_name(name))? as usize;
        let mut state = self.dfl[bi];
        for (key, value) in properties {
            let property = self.props[bi].iter().find(|p| p.name == *key)?;
            if !property.vals.contains(value) {
                return None;
            }
            state = self.with_prop(state, key, value);
        }
        Some(state)
    }

    pub(crate) fn properties_of(&self, state: u32) -> Option<Vec<(String, String)>> {
        let bi = *self.st_blk.get(state as usize)? as usize;
        self.props[bi]
            .iter()
            .map(|p| Some((p.name.clone(), self.get_prop(state, &p.name)?)))
            .collect()
    }

    pub(crate) fn is_template_solid(&self, state: u32) -> Option<bool> {
        include_bytes!("../assets/template_solid_1211.bin")
            .get(state as usize)
            .map(|v| *v != 0)
    }
    /// Number of states the registry knows. Callers that walk the state space need
    /// this rather than a literal, so the extent has one owner.
    #[cfg(test)]
    pub(crate) fn state_count(&self) -> usize {
        self.st_blk.len()
    }

    /// Vanilla `canHarvest`: whether this tool gets the block's drop, and whether
    /// mining uses the fast multiplier. A block with no harvestTools entry can be
    /// harvested by anything.
    pub(crate) fn can_harvest(&self, st: u32, item: i64) -> bool {
        let Some(&bi) = self.st_blk.get(st as usize) else {
            return true;
        };
        match self.htools.get(bi as usize) {
            Some(ht) if !ht.is_empty() => ht.contains(&item),
            _ => true,
        }
    }

    /// Light this state emits, 0..15.
    pub(crate) fn luminance_of(&self, st: u32) -> u8 {
        self.luminance.get(st as usize).copied().unwrap_or(0)
    }

    /// How much light this state stops. Zero is fully transparent.
    pub(crate) fn opacity_of(&self, st: u32) -> u8 {
        self.opacity.get(st as usize).copied().unwrap_or(0)
    }

    /// The random-tick behaviour of this block, or B_NONE.
    pub(crate) fn behaviour_of(&self, st: u32) -> u8 {
        let Some(&bi) = self.st_blk.get(st as usize) else {
            return B_NONE;
        };
        self.behaviour.get(bi as usize).copied().unwrap_or(B_NONE)
    }

    /// How many of this item fit in one slot, as the asset states it.
    pub(crate) fn stack_size_of(&self, st: u32) -> i64 {
        self.stack_size.get(st as usize).copied().unwrap_or(64)
    }

    /// The item this block drops when broken with no enchantment, or None when it
    /// drops nothing (glass, ice, tall grass) - which is a real answer, not a missing
    /// one, and the reason this cannot be inferred from the block's own item.
    pub(crate) fn block_drop(&self, st: u32) -> Option<i64> {
        match self.drops.get(st as usize).copied() {
            Some(d) if d >= 0 => Some(d),
            _ => None,
        }
    }

    /// The second item a block drops, when the asset lists one.
    pub(crate) fn block_drop_second(&self, st: u32) -> Option<i64> {
        match self.drops_second.get(st as usize).copied() {
            Some(d) if d >= 0 => Some(d),
            _ => None,
        }
    }

    /// Which tool type (0 sword, 1 shovel, 2 pickaxe, 3 axe, 4 hoe) mines this block
    /// at full speed, or -1 when no tool is faster than bare hands.
    pub(crate) fn tool_kind_of(&self, st: u32) -> i8 {
        let Some(&bi) = self.st_blk.get(st as usize) else {
            return -1;
        };
        self.tool_kind.get(bi as usize).copied().unwrap_or(-1)
    }

    /// Vanilla destroy time in seconds. Negative means unbreakable.
    pub(crate) fn hardness_of(&self, st: u32) -> f32 {
        self.hardness.get(st as usize).copied().unwrap_or(0.6)
    }

    pub(crate) fn is_air(&self, st: u32) -> bool {
        st == 0
            || self
                .st_fl
                .get(st as usize)
                .map(|f| f & FL_AIR != 0)
                .unwrap_or(true)
    }
    pub(crate) fn is_solid(&self, st: u32) -> bool {
        self.st_fl
            .get(st as usize)
            .map(|f| f & FL_SOLID != 0)
            .unwrap_or(false)
    }
    /// Pinned BlockState.isSolidRender, not isSolid or light opacity: e.g.
    /// tinted glass blocks light but does not orient a dungeon chest.
    /// Generated by tools/block_predicate_oracle.py with names/property translation.
    fn native_predicates(&self, st: u32) -> u8 {
        include_bytes!("../assets/block_predicates_1211.bin")
            .get(st as usize)
            .copied()
            .unwrap_or(0)
    }
    pub(crate) fn is_solid_render(&self, st: u32) -> bool {
        self.native_predicates(st) & 1 != 0
    }
    /// Pinned `BlockStateBase.isCollisionShapeFullBlock`; distinct from render, heightmap,
    /// and sturdy-face predicates. Kept separately because each byte of native_predicates
    /// already has a documented consumer.
    pub(crate) fn is_collision_shape_full_block(&self, st: u32) -> bool {
        include_bytes!("../assets/collision_shape_full_1211.bin")
            .get(st as usize)
            .is_some_and(|full| *full != 0)
    }
    pub(crate) fn heightmap_counts(&self, st: u32, kind: HeightmapKind) -> bool {
        self.native_predicates(st) & kind.mask() != 0
    }
    pub(crate) fn is_double_plant(&self, st: u32) -> bool {
        self.native_predicates(st) & 32 != 0
    }
    pub(crate) fn has_water(&self, st: u32) -> bool {
        self.native_predicates(st) & 64 != 0
    }
    /// Native MultifaceBlock.canAttachTo on this state, seen in the attachment
    /// direction from the growth cell (the tested support face is opposite).
    pub(crate) fn supports_multiface(&self, st: u32, direction: u8) -> bool {
        include_bytes!("../assets/multiface_support_1211.bin")
            .get(st as usize)
            .is_some_and(|faces| faces & (1 << direction) != 0)
    }
    /// The actual FluidState type, not the block name (waterlogged plants and
    /// solids contain source water; liquid block levels may contain flowing fluid).
    pub(crate) fn fluid_name(&self, st: u32) -> &'static str {
        let source = self.native_predicates(st) & 128 != 0;
        if self.has_water(st) {
            if source {
                "minecraft:water"
            } else {
                "minecraft:flowing_water"
            }
        } else if self.name_of(st) == "minecraft:lava" {
            if source {
                "minecraft:lava"
            } else {
                "minecraft:flowing_lava"
            }
        } else {
            "minecraft:empty"
        }
    }
    pub(crate) fn is_liquid(&self, st: u32) -> bool {
        self.st_fl
            .get(st as usize)
            .map(|f| f & FL_LIQUID != 0)
            .unwrap_or(false)
    }
    pub(crate) fn is_replaceable(&self, st: u32) -> bool {
        self.is_air(st)
            || self
                .st_fl
                .get(st as usize)
                .map(|f| f & FL_REPLACEABLE != 0)
                .unwrap_or(false)
    }
    pub(crate) fn side_solid(&self, st: u32, dir: usize) -> bool {
        self.st_side
            .get(st as usize)
            .map(|f| f & (1 << dir) != 0)
            .unwrap_or(false)
    }
    /// Bounded shape update when an exposed template face supplies a newly
    /// placed ordinary full cube to an existing horizontal fence. The direction
    /// points FROM the fence TOWARDS the new neighbour, using the native
    /// DOWN/UP/NORTH/SOUTH/WEST/EAST indices.
    ///
    /// This is deliberately not a general BlockState.updateShape implementation.
    /// Fences also connect to eligible gates/fences and some non-full faces,
    /// and other block families have different rules. The solid-template path
    /// only adds full-cube neighbours, so it can prove this positive transition
    /// without fabricating negative/replacement transitions or runtime behavior.
    pub(crate) fn fence_after_full_neighbor(
        &self,
        state: u32,
        neighbor: u32,
        toward_neighbor: u8,
    ) -> Option<u32> {
        if !self.name_of(state).ends_with("_fence") {
            return None;
        }
        let key = match toward_neighbor {
            2 => "north",
            3 => "south",
            4 => "west",
            5 => "east",
            _ => return None,
        };
        // A fully colliding cube is not necessarily a valid fence support:
        // non-occluding blocks and vanilla's connection exceptions are excluded.
        if self.is_template_solid(neighbor) != Some(true)
            || !self.is_collision_shape_full_block(neighbor)
            || !self.is_solid_render(neighbor)
            || matches!(
                self.name_of(neighbor),
                "minecraft:barrier"
                    | "minecraft:carved_pumpkin"
                    | "minecraft:jack_o_lantern"
                    | "minecraft:melon"
                    | "minecraft:pumpkin"
            )
        {
            return None;
        }
        (self.get_prop(state, key).as_deref() == Some("false"))
            .then(|| self.with_prop(state, key, "true"))
    }

    /// set a property by name on a resolved state; missing -> unchanged
    pub(crate) fn with_prop(&self, st: u32, prop: &str, val: &str) -> u32 {
        let bi = match self.st_blk.get(st as usize) {
            Some(&b) => b,
            None => return st,
        };
        let ps = &self.props[bi as usize];
        let off = st.saturating_sub(self.st_lo[bi as usize]);
        for (i, p) in ps.iter().enumerate() {
            if p.name != prop {
                continue;
            }
            let mult: u64 = ps[i + 1..]
                .iter()
                .map(|q| q.vals.len().max(1) as u64)
                .product();
            let len = p.vals.len().max(1) as u64;
            let digit = (off as u64).checked_div(mult).map_or(0, |q| q % len);
            if let Some(nd) = p.vals.iter().position(|v| v == val) {
                // Move this property's digit to `nd` by the difference, NOT by
                // (nd + len - digit) % len. The offset already carries digit*mult, so
                // the wrapping form adds a positive delta where the answer needed a
                // negative one: setting age 15 back to 0 landed one state past the end
                // of the block and produced a jukebox.
                let o = off as i64 + (nd as i64 - digit as i64) * mult as i64;
                if o < 0 {
                    return st;
                }
                return self.st_lo[bi as usize] + o as u32;
            }
            return st;
        }
        st
    }
    /// LeavesBlock.getOptionalDistance: LOGS are zero; otherwise require
    /// the integer `distance` property with domain 1..=7, not scaffolding's 0..=7.
    pub(crate) fn distance_from_log(&self, st: u32) -> Option<u8> {
        if self.in_tag(st, "minecraft:logs") {
            return Some(0);
        }
        let bi = *self.st_blk.get(st as usize)? as usize;
        let p = self.props[bi].iter().find(|p| p.name == "distance")?;
        if p.vals.len() != 7
            || !p
                .vals
                .iter()
                .zip(1..=7)
                .all(|(v, d)| v.parse::<u8>().ok() == Some(d))
        {
            return None;
        }
        self.get_prop(st, "distance")?.parse().ok()
    }

    pub(crate) fn get_prop(&self, st: u32, prop: &str) -> Option<String> {
        let bi = *self.st_blk.get(st as usize)?;
        let ps = &self.props[bi as usize];
        let off = st.saturating_sub(self.st_lo[bi as usize]);
        for (i, p) in ps.iter().enumerate() {
            if p.name != prop {
                continue;
            }
            let mult: u64 = ps[i + 1..]
                .iter()
                .map(|q| q.vals.len().max(1) as u64)
                .product();
            let len = p.vals.len().max(1) as u64;
            let d = (off as u64).checked_div(mult).map_or(0, |q| q % len);
            return p.vals.get(d as usize).cloned();
        }
        None
    }
    pub(crate) fn has_prop(&self, name: &str, prop: &str) -> bool {
        self.idx
            .get(&ns_name(name))
            .map(|&bi| self.props[bi as usize].iter().any(|p| p.name == prop))
            .unwrap_or(false)
    }
    /// Ordered members of a block tag, as `getRandomElement(tag, random)` sees them.
    pub(crate) fn tag_members(&self, tag: &str) -> &[u32] {
        self.tag_order
            .get(&ns_name(tag))
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// A block index (a tag member) as its default state id.
    pub(crate) fn default_state(&self, block_idx: u32) -> u32 {
        self.dfl.get(block_idx as usize).copied().unwrap_or(0)
    }

    pub(crate) fn in_tag(&self, st: u32, tag: &str) -> bool {
        let bi = match self.st_blk.get(st as usize) {
            Some(&b) => b,
            None => return false,
        };
        // Look the tag up by the caller's own `&str` first: this runs once per candidate
        // cell in the feature loops, and `ns_name` would allocate a `String` each time.
        if let Some(set) = self.tag.get(tag) {
            return set.contains(&bi);
        }
        // Unqualified names ("dirt") still resolve, through the one allocation they need.
        self.tag
            .get(&ns_name(tag))
            .map(|s| s.contains(&bi))
            .unwrap_or(false)
    }
}

fn dp_tag_files(sub: &str) -> HashMap<String, Vec<u8>> {
    let mut m = HashMap::new();
    let base = std::path::Path::new("datapacks");
    if let Ok(packs) = std::fs::read_dir(base) {
        for p in packs.flatten() {
            let root = p.path().join("data");
            if let Ok(nss) = std::fs::read_dir(&root) {
                for ns in nss.flatten() {
                    let dir = ns.path().join(format!("tags/{}", sub));
                    let mut stack = vec![dir.clone()];
                    while let Some(d) = stack.pop() {
                        if let Ok(rd) = std::fs::read_dir(&d) {
                            for e in rd.flatten() {
                                let p = e.path();
                                if p.is_dir() {
                                    stack.push(p);
                                } else if p.extension().map(|x| x == "json").unwrap_or(false) {
                                    if let Ok(rel) = p.strip_prefix(&dir) {
                                        let key = format!(
                                            "{}:{}",
                                            ns.file_name().to_string_lossy(),
                                            rel.with_extension("").to_string_lossy()
                                        );
                                        if let Ok(b) = std::fs::read(&p) {
                                            m.entry(key).or_insert(b);
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

pub(crate) fn reg_props_axis(name: &str) -> Option<()> {
    breg().and_then(|r| r.has_prop(name, "axis").then_some(()))
}

static BREG: std::sync::OnceLock<Option<BReg>> = std::sync::OnceLock::new();
pub(crate) fn breg() -> Option<&'static BReg> {
    BREG.get_or_init(BReg::load).as_ref()
}

pub(crate) fn mkprops(pairs: &[(&str, &str)]) -> J {
    J::Obj(
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), J::Str(v.to_string())))
            .collect(),
    )
}

#[cfg(test)]
mod tag_tests {
    /// `ore_*` features target `#minecraft:base_stone_overworld`; if the tag lookup
    /// also matched `gravel`/`dirt`, a later ore would overwrite the gravel and dirt
    /// an earlier one placed (measured: this port's gravel set was a strict subset of
    /// vanilla's, 121 cells missing, while its stone-variant sets were supersets).
    #[test]
    fn base_stone_overworld_excludes_gravel_and_dirt() {
        let reg = super::breg().expect("registry");
        for name in [
            "stone",
            "granite",
            "diorite",
            "andesite",
            "tuff",
            "deepslate",
        ] {
            let st = reg.state_of(name, None);
            assert!(
                reg.in_tag(st, "minecraft:base_stone_overworld"),
                "{name} should match"
            );
        }
        for name in [
            "gravel",
            "dirt",
            "grass_block",
            "sand",
            "clay",
            "andesite_slab",
        ] {
            let st = reg.state_of(name, None);
            assert!(
                !reg.in_tag(st, "minecraft:base_stone_overworld"),
                "{name} must not match base_stone_overworld"
            );
        }
    }
}
