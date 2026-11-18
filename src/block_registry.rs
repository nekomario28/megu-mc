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
    shapes: Vec<([f64; 3], [f64; 3])>,
    col_range: Vec<(u32, u8)>, // state id -> (start in col_idx, len)
    col_idx: Vec<u16>,
    tag: HashMap<String, HashSet<u32>>, // tag name -> block idx set (expanded)
    /// tag name -> block idx list in the pack's own `values` order, nested tags expanded in
    /// place. `getRandomElement(tag, random)` indexes into that order, so a set would return
    /// a different member for the same draw.
    tag_order: HashMap<String, Vec<u32>>,
}

/// Pumpkin's replaceable flag. The physical predicates that used to live in the same
/// `state_flags` word (`FL_AIR`, `FL_LIQUID`, `FL_SOLID`) are read from the pinned table below
/// instead, which is their only owner now.
const FL_REPLACEABLE: u16 = 1 << 4;

/// The pinned per-state physical facts: six bytes per state id, generated by
/// `tools/block_state_facts_from_jar.py` from the 1.21.1 server jar.
///
/// This table is the only owner of these predicates. `assets/block_predicates_1211.bin`'s
/// solid-render bit, `assets/collision_shape_full_1211.bin` and the face bits of
/// `assets/mineshaft_support_1211.bin` are the same native calls recorded by other oracles;
/// `generation_contract_tests::pinned_fact_tables_agree_with_the_older_oracle_assets` reads them
/// as a cross-check on the state-id alignment rather than as a second definition.
///
/// Byte 5 is not a physical fact but `BlockStateBase.hasPostProcess(BlockGetter, BlockPos)`, the
/// `WorldGenRegion.setBlock` gate for `markPosForPostprocessing`; it lives here because it is a
/// cached `StatePredicate` on the state, and the oracle derives the three names from
/// `Blocks.<clinit>` instead of a Rust constant.
const STATE_FACTS: &[u8] = include_bytes!("../assets/block_state_facts_1211.bin");
const STATE_FACT_BYTES: usize = 6;
const FACT_IS_AIR: u8 = 1;
const FACT_LIQUID: u8 = 1 << 1;
const FACT_BLOCKS_MOTION: u8 = 1 << 2;
const FACT_CAN_OCCLUDE: u8 = 1 << 3;
const FACT_SOLID_RENDER: u8 = 1 << 4;
const FACT_COLLISION_EMPTY: u8 = 1 << 5;
const FACT_COLLISION_FULL: u8 = 1 << 6;
const FACT_FLUID_EMPTY: u8 = 1 << 7;
/// Byte 5: `BlockStateBase.hasPostProcess` (`dtb$a.q`). A whole byte per state, not a bit, so a
/// future column cannot silently alias it.
const FACT_POSTPROCESS: u8 = 1;

/// The pinned `SupportType`: which part of a face has to be covered for it to count as
/// sturdy. `Full` is vanilla's three-argument `isFaceSturdy`; `Center` is what
/// `canSupportCenter` (torch, plant and sign support) asks for, and it accepts a fence post or
/// a piston where `Full` does not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Support {
    Full,
    Center,
    // Nothing in generation asks for RIGID yet - it is the plane a boat or a shulker box asks -
    // but the table records all three so a future caller reads a fact instead of a heuristic.
    #[allow(dead_code)]
    Rigid,
}

/// The pinned bytes for `st`. An id outside the table answers every predicate false and
/// every level zero - "nothing here" - rather than panicking mid-generation.
fn state_fact(st: u32) -> [u8; STATE_FACT_BYTES] {
    let at = st as usize * STATE_FACT_BYTES;
    match STATE_FACTS.get(at..at + STATE_FACT_BYTES) {
        Some(bytes) => [bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5]],
        None => [0; STATE_FACT_BYTES],
    }
}

impl BReg {
    fn load() -> Option<BReg> {
        let md = jparse(&std::fs::read("assets/blocks_1211.json").ok()?)?
            .as_arr()?
            .clone();
        let pb = jparse(&std::fs::read("assets/pblocks.json").ok()?)?;
        let pb_blocks = pb.get("blocks")?.as_arr()?;
        let mut shapes = Vec::new();
        if let Some(pb_shapes) = pb.get("shapes").and_then(J::as_arr) {
            shapes.reserve(pb_shapes.len());
            for sh in pb_shapes {
                let min_a = sh.get("min").and_then(J::as_arr)?;
                let max_a = sh.get("max").and_then(J::as_arr)?;
                if min_a.len() != 3 || max_a.len() != 3 {
                    return None;
                }
                shapes.push((
                    [min_a[0].num(), min_a[1].num(), min_a[2].num()],
                    [max_a[0].num(), max_a[1].num(), max_a[2].num()],
                ));
            }
        }
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
            hardness: vec![0.6; state_count],
            htools: vec![],
            tool_kind: vec![],
            unpaired: Vec::new(),
            drops: vec![-1; state_count],
            drops_second: vec![-1; state_count],
            stack_size: vec![64; state_count],
            behaviour: vec![],
            shapes,
            col_range: vec![(0, 0); state_count],
            col_idx: Vec::new(),
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
                        let start = reg.col_idx.len() as u32;
                        let mut len = 0u8;
                        if let Some(cols) = jarr(state, "collision_shapes") {
                            for sh in cols {
                                let idx = sh.num() as usize;
                                if idx < reg.shapes.len() && len < u8::MAX {
                                    reg.col_idx.push(idx as u16);
                                    len += 1;
                                }
                            }
                        }
                        reg.col_range[st] = (start, len);
                        Some(jnum(state, "state_flags")? as u16)
                    });
                match values {
                    Some(flags) => reg.st_fl[st] = flags,
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
    /// The block registry's raw id (`Registries.BLOCK`), which is what the wire
    /// carries where a packet names a *block* rather than a state - for example
    /// `ClientboundBlockEventPacket`'s trailing VarInt. It is not a state id.
    pub(crate) fn block_id_of(&self, st: u32) -> Option<i32> {
        self.st_blk.get(st as usize).map(|block| *block as i32)
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

    /// Light this state emits, 0..15, from the pinned `getLightEmission()`.
    pub(crate) fn luminance_of(&self, st: u32) -> u8 {
        state_fact(st)[1] >> 4
    }

    /// How much light this state stops, 0..15, from the pinned `getLightBlock(level, pos)`.
    /// Zero is fully transparent.
    pub(crate) fn opacity_of(&self, st: u32) -> u8 {
        state_fact(st)[1] & 0xf
    }

    /// `BlockStateBase.hasPostProcess(BlockGetter, BlockPos)` (`dtb$a.q`): the predicate
    /// `WorldGenRegion.setBlock` reads to decide whether the written cell is filed with
    /// `ChunkAccess.markPosForPostprocessing`. Constant true for exactly three blocks in 1.21.1
    /// (`brown_mushroom`, `red_mushroom`, `magma_block`) and false for every other state. The
    /// table column behind this is re-derived from `Blocks.<clinit>` by
    /// `tools/block_state_facts_from_jar.py`, so no block name is typed in here.
    pub(crate) fn has_post_process(&self, st: u32) -> bool {
        state_fact(st)[5] & FACT_POSTPROCESS != 0
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

    /// Pinned `isAir()`. Three states answer true (air, cave air, void air); the old
    /// `state_flags` bit happened to agree on all 26,684, and the table removes the guess.
    pub(crate) fn is_air(&self, st: u32) -> bool {
        state_fact(st)[0] & FACT_IS_AIR != 0
    }
    /// Pinned `blocksMotion()`: whether the state's collision shape stops an entity. Named for
    /// the native method; `is_solid` remains the name the generation callers use.
    pub(crate) fn blocks_motion(&self, st: u32) -> bool {
        state_fact(st)[0] & FACT_BLOCKS_MOTION != 0
    }
    /// The name the generation callers use for `blocksMotion`, kept because forty call sites
    /// read better as "is this solid"; the value is the native predicate, not a second one.
    pub(crate) fn is_solid(&self, st: u32) -> bool {
        self.blocks_motion(st)
    }
    /// Pinned `canOcclude()`: the state's occlusion shape is a full cube. A different question
    /// from `isSolidRender`, and the looser of the two: every solid render occludes, while 14,747
    /// states (pistons, walls, dirt paths) occlude as a full cube without being one. Exported for
    /// the audit; no generation caller asks it yet, which is why it is allowed to be idle rather
    /// than plumbed through a call site that does not want it.
    #[allow(dead_code)]
    pub(crate) fn can_occlude(&self, st: u32) -> bool {
        state_fact(st)[0] & FACT_CAN_OCCLUDE != 0
    }
    pub(crate) fn collision_boxes(
        &self,
        st: u32,
    ) -> impl Iterator<Item = ([f64; 3], [f64; 3])> + '_ {
        let (start, len) = self.col_range.get(st as usize).copied().unwrap_or((0, 0));
        let slice = &self.col_idx[start as usize..start as usize + len as usize];
        slice.iter().map(|&idx| self.shapes[idx as usize])
    }
    /// Pinned BlockState.isSolidRender, not isSolid or light opacity: e.g.
    /// tinted glass blocks light but does not orient a dungeon chest.
    pub(crate) fn is_solid_render(&self, st: u32) -> bool {
        state_fact(st)[0] & FACT_SOLID_RENDER != 0
    }
    /// Pinned `getCollisionShape(level, pos).isEmpty()`. The registry's `collision_boxes` list
    /// agrees with it on 26,683 of the 26,684 states; the exception is a pitcher crop's upper
    /// half at age 0, where the other asset lists no box and vanilla does collide.
    pub(crate) fn is_collision_empty(&self, st: u32) -> bool {
        state_fact(st)[0] & FACT_COLLISION_EMPTY != 0
    }
    /// Pinned `BlockStateBase.isCollisionShapeFullBlock`; distinct from render, heightmap,
    /// and sturdy-face predicates.
    pub(crate) fn is_collision_shape_full_block(&self, st: u32) -> bool {
        state_fact(st)[0] & FACT_COLLISION_FULL != 0
    }
    /// Pinned `BlockStateBase.isRedstoneConductor(level, pos)` (`dtb$a.g`):
    /// default in `BlockBehaviour.Properties` is `blocksMotion && isCollisionShapeFullBlock`,
    /// overridden in `Blocks.<clinit>` to `always` for `mud` and `soul_sand`, and `never` for
    /// glass, stained_glass, tinted_glass, ice, frosted_ice, beacon, leaves, copper grates,
    /// copper bulbs, glowstone, sea_lantern, redstone_block, tnt, observer, piston, and
    /// sticky_piston. Verified across all 26,684 states of the pinned 1.21.1 server jar.
    pub(crate) fn is_redstone_conductor(&self, st: u32) -> bool {
        let name = self.name_of(st);
        if matches!(name, "minecraft:mud" | "minecraft:soul_sand") {
            return true;
        }
        if matches!(
            name,
            "minecraft:glass"
                | "minecraft:tinted_glass"
                | "minecraft:ice"
                | "minecraft:frosted_ice"
                | "minecraft:beacon"
                | "minecraft:glowstone"
                | "minecraft:sea_lantern"
                | "minecraft:redstone_block"
                | "minecraft:tnt"
                | "minecraft:observer"
                | "minecraft:piston"
                | "minecraft:sticky_piston"
        ) || name.ends_with("_stained_glass")
            || name.ends_with("_leaves")
            || name.ends_with("copper_grate")
            || name.ends_with("copper_bulb")
        {
            return false;
        }
        self.blocks_motion(st) && self.is_collision_shape_full_block(st)
    }
    /// Pinned `isFaceSturdy(level, pos, direction, supportType)`. `direction` uses the native
    /// DOWN/UP/NORTH/SOUTH/WEST/EAST indices. `Support::Center` is the `canSupportCenter`
    /// question (torches, plants, signs) and differs from `Full` on 10,621 states for one
    /// direction alone: a fence post, a piston or a wall supports a centre but covers no
    /// whole face.
    pub(crate) fn is_face_sturdy(&self, st: u32, direction: u8, support: Support) -> bool {
        if direction >= 6 {
            return false;
        }
        let fact = state_fact(st);
        let faces = fact[2] as u32 | (fact[3] as u32) << 8 | (fact[4] as u32) << 16;
        (faces >> (support as usize * 6)) & 0x3f & (1 << direction) != 0
    }
    /// The pinned FULL face, which is what vanilla's three-argument
    /// `BlockStateBase.isFaceSturdy(level, pos, dir)` answers and what every caller of this
    /// older name asks.
    pub(crate) fn side_solid(&self, st: u32, dir: usize) -> bool {
        dir < 6 && self.is_face_sturdy(st, dir as u8, Support::Full)
    }
    /// The remaining bits of `assets/block_predicates_1211.bin`: the four heightmap predicate
    /// bits, the double-plant flag and the water/source-water pair. The solid-render bit that
    /// table also carries is read from [`BReg::is_solid_render`] instead, from the pinned facts.
    fn native_predicates(&self, st: u32) -> u8 {
        include_bytes!("../assets/block_predicates_1211.bin")
            .get(st as usize)
            .copied()
            .unwrap_or(0)
    }
    /// Native FULL support face for the mineshaft floor and chain anchors.
    pub(crate) fn mineshaft_sturdy_face(&self, st: u32, face: u8) -> bool {
        face < 6 && self.is_face_sturdy(st, face, Support::Full)
    }
    /// The chain anchor also asks whether the block is an instance of the chain class, which is
    /// the one fact `assets/mineshaft_support_1211.bin` records that the pinned table does not.
    pub(crate) fn mineshaft_chain_anchor(&self, st: u32) -> bool {
        include_bytes!("../assets/mineshaft_support_1211.bin")
            .get(st as usize)
            .is_some_and(|v| v & 192 == 64)
            && !self.in_tag(st, "minecraft:unstable_bottom_center")
    }
    /// Pinned `Heightmap.Types` predicate bits, from `assets/block_predicates_1211.bin`.
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
    /// Pinned `!getFluidState().isEmpty()`: the state carries fluid, which includes waterlogged
    /// plants and corals. The narrower native `liquid()` - only water, lava and bubble columns -
    /// is [`BReg::is_liquid_block`].
    pub(crate) fn is_liquid(&self, st: u32) -> bool {
        state_fact(st)[0] & FACT_FLUID_EMPTY == 0
    }
    /// Pinned `BlockStateBase.liquid()`. Exported for the audit; generation asks
    /// [`BReg::is_liquid`], which is the fluid-state question the call sites mean.
    #[allow(dead_code)]
    pub(crate) fn is_liquid_block(&self, st: u32) -> bool {
        state_fact(st)[0] & FACT_LIQUID != 0
    }
    /// The pinned `SupportType` face planes as a six-bit mask, for the audit and for the
    /// cross-check against the older single-plane assets.
    #[allow(dead_code)]
    pub(crate) fn face_plane(&self, st: u32, support: Support) -> u8 {
        let fact = state_fact(st);
        let faces = fact[2] as u32 | (fact[3] as u32) << 8 | (fact[4] as u32) << 16;
        ((faces >> (support as usize * 6)) & 0x3f) as u8
    }
    pub(crate) fn is_replaceable(&self, st: u32) -> bool {
        self.is_air(st)
            || self
                .st_fl
                .get(st as usize)
                .map(|f| f & FL_REPLACEABLE != 0)
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

    /// The solid-template placer admits only ordinary full cube outputs.
    /// Adjacent panes connect to that face (including iron bars, whose name
    /// does not end in "_pane"). Other, non-full neighbors need their own
    /// native updateShape contract.
    pub(crate) fn pane_after_full_neighbor(
        &self,
        state: u32,
        neighbor: u32,
        toward_neighbor: u8,
    ) -> Option<u32> {
        let name = self.name_of(state);
        if !name.ends_with("_pane") && name != "minecraft:iron_bars" {
            return None;
        }
        let key = match toward_neighbor {
            2 => "north",
            3 => "south",
            4 => "west",
            5 => "east",
            _ => return None,
        };
        if self.is_template_solid(neighbor) != Some(true)
            || !self.is_collision_shape_full_block(neighbor)
            || !self.is_solid_render(neighbor)
        {
            return None;
        }
        (self.get_prop(state, key).as_deref() == Some("false"))
            .then(|| self.with_prop(state, key, "true"))
    }

    /// Recompute LOW/NONE wall sides for horizontal changes that are ordinary
    /// opaque full cubes, other walls, or air. With air above, a straight
    /// opposite two-sided wall suppresses its central post; corners, endpoints
    /// and T junctions retain it. Native TALL/covered walls need shape coverage
    /// checks and are intentionally not rewritten by this bounded rule.
    pub(crate) fn wall_after_full_neighbor(
        &self,
        state: u32,
        neighbor: u32,
        above: u32,
        toward_neighbor: u8,
    ) -> Option<u32> {
        if !self.name_of(state).ends_with("_wall") || !self.is_air(above) {
            return None;
        }
        let key = match toward_neighbor {
            2 => "north",
            3 => "south",
            4 => "west",
            5 => "east",
            _ => return None,
        };
        let other_name = self.name_of(neighbor);
        let cube = self.is_template_solid(neighbor) == Some(true)
            && self.is_collision_shape_full_block(neighbor)
            && self.is_solid_render(neighbor);
        let wanted = if cube || other_name.ends_with("_wall") {
            "low"
        } else if self.is_air(neighbor) {
            "none"
        } else {
            return None;
        };
        if ["north", "south", "west", "east"]
            .iter()
            .any(|side| self.get_prop(state, side).as_deref() == Some("tall"))
        {
            return None;
        }
        let next = self.with_prop(state, key, wanted);
        let north = self.get_prop(next, "north").as_deref() == Some("low");
        let south = self.get_prop(next, "south").as_deref() == Some("low");
        let east = self.get_prop(next, "east").as_deref() == Some("low");
        let west = self.get_prop(next, "west").as_deref() == Some("low");
        let straight = (north && south && !east && !west)
            || (east && west && !north && !south);
        let result = self.with_prop(next, "up", if straight { "false" } else { "true" });
        (result != state).then_some(result)
    }

    /// Bounded runtime counterpart for immediate air removal / full-cube
    /// replacement. This can disconnect a previously connected side. For
    /// non-full neighbors, other connection types (gates, fences, walls and
    /// panes) require independent native classification and are not guessed.
    pub(crate) fn connection_after_neighbor(
        &self,
        state: u32,
        neighbor: u32,
        above: u32,
        direction: u8,
    ) -> Option<u32> {
        if let Some(connected) = self.fence_after_full_neighbor(state, neighbor, direction) {
            return Some(connected);
        }
        if let Some(connected) = self.pane_after_full_neighbor(state, neighbor, direction) {
            return Some(connected);
        }
        if let Some(connected) = self.wall_after_full_neighbor(state, neighbor, above, direction) {
            return Some(connected);
        }
        let key = match direction {
            2 => "north",
            3 => "south",
            4 => "west",
            5 => "east",
            _ => return None,
        };
        let name = self.name_of(state);
        let other = self.name_of(neighbor);
        let fence = name.ends_with("_fence");
        let other_fence = other.ends_with("_fence");
        let fence_connect = fence && other_fence
            && ((name == "minecraft:nether_brick_fence")
                == (other == "minecraft:nether_brick_fence"));
        let pane = name.ends_with("_pane") || name == "minecraft:iron_bars";
        let other_pane = other.ends_with("_pane") || other == "minecraft:iron_bars";
        if fence_connect || (pane && other_pane) {
            return (self.get_prop(state, key).as_deref() != Some("true"))
                .then(|| self.with_prop(state, key, "true"));
        }
        if self.is_air(neighbor) && (fence || pane) {
            return (self.get_prop(state, key).as_deref() == Some("true"))
                .then(|| self.with_prop(state, key, "false"));
        }
        // The isolated post remains raised in this case. Do not silently
        // overwrite tall/multi-sided wall shape geometry with a guessed post.
        if name.ends_with("_wall")
            && self.is_air(above)
            && self.get_prop(state, "up").as_deref() == Some("true")
            && self.get_prop(state, key).as_deref() == Some("low")
            && ["north", "south", "west", "east"].iter().all(|other| {
                *other == key || self.get_prop(state, other).as_deref() == Some("none")
            })
        {
            return Some(self.with_prop(state, key, "none"));
        }
        None
    }

    /// Stair shape selection is a function of surrounding STAIR orientations,
    /// not whether the changed neighbor is a solid cube. After template writes,
    /// the caller supplies a write-through world view. The result is the state
    /// change only; waterlogged, half and facing properties are preserved.
    pub(crate) fn stair_after_neighbors(
        &self,
        state: u32,
        pos: (i32, i32, i32),
        mut get: impl FnMut((i32, i32, i32)) -> u32,
    ) -> Option<u32> {
        if !self.name_of(state).ends_with("_stairs") {
            return None;
        }
        let facing = self.get_prop(state, "facing")?;
        let half = self.get_prop(state, "half")?;
        let offset = |direction: &str| -> Option<(i32, i32, i32)> {
            Some(match direction {
                "north" => (0, 0, -1),
                "south" => (0, 0, 1),
                "west" => (-1, 0, 0),
                "east" => (1, 0, 0),
                _ => return None,
            })
        };
        let at = |(dx, dy, dz): (i32, i32, i32)| {
            (pos.0.wrapping_add(dx), pos.1.wrapping_add(dy), pos.2.wrapping_add(dz))
        };
        let (dx, _, dz) = offset(&facing)?;
        let front = get(at((dx, 0, dz)));
        let stair_face = |other: u32| -> Option<String> {
            (self.name_of(other).ends_with("_stairs")
                && self.get_prop(other, "half").as_deref() == Some(half.as_str()))
                .then(|| self.get_prop(other, "facing"))
                .flatten()
        };
        let perpendicular = |direction: &str| match facing.as_str() {
            "north" | "south" => matches!(direction, "east" | "west"),
            "east" | "west" => matches!(direction, "north" | "south"),
            _ => false,
        };
        let ccw = match facing.as_str() {
            "north" => "west",
            "south" => "east",
            "west" => "south",
            "east" => "north",
            _ => return None,
        };
        // Vanilla's canTakeShape checks the flank opposite the front stair's
        // turn, or the same direction as the back stair's turn. Without that
        // check a third aligned stair becomes an incorrect inside corner.
        let can_take = |side: &str, get: &mut dyn FnMut((i32, i32, i32)) -> u32| -> bool {
            let Some(o) = offset(side) else { return false };
            let s = get(at(o));
            !self.name_of(s).ends_with("_stairs")
                || self.get_prop(s, "half").as_deref() != Some(half.as_str())
                || self.get_prop(s, "facing").as_deref() != Some(facing.as_str())
        };
        let mut shape = "straight";
        if let Some(dir) = stair_face(front) {
            if perpendicular(&dir) {
                let opposite = match dir.as_str() {
                    "north" => "south",
                    "south" => "north",
                    "west" => "east",
                    "east" => "west",
                    _ => return None,
                };
                if can_take(opposite, &mut get) {
                    shape = if dir == ccw { "outer_left" } else { "outer_right" };
                }
            }
        }
        if shape == "straight" {
            // Native only reads the rear stair if the front did not already
            // establish an outer corner; keep scripted-world reads bounded.
            let back = get(at((-dx, 0, -dz)));
            if let Some(dir) = stair_face(back) {
                if perpendicular(&dir) && can_take(&dir, &mut get) {
                    shape = if dir == ccw { "inner_left" } else { "inner_right" };
                }
            }
        }
        (self.get_prop(state, "shape").as_deref() != Some(shape))
            .then(|| self.with_prop(state, "shape", shape))
    }

    /// `StructurePiece.SHAPE_CHECK_BLOCKS`: the blocks whose `placeBlock` marks the
    /// cell with `ChunkAccess.markPosForPostprocessing`, so the owning chunk's
    /// `LevelChunk.postProcessGeneration` recomputes the connection shape when it is
    /// promoted. Exactly eleven `Blocks` identities in the pinned version — the set
    /// is by block, so the other fence woods, glass panes and the soul/redstone torch
    /// family are deliberately absent.
    pub(crate) fn needs_postprocess_shape(&self, st: u32) -> bool {
        matches!(
            self.name_of(st),
            "minecraft:nether_brick_fence"
                | "minecraft:torch"
                | "minecraft:wall_torch"
                | "minecraft:oak_fence"
                | "minecraft:spruce_fence"
                | "minecraft:birch_fence"
                | "minecraft:jungle_fence"
                | "minecraft:acacia_fence"
                | "minecraft:dark_oak_fence"
                | "minecraft:ladder"
                | "minecraft:iron_bars"
        )
    }

    /// A known fragile support block is removed if its direct attachment
    /// becomes air or liquid. The direction points FROM the fragile block
    /// TOWARDS its old support, in the six native direction indices.
    ///
    /// This handles known torch/lantern families only. Rails, buttons, plates,
    /// plants and conditional face-sturdiness are separate shape contracts.
    pub(crate) fn fragile_loses_support(
        &self,
        state: u32,
        toward_support: u8,
        new_support: u32,
    ) -> bool {
        if !self.is_air(new_support) && !self.is_liquid(new_support) {
            return false;
        }
        let name = self.name_of(state);
        match name {
            "minecraft:torch" | "minecraft:soul_torch" | "minecraft:redstone_torch" => {
                toward_support == 0
            }
            "minecraft:wall_torch"
            | "minecraft:soul_wall_torch"
            | "minecraft:redstone_wall_torch" => {
                let face = self.get_prop(state, "facing");
                let behind = match face.as_deref() {
                    Some("north") => 3,
                    Some("south") => 2,
                    Some("west") => 5,
                    Some("east") => 4,
                    _ => return false,
                };
                toward_support == behind
            }
            "minecraft:lantern" | "minecraft:soul_lantern" => {
                let hanging = self.get_prop(state, "hanging");
                toward_support
                    == if hanging.as_deref() == Some("true") { 1 } else { 0 }
            }
            _ => false,
        }
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

    /// `minecraft:name[a=1,b=2]` with the property names sorted, the same spelling the
    /// region reader emits for a saved chunk. Whole-chunk comparison needs this: a name-only
    /// diff counts a stair facing the wrong way as equal.
    pub(crate) fn canonical_state(&self, st: u32) -> String {
        let Some(blk) = self.st_blk.get(st as usize).map(|b| *b as usize) else {
            return format!("<state {st} outside the registry>");
        };
        let name = self.bname.get(blk).cloned().unwrap_or_default();
        let mut parts: Vec<String> = self
            .props
            .get(blk)
            .map(|props| {
                props
                    .iter()
                    .filter_map(|p| self.get_prop(st, &p.name).map(|v| format!("{}={}", p.name, v)))
                    .collect()
            })
            .unwrap_or_default();
        parts.sort();
        if parts.is_empty() {
            name
        } else {
            format!("{}[{}]", name, parts.join(","))
        }
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
