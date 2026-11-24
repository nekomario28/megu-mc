//! Seeded generation mathematics; no terrain, registry or world-state owner.

// ---------- MD5 (name -> 128-bit seed) ----------

pub(crate) fn md5(data: &[u8]) -> [u8; 16] {
    megu_core::crypto::md5(data)
}

// ---------- Xoroshiro128++ with Java RandomSource semantics ----------

fn stafford13(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^ (z >> 31)
}

// ---------- Mth.sin / Mth.cos (vanilla's lookup table) ----------
//
// `Mth.sin(float)` is not libm: it indexes a 65536-entry float table built from
// `(float)Math.sin(i * PI * 2.0 / 65536.0)`. Feature geometry mixes the two
// precisions - `OreFeature` places its centre with `Math.sin((double)f)` and its
// radii with `Mth.sin` - so a port that calls `f32::sin` everywhere is off by an
// ulp exactly where a cell boundary is decided.
const SIN_STEPS: f32 = 10430.378;

fn sin_table() -> &'static [f32; 65536] {
    static T: std::sync::OnceLock<Box<[f32; 65536]>> = std::sync::OnceLock::new();
    T.get_or_init(|| {
        let mut t = Box::new([0.0f32; 65536]);
        for (i, v) in t.iter_mut().enumerate() {
            *v = (i as f64 * std::f64::consts::PI * 2.0 / 65536.0).sin() as f32;
        }
        t
    })
}

pub(crate) fn mth_sin(v: f32) -> f32 {
    sin_table()[(v * SIN_STEPS) as i32 as usize & 65535]
}

/// `Mth.cos` is the same table read a quarter turn ahead, not libm. The
/// quarter-turn is added in `f32` *before* the truncation, as in the bytecode.
pub(crate) fn mth_cos(v: f32) -> f32 {
    sin_table()[(v * SIN_STEPS + 16384.0) as i32 as usize & 65535]
}

/// `FSEED_ARM=cx,cz,i,step` logs the first draws of that placed feature the way
/// the server's `WorldgenRandom.next(bits)` does, so both streams can be diffed.
pub(crate) fn nxtrace(bits: u32, v: u64) {
    let n = NTRACE.load(std::sync::atomic::Ordering::Relaxed);
    if n < 0 {
        return;
    }
    eprintln!("NDRAW {n} bits={bits} -> {v}");
    NTRACE.store(
        if n + 1 >= 20 { -1 } else { n + 1 },
        std::sync::atomic::Ordering::Relaxed,
    );
}

pub(crate) static NTRACE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

/// The same trace for `nextFloat`, which `NTRACE` (ints) cannot show.
pub(crate) fn fdraw(v: f32) {
    let n = FTRACE.load(std::sync::atomic::Ordering::Relaxed);
    if n < 0 {
        return;
    }
    eprintln!("FDRAW {n} -> {v}");
    FTRACE.store(
        if n + 1 >= 20 { -1 } else { n + 1 },
        std::sync::atomic::Ordering::Relaxed,
    );
}

pub(crate) static FTRACE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

/// One chronological trace of the *typed* draws, mirroring the agent's hook on
/// `RandomSource`'s default methods, so the two sequences can be diffed directly
/// (the raw `next(bits)` traces cannot be interleaved with each other). Positional
/// aquifer/ore RNGs can run lazily inside a feature; keep their records separate
/// from WorldgenRandom, matching the native comparator's `@dyn` boundary.
pub(crate) fn dtrace(wrapped: bool, kind: impl std::fmt::Display, value: f64) {
    let n = DTRACE.load(std::sync::atomic::Ordering::Relaxed);
    if n < 0 {
        return;
    }
    let f = XTRACE_FEAT.load(std::sync::atomic::Ordering::Relaxed);
    let source = if wrapped { "DRAW" } else { "RAW_DRAW" };
    eprintln!("{source} {n} f={},{} {kind}={value}", f >> 32, f & 0xFFFF);
    let limit = crate::trace_env::env_str!("FEATURE_DRAW_LIMIT")
        .and_then(|s| s.parse::<i32>().ok())
        .unwrap_or(200);
    DTRACE.store(
        if n + 1 >= limit { -1 } else { n + 1 },
        std::sync::atomic::Ordering::Relaxed,
    );
}

pub(crate) static DTRACE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

/// Which placed feature the trace is currently inside, so a draw can be attributed
/// without guessing from the sequence shape.
pub(crate) static XTRACE_FEAT: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(-1);
/// The chunk whose population pass is running (`cx<<32 | cz`), for the write trace: a cell can
/// be written by its own chunk's pass or by a neighbour's, and "which pass wrote this" is what
/// makes a single pass comparable on its own.
pub(crate) static CUR_POP_CHUNK: std::sync::atomic::AtomicI64 =
    std::sync::atomic::AtomicI64::new(-1);

/// Env-gated draw trace for feature placement (`FSEED_ARM=i,step`).
pub(crate) static XTRACE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

fn xtrace(kind: &str, arg: i64, val: i64) {
    let n = XTRACE.load(std::sync::atomic::Ordering::Relaxed);
    if n < 0 {
        return;
    }
    eprintln!("XDRAW {n} bound={arg} -> {val} ({kind})");
    XTRACE.store(
        if n + 1 >= 16 { -1 } else { n + 1 },
        std::sync::atomic::Ordering::Relaxed,
    );
}

/// fastutil HashCommon.mix(long), shared by native chunk-deadline maps and
/// structure reference sets. This is NOT its similarly named murmurHash3.
pub(crate) fn fastutil_mix64(key: u64) -> u64 {
    let h = key.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    let h = h ^ (h >> 32);
    h ^ (h >> 16)
}

pub(crate) struct Xor {
    lo: u64,
    hi: u64,
    g: Option<f64>,
    /// `WorldgenRandom` wraps a source but implements `dyn`'s *legacy* draws:
    /// `nextInt()` is `next(32)` (top bits), `nextInt(bound)` is the
    /// multiply-shift on `next(31)`, `nextLong()` is two `next(32)` calls.
    /// The raw Xoroshiro source (aquifer/vein sampling) uses the modern
    /// `RandomGenerator` forms instead, so both flavours are needed.
    legacy: bool,
}
/// `Mth.nextFloat(RandomSource, float, float)` - `random.nextFloat() * (max - min) +
/// min`, with the subtraction done in f32 (so `0.7f32 - 0.3` is not `0.4f32` to the
/// last bit; the float-provider test caught exactly that).
pub(crate) fn mth_next_f32(r: &mut Xor, min: f32, max: f32) -> f32 {
    r.next_f32() * (max - min) + min
}

/// `Mth.map(value, fromMin, fromMax, toMin, toMax)` - the unclamped linear map.
pub(crate) fn map_f64(value: f64, from_min: f64, from_max: f64, to_min: f64, to_max: f64) -> f64 {
    to_min + (value - from_min) * (to_max - to_min) / (from_max - from_min)
}

/// `Mth.clampedMap(double...)`: `map` clamped to the destination range (in either
/// order - the clamp bounds are the min/max of the two destinations).
pub(crate) fn clamped_map_f64(
    value: f64,
    from_min: f64,
    from_max: f64,
    to_min: f64,
    to_max: f64,
) -> f64 {
    let v = map_f64(value, from_min, from_max, to_min, to_max);
    let (lo, hi) = if to_min <= to_max {
        (to_min, to_max)
    } else {
        (to_max, to_min)
    };
    if v < lo {
        lo
    } else if v > hi {
        hi
    } else {
        v
    }
}

/// `Mth.clampedMap(float...)` - the float overload, which the dripstone cluster's
/// chance uses (the casts are load-bearing: the f32 division rounds differently).
pub(crate) fn clamped_map_f32(
    value: f32,
    from_min: f32,
    from_max: f32,
    to_min: f32,
    to_max: f32,
) -> f32 {
    let v = to_min + (value - from_min) * (to_max - to_min) / (from_max - from_min);
    let (lo, hi) = if to_min <= to_max {
        (to_min, to_max)
    } else {
        (to_max, to_min)
    };
    if v < lo {
        lo
    } else if v > hi {
        hi
    } else {
        v
    }
}

impl Xor {
    pub(crate) fn seed(seed: u64) -> Self {
        let l = seed ^ 0x6A09E667F3BCC909;
        let m = l.wrapping_add(0x9E3779B97F4A7C15);
        Self::raw(stafford13(l), stafford13(m))
    }
    pub(crate) fn seed_worldgen(seed: u64) -> Self {
        let mut x = Self::seed(seed);
        x.legacy = true;
        x
    }
    /// Non-consuming observation; unlike nextLong this must not move the stream.
    #[cfg(test)]
    pub(crate) fn state128(&self) -> (u64, u64) {
        (self.lo, self.hi)
    }
    pub(crate) const fn raw(lo: u64, hi: u64) -> Self {
        let (lo, hi) = if (lo | hi) == 0 {
            (0x9E3779B97F4A7C15, 0x6A09E667F3BCC909)
        } else {
            (lo, hi)
        };
        Xor {
            lo,
            hi,
            g: None,
            legacy: false,
        }
    }
    pub(crate) fn next_long(&mut self) -> u64 {
        let l = self.lo;
        let m = self.hi;
        let n = l.wrapping_add(m).rotate_left(17).wrapping_add(l);
        let m = m ^ l;
        self.lo = l.rotate_left(49) ^ m ^ (m << 21);
        self.hi = m.rotate_left(28);
        n
    }
    pub(crate) fn next(&mut self, bits: u32) -> u64 {
        let v = self.next_long() >> (64 - bits);
        nxtrace(bits, v);
        v
    }
    /// The Java-facing nextLong: a wrapped WorldgenRandom consumes two
    /// next(32) calls; a bare XoroshiroRandomSource consumes one engine word.
    pub(crate) fn next_i64(&mut self) -> i64 {
        if self.legacy {
            self.next_i64_legacy()
        } else {
            self.next_long() as i64
        }
    }
    /// `WorldgenRandom`-style `nextLong()`: two `next(32)` calls, sign-extended.
    pub(crate) fn next_i64_legacy(&mut self) -> i64 {
        let hi = self.next(32) as u32 as i32 as i64;
        let lo = self.next(32) as u32 as i32 as i64;
        (hi << 32).wrapping_add(lo)
    }
    pub(crate) fn next_i32(&mut self) -> i32 {
        if self.legacy {
            // `dyn`'s default `nextInt()` is `next(32)`, i.e. the top word.
            self.next(32) as i32
        } else {
            self.next_long() as i32
        }
    }
    pub(crate) fn next_bound(&mut self, bound: i32) -> i32 {
        let v = self.next_bound_raw(bound);
        xtrace("nextInt", bound as i64, v as i64);
        dtrace(self.legacy, format_args!("nextInt({bound})"), v as f64);
        v
    }
    fn next_bound_raw(&mut self, bound: i32) -> i32 {
        if self.legacy {
            // `dyn.a(int)` (the default WorldgenRandom inherits):
            // power-of-two -> (bound * next(31)) >> 31, else rejection on next(31).
            if bound > 0 && (bound & (bound - 1)) == 0 {
                return ((bound as i64).wrapping_mul(self.next(31) as i64) >> 31) as i32;
            }
            loop {
                let bits = self.next(31) as i32;
                let val = bits % bound;
                if bits.wrapping_sub(val).wrapping_add(bound - 1) >= 0 {
                    return val;
                }
            }
        }
        // The modern bounded draw is Lemire's multiply-then-shift reduction: a
        // 32-bit draw times the bound is a 64-bit product whose high word is the
        // result, and products whose low word falls under a threshold are redrawn
        // so every value stays equally likely.
        let span = bound as u64;
        let widen = |draw: i32| u64::from(draw as u32).wrapping_mul(span);
        let mut product = widen(self.next_i32());
        if (product & 0xFFFF_FFFF) < span {
            // `Integer.remainderUnsigned(~bound + 1, bound)`: the remainder is taken on the
            // 32-bit unsigned values. Widening `-bound` to 64 bits first (sign extension)
            // gives a different remainder for every bound that is not a power of two.
            let threshold = u64::from((bound.wrapping_neg() as u32) % (bound as u32));
            while (product & 0xFFFF_FFFF) < threshold {
                product = widen(self.next_i32());
            }
        }
        (product >> 32) as i32
    }
    pub(crate) fn next_f64(&mut self) -> f64 {
        let traced = self.next_f64_inner();
        dtrace(self.legacy, "nextDouble", traced);
        traced
    }
    fn next_f64_inner(&mut self) -> f64 {
        if self.legacy {
            // `dyn`'s default: ((next(26) << 27) + next(27)) * 2^-53
            let hi = self.next(26) as i64;
            let lo = self.next(27) as i64;
            ((hi << 27).wrapping_add(lo)) as f64 * f64::from_bits(0x3CA0000000000000)
        } else {
            (self.next_long() >> 11) as f64 * f64::from_bits(0x3CA0000000000000)
            // 2^-53
        }
    }
    pub(crate) fn next_f32(&mut self) -> f32 {
        let v = self.next(24) as f32 * 5.960_464_5E-8;
        fdraw(v);
        dtrace(self.legacy, "nextFloat", v as f64);
        v
    }
    pub(crate) fn next_bool(&mut self) -> bool {
        if self.legacy {
            // `dyn`'s default `nextBoolean()` is `next(1) != 0`, i.e. the top bit.
            self.next(1) != 0
        } else {
            (self.next_long() & 1) != 0
        }
    }
    // nextInbetween inclusive: next_bounded(max-min+1)+min
    pub(crate) fn next_inbetween(&mut self, min: i32, max: i32) -> i32 {
        // `Mth.randomBetweenInclusive` = `nextInt(max - min + 1) + min`: it
        // draws even when min == max (`nextInt(1)`), and skipping that draw
        // shifts every later value in the stream.
        if max < min {
            return min;
        }
        self.next_bound(max - min + 1) + min
    }
    // java.util.Random.nextGaussian (Marsaglia polar, with spare)
    pub(crate) fn next_gauss(&mut self) -> f64 {
        if let Some(v) = self.g.take() {
            return v;
        }
        loop {
            let v1 = 2.0 * self.next_f64() - 1.0;
            let v2 = 2.0 * self.next_f64() - 1.0;
            let s = v1 * v1 + v2 * v2;
            if s >= 1.0 || s == 0.0 {
                continue;
            }
            let m = (-2.0 * s.ln() / s).sqrt();
            self.g = Some(v2 * m);
            return v1 * m;
        }
    }
    #[cfg(test)]
    #[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
    pub(crate) fn split(&mut self) -> Self {
        Xor::raw(self.next_long(), self.next_long())
    }

    pub(crate) fn splitter(&mut self) -> Split {
        Split {
            lo: self.next_long(),
            hi: self.next_long(),
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Split {
    pub(crate) lo: u64,
    pub(crate) hi: u64,
}
impl Split {
    pub(crate) fn of_str(&self, name: &str) -> Xor {
        let h = md5(name.as_bytes());
        let l = u64::from_be_bytes(h[0..8].try_into().unwrap());
        let m = u64::from_be_bytes(h[8..16].try_into().unwrap());
        Xor::raw(l ^ self.lo, m ^ self.hi)
    }
    // vanilla PositionalRandomFactory.at(x,y,z) — seed = hash_block_pos ^ lo
    pub(crate) fn pos(&self, x: i32, y: i32, z: i32) -> Xor {
        Xor::raw(hash_block_pos(x, y, z) as u64 ^ self.lo, self.hi)
    }
}

pub(crate) fn hash_block_pos(x: i32, y: i32, z: i32) -> i64 {
    let l = (x.wrapping_mul(3129871) as i64) ^ (z as i64).wrapping_mul(116129781) ^ (y as i64);
    let l = l
        .wrapping_mul(l)
        .wrapping_mul(42317861)
        .wrapping_add(l.wrapping_mul(11));
    l >> 16
}

// java String.hashCode (UTF-16 units)
pub(crate) fn java_hash(s: &str) -> i32 {
    let mut h = 0i32;
    for u in s.encode_utf16() {
        h = h.wrapping_mul(31).wrapping_add(u as i32);
    }
    h
}

// ---------- legacy java.util.Random LCG (population seeds, legacy noise) ----------

/// Gameplay debugging aid: while this is non-zero, every `Lcg` draw is
/// printed as `RNG <method> <value>` and the counter is decremented. Used to
/// diff a carver's draw sequence against the vanilla probe's `DRAW` lines.
pub(crate) static RNG_TRACE: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

fn rtrace(name: &str, v: impl std::fmt::Display) {
    if RNG_TRACE.load(std::sync::atomic::Ordering::Relaxed) == 0 {
        return;
    }
    RNG_TRACE.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    eprintln!("RNG {name} {v}");
}

/// -1 = disarmed; otherwise the number of draws already traced.
pub(crate) static RAW_ARM: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

pub(crate) struct Lcg {
    seed: u64,
}
#[allow(dead_code)]
impl Lcg {
    pub(crate) fn new(seed: u64) -> Self {
        Lcg {
            seed: (seed ^ 0x5_DEEC_E66D) & 0xFFFF_FFFF_FFFF,
        }
    }
    /// WorldgenRandom.setLargeFeatureSeed: raw nextLong values, not the odd
    /// multipliers of setDecorationSeed. Shared by carvers and structure layout.
    pub(crate) fn for_large_feature(seed: u64, x: i32, z: i32) -> Self {
        let mut source = Self::new(seed);
        let a = source.next_i64();
        let b = source.next_i64();
        Self::new(
            (i64::from(x).wrapping_mul(a) ^ i64::from(z).wrapping_mul(b) ^ seed as i64) as u64,
        )
    }

    /// Non-consuming position; unlike a nextLong sentinel this is safe mid-stream.
    pub(crate) fn state48(&self) -> u64 {
        self.seed
    }
    pub(crate) fn from_raw_state(state48: u64) -> Self {
        Lcg {
            seed: state48 & 0xFFFF_FFFF_FFFF,
        }
    }

    fn nx(&mut self, bits: u32) -> u64 {
        self.seed = self.seed.wrapping_mul(0x5_DEEC_E66D).wrapping_add(11) & 0xFFFF_FFFF_FFFF;
        let v = self.seed >> (48 - bits);
        // CARVE_RAW=<cx>,<cz>: dump the first 16 raw next(bits) values after that
        // chunk's carver reseed, to diff against the server's RBITS trace.
        let n = RAW_ARM.load(std::sync::atomic::Ordering::Relaxed);
        if n >= 0 {
            eprintln!("RBITS {n} bits={bits} -> {v}");
            RAW_ARM.store(
                if n + 1 >= 16 { -1 } else { n + 1 },
                std::sync::atomic::Ordering::Relaxed,
            );
        }
        v
    }
    pub(crate) fn next_i32(&mut self) -> i32 {
        let v = self.nx(32) as u32 as i32;
        rtrace("nextInt", v);
        v
    }
    pub(crate) fn next_i64(&mut self) -> i64 {
        // `RandomSource`'s default `nextLong` is `(long)next(32) << 32 + next(32)`
        // where `next(32)` is an **int**: both halves sign-extend. Treating them as
        // unsigned shifts the result by 2^32 whenever bit 31 of the low word is
        // set, which is half the draws - so every seed derived from this stream
        // (population seed, large-feature seed, carver seeds) came out wrong half
        // the time.
        let hi = self.nx(32) as u32 as i32 as i64;
        let lo = self.nx(32) as u32 as i32 as i64;
        let v = (hi << 32).wrapping_add(lo);
        rtrace("nextLong", v);
        v
    }
    // java nextInt(bound): power-of-2 fast path + rejection otherwise
    pub(crate) fn next_bound(&mut self, bound: i32) -> i32 {
        let v = self.next_bound_raw(bound);
        rtrace("nextInt", format_args!("{v} (bound {bound})"));
        v
    }
    fn next_bound_raw(&mut self, bound: i32) -> i32 {
        if bound <= 0 {
            return 0;
        }
        if bound & (bound - 1) == 0 {
            return ((bound as i64).wrapping_mul(self.nx(31) as i64) >> 31) as i32;
        }
        loop {
            let u = self.nx(31) as i32;
            let v = u % bound;
            if u.wrapping_sub(v).wrapping_add(bound - 1) >= 0 {
                return v;
            }
        }
    }
    pub(crate) fn next_f32(&mut self) -> f32 {
        let v = self.nx(24) as f32 * 5.960_464_5E-8;
        rtrace("nextFloat", v);
        v
    }
    pub(crate) fn next_f64(&mut self) -> f64 {
        let v = ((self.nx(26) << 27).wrapping_add(self.nx(27)) as f64)
            * f64::from_bits(0x3CA0000000000000);
        rtrace("nextDouble", v);
        v
    }
    pub(crate) fn next_bool(&mut self) -> bool {
        let v = self.nx(1) != 0;
        rtrace("nextBoolean", v);
        v
    }
    pub(crate) fn skip(&mut self, n: i32) {
        for _ in 0..n {
            self.next_i32();
        }
    }
    // legacy RandomDeriver.split_string: java_hash ^ seed
    pub(crate) fn split_str(seed: u64, name: &str) -> Self {
        Lcg::new((java_hash(name) as i64 as u64) ^ seed)
    }
}

pub(crate) fn lmap(v: f64, a: f64, b: f64, c: f64, d: f64) -> f64 {
    c + (v - a) * (d - c) / (b - a)
}
pub(crate) fn lerp2(fx: f64, fz: f64, v00: f64, v10: f64, v01: f64, v11: f64) -> f64 {
    let a = v00 + fx * (v10 - v00);
    let b = v01 + fx * (v11 - v01);
    a + fz * (b - a)
}

// ---------- Perlin / smeared perlin / normal (octave) noise ----------

const GRAD: [[f64; 3]; 16] = [
    [1.0, 1.0, 0.0],
    [-1.0, 1.0, 0.0],
    [1.0, -1.0, 0.0],
    [-1.0, -1.0, 0.0],
    [1.0, 0.0, 1.0],
    [-1.0, 0.0, 1.0],
    [1.0, 0.0, -1.0],
    [-1.0, 0.0, -1.0],
    [0.0, 1.0, 1.0],
    [0.0, -1.0, 1.0],
    [0.0, 1.0, -1.0],
    [0.0, -1.0, -1.0],
    [1.0, 1.0, 0.0],
    [0.0, -1.0, 1.0],
    [-1.0, 1.0, 0.0],
    [0.0, -1.0, -1.0],
];

#[inline(always)]
fn wrap(x: f64) -> f64 {
    const RO: f64 = 33_554_432.0;
    const HALF: f64 = f64::from_bits(0x416F_FFFF_FFFF_FFFF);
    if x > -HALF && x < HALF {
        x
    } else {
        x - (x / RO + 0.5).floor() * RO
    }
}
#[inline(always)]
fn smooth(t: f64) -> f64 {
    t * t * t * (t * (t * 6.0 - 15.0) + 10.0)
}
// vanilla's own signature (`Mth.lerp3` takes the three weights and eight ends);
// grouping them would invent a type the reference does not have
#[expect(clippy::too_many_arguments)]
pub(crate) fn lerp3(
    a: f64,
    b: f64,
    c: f64,
    v0: f64,
    v1: f64,
    v2: f64,
    v3: f64,
    v4: f64,
    v5: f64,
    v6: f64,
    v7: f64,
) -> f64 {
    let l0 = v0 + a * (v1 - v0);
    let l1 = v2 + a * (v3 - v2);
    let l2 = v4 + a * (v5 - v4);
    let l3 = v6 + a * (v7 - v6);
    let m0 = l0 + b * (l1 - l0);
    let m1 = l2 + b * (l3 - l2);
    m0 + c * (m1 - m0)
}

#[derive(Clone)]
pub(crate) struct Perlin {
    p: [u8; 256],
    xo: f64,
    yo: f64,
    zo: f64,
}
impl Perlin {
    fn new(r: &mut Xor) -> Self {
        let xo = r.next_f64() * 256.0;
        let yo = r.next_f64() * 256.0;
        let zo = r.next_f64() * 256.0;
        let mut p = [0u8; 256];
        for (i, v) in p.iter_mut().enumerate() {
            *v = i as u8;
        }
        for i in 0..256usize {
            let off = r.next_bound((256 - i) as i32) as usize;
            p.swap(i, off + i);
        }
        Perlin { p, xo, yo, zo }
    }
    fn new_l(r: &mut Lcg) -> Self {
        let xo = r.next_f64() * 256.0;
        let yo = r.next_f64() * 256.0;
        let zo = r.next_f64() * 256.0;
        let mut p = [0u8; 256];
        for (i, v) in p.iter_mut().enumerate() {
            *v = i as u8;
        }
        for i in 0..256usize {
            let off = r.next_bound((256 - i) as i32) as usize;
            p.swap(i, off + i);
        }
        Perlin { p, xo, yo, zo }
    }
    #[inline(always)]
    fn perm(&self, x: i32) -> i32 {
        self.p[(x & 0xFF) as usize] as i32
    }
    #[inline(always)]
    fn grad_dot(hash: i32, x: f64, y: f64, z: f64) -> f64 {
        let g = &GRAD[(hash & 15) as usize];
        g[0] * x + g[1] * y + g[2] * z
    }
    #[allow(clippy::too_many_arguments)]
    fn sample_lerp(&self, x: i32, y: i32, z: i32, rx: f64, ry: f64, rz: f64, ory: f64) -> f64 {
        let x0 = self.perm(x);
        let x1 = self.perm(x + 1);
        let xy00 = self.perm(x0 + y);
        let xy01 = self.perm(x0 + y + 1);
        let xy10 = self.perm(x1 + y);
        let xy11 = self.perm(x1 + y + 1);
        let d000 = Self::grad_dot(self.perm(xy00 + z), rx, ry, rz);
        let d100 = Self::grad_dot(self.perm(xy10 + z), rx - 1.0, ry, rz);
        let d010 = Self::grad_dot(self.perm(xy01 + z), rx, ry - 1.0, rz);
        let d110 = Self::grad_dot(self.perm(xy11 + z), rx - 1.0, ry - 1.0, rz);
        let d001 = Self::grad_dot(self.perm(xy00 + z + 1), rx, ry, rz - 1.0);
        let d101 = Self::grad_dot(self.perm(xy10 + z + 1), rx - 1.0, ry, rz - 1.0);
        let d011 = Self::grad_dot(self.perm(xy01 + z + 1), rx, ry - 1.0, rz - 1.0);
        let d111 = Self::grad_dot(self.perm(xy11 + z + 1), rx - 1.0, ry - 1.0, rz - 1.0);
        lerp3(
            smooth(rx),
            smooth(ory),
            smooth(rz),
            d000,
            d100,
            d010,
            d110,
            d001,
            d101,
            d011,
            d111,
        )
    }
    fn get(&self, x: f64, y: f64, z: f64) -> f64 {
        let x = wrap(x) + self.xo;
        let y = wrap(y) + self.yo;
        let z = wrap(z) + self.zo;
        let fx = x.floor() as i32;
        let fy = y.floor() as i32;
        let fz = z.floor() as i32;
        let rx = x - fx as f64;
        let ry = y - fy as f64;
        let rz = z - fz as f64;
        self.sample_lerp(fx, fy, fz, rx, ry, rz, ry)
    }

    /// Four scalar [`Perlin::get`] values, bit for bit. The coordinate wrap, the
    /// permutation hashes and the eight gradient dots are the same scalar operations
    /// (in the same order); only `smooth` and `lerp3`'s seven stages run across four
    /// `__m256d` lanes. No FMA: the vector operations are plain mul/add/sub, so no
    /// operand is rounded differently from the scalar path.
    pub(crate) fn get_4(&self, x: [f64; 4], y: [f64; 4], z: [f64; 4]) -> [f64; 4] {
        #[cfg(target_arch = "x86_64")]
        {
            static AVX2: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            if *AVX2.get_or_init(|| std::arch::is_x86_feature_detected!("avx2")) {
                // SAFETY: AVX2 was just detected at runtime.
                return unsafe { self.get_4_avx2(x, y, z) };
            }
        }
        std::array::from_fn(|k| self.get(x[k], y[k], z[k]))
    }

    /// AVX2 kernel behind [`Perlin::get_4`].
    ///
    /// # Safety
    /// The caller must have checked `is_x86_feature_detected!("avx2")`.
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2")]
    unsafe fn get_4_avx2(&self, x: [f64; 4], y: [f64; 4], z: [f64; 4]) -> [f64; 4] {
        use std::arch::x86_64::*;
        let x_same = x[0].to_bits() == x[1].to_bits()
            && x[0].to_bits() == x[2].to_bits()
            && x[0].to_bits() == x[3].to_bits();
        let y_same = y[0].to_bits() == y[1].to_bits()
            && y[0].to_bits() == y[2].to_bits()
            && y[0].to_bits() == y[3].to_bits();
        let z_same = z[0].to_bits() == z[1].to_bits()
            && z[0].to_bits() == z[2].to_bits()
            && z[0].to_bits() == z[3].to_bits();
        let mut ix = [0i32; 4];
        let mut iy = [0i32; 4];
        let mut iz = [0i32; 4];
        let mut rx = [0.0f64; 4];
        let mut ry = [0.0f64; 4];
        let mut rz = [0.0f64; 4];
        if x_same {
            let wx = wrap(x[0]) + self.xo;
            let fx = wx.floor() as i32;
            ix = [fx; 4];
            rx = [wx - fx as f64; 4];
        } else {
            for k in 0..4 {
                let wx = wrap(x[k]) + self.xo;
                ix[k] = wx.floor() as i32;
                rx[k] = wx - ix[k] as f64;
            }
        }
        if y_same {
            let wy = wrap(y[0]) + self.yo;
            let fy = wy.floor() as i32;
            iy = [fy; 4];
            ry = [wy - fy as f64; 4];
        } else {
            for k in 0..4 {
                let wy = wrap(y[k]) + self.yo;
                iy[k] = wy.floor() as i32;
                ry[k] = wy - iy[k] as f64;
            }
        }
        if z_same {
            let wz = wrap(z[0]) + self.zo;
            let fz = wz.floor() as i32;
            iz = [fz; 4];
            rz = [wz - fz as f64; 4];
        } else {
            for k in 0..4 {
                let wz = wrap(z[k]) + self.zo;
                iz[k] = wz.floor() as i32;
                rz[k] = wz - iz[k] as f64;
            }
        }
        // The eight gradient dots per lane, in `sample_lerp`'s order and with the
        // scalar grad_dot operation order `((g0*x) + (g1*y)) + (g2*z)`.
        let mut d0 = [0.0f64; 4];
        let mut d1 = [0.0f64; 4];
        let mut d2 = [0.0f64; 4];
        let mut d3 = [0.0f64; 4];
        let mut d4 = [0.0f64; 4];
        let mut d5 = [0.0f64; 4];
        let mut d6 = [0.0f64; 4];
        let mut d7 = [0.0f64; 4];
        for k in 0..4 {
            let (px, py, pz) = (ix[k], iy[k], iz[k]);
            let (dx, dy, dz) = (rx[k], ry[k], rz[k]);
            let x0 = self.perm(px);
            let x1 = self.perm(px + 1);
            let xy00 = self.perm(x0 + py);
            let xy01 = self.perm(x0 + py + 1);
            let xy10 = self.perm(x1 + py);
            let xy11 = self.perm(x1 + py + 1);
            d0[k] = Self::grad_dot(self.perm(xy00 + pz), dx, dy, dz);
            d1[k] = Self::grad_dot(self.perm(xy10 + pz), dx - 1.0, dy, dz);
            d2[k] = Self::grad_dot(self.perm(xy01 + pz), dx, dy - 1.0, dz);
            d3[k] = Self::grad_dot(self.perm(xy11 + pz), dx - 1.0, dy - 1.0, dz);
            d4[k] = Self::grad_dot(self.perm(xy00 + pz + 1), dx, dy, dz - 1.0);
            d5[k] = Self::grad_dot(self.perm(xy10 + pz + 1), dx - 1.0, dy, dz - 1.0);
            d6[k] = Self::grad_dot(self.perm(xy01 + pz + 1), dx, dy - 1.0, dz - 1.0);
            d7[k] = Self::grad_dot(self.perm(xy11 + pz + 1), dx - 1.0, dy - 1.0, dz - 1.0);
        }
        let v0 = _mm256_loadu_pd(d0.as_ptr());
        let v1 = _mm256_loadu_pd(d1.as_ptr());
        let v2 = _mm256_loadu_pd(d2.as_ptr());
        let v3 = _mm256_loadu_pd(d3.as_ptr());
        let v4 = _mm256_loadu_pd(d4.as_ptr());
        let v5 = _mm256_loadu_pd(d5.as_ptr());
        let v6 = _mm256_loadu_pd(d6.as_ptr());
        let v7 = _mm256_loadu_pd(d7.as_ptr());
        // smooth(t) = t*t*t*(t*(t*6-15)+10), same op order as scalar `smooth`.
        let smoothv = |t: __m256d| -> __m256d {
            let t2 = _mm256_mul_pd(t, t);
            let t3 = _mm256_mul_pd(t2, t);
            let a = _mm256_mul_pd(t, _mm256_set1_pd(6.0));
            let b = _mm256_sub_pd(a, _mm256_set1_pd(15.0));
            let c = _mm256_mul_pd(t, b);
            let e = _mm256_add_pd(c, _mm256_set1_pd(10.0));
            _mm256_mul_pd(t3, e)
        };
        let a = if x_same {
            _mm256_set1_pd(smooth(rx[0]))
        } else {
            smoothv(_mm256_loadu_pd(rx.as_ptr()))
        };
        let b = if y_same {
            _mm256_set1_pd(smooth(ry[0]))
        } else {
            smoothv(_mm256_loadu_pd(ry.as_ptr()))
        };
        let c = if z_same {
            _mm256_set1_pd(smooth(rz[0]))
        } else {
            smoothv(_mm256_loadu_pd(rz.as_ptr()))
        };
        // lerp3's seven stages, in the exact order of the scalar helper.
        let l0 = _mm256_add_pd(v0, _mm256_mul_pd(a, _mm256_sub_pd(v1, v0)));
        let l1 = _mm256_add_pd(v2, _mm256_mul_pd(a, _mm256_sub_pd(v3, v2)));
        let l2 = _mm256_add_pd(v4, _mm256_mul_pd(a, _mm256_sub_pd(v5, v4)));
        let l3 = _mm256_add_pd(v6, _mm256_mul_pd(a, _mm256_sub_pd(v7, v6)));
        let m0 = _mm256_add_pd(l0, _mm256_mul_pd(b, _mm256_sub_pd(l1, l0)));
        let m1 = _mm256_add_pd(l2, _mm256_mul_pd(b, _mm256_sub_pd(l3, l2)));
        let r = _mm256_add_pd(m0, _mm256_mul_pd(c, _mm256_sub_pd(m1, m0)));
        // SAFETY: `__m256d` is four `f64` lanes with no padding.
        std::mem::transmute::<__m256d, [f64; 4]>(r)
    }

    /// Eight scalar [`Perlin::get`] values, bit for bit. Layer A2-c: the same contract as
    /// [`Perlin::get_4`], but on one AVX-512 `__m512d`. The wrap, permutation hashes and eight
    /// gradient dots stay scalar (same order); only `smooth` and `lerp3`'s seven stages run
    /// across lanes. No FMA is enabled on the kernel, so every add/mul/sub rounds exactly as
    /// the scalar helper does. A host without AVX-512F/DQ keeps the two-`get_4` path.
    pub(crate) fn get_8(&self, x: [f64; 8], y: [f64; 8], z: [f64; 8]) -> [f64; 8] {
        #[cfg(target_arch = "x86_64")]
        {
            static AVX512: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            if *AVX512.get_or_init(|| {
                std::arch::is_x86_feature_detected!("avx512f")
                    && std::arch::is_x86_feature_detected!("avx512dq")
            }) {
                // SAFETY: both AVX-512 features were just detected at runtime.
                return unsafe { self.get_8_avx512(x, y, z) };
            }
        }
        let a = self.get_4([x[0], x[1], x[2], x[3]], [y[0], y[1], y[2], y[3]], [z[0], z[1], z[2], z[3]]);
        let b = self.get_4([x[4], x[5], x[6], x[7]], [y[4], y[5], y[6], y[7]], [z[4], z[5], z[6], z[7]]);
        std::array::from_fn(|k| if k < 4 { a[k] } else { b[k - 4] })
    }

    /// AVX-512 kernel behind [`Perlin::get_8`].
    ///
    /// # Safety
    /// The caller must have checked `is_x86_feature_detected!("avx512f")` and
    /// `is_x86_feature_detected!("avx512dq")`.
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx512f,avx512dq")]
    unsafe fn get_8_avx512(&self, x: [f64; 8], y: [f64; 8], z: [f64; 8]) -> [f64; 8] {
        use std::arch::x86_64::*;
        let x0b = x[0].to_bits();
        let x_same = x[1..].iter().all(|v| v.to_bits() == x0b);
        let y0b = y[0].to_bits();
        let y_same = y[1..].iter().all(|v| v.to_bits() == y0b);
        let z0b = z[0].to_bits();
        let z_same = z[1..].iter().all(|v| v.to_bits() == z0b);
        let mut ix = [0i32; 8];
        let mut iy = [0i32; 8];
        let mut iz = [0i32; 8];
        let mut rx = [0.0f64; 8];
        let mut ry = [0.0f64; 8];
        let mut rz = [0.0f64; 8];
        if x_same {
            let wx = wrap(x[0]) + self.xo;
            let fx = wx.floor() as i32;
            ix = [fx; 8];
            rx = [wx - fx as f64; 8];
        } else {
            for k in 0..8 {
                let wx = wrap(x[k]) + self.xo;
                ix[k] = wx.floor() as i32;
                rx[k] = wx - ix[k] as f64;
            }
        }
        if y_same {
            let wy = wrap(y[0]) + self.yo;
            let fy = wy.floor() as i32;
            iy = [fy; 8];
            ry = [wy - fy as f64; 8];
        } else {
            for k in 0..8 {
                let wy = wrap(y[k]) + self.yo;
                iy[k] = wy.floor() as i32;
                ry[k] = wy - iy[k] as f64;
            }
        }
        if z_same {
            let wz = wrap(z[0]) + self.zo;
            let fz = wz.floor() as i32;
            iz = [fz; 8];
            rz = [wz - fz as f64; 8];
        } else {
            for k in 0..8 {
                let wz = wrap(z[k]) + self.zo;
                iz[k] = wz.floor() as i32;
                rz[k] = wz - iz[k] as f64;
            }
        }
        // The eight gradient dots per lane, in `sample_lerp`'s order and with the
        // scalar grad_dot operation order `((g0*x) + (g1*y)) + (g2*z)`.
        let mut d0 = [0.0f64; 8];
        let mut d1 = [0.0f64; 8];
        let mut d2 = [0.0f64; 8];
        let mut d3 = [0.0f64; 8];
        let mut d4 = [0.0f64; 8];
        let mut d5 = [0.0f64; 8];
        let mut d6 = [0.0f64; 8];
        let mut d7 = [0.0f64; 8];
        if x_same && z_same {
            let x0 = self.perm(ix[0]);
            let x1 = self.perm(ix[0] + 1);
            let pz = iz[0];
            let dx = rx[0];
            let dx1 = dx - 1.0;
            let dz = rz[0];
            let dz1 = dz - 1.0;
            for k in 0..8 {
                let py = iy[k];
                let dy = ry[k];
                let dy1 = dy - 1.0;
                let xy00 = self.perm(x0 + py);
                let xy01 = self.perm(x0 + py + 1);
                let xy10 = self.perm(x1 + py);
                let xy11 = self.perm(x1 + py + 1);
                d0[k] = Self::grad_dot(self.perm(xy00 + pz), dx, dy, dz);
                d1[k] = Self::grad_dot(self.perm(xy10 + pz), dx1, dy, dz);
                d2[k] = Self::grad_dot(self.perm(xy01 + pz), dx, dy1, dz);
                d3[k] = Self::grad_dot(self.perm(xy11 + pz), dx1, dy1, dz);
                d4[k] = Self::grad_dot(self.perm(xy00 + pz + 1), dx, dy, dz1);
                d5[k] = Self::grad_dot(self.perm(xy10 + pz + 1), dx1, dy, dz1);
                d6[k] = Self::grad_dot(self.perm(xy01 + pz + 1), dx, dy1, dz1);
                d7[k] = Self::grad_dot(self.perm(xy11 + pz + 1), dx1, dy1, dz1);
            }
        } else {
            for k in 0..8 {
                let (px, py, pz) = (ix[k], iy[k], iz[k]);
                let (dx, dy, dz) = (rx[k], ry[k], rz[k]);
                let x0 = self.perm(px);
                let x1 = self.perm(px + 1);
                let xy00 = self.perm(x0 + py);
                let xy01 = self.perm(x0 + py + 1);
                let xy10 = self.perm(x1 + py);
                let xy11 = self.perm(x1 + py + 1);
                d0[k] = Self::grad_dot(self.perm(xy00 + pz), dx, dy, dz);
                d1[k] = Self::grad_dot(self.perm(xy10 + pz), dx - 1.0, dy, dz);
                d2[k] = Self::grad_dot(self.perm(xy01 + pz), dx, dy - 1.0, dz);
                d3[k] = Self::grad_dot(self.perm(xy11 + pz), dx - 1.0, dy - 1.0, dz);
                d4[k] = Self::grad_dot(self.perm(xy00 + pz + 1), dx, dy, dz - 1.0);
                d5[k] = Self::grad_dot(self.perm(xy10 + pz + 1), dx - 1.0, dy, dz - 1.0);
                d6[k] = Self::grad_dot(self.perm(xy01 + pz + 1), dx, dy - 1.0, dz - 1.0);
                d7[k] = Self::grad_dot(self.perm(xy11 + pz + 1), dx - 1.0, dy - 1.0, dz - 1.0);
            }
        }
        let v0 = _mm512_loadu_pd(d0.as_ptr());
        let v1 = _mm512_loadu_pd(d1.as_ptr());
        let v2 = _mm512_loadu_pd(d2.as_ptr());
        let v3 = _mm512_loadu_pd(d3.as_ptr());
        let v4 = _mm512_loadu_pd(d4.as_ptr());
        let v5 = _mm512_loadu_pd(d5.as_ptr());
        let v6 = _mm512_loadu_pd(d6.as_ptr());
        let v7 = _mm512_loadu_pd(d7.as_ptr());
        // smooth(t) = t*t*t*(t*(t*6-15)+10), same op order as scalar `smooth`.
        let smoothv = |t: __m512d| -> __m512d {
            let t2 = _mm512_mul_pd(t, t);
            let t3 = _mm512_mul_pd(t2, t);
            let a = _mm512_mul_pd(t, _mm512_set1_pd(6.0));
            let b = _mm512_sub_pd(a, _mm512_set1_pd(15.0));
            let c = _mm512_mul_pd(t, b);
            let e = _mm512_add_pd(c, _mm512_set1_pd(10.0));
            _mm512_mul_pd(t3, e)
        };
        let a = if x_same {
            _mm512_set1_pd(smooth(rx[0]))
        } else {
            smoothv(_mm512_loadu_pd(rx.as_ptr()))
        };
        let b = if y_same {
            _mm512_set1_pd(smooth(ry[0]))
        } else {
            smoothv(_mm512_loadu_pd(ry.as_ptr()))
        };
        let c = if z_same {
            _mm512_set1_pd(smooth(rz[0]))
        } else {
            smoothv(_mm512_loadu_pd(rz.as_ptr()))
        };
        // lerp3's seven stages, in the exact order of the scalar helper.
        let l0 = _mm512_add_pd(v0, _mm512_mul_pd(a, _mm512_sub_pd(v1, v0)));
        let l1 = _mm512_add_pd(v2, _mm512_mul_pd(a, _mm512_sub_pd(v3, v2)));
        let l2 = _mm512_add_pd(v4, _mm512_mul_pd(a, _mm512_sub_pd(v5, v4)));
        let l3 = _mm512_add_pd(v6, _mm512_mul_pd(a, _mm512_sub_pd(v7, v6)));
        let m0 = _mm512_add_pd(l0, _mm512_mul_pd(b, _mm512_sub_pd(l1, l0)));
        let m1 = _mm512_add_pd(l2, _mm512_mul_pd(b, _mm512_sub_pd(l3, l2)));
        let r = _mm512_add_pd(m0, _mm512_mul_pd(c, _mm512_sub_pd(m1, m0)));
        // SAFETY: `__m512d` is eight `f64` lanes with no padding.
        std::mem::transmute::<__m512d, [f64; 8]>(r)
    }
    // smeared variant (old_blended_noise): y snapped up within fudge cells
    fn get_smeared(&self, x: f64, oy: f64, z: f64, fudge_y_scale: f64) -> f64 {
        let x = wrap(x) + self.xo;
        let y = wrap(oy) + self.yo;
        let z = wrap(z) + self.zo;
        let fx = x.floor() as i32;
        let fy = y.floor() as i32;
        let fz = z.floor() as i32;
        let rx = x - fx as f64;
        let ry = y - fy as f64;
        let rz = z - fz as f64;
        let lim = if oy >= 0.0 && oy < ry { oy } else { ry };
        let fudge = (lim / fudge_y_scale + 1.0e-7).floor() * fudge_y_scale;
        self.sample_lerp(fx, fy, fz, rx, ry - fudge, rz, ry)
    }
}

const PERSISTENCE: f64 = 0.5;
const LACUNARITY: f64 = 2.0;
const STD_DEV: f64 = 0.270_224_783_124_521_1;
const TARGET_DEV: f64 = 1.0 / 3.0;
const INPUT_FACTOR: f64 = 1.018_126_888_217_522_7;
/// `NormalNoise`'s numerator for its final value factor; divided by the
/// expected deviation of the first-to-last nonzero amplitude span.
const NATIVE_VALUE_FACTOR_NUMERATOR: f64 = 0.166_666_666_666_666_66;

#[derive(Clone)]
pub(crate) struct Octave {
    n: Perlin,
    f: f64,
    a: f64,
}
/// A direct-mapped, thread-local cache of `Normal::get` values. The material rule reaches the
/// same noise at the same coordinate several times while evaluating one cell, and the function is
/// pure, so caching the value is exact. A fixed array (not a hash map) keeps the hit path to a
/// hash, an index, and one comparison.
const NCACHE_N: usize = 8192;
type NKey = (usize, u64, u64, u64);
thread_local! {
    static NCACHE: std::cell::RefCell<Vec<(NKey, f64)>> =
        std::cell::RefCell::new(vec![((0usize, 0, 0, 0), 0.0); NCACHE_N]);
}
#[inline]
fn nhash(k: NKey) -> usize {
    let mut h = k.0 as u64;
    h = (h ^ k.1).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    h = (h ^ k.2).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    h = (h ^ k.3).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    // Integral block coordinates have zero low mantissa bits. Fold in the high
    // bits before indexing, or many distinct positions share the same slot.
    h = (h ^ (h >> 32)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    (h >> 32) as usize & (NCACHE_N - 1)
}
/// Native 1.21.1 `NormalNoise` summation shape. The first `split` layers are
/// one `PerlinNoise`, the rest the other; `value_factor` is applied once after
/// the two stacks are added.
#[derive(Clone)]
struct NativeNormal {
    split: usize,
    value_factor: f64,
    octave_weights: Vec<f64>,
}
static NEXT_NORMAL_ID: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);
#[derive(Clone)]
pub(crate) struct Normal {
    // Addresses can be reused after drop (or change on move). Cache identity
    // belongs to the sampler's lifetime, not its current allocation address.
    cache_id: usize,
    layers: Vec<Octave>,
    /// `Some` for the native `firstOctave`/`amplitudes` encoding, whose layers
    /// keep their unnormalised per-octave weights. `None` for the converted
    /// encoding, whose normalization is already folded into each `a`.
    native: Option<NativeNormal>,
}
impl Normal {
    pub(crate) fn get_uncached(&self, x: f64, y: f64, z: f64) -> f64 {
        match &self.native {
            Some(native) => {
                let mut first = 0.0;
                for (o, weight) in self.layers[..native.split]
                    .iter()
                    .zip(&native.octave_weights)
                {
                    first += o.a * o.n.get(x * o.f, y * o.f, z * o.f) * weight;
                }
                let ix = x * INPUT_FACTOR;
                let iy = y * INPUT_FACTOR;
                let iz = z * INPUT_FACTOR;
                let mut second = 0.0;
                for (o, weight) in self.layers[native.split..]
                    .iter()
                    .zip(&native.octave_weights)
                {
                    second += o.a * o.n.get(ix * o.f, iy * o.f, iz * o.f) * weight;
                }
                (first + second) * native.value_factor
            }
            None => {
                let mut v = 0.0;
                for o in &self.layers {
                    v += o.a * o.n.get(x * o.f, y * o.f, z * o.f);
                }
                v
            }
        }
    }

    pub(crate) fn get(&self, x: f64, y: f64, z: f64) -> f64 {
        let key = (self.cache_id, x.to_bits(), y.to_bits(), z.to_bits());
        let idx = nhash(key);
        // Probe and store share one `RefCell` borrow: `get_uncached` never re-enters
        // NCACHE (it evaluates `Perlin::get` octaves only), so the mutable borrow can
        // span the miss evaluation.
        NCACHE.with(|c| {
            let mut c = c.borrow_mut();
            let slot = c[idx];
            if slot.0 == key {
                slot.1
            } else {
                let v = self.get_uncached(x, y, z);
                c[idx] = (key, v);
                v
            }
        })
    }

    /// Four scalar [`Normal::get`] values, bit for bit. NCACHE probes share one
    /// `RefCell` borrow. The common batch path stores misses together and uses
    /// [`Perlin::get_4`] for each octave, preserving scalar accumulation order.
    pub(crate) fn get_4(&self, x: [f64; 4], y: [f64; 4], z: [f64; 4]) -> [f64; 4] {
        let key: [NKey; 4] = std::array::from_fn(|k| {
            (
                self.cache_id,
                x[k].to_bits(),
                y[k].to_bits(),
                z[k].to_bits(),
            )
        });
        let idx: [usize; 4] = std::array::from_fn(|k| nhash(key[k]));
        let cached: [Option<f64>; 4] = NCACHE.with(|c| {
            let c = c.borrow();
            std::array::from_fn(|k| {
                let slot = c[idx[k]];
                if slot.0 == key[k] {
                    Some(slot.1)
                } else {
                    None
                }
            })
        });
        if cached.iter().all(Option::is_some) {
            return cached.map(Option::unwrap);
        }
        // A mostly warm batch is cheaper as individual scalar misses than as
        // an entire four-lane octave evaluation. The scalar path preserves the
        // exact octave accumulation and cache key semantics.
        if cached.iter().filter(|v| v.is_none()).count() == 1 {
            return std::array::from_fn(|k| {
                cached[k].unwrap_or_else(|| self.get(x[k], y[k], z[k]))
            });
        }
        let vals = self.eval_x4_uncached(x, y, z);
        NCACHE.with(|c| {
            let mut c = c.borrow_mut();
            for k in 0..4 {
                if cached[k].is_none() {
                    c[idx[k]] = (key[k], vals[k]);
                }
            }
        });
        vals
    }

    /// `Normal::get`'s body without the NCACHE, evaluated for four lanes at once.
    /// Each octave's four Perlin samples are one [`Perlin::get_4`] call; the scalar
    /// per-lane multiply and accumulate order is preserved exactly.
    pub(crate) fn eval_x4_uncached(&self, x: [f64; 4], y: [f64; 4], z: [f64; 4]) -> [f64; 4] {
        match &self.native {
            Some(native) => {
                let mut first = [0.0f64; 4];
                for (o, weight) in self.layers[..native.split]
                    .iter()
                    .zip(&native.octave_weights)
                {
                    let nx = std::array::from_fn(|k| x[k] * o.f);
                    let ny = std::array::from_fn(|k| y[k] * o.f);
                    let nz = std::array::from_fn(|k| z[k] * o.f);
                    let nv = o.n.get_4(nx, ny, nz);
                    for k in 0..4 {
                        first[k] += o.a * nv[k] * weight;
                    }
                }
                // Java multiplies the coordinate by INPUT_FACTOR before the second
                // PerlinNoise applies its per-octave frequency scaling.
                let ix: [f64; 4] = std::array::from_fn(|k| x[k] * INPUT_FACTOR);
                let iy: [f64; 4] = std::array::from_fn(|k| y[k] * INPUT_FACTOR);
                let iz: [f64; 4] = std::array::from_fn(|k| z[k] * INPUT_FACTOR);
                let mut second = [0.0f64; 4];
                for (o, weight) in self.layers[native.split..]
                    .iter()
                    .zip(&native.octave_weights)
                {
                    let nx = std::array::from_fn(|k| ix[k] * o.f);
                    let ny = std::array::from_fn(|k| iy[k] * o.f);
                    let nz = std::array::from_fn(|k| iz[k] * o.f);
                    let nv = o.n.get_4(nx, ny, nz);
                    for k in 0..4 {
                        second[k] += o.a * nv[k] * weight;
                    }
                }
                std::array::from_fn(|k| (first[k] + second[k]) * native.value_factor)
            }
            None => {
                let mut v = [0.0f64; 4];
                for o in &self.layers {
                    let nx = std::array::from_fn(|k| x[k] * o.f);
                    let ny = std::array::from_fn(|k| y[k] * o.f);
                    let nz = std::array::from_fn(|k| z[k] * o.f);
                    let nv = o.n.get_4(nx, ny, nz);
                    for k in 0..4 {
                        v[k] += o.a * nv[k];
                    }
                }
                v
            }
        }
    }

    /// Eight scalar [`Normal::get`] values, bit for bit (layer A2-c). NCACHE
    /// probes share one `RefCell` borrow. The common batch path stores misses
    /// together and uses [`Perlin::get_8`] for each octave, preserving scalar
    /// accumulation order.
    pub(crate) fn get_8(&self, x: [f64; 8], y: [f64; 8], z: [f64; 8]) -> [f64; 8] {
        let key: [NKey; 8] = std::array::from_fn(|k| {
            (
                self.cache_id,
                x[k].to_bits(),
                y[k].to_bits(),
                z[k].to_bits(),
            )
        });
        let idx: [usize; 8] = std::array::from_fn(|k| nhash(key[k]));
        let cached: [Option<f64>; 8] = NCACHE.with(|c| {
            let c = c.borrow();
            std::array::from_fn(|k| {
                let slot = c[idx[k]];
                if slot.0 == key[k] {
                    Some(slot.1)
                } else {
                    None
                }
            })
        });
        if cached.iter().all(Option::is_some) {
            return cached.map(Option::unwrap);
        }
        // The SIMD evaluator computes all eight octaves even for cache hits.
        // Re-evaluate the one or two misses through the identical scalar path.
        if cached.iter().filter(|v| v.is_none()).count() <= 2 {
            return std::array::from_fn(|k| {
                cached[k].unwrap_or_else(|| self.get(x[k], y[k], z[k]))
            });
        }
        let vals = self.eval_x8_uncached(x, y, z);
        NCACHE.with(|c| {
            let mut c = c.borrow_mut();
            for k in 0..8 {
                if cached[k].is_none() {
                    c[idx[k]] = (key[k], vals[k]);
                }
            }
        });
        vals
    }

    /// `Normal::get`'s body without the NCACHE, evaluated for eight lanes at once. Each
    /// octave's eight Perlin samples are one [`Perlin::get_8`] call; the scalar per-lane
    /// multiply and accumulate order is preserved exactly.
    pub(crate) fn eval_x8_uncached(&self, x: [f64; 8], y: [f64; 8], z: [f64; 8]) -> [f64; 8] {
        match &self.native {
            Some(native) => {
                let mut first = [0.0f64; 8];
                for (o, weight) in self.layers[..native.split]
                    .iter()
                    .zip(&native.octave_weights)
                {
                    let nx = std::array::from_fn(|k| x[k] * o.f);
                    let ny = std::array::from_fn(|k| y[k] * o.f);
                    let nz = std::array::from_fn(|k| z[k] * o.f);
                    let nv = o.n.get_8(nx, ny, nz);
                    for k in 0..8 {
                        first[k] += o.a * nv[k] * weight;
                    }
                }
                // Java multiplies the coordinate by INPUT_FACTOR before the second
                // PerlinNoise applies its per-octave frequency scaling.
                let ix: [f64; 8] = std::array::from_fn(|k| x[k] * INPUT_FACTOR);
                let iy: [f64; 8] = std::array::from_fn(|k| y[k] * INPUT_FACTOR);
                let iz: [f64; 8] = std::array::from_fn(|k| z[k] * INPUT_FACTOR);
                let mut second = [0.0f64; 8];
                for (o, weight) in self.layers[native.split..]
                    .iter()
                    .zip(&native.octave_weights)
                {
                    let nx = std::array::from_fn(|k| ix[k] * o.f);
                    let ny = std::array::from_fn(|k| iy[k] * o.f);
                    let nz = std::array::from_fn(|k| iz[k] * o.f);
                    let nv = o.n.get_8(nx, ny, nz);
                    for k in 0..8 {
                        second[k] += o.a * nv[k] * weight;
                    }
                }
                std::array::from_fn(|k| (first[k] + second[k]) * native.value_factor)
            }
            None => {
                let mut v = [0.0f64; 8];
                for o in &self.layers {
                    let nx = std::array::from_fn(|k| x[k] * o.f);
                    let ny = std::array::from_fn(|k| y[k] * o.f);
                    let nz = std::array::from_fn(|k| z[k] * o.f);
                    let nv = o.n.get_8(nx, ny, nz);
                    for k in 0..8 {
                        v[k] += o.a * nv[k];
                    }
                }
                v
            }
        }
    }
}

fn build_octaves(
    base_octave: i32,
    base_amplitude: f64,
    count: usize,
    mods: &[f64],
) -> Vec<(i32, f64, f64)> {
    let mut f = LACUNARITY.powi(base_octave);
    let mut a = base_amplitude
        * (PERSISTENCE.powi(-(count as i32 - 1)) / (PERSISTENCE.powi(-(count as i32)) - 1.0));
    let mut out = Vec::new();
    for i in 0..count {
        let m = if i < mods.len() { mods[i] } else { 1.0 };
        if m != 0.0 {
            out.push((base_octave + i as i32, f, a * m));
        }
        f *= LACUNARITY;
        a *= PERSISTENCE;
    }
    out
}
fn estimate_dev(octaves: &[(i32, f64, f64)]) -> f64 {
    let mut var = 0.0;
    for (_, _, a) in octaves {
        let d = STD_DEV * a.abs();
        var += d * d;
    }
    var.sqrt()
}
fn norm_factor(target: f64, octaves: &[(i32, f64, f64)]) -> f64 {
    let d = estimate_dev(octaves);
    if d == 0.0 {
        0.0
    } else {
        (target * TARGET_DEV) / (d * std::f64::consts::SQRT_2)
    }
}

// modern (non-legacy) NormalNoise: JSON carries precomputed base_amplitude
pub(crate) fn normal_new(
    r: &mut Xor,
    base_octave: i32,
    base_amplitude: f64,
    mods: &[f64],
) -> Normal {
    let octaves = build_octaves(base_octave, base_amplitude, mods.len(), mods);
    normal_from_octaves(r, octaves)
}

/// The 1.21.1 encoding of the same construction: `firstOctave` plus one amplitude per
/// octave, instead of `base_octave`/`base_amplitude` plus modifiers. This is the
/// ordinary `NormalNoise.create` entry point; legacy Nether noise has an explicit
/// alternate constructor. Router noises use name-derived Xoroshiro sources.
/// Preserve `amplitude * sample * octave_weight`, then add both Perlin stacks
/// and multiply by the span-derived value factor: precombining weights changes bits.
pub(crate) fn normal_new_amps(r: &mut Xor, first_octave: i32, amps: &[f64]) -> Normal {
    let first = r.splitter();
    let second = r.splitter();
    normal_native(first_octave, amps, |stack, octave| {
        let factory = if stack == 0 { &first } else { &second };
        Perlin::new(&mut factory.of_str(&format!("octave_{octave}")))
    })
}

/// `NormalNoise`'s span for `expectedDeviation`: the last nonzero amplitude
/// index minus the first, over the original list (gaps count). With no nonzero
/// amplitude Java's sentinels make the subtraction wrap to 1, so mirror that
/// instead of special-casing the all-zero case.
fn native_amplitude_span(amps: &[f64]) -> i32 {
    let mut min_o = i32::MAX;
    let mut max_o = i32::MIN;
    for (i, a) in amps.iter().enumerate() {
        if *a != 0.0 {
            min_o = min_o.min(i as i32);
            max_o = max_o.max(i as i32);
        }
    }
    max_o.wrapping_sub(min_o)
}
fn native_value_factor(amps: &[f64]) -> f64 {
    let span = native_amplitude_span(amps);
    NATIVE_VALUE_FACTOR_NUMERATOR / (0.1 * (1.0 + 1.0 / span.wrapping_add(1) as f64))
}

/// Native `NormalNoise` builds the two `PerlinNoise` stacks with the same
/// positional forks and octave names as the converted encoding, but keeps the
/// per-octave weights separate and applies the final factor only after summing
/// both stacks.
fn normal_native(
    first_octave: i32,
    amps: &[f64],
    mut perlin: impl FnMut(usize, i32) -> Perlin,
) -> Normal {
    let count = amps.len() as i32;
    let mut layers = Vec::with_capacity(amps.len() * 2);
    let mut octave_weights = Vec::new();
    for stack in 0..2 {
        let mut frequency = LACUNARITY.powi(first_octave);
        let mut weight = LACUNARITY.powi(count - 1) / (LACUNARITY.powi(count) - 1.0);
        for (i, &amplitude) in amps.iter().enumerate() {
            if amplitude != 0.0 {
                layers.push(Octave {
                    n: perlin(stack, first_octave + i as i32),
                    f: frequency,
                    a: amplitude,
                });
                if stack == 0 {
                    octave_weights.push(weight);
                }
            }
            frequency *= LACUNARITY;
            weight /= LACUNARITY;
        }
    }
    Normal {
        cache_id: NEXT_NORMAL_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        native: Some(NativeNormal {
            split: layers.len() / 2,
            value_factor: native_value_factor(amps),
            octave_weights,
        }),
        layers,
    }
}

fn normal_from_octaves(r: &mut Xor, octaves: Vec<(i32, f64, f64)>) -> Normal {
    let target: f64 = octaves.iter().map(|o| o.2.abs()).sum();
    let nf = norm_factor(target, &octaves);
    let first = r.splitter();
    let second = r.splitter();
    let mut layers = Vec::with_capacity(octaves.len() * 2);
    for (idx, f, a) in octaves {
        let vf = nf * a;
        layers.push(Octave {
            n: Perlin::new(&mut first.of_str(&format!("octave_{}", idx))),
            f,
            a: vf,
        });
        layers.push(Octave {
            n: Perlin::new(&mut second.of_str(&format!("octave_{}", idx))),
            f: f * INPUT_FACTOR,
            a: vf,
        });
    }
    Normal {
        cache_id: NEXT_NORMAL_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        layers,
        native: None,
    }
}

/// NormalNoise.create over WorldgenRandom(LegacyRandomSource(seed)). The
/// source is legacy, but the constructor is NOT createLegacyNetherBiome:
/// each stack forks once, and each nonzero octave uses its named positional
/// source. Vegetation providers and geodes share this construction.
pub(crate) fn normal_new_l(seed: u64, first_octave: i32, amps: &[f64]) -> Normal {
    let mut r = Lcg::new(seed);
    let seeds = [r.next_i64() as u64, r.next_i64() as u64];
    normal_native(first_octave, amps, |stack, octave| {
        Perlin::new_l(&mut Lcg::split_str(
            seeds[stack],
            &format!("octave_{octave}"),
        ))
    })
}

/// RandomState's legacy Nether temperature/vegetation override: two consecutive
/// PerlinNoise.createLegacyForLegacyNetherBiome stacks, octaves -7 and -6.
pub(crate) fn normal_legacy_nether(seed: u64) -> Normal {
    let mut r = Lcg::new(seed);
    let mut octaves = Vec::with_capacity(4);
    for _ in 0..2 {
        // Constructor creates octave zero even when it is outside the array.
        let _ = Perlin::new_l(&mut r);
        for _ in (2..7).rev() {
            r.skip(262);
        }
        let high = Perlin::new_l(&mut r);
        let low = Perlin::new_l(&mut r);
        octaves.extend([low, high]);
    }
    let mut iter = octaves.into_iter();
    normal_native(-7, &[1.0, 1.0], |_, _| iter.next().unwrap())
}

pub(crate) fn legacy_world_factory_seed(seed: u64) -> u64 {
    Lcg::new(seed).next_i64() as u64
}

pub(crate) fn legacy_named_factory_seed(seed: u64, name: &str) -> u64 {
    Lcg::split_str(legacy_world_factory_seed(seed), name).next_i64() as u64
}

pub(crate) fn normal_legacy_named(seed: u64, name: &str, first: i32, amps: &[f64]) -> Normal {
    let seed = legacy_world_factory_seed(seed) ^ java_hash(name) as i64 as u64;
    normal_new_l(seed, first, amps)
}

// ---------- simplex (foliage / noise-based placement modifiers) ----------

#[derive(Clone)]
pub(crate) struct Simp {
    p: [u8; 256],
}
impl Simp {
    // consumes the same draws as Perlin::new_l (3 offsets + 256 swaps);
    // oct/simplex still keeps origin offsets but 2D callers use use_origin=false.
    pub(crate) fn new_l(r: &mut Lcg) -> (Self, f64, f64, f64) {
        let xo = r.next_f64() * 256.0;
        let yo = r.next_f64() * 256.0;
        let zo = r.next_f64() * 256.0;
        let mut p = [0u8; 256];
        for (i, v) in p.iter_mut().enumerate() {
            *v = i as u8;
        }
        for i in 0..256usize {
            let off = r.next_bound((256 - i) as i32) as usize;
            p.swap(i, off + i);
        }
        (Simp { p }, xo, yo, zo)
    }
    #[inline]
    fn map(&self, x: i32) -> i32 {
        self.p[(x & 0xFF) as usize] as i32
    }
    fn grad(t: usize, x: f64, y: f64) -> f64 {
        let d = 0.5 - x * x - y * y;
        if d < 0.0 {
            return 0.0;
        }
        let g = &GRAD[t % 12];
        let dd = d * d;
        dd * dd * (g[0] * x + g[1] * y)
    }
    fn grad3(t: usize, x: f64, y: f64, z: f64) -> f64 {
        let d = 0.6 - x * x - y * y - z * z;
        if d < 0.0 {
            return 0.0;
        }
        let g = &GRAD[t % 12];
        let dd = d * d;
        dd * dd * (g[0] * x + g[1] * y + g[2] * z)
    }
    pub(crate) fn get3(&self, x: f64, y: f64, z: f64) -> f64 {
        let e = (x + y + z) * (1.0 / 3.0);
        let i = (x + e).floor() as i32;
        let j = (y + e).floor() as i32;
        let k = (z + e).floor() as i32;
        let g = i.wrapping_add(j).wrapping_add(k) as f64 * (1.0 / 6.0);
        let n = x - (i as f64 - g);
        let o = y - (j as f64 - g);
        let p = z - (k as f64 - g);
        let (q, r, s, t, u, v) = if n >= o {
            if o >= p {
                (1, 0, 0, 1, 1, 0)
            } else if n >= p {
                (1, 0, 0, 1, 0, 1)
            } else {
                (0, 0, 1, 1, 0, 1)
            }
        } else if o < p {
            (0, 0, 1, 0, 1, 1)
        } else if n < p {
            (0, 1, 0, 0, 1, 1)
        } else {
            (0, 1, 0, 1, 1, 0)
        };
        let w = n - q as f64 + 1.0 / 6.0;
        let aa = o - r as f64 + 1.0 / 6.0;
        let ab = p - s as f64 + 1.0 / 6.0;
        let ac = n - t as f64 + 1.0 / 3.0;
        let ad = o - u as f64 + 1.0 / 3.0;
        let ae = p - v as f64 + 1.0 / 3.0;
        let af = n - 0.5;
        let ag = o - 0.5;
        let ah = p - 0.5;
        let ai = i & 0xFF;
        let aj = j & 0xFF;
        let ak = k & 0xFF;
        let al = self.map(ai.wrapping_add(self.map(aj.wrapping_add(self.map(ak))))) % 12;
        let am = self.map(
            ai.wrapping_add(q).wrapping_add(
                self.map(
                    aj.wrapping_add(r)
                        .wrapping_add(self.map(ak.wrapping_add(s))),
                ),
            ),
        ) % 12;
        let an = self.map(
            ai.wrapping_add(t).wrapping_add(
                self.map(
                    aj.wrapping_add(u)
                        .wrapping_add(self.map(ak.wrapping_add(v))),
                ),
            ),
        ) % 12;
        let ao = self.map(
            ai.wrapping_add(1).wrapping_add(
                self.map(
                    aj.wrapping_add(1)
                        .wrapping_add(self.map(ak.wrapping_add(1))),
                ),
            ),
        ) % 12;
        32.0 * (Self::grad3(al as usize, n, o, p)
            + Self::grad3(am as usize, w, aa, ab)
            + Self::grad3(an as usize, ac, ad, ae)
            + Self::grad3(ao as usize, af, ag, ah))
    }
    pub(crate) fn get2(&self, x: f64, y: f64) -> f64 {
        const SK: f64 = 0.5 * (1.7320508075688772 - 1.0);
        const US: f64 = (3.0 - 1.7320508075688772) / 6.0;
        let d = (x + y) * SK;
        let i = (x + d).floor() as i32;
        let j = (y + d).floor() as i32;
        let e = (i.wrapping_add(j)) as f64 * US;
        let f = i as f64 - e;
        let g = j as f64 - e;
        let h = x - f;
        let k = y - g;
        let (l, m) = if h > k { (1i32, 0i32) } else { (0i32, 1i32) };
        let n = h - l as f64 + US;
        let o = k - m as f64 + US;
        let p = 2.0 * US + (h - 1.0);
        let q = 2.0 * US + (k - 1.0);
        let r = i & 0xFF;
        let s = j & 0xFF;
        let t = self.map(r.wrapping_add(self.map(s)));
        let u = self.map(r.wrapping_add(l).wrapping_add(self.map(s.wrapping_add(m))));
        let v = self.map(r.wrapping_add(1).wrapping_add(self.map(s.wrapping_add(1))));
        let w = Self::grad(t as usize, h, k);
        let z = Self::grad(u as usize, n, o);
        let aa = Self::grad(v as usize, p, q);
        70.0 * (w + z + aa)
    }
}

/// DensityFunctions.EndIslandDensityFunction's seeded simplex field. Float
/// geometry, Java int overflow and truncating divisions are observable here.
#[derive(Clone)]
pub(crate) struct EndIslands {
    noise: Simp,
}
impl EndIslands {
    pub(crate) fn new(seed: u64) -> Self {
        let mut random = Lcg::new(seed);
        random.skip(17292);
        let (noise, _, _, _) = Simp::new_l(&mut random);
        Self { noise }
    }

    pub(crate) fn height(&self, x: i32, z: i32) -> f32 {
        let (cx, cz, rx, rz) = (x / 2, z / 2, x % 2, z % 2);
        let sqrt = |value: f32| f64::from(value).sqrt() as f32;
        let sum = x.wrapping_mul(x).wrapping_add(z.wrapping_mul(z));
        let mut height = (100.0 - sqrt(sum as f32) * 8.0).clamp(-100.0, 80.0);
        for dx in -12i32..=12 {
            for dz in -12i32..=12 {
                let nx = i64::from(cx.wrapping_add(dx));
                let nz = i64::from(cz.wrapping_add(dz));
                if nx * nx + nz * nz > 4096
                    && self.noise.get2(nx as f64, nz as f64) < f64::from(-0.9f32)
                {
                    let scale =
                        ((nx as f32).abs() * 3439.0 + (nz as f32).abs() * 147.0) % 13.0 + 9.0;
                    let ox = (rx - dx * 2) as f32;
                    let oz = (rz - dz * 2) as f32;
                    let island = (100.0 - sqrt(ox * ox + oz * oz) * scale).clamp(-100.0, 80.0);
                    // Java Math.max propagates NaN; Rust f32::max ignores it.
                    height = if height.is_nan() {
                        height
                    } else if island.is_nan() {
                        island
                    } else if height == 0.0 && island == 0.0 {
                        f32::from_bits(height.to_bits() & island.to_bits())
                    } else {
                        height.max(island)
                    };
                }
            }
        }
        height
    }

    pub(crate) fn density(&self, x: i32, z: i32) -> f64 {
        (f64::from(self.height(x / 8, z / 8)) - 8.0) / 128.0
    }
}

#[cfg(all(test, megu_vanilla_data))]
mod end_island_tests {
    use super::*;
    #[test]
    fn end_island_density_and_float_height_match_native() {
        let mut seed = None;
        let mut field = EndIslands::new(0);
        let mut count = 0;
        for row in include_str!("../tests/data/end_islands_1211.tsv").lines() {
            let f: Vec<_> = row.split('\t').collect();
            assert_eq!(f.len(), 5);
            let current = f[0].parse::<i64>().unwrap() as u64;
            if seed != Some(current) {
                field = EndIslands::new(current);
                seed = Some(current);
            }
            let (x, z) = (f[1].parse().unwrap(), f[2].parse().unwrap());
            assert_eq!(
                field.height(x, z).to_bits(),
                u32::from_str_radix(f[3], 16).unwrap(),
                "native height {row}"
            );
            assert_eq!(
                field.density(x, z).to_bits(),
                u64::from_str_radix(f[4], 16).unwrap(),
                "native density {row}"
            );
            count += 1;
        }
        assert_eq!(count, 560);
    }
}

// vanilla OctaveSimplexNoiseSampler over octave indices (can be negative)
pub(crate) struct OSimp {
    layers: Vec<Option<Simp>>,
    highest_octave: i32,
}
impl OSimp {
    pub(crate) fn new_l(r: &mut Lcg, octaves: &[i32]) -> Self {
        let mut oc: Vec<i32> = octaves.to_vec();
        oc.sort_unstable();
        oc.dedup();
        let (first, last) = match (oc.first(), oc.last()) {
            (Some(&a), Some(&b)) => (a, b),
            _ => {
                return OSimp {
                    layers: Vec::new(),
                    highest_octave: 0,
                }
            }
        };
        let j = last;
        let k = (-first).wrapping_add(j).wrapping_add(1);
        let mut layers: Vec<Option<Simp>> = Vec::with_capacity(k.max(0) as usize);
        for _ in 0..k.max(0) {
            layers.push(None);
        }
        let (s0, xo, yo, zo) = Simp::new_l(r);
        // PerlinSimplexNoise stores octave zero at the highest-octave index,
        // not at -first: those differ for the frozen-temperature stack [-2,-1,0].
        let zi = j;
        if zi >= 0 && (zi as usize) < layers.len() && oc.contains(&0) {
            layers[zi as usize] = Some(s0.clone());
        }
        for m in (j + 1)..k {
            let oi = j - m;
            if m >= 0 && oc.contains(&oi) {
                let (s, _, _, _) = Simp::new_l(r);
                layers[m as usize] = Some(s);
            } else {
                r.skip(262);
            }
        }
        if j > 0 {
            // negative-octave samplers come from a fresh LegacyRand seeded by
            // s0.sample_3d(origin) * 2^63
            let seed = (s0.get3(xo, yo, zo) * f64::from(9.223372E18f32)) as i64;
            let mut r2 = Lcg::new(seed as u64);
            for o in (0..j).rev() {
                let oi = j - o;
                if o < k && oc.contains(&oi) {
                    let (s, _, _, _) = Simp::new_l(&mut r2);
                    layers[o as usize] = Some(s);
                } else {
                    r2.skip(262);
                }
            }
        }
        OSimp {
            layers,
            highest_octave: j,
        }
    }
    pub(crate) fn sample(&self, x: f64, y: f64) -> f64 {
        let mut d = 0.0;
        let mut e = 2.0f64.powi(self.highest_octave);
        let mut f = 1.0 / (2.0f64.powi(self.layers.len() as i32) - 1.0);
        for l in self.layers.iter() {
            if let Some(s) = l {
                d += s.get2(x * e, y * e) * f;
            }
            e /= 2.0;
            f *= 2.0;
        }
        d
    }
}

/// BlendedNoise constructs descending octaves from one sequential random source.
/// RandomState chooses that source; both engines share the sampling arithmetic.
#[derive(Clone)]
pub(crate) struct BlendedNoise {
    min: Vec<Perlin>,
    max: Vec<Perlin>,
    main: Vec<Perlin>,
    xzm: f64,
    ym: f64,
    xzf: f64,
    yf: f64,
    smear: f64,
}
impl BlendedNoise {
    pub(crate) fn legacy(seed: u64, xzs: f64, ys: f64, xzf: f64, yf: f64, smear: f64) -> Self {
        let mut random = Lcg::new(seed);
        Self::build(|| Perlin::new_l(&mut random), xzs, ys, xzf, yf, smear)
    }

    pub(crate) fn modern(
        random: &mut Xor,
        xzs: f64,
        ys: f64,
        xzf: f64,
        yf: f64,
        smear: f64,
    ) -> Self {
        Self::build(|| Perlin::new(random), xzs, ys, xzf, yf, smear)
    }

    fn build(
        mut perlin: impl FnMut() -> Perlin,
        xzs: f64,
        ys: f64,
        xzf: f64,
        yf: f64,
        smear: f64,
    ) -> Self {
        let min = (0..16).map(|_| perlin()).collect();
        let max = (0..16).map(|_| perlin()).collect();
        let main = (0..8).map(|_| perlin()).collect();
        Self {
            min,
            max,
            main,
            xzm: 684.412 * xzs,
            ym: 684.412 * ys,
            xzf,
            yf,
            smear,
        }
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx512f,avx512dq")]
    unsafe fn sample_8_avx512(
        layers: &[Perlin],
        x: f64,
        y: f64,
        z: f64,
        smear: f64,
        mut freq: f64,
    ) -> ([f64; 8], [f64; 8], f64) {
        use std::arch::x86_64::*;
        let mut freqs = [0.0f64; 8];
        let mut rx = [0.0f64; 8];
        let mut ory = [0.0f64; 8];
        let mut rz = [0.0f64; 8];
        let mut d0 = [0.0f64; 8];
        let mut d1 = [0.0f64; 8];
        let mut d2 = [0.0f64; 8];
        let mut d3 = [0.0f64; 8];
        let mut d4 = [0.0f64; 8];
        let mut d5 = [0.0f64; 8];
        let mut d6 = [0.0f64; 8];
        let mut d7 = [0.0f64; 8];
        for k in 0..8 {
            let layer = &layers[k];
            let f = freq;
            freqs[k] = f;
            freq /= 2.0;
            let sx = x * f;
            let oy = y * f;
            let sz = z * f;
            let fudge_y_scale = smear * f;
            let wx = wrap(sx) + layer.xo;
            let wy = wrap(oy) + layer.yo;
            let wz = wrap(sz) + layer.zo;
            let px = wx.floor() as i32;
            let py = wy.floor() as i32;
            let pz = wz.floor() as i32;
            let dx = wx - px as f64;
            let ry = wy - py as f64;
            let dz = wz - pz as f64;
            rx[k] = dx;
            ory[k] = ry;
            rz[k] = dz;
            let lim = if oy >= 0.0 && oy < ry { oy } else { ry };
            let fudge = (lim / fudge_y_scale + 1.0e-7).floor() * fudge_y_scale;
            let dy = ry - fudge;
            let dx1 = dx - 1.0;
            let dy1 = dy - 1.0;
            let dz1 = dz - 1.0;
            let x0 = layer.perm(px);
            let x1 = layer.perm(px + 1);
            let xy00 = layer.perm(x0 + py);
            let xy01 = layer.perm(x0 + py + 1);
            let xy10 = layer.perm(x1 + py);
            let xy11 = layer.perm(x1 + py + 1);
            d0[k] = Perlin::grad_dot(layer.perm(xy00 + pz), dx, dy, dz);
            d1[k] = Perlin::grad_dot(layer.perm(xy10 + pz), dx1, dy, dz);
            d2[k] = Perlin::grad_dot(layer.perm(xy01 + pz), dx, dy1, dz);
            d3[k] = Perlin::grad_dot(layer.perm(xy11 + pz), dx1, dy1, dz);
            d4[k] = Perlin::grad_dot(layer.perm(xy00 + pz + 1), dx, dy, dz1);
            d5[k] = Perlin::grad_dot(layer.perm(xy10 + pz + 1), dx1, dy, dz1);
            d6[k] = Perlin::grad_dot(layer.perm(xy01 + pz + 1), dx, dy1, dz1);
            d7[k] = Perlin::grad_dot(layer.perm(xy11 + pz + 1), dx1, dy1, dz1);
        }
        let v0 = _mm512_loadu_pd(d0.as_ptr());
        let v1 = _mm512_loadu_pd(d1.as_ptr());
        let v2 = _mm512_loadu_pd(d2.as_ptr());
        let v3 = _mm512_loadu_pd(d3.as_ptr());
        let v4 = _mm512_loadu_pd(d4.as_ptr());
        let v5 = _mm512_loadu_pd(d5.as_ptr());
        let v6 = _mm512_loadu_pd(d6.as_ptr());
        let v7 = _mm512_loadu_pd(d7.as_ptr());
        let smoothv = |t: __m512d| -> __m512d {
            let t2 = _mm512_mul_pd(t, t);
            let t3 = _mm512_mul_pd(t2, t);
            let a = _mm512_mul_pd(t, _mm512_set1_pd(6.0));
            let b = _mm512_sub_pd(a, _mm512_set1_pd(15.0));
            let c = _mm512_mul_pd(t, b);
            let e = _mm512_add_pd(c, _mm512_set1_pd(10.0));
            _mm512_mul_pd(t3, e)
        };
        let a = smoothv(_mm512_loadu_pd(rx.as_ptr()));
        let b = smoothv(_mm512_loadu_pd(ory.as_ptr()));
        let c = smoothv(_mm512_loadu_pd(rz.as_ptr()));
        let l0 = _mm512_add_pd(v0, _mm512_mul_pd(a, _mm512_sub_pd(v1, v0)));
        let l1 = _mm512_add_pd(v2, _mm512_mul_pd(a, _mm512_sub_pd(v3, v2)));
        let l2 = _mm512_add_pd(v4, _mm512_mul_pd(a, _mm512_sub_pd(v5, v4)));
        let l3 = _mm512_add_pd(v6, _mm512_mul_pd(a, _mm512_sub_pd(v7, v6)));
        let m0 = _mm512_add_pd(l0, _mm512_mul_pd(b, _mm512_sub_pd(l1, l0)));
        let m1 = _mm512_add_pd(l2, _mm512_mul_pd(b, _mm512_sub_pd(l3, l2)));
        let r = _mm512_add_pd(m0, _mm512_mul_pd(c, _mm512_sub_pd(m1, m0)));
        let out = std::mem::transmute::<__m512d, [f64; 8]>(r);
        (out, freqs, freq)
    }

    fn sample(layers: &[Perlin], x: f64, y: f64, z: f64, smear: f64) -> f64 {
        let mut value = 0.0;
        let mut frequency = 1.0;
        #[cfg(target_arch = "x86_64")]
        {
            static AVX512: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            if layers.len() >= 8
                && *AVX512.get_or_init(|| {
                    std::arch::is_x86_feature_detected!("avx512f")
                        && std::arch::is_x86_feature_detected!("avx512dq")
                })
            {
                let mut i = 0usize;
                while i + 8 <= layers.len() {
                    // SAFETY: AVX-512F+DQ detected at runtime above.
                    let (vals, freqs, next_freq) = unsafe {
                        Self::sample_8_avx512(&layers[i..i + 8], x, y, z, smear, frequency)
                    };
                    for k in 0..8 {
                        value += vals[k] / freqs[k];
                    }
                    frequency = next_freq;
                    i += 8;
                }
                for layer in &layers[i..] {
                    value += layer.get_smeared(
                        x * frequency,
                        y * frequency,
                        z * frequency,
                        smear * frequency,
                    ) / frequency;
                    frequency /= 2.0;
                }
                return value;
            }
        }
        for layer in layers {
            value += layer.get_smeared(
                x * frequency,
                y * frequency,
                z * frequency,
                smear * frequency,
            ) / frequency;
            frequency /= 2.0;
        }
        value
    }

    pub(crate) fn get(&self, x: i32, y: i32, z: i32) -> f64 {
        let x = f64::from(x) * self.xzm;
        let y = f64::from(y) * self.ym;
        let z = f64::from(z) * self.xzm;
        let smear = self.ym * self.smear;
        let main = Self::sample(
            &self.main,
            x / self.xzf,
            y / self.yf,
            z / self.xzf,
            smear / self.yf,
        );
        let alpha = (main / 10.0 + 1.0) / 2.0;
        let min = if alpha >= 1.0 {
            0.0
        } else {
            Self::sample(&self.min, x, y, z, smear) / 512.0
        };
        let max = if alpha <= 0.0 {
            0.0
        } else {
            Self::sample(&self.max, x, y, z, smear) / 512.0
        };
        if alpha < 0.0 {
            min / 128.0
        } else if alpha > 1.0 {
            max / 128.0
        } else {
            (min + alpha * (max - min)) / 128.0
        }
    }
}

#[cfg(all(test, megu_vanilla_data))]
mod tests {
    use super::*;

    #[test]
    fn draw_trace_separates_wrapped_and_raw_sources() {
        const CHILD: &str = "MEGU_DRAW_TRACE_CHILD";
        if std::env::var_os(CHILD).is_some() {
            DTRACE.store(0, std::sync::atomic::Ordering::Relaxed);
            XTRACE_FEAT.store((1_i64 << 32) | 7, std::sync::atomic::Ordering::Relaxed);
            for mut rng in [Xor::seed_worldgen(0), Xor::seed(0)] {
                rng.next_bound(16);
                rng.next_f32();
                rng.next_f64();
            }
            return;
        }
        // Isolate the global diagnostic counters from parallel tests, and inspect
        // actual emitted records rather than a format helper's intended output.
        let name = format!(
            "{}::draw_trace_separates_wrapped_and_raw_sources",
            module_path!().split_once("::").unwrap().1
        );
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &name, "--nocapture"])
            .env(CHILD, "1")
            .env("FEATURE_DRAW_LIMIT", "100")
            .output()
            .unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(output.status.success(), "{stderr}");
        for prefix in ["DRAW ", "RAW_DRAW "] {
            let lines: Vec<_> = stderr
                .lines()
                .filter(|line| line.starts_with(prefix))
                .collect();
            assert_eq!(lines.len(), 3, "missing {prefix} source records: {stderr}");
            assert!(lines[0].contains(" nextInt(16)="), "{stderr}");
            assert!(lines[1].contains(" nextFloat="), "{stderr}");
            assert!(lines[2].contains(" nextDouble="), "{stderr}");
        }
    }

    #[test]
    fn dimension_blended_noise_matches_native_bits() {
        let configs = [
            [0.25, 0.25, 80.0, 160.0, 4.0],
            [0.25, 0.375, 80.0, 60.0, 8.0],
            [0.25, 0.125, 80.0, 160.0, 8.0],
        ];
        let mut current = None;
        let mut noise = None;
        for row in include_str!("../tests/data/blended_noise_1211.tsv").lines() {
            let p: Vec<_> = row.split('\t').collect();
            let seed = p[0].parse::<i64>().unwrap() as u64;
            let c = p[1].parse::<usize>().unwrap();
            if current != Some((seed, c)) {
                let [xz, y, xf, yf, smear] = configs[c];
                noise = Some(if c == 2 {
                    let mut random = Xor::seed(seed).splitter().of_str("minecraft:terrain");
                    BlendedNoise::modern(&mut random, xz, y, xf, yf, smear)
                } else {
                    BlendedNoise::legacy(seed, xz, y, xf, yf, smear)
                });
                current = Some((seed, c));
            }
            let actual = noise.as_ref().unwrap().get(
                p[2].parse().unwrap(),
                p[3].parse().unwrap(),
                p[4].parse().unwrap(),
            );
            assert_eq!(
                actual.to_bits(),
                u64::from_str_radix(p[5], 16).unwrap(),
                "{row}"
            );
        }
    }
    #[test]
    fn xoroshiro_parity() {
        // pumpkin xoroshiro128.rs test vectors
        let mut x = Xor::seed(0);
        eprintln!("DBG seed0 lo={:#x} hi={:#x}", x.lo, x.hi);
        let mut ng = x.split();
        eprintln!("DBG ng lo={:#x} hi={:#x}", ng.lo, ng.hi);
        let h = md5(b"TEST STRING");
        eprintln!("DBG md5 = {:02x?}", h);
        let raw = ng.next_long();
        eprintln!("DBG ng.next_long = {:#x}", raw);
        let v = raw as i32;
        assert_eq!(v, 542195535);
        {
            let sp = ng.splitter();
            let mut r1 = sp.of_str("TEST STRING");
            eprintln!("DBG sp lo={:#x} hi={:#x}", sp.lo, sp.hi);
            eprintln!("DBG r1 lo={:#x} hi={:#x}", r1.lo, r1.hi);
            let rv = r1.next_long();
            eprintln!("DBG r1.next_long = {:#x} i32={}", rv, rv as i32);
            assert_eq!(rv as i32, -641435713);
        }
        assert_eq!(x.next_i32(), 653572596);
        assert_eq!(ng.next_i32(), 435917842);
    }

    /// `RandomSource`'s default `nextLong` is `(long)next(32) << 32 + next(32)`
    /// with **signed** halves, so the low word sign-extends. Reading both halves
    /// as unsigned is off by 2^32 whenever bit 31 of the low word is set, which is
    /// half the draws — every seed derived from the stream (population seed,
    /// large-feature seed, carver seeds) came out wrong half the time.
    ///
    /// Expected values are `new java.util.Random(0).nextLong()` and the second
    /// draw of the same stream.
    #[test]
    fn legacy_next_long_sign_extends_like_javas() {
        let mut r = Lcg::new(0);
        assert_eq!(r.next_i64(), -4962768465676381896, "seed 0, first nextLong");
        assert_eq!(r.next_i64(), 4437113781045784766, "seed 0, second nextLong");
        // 0xbb20b45fd4d95138: the low word 0xd4d95138 has bit 31 set, so the
        // unsigned concatenation (0xbb20b460d4d95138) is the wrong answer.
        assert_ne!(r.seed, 0);
    }

    /// `Mth.cos` is the sine table a quarter turn ahead, not libm.
    #[test]
    fn mth_cos_is_the_sine_table_shifted() {
        assert_eq!(mth_sin(0.0), 0.0);
        assert_eq!(mth_cos(0.0), 1.0);
        let v = 1.570_796_4_f32; // pi/2 as the carver spells it
        assert_eq!(mth_sin(v), 1.0);
        assert!((mth_cos(v)).abs() < 1e-6);
    }
}

// ---------- SHA-256 (for `BiomeManager`'s zoom seed) ----------

/// `BiomeManager.obfuscateSeed` is `Hashing.sha256().hashLong(seed).asLong()`:
/// Guava's `putLong` is little-endian and `HashCode.asLong` reads the first eight
/// digest bytes little-endian. Vanilla uses it as the biome-blend zoom seed, so
/// the value has to match byte for byte.
pub(crate) fn sha256(data: &[u8]) -> [u8; 32] {
    megu_core::crypto::sha256(data)
}

#[cfg(test)]
mod sha_tests {
    #[test]
    fn sha256_vectors() {
        // FIPS vectors plus the value vanilla's BiomeManager starts from for seed 0
        assert_eq!(
            super::sha256(b"abc")
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(super::biome_zoom_seed(0), 8794265229978523055);
        assert_eq!(super::biome_zoom_seed(1), -6467378160175308932);
    }
}

/// `Mth.hash(long, long)` — the mixer `BiomeManager.getFiddledDistance` runs the
/// quart coordinates and the zoom seed through.
pub(crate) fn mth_hash(seed: i64, value: i64) -> i64 {
    seed.wrapping_mul(
        seed.wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407),
    )
    .wrapping_add(value)
}

/// The per-axis jitter the fiddled distance is built from:
/// `((Math.floorMod(s >> 24, 1024) / 1024.0) - 0.5) * 0.9`.
fn fiddle_raw(s: i64) -> f64 {
    let m = (s >> 24) % 1024;
    let m = if m < 0 { m + 1024 } else { m };
    (m as f64 / 1024.0 - 0.5) * 0.9
}

/// `BiomeManager.getFiddledDistance` — a distance between the queried fractional
/// position and one quart corner, jittered by the seed.
pub(crate) fn fiddled_distance(
    zoom: i64,
    bx: i32,
    by: i32,
    bz: i32,
    fx: f64,
    fy: f64,
    fz: f64,
) -> f64 {
    let mut s = zoom;
    for v in [bx, by, bz, bx, by, bz] {
        s = mth_hash(s, v as i64);
    }
    let d = fiddle_raw(s);
    s = mth_hash(s, zoom);
    let e = fiddle_raw(s);
    s = mth_hash(s, zoom);
    let f = fiddle_raw(s);
    let sq = |v: f64| v * v;
    sq(fz + f) + sq(fy + e) + sq(fx + d)
}

/// `BiomeManager(seed)`'s zoom seed.
pub(crate) fn biome_zoom_seed(seed: u64) -> i64 {
    let d = sha256(&seed.to_le_bytes());
    i64::from_le_bytes([d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]])
}

#[cfg(test)]
mod deco_seed_tests {
    /// `setDecorationSeed` run on the Xoroshiro source, cross-checked against a
    /// Python implementation of the same bytecode (and against the live server's
    /// `FSEED` log): chunk (-1,0) -> 6716476193438028432, chunk (-2,1) ->
    /// -4660739044861673552, both for world seed 0.
    #[test]
    fn decoration_seed_values() {
        let f = |bx: i32, bz: i32| -> i64 {
            let mut r = super::Xor::seed_worldgen(0);
            let l = r.next_i64_legacy() | 1;
            let m = r.next_i64_legacy() | 1;
            (bx as i64)
                .wrapping_mul(l)
                .wrapping_add((bz as i64).wrapping_mul(m))
        };
        assert_eq!(f(-16, 0), 6716476193438028432);
        assert_eq!(f(-32, 16), -4660739044861673552);
    }
}

/// `nextInt(bound)` against the game's own random classes.
/// `tests/data/random_next_int_1211.tsv` is written by `tools/vanilla_data/gen_vanilla_data.sh`
/// from the running 1.21.1 server jar: for each (class, seed, bound) it holds a hash of the
/// first 256 `nextInt(bound)` results from one fresh generator, and that generator's next
/// `nextLong()`. The bounds include large non-powers of two, where the modern generator's
/// rejection path (`n < bound`, redraw while `n < (-bound) mod bound` in 32-bit unsigned
/// arithmetic) is taken often.
#[cfg(test)]
mod next_int_vs_vanilla_tests {
    use super::*;

    #[cfg_attr(not(megu_vanilla_data), allow(dead_code))]
    fn fnv_of(results: impl Iterator<Item = i32>) -> u64 {
        let mut h = megu_core::hash::Fnv1a64::new();
        for x in results {
            h.update_i32(x);
        }
        h.finish()
    }

    #[test]
    #[cfg(megu_vanilla_data)]
    fn next_int_streams_match_the_game_for_every_class_seed_and_bound() {
        let data = std::fs::read_to_string("tests/data/random_next_int_1211.tsv")
            .expect("vanilla nextInt vectors");
        let (mut n, mut wrong) = (0usize, Vec::new());
        let mut per_kind = std::collections::BTreeMap::new();
        for line in data.lines() {
            let f: Vec<&str> = line.split('\t').collect();
            assert_eq!(f.len(), 6, "bad line {line:?}");
            let kind = f[0];
            let seed: i64 = f[1].parse().unwrap();
            let bound: i32 = f[2].parse().unwrap();
            let draws: usize = f[3].parse().unwrap();
            let want_hash: u64 = f[4].parse().unwrap();
            let want_after: i64 = f[5].parse().unwrap();
            let (hash, after) = match kind {
                "xoroshiro" => {
                    let mut r = Xor::seed(seed as u64);
                    let h = fnv_of((0..draws).map(|_| r.next_bound(bound)));
                    (h, r.next_long() as i64)
                }
                "worldgen" => {
                    let mut r = Xor::seed_worldgen(seed as u64);
                    let h = fnv_of((0..draws).map(|_| r.next_bound(bound)));
                    (h, r.next_i64_legacy())
                }
                "checked" => {
                    let mut r = Lcg::new(seed as u64);
                    let h = fnv_of((0..draws).map(|_| r.next_bound(bound)));
                    (h, r.next_i64())
                }
                k => panic!("unknown kind {k}"),
            };
            n += 1;
            *per_kind.entry(kind.to_string()).or_insert(0usize) += 1;
            if hash != want_hash || after != want_after {
                wrong.push(format!("{kind} seed {seed} bound {bound}"));
            }
        }
        let mut bad_kind = std::collections::BTreeMap::new();
        for w in &wrong {
            *bad_kind
                .entry(w.split(' ').next().unwrap())
                .or_insert(0usize) += 1;
        }
        eprintln!("nextInt vectors checked: {n} {per_kind:?}, mismatches by kind {bad_kind:?}");
        assert!(n >= 900, "vector set too small: {n}");
        assert!(
            wrong.is_empty(),
            "{} of {n} streams differ from the game, first: {:?}",
            wrong.len(),
            &wrong[..wrong.len().min(8)]
        );
    }

    /// Layer A2-a: four-lane `Perlin::get_4` / `Normal::get_4` must equal four scalar
    /// calls bit for bit, including negative / zero / large coordinates (the `wrap`
    /// boundary at 33,554,432 is included) and both normalization encodings.
    #[test]
    fn a2a_perlin_and_normal_get_4_match_scalar_bit_for_bit() {
        fn clear_ncache() {
            NCACHE.with(|c| c.borrow_mut().fill(((usize::MAX, 0, 0, 0), 0.0)));
        }
        // Coordinates chosen to cross zero, the two wrap thresholds, integer cells and
        // fractional interiors. Four consecutive x values form one AVX2 group.
        const COORDS: [f64; 16] = [
            0.0,
            -0.0,
            0.25,
            -0.375,
            1.0,
            -1.0,
            7.9375,
            -8.0625,
            255.999,
            -256.001,
            1_000.0,
            -1_000.0,
            33_554_431.75,
            -33_554_431.75,
            1.0e8,
            -1.0e8,
        ];
        let group = |base: usize| -> ([f64; 4], [f64; 4], [f64; 4]) {
            (
                std::array::from_fn(|k| COORDS[(base + k) % COORDS.len()]),
                std::array::from_fn(|k| COORDS[(base + k + 5) % COORDS.len()]),
                std::array::from_fn(|k| COORDS[(base + k + 11) % COORDS.len()]),
            )
        };
        let mut perlins = Vec::new();
        let mut r = Xor::seed(0x5eed_1234_5678_9abc);
        for _ in 0..3 {
            perlins.push(Perlin::new(&mut r));
        }
        let mut perlin_cases = 0usize;
        for p in &perlins {
            for base in 0..COORDS.len() {
                let (gx, gy, gz) = group(base);
                let got = p.get_4(gx, gy, gz);
                for k in 0..4 {
                    let want = p.get(gx[k], gy[k], gz[k]);
                    assert_eq!(
                        want.to_bits(),
                        got[k].to_bits(),
                        "Perlin lane {k} of group {base}: x={} y={} z={}",
                        gx[k],
                        gy[k],
                        gz[k]
                    );
                    perlin_cases += 1;
                }
            }
        }

        let normals = [
            normal_new(&mut Xor::seed(11), -2, 0.9, &[1.0, 0.75, 0.5]),
            normal_new_amps(&mut Xor::seed(22), 0, &[1.0, 0.5, 0.25, 0.125]),
            normal_new_amps(&mut Xor::seed(33), -1, &[0.0, 1.0, 0.0, 0.5]),
            normal_legacy_nether(44),
        ];
        let mut normal_cases = 0usize;
        let mut cache_hit_cases = 0usize;
        for n in &normals {
            for base in 0..COORDS.len() {
                let (gx, gy, gz) = group(base);
                // Miss path: clear the cache so both sides actually compute.
                clear_ncache();
                let want: [f64; 4] = std::array::from_fn(|k| n.get(gx[k], gy[k], gz[k]));
                clear_ncache();
                let got = n.get_4(gx, gy, gz);
                for k in 0..4 {
                    assert_eq!(
                        want[k].to_bits(),
                        got[k].to_bits(),
                        "Normal lane {k} of group {base}: x={} y={} z={}",
                        gx[k],
                        gy[k],
                        gz[k]
                    );
                    normal_cases += 1;
                }
                // All-hit path: a second get_4 must return exactly the same bits.
                let hit = n.get_4(gx, gy, gz);
                for k in 0..4 {
                    assert_eq!(got[k].to_bits(), hit[k].to_bits(), "NCACHE hit lane {k}");
                    cache_hit_cases += 1;
                }
            }
        }
        assert!(perlin_cases >= 3 * 16 * 4, "Perlin cases did not run");
        assert!(normal_cases >= 4 * 16 * 4, "Normal cases did not run");
        assert!(cache_hit_cases > 0, "cache-hit path was never exercised");
    }

    /// Layer A2-c: eight-lane `Perlin::get_8` / `Normal::get_8` must equal eight scalar
    /// calls bit for bit, including NaN, infinities, the two zeros and the `wrap` boundary.
    #[test]
    fn a2c_perlin_and_normal_get_8_match_scalar_bit_for_bit() {
        fn clear_ncache() {
            NCACHE.with(|c| c.borrow_mut().fill(((usize::MAX, 0, 0, 0), 0.0)));
        }
        const COORDS: [f64; 24] = [
            0.0,
            -0.0,
            0.25,
            -0.375,
            1.0,
            -1.0,
            7.9375,
            -8.0625,
            255.999,
            -256.001,
            1_000.0,
            -1_000.0,
            33_554_431.75,
            -33_554_431.75,
            1.0e8,
            -1.0e8,
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::MAX,
            f64::MIN,
            f64::MIN_POSITIVE,
            -f64::MIN_POSITIVE,
            f64::from_bits(1),
        ];
        let group = |base: usize| -> ([f64; 8], [f64; 8], [f64; 8]) {
            (
                std::array::from_fn(|k| COORDS[(base + k) % COORDS.len()]),
                std::array::from_fn(|k| COORDS[(base + k + 7) % COORDS.len()]),
                std::array::from_fn(|k| COORDS[(base + k + 13) % COORDS.len()]),
            )
        };
        let mut r = Xor::seed(0xa2c8_1234_5678_9abc);
        let perlins: Vec<Perlin> = (0..3).map(|_| Perlin::new(&mut r)).collect();
        let mut perlin_cases = 0usize;
        for p in &perlins {
            for base in 0..COORDS.len() {
                let (gx, gy, gz) = group(base);
                let got = p.get_8(gx, gy, gz);
                for k in 0..8 {
                    let want = p.get(gx[k], gy[k], gz[k]);
                    assert_eq!(
                        want.to_bits(),
                        got[k].to_bits(),
                        "Perlin lane {k} of group {base}: x={} y={} z={}",
                        gx[k],
                        gy[k],
                        gz[k]
                    );
                    perlin_cases += 1;
                }
            }
        }

        let normals = [
            normal_new(&mut Xor::seed(111), -2, 0.9, &[1.0, 0.75, 0.5]),
            normal_new_amps(&mut Xor::seed(222), 0, &[1.0, 0.5, 0.25, 0.125]),
            normal_new_amps(&mut Xor::seed(333), -1, &[0.0, 1.0, 0.0, 0.5]),
            normal_legacy_nether(444),
        ];
        let mut normal_cases = 0usize;
        let mut cache_hit_cases = 0usize;
        for n in &normals {
            for base in 0..COORDS.len() {
                let (gx, gy, gz) = group(base);
                clear_ncache();
                let want: [f64; 8] = std::array::from_fn(|k| n.get(gx[k], gy[k], gz[k]));
                clear_ncache();
                let got = n.get_8(gx, gy, gz);
                for k in 0..8 {
                    assert_eq!(
                        want[k].to_bits(),
                        got[k].to_bits(),
                        "Normal lane {k} of group {base}: x={} y={} z={}",
                        gx[k],
                        gy[k],
                        gz[k]
                    );
                    normal_cases += 1;
                }
                // All-hit path: a second get_8 must return exactly the same bits.
                let hit = n.get_8(gx, gy, gz);
                for k in 0..8 {
                    assert_eq!(got[k].to_bits(), hit[k].to_bits(), "NCACHE hit lane {k}");
                    cache_hit_cases += 1;
                }
            }
        }
        assert!(perlin_cases >= 3 * 24 * 8, "Perlin cases did not run");
        assert!(normal_cases >= 4 * 24 * 8, "Normal cases did not run");
        assert!(cache_hit_cases > 0, "cache-hit path was never exercised");
    }

    #[test]
    fn normal_partial_cache_batches_match_scalar_bits() {
        fn check<const N: usize>(
            normal: &Normal,
            x: [f64; N],
            y: [f64; N],
            z: [f64; N],
            misses: usize,
            batch: impl FnOnce([f64; N], [f64; N], [f64; N]) -> [f64; N],
        ) {
            let clear = || NCACHE.with(|c| c.borrow_mut().fill(((usize::MAX, 0, 0, 0), 0.0)));
            let mut indices = std::collections::HashSet::new();
            for k in 0..N {
                let key = (
                    normal.cache_id,
                    x[k].to_bits(),
                    y[k].to_bits(),
                    z[k].to_bits(),
                );
                assert!(
                    indices.insert(nhash(key)),
                    "test coordinates collide in NCACHE"
                );
            }

            clear();
            let expected: [f64; N] = std::array::from_fn(|k| normal.get(x[k], y[k], z[k]));
            clear();
            for k in misses..N {
                assert_eq!(
                    normal.get(x[k], y[k], z[k]).to_bits(),
                    expected[k].to_bits()
                );
            }
            let actual = batch(x, y, z);
            for k in 0..N {
                assert_eq!(
                    actual[k].to_bits(),
                    expected[k].to_bits(),
                    "lane {k}, misses {misses}"
                );
            }
        }

        let normals = [
            normal_new(&mut Xor::seed(41), -2, 0.9, &[1.0, 0.75, 0.5]),
            normal_new_amps(&mut Xor::seed(42), -1, &[1.0, 0.0, 0.5, 0.25]),
            normal_legacy_nether(43),
        ];
        let y: [f64; 8] = std::array::from_fn(|k| k as f64 * -0.5 + 1.125);
        let z: [f64; 8] = std::array::from_fn(|k| k as f64 * 0.75 - 4.375);
        for normal in &normals {
            let mut x = [0.0; 8];
            let mut used_indices = std::collections::HashSet::new();
            for k in 0..8 {
                let mut trial = 0;
                loop {
                    x[k] = k as f64 * 0.25 - 0.75 + trial as f64 * 0.03125;
                    let key = (
                        normal.cache_id,
                        x[k].to_bits(),
                        y[k].to_bits(),
                        z[k].to_bits(),
                    );
                    if used_indices.insert(nhash(key)) {
                        break;
                    }
                    trial += 1;
                    assert!(trial < 1000, "could not find distinct cache index");
                }
            }
            check(
                normal,
                x[..4].try_into().unwrap(),
                y[..4].try_into().unwrap(),
                z[..4].try_into().unwrap(),
                1,
                |x, y, z| normal.get_4(x, y, z),
            );
            for misses in 1..=2 {
                check(normal, x, y, z, misses, |x, y, z| normal.get_8(x, y, z));
            }
        }
    }

    #[test]
    fn normal_cache_hash_distributes_integer_coordinates() {
        let indices: std::collections::HashSet<_> = (0..128)
            .map(|x| nhash((1, (x as f64).to_bits(), 64.0f64.to_bits(), 0.0f64.to_bits())))
            .collect();
        assert_eq!(indices.len(), 128, "integer coordinates collide in NCACHE");
    }
}
