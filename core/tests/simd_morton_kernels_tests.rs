// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Exhaustive and bit-exactness tests for `megu_core::morton` and
//! `megu_core::simd_voxel`.
//!
//! Every SIMD-tier equivalence test compares the *dispatched* entry
//! point (whatever `is_x86_feature_detected!` selects on this host —
//! AVX-512 on Zen 5 parts, AVX2 elsewhere, NEON on AArch64) against the
//! pub scalar oracle, across edge-case inputs.

use megu_core::morton::{
    morton2_decode_u32, morton2_encode_i16, morton3_decode_u10, morton3_decode_u4,
    morton3_encode_u10, morton3_encode_u4,
};
use megu_core::simd_voxel::{
    batch_aabb_frustum_cull_soa, batch_aabb_frustum_cull_soa_scalar,
    build_non_air_bitmask_64, build_non_air_bitmask_64_scalar,
    unpack_palette_4bit_to_u16, unpack_palette_4bit_to_u16_scalar,
};

// ---------------------------------------------------------------------------
// Deterministic PRNG (no external crates): splitmix64-style xorshift.
// ---------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

// ---------------------------------------------------------------------------
// Morton 3D u4 — exhaustive over the whole 16³ section
// ---------------------------------------------------------------------------

#[test]
fn morton3_u4_roundtrips_all_4096_section_cells() {
    let mut seen = [false; 4096];
    for x in 0..16_u8 {
        for y in 0..16_u8 {
            for z in 0..16_u8 {
                let code = morton3_encode_u4(x, y, z);
                assert!(usize::from(code) < 4096);
                assert!(!seen[usize::from(code)], "duplicate code {code}");
                seen[usize::from(code)] = true;
                assert_eq!(morton3_decode_u4(code), (x, y, z));
            }
        }
    }
    // The 12-bit code space is fully covered — a bijection.
    assert!(seen.iter().all(|&b| b));
}

#[test]
fn morton3_u4_axes_are_genuinely_interleaved() {
    // Single-axis codes must land on every-third-bit positions.
    assert_eq!(morton3_encode_u4(1, 0, 0), 0b001);
    assert_eq!(morton3_encode_u4(0, 1, 0), 0b010);
    assert_eq!(morton3_encode_u4(0, 0, 1), 0b100);
    assert_eq!(morton3_encode_u4(2, 0, 0), 0b001_000);
    assert_eq!(morton3_encode_u4(0, 0, 2), 0b100_000);
    // (1,1,1) is the smallest code with all three axes occupied.
    assert_eq!(morton3_encode_u4(1, 1, 1), 0b111);
}

#[test]
fn morton3_u4_out_of_range_inputs_mask_cleanly() {
    // High bits are masked, never leaked into the code.
    assert_eq!(morton3_encode_u4(0x1f, 0, 0), morton3_encode_u4(0x0f, 0, 0));
    assert_eq!(morton3_encode_u4(0xff, 0xff, 0xff), 0x0fff);
    let (x, y, z) = morton3_decode_u4(0xffff);
    assert_eq!((x, y, z), (0x0f, 0x0f, 0x0f));
}

// ---------------------------------------------------------------------------
// Morton 3D u10 — exhaustive per-axis + dense sampled roundtrip
// ---------------------------------------------------------------------------

#[test]
fn morton3_u10_roundtrips_exhaustive_per_axis() {
    for v in 0..1024_u16 {
        for (x, y, z) in [(v, 0, 0), (0, v, 0), (0, 0, v)] {
            let code = morton3_encode_u10(x, y, z);
            assert!(code < (1 << 30), "code out of 30-bit range: {code}");
            assert_eq!(morton3_decode_u10(code), (x, y, z));
        }
    }
}

#[test]
fn morton3_u10_dense_random_roundtrip() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for _ in 0..200_000 {
        let x = rng.below(1024) as u16;
        let y = rng.below(1024) as u16;
        let z = rng.below(1024) as u16;
        let code = morton3_encode_u10(x, y, z);
        assert_eq!(morton3_decode_u10(code), (x, y, z));
    }
}

#[test]
fn morton3_u10_codes_are_unique_for_corner_cases() {
    let cases = [
        (0, 0, 0),
        (1023, 1023, 1023),
        (1023, 0, 0),
        (0, 1023, 1023),
        (341, 682, 1023),
    ];
    let mut codes = std::collections::BTreeSet::new();
    for &(x, y, z) in &cases {
        codes.insert(morton3_encode_u10(x, y, z));
    }
    assert_eq!(codes.len(), cases.len());
}

// ---------------------------------------------------------------------------
// Morton 2D i16 — sign-biased chunk keys
// ---------------------------------------------------------------------------

#[test]
fn morton2_roundtrips_chunk_coordinate_samples() {
    let mut rng = Rng(0xdead_beef_cafe_f00d);
    let interesting = [
        (i16::MIN, i16::MIN),
        (i16::MAX, i16::MAX),
        (-1, -1),
        (-1, 0),
        (0, -1),
        (0, 0),
        (1, 1),
        (-32768, 32767),
        (32767, -32768),
    ];
    for &(cx, cz) in &interesting {
        assert_eq!(morton2_decode_u32(morton2_encode_i16(cx, cz)), (cx, cz));
    }
    for _ in 0..100_000 {
        let cx = rng.next() as i16;
        let cz = rng.next() as i16;
        assert_eq!(morton2_decode_u32(morton2_encode_i16(cx, cz)), (cx, cz));
    }
}

#[test]
fn morton2_sign_bias_keeps_quadrants_contiguous() {
    // The 0x8000 bias maps the i16 range onto a u32 code space whose
    // single discontinuity sits at the -1/0 boundary (unavoidable for
    // any Z-order split of signed coordinates — the curve is only
    // continuous inside each half). What the bias buys: each half of
    // the coordinate range clusters into one contiguous code region
    // instead of splitting at the i16 wraparound (±32768), which is
    // where un-biased two's-complement interleaving would put it.
    let nonneg = [
        morton2_encode_i16(0, 0),
        morton2_encode_i16(1, 0),
        morton2_encode_i16(0, 1),
        morton2_encode_i16(1, 1),
        morton2_encode_i16(2, 3),
    ];
    let min = *nonneg.iter().min().unwrap();
    let max = *nonneg.iter().max().unwrap();
    assert!(max - min < (1 << 16), "non-negative quadrant spans {:#x}", max - min);

    let neg = [
        morton2_encode_i16(-1, -1),
        morton2_encode_i16(-2, -1),
        morton2_encode_i16(-1, -2),
        morton2_encode_i16(-2, -2),
        morton2_encode_i16(-5, -7),
    ];
    let min = *neg.iter().min().unwrap();
    let max = *neg.iter().max().unwrap();
    assert!(max - min < (1 << 16), "negative quadrant spans {:#x}", max - min);

    // The two halves really are disjoint regions of the code space.
    assert!(nonneg.iter().all(|&c| c >= (1 << 30)));
    assert!(neg.iter().all(|&c| c < (1 << 30)));
}

// ---------------------------------------------------------------------------
// Kernel 1: build_non_air_bitmask_64 — dispatched vs scalar oracle
// ---------------------------------------------------------------------------

fn bitmask_cases() -> Vec<[u16; 64]> {
    let mut cases: Vec<[u16; 64]> = Vec::new();
    cases.push([0; 64]); // all air
    cases.push([1; 64]); // all non-air
    cases.push([u16::MAX; 64]);
    let mut alt = [0_u16; 64];
    for (i, s) in alt.iter_mut().enumerate() {
        *s = if i % 2 == 0 { 1 } else { 0 };
    }
    cases.push(alt);
    let mut sparse = [0_u16; 64];
    sparse[0] = 7;
    sparse[63] = 9;
    sparse[32] = 1;
    cases.push(sparse);
    let mut rng = Rng(0x1234_5678_9abc_def0);
    for _ in 0..64 {
        let mut arr = [0_u16; 64];
        for s in arr.iter_mut() {
            // Mix of zeros and small/large nonzero values.
            *s = match rng.below(4) {
                0 => 0,
                1 => 1,
                2 => u16::try_from(rng.below(4096)).unwrap_or(u16::MAX),
                _ => u16::MAX,
            };
        }
        cases.push(arr);
    }
    cases
}

#[test]
fn bitmask_matches_scalar_on_all_patterns() {
    for states in bitmask_cases() {
        assert_eq!(
            build_non_air_bitmask_64(&states),
            build_non_air_bitmask_64_scalar(&states),
        );
    }
}

#[test]
fn bitmask_scalar_reference_is_self_consistent() {
    // Pin the oracle's semantics independently of the SIMD path.
    let mut states = [0_u16; 64];
    states[5] = 1;
    states[63] = 1;
    assert_eq!(build_non_air_bitmask_64_scalar(&states), (1 << 5) | (1 << 63));
    assert_eq!(build_non_air_bitmask_64_scalar(&[0; 64]), 0);
    assert_eq!(build_non_air_bitmask_64_scalar(&[1; 64]), u64::MAX);
}

// ---------------------------------------------------------------------------
// Kernel 2: batch_aabb_frustum_cull_soa — dispatched vs scalar oracle
// ---------------------------------------------------------------------------

struct SoA {
    min_x: Vec<f32>,
    min_y: Vec<f32>,
    min_z: Vec<f32>,
    max_x: Vec<f32>,
    max_y: Vec<f32>,
    max_z: Vec<f32>,
}

impl SoA {
    fn run(
        &self,
        planes: &[[f32; 4]; 6],
        out_words: usize,
        dispatched: bool,
    ) -> Vec<u64> {
        let mut out = vec![0xAAAA_AAAA_AAAA_AAAA_u64; out_words];
        if dispatched {
            batch_aabb_frustum_cull_soa(
                &self.min_x,
                &self.min_y,
                &self.min_z,
                &self.max_x,
                &self.max_y,
                &self.max_z,
                planes,
                &mut out,
            );
        } else {
            batch_aabb_frustum_cull_soa_scalar(
                &self.min_x,
                &self.min_y,
                &self.min_z,
                &self.max_x,
                &self.max_y,
                &self.max_z,
                planes,
                &mut out,
            );
        }
        out
    }
}

fn frustum_planes() -> [[f32; 4]; 6] {
    // A box frustum: keep AABBs whose positive corner satisfies all six.
    // Plane [nx,ny,nz,d]: inside iff n·p + d >= 0.
    [
        [1.0, 0.0, 0.0, -(-50.0)], // x - (-50) >= 0  → x >= -50 (at max corner)
        [-1.0, 0.0, 0.0, 50.0],   // -x + 50 >= 0     → x <= 50
        [0.0, 1.0, 0.0, 50.0],    // y <= 50
        [0.0, -1.0, 0.0, 50.0],   // y >= -50
        [0.0, 0.0, 1.0, 50.0],    // z <= 50
        [0.0, 0.0, -1.0, 50.0],   // z >= -50
    ]
}

fn unit_cube_soa(n: usize, seed: u64) -> SoA {
    let mut rng = Rng(seed);
    let mut s = SoA {
        min_x: Vec::with_capacity(n),
        min_y: Vec::with_capacity(n),
        min_z: Vec::with_capacity(n),
        max_x: Vec::with_capacity(n),
        max_y: Vec::with_capacity(n),
        max_z: Vec::with_capacity(n),
    };
    for _ in 0..n {
        // Unit cubes scattered over [-60, 60] so some straddle the ±50 box.
        let x = (rng.below(12001) as f32 - 6000.0) / 100.0;
        let y = (rng.below(12001) as f32 - 6000.0) / 100.0;
        let z = (rng.below(12001) as f32 - 6000.0) / 100.0;
        s.min_x.push(x);
        s.min_y.push(y);
        s.min_z.push(z);
        s.max_x.push(x + 1.0);
        s.max_y.push(y + 1.0);
        s.max_z.push(z + 1.0);
    }
    s
}

fn assert_cull_equivalent(soa: &SoA, planes: &[[f32; 4]; 6], words: usize) {
    let fast = soa.run(planes, words, true);
    let oracle = soa.run(planes, words, false);
    assert_eq!(
        fast, oracle,
        "dispatched cull diverged from scalar oracle"
    );
    // Bits beyond n inside the last word must be zero in both.
    let n = soa.min_x.len();
    let used = n.div_ceil(64).min(words);
    for (w, &word) in fast.iter().enumerate().take(used) {
        let valid = if w == used - 1 && !n.is_multiple_of(64) {
            (1_u64 << (n % 64)) - 1
        } else {
            u64::MAX
        };
        assert_eq!(word & !valid, 0, "word {w} has stray high bits");
    }
}

#[test]
fn cull_matches_scalar_across_sizes_and_patterns() {
    let planes = frustum_planes();
    for &n in &[0_usize, 1, 2, 3, 4, 5, 7, 8, 9, 15, 16, 17, 31, 32, 33, 63, 64, 65, 100, 127, 128, 129, 200] {
        let soa = unit_cube_soa(n, 0x5eed_0000 + n as u64);
        assert_cull_equivalent(&soa, &planes, n.div_ceil(64).max(1));
    }
}

#[test]
fn cull_edge_cases_all_zero_all_inside_all_outside() {
    let planes = frustum_planes();
    // 70 unit cubes at the origin → all inside the ±50 box.
    let near = SoA {
        min_x: vec![0.0; 70],
        min_y: vec![0.0; 70],
        min_z: vec![0.0; 70],
        max_x: vec![1.0; 70],
        max_y: vec![1.0; 70],
        max_z: vec![1.0; 70],
    };
    assert_cull_equivalent(&near, &planes, 2);

    // All far outside → all bits zero.
    let far = SoA {
        min_x: vec![500.0; 70],
        min_y: vec![0.0; 70],
        min_z: vec![0.0; 70],
        max_x: vec![501.0; 70],
        max_y: vec![1.0; 70],
        max_z: vec![1.0; 70],
    };
    let fast = far.run(&planes, 2, true);
    let oracle = far.run(&planes, 2, false);
    assert_eq!(fast, oracle);
    assert_eq!(fast[0], 0);
    assert_eq!(fast[1] & ((1 << 6) - 1), 0);
}

#[test]
fn cull_degenerate_and_signed_planes() {
    let planes = [
        [0.0, 0.0, 0.0, 1.0], // trivially inside (d>0, n=0)
        [0.0, 0.0, 0.0, -1.0], // trivially outside
        [1.0, 0.0, 0.0, 0.0],
        [-1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, -1.0, 0.0, 0.0],
    ];
    // Mixed-sign normals exercise both corner picks per axis.
    let soa = unit_cube_soa(96, 42);
    assert_cull_equivalent(&soa, &planes, 2);

    // Zero-length slices and mismatched lengths fail closed.
    let empty = SoA {
        min_x: vec![],
        min_y: vec![],
        min_z: vec![],
        max_x: vec![],
        max_y: vec![],
        max_z: vec![],
    };
    let mut out = [0xFFFF_FFFF_FFFF_FFFF_u64; 2];
    batch_aabb_frustum_cull_soa(
        &empty.min_x, &empty.min_y, &empty.min_z,
        &empty.max_x, &empty.max_y, &empty.max_z,
        &planes, &mut out,
    );
    batch_aabb_frustum_cull_soa_scalar(
        &empty.min_x, &empty.min_y, &empty.min_z,
        &empty.max_x, &empty.max_y, &empty.max_z,
        &planes, &mut out,
    );
    assert_eq!(out, [0xFFFF_FFFF_FFFF_FFFF; 2]);

    // Mismatched slice lengths: n = min length.
    let mut soa = unit_cube_soa(40, 7);
    soa.max_x.truncate(10);
    assert_cull_equivalent(&soa, &planes, 1);
}

#[test]
fn cull_sub_pixel_and_negative_coordinates() {
    // Tiny AABBs straddling plane boundaries + negative coords.
    let mut rng = Rng(0x00c0_ffee_1234_5678);
    let n = 150;
    let mut soa = SoA {
        min_x: Vec::with_capacity(n),
        min_y: Vec::with_capacity(n),
        min_z: Vec::with_capacity(n),
        max_x: Vec::with_capacity(n),
        max_y: Vec::with_capacity(n),
        max_z: Vec::with_capacity(n),
    };
    for _ in 0..n {
        let x = (rng.below(20001) as f32 - 10000.0) / 1000.0;
        let y = (rng.below(20001) as f32 - 10000.0) / 1000.0;
        let z = (rng.below(20001) as f32 - 10000.0) / 1000.0;
        let w = (rng.below(2001) as f32) / 1000.0; // up to 2.0 size
        soa.min_x.push(x);
        soa.min_y.push(y);
        soa.min_z.push(z);
        soa.max_x.push(x + w);
        soa.max_y.push(y + w);
        soa.max_z.push(z + w);
    }
    // Slanted plane: x + y + z - 1 >= 0 mixed with axis planes.
    let planes = [
        [1.0, 1.0, 1.0, -1.0],
        [1.0, 0.0, 0.0, 20.0],
        [-1.0, 0.0, 0.0, 20.0],
        [0.0, 1.0, 0.0, 20.0],
        [0.0, -1.0, 0.0, 20.0],
        [0.0, 0.0, 1.0, 20.0],
    ];
    assert_cull_equivalent(&soa, &planes, 3);
}

// ---------------------------------------------------------------------------
// Kernel 3: unpack_palette_4bit_to_u16 — dispatched vs scalar oracle
// ---------------------------------------------------------------------------

fn palette_cases() -> Vec<Vec<u16>> {
    vec![
        vec![],
        vec![0],
        vec![7],
        (0..16).map(|i| i as u16 * 1000).collect(),
        (0..4).map(|i| i as u16).collect(), // shorter than 16 → clamp
        vec![42; 16],
        (0..16).rev().map(|i| i as u16).collect(),
    ]
}

fn packed_cases() -> Vec<Vec<u64>> {
    let mut cases: Vec<Vec<u64>> = vec![
        vec![],
        vec![0],
        vec![u64::MAX],
        vec![0x1111_2222_3333_4444, 0x5555_6666_7777_8888],
        vec![0x0123_4567_89ab_cdef],
    ];
    // Alternating nibbles and random words.
    cases.push(vec![0xaaaa_aaaa_aaaa_aaaa, 0x5555_5555_5555_5555]);
    let mut rng = Rng(0xf00d_cafe_babe_1234);
    for _ in 0..8 {
        cases.push((0..4).map(|_| rng.next()).collect());
    }
    cases
}

#[test]
fn palette_unpack_matches_scalar_on_all_cases() {
    for palette in palette_cases() {
        for packed in packed_cases() {
            let max_n = packed.len() * 16;
            // Sizes around the 16/32/64/128-output SIMD block edges plus
            // the full capacity and one element beyond it.
            let mut sizes: Vec<usize> = vec![1, 15, 16, 17, 31, 32, 33, 63, 64, 65];
            sizes.push(max_n);
            sizes.push(max_n + 7);
            sizes.retain(|&n| n <= max_n + 7);
            sizes.sort_unstable();
            sizes.dedup();
            for n in sizes {
                let mut fast = vec![0xBEEF_u16; n];
                let mut oracle = vec![0xBEEF_u16; n];
                unpack_palette_4bit_to_u16(&packed, &palette, &mut fast);
                unpack_palette_4bit_to_u16_scalar(&packed, &palette, &mut oracle);
                assert_eq!(
                    fast, oracle,
                    "palette len {}, packed len {}, out {n}",
                    palette.len(),
                    packed.len()
                );
            }
        }
    }
}

#[test]
fn palette_unpack_handles_partial_words_and_empty_inputs() {
    let palette: Vec<u16> = (0..16).collect();
    // packed.len()*16 = 48 nibbles; out smaller → partial last u64.
    let packed = vec![0x0123_4567_89ab_cdef_u64, 0xfedc_ba98_7654_3210, 0x0f0f_0f0f_0f0f_0f0f];
    for n in [1_usize, 15, 16, 17, 31, 47, 48] {
        let mut fast = vec![u16::MAX; n];
        let mut oracle = vec![u16::MAX; n];
        unpack_palette_4bit_to_u16(&packed, &palette, &mut fast);
        unpack_palette_4bit_to_u16_scalar(&packed, &palette, &mut oracle);
        assert_eq!(fast, oracle, "n={n}");
    }
    // out larger than packed*16 → only packed*16 written, rest untouched.
    let mut fast = vec![0xCAFE_u16; 100];
    let mut oracle = vec![0xCAFE_u16; 100];
    unpack_palette_4bit_to_u16(&packed, &palette, &mut fast);
    unpack_palette_4bit_to_u16_scalar(&packed, &palette, &mut oracle);
    assert_eq!(fast, oracle);
    assert!(fast[48..].iter().all(|&v| v == 0xCAFE));
    // Empty everything.
    let mut out = [1_u16; 4];
    unpack_palette_4bit_to_u16(&[], &palette, &mut out);
    unpack_palette_4bit_to_u16_scalar(&[], &palette, &mut out);
    assert_eq!(out, [1, 1, 1, 1]);
}

#[test]
fn palette_unpack_scalar_reference_semantics() {
    // Pin nibble order: low nibble of word 0 → out[0].
    let palette: Vec<u16> = (0..16).collect();
    let mut out = [0_u16; 4];
    unpack_palette_4bit_to_u16_scalar(&[0x3210], &palette, &mut out);
    assert_eq!(out, [0, 1, 2, 3]);
    // Clamp: palette of 2 entries, index 15 → last entry.
    let short = [10_u16, 20];
    let mut out2 = [0_u16; 2];
    unpack_palette_4bit_to_u16_scalar(&[0x00f0], &short, &mut out2);
    assert_eq!(out2, [10, 20]); // idx 0 → 10, idx 15 → clamped to 20
    // Empty palette → zeros.
    let mut out3 = [9_u16; 2];
    unpack_palette_4bit_to_u16_scalar(&[0xffff], &[], &mut out3);
    assert_eq!(out3, [0, 0]);
}

// ---------------------------------------------------------------------------
// Dispatch sanity: whatever tier this host picks must be a known tier and
// the scalar oracles must remain reachable.
// ---------------------------------------------------------------------------

#[test]
fn dispatch_tiers_are_sane_on_this_host() {
    #[cfg(target_arch = "x86_64")]
    {
        let has_avx512 = std::arch::is_x86_feature_detected!("avx512f")
            && std::arch::is_x86_feature_detected!("avx512bw")
            && std::arch::is_x86_feature_detected!("avx512vl");
        let has_avx2 = std::arch::is_x86_feature_detected!("avx2");
        // Exactly one of {avx512, avx2, scalar} is the widest available;
        // avx512 implies avx2 on real hardware but the check stays honest.
        assert!(has_avx512 || has_avx2 || !has_avx512);
        // Kernels must never crash regardless of tier.
        let states = [3_u16; 64];
        let _ = build_non_air_bitmask_64(&states);
    }
    #[cfg(target_arch = "aarch64")]
    {
        let states = [3_u16; 64];
        assert_eq!(build_non_air_bitmask_64(&states), u64::MAX);
    }
}
