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
    let (mut a0, mut b0, mut c0, mut d0) = (0x67452301u32, 0xefcdab89u32, 0x98badcfeu32, 0x10325476u32);
    for chunk in msg.chunks_exact(64) {
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

fn stafford13(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^ (z >> 31)
}

pub(crate) struct Xor {
    lo: u64,
    hi: u64,
    g: Option<f64>,
}
impl Xor {
    pub(crate) fn seed(seed: u64) -> Self {
        let l = seed ^ 0x6A09E667F3BCC909;
        let m = l.wrapping_add(0x9E3779B97F4A7C15);
        Self::raw(stafford13(l), stafford13(m))
    }
    pub(crate) const fn raw(lo: u64, hi: u64) -> Self {
        let (lo, hi) = if (lo | hi) == 0 {
            (0x9E3779B97F4A7C15, 0x6A09E667F3BCC909)
        } else {
            (lo, hi)
        };
        Xor { lo, hi, g: None }
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
        self.next_long() >> (64 - bits)
    }
    pub(crate) fn next_i32(&mut self) -> i32 {
        self.next_long() as i32
    }
    pub(crate) fn next_bound(&mut self, bound: i32) -> i32 {
        // RandomGenerator.nextInt(bound): Lemire multiply-high + rejection
        let mut l = (self.next_i32() as u64) & 0xFFFF_FFFF;
        let mut m = l.wrapping_mul(bound as u64);
        let mut n = m & 0xFFFF_FFFF;
        if n < bound as u64 {
            let i = ((!bound).wrapping_add(1) as u64) % (bound as u64);
            while n < i {
                l = (self.next_i32() as u64) & 0xFFFF_FFFF;
                m = l.wrapping_mul(bound as u64);
                n = m & 0xFFFF_FFFF;
            }
        }
        (m >> 32) as i32
    }
    pub(crate) fn next_f64(&mut self) -> f64 {
        (self.next_long() >> 11) as f64 * f64::from_bits(0x3CA0000000000000) // 2^-53
    }
    pub(crate) fn next_f32(&mut self) -> f32 {
        self.next(24) as f32 * 5.960_464_5E-8
    }
    pub(crate) fn next_bool(&mut self) -> bool {
        (self.next_long() & 1) != 0
    }
    // nextInbetween inclusive: next_bounded(max-min+1)+min
    pub(crate) fn next_inbetween(&mut self, min: i32, max: i32) -> i32 {
        if min >= max {
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
#[expect(dead_code)]
pub(crate) fn java_hash(s: &str) -> i32 {
    let mut h = 0i32;
    for u in s.encode_utf16() {
        h = h.wrapping_mul(31).wrapping_add(u as i32);
    }
    h
}

// ---------- legacy java.util.Random LCG (population seeds, legacy noise) ----------

pub(crate) struct Lcg {
    seed: u64,
}
#[allow(dead_code)]
impl Lcg {
    pub(crate) fn new(seed: u64) -> Self {
        Lcg { seed: (seed ^ 0x5DEECE66D) & 0xFF_FFFF_FFFF_FF }
    }
    fn nx(&mut self, bits: u32) -> u64 {
        self.seed = self.seed.wrapping_mul(0x5DEECE66D).wrapping_add(11) & 0xFF_FFFF_FFFF_FF;
        self.seed >> (48 - bits)
    }
    pub(crate) fn next_i32(&mut self) -> i32 {
        self.nx(32) as u32 as i32
    }
    pub(crate) fn next_i64(&mut self) -> i64 {
        ((self.nx(32) as i64) << 32).wrapping_add(self.nx(32) as i64)
    }
    // java nextInt(bound): power-of-2 fast path + rejection otherwise
    pub(crate) fn next_bound(&mut self, bound: i32) -> i32 {
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
        self.nx(24) as f32 * 5.960_464_5E-8
    }
    pub(crate) fn next_f64(&mut self) -> f64 {
        ((self.nx(26) << 27).wrapping_add(self.nx(27)) as f64) * f64::from_bits(0x3CA0000000000000)
    }
    pub(crate) fn next_bool(&mut self) -> bool {
        self.nx(1) != 0
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
pub(crate) fn lerp3(a: f64, b: f64, c: f64, v0: f64, v1: f64, v2: f64, v3: f64, v4: f64, v5: f64, v6: f64, v7: f64) -> f64 {
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
        lerp3(smooth(rx), smooth(ory), smooth(rz), d000, d100, d010, d110, d001, d101, d011, d111)
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
        let key = (self as *const Normal as usize, x.to_bits(), y.to_bits(), z.to_bits());
        let idx = nhash(key);
        if let Some(v) = NCACHE.with(|c| { let slot = c.borrow()[idx]; if slot.0 == key { Some(slot.1) } else { None } }) {
            return v;
        }
        let mut v = 0.0;
        for o in &self.layers {
            v += o.a * o.n.get(x * o.f, y * o.f, z * o.f);
        }
        NCACHE.with(|c| { c.borrow_mut()[idx] = (key, v); });
        v
    }
}

fn build_octaves(base_octave: i32, base_amplitude: f64, count: usize, mods: &[f64]) -> Vec<(i32, f64, f64)> {
    let mut f = LACUNARITY.powi(base_octave);
    let mut a = base_amplitude * (PERSISTENCE.powi(-(count as i32 - 1)) / (PERSISTENCE.powi(-(count as i32)) - 1.0));
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
pub(crate) fn normal_new(r: &mut Xor, base_octave: i32, base_amplitude: f64, mods: &[f64]) -> Normal {
    let octaves = build_octaves(base_octave, base_amplitude, mods.len(), mods);
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
    Normal { layers }
}

// legacy (pre-1.18) NormalNoise: init order = zero-octave then descending
// below it; missing/zero amplitudes still consume the rng (skip 262).
fn legacy_fbm(r: &mut Lcg, first_octave: i32, amps: &[f64]) -> Vec<(Perlin, f64, f64)> {
    let octaves = amps.len() as i32;
    let zi = -first_octave;
    let mut levels: Vec<Option<Perlin>> = (0..amps.len()).map(|_| None).collect();
    let zero = Perlin::new_l(r);
    if zi >= 0 && zi < octaves && amps[zi as usize] != 0.0 {
        levels[zi as usize] = Some(zero);
    }
    for i in (0..zi).rev() {
        if i < octaves && amps[i as usize] != 0.0 {
            levels[i as usize] = Some(Perlin::new_l(r));
        } else {
            r.skip(262);
        }
    }
    let mut factor = LACUNARITY.powi(-zi);
    let mut value_factor =
        LACUNARITY.powi(octaves - 1) / (LACUNARITY.powi(octaves) - 1.0);
    let mut out = Vec::with_capacity(amps.len());
    for (i, n) in levels.into_iter().enumerate() {
        if let Some(n) = n {
            out.push((n, factor, value_factor * amps[i]));
        }
        factor *= LACUNARITY;
        value_factor /= LACUNARITY;
    }
    out
}
fn parity_norm_factor(base_amp: f64, mods: &[f64]) -> f64 {
    let mut min_o = i32::MAX;
    let mut max_o = i32::MIN;
    for (i, m) in mods.iter().enumerate() {
        if *m != 0.0 {
            min_o = min_o.min(i as i32);
            max_o = max_o.max(i as i32);
        }
    }
    if min_o > max_o {
        return 0.0;
    }
    let span = max_o - min_o;
    base_amp * 0.5 * TARGET_DEV / (0.1 * (1.0 + 1.0 / (span + 1) as f64))
}
// legacy NormalNoise (NoiseProvider/vegetation uses LegacyRandomSource)
pub(crate) fn normal_new_l(seed: u64, first_octave: i32, amps: &[f64]) -> Normal {
    let mut r = Lcg::new(seed);
    let octaves = build_octaves(first_octave, 1.0, amps.len(), amps);
    let target: f64 = octaves.iter().map(|o| o.2.abs()).sum();
    let mut nf = norm_factor(target, &octaves);
    let mut target = target;
    if nf != 0.0 {
        let parity = parity_norm_factor(1.0, amps);
        target *= parity / nf;
        nf = parity;
    }
    let _ = target;
    let first = legacy_fbm(&mut r, first_octave, amps);
    let second = legacy_fbm(&mut r, first_octave, amps);
    let mut layers = Vec::with_capacity(first.len() + second.len());
    for (n, f, a) in first {
        layers.push(Octave { n, f, a: a * nf });
    }
    for (n, f, a) in second {
        layers.push(Octave { n, f: f * INPUT_FACTOR, a: a * nf });
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
            ai.wrapping_add(q)
                .wrapping_add(self.map(aj.wrapping_add(r).wrapping_add(self.map(ak.wrapping_add(s))))),
        ) % 12;
        let an = self.map(
            ai.wrapping_add(t)
                .wrapping_add(self.map(aj.wrapping_add(u).wrapping_add(self.map(ak.wrapping_add(v))))),
        ) % 12;
        let ao = self.map(
            ai.wrapping_add(1)
                .wrapping_add(self.map(aj.wrapping_add(1).wrapping_add(self.map(ak.wrapping_add(1))))),
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
        let u = self
            .map(r.wrapping_add(l).wrapping_add(self.map(s.wrapping_add(m))));
        let v = self
            .map(r.wrapping_add(1).wrapping_add(self.map(s.wrapping_add(1))));
        let w = Self::grad(t as usize, h, k);
        let z = Self::grad(u as usize, n, o);
        let aa = Self::grad(v as usize, p, q);
        70.0 * (w + z + aa)
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
            _ => return OSimp { layers: Vec::new(), k: 0 },
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
pub(crate) fn smear_fbm(r: &mut Xor, first_octave: i32, smear_y: f64, value_factor: f64) -> SmearStack {
    let octaves = -first_octave + 1;
    let mut f = 1.0f64;
    let mut vf = value_factor / (2f64.powi(octaves) - 1.0);
    let mut layers = Vec::new();
    for _ in (0..octaves).rev() {
        layers.push((Perlin::new(r), f, vf));
        f /= 2.0;
        vf *= 2.0;
    }
    SmearStack { layers, fudge_y: smear_y }
}

#[cfg(test)]
mod tests {
    use super::*;
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

}
