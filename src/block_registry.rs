// block_registry.rs — block-state ids, properties, tags and the per-state
// values (hardness, drops, light, behaviour). Block-level data comes from
// assets/blocks_1211.json (minecraft-data, MIT - see THIRD_PARTY.md); the per-state light,
// flag and face facts come from assets/block_states_1211.json, which
// tools/vanilla_data/gen_vanilla_data.sh reads out of the vanilla 1.21.1 server jar.
// Terrain and feature generation read it; it knows nothing about terrain, placement or features.

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

/// What a block does when a random tick lands on it. Zero is the common case and the
/// registry stores it for every block, so the dispatch is a compare against zero.
pub(crate) const B_NONE: u8 = 0;
pub(crate) const B_SUGAR_CANE: u8 = 1;
/// Blocks affected by gravity; scheduling and physics belong to the falling loop.
pub(crate) const B_FALLING: u8 = 2;

pub(crate) struct BReg {
    bname: Vec<String>,
    idx: HashMap<String, u32>,
    props: Vec<Box<[Prop]>>,
    dfl: Vec<u32>,
    st_lo: Vec<u32>,
    n_states: Vec<u32>,
    st_blk: Vec<u32>,      // state id -> block idx
    st_fl: Vec<u16>,       // state id -> FL_* bits (assets/block_states_1211.json `flags`)
    st_side: Vec<u8>,      // state id -> bit d set when face d is fully solid (`full_sides`)
    hardness: Vec<f32>,    // state id -> vanilla destroy time (-1 unbreakable)
    htools: Vec<Vec<i64>>, // block idx -> tool item ids that can harvest it (empty = any)
    tool_kind: Vec<i8>,    // block idx -> tool kind mined at full speed (-1 = none)
    /// States of blocks that assets/block_states_1211.json does not cover under the same name and
    /// state range as the block table. The registry gate requires this to be empty.
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
}

// Bit meanings are fixed by `flag_bits` in assets/block_states_1211.json; load() checks them.
const FL_AIR: u16 = 1;
const FL_REPLACEABLE: u16 = 1 << 1;
/// BlockState.isLiquid(): water, lava, bubble column. Not read yet; kept so the file's bits are all named.
#[allow(dead_code)]
const FL_LIQUID: u16 = 1 << 2;
const FL_SOLID: u16 = 1 << 3;
/// The state holds a non-empty fluid: water, lava, waterlogging, kelp, seagrass.
const FL_FLUID: u16 = 1 << 4;

impl BReg {
    fn load() -> Option<BReg> {
        let md = jparse(&std::fs::read("assets/blocks_1211.json").ok()?)?
            .into_arr()?
            .clone();
        let facts = jparse(&std::fs::read("assets/block_states_1211.json").ok()?)?;
        let bits = facts.get("flag_bits")?;
        for (name, bit) in [
            ("air", FL_AIR),
            ("replaceable", FL_REPLACEABLE),
            ("liquid", FL_LIQUID),
            ("solid", FL_SOLID),
            ("fluid", FL_FLUID),
        ] {
            if jnum(bits, name)? as u16 != bit {
                return None;
            }
        }
        let nums = |key: &str| -> Option<Vec<u32>> {
            Some(jarr(&facts, key)?.iter().map(|v| v.num() as u32).collect())
        };
        let (f_lum, f_opa, f_fl, f_sides) = (
            nums("luminance")?,
            nums("light_block")?,
            nums("flags")?,
            nums("full_sides")?,
        );
        if [f_opa.len(), f_fl.len(), f_sides.len()]
            .iter()
            .any(|&n| n != f_lum.len())
        {
            return None;
        }
        // name -> (first state id, state count) as the facts file numbers them
        let mut f_blocks: HashMap<String, (u32, u32)> = HashMap::new();
        for b in jarr(&facts, "blocks")? {
            let a = b.into_arr()?;
            f_blocks.insert(
                a.first()?.str().to_string(),
                (a.get(1)?.num() as u32, a.get(2)?.num() as u32),
            );
        }
        let n_states_max = md
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
            st_blk: vec![0; n_states_max + 1],
            st_fl: vec![0; n_states_max + 1],
            st_side: vec![0; n_states_max + 1],
            hardness: vec![0.6; n_states_max + 1],
            htools: vec![],
            tool_kind: vec![],
            unpaired: Vec::new(),
            drops: vec![-1; n_states_max + 1],
            drops_second: vec![-1; n_states_max + 1],
            stack_size: vec![64; n_states_max + 1],
            behaviour: vec![],
            luminance: vec![0; n_states_max + 1],
            opacity: vec![0; n_states_max + 1],
            tag: HashMap::new(),
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
            // The facts file is indexed by state id. It is only trusted for a block whose name,
            // first state and state count are the ones the block table has.
            let facts_agree =
                f_blocks.get(&name) == Some(&(lo, hi - lo + 1)) && (hi as usize) < f_lum.len();
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
            let (drop0, drop1) = match b.get("drops").and_then(|d| d.into_arr()) {
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
                if st > n_states_max {
                    break;
                }
                reg.st_blk[st] = bi;
                reg.hardness[st] = hardness;
                reg.drops[st] = drop0;
                reg.drops_second[st] = drop1;
                reg.stack_size[st] = stack_size;
                if facts_agree {
                    reg.st_fl[st] = f_fl[st] as u16;
                    reg.st_side[st] = f_sides[st] as u8;
                    reg.luminance[st] = f_lum[st] as u8;
                    reg.opacity[st] = f_opa[st] as u8;
                } else {
                    reg.unpaired.push(format!("{name} state {st}"));
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
        let names: Vec<String> = raw.keys().cloned().collect();
        for n in names {
            let mut stack = HashSet::new();
            let set = expand(&n, &raw, &reg.idx, &mut reg.tag, &mut stack);
            reg.tag.insert(n, set);
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
    /// True when the state holds a fluid (FL_FLUID): water, lava, waterlogged blocks, kelp,
    /// seagrass. This is wider than vanilla's `BlockState.isLiquid()` (FL_LIQUID), which is
    /// only water, lava and bubble columns; the callers were written against the wider set.
    pub(crate) fn is_liquid(&self, st: u32) -> bool {
        self.st_fl
            .get(st as usize)
            .map(|f| f & FL_FLUID != 0)
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
            let digit = if mult > 0 {
                (off as u64 / mult) % len
            } else {
                0
            };
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
            let d = if mult > 0 {
                (off as u64 / mult) % len
            } else {
                0
            };
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
    pub(crate) fn in_tag(&self, st: u32, tag: &str) -> bool {
        let bi = match self.st_blk.get(st as usize) {
            Some(&b) => b,
            None => return false,
        };
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
