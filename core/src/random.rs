// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Java-exact random sources, clean-room from the pinned vanilla contracts.
//!
//! Three distinct generators share this module because the game uses three
//! distinct draw contracts:
//!
//! * [`LegacyRandom`] — `net.minecraft.world.level.levelgen.LegacyRandomSource`:
//!   the 48-bit `java.util.Random` LCG (`multiplier 0x5DEECE66D`, increment 11)
//!   behind the "checked" vectors, population seeds and legacy noise.
//! * [`Xoroshiro128pp`] — `net.minecraft.util.Xoroshiro128PlusPlusRandomSource`:
//!   the modern `RandomGenerator` draws (Lemire `nextInt(bound)`, one-word
//!   `nextLong`) behind the "xoroshiro" vectors and all modern sampling.
//! * [`WorldgenRandom`] — `net.minecraft.world.level.levelgen.WorldgenRandom`:
//!   the *same* engine wrapped in the legacy `BitRandomSource` contract
//!   (`nextInt` = `next(32)`, `nextLong` = two `next(32)` calls) the worldgen
//!   pipeline draws through. These are the "worldgen" vectors.
//!
//! All three are pinned bit-for-bit against
//! `tests/data/random_next_int_1211.tsv`, generated from the pinned 1.21.1
//! server jar (`tools/vanilla_data/`), so "Java-exact" is a test result, not a
//! claim.
//!
//! [`Xoroshiro128pp::fork_positional`] reproduces the `PositionalRandomFactory`
//! machinery: [`XoroshiroPositionalFactory::at`] mixes `Mth.getSeed` into the
//! saved seed words and [`XoroshiroPositionalFactory::from_hash_of`] mixes an
//! MD5 of the name, exactly as the vanilla splitter does.

use crate::crypto::md5;
use crate::math::Mth;

// ---------- shared draw helpers ----------

/// `java.util.Random.nextGaussian` / `MarsagliaPolarGaussian`: Marsaglia's
/// polar method with the spare value carried between calls. `next_double`
/// supplies the stream's own `nextDouble` so each generator spends its own
/// draw budget; `spare` is that generator's saved second value.
fn marsaglia(next_double: &mut impl FnMut() -> f64, spare: &mut Option<f64>) -> f64 {
    if let Some(v) = spare.take() {
        return v;
    }
    loop {
        let v1 = 2.0 * next_double() - 1.0;
        let v2 = 2.0 * next_double() - 1.0;
        let s = v1 * v1 + v2 * v2;
        if s >= 1.0 || s == 0.0 {
            continue;
        }
        let m = (-2.0 * s.ln() / s).sqrt();
        *spare = Some(v2 * m);
        return v1 * m;
    }
}

/// `BitRandomSource.nextInt(bound)`: power-of-two fast path on `next(31)`,
/// otherwise the mod-reduction rejection loop — the *legacy* contract both
/// [`LegacyRandom`] and [`WorldgenRandom`] share.
fn legacy_bound(next: &mut impl FnMut(u32) -> i32, bound: i32) -> i32 {
    debug_assert!(bound > 0);
    if bound & (bound - 1) == 0 {
        return ((bound as i64).wrapping_mul(next(31) as i64) >> 31) as i32;
    }
    loop {
        let bits = next(31);
        let value = bits % bound;
        // `bits - value + bound - 1 >= 0`, evaluated in wrapping i32 exactly
        // like the bytecode's int arithmetic.
        if bits.wrapping_sub(value).wrapping_add(bound - 1) >= 0 {
            return value;
        }
    }
}

/// `BitRandomSource.nextLong`: two `next(32)` calls where **both halves
/// sign-extend** as `int`s — not the unsigned assembly a first read suggests.
fn legacy_next_long(next: &mut impl FnMut(u32) -> i32) -> i64 {
    let hi = next(32) as i64;
    let lo = next(32) as i64;
    (hi << 32).wrapping_add(lo)
}

/// `BitRandomSource.nextDouble`: `((next(26) << 27) + next(27)) * 2^-53`.
fn legacy_next_double(next: &mut impl FnMut(u32) -> i32) -> f64 {
    let hi = next(26) as i64;
    let lo = next(27) as i64;
    ((hi << 27).wrapping_add(lo)) as f64 * f64::from_bits(0x3CA0_0000_0000_0000)
}

// ---------- LegacyRandom: the 48-bit java.util.Random LCG ----------

const LCG_MULTIPLIER: u64 = 0x5DEECE66D;
const LCG_INCREMENT: u64 = 11;
const LCG_MASK: u64 = (1 << 48) - 1;

/// `LegacyRandomSource` / `java.util.Random`: the 48-bit linear congruential
/// generator vanilla still uses for population seeds, structure layout,
/// carvers and legacy noise.
///
/// Every typed draw goes through [`next`](Self::next), so
/// [`consume_count`](Self::consume_count) counts raw `next(bits)` calls the
/// way the game's `count` field does — useful when diffing a draw sequence
/// against the server.
#[derive(Clone, Debug)]
pub struct LegacyRandom {
    seed: u64,
    count: u64,
    spare_gaussian: Option<f64>,
}

impl LegacyRandom {
    /// `new LegacyRandomSource(seed)` / `new Random(seed)`: the seed is
    /// scrambled with the multiplier before it becomes state.
    pub fn new(seed: u64) -> Self {
        Self {
            seed: (seed ^ LCG_MULTIPLIER) & LCG_MASK,
            count: 0,
            spare_gaussian: None,
        }
    }

    /// `setSeed`: re-scrambles and clears the Gaussian spare and the counter.
    pub fn set_seed(&mut self, seed: u64) {
        *self = Self::new(seed);
    }

    /// Rebuild a generator from a saved 48-bit state (mask-applied). Use with
    /// [`raw_state`](Self::raw_state) for save/restore round-trips.
    pub fn from_raw_state(state48: u64) -> Self {
        Self {
            seed: state48 & LCG_MASK,
            count: 0,
            spare_gaussian: None,
        }
    }

    /// The current 48-bit state. Non-consuming, so it is safe mid-stream
    /// where a `next_long` sentinel would move the sequence.
    pub fn raw_state(&self) -> u64 {
        self.seed
    }

    /// `WorldgenRandom.setLargeFeatureSeed`: draw two `nextLong`s from the
    /// base seed, then `x * a ^ z * b ^ seed` — carvers and structure layout
    /// reseed this way per chunk.
    pub fn for_large_feature(seed: u64, x: i32, z: i32) -> Self {
        let mut source = Self::new(seed);
        let a = source.next_long();
        let b = source.next_long();
        Self::new(
            (i64::from(x).wrapping_mul(a) ^ i64::from(z).wrapping_mul(b) ^ seed as i64) as u64,
        )
    }

    /// How many raw `next(bits)` calls the stream has served. The typed draws
    /// spend this budget: `next_int` costs one call, `next_long` two, and a
    /// rejecting `next_int_bounded` as many as it needs.
    pub fn consume_count(&self) -> u64 {
        self.count
    }

    /// `next(int bits)`: one LCG step, then the top `bits` of the 48-bit
    /// state. Every other draw is built on this.
    #[inline]
    pub fn next(&mut self, bits: u32) -> i32 {
        self.count += 1;
        self.seed = self
            .seed
            .wrapping_mul(LCG_MULTIPLIER)
            .wrapping_add(LCG_INCREMENT)
            & LCG_MASK;
        (self.seed >> (48 - bits)) as i32
    }

    /// `nextInt()` — `next(32)`.
    #[inline]
    pub fn next_int(&mut self) -> i32 {
        self.next(32)
    }

    /// `nextInt(bound)` in `[0, bound)` with the legacy rejection loop.
    ///
    /// # Panics
    ///
    /// Vanilla throws `IllegalArgumentException` for `bound <= 0`; the
    /// equivalent here is a panic.
    pub fn next_int_bounded(&mut self, bound: i32) -> i32 {
        assert!(bound > 0, "bound must be positive");
        legacy_bound(&mut |bits| self.next(bits), bound)
    }

    /// `nextLong()`: two `next(32)` calls, both halves sign-extended — see
    /// [`legacy_next_long`] for why the low half is not unsigned.
    #[inline]
    pub fn next_long(&mut self) -> i64 {
        legacy_next_long(&mut |bits| self.next(bits))
    }

    /// `nextFloat()` — `next(24) * 2^-24`.
    #[inline]
    pub fn next_float(&mut self) -> f32 {
        self.next(24) as f32 * 5.960_464_5e-8
    }

    /// `nextDouble()` — `((next(26) << 27) + next(27)) * 2^-53`.
    #[inline]
    pub fn next_double(&mut self) -> f64 {
        legacy_next_double(&mut |bits| self.next(bits))
    }

    /// `nextBoolean()` — `next(1) != 0`.
    #[inline]
    pub fn next_bool(&mut self) -> bool {
        self.next(1) != 0
    }

    /// `nextGaussian()`: Marsaglia polar on this stream's `next_double`,
    /// carrying the spare value between calls as `java.util.Random` does.
    pub fn next_gaussian(&mut self) -> f64 {
        let mut spare = self.spare_gaussian.take();
        let v = marsaglia(&mut || self.next_double(), &mut spare);
        self.spare_gaussian = spare;
        v
    }
}

// ---------- Xoroshiro128++: the modern engine ----------

/// `RandomSupport.GOLDEN_RATIO_64` / `SILVER_RATIO_64`: the constants behind
/// both `upgradeSeedTo128bit` and the all-zero state fallback.
const GOLDEN_RATIO_64: u64 = 0x9E3779B97F4A7C15;
const SILVER_RATIO_64: u64 = 0x6A09E667F3BCC909;

/// SplitMix64's finalizer (`RandomSupport.Seed128bit.mixed` / stafford13):
/// turns the two seed words into a decorrelated engine state.
fn stafford13(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^ (z >> 31)
}

/// `Xoroshiro128PlusPlusRandomSource`: the xoroshiro128++ engine with the
/// modern `java.util.random.RandomGenerator` draw contract — Lemire's
/// multiply-shift `nextInt(bound)`, one-word `nextLong`, low-half `nextInt`.
#[derive(Clone, Debug)]
pub struct Xoroshiro128pp {
    lo: u64,
    hi: u64,
    spare_gaussian: Option<f64>,
}

impl Xoroshiro128pp {
    /// `new Xoroshiro128PlusPlusRandomSource(seed)`:
    /// `RandomSupport.upgradeSeedTo128bit` — `seed ^ SILVER`, `+ GOLDEN`,
    /// both words through `stafford13`.
    pub fn new(seed: u64) -> Self {
        let l = seed ^ SILVER_RATIO_64;
        let m = l.wrapping_add(GOLDEN_RATIO_64);
        Self::from_seed128(stafford13(l), stafford13(m))
    }

    /// `new Xoroshiro128PlusPlusRandomSource(seedLo, seedHi)`: raw 128-bit
    /// state with vanilla's golden-ratio fallback — an all-zero state is a
    /// fixed point of the engine and must never be installed.
    pub fn from_seed128(lo: u64, hi: u64) -> Self {
        let (lo, hi) = if (lo | hi) == 0 {
            (GOLDEN_RATIO_64, SILVER_RATIO_64)
        } else {
            (lo, hi)
        };
        Self {
            lo,
            hi,
            spare_gaussian: None,
        }
    }

    /// `setSeed`: re-upgrades the 64-bit seed and clears the Gaussian spare.
    pub fn set_seed(&mut self, seed: u64) {
        *self = Self::new(seed);
    }

    /// The `(lo, hi)` engine state, non-consuming.
    pub fn seed_words(&self) -> (u64, u64) {
        (self.lo, self.hi)
    }

    /// One raw engine step, the `Xoroshiro128PlusPlus.nextLong` word.
    #[inline]
    fn next_word(&mut self) -> u64 {
        let l = self.lo;
        let m = self.hi;
        let n = l.wrapping_add(m).rotate_left(17).wrapping_add(l);
        let m = m ^ l;
        self.lo = l.rotate_left(49) ^ m ^ (m << 21);
        self.hi = m.rotate_left(28);
        n
    }

    /// `next(int bits)`: the top `bits` of one engine word — the
    /// `BitRandomSource` surface [`WorldgenRandom`] delegates to.
    #[inline]
    pub fn next(&mut self, bits: u32) -> i32 {
        (self.next_word() >> (64 - bits)) as i32
    }

    /// `nextLong()`: one engine word, the modern contract — **not** the
    /// two-`next(32)` assembly [`LegacyRandom`] uses.
    #[inline]
    pub fn next_long(&mut self) -> i64 {
        self.next_word() as i64
    }

    /// `nextInt()`: the low 32 bits of one engine word
    /// (`RandomGenerator.nextInt` is `(int)nextLong()`).
    #[inline]
    pub fn next_int(&mut self) -> i32 {
        self.next_word() as i32
    }

    /// `nextInt(bound)` in `[0, bound)`, Lemire's multiply-then-shift
    /// reduction: a 32-bit draw times the bound gives a 64-bit product whose
    /// high word is the result; products whose low word falls under the
    /// unsigned threshold `(-bound) mod bound` are redrawn so every value
    /// stays equally likely.
    ///
    /// # Panics
    ///
    /// Vanilla throws `IllegalArgumentException` for `bound <= 0`; the
    /// equivalent here is a panic.
    pub fn next_int_bounded(&mut self, bound: i32) -> i32 {
        assert!(bound > 0, "bound must be positive");
        let span = bound as u64;
        let widen = |draw: i32| u64::from(draw as u32).wrapping_mul(span);
        let mut product = widen(self.next_int());
        if (product & 0xFFFF_FFFF) < span {
            // `Integer.remainderUnsigned(-bound, bound)`: computed on the
            // 32-bit unsigned values — widening `-bound` to 64 bits first
            // (sign extension) gives a different remainder for every bound
            // that is not a power of two.
            let threshold = u64::from((bound.wrapping_neg() as u32) % (bound as u32));
            while (product & 0xFFFF_FFFF) < threshold {
                product = widen(self.next_int());
            }
        }
        (product >> 32) as i32
    }

    /// `nextFloat()` — `next(24) * 2^-24`.
    #[inline]
    pub fn next_float(&mut self) -> f32 {
        self.next(24) as f32 * 5.960_464_5e-8
    }

    /// `nextDouble()` — `(nextLong >>> 11) * 2^-53`, one engine word.
    #[inline]
    pub fn next_double(&mut self) -> f64 {
        (self.next_word() >> 11) as f64 * f64::from_bits(0x3CA0_0000_0000_0000)
    }

    /// `nextBoolean()` — the low bit of one engine word
    /// (`RandomGenerator.nextBoolean` is `(nextLong() & 1) != 0`).
    #[inline]
    pub fn next_bool(&mut self) -> bool {
        self.next_word() & 1 != 0
    }

    /// `nextGaussian()`: `MarsagliaPolarGaussian` on this stream's
    /// `next_double` — one word per `nextDouble`, not two `next(bits)` calls.
    pub fn next_gaussian(&mut self) -> f64 {
        let mut spare = self.spare_gaussian.take();
        let v = marsaglia(&mut || self.next_double(), &mut spare);
        self.spare_gaussian = spare;
        v
    }

    /// `fork()`: a child whose state is the next two engine words.
    pub fn fork(&mut self) -> Self {
        Self::from_seed128(self.next_word(), self.next_word())
    }

    /// `forkPositional()`: the `PositionalRandomFactory` carrying the next
    /// two engine words — the same draw budget the game spends.
    pub fn fork_positional(&mut self) -> XoroshiroPositionalFactory {
        XoroshiroPositionalFactory {
            seed_lo: self.next_word(),
            seed_hi: self.next_word(),
        }
    }
}

/// `Xoroshiro128PlusPlusRandomSource.XoroshiroPositionalRandomFactory`: the
/// splitter [`fork_positional`](Xoroshiro128pp::fork_positional) returns. It
/// owns two seed words and derives a fresh generator per position or name
/// without advancing the parent stream any further.
#[derive(Clone, Copy, Debug)]
pub struct XoroshiroPositionalFactory {
    seed_lo: u64,
    seed_hi: u64,
}

impl XoroshiroPositionalFactory {
    /// `at(int x, int y, int z)`: `Mth.getSeed` of the position XORed into
    /// the low seed word — the per-block-position fork.
    pub fn at(&self, x: i32, y: i32, z: i32) -> Xoroshiro128pp {
        Xoroshiro128pp::from_seed128(
            (Mth::get_seed(x, y, z) as u64) ^ self.seed_lo,
            self.seed_hi,
        )
    }

    /// `fromHashOf(String)`: MD5 of the name, split big-endian into two
    /// words, XORed into the seed pair — the per-feature-name fork.
    pub fn from_hash_of(&self, name: &str) -> Xoroshiro128pp {
        let h = md5(name.as_bytes());
        let lo = u64::from_be_bytes(h[0..8].try_into().unwrap());
        let hi = u64::from_be_bytes(h[8..16].try_into().unwrap());
        Xoroshiro128pp::from_seed128(lo ^ self.seed_lo, hi ^ self.seed_hi)
    }

    /// The seed pair this factory was forked with, for save/parity strings.
    pub fn seed_words(&self) -> (u64, u64) {
        (self.seed_lo, self.seed_hi)
    }
}

// ---------- WorldgenRandom: the BitRandomSource draw contract ----------

/// `net.minecraft.world.level.levelgen.WorldgenRandom`: the *worldgen* view
/// of a random source. The engine is the same xoroshiro128++, but every typed
/// draw follows the legacy `BitRandomSource` contract — `nextInt` is
/// `next(32)`, `nextInt(bound)` the power-of-two/rejection pair, `nextLong`
/// two sign-extended `next(32)` calls. A worldgen feature and a modern caller
/// on the same seed therefore produce different streams; keeping them as
/// distinct types is what makes that visible at the call site.
#[derive(Clone, Debug)]
pub struct WorldgenRandom {
    source: Xoroshiro128pp,
    count: u64,
    spare_gaussian: Option<f64>,
}

impl WorldgenRandom {
    /// `new WorldgenRandom(randomSource)` — wraps an existing engine.
    pub fn new(source: Xoroshiro128pp) -> Self {
        Self {
            source,
            count: 0,
            spare_gaussian: None,
        }
    }

    /// Convenience for `WorldgenRandom::new(Xoroshiro128pp::new(seed))` — the
    /// shape the pinned vectors exercise.
    pub fn seeded(seed: u64) -> Self {
        Self::new(Xoroshiro128pp::new(seed))
    }

    /// How many raw `next(bits)` calls the stream has served.
    pub fn consume_count(&self) -> u64 {
        self.count
    }

    /// `next(int bits)`: delegates to the wrapped source — the top `bits` of
    /// one engine word — and counts the call.
    #[inline]
    pub fn next(&mut self, bits: u32) -> i32 {
        self.count += 1;
        self.source.next(bits)
    }

    /// `nextInt()` — `next(32)`, the *top* word (unlike
    /// [`Xoroshiro128pp::next_int`]'s low half).
    #[inline]
    pub fn next_int(&mut self) -> i32 {
        self.next(32)
    }

    /// `nextInt(bound)` with the legacy rejection loop on `next(31)`.
    ///
    /// # Panics
    ///
    /// Vanilla throws `IllegalArgumentException` for `bound <= 0`; the
    /// equivalent here is a panic.
    pub fn next_int_bounded(&mut self, bound: i32) -> i32 {
        assert!(bound > 0, "bound must be positive");
        legacy_bound(&mut |bits| self.next(bits), bound)
    }

    /// `nextLong()`: two `next(32)` calls, both halves sign-extended.
    #[inline]
    pub fn next_long(&mut self) -> i64 {
        legacy_next_long(&mut |bits| self.next(bits))
    }

    /// `nextFloat()` — `next(24) * 2^-24`.
    #[inline]
    pub fn next_float(&mut self) -> f32 {
        self.next(24) as f32 * 5.960_464_5e-8
    }

    /// `nextDouble()` — `((next(26) << 27) + next(27)) * 2^-53`.
    #[inline]
    pub fn next_double(&mut self) -> f64 {
        legacy_next_double(&mut |bits| self.next(bits))
    }

    /// `nextBoolean()` — `next(1) != 0`.
    #[inline]
    pub fn next_bool(&mut self) -> bool {
        self.next(1) != 0
    }

    /// `nextGaussian()`: Marsaglia polar on this stream's `next_double`.
    pub fn next_gaussian(&mut self) -> f64 {
        let mut spare = self.spare_gaussian.take();
        let v = marsaglia(&mut || self.next_double(), &mut spare);
        self.spare_gaussian = spare;
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// The same FNV-1a fold `NextIntOracle.java` writes into the tsv: four
    /// little-endian bytes per `nextInt` result.
    fn fnv_of(results: impl Iterator<Item = i32>) -> u64 {
        let mut h = crate::hash::Fnv1a64::new();
        for x in results {
            h.update_i32(x);
        }
        h.finish()
    }

    /// `tests/data/random_next_int_1211.tsv`, resolved against the repo root
    /// (the crate lives at `core/`, so the fixtures are one directory up).
    fn vector_path() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("tests/data/random_next_int_1211.tsv")
    }

    /// Every row of the pinned-jar vectors: for each (kind, seed, bound) the
    /// file holds an FNV hash of 256 `nextInt(bound)` results and the
    /// generator's following `nextLong()`. "xoroshiro" rows run the modern
    /// [`Xoroshiro128pp`], "worldgen" rows the [`WorldgenRandom`] view of the
    /// same engine, "checked" rows the [`LegacyRandom`] LCG.
    ///
    /// The file is a generated fixture: in a strict-public export it is
    /// absent, which is a skip — anything else (unreadable, malformed,
    /// mismatched) is a failure, so a silently dropped row cannot pass.
    #[test]
    fn next_int_streams_match_the_pinned_java_oracle() {
        let path = vector_path();
        let data = match std::fs::read_to_string(&path) {
            Ok(data) => data,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                eprintln!(
                    "{}: optional generated fixture absent (strict-public export); \
                     vector check skipped",
                    path.display()
                );
                return;
            }
            Err(e) => panic!("cannot read {}: {e}", path.display()),
        };
        let (mut n, mut wrong) = (0usize, Vec::new());
        for line in data.lines() {
            if line.is_empty() {
                continue;
            }
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
                    let mut r = Xoroshiro128pp::new(seed as u64);
                    let h = fnv_of((0..draws).map(|_| r.next_int_bounded(bound)));
                    (h, r.next_long())
                }
                "worldgen" => {
                    let mut r = WorldgenRandom::seeded(seed as u64);
                    let h = fnv_of((0..draws).map(|_| r.next_int_bounded(bound)));
                    (h, r.next_long())
                }
                "checked" => {
                    let mut r = LegacyRandom::new(seed as u64);
                    let h = fnv_of((0..draws).map(|_| r.next_int_bounded(bound)));
                    (h, r.next_long())
                }
                k => panic!("unknown kind {k}"),
            };
            n += 1;
            if hash != want_hash || after != want_after {
                wrong.push(format!("{kind} seed {seed} bound {bound}"));
            }
        }
        assert!(n >= 900, "vector set too small: {n} rows");
        assert!(
            wrong.is_empty(),
            "{} of {n} streams differ from the game, first: {:?}",
            wrong.len(),
            &wrong[..wrong.len().min(8)]
        );
    }

    #[test]
    fn legacy_random_is_java_util_random() {
        // `new java.util.Random(42).nextInt()` — the widely published oracle.
        assert_eq!(LegacyRandom::new(42).next_int(), -1170105035);
        // A restored 48-bit state continues the same stream.
        let mut r = LegacyRandom::new(7);
        let _ = r.next_int();
        let state = r.raw_state();
        let a = r.next_long();
        let mut restored = LegacyRandom::from_raw_state(state);
        assert_eq!(restored.next_long(), a);
    }

    #[test]
    fn consume_count_tracks_raw_next_calls() {
        let mut r = LegacyRandom::new(1);
        assert_eq!(r.consume_count(), 0);
        let _ = r.next_int();
        assert_eq!(r.consume_count(), 1);
        let _ = r.next_long();
        assert_eq!(r.consume_count(), 3);
        let _ = r.next_double();
        assert_eq!(r.consume_count(), 5);
        // set_seed resets the counter like the game's setSeed does.
        r.set_seed(9);
        assert_eq!(r.consume_count(), 0);
    }

    #[test]
    fn bounded_draws_stay_in_range_and_cover_it() {
        let mut r = Xoroshiro128pp::new(12345);
        let mut hits = [0u32; 16];
        for _ in 0..4000 {
            let v = r.next_int_bounded(16);
            assert!((0..16).contains(&v));
            hits[v as usize] += 1;
        }
        assert!(hits.iter().all(|&n| n > 0), "incomplete coverage: {hits:?}");
        // Bound 1 can only draw 0 but still spends a word.
        assert_eq!(r.next_int_bounded(1), 0);
        // Same rule for the legacy view.
        let mut l = LegacyRandom::new(12345);
        for _ in 0..256 {
            assert!((0..7).contains(&l.next_int_bounded(7)));
        }
    }

    #[test]
    fn a_zero_xoroshiro_state_falls_back_to_golden_ratio() {
        let r = Xoroshiro128pp::from_seed128(0, 0);
        assert_eq!(r.seed_words(), (GOLDEN_RATIO_64, SILVER_RATIO_64));
    }

    #[test]
    fn the_gaussian_spare_consumes_no_engine_words() {
        // First call draws two doubles and banks a spare; the second call
        // must serve the spare without touching the engine, so the stream
        // after two gaussians matches the stream after one.
        let mut a = Xoroshiro128pp::new(99);
        let g1 = a.next_gaussian();
        let after_one = a.next_long();
        let mut b = Xoroshiro128pp::new(99);
        assert_eq!(b.next_gaussian(), g1);
        let g2 = b.next_gaussian();
        assert_ne!(g2, g1, "the spare is a different value, not a replay");
        assert_eq!(b.next_long(), after_one);
        assert!(g1.is_finite() && g2.is_finite());

        let mut l = LegacyRandom::new(99);
        let _ = l.next_gaussian();
        let after = l.next_long();
        let mut l2 = LegacyRandom::new(99);
        let _ = l2.next_gaussian();
        let _ = l2.next_gaussian();
        assert_eq!(l2.next_long(), after);
    }

    #[test]
    fn fork_positional_is_deterministic_and_name_sensitive() {
        let mut parent = Xoroshiro128pp::new(2024);
        let factory = parent.fork_positional();
        let mut parent2 = Xoroshiro128pp::new(2024);
        assert_eq!(factory.seed_words(), parent2.fork_positional().seed_words());

        // `at` is Mth.getSeed-keyed: same position, same stream.
        let mut p1 = factory.at(4, 60, -7);
        let mut p2 = factory.at(4, 60, -7);
        assert_eq!(p1.next_long(), p2.next_long());
        let mut other = factory.at(4, 60, -8);
        assert_ne!(p1.next_long(), other.next_long());

        // `from_hash_of` mixes an MD5 of the name: same name same stream,
        // different name different stream.
        let mut n1 = factory.from_hash_of("minecraft:ore_iron");
        let mut n2 = factory.from_hash_of("minecraft:ore_iron");
        assert_eq!(n1.next_long(), n2.next_long());
        let mut n3 = factory.from_hash_of("minecraft:ore_gold");
        assert_ne!(n1.next_long(), n3.next_long());

        // The derived stream is a valid Xoroshiro128++: bounded draws work.
        for _ in 0..64 {
            assert!((0..bound_check()).contains(&n1.next_int_bounded(bound_check())));
        }
        fn bound_check() -> i32 {
            100
        }
    }

    #[test]
    fn md5_matches_the_rfc_vectors() {
        // RFC 1321 suite — the splitter's name hash must be a real MD5.
        let digest = |s: &str| {
            md5(s.as_bytes())
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        };
        assert_eq!(digest(""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(digest("abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            digest("The quick brown fox jumps over the lazy dog"),
            "9e107d9d372bb6826bd81d3542a419d6"
        );
    }
}
