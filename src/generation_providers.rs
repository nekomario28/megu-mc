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
///   spare is already populated), computed in f64 and truncated back to f32.
/// * `minecraft:trapezoid` (`bqa`): two `nextFloat`s, `min + r*h + r*g`.
///
/// The shared piece of `minecraft:clamped_normal`: `Mth.clamp(Mth.nextGaussian(random,
/// mean, deviation), min, max)`. `Mth.nextGaussian` promotes its float arguments, so the
/// arithmetic is f64 and only the result is f32 - the dripstone cluster's own tip length
/// calls the same helper, so it lives here rather than in the feature.
pub(crate) fn fp_clamped_normal(r: &mut Xor, mean: f32, dev: f32, lo: f32, hi: f32) -> f32 {
    let v = mean as f64 + r.next_gauss() * dev as f64;
    let (a, b) = (lo as f64, hi as f64);
    (if v < a {
        a
    } else if v > b {
        b
    } else {
        v
    }) as f32
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
                    || matches!(reg.name_of(below), "minecraft:ice" | "minecraft:frosted_ice"))
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
