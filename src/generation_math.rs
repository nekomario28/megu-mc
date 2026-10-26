//! Seeded generation mathematics; no terrain, registry or world-state owner.

// ---------- MD5 (name -> 128-bit seed) ----------

pub(crate) fn md5(data: &[u8]) -> [u8; 16] {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    const K: [u32; 64] = [
        0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613,
        0xfd469501, 0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193,
        0xa679438e, 0x49b40821, 0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d,
        0x02441453, 0xd8a1e681, 0xe7d3fbc8, 0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed,
        0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a, 0xfffa3942, 0x8771f681, 0x6d9d6122,
        0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70, 0x289b7ec6, 0xeaa127fa,
        0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665, 0xf4292244,
        0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
        0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb,
        0xeb86d391,
    ];
    let mut msg = data.to_vec();
    let bitlen = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bitlen.to_le_bytes());
    let (mut a0, mut b0, mut c0, mut d0) =
        (0x67452301u32, 0xefcdab89u32, 0x98badcfeu32, 0x10325476u32);
    for chunk in msg.as_chunks::<64>().0 {
        let mut m = [0u32; 16];
        for i in 0..16 {
            m[i] = u32::from_le_bytes(chunk[i * 4..i * 4 + 4].try_into().unwrap());
        }
        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for i in 0..64 {
            let (f, g) = match i {
                0..=15 => ((b & c) | (!b & d), i),
                16..=31 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                32..=47 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let tmp = d;
            d = c;
            c = b;
            b = b.wrapping_add(
                a.wrapping_add(f)
                    .wrapping_add(K[i])
                    .wrapping_add(m[g])
                    .rotate_left(S[i]),
            );
            a = tmp;
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }
    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&a0.to_le_bytes());
    out[4..8].copy_from_slice(&b0.to_le_bytes());
    out[8..12].copy_from_slice(&c0.to_le_bytes());
    out[12..16].copy_from_slice(&d0.to_le_bytes());
    out
}

// ---------- Xoroshiro128++ with Java RandomSource semantics ----------

/// Stafford's "Mix13" 64-bit finaliser (a published xor-shift/multiply avalanche
/// permutation): two rounds of `v ^= v >> shift; v *= multiplier`, then a last
/// xor-shift.
fn stafford13(z: u64) -> u64 {
    const ROUNDS: [(u32, u64); 2] = [(30, 0xBF58476D1CE4E5B9), (27, 0x94D049BB133111EB)];
    let mixed = ROUNDS
        .iter()
        .fold(z, |v, &(shift, mul)| (v ^ (v >> shift)).wrapping_mul(mul));
    mixed ^ (mixed >> 31)
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
/// (the raw `next(bits)` traces cannot be interleaved with each other).
pub(crate) fn dtrace(kind: &str, value: f64) {
    let n = DTRACE.load(std::sync::atomic::Ordering::Relaxed);
    if n < 0 {
        return;
    }
    let f = XTRACE_FEAT.load(std::sync::atomic::Ordering::Relaxed);
    eprintln!("DRAW {n} f={},{} {kind}={value}", f >> 32, f & 0xFFFF);
    DTRACE.store(
        if n + 1 >= 200 { -1 } else { n + 1 },
        std::sync::atomic::Ordering::Relaxed,
    );
}

pub(crate) static DTRACE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

/// Which placed feature the trace is currently inside, so a draw can be attributed
/// without guessing from the sequence shape.
pub(crate) static XTRACE_FEAT: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(-1);

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
    pub(crate) const fn raw(lo: u64, hi: u64) -> Self {
        // The all-zero state is the generator's one fixed point, so it is replaced
        // by the two seeding constants.
        let degenerate = lo == 0 && hi == 0;
        Xor {
            lo: if degenerate { 0x9E3779B97F4A7C15 } else { lo },
            hi: if degenerate { 0x6A09E667F3BCC909 } else { hi },
            g: None,
            legacy: false,
        }
    }
    pub(crate) fn next_long(&mut self) -> u64 {
        // xoroshiro128++: output = rotl(s0 + s1, 17) + s0, then the linear step.
        let (s0, s1) = (self.lo, self.hi);
        let output = s0.wrapping_add(s1).rotate_left(17).wrapping_add(s0);
        let folded = s1 ^ s0;
        self.lo = s0.rotate_left(49) ^ folded ^ (folded << 21);
        self.hi = folded.rotate_left(28);
        output
    }
    pub(crate) fn next(&mut self, bits: u32) -> u64 {
        let v = self.next_long() >> (64 - bits);
        nxtrace(bits, v);
        v
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
        dtrace(&format!("nextInt({bound})"), v as f64);
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
            let threshold = (bound.wrapping_neg() as u64) % span;
            while (product & 0xFFFF_FFFF) < threshold {
                product = widen(self.next_i32());
            }
        }
        (product >> 32) as i32
    }
    pub(crate) fn next_f64(&mut self) -> f64 {
        let traced = self.next_f64_inner();
        dtrace("nextDouble", traced);
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
        dtrace("nextFloat", v as f64);
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
    // `PositionalRandomFactory.at(x, y, z)`: low seed word = `Mth.getSeed(x, y, z) ^ lo`
    pub(crate) fn pos(&self, x: i32, y: i32, z: i32) -> Xor {
        Xor::raw(mth_get_seed(x, y, z) as u64 ^ self.lo, self.hi)
    }
}

/// `Mth.getSeed(x, y, z)` (Yarn `MathHelper.hashCode(int, int, int)`).
pub(crate) fn mth_get_seed(x: i32, y: i32, z: i32) -> i64 {
    let mixed = (x.wrapping_mul(3129871) as i64) ^ (z as i64).wrapping_mul(116129781) ^ (y as i64);
    let scrambled = mixed
        .wrapping_mul(mixed)
        .wrapping_mul(42317861)
        .wrapping_add(mixed.wrapping_mul(11));
    scrambled >> 16
}

// `java.lang.String.hashCode()` (over the UTF-16 code units)
#[expect(dead_code)]
pub(crate) fn string_hash_code(s: &str) -> i32 {
    let mut hash = 0i32;
    for unit in s.encode_utf16() {
        hash = hash.wrapping_mul(31).wrapping_add(unit as i32);
    }
    hash
}

// ---------- legacy java.util.Random LCG (population seeds, legacy noise) ----------

/// Gameplay debugging aid: while this is non-zero, every `Lcg` draw is
/// printed as `RNG <method> <value>` and the counter is decremented. Used to
/// diff a carver's draw sequence against the vanilla probe's `DRAW` lines.
pub(crate) static RNG_TRACE: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

fn rtrace(name: &str, v: String) {
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
            seed: (seed ^ 0x5DEECE66D) & 0xFF_FFFF_FFFF_FF,
        }
    }
    fn nx(&mut self, bits: u32) -> u64 {
        self.seed = self.seed.wrapping_mul(0x5DEECE66D).wrapping_add(11) & 0xFF_FFFF_FFFF_FF;
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
        rtrace("nextInt", v.to_string());
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
        rtrace("nextLong", v.to_string());
        v
    }
    // java nextInt(bound): power-of-2 fast path + rejection otherwise
    pub(crate) fn next_bound(&mut self, bound: i32) -> i32 {
        let v = self.next_bound_raw(bound);
        rtrace("nextInt", format!("{v} (bound {bound})"));
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
        rtrace("nextFloat", format!("{v}"));
        v
    }
    pub(crate) fn next_f64(&mut self) -> f64 {
        let v = ((self.nx(26) << 27).wrapping_add(self.nx(27)) as f64)
            * f64::from_bits(0x3CA0000000000000);
        rtrace("nextDouble", format!("{v}"));
        v
    }
    pub(crate) fn next_bool(&mut self) -> bool {
        let v = self.nx(1) != 0;
        rtrace("nextBoolean", v.to_string());
        v
    }
    pub(crate) fn skip(&mut self, n: i32) {
        for _ in 0..n {
            self.next_i32();
        }
    }
    // legacy `LegacyRandomSource.Splitter.split(String)`: String.hashCode ^ seed
    pub(crate) fn split_str(seed: u64, name: &str) -> Self {
        Lcg::new((string_hash_code(name) as i64 as u64) ^ seed)
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

fn wrap(x: f64) -> f64 {
    const RO: f64 = 33_554_432.0;
    const HALF: f64 = f64::from_bits(0x416F_FFFF_FFFF_FFFF);
    if x > -HALF && x < HALF {
        x
    } else {
        x - (x / RO + 0.5).floor() * RO
    }
}
fn smooth(t: f64) -> f64 {
    t * t * t * (t * (t * 6.0 - 15.0) + 10.0)
}
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
    fn perm(&self, x: i32) -> i32 {
        self.p[(x & 0xFF) as usize] as i32
    }
    fn grad_dot(hash: i32, x: f64, y: f64, z: f64) -> f64 {
        let [gx, gy, gz] = GRAD[(hash & 15) as usize];
        gx * x + gy * y + gz * z
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
    (h >> 32) as usize & (NCACHE_N - 1)
}
pub(crate) struct Normal {
    layers: Vec<Octave>,
}
impl Normal {
    pub(crate) fn get(&self, x: f64, y: f64, z: f64) -> f64 {
        let key = (
            self as *const Normal as usize,
            x.to_bits(),
            y.to_bits(),
            z.to_bits(),
        );
        let idx = nhash(key);
        if let Some(v) = NCACHE.with(|c| {
            let slot = c.borrow()[idx];
            if slot.0 == key {
                Some(slot.1)
            } else {
                None
            }
        }) {
            return v;
        }
        let mut v = 0.0;
        for o in &self.layers {
            v += o.a * o.n.get(x * o.f, y * o.f, z * o.f);
        }
        NCACHE.with(|c| {
            c.borrow_mut()[idx] = (key, v);
        });
        v
    }
}

/// Octave list `(octave index, frequency, amplitude)` of a normal noise. Octave `i`
/// runs at `lacunarity^(first_octave + i)`; its weight starts from the geometric
/// series that makes the first and last octaves' weights sum to `amplitude_scale`
/// and is multiplied by the per-octave entry of `amplitude_mods` (missing = 1,
/// zero = octave left out).
fn octave_table(
    first_octave: i32,
    amplitude_scale: f64,
    table_len: usize,
    amplitude_mods: &[f64],
) -> Vec<(i32, f64, f64)> {
    let count = table_len as i32;
    let series_top = PERSISTENCE.powi(1 - count);
    let series_sum = PERSISTENCE.powi(-count) - 1.0;
    let mut frequency = LACUNARITY.powi(first_octave);
    let mut weight = amplitude_scale * (series_top / series_sum);
    let mut table = Vec::new();
    for i in 0..table_len {
        let modifier = amplitude_mods.get(i).copied().unwrap_or(1.0);
        if modifier != 0.0 {
            table.push((first_octave + i as i32, frequency, weight * modifier));
        }
        frequency *= LACUNARITY;
        weight *= PERSISTENCE;
    }
    table
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

// modern (non-legacy) NormalNoise from the 1.21.1 definition (`firstOctave`, `amplitudes`):
// value = (first + second) * valueFactor, valueFactor = (1/6) / (0.1 * (1 + 1/(span + 1))),
// span = last non-zero amplitude index - first non-zero amplitude index.
pub(crate) fn normal_from_amplitudes(r: &mut Xor, first_octave: i32, amps: &[f64]) -> Normal {
    let octaves = octave_table(first_octave, 1.0, amps.len(), amps);
    let idx: Vec<usize> = (0..amps.len()).filter(|&i| amps[i] != 0.0).collect();
    let nf = match (idx.first(), idx.last()) {
        (Some(&lo), Some(&hi)) => {
            let span = (hi - lo) as f64;
            (1.0 / 6.0) / (0.1 * (1.0 + 1.0 / (span + 1.0)))
        }
        _ => 0.0,
    };
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
    Normal { layers }
}

// legacy (pre-1.18) NormalNoise octaves: the zero octave is seeded first, then the octaves
// below it in descending order; absent or zero-amplitude octaves still advance the random
// stream (by 262 draws, one Perlin table's worth).
fn legacy_octave_perlins(
    rng: &mut Lcg,
    first_octave: i32,
    amps: &[f64],
) -> Vec<(Perlin, f64, f64)> {
    let octave_total = amps.len() as i32;
    let zero_slot = -first_octave;
    let mut slots: Vec<Option<Perlin>> = (0..amps.len()).map(|_| None).collect();
    let zero_octave = Perlin::new_l(rng);
    if zero_slot >= 0 && zero_slot < octave_total && amps[zero_slot as usize] != 0.0 {
        slots[zero_slot as usize] = Some(zero_octave);
    }
    for slot in (0..zero_slot).rev() {
        if slot < octave_total && amps[slot as usize] != 0.0 {
            slots[slot as usize] = Some(Perlin::new_l(rng));
        } else {
            rng.skip(262);
        }
    }
    let mut frequency = LACUNARITY.powi(-zero_slot);
    let mut weight = LACUNARITY.powi(octave_total - 1) / (LACUNARITY.powi(octave_total) - 1.0);
    let mut octaves = Vec::with_capacity(amps.len());
    for (slot, perlin) in slots.into_iter().enumerate() {
        if let Some(perlin) = perlin {
            octaves.push((perlin, frequency, weight * amps[slot]));
        }
        frequency *= LACUNARITY;
        weight /= LACUNARITY;
    }
    octaves
}
fn parity_norm_factor(amplitude_scale: f64, mods: &[f64]) -> f64 {
    let mut lowest = i32::MAX;
    let mut highest = i32::MIN;
    for (i, m) in mods.iter().enumerate() {
        if *m != 0.0 {
            lowest = lowest.min(i as i32);
            highest = highest.max(i as i32);
        }
    }
    if lowest > highest {
        return 0.0;
    }
    let span = highest - lowest;
    amplitude_scale * 0.5 * TARGET_DEV / (0.1 * (1.0 + 1.0 / (span + 1) as f64))
}
// legacy NormalNoise (NoiseProvider/vegetation uses LegacyRandomSource)
pub(crate) fn normal_new_l(seed: u64, first_octave: i32, amps: &[f64]) -> Normal {
    let mut r = Lcg::new(seed);
    let octaves = octave_table(first_octave, 1.0, amps.len(), amps);
    let target: f64 = octaves.iter().map(|o| o.2.abs()).sum();
    let mut nf = norm_factor(target, &octaves);
    let mut target = target;
    if nf != 0.0 {
        let parity = parity_norm_factor(1.0, amps);
        target *= parity / nf;
        nf = parity;
    }
    let _ = target;
    let first = legacy_octave_perlins(&mut r, first_octave, amps);
    let second = legacy_octave_perlins(&mut r, first_octave, amps);
    let mut layers = Vec::with_capacity(first.len() + second.len());
    for (n, f, a) in first {
        layers.push(Octave { n, f, a: a * nf });
    }
    for (n, f, a) in second {
        layers.push(Octave {
            n,
            f: f * INPUT_FACTOR,
            a: a * nf,
        });
    }
    Normal { layers }
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
    /// Contribution of one 2D simplex corner: a radial falloff `(r0 - |v|^2)^4`
    /// (r0 = 0.5) scaled by the dot product of the offset with the corner's gradient.
    #[inline]
    fn corner2(index: usize, dx: f64, dy: f64) -> f64 {
        let falloff = 0.5 - dx * dx - dy * dy;
        if falloff < 0.0 {
            return 0.0;
        }
        let [gx, gy, _] = GRAD[index % 12];
        let squared = falloff * falloff;
        squared * squared * (gx * dx + gy * dy)
    }
    /// Contribution of one 3D simplex corner (falloff radius term r0 = 0.6).
    #[inline]
    fn corner3(index: usize, dx: f64, dy: f64, dz: f64) -> f64 {
        let falloff = 0.6 - dx * dx - dy * dy - dz * dz;
        if falloff < 0.0 {
            return 0.0;
        }
        let [gx, gy, gz] = GRAD[index % 12];
        let squared = falloff * falloff;
        squared * squared * (gx * dx + gy * dy + gz * dz)
    }
    /// Gradient index of a 3D lattice point: the three coordinates are chained
    /// through the permutation table.
    #[inline]
    fn lattice_hash3(&self, i: i32, j: i32, k: i32) -> usize {
        let through_k = self.map(k);
        let through_j = self.map(j.wrapping_add(through_k));
        (self.map(i.wrapping_add(through_j)) % 12) as usize
    }
    /// The lattice steps (0 or 1 per axis) that reach the second and third
    /// corners of the simplex: the walk from the cell origin to the opposite
    /// corner crosses the axes in order of decreasing fractional offset, and ties
    /// resolve toward the earlier axis in the comparisons below.
    #[inline]
    fn corner_steps(fx: f64, fy: f64, fz: f64) -> ([i32; 3], [i32; 3]) {
        if fx >= fy {
            if fy >= fz {
                ([1, 0, 0], [1, 1, 0])
            } else if fx >= fz {
                ([1, 0, 0], [1, 0, 1])
            } else {
                ([0, 0, 1], [1, 0, 1])
            }
        } else if fy < fz {
            ([0, 0, 1], [0, 1, 1])
        } else if fx < fz {
            ([0, 1, 0], [0, 1, 1])
        } else {
            ([0, 1, 0], [1, 1, 0])
        }
    }
    pub(crate) fn get3(&self, x: f64, y: f64, z: f64) -> f64 {
        // Skew the input into the lattice of unit cubes that tile into simplices
        // (factor 1/3), find the cell, and unskew its origin (factor 1/6).
        let skew = (x + y + z) * (1.0 / 3.0);
        let cell = [
            (x + skew).floor() as i32,
            (y + skew).floor() as i32,
            (z + skew).floor() as i32,
        ];
        let unskew = cell[0].wrapping_add(cell[1]).wrapping_add(cell[2]) as f64 * (1.0 / 6.0);
        let from_origin = [
            x - (cell[0] as f64 - unskew),
            y - (cell[1] as f64 - unskew),
            z - (cell[2] as f64 - unskew),
        ];
        // Lattice steps (0/1 per axis) reaching the second and third corners.
        let (second, third) = Self::corner_steps(from_origin[0], from_origin[1], from_origin[2]);
        // Offsets from the point to each of the four corners.
        let to_second = [0, 1, 2].map(|a| from_origin[a] - second[a] as f64 + 1.0 / 6.0);
        let to_third = [0, 1, 2].map(|a| from_origin[a] - third[a] as f64 + 1.0 / 3.0);
        let to_last = from_origin.map(|v| v - 0.5);
        let base = cell.map(|c| c & 0xFF);
        let hash_at = |step: [i32; 3]| {
            self.lattice_hash3(
                base[0].wrapping_add(step[0]),
                base[1].wrapping_add(step[1]),
                base[2].wrapping_add(step[2]),
            )
        };
        let first = Self::corner3(
            hash_at([0, 0, 0]),
            from_origin[0],
            from_origin[1],
            from_origin[2],
        );
        let second_c = Self::corner3(hash_at(second), to_second[0], to_second[1], to_second[2]);
        let third_c = Self::corner3(hash_at(third), to_third[0], to_third[1], to_third[2]);
        let last = Self::corner3(hash_at([1, 1, 1]), to_last[0], to_last[1], to_last[2]);
        32.0 * (first + second_c + third_c + last)
    }
    pub(crate) fn get2(&self, x: f64, y: f64) -> f64 {
        // Skew factor (sqrt(3) - 1) / 2 and unskew factor (3 - sqrt(3)) / 6 map the
        // plane onto a lattice of rhombi, each split into two triangles.
        const SKEW: f64 = 0.5 * (1.7320508075688772 - 1.0);
        const UNSKEW: f64 = (3.0 - 1.7320508075688772) / 6.0;
        let skew = (x + y) * SKEW;
        let (cx, cy) = ((x + skew).floor() as i32, (y + skew).floor() as i32);
        let unskew = cx.wrapping_add(cy) as f64 * UNSKEW;
        let (fx, fy) = (x - (cx as f64 - unskew), y - (cy as f64 - unskew));
        // The middle corner is reached along whichever axis has the larger offset.
        let (step_x, step_y) = if fx > fy { (1i32, 0i32) } else { (0i32, 1i32) };
        let middle = (fx - step_x as f64 + UNSKEW, fy - step_y as f64 + UNSKEW);
        let far = (2.0 * UNSKEW + (fx - 1.0), 2.0 * UNSKEW + (fy - 1.0));
        let (bx, by) = (cx & 0xFF, cy & 0xFF);
        let at = |sx: i32, sy: i32| {
            self.map(
                bx.wrapping_add(sx)
                    .wrapping_add(self.map(by.wrapping_add(sy))),
            ) as usize
        };
        let near_c = Self::corner2(at(0, 0), fx, fy);
        let middle_c = Self::corner2(at(step_x, step_y), middle.0, middle.1);
        let far_c = Self::corner2(at(1, 1), far.0, far.1);
        70.0 * (near_c + middle_c + far_c)
    }
}

// vanilla OctaveSimplexNoiseSampler over octave indices (can be negative)
pub(crate) struct OSimp {
    layers: Vec<Option<Simp>>,
    k: i32,
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
                    k: 0,
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
        let zi = -first;
        // octave 0 (index zi=-first) uses s0
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
            // negative-octave samplers come from a second legacy generator seeded by
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
        OSimp { layers, k }
    }
    pub(crate) fn sample(&self, x: f64, y: f64) -> f64 {
        let mut d = 0.0;
        let mut e = 2.0f64.powi(self.k - 1);
        let mut f = 1.0 / (2.0f64.powi(self.k) - 1.0);
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

// fbm of smeared perlin (old_blended_noise internals)
pub(crate) struct SmearStack {
    layers: Vec<(Perlin, f64, f64)>, // noise, freq, amp
    fudge_y: f64,
}
impl SmearStack {
    pub(crate) fn get(&self, x: f64, y: f64, z: f64) -> f64 {
        let mut v = 0.0;
        for (n, f, a) in &self.layers {
            v += a * n.get_smeared(x * f, y * f, z * f, self.fudge_y * f);
        }
        v
    }
}
pub(crate) fn smear_fbm(
    r: &mut Xor,
    first_octave: i32,
    smear_y: f64,
    value_factor: f64,
) -> SmearStack {
    let octaves = -first_octave + 1;
    let mut f = 1.0f64;
    let mut vf = value_factor / (2f64.powi(octaves) - 1.0);
    let mut layers = Vec::new();
    for _ in (0..octaves).rev() {
        layers.push((Perlin::new(r), f, vf));
        f /= 2.0;
        vf *= 2.0;
    }
    SmearStack {
        layers,
        fudge_y: smear_y,
    }
}

/// Reader for `tests/data/vanilla_vectors_1211.tsv`: questions put to the 1.21.1 game itself
/// and the answers it gave. A line is `kind <args...> | <answers...>`, tab separated.
#[cfg(test)]
pub(crate) mod vector_file {
    pub(crate) fn read(kind: &str) -> Vec<(Vec<String>, Vec<String>)> {
        let text = std::fs::read_to_string("tests/data/vanilla_vectors_1211.tsv").expect(
            "tests/data/vanilla_vectors_1211.tsv is missing: run tools/vanilla_data/gen_vanilla_data.sh (see README)",
        );
        let mut rows = Vec::new();
        for line in text.lines() {
            let f: Vec<&str> = line.split('\t').collect();
            if f[0] != kind {
                continue;
            }
            let bar = f
                .iter()
                .position(|&c| c == "|")
                .expect("vector line without `|`");
            let to_vec = |s: &[&str]| s.iter().map(|c| c.to_string()).collect::<Vec<_>>();
            rows.push((to_vec(&f[1..bar]), to_vec(&f[bar + 1..])));
        }
        assert!(!rows.is_empty(), "no `{kind}` vectors in the vector file");
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::vector_file;
    use super::*;
    /// One fixed script of draws on `Xoroshiro128PlusPlusRandom`, for five seeds, against the
    /// answers the 1.21.1 game gave (`tests/data/vanilla_vectors_1211.tsv`, written by
    /// tools/vanilla_data/gen_vanilla_data.sh); the script is `Vectors.xoro` in `Extract.java`.
    #[test]
    fn xoroshiro_parity() {
        crate::test_support::require_vanilla_data();
        let rows = vector_file::read("xoro");
        assert!(rows.len() >= 5);
        for (q, a) in rows {
            let seed = q[0].parse::<i64>().unwrap() as u64;
            let mut r = Xor::seed(seed);
            let got = [
                (r.next_long() as i64).to_string(),
                (r.next_long() as i64).to_string(),
                r.next_i32().to_string(),
                r.next_bound(100).to_string(),
                r.next_bound(1000003).to_string(),
                r.next_f64().to_string(),
                (r.next_bool() as i32).to_string(),
            ];
            assert_eq!(got[..], a[..7], "seed {seed}, direct draws");
            let mut child = r.split();
            assert_eq!(
                (child.next_long() as i64).to_string(),
                a[7],
                "seed {seed}, split"
            );
            let sp = r.splitter();
            let named = sp.of_str("minecraft:test_stream").next_long() as i64;
            let at = sp.pos(-37, 64, 1021).next_long() as i64;
            assert_eq!(named.to_string(), a[8], "seed {seed}, splitter by name");
            assert_eq!(at.to_string(), a[9], "seed {seed}, splitter by position");
        }
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
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut msg = data.to_vec();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());
    for chunk in msg.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (i, v) in [a, b, c, d, e, f, g, hh].iter().enumerate() {
            h[i] = h[i].wrapping_add(*v);
        }
    }
    let mut out = [0u8; 32];
    for (i, v) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
    }
    out
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
#[expect(clippy::too_many_arguments)]
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
