// feature.rs — datapack-driven vanilla feature layer for mc-spine.
// Loads configured/placed feature JSON + biome feature lists + block tags from
// datapacks, evaluates the vanilla placement-modifier chain per chunk (3x3
// neighborhood, population/decorator seeds), and writes block-state overrides
// into Gen::feat_c. State ids come from assets/blocks_1211.json (1.21.1 wire
// ids); solidity/side flags come from assets/pblocks.json (pumpkin flags table).

use crate::worldgen::*;
use std::collections::{HashMap, HashSet};

// ---------- helpers ----------

pub(crate) fn js_pub<'a>(j: &'a J, k: &str) -> Option<&'a J> {
    js(j,k)
}
fn js<'a>(j: &'a J, k: &str) -> Option<&'a J> {
    j.get(k)
}
pub(crate) fn jstr_pub(j: &J, k: &str) -> Option<String> { jstr(j,k) }
pub(crate) fn jparse_pub(raw: &[u8]) -> Option<J> { jparse(raw) }
fn jstr(j: &J, k: &str) -> Option<String> {
    js(j, k).map(|v| v.str().to_string())
}
pub(crate) fn jnum_pub(j: &J, k: &str) -> Option<f64> { jnum(j,k) }
fn jnum(j: &J, k: &str) -> Option<f64> {
    js(j, k).map(|v| v.num())
}
pub(crate) fn jint_pub(j: &J, k: &str, d: i32) -> i32 { jint(j,k,d) }
fn jint(j: &J, k: &str, d: i32) -> i32 {
    jnum(j, k).unwrap_or(d as f64) as i32
}
fn jf32(j: &J, k: &str, d: f32) -> f32 {
    jnum(j, k).unwrap_or(d as f64) as f32
}
fn jbool(j: &J, k: &str, d: bool) -> bool {
    match js(j, k) {
        Some(J::Num(n)) => *n != 0.0,
        Some(J::Str(s)) => s == "true",
        _ => d,
    }
}
fn jarr<'a>(j: &'a J, k: &str) -> Option<&'a Vec<J>> {
    js(j, k).and_then(|v| v.into_arr())
}
fn ns_name(s: &str) -> String {
    if s.contains(':') {
        s.to_string()
    } else {
        format!("minecraft:{}", s)
    }
}
// blockstate json (string or {id,properties}) -> (name, props)
fn bs_parts(j: &J) -> Option<(String, Option<&J>)> {
    match j {
        J::Str(s) => Some((ns_name(s), None)),
        J::Obj(_) => Some((ns_name(&jstr(j, "id")?), js(j, "properties"))),
        _ => None,
    }
}

// ---------- block registry ----------

struct Prop {
    name: String,
    vals: Vec<String>,
}

pub(crate) struct BReg {
    bname: Vec<String>,
    idx: HashMap<String, u32>,
    props: Vec<Box<[Prop]>>,
    dfl: Vec<u32>,
    st_lo: Vec<u32>,
    n_states: Vec<u32>,
    st_blk: Vec<u32>,  // state id -> block idx
    st_fl: Vec<u16>,   // pumpkin state_flags
    st_side: Vec<u8>,  // pumpkin side_flags
    hardness: Vec<f32>, // state id -> vanilla destroy time (-1 unbreakable)
    tag: HashMap<String, HashSet<u32>>, // tag name -> block idx set (expanded)
}

const FL_AIR: u16 = 1;
const FL_REPLACEABLE: u16 = 1 << 4;
const FL_LIQUID: u16 = 1 << 5;
const FL_SOLID: u16 = 1 << 6;

impl BReg {
    fn load() -> Option<BReg> {
        let md = jparse(&std::fs::read("assets/blocks_1211.json").ok()?)?.into_arr()?.clone();
        let pb = jparse(&std::fs::read("assets/pblocks.json").ok()?)?;
        let pb_blocks = pb.get("blocks")?.into_arr()?;
        let mut pb_by_name: HashMap<String, &J> = HashMap::new();
        for b in pb_blocks {
            pb_by_name.insert(format!("minecraft:{}", jstr(b, "name")?), b);
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
            reg.dfl.push(jnum(b, "defaultState").unwrap_or(lo as f64) as u32);
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
            // Destroy time is a property of the block, so it is read here once and
            // held per state rather than re-derived by a hand table. A hand table
            // covered 356 of the 26684 states, which meant mining time was wrong for
            // almost every block in the world.
            let hardness = jnum(b, "hardness").unwrap_or(0.6) as f32;
            let n = (hi - lo + 1) as usize;
            for i in 0..n {
                let st = (lo + i as u32) as usize;
                if st > n_states_max {
                    break;
                }
                reg.st_blk[st] = bi;
                reg.hardness[st] = hardness;
                if let Some(pbb) = pbr {
                    if let Some(sts) = jarr(pbb, "states") {
                        if sts.len() == n {
                            if let J::Obj(_) = &sts[i] {
                                reg.st_fl[st] =
                                    jnum(&sts[i], "state_flags").unwrap_or(0.0) as u16;
                                reg.st_side[st] =
                                    jnum(&sts[i], "side_flags").unwrap_or(0.0) as u8;
                            }
                        }
                    }
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
    pub(crate) fn state_count(&self) -> usize {
        self.st_blk.len()
    }

    /// Vanilla destroy time in seconds. Negative means unbreakable.
    pub(crate) fn hardness_of(&self, st: u32) -> f32 {
        self.hardness.get(st as usize).copied().unwrap_or(0.6)
    }

    pub(crate) fn is_air(&self, st: u32) -> bool {
        st == 0
            || self.st_fl.get(st as usize).map(|f| f & FL_AIR != 0).unwrap_or(true)
    }
    pub(crate) fn is_solid(&self, st: u32) -> bool {
        self.st_fl.get(st as usize).map(|f| f & FL_SOLID != 0).unwrap_or(false)
    }
    pub(crate) fn is_liquid(&self, st: u32) -> bool {
        self.st_fl.get(st as usize).map(|f| f & FL_LIQUID != 0).unwrap_or(false)
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
            let digit = if mult > 0 { (off as u64 / mult) % len } else { 0 };
            if let Some(nd) = p.vals.iter().position(|v| v == val) {
                let o = off as u64 + (nd as u64 + len - digit) % len * mult;
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
            let d = if mult > 0 { (off as u64 / mult) % len } else { 0 };
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
        self.tag.get(&ns_name(tag)).map(|s| s.contains(&bi)).unwrap_or(false)
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
                                } else if p
                                    .extension()
                                    .map(|x| x == "json")
                                    .unwrap_or(false)
                                {
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

fn reg_props_axis(name: &str) -> Option<()> {
    breg().and_then(|r| r.has_prop(name, "axis").then_some(()))
}

static BREG: std::sync::OnceLock<Option<BReg>> = std::sync::OnceLock::new();
pub(crate) fn breg() -> Option<&'static BReg> {
    BREG.get_or_init(BReg::load).as_ref()
}

// ---------- providers ----------

pub(crate) enum IP {
    Const(i32),
    Uniform(i32, i32),
    BiasBottom(i32, i32),
    VeryBias(i32, i32, #[expect(dead_code)] i32), // (min,max,inner) inner unused — vanilla min-of-3
    Clamped(Box<IP>, i32, i32),
    Trap(i32, i32, i32),
    Norm(f64, f64, i32, i32),
    Weighted(Vec<(i32, IP)>),
}
impl IP {
    pub(crate) fn get(&self, r: &mut Xor) -> i32 {
        match self {
            IP::Const(v) => *v,
            IP::Uniform(a, b) => r.next_inbetween(*a, *b),
            IP::BiasBottom(a, b) => {
                let u1 = r.next_inbetween(*a, *b);
                let u2 = r.next_inbetween(*a, *b);
                u1.min(u2)
            }
            IP::VeryBias(a, b, _) => {
                let u1 = r.next_inbetween(*a, *b);
                let u2 = r.next_inbetween(*a, *b);
                let u3 = r.next_inbetween(*a, *b);
                u1.min(u2).min(u3)
            }
            IP::Clamped(inner, lo, hi) => inner.get(r).clamp(*lo, *hi),
            IP::Trap(a, b, pl) => {
                let (a, b, pl) = (*a, *b, *pl);
                if b - a - pl < 0 {
                    return a;
                }
                let lo = a + (b - a - pl) / 2;
                let hi = b - (b - a - pl) / 2;
                let v = r.next_inbetween(a, b);
                if v > lo && v < hi {
                    hi - (v - lo)
                } else {
                    v
                }
            }
            IP::Norm(mean, dev, lo, hi) => (mean + r.next_gauss() * dev)
                .round()
                .clamp(*lo as f64, *hi as f64) as i32,
            IP::Weighted(list) => {
                let total: i32 = list.iter().map(|(w, _)| (*w).max(0)).sum();
                if total <= 0 {
                    return 0;
                }
                let mut roll = r.next_bound(total);
                for (w, p) in list {
                    roll -= (*w).max(0);
                    if roll < 0 {
                        return p.get(r);
                    }
                }
                list.last().unwrap().1.get(r)
            }
        }
    }
}
pub(crate) fn p_ip_pub(j: &J) -> Option<IP> { p_ip(j) }
fn p_ip(j: &J) -> Option<IP> {
    if let J::Num(n) = j {
        return Some(IP::Const(*n as i32));
    }
    let ty = jstr(j, "type")?;
    let ty = ty.strip_prefix("minecraft:").unwrap_or(&ty);
    Some(match ty {
        "constant" | "literal" => IP::Const(jint(j, "value", jint(j, "constant", 0))),
        "uniform" => IP::Uniform(
            jint(j, "min_inclusive", jint(j, "min", 0)),
            jint(j, "max_inclusive", jint(j, "max", 0)),
        ),
        "biased_to_bottom" => IP::BiasBottom(
            jint(j, "min_inclusive", jint(j, "min", 0)),
            jint(j, "max_inclusive", jint(j, "max", 0)),
        ),
        "very_biased_to_bottom" => IP::VeryBias(
            jint(j, "min_inclusive", jint(j, "min", 0)),
            jint(j, "max_inclusive", jint(j, "max", 0)),
            jint(j, "inner", 1),
        ),
        "clamped" => IP::Clamped(
            Box::new(p_ip(js(j, "source")?)?),
            jint(j, "min_inclusive", 0),
            jint(j, "max_inclusive", 0),
        ),
        "trapezoid" => IP::Trap(
            jint(j, "min_inclusive", jint(j, "min", 0)),
            jint(j, "max_inclusive", jint(j, "max", 0)),
            jint(j, "plateau", 0),
        ),
        "clamped_normal" => IP::Norm(
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
                v.push((w, p_ip(d)?));
            }
            IP::Weighted(v)
        }
        _ => return None,
    })
}

#[derive(Clone)]
enum YA {
    Abs(i32),
    AboveBottom(i32),
    BelowTop(i32),
}
impl YA {
    fn get(&self, min_y: i32, height: i32) -> i32 {
        match self {
            YA::Abs(y) => *y,
            YA::AboveBottom(o) => min_y.saturating_add(*o),
            YA::BelowTop(o) => min_y.saturating_add(height).saturating_sub(*o),
        }
    }
}
fn p_ya(j: &J) -> Option<YA> {
    match j {
        J::Num(n) => Some(YA::Abs(*n as i32)),
        J::Obj(_) => {
            if let Some(v) = jnum(j, "absolute") {
                Some(YA::Abs(v as i32))
            } else if let Some(v) = jnum(j, "above_bottom") {
                Some(YA::AboveBottom(v as i32))
            } else {
                jnum(j, "below_top").map(|v| YA::BelowTop(v as i32))
            }
        }
        _ => None,
    }
}
enum HP {
    Uniform(YA, YA),
    Trap(YA, YA),
    VeryBias(YA, YA),
}
impl HP {
    fn get(&self, r: &mut Xor, min_y: i32, height: i32) -> i32 {
        match self {
            HP::Uniform(a, b) => r.next_inbetween(a.get(min_y, height), b.get(min_y, height)),
            HP::Trap(a, b) => {
                let lo = a.get(min_y, height);
                let hi = b.get(min_y, height);
                if lo >= hi {
                    return lo;
                }
                // vanilla TrapezoidHeightProvider: triangular via midpoint of two rolls
                let u1 = r.next_inbetween(lo, hi);
                let u2 = r.next_inbetween(lo, hi);
                (u1 + u2) / 2
            }
            HP::VeryBias(a, b) => {
                let lo = a.get(min_y, height);
                let hi = b.get(min_y, height);
                if lo >= hi {
                    return lo;
                }
                let u1 = r.next_inbetween(lo, hi);
                let u2 = r.next_inbetween(lo, hi);
                let u3 = r.next_inbetween(lo, hi);
                u1.min(u2).min(u3)
            }
        }
    }
}
fn p_hp(j: &J) -> Option<HP> {
    let ty = jstr(j, "type")?;
    let ty = ty.strip_prefix("minecraft:").unwrap_or(&ty);
    if ty == "constant" {
        return Some(HP::Uniform(
            p_ya(js(j, "value").or_else(|| js(j, "min_inclusive"))?)?,
            p_ya(js(j, "value").or_else(|| js(j, "min_inclusive"))?)?,
        ));
    }
    let lo = p_ya(js(j, "min_inclusive")?)?;
    let hi = p_ya(js(j, "max_inclusive")?)?;
    Some(match ty {
        "uniform" => HP::Uniform(lo, hi),
        "trapezoid" => HP::Trap(lo, hi),
        "very_biased_to_bottom" | "biased_to_bottom" => HP::VeryBias(lo, hi),
        _ => return None,
    })
}

// ---------- block state providers ----------

pub(crate) enum BSP {
    Simple(u32),
    Rotated { base: u32, axis: bool },
    Weighted(Vec<(i32, u32)>),
    RandomInt { src: Box<BSP>, prop: String, vals: Vec<i32> },
    Noise { n: Normal, scale: f64, states: Vec<u32> },
    NoiseThr { n: Normal, scale: f64, thr: f32, hchance: f32, def: u32, low: Vec<u32>, high: Vec<u32> },
    Dual { n: Normal, scale: f64, variety: (i32, i32), slow_scale: f64, slow_variety: (i32, i32), states: Vec<u32> },
    RuleBased { fb: Option<Box<BSP>>, rules: Vec<(Pr, BSP)> },
}
fn noise_state(states: &[u32], v: f64) -> u32 {
    // vanilla: midpoint(1.0, v) -> index into the list
    let val = ((1.0 + v) / 2.0).clamp(0.0, 0.9999);
    states[(val * states.len() as f64) as usize]
}
impl BSP {
    pub(crate) fn get(&self, ctx: &mut FC, r: &mut Xor, x: i32, y: i32, z: i32) -> u32 {
        match self {
            BSP::Simple(s) => *s,
            BSP::Rotated { base, axis } => {
                if !*axis {
                    *base
                } else {
                    let ax = match r.next_bound(3) {
                        0 => "x",
                        1 => "y",
                        _ => "z",
                    };
                    breg().map(|reg| reg.with_prop(*base, "axis", ax)).unwrap_or(*base)
                }
            }
            BSP::Weighted(list) => {
                let total: i32 = list.iter().map(|(w, _)| (*w).max(0)).sum();
                if total <= 0 {
                    return list.first().map(|e| e.1).unwrap_or(0);
                }
                let mut roll = r.next_bound(total);
                for (w, s) in list {
                    roll -= (*w).max(0);
                    if roll < 0 {
                        return *s;
                    }
                }
                list.last().unwrap().1
            }
            BSP::RandomInt { src, prop, vals } => {
                let st = src.get(ctx, r, x, y, z);
                if !vals.is_empty() {
                    let i = r.next_bound(vals.len() as i32) as usize;
                    if let Some(reg) = breg() {
                        return reg.with_prop(st, prop, &vals[i].to_string());
                    }
                }
                st
            }
            BSP::Noise { n, scale, states } => {
                if states.is_empty() {
                    return 0;
                }
                let v = n.get(x as f64 * scale, y as f64 * scale, z as f64 * scale);
                noise_state(states, v)
            }
            BSP::NoiseThr { n, scale, thr, hchance, def, low, high } => {
                let v = n.get(x as f64 * scale, y as f64 * scale, z as f64 * scale);
                if v < *thr as f64 {
                    if low.is_empty() {
                        return *def;
                    }
                    return low[r.next_bound(low.len() as i32) as usize];
                }
                if r.next_f32() < *hchance {
                    if !high.is_empty() {
                        return high[r.next_bound(high.len() as i32) as usize];
                    }
                }
                *def
            }
            BSP::Dual { n, scale, variety, slow_scale, slow_variety: _sv, states } => {
                if states.is_empty() {
                    return 0;
                }
                // vanilla: slow noise picks a sub-range bucket, fast noise picks
                // within it
                let sv = n.get(
                    x as f64 * slow_scale,
                    y as f64 * slow_scale,
                    z as f64 * slow_scale,
                );
                let span = variety.1 - variety.0 + 1;
                let bucket = (((1.0 + sv) / 2.0) * span.max(1) as f64 + variety.0 as f64)
                    .floor() as i32;
                let bucket = bucket.clamp(0, span.max(1) - 1);
                let per = states.len() as i32 / span.max(1).max(1);
                let lo = (bucket * per).clamp(0, states.len() as i32 - 1) as usize;
                let hi = ((bucket + 1) * per).clamp(lo as i32 + 1, states.len() as i32) as usize;
                let sub = &states[lo..hi];
                let v = n.get(x as f64 * scale, y as f64 * scale, z as f64 * scale);
                noise_state(sub, v)
            }
            BSP::RuleBased { fb, rules } => {
                if let Some(reg) = breg() {
                    for (cond, prov) in rules {
                        if test_pred(cond, reg, ctx, x, y, z) {
                            return prov.get(ctx, r, x, y, z);
                        }
                    }
                }
                fb.as_ref().map(|f| f.get(ctx, r, x, y, z)).unwrap_or_else(|| ctx.get(x, y, z))
            }
        }
    }
}
fn noise_j(j: &J) -> Option<Normal> {
    let noise = js(j, "noise")?;
    let fo = jint(noise, "first_octave", -4);
    let mut amps = vec![];
    for v in jarr(noise, "amplitudes")? {
        amps.push(v.num());
    }
    let seed = jnum(j, "seed").unwrap_or(0.0) as i64 as u64;
    Some(normal_new_l(seed, fo, &amps))
}
fn st_list(j: &J, k: &str) -> Vec<u32> {
    let mut out = vec![];
    if let Some(vs) = jarr(j, k) {
        for v in vs {
            if let Some((n, p)) = bs_parts(v) {
                if let Some(reg) = breg() {
                    out.push(reg.state_of(&n, p));
                }
            }
        }
    }
    out
}
pub(crate) fn p_bsp_pub(j: &J) -> Option<BSP> { p_bsp(j) }
fn p_bsp(j: &J) -> Option<BSP> {
    if let Some((n, p)) = bs_parts(j) {
        if js(j, "type").is_none() || js(j, "id").is_some() && jstr(j, "type").is_none() {
            return Some(BSP::Simple(breg()?.state_of(&n, p)));
        }
    }
    let ty = jstr(j, "type")?;
    let ty = ty.strip_prefix("minecraft:").unwrap_or(&ty);
    Some(match ty {
        "simple_state_provider" => p_bsp(js(j, "state")?)?,
        "rotated_block_provider" => {
            // vanilla RotatedBlockProvider randomizes the axis
            let (n, p) = bs_parts(js(j, "state")?)?;
            BSP::Rotated {
                base: breg()?.state_of(&n, p),
                axis: reg_props_axis(&n).is_some(),
            }
        }
        "weighted_state_provider" | "weighted" => {
            let mut v = vec![];
            for e in jarr(j, "entries")? {
                let w = jint(e, "weight", 1);
                let (n, p) = bs_parts(js(e, "data")?)?;
                v.push((w, breg()?.state_of(&n, p)));
            }
            BSP::Weighted(v)
        }
        "randomized_int_state_provider" => {
            let mut vals = vec![];
            if let Some(vs) = jarr(j, "values") {
                for v in vs {
                    vals.push(v.num() as i32);
                }
            }
            BSP::RandomInt {
                src: Box::new(p_bsp(js(j, "source")?)?),
                prop: jstr(j, "property")?,
                vals,
            }
        }
        "noise_provider" => BSP::Noise {
            n: noise_j(j)?,
            scale: jnum(j, "scale").unwrap_or(1.0),
            states: st_list(j, "states"),
        },
        "noise_threshold_provider" => {
            let (dn, dp) = bs_parts(js(j, "default_state")?)?;
            BSP::NoiseThr {
                n: noise_j(j)?,
                scale: jnum(j, "scale").unwrap_or(1.0),
                thr: jf32(j, "threshold", 0.0),
                hchance: jf32(j, "high_chance", 1.0),
                def: breg()?.state_of(&dn, dp),
                low: st_list(j, "low_states"),
                high: st_list(j, "high_states"),
            }
        }
        "dual_noise_provider" => {
            let pr = |k: &str| -> (i32, i32) {
                match js(j, k) {
                    Some(J::Arr(a)) => (
                        a.first().map(|v| v.num() as i32).unwrap_or(1),
                        a.get(1).map(|v| v.num() as i32).unwrap_or(1),
                    ),
                    Some(J::Obj(_)) => (
                        jint(js(j, k).unwrap(), "min_inclusive", 1),
                        jint(js(j, k).unwrap(), "max_inclusive", 1),
                    ),
                    _ => (1, 1),
                }
            };
            BSP::Dual {
                n: noise_j(j)?,
                scale: jnum(j, "scale").unwrap_or(1.0),
                variety: pr("variety"),
                slow_scale: jnum(j, "slow_noise_scale").unwrap_or(1.0),
                slow_variety: pr("slow_variety"),
                states: st_list(j, "states"),
            }
        }
        "rule_based" | "rule_based_block_state_provider" => {
            let mut rules = vec![];
            if let Some(rs) = jarr(j, "rules") {
                for e in rs {
                    let cond = js(e, "if_true").and_then(p_pred);
                    let then = js(e, "then").and_then(p_bsp);
                    if let (Some(c), Some(t)) = (cond, then) {
                        rules.push((c, t));
                    }
                }
            }
            BSP::RuleBased {
                fb: js(j, "fallback").and_then(p_bsp).map(Box::new),
                rules,
            }
        }
        _ => return None,
    })
}

// ---------- predicates / rule tests ----------

pub(crate) enum Pr {
    True,
    Blocks(Vec<String>),
    Tag(String),
    Fluids(Vec<String>),
    Solid,
    Replaceable,
    WouldSurvive,
    InsideWB,
    HasSturdy([i32; 3], u8),
    Unobstructed,
    Any(Vec<Pr>),
    All(Vec<Pr>),
    Not(Box<Pr>),
}
fn off_arr(j: &J) -> [i32; 3] {
    jarr(j, "offset")
        .map(|a| {
            [
                a.first().map(|v| v.num() as i32).unwrap_or(0),
                a.get(1).map(|v| v.num() as i32).unwrap_or(0),
                a.get(2).map(|v| v.num() as i32).unwrap_or(0),
            ]
        })
        .unwrap_or([0, 0, 0])
}
fn p_pred(j: &J) -> Option<Pr> {
    let ty = jstr(j, "type")?;
    let ty = ty.strip_prefix("minecraft:").unwrap_or(&ty);
    Some(match ty {
        "true" | "always_true" => Pr::True,
        "matching_blocks" | "block_match" | "blockstate_match" => {
            let mut v = vec![];
            if let Some(bs) = jarr(j, "blocks") {
                for b in bs {
                    v.push(ns_name(b.str()));
                }
            } else if let Some(b) = jstr(j, "block").or_else(|| jstr(j, "id")) {
                v.push(ns_name(&b));
            }
            Pr::Blocks(v)
        }
        "matching_block_tag" | "tag_match" => Pr::Tag(jstr(j, "tag")?),
        "matching_fluids" => {
            let mut v = vec![];
            if let Some(fs) = jarr(j, "fluids") {
                for f in fs {
                    v.push(ns_name(f.str()));
                }
            } else if let Some(f) = jstr(j, "fluid") {
                v.push(ns_name(&f));
            }
            Pr::Fluids(v)
        }
        "solid" => Pr::Solid,
        "replaceable" => Pr::Replaceable,
        "not" => Pr::Not(Box::new(p_pred(js(j, "predicate")?)?)),
        "any_of" => Pr::Any(jarr(j, "predicates")?.iter().filter_map(p_pred).collect()),
        "all_of" => Pr::All(jarr(j, "predicates")?.iter().filter_map(p_pred).collect()),
        "would_survive" => Pr::WouldSurvive,
        "inside_world_bounds" => Pr::InsideWB,
        "has_sturdy_face" => {
            let d = match jstr(j, "direction").as_deref() {
                Some("down") => 0u8,
                Some("up") => 1,
                Some("north") => 2,
                Some("south") => 3,
                Some("west") => 4,
                _ => 5,
            };
            Pr::HasSturdy(off_arr(j), d)
        }
        "unobstructed" => Pr::Unobstructed,
        _ => return None,
    })
}

enum RT {
    Tag(String),
    Block(String),
    BlockState(u32),
    Any(Vec<RT>),
    All(Vec<RT>),
    Not(Box<RT>),
    Height(i32, i32),
    RandBlock(String, f32),
    RandBlockState(u32, f32),
    True,
}
fn p_rt(j: &J) -> Option<RT> {
    let ty = jstr(j, "predicate_type").or_else(|| jstr(j, "type"))?;
    let ty = ty.strip_prefix("minecraft:").unwrap_or(&ty);
    Some(match ty {
        "always_true" => RT::True,
        "tag_match" => RT::Tag(jstr(j, "tag")?),
        "block_match" => RT::Block(ns_name(&jstr(j, "block")?)),
        "blockstate_match" => {
            let (n, p) = bs_parts(js(j, "block_state")?)?;
            RT::BlockState(breg()?.state_of(&n, p))
        }
        "any_of" => RT::Any(jarr(j, "rules")?.iter().filter_map(p_rt).collect()),
        "all_of" => RT::All(jarr(j, "rules")?.iter().filter_map(p_rt).collect()),
        "not" => RT::Not(Box::new(p_rt(js(j, "predicate")?)?)),
        "height_match" => {
            RT::Height(jint(j, "min_inclusive", 0), jint(j, "max_inclusive", 0))
        }
        "random_block_match" => {
            RT::RandBlock(ns_name(&jstr(j, "block")?), jf32(j, "probability", 0.0))
        }
        "random_blockstate_match" => {
            let (n, p) = bs_parts(js(j, "block_state")?)?;
            RT::RandBlockState(breg()?.state_of(&n, p), jf32(j, "probability", 0.0))
        }
        _ => return None,
    })
}

// ---------- placement modifiers ----------

enum PM {
    Count(IP),
    CountEveryLayer(IP),
    InSquare,
    Offset(IP, IP),
    HeightRange(HP),
    Heightmap(u8), // 0=world_surface 1=ocean_floor
    Biome,
    BlockPred(Pr),
    Rarity(i32),
    SurfaceRel(i32, i32),   // min_inclusive, max_inclusive (i32::MIN/MAX for absent)
    SurfWaterDepth(i32),
    NoiseCount(i32, f64, f64),
    NoiseThrCount(f64, i32, i32),
    RandomChance(f32),
    EnvScan(u8, Option<Pr>, Pr, i32),
    Carving,
}
fn p_mod(j: &J) -> Option<PM> {
    let ty = jstr(j, "type")?;
    let ty = ty.strip_prefix("minecraft:").unwrap_or(&ty);
    Some(match ty {
        "count" => PM::Count(p_ip(js(j, "count")?)?),
        "count_on_every_layer" => PM::CountEveryLayer(p_ip(js(j, "count")?)?),
        "in_square" => PM::InSquare,
        "offset" | "random_offset" => PM::Offset(
            p_ip(js(j, "x_spread").or_else(|| js(j, "x")).or_else(|| js(j, "xz_spread"))?)?,
            p_ip(js(j, "y_spread").or_else(|| js(j, "y"))?)?,
        ),
        "height_range" => PM::HeightRange(p_hp(js(j, "height")?)?),
        "heightmap" => PM::Heightmap(match jstr(j, "heightmap").as_deref() {
            Some(v) if v.contains("OCEAN_FLOOR") => 1,
            _ => 0,
        }),
        "biome" => PM::Biome,
        "block_predicate_filter" => PM::BlockPred(p_pred(js(j, "predicate")?)?),
        "rarity_filter" => PM::Rarity(jint(j, "chance", 1)),
        "surface_relative_threshold_filter" => PM::SurfaceRel(
            jint(j, "min_inclusive", i32::MIN),
            jint(j, "max_inclusive", i32::MAX),
        ),
        "surface_water_depth_filter" => PM::SurfWaterDepth(jint(j, "max_water_depth", 0)),
        "noise_based_count" => PM::NoiseCount(
            jint(j, "noise_to_count_ratio", 1),
            jnum(j, "noise_factor").unwrap_or(1.0),
            jnum(j, "noise_offset").unwrap_or(0.0),
        ),
        "noise_threshold_count" => PM::NoiseThrCount(
            jnum(j, "noise_level").unwrap_or(0.0),
            jint(j, "below_noise", 0),
            jint(j, "above_noise", 0),
        ),
        "random_chance" => PM::RandomChance(jf32(j, "chance", 0.0)),
        "environment_scan" => PM::EnvScan(
            if jstr(j, "direction_of_search").as_deref() == Some("up") { 1 } else { 0 },
            js(j, "allowed_search_condition").and_then(p_pred),
            p_pred(js(j, "target_condition")?)?,
            jint(j, "max_steps", 16),
        ),
        "carving_mask" => PM::Carving,
        _ => return None,
    })
}

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
    let mut order: Vec<Vec<(u32, String)>> = vec![vec![]; max_steps];
    for step in 0..max_steps {
        let mut nodes: Vec<String> = vec![];
        let mut idx_of: HashMap<String, usize> = HashMap::new();
        let mut edges: Vec<(usize, usize)> = vec![];
        for b in OW_BIOMES {
            let Some(steps) = reg.bsteps.get(*b) else { continue };
            let Some(list) = steps.get(step) else { continue };
            let mut enc: HashMap<String, usize> = HashMap::new();
            let mut ordered: Vec<&String> = vec![];
            for f in list {
                if !enc.contains_key(f) {
                    enc.insert(f.clone(), ordered.len());
                    ordered.push(f);
                    if !idx_of.contains_key(f) {
                        idx_of.insert(f.clone(), nodes.len());
                        nodes.push(f.clone());
                    }
                }
            }
            for w in ordered.windows(2) {
                edges.push((idx_of[w[0]], idx_of[w[1]]));
            }
        }
        let n = nodes.len();
        let mut indeg = vec![0usize; n];
        let mut adj: Vec<Vec<usize>> = vec![vec![]; n];
        for (a, b) in edges {
            adj[a].push(b);
            indeg[b] += 1;
        }
        let mut queue: Vec<usize> = (0..n).filter(|&i| indeg[i] == 0).collect();
        let mut out = vec![];
        while let Some(i) = queue.first().copied() {
            queue.remove(0);
            out.push(i);
            for &m in &adj[i] {
                indeg[m] -= 1;
                if indeg[m] == 0 {
                    queue.push(m);
                }
            }
        }
        if out.len() < n {
            for i in 0..n {
                if !out.contains(&i) {
                    out.push(i);
                }
            }
        }
        order[step] = out
            .iter()
            .enumerate()
            .map(|(gi, &ni)| (gi as u32, nodes[ni].clone()))
            .collect();
    }
    reg.order = order;
    reg.steps = max_steps;
    reg
}

static FREG: std::sync::OnceLock<FReg> = std::sync::OnceLock::new();
fn freg() -> &'static FReg {
    FREG.get_or_init(load_freg)
}

// ---------- generation context ----------

pub(crate) struct FC<'a> {
    g: &'a mut Gen,
    wr: HashMap<(i32, i32, i32), u32>,
    rc: HashMap<(i32, i32, i32), u32>,   // terrain read-through cache
    ws_t: HashMap<(i32, i32), i32>,
    oc_t: HashMap<(i32, i32), i32>,
    bc: HashMap<(i32, i32, i32), Option<String>>, // biome cache per quart
}
impl<'a> FC<'a> {
    pub(crate) fn get(&mut self, x: i32, y: i32, z: i32) -> u32 {
        if let Some(&s) = self.wr.get(&(x, y, z)) {
            return s;
        }
        if let Some(&s) = self.rc.get(&(x, y, z)) {
            return s;
        }
        let st = self.g.terrain_state(x, y, z);
        self.rc.insert((x, y, z), st);
        st
    }
    pub(crate) fn set(&mut self, x: i32, y: i32, z: i32, st: u32) {
        self.wr.insert((x, y, z), st);
        if let Some(reg) = breg() {
            if !reg.is_air(st) {
                *self.ws_t.entry((x, z)).or_insert(i32::MIN) =
                    (*self.ws_t.entry((x, z)).or_insert(i32::MIN)).max(y + 1);
                if reg.is_solid(st) {
                    *self.oc_t.entry((x, z)).or_insert(i32::MIN) =
                        (*self.oc_t.entry((x, z)).or_insert(i32::MIN)).max(y + 1);
                }
            }
        }
    }
    pub(crate) fn is_air(&mut self, x: i32, y: i32, z: i32) -> bool {
        let st = self.get(x, y, z);
        breg().map(|r| r.is_air(st)).unwrap_or(st == 0)
    }
    pub(crate) fn hm_ws(&mut self, x: i32, z: i32) -> i32 {
        let base = self.g.top_ws(x, z);
        self.ws_t.get(&(x, z)).copied().unwrap_or(i32::MIN).max(base)
    }
    pub(crate) fn hm_oc(&mut self, x: i32, z: i32) -> i32 {
        let base = self.g.top_oc(x, z);
        self.oc_t.get(&(x, z)).copied().unwrap_or(i32::MIN).max(base)
    }
    pub(crate) fn biome(&mut self, x: i32, y: i32, z: i32) -> Option<String> {
        let q = (x >> 2, y >> 2, z >> 2);
        if let Some(b) = self.bc.get(&q) {
            return b.clone();
        }
        let b = self.g.biome_name_at(x, y, z);
        self.bc.insert(q, b.clone());
        b
    }
    pub(crate) fn out_of_h(&self, y: i32) -> bool {
        y < self.g.min_y || y >= self.g.min_y + self.g.height
    }
}

// ---------- predicate / survival ----------

pub(crate) fn dir_off(d: u8) -> (i32, i32, i32) {
    match d {
        0 => (0, -1, 0),
        1 => (0, 1, 0),
        2 => (0, 0, -1),
        3 => (0, 0, 1),
        4 => (-1, 0, 0),
        _ => (1, 0, 0),
    }
}
fn fluid_of(reg: &BReg, st: u32) -> &str {
    reg.name_of(st)
}
pub(crate) fn is_water_st(reg: &BReg, st: u32) -> bool {
    reg.name_of(st) == "minecraft:water"
}

fn is_dirtish(reg: &BReg, st: u32) -> bool {
    let n = reg.name_of(st);
    matches!(
        n,
        "minecraft:grass_block" | "minecraft:mycelium" | "minecraft:podzol"
            | "minecraft:moss_block" | "minecraft:pale_moss_block" | "minecraft:mud"
            | "minecraft:muddy_mangrove_roots" | "minecraft:farmland"
            | "minecraft:dirt_path" | "minecraft:rooted_dirt" | "minecraft:coarse_dirt"
    ) || reg.in_tag(st, "minecraft:dirt")
}
fn can_place(reg: &BReg, ctx: &mut FC, name: &str, x: i32, y: i32, z: i32) -> bool {
    let below = ctx.get(x, y - 1, z);
    let at = ctx.get(x, y, z);
    match name {
        "minecraft:cactus" => {
            let base_ok = matches!(
                reg.name_of(below),
                "minecraft:cactus" | "minecraft:sand" | "minecraft:red_sand"
            ) || reg.in_tag(below, "minecraft:sand");
            if !base_ok {
                return false;
            }
            for (dx, dz) in [(-1i32, 0i32), (1, 0), (0, -1), (0, 1)] {
                let s = ctx.get(x + dx, y, z + dz);
                if reg.is_solid(s) || reg.name_of(s) == "minecraft:lava" {
                    return false;
                }
            }
            true
        }
        "minecraft:sugar_cane" => {
            if reg.name_of(below) == "minecraft:sugar_cane" {
                return true;
            }
            if !(is_dirtish(reg, below)
                || reg.in_tag(below, "minecraft:sand")
                || reg.in_tag(below, "minecraft:moss_replaceable"))
            {
                return false;
            }
            for (dx, dz) in [(-1i32, 0i32), (1, 0), (0, -1), (0, 1)] {
                let s = ctx.get(x + dx, y - 1, z + dz);
                if is_water_st(reg, s) || reg.name_of(s) == "minecraft:frosted_ice" {
                    return true;
                }
                let s2 = ctx.get(x + dx, y, z + dz);
                if reg.get_prop(s2, "waterlogged").as_deref() == Some("true") {
                    return true;
                }
            }
            false
        }
        "minecraft:bamboo" | "minecraft:bamboo_sapling" => {
            reg.in_tag(below, "minecraft:bamboo_plantable_on")
                || reg.in_tag(below, "minecraft:dirt")
                || reg.in_tag(below, "minecraft:sand")
                || reg.name_of(below) == "minecraft:gravel"
                || reg.name_of(below) == "minecraft:bamboo"
                || reg.name_of(below) == "minecraft:bamboo_sapling"
        }
        "minecraft:kelp" | "minecraft:kelp_plant" => {
            is_water_st(reg, at)
                && (reg.is_solid(below) || reg.name_of(below) == "minecraft:kelp")
        }
        "minecraft:seagrass" | "minecraft:tall_seagrass" => {
            is_water_st(reg, at)
                && (reg.side_solid(below, 1)
                    || reg.name_of(below) == "minecraft:magma_block")
        }
        "minecraft:sea_pickle" => is_water_st(reg, at) && reg.is_solid(below),
        "minecraft:small_dripleaf" => {
            is_water_st(reg, at)
                && matches!(reg.name_of(below), "minecraft:clay" | "minecraft:moss_block")
        }
        "minecraft:big_dripleaf" | "minecraft:big_dripleaf_stem" => {
            let bn = reg.name_of(below);
            matches!(bn, "minecraft:clay" | "minecraft:moss_block" | "minecraft:big_dripleaf_stem")
                || is_dirtish(reg, below)
        }
        "minecraft:hanging_roots" | "minecraft:cave_vines" | "minecraft:cave_vines_plant" => {
            let above = ctx.get(x, y + 1, z);
            reg.side_solid(above, 0)
                || matches!(
                    reg.name_of(above),
                    "minecraft:cave_vines" | "minecraft:cave_vines_plant"
                        | "minecraft:hanging_roots" | "minecraft:azalea_leaves"
                )
        }
        "minecraft:mangrove_propagule" => {
            let an = reg.name_of(ctx.get(x, y + 1, z));
            an.contains("leaves")
                || an.contains("log")
                || is_dirtish(reg, below)
                || reg.name_of(below) == "minecraft:clay"
        }
        "minecraft:brown_mushroom" | "minecraft:red_mushroom" => {
            is_dirtish(reg, below)
                || matches!(
                    reg.name_of(below),
                    "minecraft:podzol" | "minecraft:mycelium"
                        | "minecraft:crimson_nylium" | "minecraft:warped_nylium"
                        | "minecraft:soul_soil" | "minecraft:basalt"
                )
        }
        _ => {
            // BushBlock family (saplings, flowers, grasses, ferns, bushes,
            // petals, dead_bush, torchflower...): dirt-tag or farmland below
            is_dirtish(reg, below) || reg.name_of(below) == "minecraft:farmland"
        }
    }
}

fn test_pred(pr: &Pr, reg: &BReg, ctx: &mut FC, x: i32, y: i32, z: i32) -> bool {
    match pr {
        Pr::True => true,
        Pr::Blocks(names) => {
            let st = ctx.get(x, y, z);
            names.iter().any(|t| t == reg.name_of(st))
        }
        Pr::Tag(tag) => {
            let st = ctx.get(x, y, z);
            reg.in_tag(st, tag)
        }
        Pr::Fluids(names) => {
            let st = ctx.get(x, y, z);
            let f = fluid_of(reg, st);
            names.iter().any(|t| t == f)
        }
        Pr::Solid => reg.is_solid(ctx.get(x, y, z)),
        Pr::Replaceable => reg.is_replaceable(ctx.get(x, y, z)),
        Pr::WouldSurvive => {
            let st = ctx.get(x, y, z);
            let n = reg.name_of(st);
            can_place(reg, ctx, n, x, y, z)
        }
        Pr::InsideWB => !ctx.out_of_h(y),
        Pr::HasSturdy(off, d) => {
            let st = ctx.get(x + off[0], y + off[1], z + off[2]);
            reg.side_solid(st, *d as usize)
        }
        Pr::Unobstructed => {
            let top = ctx.g.min_y + ctx.g.height - 1;
            let mut yy = y + 1;
            let limit = top.min(y + 320);
            while yy <= limit {
                if !ctx.is_air(x, yy, z) {
                    return false;
                }
                yy += 1;
            }
            true
        }
        Pr::Any(v) => v.iter().any(|p| test_pred(p, reg, ctx, x, y, z)),
        Pr::All(v) => v.iter().all(|p| test_pred(p, reg, ctx, x, y, z)),
        Pr::Not(p) => !test_pred(p, reg, ctx, x, y, z),
    }
}

fn test_rt(rt: &RT, reg: &BReg, st: u32, y: i32, r: &mut Xor) -> bool {
    match rt {
        RT::True => true,
        RT::Tag(t) => reg.in_tag(st, t),
        RT::Block(b) => reg.name_of(st) == b.as_str(),
        RT::BlockState(s) => st == *s,
        RT::Any(v) => v.iter().any(|t| test_rt(t, reg, st, y, r)),
        RT::All(v) => v.iter().all(|t| test_rt(t, reg, st, y, r)),
        RT::Not(t) => !test_rt(t, reg, st, y, r),
        RT::Height(lo, hi) => y >= *lo && y <= *hi,
        RT::RandBlock(b, p) => reg.name_of(st) == b.as_str() && r.next_f32() < *p,
        RT::RandBlockState(s, p) => st == *s && r.next_f32() < *p,
    }
}

fn foliage_noise() -> &'static OSimp {
    static N: std::sync::OnceLock<OSimp> = std::sync::OnceLock::new();
    N.get_or_init(|| OSimp::new_l(&mut Lcg::new(2345), &[0]))
}

// ---------- placed feature execution ----------

fn gen_pf(pf: &Pf, fr: &'static FReg, ctx: &mut FC, rng: &mut Xor, pos: (i32, i32, i32)) {
    let mut cur: Vec<(i32, i32, i32)> = vec![pos];
    for m in &pf.mods {
        let mut next = Vec::new();
        for p in &cur {
            mod_positions(m, fr, ctx, rng, *p, pf, &mut next);
        }
        cur = next;
        if cur.is_empty() {
            return;
        }
    }
    for p in cur {
        gen_cf_ref(&pf.feat, fr, ctx, rng, p);
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
        let hh = rng.next_f64() * j as f64 / 16.0;
        ds[k as usize * 4 + 3] =
            (((std::f32::consts::PI * f).sin() + 1.0) * hh as f32 + 1.0) as f64 / 2.0;
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

pub(crate) fn mkprops(pairs: &[(&str, &str)]) -> J {
    J::Obj(pairs.iter().map(|(k, v)| (k.to_string(), J::Str(v.to_string()))).collect())
}

fn gen_cf(cf: &Cf, fr: &'static FReg, ctx: &mut FC, rng: &mut Xor, pos: (i32, i32, i32)) {
    match cf {
        Cf::Unknown(_) => {}
        Cf::Tree(t) => {
            crate::tree::gen_tree(ctx, rng, pos, t);
        }
        Cf::Ore { size, discard, targets, scattered } => {
            if *scattered {
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
                        if ore_should_place(*discard, ctx, rt, tp, rng) {
                            ctx.set(tp.0, tp.1, tp.2, *st);
                            break;
                        }
                    }
                }
                return;
            }
            let f = rng.next_f32() * std::f32::consts::PI;
            let g = *size as f32 / 8.0;
            let i = ((*size as f32 / 16.0 * 2.0 + 1.0) / 2.0).ceil() as i32;
            let d = pos.0 as f64 + f.sin() as f64 * g as f64;
            let e = pos.0 as f64 - f.sin() as f64 * g as f64;
            let h = pos.2 as f64 + f.cos() as f64 * g as f64;
            let j = pos.2 as f64 - f.cos() as f64 * g as f64;
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
                        gen_vein(ctx, rng, d, e, h, j, l, mm, n, o, p, q, r2, *size, *discard, targets);
                        return;
                    }
                }
            }
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
pub(crate) fn populate(g: &mut Gen, sx: i32, sz: i32) {
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
    let mut ctx = FC {
        g,
        wr: HashMap::new(),
        rc: HashMap::new(),
        ws_t: HashMap::new(),
        oc_t: HashMap::new(),
        bc: HashMap::new(),
    };
    let trace = std::env::var("FEAT_TRACE").is_ok();
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
            gen_pf(pf, fr, &mut ctx, &mut rng, (sx * 16, 0, sz * 16));
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
