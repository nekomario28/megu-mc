// generation_providers.rs — integer/height/block-state providers, placement
// modifiers and block predicates. Parsed from datapack JSON and evaluated
// against a generation context; owns the vanilla value and RNG-consumption
// semantics, not placement orchestration or tree generation.

use crate::block_registry::{breg, bs_parts, ns_name, reg_props_axis, BReg, HeightmapKind};
use crate::generation_context::FC;
use crate::generation_math::{normal_new_l, Normal, Xor};
use crate::json::{jarr, jf32, jint, jnum, js, jstr, J};

// ---------- providers ----------

#[derive(Clone, Debug)]
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
                // BiasedToBottomInt (bpo): the second bound depends on the first draw.
                let bound = r.next_bound(b - a + 1) + 1;
                a + r.next_bound(bound)
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
            // `ClampedNormalInt.sample` (`bpr.a(ayw)`): `(int) Mth.clamp(Mth.nextGaussian(
            // random, mean, deviation), minInclusive, maxInclusive)`. Three details are the
            // behaviour, and all three were wrong here before:
            //
            // * `Mth.nextGaussian(random, mean, deviation)` (`ayo.c`) narrows the gaussian to
            //   **f32** *before* the multiply and does `mean + (float)g * deviation` in f32:
            //   `d2f` then `fmul` then `fadd`, with `mean`/`deviation` being the record's
            //   `float` fields (double arithmetic lands on a different cell at a boundary).
            // * the clamp is `Mth.clamp(float,float,float)` (`ayo.a(FFF)`: `v < min ? min :
            //   Math.min(v, max)`), i.e. **before** the cast, not after a rounding.
            // * the cast is `f2i` - truncation toward zero. `f64::round` moved every offset
            //   whose fraction reached .5 by one cell, which flipped `pointed_dripstone`'s
            //   `random_offset` candidates and, through the biome filter, the whole
            //   placement stream (47 cells at jungle (74,-41), seed 4242).
            IP::Norm(mean, dev, lo, hi) => {
                let v = (*mean as f32) + (r.next_gauss() as f32) * (*dev as f32);
                let v = if v < *lo as f32 {
                    *lo as f32
                } else {
                    v.min(*hi as f32)
                };
                v as i32
            }
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
    /// `IntProvider.getMaxValue()`: the largest value this provider can return.
    /// The geode's layer thresholds are `1/sqrt(layer + points/max)`, so this is a
    /// real input rather than a bound for validation. Each arm mirrors the vanilla
    /// provider's own `getMaxValue` (checked in the bytecode for `ClampedNormalInt`,
    /// which returns its declared maximum rather than mean + 3 deviation: `b()`
    /// loads field `h` and returns).
    /// The provider's own minimum (`getMinValue`), for the `Mth.clamp` the dripstone
    /// features do on a value derived from the cave's height.
    pub(crate) fn min_inclusive(&self) -> i32 {
        match self {
            IP::Const(v) => *v,
            IP::Uniform(a, _)
            | IP::BiasBottom(a, _)
            | IP::VeryBias(a, _, _)
            | IP::Trap(a, _, _) => *a,
            IP::Clamped(_, a, _) => *a,
            IP::Norm(_, _, a, _) => *a,
            IP::Weighted(l) => l.iter().map(|(_, p)| p.min_inclusive()).min().unwrap_or(0),
        }
    }
    pub(crate) fn max_inclusive(&self) -> i32 {
        match self {
            IP::Const(v) => *v,
            IP::Uniform(_, b)
            | IP::BiasBottom(_, b)
            | IP::VeryBias(_, b, _)
            | IP::Trap(_, b, _) => *b,
            IP::Clamped(_, _, hi) => *hi,
            IP::Norm(_, _, _, hi) => *hi,
            IP::Weighted(list) => list
                .iter()
                .map(|(_, p)| p.max_inclusive())
                .max()
                .unwrap_or(0),
        }
    }
}

// ---------- float providers (`bpu` + its four types) ----------

/// Float value providers. The dripstone cluster is the first feature in the pack to
/// need one (`density` is a uniform float, `wetness` a clamped normal), so the set is
/// written out in full rather than partially: an unparsed provider type must not look
/// like a config that merely says zero.
///
/// * `minecraft:constant` (`bps`): no draw.
/// * `minecraft:uniform` (`bqb`) = `Mth.nextFloat(random, min, max)`: one `nextFloat`,
///   with `min_inclusive` / `max_exclusive` (not inclusive - the int provider's rule
///   does not carry over).
/// * `minecraft:clamped_normal` (`bpq`) = `Mth.clamp(Mth.nextGaussian(random, mean,
///   deviation), min, max)`: one Gaussian (two draws, or none when the Marsaglia
///   spare is already populated), narrowed to f32 before the multiply and add.
/// * `minecraft:trapezoid` (`bqa`): two `nextFloat`s, `min + r*h + r*g`.
///
/// The shared piece of `minecraft:clamped_normal`: `Mth.clamp(Mth.nextGaussian(random,
/// mean, deviation), min, max)`. `Mth.nextGaussian(RandomSource, float, float)` narrows
/// the Gaussian with `d2f` before `fmul`/`fadd`; the dripstone cluster's own tip length
/// calls the same helper, so it lives here rather than in the feature.
pub(crate) fn fp_clamped_normal(r: &mut Xor, mean: f32, dev: f32, lo: f32, hi: f32) -> f32 {
    let v = mean + (r.next_gauss() as f32) * dev;
    if v < lo {
        lo
    } else {
        v.min(hi)
    }
}

#[derive(Clone)]
pub(crate) enum FP {
    Const(f32),
    Uniform(f32, f32),
    Norm {
        mean: f32,
        dev: f32,
        lo: f32,
        hi: f32,
    },
    Trap {
        lo: f32,
        hi: f32,
        plateau: f32,
    },
}
impl FP {
    pub(crate) fn get(&self, r: &mut Xor) -> f32 {
        match self {
            FP::Const(v) => *v,
            FP::Uniform(lo, hi) => crate::generation_math::mth_next_f32(r, *lo, *hi),
            FP::Norm { mean, dev, lo, hi } => fp_clamped_normal(r, *mean, *dev, *lo, *hi),
            FP::Trap { lo, hi, plateau } => {
                // bqa: f = max - min; g = (f - plateau)/2; h = f - g
                let f = hi - lo;
                let g = (f - plateau) / 2.0;
                let h = f - g;
                lo + r.next_f32() * h + r.next_f32() * g
            }
        }
    }
}
pub(crate) fn p_fp(j: &J) -> Option<FP> {
    let ty = jstr(j, "type")?;
    let ty = ty.strip_prefix("minecraft:").unwrap_or(&ty);
    Some(match ty {
        "constant" | "literal" => FP::Const(jnum(j, "value").unwrap_or(0.0) as f32),
        "uniform" => FP::Uniform(
            jnum(j, "min_inclusive").unwrap_or(0.0) as f32,
            jnum(j, "max_exclusive").unwrap_or(1.0) as f32,
        ),
        "clamped_normal" => FP::Norm {
            mean: jnum(j, "mean").unwrap_or(0.0) as f32,
            dev: jnum(j, "deviation").unwrap_or(1.0) as f32,
            lo: jnum(j, "min").unwrap_or(0.0) as f32,
            hi: jnum(j, "max").unwrap_or(0.0) as f32,
        },
        "trapezoid" => FP::Trap {
            lo: jnum(j, "min").unwrap_or(0.0) as f32,
            hi: jnum(j, "max").unwrap_or(0.0) as f32,
            plateau: jnum(j, "plateau").unwrap_or(0.0) as f32,
        },
        _ => return None,
    })
}

pub(crate) fn p_ip(j: &J) -> Option<IP> {
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
pub(crate) enum YA {
    Abs(i32),
    AboveBottom(i32),
    BelowTop(i32),
}
impl YA {
    fn get(&self, min_y: i32, height: i32) -> i32 {
        match self {
            YA::Abs(y) => *y,
            YA::AboveBottom(o) => min_y.saturating_add(*o),
            // `VerticalAnchor.BELOW_TOP` resolves to minY + height - 1 - value:
            // the top *block* is at minY + height - 1.
            YA::BelowTop(o) => min_y
                .saturating_add(height)
                .saturating_sub(1)
                .saturating_sub(*o),
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
pub(crate) enum HP {
    /// `minecraft:uniform` = `eia`: one `nextInt(max - min + 1)`.
    Uniform(YA, YA),
    /// `minecraft:biased_to_bottom` = `ehv` (inner defaults to 1): two draws.
    Biased(YA, YA, i32),
    /// `minecraft:very_biased_to_bottom` = `eib` (inner defaults to 1): three draws.
    VeryBias(YA, YA, i32),
    /// `minecraft:trapezoid` = `ehz` (plateau defaults to 0): two draws.
    Trap(YA, YA, i32),
}
impl HP {
    pub(crate) fn get(&self, r: &mut Xor, min_y: i32, height: i32) -> i32 {
        match self {
            HP::Uniform(a, b) => r.next_inbetween(a.get(min_y, height), b.get(min_y, height)),
            HP::Biased(a, b, inner) => {
                // ehv: k = nextInt(hi - lo - inner + 1); return nextInt(k + inner) + lo
                let lo = a.get(min_y, height);
                let hi = b.get(min_y, height);
                if hi - lo - inner < 0 {
                    return lo;
                }
                let k = r.next_bound(hi - lo - inner + 1);
                r.next_bound(k + inner) + lo
            }
            HP::Trap(a, b, plateau) => {
                // ehz: if plateau >= range -> uniform; else lo + rbi(0, other) + rbi(0, half)
                let lo = a.get(min_y, height);
                let hi = b.get(min_y, height);
                if lo > hi {
                    return lo;
                }
                let range = hi - lo;
                if *plateau >= range {
                    return r.next_inbetween(lo, hi);
                }
                let half = (range - plateau) / 2;
                let other = range - half;
                lo + r.next_inbetween(0, other) + r.next_inbetween(0, half)
            }
            HP::VeryBias(a, b, inner) => {
                // eib: k = rbi(lo + inner, hi); m = rbi(lo, k - 1);
                //      return rbi(lo, m - 1 + inner)
                let lo = a.get(min_y, height);
                let hi = b.get(min_y, height);
                if hi - lo - inner < 0 {
                    return lo;
                }
                let k = r.next_inbetween(lo + inner, hi);
                let m = r.next_inbetween(lo, k - 1);
                r.next_inbetween(lo, m - 1 + inner)
            }
        }
    }
}
pub(crate) fn p_hp(j: &J) -> Option<HP> {
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
    // `inner` defaults to 1 (biased_to_bottom / very_biased_to_bottom);
    // `plateau` defaults to 0 (trapezoid).
    let inner = jnum(j, "inner").map(|v| v as i32).unwrap_or(1);
    let plateau = jnum(j, "plateau").map(|v| v as i32).unwrap_or(0);
    Some(match ty {
        "uniform" => HP::Uniform(lo, hi),
        "trapezoid" => HP::Trap(lo, hi, plateau),
        "biased_to_bottom" => HP::Biased(lo, hi, inner),
        "very_biased_to_bottom" => HP::VeryBias(lo, hi, inner),
        _ => return None,
    })
}

// ---------- block state providers ----------

pub(crate) enum Bsp {
    Simple(u32),
    Rotated {
        base: u32,
        axis: bool,
    },
    Weighted(Vec<(i32, u32)>),
    RandomInt {
        src: Box<Bsp>,
        prop: String,
        vals: Option<IP>,
    },
    Noise {
        n: Normal,
        scale: f64,
        states: Vec<u32>,
    },
    NoiseThr {
        n: Normal,
        scale: f64,
        thr: f32,
        hchance: f32,
        def: u32,
        low: Vec<u32>,
        high: Vec<u32>,
    },
    Dual {
        n: Normal,
        scale: f64,
        variety: (i32, i32),
        slow_scale: f64,
        slow_variety: (i32, i32),
        states: Vec<u32>,
    },
    RuleBased {
        fb: Option<Box<Bsp>>,
        rules: Vec<(Pr, Bsp)>,
    },
}
fn noise_state(states: &[u32], v: f64) -> u32 {
    // vanilla: midpoint(1.0, v) -> index into the list
    let unit = ((1.0 + v) / 2.0).clamp(0.0, 0.9999);
    let slot = (unit * states.len() as f64) as usize;
    states[slot]
}
impl Bsp {
    pub(crate) fn get(&self, ctx: &mut FC, r: &mut Xor, x: i32, y: i32, z: i32) -> u32 {
        match self {
            Bsp::Simple(s) => *s,
            Bsp::Rotated { base, axis } => {
                if !*axis {
                    *base
                } else {
                    let ax = match r.next_bound(3) {
                        0 => "x",
                        1 => "y",
                        _ => "z",
                    };
                    breg()
                        .map(|reg| reg.with_prop(*base, "axis", ax))
                        .unwrap_or(*base)
                }
            }
            Bsp::Weighted(list) => {
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
            Bsp::RandomInt { src, prop, vals } => {
                let st = src.get(ctx, r, x, y, z);
                if let Some(vals) = vals {
                    let value = vals.get(r);
                    if let Some(reg) = breg() {
                        return reg.with_prop(st, prop, &value.to_string());
                    }
                }
                st
            }
            Bsp::Noise { n, scale, states } => {
                if states.is_empty() {
                    return 0;
                }
                let v = n.get(x as f64 * scale, y as f64 * scale, z as f64 * scale);
                noise_state(states, v)
            }
            Bsp::NoiseThr {
                n,
                scale,
                thr,
                hchance,
                def,
                low,
                high,
            } => {
                let v = n.get(x as f64 * scale, y as f64 * scale, z as f64 * scale);
                if v < *thr as f64 {
                    if low.is_empty() {
                        return *def;
                    }
                    return low[r.next_bound(low.len() as i32) as usize];
                }
                if r.next_f32() < *hchance && !high.is_empty() {
                    return high[r.next_bound(high.len() as i32) as usize];
                }
                *def
            }
            Bsp::Dual {
                n,
                scale,
                variety,
                slow_scale,
                slow_variety: _sv,
                states,
            } => {
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
                let bucket =
                    (((1.0 + sv) / 2.0) * span.max(1) as f64 + variety.0 as f64).floor() as i32;
                let bucket = bucket.clamp(0, span.max(1) - 1);
                let per = states.len() as i32 / span.max(1).max(1);
                let lo = (bucket * per).clamp(0, states.len() as i32 - 1) as usize;
                let hi = ((bucket + 1) * per).clamp(lo as i32 + 1, states.len() as i32) as usize;
                let sub = &states[lo..hi];
                let v = n.get(x as f64 * scale, y as f64 * scale, z as f64 * scale);
                noise_state(sub, v)
            }
            Bsp::RuleBased { fb, rules } => {
                if let Some(reg) = breg() {
                    for (cond, prov) in rules {
                        if test_pred(cond, reg, ctx, x, y, z) {
                            return prov.get(ctx, r, x, y, z);
                        }
                    }
                }
                fb.as_ref()
                    .map(|f| f.get(ctx, r, x, y, z))
                    .unwrap_or_else(|| ctx.get(x, y, z))
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
pub(crate) fn p_bsp(j: &J) -> Option<Bsp> {
    if let Some((n, p)) = bs_parts(j) {
        if js(j, "type").is_none() || js(j, "id").is_some() && jstr(j, "type").is_none() {
            return Some(Bsp::Simple(breg()?.state_of(&n, p)));
        }
    }
    // 1.21.1 writes the rule-based provider as `{"fallback": ..., "rules": [...]}`
    // with no `type` field; 1.21.2+ names it `minecraft:rule_based_state_provider`.
    // Without this the whole *feature* fails to parse and is dropped silently.
    let ty = match jstr(j, "type") {
        Some(t) => t,
        None if js(j, "fallback").is_some() || js(j, "rules").is_some() => "rule_based".to_string(),
        None => return None,
    };
    let ty = ty.strip_prefix("minecraft:").unwrap_or(&ty);
    Some(match ty {
        "simple_state_provider" => p_bsp(js(j, "state")?)?,
        "rotated_block_provider" => {
            // vanilla RotatedBlockProvider randomizes the axis
            let (n, p) = bs_parts(js(j, "state")?)?;
            Bsp::Rotated {
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
            Bsp::Weighted(v)
        }
        "randomized_int_state_provider" => {
            // 1.21.1 has an IntProvider here, not a list of equally likely values.
            // Keep the older converted array spelling with the same draw semantics.
            let vals = match js(j, "values") {
                Some(J::Arr(vs)) if vs.is_empty() => None,
                Some(J::Arr(vs)) => Some(IP::Weighted(
                    vs.iter().map(|v| (1, IP::Const(v.num() as i32))).collect(),
                )),
                Some(provider) => Some(p_ip(provider)?),
                None => None,
            };
            Bsp::RandomInt {
                src: Box::new(p_bsp(js(j, "source")?)?),
                prop: jstr(j, "property")?,
                vals,
            }
        }
        "noise_provider" => Bsp::Noise {
            n: noise_j(j)?,
            scale: jnum(j, "scale").unwrap_or(1.0),
            states: st_list(j, "states"),
        },
        "noise_threshold_provider" => {
            let (dn, dp) = bs_parts(js(j, "default_state")?)?;
            Bsp::NoiseThr {
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
            Bsp::Dual {
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
            Bsp::RuleBased {
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
    WouldSurvive(String),
    InsideWB,
    HasSturdy([i32; 3], u8),
    Unobstructed,
    Any(Vec<Pr>),
    All(Vec<Pr>),
    Not(Box<Pr>),
    Offset(Box<Pr>, [i32; 3]),
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
pub(crate) fn p_pred(j: &J) -> Option<Pr> {
    let ty = jstr(j, "type")?;
    let ty = ty.strip_prefix("minecraft:").unwrap_or(&ty);
    let pred = match ty {
        "true" | "always_true" => Pr::True,
        "matching_blocks" | "block_match" | "blockstate_match" => {
            let mut v = vec![];
            if let Some(bs) = jarr(j, "blocks") {
                for b in bs {
                    // entries may be plain names or block-state objects
                    v.push(if let Some((n, _)) = bs_parts(b) {
                        n
                    } else {
                        ns_name(b.str())
                    });
                }
            } else if let Some(b) = jstr(j, "blocks")
                .or_else(|| jstr(j, "block"))
                .or_else(|| jstr(j, "id"))
            {
                // `"blocks": "minecraft:air"` is a single value, not a list - the
                // nested `patch_grass` predicate is written this way, and an empty
                // match set made every grass patch unplaceable
                v.push(ns_name(&b));
            }
            Pr::Blocks(v)
        }
        "matching_block_tag" | "tag_match" => Pr::Tag(jstr(j, "tag")?),
        "matching_fluids" => {
            let mut v = vec![];
            if let Some(fs) = jarr(j, "fluids") {
                for f in fs {
                    // the 1.21.1 data writes plain names here, the 1.21.2 one
                    // block-state-shaped objects
                    v.push(if let Some((n, _)) = bs_parts(f) {
                        n
                    } else {
                        ns_name(f.str())
                    });
                }
            } else if let Some(f) = jstr(j, "fluids").or_else(|| jstr(j, "fluid")) {
                // `"fluids": "minecraft:water"` is a single value, not a list
                v.push(ns_name(&f));
            }
            Pr::Fluids(v)
        }
        "solid" => Pr::Solid,
        "replaceable" => Pr::Replaceable,
        "not" => Pr::Not(Box::new(p_pred(js(j, "predicate")?)?)),
        "any_of" => Pr::Any(
            jarr(j, "predicates")?
                .iter()
                .map(p_pred)
                .collect::<Option<_>>()?,
        ),
        "all_of" => Pr::All(
            jarr(j, "predicates")?
                .iter()
                .map(p_pred)
                .collect::<Option<_>>()?,
        ),
        "would_survive" => Pr::WouldSurvive(
            js(j, "state")
                .and_then(bs_parts)
                .map(|(n, _)| n)
                .unwrap_or_default(),
        ),
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
    };
    // The native state-testing predicates, survival, bounds and unobstructed
    // predicates evaluate at pos + offset. HasSturdy already owns its offset.
    let offset = off_arr(j);
    let supports_offset = matches!(
        pred,
        Pr::Blocks(_)
            | Pr::Tag(_)
            | Pr::Fluids(_)
            | Pr::Solid
            | Pr::Replaceable
            | Pr::WouldSurvive(_)
            | Pr::InsideWB
            | Pr::Unobstructed
    );
    Some(if supports_offset && offset != [0, 0, 0] {
        Pr::Offset(Box::new(pred), offset)
    } else {
        pred
    })
}

pub(crate) enum RT {
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
pub(crate) fn p_rt(j: &J) -> Option<RT> {
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
        "height_match" => RT::Height(jint(j, "min_inclusive", 0), jint(j, "max_inclusive", 0)),
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

pub(crate) enum PM {
    Count(IP),
    CountEveryLayer(IP),
    InSquare,
    Offset(IP, IP),
    HeightRange(HP),
    Heightmap(HeightmapKind),
    Biome,
    BlockPred(Pr),
    Rarity(i32),
    Fixed,
    SurfaceRel(HeightmapKind, i32, i32), // inclusive offsets; i32::MIN/MAX when absent
    SurfWaterDepth(i32),
    NoiseCount(i32, f64, f64),
    NoiseThrCount(f64, i32, i32),
    RandomChance(f32),
    EnvScan(u8, Option<Pr>, Pr, i32),
    Carving,
}
fn p_heightmap(j: &J) -> Option<HeightmapKind> {
    let name = jstr(j, "heightmap")?;
    let kind = HeightmapKind::parse(&name);
    if kind.is_none() {
        eprintln!("[feature] unsupported heightmap {name}");
    }
    kind
}
pub(crate) fn p_mod(j: &J) -> Option<PM> {
    let ty = jstr(j, "type")?;
    let ty = ty.strip_prefix("minecraft:").unwrap_or(&ty);
    Some(match ty {
        "count" => PM::Count(p_ip(js(j, "count")?)?),
        "count_on_every_layer" => PM::CountEveryLayer(p_ip(js(j, "count")?)?),
        "in_square" => PM::InSquare,
        "offset" | "random_offset" => PM::Offset(
            p_ip(
                js(j, "x_spread")
                    .or_else(|| js(j, "x"))
                    .or_else(|| js(j, "xz_spread"))?,
            )?,
            p_ip(js(j, "y_spread").or_else(|| js(j, "y"))?)?,
        ),
        "height_range" => PM::HeightRange(p_hp(js(j, "height")?)?),
        "heightmap" => PM::Heightmap(p_heightmap(j)?),
        "biome" => PM::Biome,
        // keeps the position (used by the end platform); unparsed it was dropped,
        // which silently changed the modifier count for that feature
        "fixed_placement" => PM::Fixed,
        "block_predicate_filter" => PM::BlockPred(p_pred(js(j, "predicate")?)?),
        "rarity_filter" => PM::Rarity(jint(j, "chance", 1)),
        "surface_relative_threshold_filter" => PM::SurfaceRel(
            p_heightmap(j)?,
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
            if jstr(j, "direction_of_search").as_deref() == Some("up") {
                1
            } else {
                0
            },
            js(j, "allowed_search_condition").and_then(p_pred),
            p_pred(js(j, "target_condition")?)?,
            jint(j, "max_steps", 16),
        ),
        "carving_mask" => PM::Carving,
        _ => return None,
    })
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
pub(crate) fn is_water_st(reg: &BReg, st: u32) -> bool {
    reg.name_of(st) == "minecraft:water"
}

/// `!state.getFluidState().isEmpty()`, through the exported fluid identity: a waterlogged
/// stair answers true, `magma_block` and every other solid answers false. Three 1.21.1 mark
/// gates ask exactly this, not "the block is water or lava".
pub(crate) fn has_fluid_state(reg: &BReg, st: u32) -> bool {
    reg.fluid_name(st) != "minecraft:empty"
}

pub(crate) fn is_dirtish(reg: &BReg, st: u32) -> bool {
    let n = reg.name_of(st);
    matches!(
        n,
        "minecraft:grass_block"
            | "minecraft:mycelium"
            | "minecraft:podzol"
            | "minecraft:moss_block"
            | "minecraft:pale_moss_block"
            | "minecraft:mud"
            | "minecraft:muddy_mangrove_roots"
            | "minecraft:farmland"
            | "minecraft:dirt_path"
            | "minecraft:rooted_dirt"
            | "minecraft:coarse_dirt"
    ) || reg.in_tag(st, "minecraft:dirt")
}
pub(crate) fn can_place(reg: &BReg, ctx: &mut FC, name: &str, x: i32, y: i32, z: i32) -> bool {
    let below = ctx.get(x, y - 1, z);
    let at = ctx.get(x, y, z);
    match name {
        // `RootsBlock` (`dmk`) and `NetherSproutsBlock` (`dld`) override the same
        // `mayPlaceOn`: `state.is(BLOCK_TAGS.NYLIUM) || state.is(SOUL_SOIL) ||
        // super.mayPlaceOn(state, ...)`, and `BushBlock`'s super is
        // `state.is(BLOCK_TAGS.DIRT) || state.is(FARMLAND)`. The `#nylium`-only test
        // this arm used to run was read off `NetherForestVegetationFeature`, which
        // gates the *origin* on `#nylium` before it asks the state to survive, so the
        // feature fixture cannot see the difference: a `patch_crimson_roots` candidate
        // on soul soil kept being refused (`would_survive` -> `canSurvive`).
        "minecraft:crimson_roots" | "minecraft:warped_roots" | "minecraft:nether_sprouts" => {
            reg.in_tag(below, "minecraft:nylium")
                || reg.name_of(below) == "minecraft:soul_soil"
                || reg.in_tag(below, "minecraft:dirt")
                || reg.name_of(below) == "minecraft:farmland"
        }
        // `FungusBlock` (`dje`) adds MYCELIUM to that set (`#nylium || MYCELIUM ||
        // SOUL_SOIL || #dirt || FARMLAND`). Its `grows_on` block (crimson_nylium /
        // warped_nylium) is only the bonemeal gate - `isValidBonemealTarget` -
        // not `canSurvive`, so it does not narrow placement.
        "minecraft:crimson_fungus" | "minecraft:warped_fungus" => {
            reg.in_tag(below, "minecraft:nylium")
                || reg.name_of(below) == "minecraft:mycelium"
                || reg.name_of(below) == "minecraft:soul_soil"
                || reg.in_tag(below, "minecraft:dirt")
                || reg.name_of(below) == "minecraft:farmland"
        }
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
            reg.name_of(below) != "minecraft:magma_block"
                && (reg.side_solid(below, 1)
                    || matches!(
                        reg.name_of(below),
                        "minecraft:kelp" | "minecraft:kelp_plant"
                    ))
        }
        "minecraft:seagrass" | "minecraft:tall_seagrass" => {
            is_water_st(reg, at)
                && (reg.side_solid(below, 1) || reg.name_of(below) == "minecraft:magma_block")
        }
        // WaterlilyBlock.mayPlaceOn checks the source-water fluid type or an
        // IceBlock instance. Frosted ice extends IceBlock. Packed and blue ice do not.
        "minecraft:lily_pad" => {
            !has_fluid_state(reg, at)
                && (reg.fluid_name(below) == "minecraft:water"
                    || matches!(
                        reg.name_of(below),
                        "minecraft:ice" | "minecraft:frosted_ice"
                    ))
        }
        "minecraft:sea_pickle" => is_water_st(reg, at) && reg.is_solid(below),
        // `SmallDripleafBlock.canSurvive` (`dne.a(dtc,dcz,jd)`), measured against the pinned
        // jar by running the native `BlockState.canSurvive` itself: the support is either a
        // `#minecraft:small_dripleaf_placeable` block (clay, moss_block) under *any* cell, or
        // `#minecraft:dirt`/farmland under water. The port required water in the cell *and* a
        // placeable support, which is neither case: it refused an air cell above clay (the
        // dripleaf of a `clay_with_dripleaves` patch) and accepted nothing the reference does
        // not. Measured cells: (69,-7,-493)+(69,-6,-493) were the missing pair.
        "minecraft:small_dripleaf" => {
            reg.in_tag(below, "minecraft:small_dripleaf_placeable")
                || (is_water_st(reg, at)
                    && (reg.in_tag(below, "minecraft:dirt")
                        || reg.name_of(below) == "minecraft:farmland"))
        }
        "minecraft:big_dripleaf" | "minecraft:big_dripleaf_stem" => {
            let bn = reg.name_of(below);
            matches!(
                bn,
                "minecraft:clay" | "minecraft:moss_block" | "minecraft:big_dripleaf_stem"
            ) || is_dirtish(reg, below)
        }
        "minecraft:hanging_roots" | "minecraft:cave_vines" | "minecraft:cave_vines_plant" => {
            let above = ctx.get(x, y + 1, z);
            reg.side_solid(above, 0)
                || matches!(
                    reg.name_of(above),
                    "minecraft:cave_vines"
                        | "minecraft:cave_vines_plant"
                        | "minecraft:hanging_roots"
                        | "minecraft:azalea_leaves"
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
            // `MushroomBlock.canSurvive` (`dla.a(dtc,dcz,jd)`):
            //
            // ```java
            // BlockPos below = pos.below();
            // BlockState support = level.getBlockState(below);
            // return support.is(BlockTags.MUSHROOM_GROW_BLOCK)
            //       ? true
            //       : level.getRawBrightness(pos, 0) < 13 && this.b(support, level, below);
            // ```
            //
            // `this.b` is *`dla`'s own override* - `BlockState.isSolidRender(BlockGetter,
            // BlockPos)` (`dtb$a.i(dcc,jd)`, mappings `1035:1042`) - not `BushBlock.mayPlaceOn`
            // (`dgh.b(dtc,dcc,jd)`, farmland | `#minecraft:dirt`), even though both have the same
            // signature. Measured both ways on the pinned Nether fixture (seed 4242,
            // `(-25,19)+3x3`): the `#minecraft:dirt` reading refuses 24 `brown/red_mushroom_nether`
            // cells the reference places on netherrack, so the solid-render reading is the one the
            // reference implements.
            reg.in_tag(below, "minecraft:mushroom_grow_block")
                || (ctx.feature_brightness(x, y, z) < 13 && reg.is_solid_render(below))
        }
        // `BaseFireBlock.canSurvive`: the block below has a sturdy up face (or the cell
        // is a portal, which a patch cannot be). Not the BushBlock rule: `patch_fire`
        // places on netherrack, which is not in `#minecraft:dirt`.
        "minecraft:fire" => reg.side_solid(below, 1),
        // `SoulFireBlock.canSurvive` is narrower: `#minecraft:soul_fire_base_blocks`
        // (soul sand, soul soil). `patch_soul_fire`'s own predicate already requires
        // soul soil, so this is the second half of the same rule.
        "minecraft:soul_fire" => reg.in_tag(below, "minecraft:soul_fire_base_blocks"),
        // `SporeBlossomBlock.canSurvive` (`dnp.a(dtc,dcz,jd)`): `Block.canSupportCenter(
        // level, pos.above(), DOWN) && !level.isWaterAt(pos)` - it hangs from a ceiling, so the
        // support is the block *above* and its DOWN face. Without an arm the family default
        // asked for dirt *below*, which air never satisfies: native had 7 spore blossoms in the
        // taiga 3x3 and the port had none.
        "minecraft:spore_blossom" => {
            reg.side_solid(ctx.get(x, y + 1, z), 0) && !is_water_st(reg, at)
        }
        _ => {
            // `BushBlock.mayPlaceOn` (`dgh.b(dtc,dcc,jd)`): `state.is(Blocks.FARMLAND)` or
            // `state.is(BlockTags.DIRT)` below, and nothing else. The family is saplings,
            // flowers, grasses, ferns, bushes, petals, torchflower... and its support test is
            // exactly this pair: `#minecraft:dirt` holds dirt/grass_block/podzol/coarse_dirt/
            // mycelium/rooted_dirt/moss_block/mud/muddy_mangrove_roots and *not*
            // `minecraft:dirt_path`, which `is_dirtish` accepts. Measured on the pinned jar
            // (`BlockState.canSurvive` against a scripted level): a sapling on `dirt_path` is
            // `false`, on farmland `true`. Accepting the path let `spruce_checked` spawn trees
            // on taiga village roads (seed 4242 (4,-32)+3x3: (65,70,-498), (71,69,-502)).
            reg.in_tag(below, "minecraft:dirt") || reg.name_of(below) == "minecraft:farmland"
        }
    }
}

pub(crate) fn test_pred(pr: &Pr, reg: &BReg, ctx: &mut FC, x: i32, y: i32, z: i32) -> bool {
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
            let f = reg.fluid_name(st);
            names.iter().any(|t| t == f)
        }
        Pr::Solid => reg.is_solid(ctx.get(x, y, z)),
        Pr::Replaceable => reg.is_replaceable(ctx.get(x, y, z)),
        Pr::WouldSurvive(want) => {
            // vanilla's `would_survive` tests the *configured* state
            // (`state.canSurvive(level, pos)`) at the candidate position, not the
            // block that happens to be there.
            if want.is_empty() {
                false
            } else {
                can_place(reg, ctx, want, x, y, z)
            }
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
        Pr::Offset(p, off) => test_pred(p, reg, ctx, x + off[0], y + off[1], z + off[2]),
    }
}

pub(crate) fn test_rt(rt: &RT, reg: &BReg, st: u32, y: i32, r: &mut Xor) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn j(s: &str) -> J {
        crate::json::jparse(s.as_bytes()).expect("test JSON parses")
    }
    fn rng(seed: u64) -> Xor {
        Xor::seed(seed)
    }
    /// Draw `n` samples from `ip` under one seeded stream.
    fn draws(ip: &IP, seed: u64, n: usize) -> Vec<i32> {
        let mut r = rng(seed);
        (0..n).map(|_| ip.get(&mut r)).collect()
    }

    // ---------- IntProvider ----------

    #[test]
    fn ip_const_returns_value_without_consuming_rng() {
        let ip = IP::Const(-42);
        let mut r = rng(7);
        let before = r.state128();
        assert_eq!(ip.get(&mut r), -42);
        assert_eq!(r.state128(), before, "constant provider draws nothing");
        assert_eq!(ip.min_inclusive(), -42);
        assert_eq!(ip.max_inclusive(), -42);
    }

    #[test]
    fn ip_uniform_stays_in_bounds_and_is_deterministic() {
        for (a, b) in [(0, 10), (-64, 320), (5, 5), (9, 3)] {
            let ip = IP::Uniform(a, b);
            let samples = draws(&ip, 1234, 500);
            for &v in &samples {
                // next_inbetween(min,max) with max<min returns min unclamped;
                // Uniform passes (a,b) through, so a>b yields `a`.
                if a <= b {
                    assert!((a..=b).contains(&v), "uniform({a},{b}) gave {v}");
                } else {
                    assert_eq!(v, a, "inverted uniform returns the min operand");
                }
            }
            if a <= b {
                // A real range actually varies.
                assert!(samples.iter().any(|&v| v != a) || b == a);
            }
            assert_eq!(draws(&ip, 1234, 500), samples, "same seed same stream");
            assert_eq!(ip.min_inclusive(), a);
            assert_eq!(ip.max_inclusive(), b);
        }
        // min == max still consumes its draw (nextInt(1)) but always yields it.
        let ip = IP::Uniform(77, 77);
        assert!(draws(&ip, 9, 20).iter().all(|&v| v == 77));
    }

    #[test]
    fn ip_bias_bottom_skews_low_and_respects_bounds() {
        let ip = IP::BiasBottom(0, 15);
        let samples = draws(&ip, 555, 2000);
        assert!(samples.iter().all(|&v| (0..=15).contains(&v)));
        let zeros = samples.iter().filter(|&&v| v == 0).count();
        let fifteens = samples.iter().filter(|&&v| v == 15).count();
        assert!(
            zeros > fifteens * 2,
            "biased_to_bottom must favour the bottom (zeros={zeros}, fifteens={fifteens})"
        );
        // Degenerate range collapses to the single value.
        let flat = IP::BiasBottom(4, 4);
        assert!(draws(&flat, 1, 50).iter().all(|&v| v == 4));
        assert_eq!(flat.min_inclusive(), 4);
        assert_eq!(flat.max_inclusive(), 4);
        assert_eq!(ip.min_inclusive(), 0);
        assert_eq!(ip.max_inclusive(), 15);
    }

    #[test]
    fn ip_very_bias_is_min_of_three_draws() {
        let ip = IP::VeryBias(0, 100, 1);
        let samples = draws(&ip, 77, 1000);
        assert!(samples.iter().all(|&v| (0..=100).contains(&v)));
        let mean = samples.iter().map(|&v| v as f64).sum::<f64>() / samples.len() as f64;
        assert!(
            mean < 40.0,
            "min-of-3 skews far below the uniform mean 50 (got {mean})"
        );
        assert_eq!(draws(&ip, 77, 1000), samples);
        // Degenerate min == max.
        let ip = IP::VeryBias(9, 9, 1);
        assert!(draws(&ip, 3, 30).iter().all(|&v| v == 9));
    }

    #[test]
    fn ip_clamped_bounds_the_inner_provider() {
        let inner = IP::Uniform(0, 100);
        let ip = IP::Clamped(Box::new(inner), 10, 20);
        let samples = draws(&ip, 42, 500);
        assert!(samples.iter().all(|&v| (10..=20).contains(&v)));
        // A constant outside the clamp range pins to the nearer bound.
        assert_eq!(
            IP::Clamped(Box::new(IP::Const(5)), 10, 20).get(&mut rng(1)),
            10
        );
        assert_eq!(
            IP::Clamped(Box::new(IP::Const(99)), 10, 20).get(&mut rng(1)),
            20
        );
        // Equal clamp bounds collapse everything.
        assert!(
            draws(&IP::Clamped(Box::new(IP::Uniform(0, 9)), 7, 7), 2, 20)
                .iter()
                .all(|&v| v == 7)
        );
        assert_eq!(ip.min_inclusive(), 10);
        assert_eq!(ip.max_inclusive(), 20);
    }

    #[test]
    fn ip_trapezoid_stays_in_range_and_plateau_degenerates() {
        let ip = IP::Trap(0, 10, 4);
        let samples = draws(&ip, 314, 1000);
        assert!(samples.iter().all(|&v| (0..=10).contains(&v)));
        assert_eq!(draws(&ip, 314, 1000), samples);
        // plateau >= range: the reflection window is empty, output is uniform.
        let ip = IP::Trap(0, 10, 10);
        assert!(draws(&ip, 8, 500).iter().all(|&v| (0..=10).contains(&v)));
        // b - a - plateau < 0 returns `a` without drawing.
        let ip = IP::Trap(3, 3, 5);
        let mut r = rng(1);
        let before = r.state128();
        assert_eq!(ip.get(&mut r), 3);
        assert_eq!(r.state128(), before, "degenerate trapezoid draws nothing");
        // min == max collapses.
        assert!(draws(&IP::Trap(6, 6, 0), 4, 30).iter().all(|&v| v == 6));
    }

    #[test]
    fn ip_norm_clamps_gaussian_to_declared_bounds() {
        let ip = IP::Norm(0.0, 1.0, -5, 5);
        let samples = draws(&ip, 2718, 300);
        assert!(samples.iter().all(|&v| (-5..=5).contains(&v)));
        // Huge mean/deviation still cannot escape the clamp: the gaussian can
        // pull the f32 value below lo, so both clamp faces are reachable.
        assert!(draws(&IP::Norm(1e6, 1e6, 0, 10), 5, 50)
            .iter()
            .all(|&v| (0..=10).contains(&v)));
        assert!(draws(&IP::Norm(-1e6, 1.0, 0, 10), 5, 50)
            .iter()
            .all(|&v| v == 0));
        // Zero deviation pins to the clamped mean.
        assert!(draws(&IP::Norm(4.0, 0.0, 3, 7), 5, 20)
            .iter()
            .all(|&v| v == 4));
        assert_eq!(draws(&ip, 2718, 300), samples);
        assert_eq!(ip.min_inclusive(), -5);
        assert_eq!(ip.max_inclusive(), 5);
    }

    #[test]
    fn ip_weighted_selects_by_weight_and_handles_degenerate_weights() {
        // Single entry always wins regardless of roll.
        let ip = IP::Weighted(vec![(1, IP::Const(7))]);
        assert!(draws(&ip, 1, 50).iter().all(|&v| v == 7));
        // Zero/negative total weight yields 0 without touching entries.
        let ip = IP::Weighted(vec![(0, IP::Const(1)), (-3, IP::Const(2))]);
        assert!(draws(&ip, 1, 20).iter().all(|&v| v == 0));
        let ip = IP::Weighted(vec![]);
        assert!(draws(&ip, 1, 20).iter().all(|&v| v == 0));
        // Negative weights count as zero: only the positive arm can fire.
        let ip = IP::Weighted(vec![(-5, IP::Const(1)), (2, IP::Const(9))]);
        assert!(draws(&ip, 6, 50).iter().all(|&v| v == 9));
        // A real distribution produces both arms and is deterministic.
        let ip = IP::Weighted(vec![(1, IP::Const(0)), (3, IP::Const(100))]);
        let samples = draws(&ip, 11, 2000);
        assert!(samples.contains(&0) && samples.contains(&100));
        let zeros = samples.iter().filter(|&&v| v == 0).count();
        assert!(
            (zeros as f64 / samples.len() as f64 - 0.25).abs() < 0.1,
            "weight 1/4 should fire ~25% (got {})",
            zeros as f64 / samples.len() as f64
        );
        assert_eq!(draws(&ip, 11, 2000), samples);
        // Nested providers evaluate per pick.
        let ip = IP::Weighted(vec![(1, IP::Uniform(0, 1)), (1, IP::Const(50))]);
        assert!(draws(&ip, 2, 200).iter().all(|&v| (0..=50).contains(&v)));
        assert_eq!(ip.min_inclusive(), 0);
        assert_eq!(ip.max_inclusive(), 50);
    }

    #[test]
    fn ip_min_max_inclusive_track_every_variant() {
        assert_eq!(IP::Trap(1, 9, 2).min_inclusive(), 1);
        assert_eq!(IP::Trap(1, 9, 2).max_inclusive(), 9);
        assert_eq!(IP::Norm(0.0, 1.0, -3, 8).min_inclusive(), -3);
        assert_eq!(IP::Norm(0.0, 1.0, -3, 8).max_inclusive(), 8);
        assert_eq!(IP::VeryBias(-4, 6, 1).min_inclusive(), -4);
        assert_eq!(IP::VeryBias(-4, 6, 1).max_inclusive(), 6);
        let w = IP::Weighted(vec![
            (1, IP::Const(3)),
            (1, IP::Uniform(-10, 20)),
            (1, IP::Clamped(Box::new(IP::Const(99)), 0, 50)),
        ]);
        assert_eq!(w.min_inclusive(), -10);
        assert_eq!(w.max_inclusive(), 50);
        assert_eq!(IP::Weighted(vec![]).min_inclusive(), 0);
        assert_eq!(IP::Weighted(vec![]).max_inclusive(), 0);
    }

    // ---------- VerticalAnchor / HeightProvider ----------

    #[test]
    fn vertical_anchor_resolves_against_negative_min_y() {
        // 1.21 overworld-style window: min_y=-64, height=384.
        assert_eq!(YA::Abs(70).get(-64, 384), 70);
        assert_eq!(YA::AboveBottom(0).get(-64, 384), -64);
        assert_eq!(YA::AboveBottom(320).get(-64, 384), 256);
        // BELOW_TOP is the top *block*: min_y + height - 1 - offset.
        assert_eq!(YA::BelowTop(0).get(-64, 384), 319);
        assert_eq!(YA::BelowTop(64).get(-64, 384), 255);
        // Sea-level-ish and nether windows resolve the same way.
        assert_eq!(YA::BelowTop(0).get(0, 256), 255);
        assert_eq!(YA::AboveBottom(32).get(0, 128), 32);
        // Saturating arithmetic survives absurd offsets.
        assert_eq!(
            YA::AboveBottom(i32::MAX).get(-64, 384),
            (-64i32).saturating_add(i32::MAX)
        );
        assert_eq!(
            YA::BelowTop(i32::MAX).get(-64, 384),
            319i32.saturating_sub(i32::MAX)
        );
    }

    #[test]
    fn hp_uniform_biased_trap_verybias_bounds_and_degenerates() {
        let (min_y, height) = (-64, 384);
        let lo = YA::AboveBottom(0);
        let hi = YA::BelowTop(0);
        // Uniform over the full window stays inside it and is deterministic.
        let hp = HP::Uniform(lo.clone(), hi.clone());
        let samples = {
            let mut r = rng(123);
            (0..2000)
                .map(|_| hp.get(&mut r, min_y, height))
                .collect::<Vec<_>>()
        };
        assert!(samples.iter().all(|&y| (-64..=319).contains(&y)));
        let mut r2 = rng(123);
        assert_eq!(hp.get(&mut r2, min_y, height), samples[0]);
        // Equal anchors: uniform still draws but always yields the anchor.
        let hp = HP::Uniform(hi.clone(), hi.clone());
        assert!(draws_hp(&hp, 9, 100).iter().all(|&y| y == 319));
        // Inverted anchors return the lower operand without drawing.
        let mut r = rng(1);
        let before = r.state128();
        assert_eq!(
            HP::Uniform(hi.clone(), lo.clone()).get(&mut r, min_y, height),
            319
        );
        assert_eq!(r.state128(), before);
        // Biased: bounded, deterministic, and collapses when the window is
        // smaller than `inner`.
        let hp = HP::Biased(lo.clone(), hi.clone(), 8);
        let samples = draws_hp(&hp, 77, 1000);
        assert!(samples.iter().all(|&y| (-64..=319).contains(&y)));
        assert_eq!(draws_hp(&hp, 77, 1000), samples);
        let collapsed = HP::Biased(YA::Abs(10), YA::Abs(10), 8);
        assert!(draws_hp(&collapsed, 5, 50).iter().all(|&y| y == 10));
        // Trapezoid: plateau >= range degrades to uniform; bounded either way.
        let hp = HP::Trap(lo.clone(), hi.clone(), 10_000);
        assert!(draws_hp(&hp, 31, 500)
            .iter()
            .all(|&y| (-64..=319).contains(&y)));
        let hp = HP::Trap(YA::Abs(0), YA::Abs(50), 0);
        let samples = draws_hp(&hp, 32, 500);
        assert!(samples.iter().all(|&y| (0..=50).contains(&y)));
        // Inverted trapezoid returns `lo`.
        let mut r = rng(2);
        assert_eq!(
            HP::Trap(hi.clone(), lo.clone(), 0).get(&mut r, min_y, height),
            319
        );
        // VeryBias: bounded and deterministic.
        let hp = HP::VeryBias(lo.clone(), hi.clone(), 8);
        let samples = draws_hp(&hp, 55, 1000);
        assert!(samples.iter().all(|&y| (-64..=319).contains(&y)));
        assert_eq!(draws_hp(&hp, 55, 1000), samples);
        let collapsed = HP::VeryBias(YA::Abs(3), YA::Abs(3), 8);
        assert!(draws_hp(&collapsed, 5, 50).iter().all(|&y| y == 3));
    }

    fn draws_hp(hp: &HP, seed: u64, n: usize) -> Vec<i32> {
        let mut r = rng(seed);
        (0..n).map(|_| hp.get(&mut r, -64, 384)).collect()
    }

    // ---------- JSON parsing ----------

    #[test]
    fn p_ip_parses_every_provider_type_and_literal() {
        // Bare number is a constant.
        assert!(matches!(p_ip(&j("5")), Some(IP::Const(5))));
        assert!(matches!(p_ip(&j("-12")), Some(IP::Const(-12))));
        let cases: &[(&str, &str)] = &[
            (r#"{"type":"constant","value":3}"#, "constant"),
            (
                r#"{"type":"minecraft:uniform","min_inclusive":1,"max_inclusive":9}"#,
                "uniform",
            ),
            (
                r#"{"type":"biased_to_bottom","min":0,"max":4}"#,
                "biasbottom",
            ),
            (
                r#"{"type":"very_biased_to_bottom","min_inclusive":0,"max_inclusive":4,"inner":2}"#,
                "verybias",
            ),
            (
                r#"{"type":"clamped","source":{"type":"uniform","min_inclusive":0,"max_inclusive":9},"min_inclusive":2,"max_inclusive":6}"#,
                "clamped",
            ),
            (
                r#"{"type":"trapezoid","min_inclusive":0,"max_inclusive":8,"plateau":3}"#,
                "trap",
            ),
            (
                r#"{"type":"clamped_normal","mean":0.0,"deviation":1.0,"min_inclusive":-2,"max_inclusive":2}"#,
                "norm",
            ),
        ];
        for &(json, tag) in cases {
            let parsed = p_ip(&j(json)).unwrap_or_else(|| panic!("{tag} parses: {json}"));
            match (tag, &parsed) {
                ("constant", IP::Const(3))
                | ("uniform", IP::Uniform(1, 9))
                | ("biasbottom", IP::BiasBottom(0, 4))
                | ("verybias", IP::VeryBias(0, 4, _))
                | ("clamped", IP::Clamped(_, 2, 6))
                | ("trap", IP::Trap(0, 8, 3))
                | ("norm", IP::Norm(..)) => {}
                _ => panic!("{tag} parsed to wrong variant: {parsed:?}"),
            }
        }
        // Weighted list: two entries, one an inline constant.
        let parsed = p_ip(&j(
            r#"{"type":"weighted_list","distribution":[{"weight":1,"data":{"type":"constant","value":1}},{"weight":2,"data":4}]}"#,
        ))
        .unwrap();
        assert!(matches!(parsed, IP::Weighted(ref v) if v.len() == 2));
        // Unknown type and malformed payloads fail closed.
        assert!(p_ip(&j(r#"{"type":"minecraft:no_such_provider"}"#)).is_none());
        assert!(
            p_ip(&j(r#"{"type":"clamped"}"#)).is_none(),
            "missing source"
        );
        assert!(
            p_ip(&j(r#"{"type":"weighted_list"}"#)).is_none(),
            "missing distribution"
        );
        assert!(
            p_ip(&j(r#"{"type":"uniform"}"#)).is_some(),
            "defaults fill missing bounds"
        );
    }

    #[test]
    fn p_hp_parses_height_providers_with_anchor_objects() {
        let hp = p_hp(&j(
            r#"{"type":"uniform","min_inclusive":{"above_bottom":0},"max_inclusive":{"below_top":0}}"#,
        ))
        .unwrap();
        assert!(matches!(
            hp,
            HP::Uniform(YA::AboveBottom(0), YA::BelowTop(0))
        ));
        let hp = p_hp(&j(
            r#"{"type":"trapezoid","min_inclusive":{"absolute":-10},"max_inclusive":80,"plateau":4}"#,
        ))
        .unwrap();
        assert!(matches!(hp, HP::Trap(YA::Abs(-10), YA::Abs(80), 4)));
        let hp = p_hp(&j(
            r#"{"type":"biased_to_bottom","min_inclusive":0,"max_inclusive":32,"inner":3}"#,
        ))
        .unwrap();
        assert!(matches!(hp, HP::Biased(YA::Abs(0), YA::Abs(32), 3)));
        let hp = p_hp(&j(
            r#"{"type":"very_biased_to_bottom","min_inclusive":{"below_top":16},"max_inclusive":{"above_bottom":8}}"#,
        ))
        .unwrap();
        // `inner` defaults to 1 when absent.
        assert!(matches!(
            hp,
            HP::VeryBias(YA::BelowTop(16), YA::AboveBottom(8), 1)
        ));
        // constant height provider.
        let hp = p_hp(&j(r#"{"type":"constant","value":{"absolute":42}}"#)).unwrap();
        assert!(matches!(hp, HP::Uniform(YA::Abs(42), YA::Abs(42))));
        assert!(p_hp(&j(r#"{"type":"nope"}"#)).is_none());
        assert!(
            p_hp(&j(r#"{"type":"uniform"}"#)).is_none(),
            "anchors required"
        );
    }

    #[test]
    fn p_fp_parses_float_providers() {
        assert!(
            matches!(p_fp(&j(r#"{"type":"constant","value":1.5}"#)), Some(FP::Const(v)) if v == 1.5)
        );
        assert!(matches!(
            p_fp(&j(
                r#"{"type":"uniform","min_inclusive":0.5,"max_exclusive":2.5}"#
            )),
            Some(FP::Uniform(0.5, 2.5))
        ));
        assert!(matches!(
            p_fp(&j(
                r#"{"type":"clamped_normal","mean":1.0,"deviation":0.5,"min":0.0,"max":2.0}"#
            )),
            Some(FP::Norm { mean: 1.0, .. })
        ));
        assert!(matches!(
            p_fp(&j(
                r#"{"type":"trapezoid","min":0.0,"max":4.0,"plateau":1.0}"#
            )),
            Some(FP::Trap {
                lo: 0.0,
                hi: 4.0,
                plateau: 1.0
            })
        ));
        assert!(p_fp(&j(r#"{"type":"bogus"}"#)).is_none());
        assert!(
            p_fp(&j("42")).is_none(),
            "bare numbers are not float providers"
        );
    }

    // ---------- FloatProvider sampling ----------

    #[test]
    fn fp_get_respects_bounds_and_is_deterministic() {
        // Uniform is [min, max): never reaches the exclusive top.
        let fp = FP::Uniform(2.0, 5.0);
        let mut r = rng(9);
        for _ in 0..2000 {
            let v = fp.get(&mut r);
            assert!((2.0..5.0).contains(&v), "uniform float escaped: {v}");
        }
        let mut r1 = rng(9);
        let mut r2 = rng(9);
        for _ in 0..100 {
            assert_eq!(fp.get(&mut r1), fp.get(&mut r2));
        }
        // Constant draws nothing.
        let fp = FP::Const(3.25);
        let mut r = rng(1);
        let before = r.state128();
        assert_eq!(fp.get(&mut r), 3.25);
        assert_eq!(r.state128(), before);
        // Trapezoid stays inside [lo, hi).
        let fp = FP::Trap {
            lo: 1.0,
            hi: 9.0,
            plateau: 2.0,
        };
        let mut r = rng(4);
        for _ in 0..2000 {
            let v = fp.get(&mut r);
            assert!((1.0..9.0).contains(&v), "trapezoid float escaped: {v}");
        }
        // Clamped normal can never leave [lo, hi] even with absurd deviation.
        let fp = FP::Norm {
            mean: 0.0,
            dev: 1e9,
            lo: -1.0,
            hi: 1.0,
        };
        let mut r = rng(6);
        for _ in 0..200 {
            let v = fp.get(&mut r);
            assert!((-1.0..=1.0).contains(&v));
        }
        // Degenerate lo == hi pins every draw.
        let fp = FP::Norm {
            mean: 5.0,
            dev: 1.0,
            lo: 2.0,
            hi: 2.0,
        };
        assert!(draws_fp(&fp, 3, 50).iter().all(|&v| v == 2.0));
    }

    fn draws_fp(fp: &FP, seed: u64, n: usize) -> Vec<f32> {
        let mut r = rng(seed);
        (0..n).map(|_| fp.get(&mut r)).collect()
    }

    #[test]
    fn fp_clamped_normal_helper_matches_provider_semantics() {
        // Direct helper: f64 gaussian clamped then truncated to f32.
        let mut r = rng(17);
        for _ in 0..500 {
            let v = fp_clamped_normal(&mut r, 0.0, 1.0, -2.0, 2.0);
            assert!((-2.0..=2.0).contains(&v));
        }
        // Same stream through FP::Norm consumes identically.
        let mut r1 = rng(17);
        let mut r2 = rng(17);
        let fp = FP::Norm {
            mean: 0.0,
            dev: 1.0,
            lo: -2.0,
            hi: 2.0,
        };
        for _ in 0..500 {
            assert_eq!(
                fp_clamped_normal(&mut r1, 0.0, 1.0, -2.0, 2.0),
                fp.get(&mut r2)
            );
        }
    }

    // ---------- pure helpers ----------

    #[test]
    fn noise_state_maps_midpoint_into_state_list() {
        let states = vec![10u32, 20, 30, 40];
        assert_eq!(noise_state(&states, -1.0), 10, "bottom of range");
        assert_eq!(noise_state(&states, -0.5), 20);
        assert_eq!(noise_state(&states, 0.0), 30, "midpoint boundary");
        assert_eq!(noise_state(&states, 0.5), 40);
        assert_eq!(noise_state(&states, 1.0), 40, "top clamps into last slot");
        assert_eq!(noise_state(&states, 99.0), 40);
        assert_eq!(noise_state(&states, -99.0), 10);
        // Single-state list always resolves to it.
        assert_eq!(noise_state(&[7], -1.0), 7);
        assert_eq!(noise_state(&[7], 1.0), 7);
    }

    #[test]
    fn dir_off_covers_all_six_directions() {
        assert_eq!(dir_off(0), (0, -1, 0));
        assert_eq!(dir_off(1), (0, 1, 0));
        assert_eq!(dir_off(2), (0, 0, -1));
        assert_eq!(dir_off(3), (0, 0, 1));
        assert_eq!(dir_off(4), (-1, 0, 0));
        assert_eq!(dir_off(5), (1, 0, 0));
        // Out-of-range direction ids fold to +x like the vanilla ordinal.
        assert_eq!(dir_off(6), (1, 0, 0));
        assert_eq!(dir_off(255), (1, 0, 0));
    }

    // ---------- predicate / modifier / rule-test parsing ----------

    #[test]
    fn p_pred_parses_native_predicate_shapes() {
        assert!(matches!(p_pred(&j(r#"{"type":"true"}"#)), Some(Pr::True)));
        assert!(matches!(
            p_pred(&j(r#"{"type":"matching_blocks","blocks":["minecraft:stone","minecraft:dirt"]}"#)),
            Some(Pr::Blocks(v)) if v.len() == 2
        ));
        // Single-string `blocks` is one entry, not a list.
        assert!(matches!(
            p_pred(&j(r#"{"type":"matching_blocks","blocks":"minecraft:grass_block"}"#)),
            Some(Pr::Blocks(v)) if v == ["minecraft:grass_block"]
        ));
        assert!(matches!(
            p_pred(&j(r#"{"type":"matching_fluids","fluids":"minecraft:water"}"#)),
            Some(Pr::Fluids(v)) if v == ["minecraft:water"]
        ));
        assert!(matches!(p_pred(&j(r#"{"type":"solid"}"#)), Some(Pr::Solid)));
        assert!(matches!(
            p_pred(&j(r#"{"type":"replaceable"}"#)),
            Some(Pr::Replaceable)
        ));
        assert!(matches!(
            p_pred(&j(r#"{"type":"inside_world_bounds"}"#)),
            Some(Pr::InsideWB)
        ));
        assert!(matches!(
            p_pred(&j(r#"{"type":"unobstructed"}"#)),
            Some(Pr::Unobstructed)
        ));
        assert!(matches!(
            p_pred(&j(
                r#"{"type":"has_sturdy_face","direction":"down","offset":[0,-1,0]}"#
            )),
            Some(Pr::HasSturdy([0, -1, 0], 0))
        ));
        // Offset wrapping: state predicates evaluate at pos + offset.
        assert!(matches!(
            p_pred(&j(r#"{"type":"solid","offset":[1,0,-2]}"#)),
            Some(Pr::Offset(inner, [1, 0, -2])) if matches!(*inner, Pr::Solid)
        ));
        // Boolean combinators nest.
        let pr = p_pred(&j(
            r#"{"type":"any_of","predicates":[{"type":"solid"},{"type":"not","predicate":{"type":"replaceable"}}]}"#,
        ))
        .unwrap();
        assert!(matches!(pr, Pr::Any(v) if v.len() == 2));
        assert!(matches!(
            p_pred(&j(r#"{"type":"all_of","predicates":[{"type":"solid"}]}"#)),
            Some(Pr::All(v)) if v.len() == 1
        ));
        assert!(matches!(
            p_pred(&j(r#"{"type":"not","predicate":{"type":"solid"}}"#)),
            Some(Pr::Not(_))
        ));
        // Unknown predicate fails closed.
        assert!(p_pred(&j(r#"{"type":"bogus_predicate"}"#)).is_none());
        assert!(
            p_pred(&j(r#"{"type":"not"}"#)).is_none(),
            "missing predicate"
        );
    }

    #[test]
    fn p_mod_parses_placement_modifiers() {
        let cases: &[(&str, &str)] = &[
            (
                r#"{"type":"count","count":{"type":"uniform","min_inclusive":1,"max_inclusive":3}}"#,
                "count",
            ),
            (r#"{"type":"in_square"}"#, "insquare"),
            (
                r#"{"type":"offset","x_spread":{"type":"uniform","min_inclusive":-2,"max_inclusive":2},"y_spread":{"type":"constant","value":1}}"#,
                "offset",
            ),
            (
                r#"{"type":"height_range","height":{"type":"uniform","min_inclusive":{"above_bottom":0},"max_inclusive":{"below_top":0}}}"#,
                "heightrange",
            ),
            (
                r#"{"type":"heightmap","heightmap":"WORLD_SURFACE_WG"}"#,
                "heightmap",
            ),
            (r#"{"type":"biome"}"#, "biome"),
            (r#"{"type":"fixed_placement"}"#, "fixed"),
            (
                r#"{"type":"block_predicate_filter","predicate":{"type":"solid"}}"#,
                "blockpred",
            ),
            (r#"{"type":"rarity_filter","chance":5}"#, "rarity"),
            (r#"{"type":"random_chance","chance":0.25}"#, "chance"),
            (r#"{"type":"carving_mask"}"#, "carving"),
        ];
        for &(json, tag) in cases {
            assert!(p_mod(&j(json)).is_some(), "{tag} must parse: {json}");
        }
        assert!(matches!(
            p_mod(&j(r#"{"type":"count","count":2}"#)),
            Some(PM::Count(_))
        ));
        assert!(matches!(
            p_mod(&j(r#"{"type":"in_square"}"#)),
            Some(PM::InSquare)
        ));
        assert!(matches!(
            p_mod(&j(r#"{"type":"rarity_filter","chance":5}"#)),
            Some(PM::Rarity(5))
        ));
        assert!(
            matches!(p_mod(&j(r#"{"type":"random_chance","chance":0.25}"#)), Some(PM::RandomChance(c)) if (c - 0.25).abs() < 1e-6)
        );
        assert!(matches!(
            p_mod(&j(r#"{"type":"fixed_placement"}"#)),
            Some(PM::Fixed)
        ));
        assert!(matches!(
            p_mod(&j(r#"{"type":"carving_mask"}"#)),
            Some(PM::Carving)
        ));
        // Missing required provider fails closed; unknown type fails closed.
        assert!(p_mod(&j(r#"{"type":"count"}"#)).is_none());
        assert!(p_mod(&j(r#"{"type":"height_range"}"#)).is_none());
        assert!(p_mod(&j(r#"{"type":"bogus_modifier"}"#)).is_none());
        // `random_offset` is the 1.21.1 spelling of `offset`.
        assert!(matches!(
            p_mod(&j(
                r#"{"type":"random_offset","x":{"type":"constant","value":1},"y":{"type":"constant","value":2}}"#
            )),
            Some(PM::Offset(_, _))
        ));
    }

    #[test]
    fn p_rt_parses_rule_tests_that_need_no_registry() {
        assert!(matches!(
            p_rt(&j(r#"{"predicate_type":"always_true"}"#)),
            Some(RT::True)
        ));
        assert!(matches!(
            p_rt(&j(r#"{"predicate_type":"tag_match","tag":"minecraft:ice"}"#)),
            Some(RT::Tag(t)) if t == "minecraft:ice"
        ));
        assert!(matches!(
            p_rt(&j(r#"{"predicate_type":"block_match","block":"minecraft:stone"}"#)),
            Some(RT::Block(b)) if b == "minecraft:stone"
        ));
        assert!(matches!(
            p_rt(&j(
                r#"{"predicate_type":"height_match","min_inclusive":60,"max_inclusive":100}"#
            )),
            Some(RT::Height(60, 100))
        ));
        assert!(matches!(
            p_rt(&j(r#"{"predicate_type":"random_block_match","block":"minecraft:tall_grass","probability":0.5}"#)),
            Some(RT::RandBlock(b, p)) if b == "minecraft:tall_grass" && (p - 0.5).abs() < 1e-6
        ));
        // Combinators compose.
        assert!(matches!(
            p_rt(&j(r#"{"predicate_type":"any_of","rules":[{"predicate_type":"always_true"},{"predicate_type":"tag_match","tag":"x"}]}"#)),
            Some(RT::Any(v)) if v.len() == 2
        ));
        assert!(matches!(
            p_rt(&j(
                r#"{"predicate_type":"not","predicate":{"predicate_type":"always_true"}}"#
            )),
            Some(RT::Not(_))
        ));
        assert!(p_rt(&j(r#"{"predicate_type":"bogus"}"#)).is_none());
    }

    // ---------- end-to-end provider evaluation through JSON ----------

    #[test]
    fn json_driven_int_and_height_providers_evaluate_in_bounds() {
        // A datapack-shaped uniform provider end to end: parse -> sample.
        let ip = p_ip(&j(
            r#"{"type":"minecraft:uniform","min_inclusive":-20,"max_inclusive":20}"#,
        ))
        .unwrap();
        assert!(draws(&ip, 2024, 1000)
            .iter()
            .all(|&v| (-20..=20).contains(&v)));
        // Weighted list with nested clamped source.
        let ip = p_ip(&j(
            r#"{"type":"weighted_list","distribution":[
                {"weight":1,"data":{"type":"clamped","source":{"type":"uniform","min_inclusive":0,"max_inclusive":100},"min_inclusive":10,"max_inclusive":20}},
                {"weight":1,"data":{"type":"constant","value":-1}}
            ]}"#,
        ))
        .unwrap();
        let samples = draws(&ip, 88, 500);
        assert!(samples.iter().all(|&v| v == -1 || (10..=20).contains(&v)));
        assert!(samples.contains(&-1) && samples.iter().any(|&v| v != -1));
        // Height provider over the vanilla overworld window.
        let hp = p_hp(&j(
            r#"{"type":"trapezoid","min_inclusive":{"above_bottom":0},"max_inclusive":{"below_top":0},"plateau":40}"#,
        ))
        .unwrap();
        let samples = draws_hp_window(&hp, 66, 1000);
        assert!(samples.iter().all(|&y| (-64..=319).contains(&y)));
    }

    fn draws_hp_window(hp: &HP, seed: u64, n: usize) -> Vec<i32> {
        let mut r = rng(seed);
        (0..n).map(|_| hp.get(&mut r, -64, 384)).collect()
    }
}
