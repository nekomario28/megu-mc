// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! `megu_core::simd_voxel` — runtime-dispatched, hardware-aware low-level
//! compute kernels for the voxel hot paths.
//!
//! Three kernels, each with a portable scalar reference that is
//! bit-identical to every SIMD tier by construction (same IEEE-754
//! operation order, no FMA contraction, identical NaN semantics):
//!
//! * [`build_non_air_bitmask_64`] — packs the non-air predicate of a
//!   `16×4×4`-ish 64-block of `u16` block states into one `u64` mask.
//!   Zen 5 / AVX-512 does it in two `_mm512_cmpneq_epi16_mask` + one OR;
//!   AVX2 in four compare+pack+movemask chains; NEON in eight
//!   `vceqq_u16`+narrow passes; scalar is a branchless 64-iteration loop
//!   that is safe on a Core 2 Duo (x86-64-v1, no SSE4+ required).
//! * [`batch_aabb_frustum_cull_soa`] — structure-of-arrays frustum
//!   culling: 16 AABBs per pass on AVX-512, 8 on AVX2, 4 on NEON, 1 on
//!   scalar, emitting packed `u64` visibility words with zero per-AABB
//!   branching. Plane normals select the min- or max-corner per axis
//!   exactly like the scalar oracle, including `-0.0`/NaN behaviour.
//! * [`unpack_palette_4bit_to_u16`] — expands 4-bit palette indices out
//!   of `u64` words into `u16` texel values with bounds-safe clamping.
//!   SIMD tiers vectorise the nibble expansion; the palette gather
//!   itself is shared scalar code so every tier produces identical
//!   bytes.
//!
//! Dispatch is strictly runtime-guarded: every `#[target_feature]`
//! kernel is only reachable behind `is_x86_feature_detected!` (or the
//! architecturally-guaranteed AArch64 NEON baseline), and every kernel
//! has a bit-identical safe fallback. No clock, no allocation in the hot
//! loops, no panics.

#![allow(unsafe_code)]

#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;
#[cfg(target_arch = "aarch64")]
use std::arch::aarch64::*;

// ===========================================================================
// Shared scalar oracles (also the portable fallback for every kernel)
// ===========================================================================

/// Scalar reference for [`build_non_air_bitmask_64`]: bit `i` of the
/// result is `1` iff `states[i] != 0`.
#[inline]
pub fn build_non_air_bitmask_64_scalar(states: &[u16; 64]) -> u64 {
    let mut bits = 0_u64;
    for (i, &s) in states.iter().enumerate() {
        bits |= u64::from(s != 0) << i;
    }
    bits
}

/// One plane of the frustum: `[nx, ny, nz, d]`, inside iff
/// `nx*x + ny*y + nz*z + d >= 0` for the extreme corner selected by the
/// normal's signs.
#[inline]
#[allow(clippy::neg_cmp_op_on_partial_ord)] // `!(n < 0.0)` is the exact SIMD NLT predicate incl. NaN.
fn plane_axis_pick(normal: f32, lo: f32, hi: f32) -> f32 {
    // `!(n < 0.0)` is deliberately ordered-less-than negated: it matches
    // the SIMD `_CMP_NLT_US`/`vcltq` predicates bit-for-bit, including
    // `-0.0` (picks `hi`) and NaN (also picks `hi`).
    if !(normal < 0.0) {
        hi
    } else {
        lo
    }
}

/// Scalar per-AABB frustum test shared by the oracle and every SIMD
/// tail. The distance expression order is fixed — SIMD kernels emit the
/// identical `((nx*px + ny*py) + nz*pz) + d` sequence with no FMA.
#[inline]
#[allow(clippy::too_many_arguments)]
#[allow(clippy::neg_cmp_op_on_partial_ord)] // `!(d >= 0.0)` mirrors _CMP_GE_OQ exactly.
fn aabb_visible_at(
    idx: usize,
    min_x: &[f32],
    min_y: &[f32],
    min_z: &[f32],
    max_x: &[f32],
    max_y: &[f32],
    max_z: &[f32],
    planes: &[[f32; 4]; 6],
) -> bool {
    for plane in planes.iter() {
        let px = plane_axis_pick(plane[0], min_x[idx], max_x[idx]);
        let py = plane_axis_pick(plane[1], min_y[idx], max_y[idx]);
        let pz = plane_axis_pick(plane[2], min_z[idx], max_z[idx]);
        let dist = plane[0] * px + plane[1] * py + plane[2] * pz + plane[3];
        if !(dist >= 0.0) {
            return false;
        }
    }
    true
}

/// Scalar reference for [`batch_aabb_frustum_cull_soa`]. `min_*`/`max_*`
/// are structure-of-arrays slices; bit `i` of the returned mask is set
/// iff AABB `i` (of the first `cnt` entries) passes all six planes.
#[inline]
#[allow(clippy::too_many_arguments)]
pub fn batch_aabb_frustum_cull_block_scalar(
    min_x: &[f32],
    min_y: &[f32],
    min_z: &[f32],
    max_x: &[f32],
    max_y: &[f32],
    max_z: &[f32],
    planes: &[[f32; 4]; 6],
) -> u64 {
    let cnt = min_x.len();
    let mut bits = 0_u64;
    for i in 0..cnt {
        if aabb_visible_at(i, min_x, min_y, min_z, max_x, max_y, max_z, planes) {
            bits |= 1_u64 << i;
        }
    }
    bits
}

/// Scalar reference for [`batch_aabb_frustum_cull_soa`] over the whole
/// batch: returns the packed visibility words exactly as the dispatched
/// entry point does.
#[allow(clippy::too_many_arguments)]
pub fn batch_aabb_frustum_cull_soa_scalar(
    min_x: &[f32],
    min_y: &[f32],
    min_z: &[f32],
    max_x: &[f32],
    max_y: &[f32],
    max_z: &[f32],
    planes: &[[f32; 4]; 6],
    out_visible_bits: &mut [u64],
) {
    batch_cull_into_words_scalar(
        min_x, min_y, min_z, max_x, max_y, max_z, planes, out_visible_bits,
    );
}

#[allow(clippy::too_many_arguments)]
fn batch_cull_into_words_scalar(
    min_x: &[f32],
    min_y: &[f32],
    min_z: &[f32],
    max_x: &[f32],
    max_y: &[f32],
    max_z: &[f32],
    planes: &[[f32; 4]; 6],
    out_visible_bits: &mut [u64],
) {
    let n = min_x
        .len()
        .min(min_y.len())
        .min(min_z.len())
        .min(max_x.len())
        .min(max_y.len())
        .min(max_z.len());
    let words = n.div_ceil(64).min(out_visible_bits.len());
    for (w, out_word) in out_visible_bits.iter_mut().enumerate().take(words) {
        let start = w * 64;
        let cnt = (n - start).min(64);
        *out_word = batch_aabb_frustum_cull_block_scalar(
            &min_x[start..start + cnt],
            &min_y[start..start + cnt],
            &min_z[start..start + cnt],
            &max_x[start..start + cnt],
            &max_y[start..start + cnt],
            &max_z[start..start + cnt],
            planes,
        );
    }
}

/// Bounds-safe palette lookup shared by every tier: indices are 4-bit
/// (`0..16`); a short palette clamps to its last entry and an empty
/// palette yields `0`.
#[inline]
fn palette_lookup(index: u8, palette: &[u16]) -> u16 {
    if palette.is_empty() {
        0
    } else {
        palette[usize::from(index).min(palette.len() - 1)]
    }
}

/// Expand one `u64` word's sixteen 4-bit indices (low nibble first) into
/// `out16` palette values. This is the scalar/SWAR reference; SIMD tiers
/// produce the identical index sequence vectorised.
#[inline]
pub fn unpack_palette_u64_scalar(word: u64, palette: &[u16], out16: &mut [u16]) {
    debug_assert!(out16.len() >= 16);
    for (j, slot) in out16[..16].iter_mut().enumerate() {
        let idx = ((word >> (4 * j)) & 0xf) as u8;
        *slot = palette_lookup(idx, palette);
    }
}

/// Scalar reference for [`unpack_palette_4bit_to_u16`]. Writes
/// `min(out.len(), packed.len()*16)` values; bit-exact with every SIMD
/// tier.
pub fn unpack_palette_4bit_to_u16_scalar(
    packed: &[u64],
    palette: &[u16],
    out: &mut [u16],
) {
    let n = out.len().min(packed.len().saturating_mul(16));
    let mut i = 0;
    while i + 16 <= n {
        unpack_palette_u64_scalar(packed[i / 16], palette, &mut out[i..i + 16]);
        i += 16;
    }
    while i < n {
        let idx = ((packed[i / 16] >> (4 * (i % 16))) & 0xf) as u8;
        out[i] = palette_lookup(idx, palette);
        i += 1;
    }
}

// ===========================================================================
// Kernel 1: 64-block non-air bitmask
// ===========================================================================

/// Pack the non-air predicate of 64 `u16` block states into one `u64`:
/// bit `i` is `1` iff `states[i] != 0`.
///
/// Dispatch order: AVX-512 (2 masked compares) → AVX2 (4× compare+pack)
/// → NEON (8× `vceqq`+narrow) → branchless scalar. Every tier is
/// bit-identical to [`build_non_air_bitmask_64_scalar`].
pub fn build_non_air_bitmask_64(states: &[u16; 64]) -> u64 {
    bitmask_dispatch(states)
}

#[cfg(target_arch = "x86_64")]
fn bitmask_dispatch(states: &[u16; 64]) -> u64 {
    if std::arch::is_x86_feature_detected!("avx512bw")
        && std::arch::is_x86_feature_detected!("avx512f")
    {
        // SAFETY: both features were just detected at runtime.
        return unsafe { bitmask_non_air_avx512(states) };
    }
    if std::arch::is_x86_feature_detected!("avx2") {
        // SAFETY: `avx2` was just detected at runtime.
        return unsafe { bitmask_non_air_avx2(states) };
    }
    build_non_air_bitmask_64_scalar(states)
}

#[cfg(target_arch = "aarch64")]
fn bitmask_dispatch(states: &[u16; 64]) -> u64 {
    // SAFETY: NEON is architecturally mandatory on AArch64.
    unsafe { bitmask_non_air_neon(states) }
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn bitmask_dispatch(states: &[u16; 64]) -> u64 {
    build_non_air_bitmask_64_scalar(states)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f,avx512bw,avx512vl")]
unsafe fn bitmask_non_air_avx512(states: &[u16; 64]) -> u64 {
    let zero = _mm512_setzero_si512();
    let ptr = states.as_ptr().cast::<__m512i>();
    let lo = _mm512_cmpneq_epi16_mask(_mm512_loadu_si512(ptr), zero) as u64;
    let hi = _mm512_cmpneq_epi16_mask(_mm512_loadu_si512(ptr.add(1)), zero) as u64;
    lo | (hi << 32)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn bitmask_non_air_avx2(states: &[u16; 64]) -> u64 {
    let zero = _mm256_setzero_si256();
    let mut acc = 0_u64;
    for i in 0..4 {
        let v = _mm256_loadu_si256(states.as_ptr().add(i * 16).cast::<__m256i>());
        // cmpeq yields 0xFFFF on *air* lanes; XOR with ones flips to the
        // non-air predicate before packing.
        let cmp = _mm256_xor_si256(
            _mm256_cmpeq_epi16(v, zero),
            _mm256_set1_epi16(-1),
        );
        // packs_epi16(lo, hi) yields the 16 lanes as 0xFF/0x00 bytes in
        // order; movemask_epi8 then gives one bit per lane directly.
        let packed = _mm_packs_epi16(
            _mm256_castsi256_si128(cmp),
            _mm256_extracti128_si256(cmp, 1),
        );
        let m = _mm_movemask_epi8(packed) as u32 & 0xffff;
        acc |= u64::from(m) << (i * 16);
    }
    acc
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn bitmask_non_air_neon(states: &[u16; 64]) -> u64 {
    let zero = vdupq_n_u16(0);
    let mut acc = 0_u64;
    for i in 0..8 {
        let v = vld1q_u16(states.as_ptr().add(i * 8));
        // u16 states are non-negative: `> 0` is exactly the non-air
        // predicate (vceqq would give the inverted air mask).
        let nz = vcgtq_u16(v, zero);
        let bytes = vmovn_u16(nz); // 0xFF per non-air lane, 0x00 otherwise
        let mut tmp = [0_u8; 8];
        vst1_u8(tmp.as_mut_ptr(), bytes);
        let mut m = 0_u64;
        for (j, &b) in tmp.iter().enumerate() {
            m |= u64::from(b >> 7) << j;
        }
        acc |= m << (i * 8);
    }
    acc
}

// ===========================================================================
// Kernel 2: SoA batch AABB frustum cull
// ===========================================================================

/// Frustum-cull `n` AABBs given as six structure-of-arrays slices plus a
/// fixed `[nx, ny, nz, d]` plane sextet, writing packed visibility words
/// into `out_visible_bits`.
///
/// Bit `i` of word `w` corresponds to AABB index `w*64 + i`; word `w`
/// covers indices `[64w, 64w+64)`. `n` is the minimum length across the
/// six coordinate slices (shorter slices fail closed). Only the first
/// `ceil(n/64)` words of `out_visible_bits` (capped at its length) are
/// written; bits beyond `n` inside the last word are always `0`.
///
/// Dispatch: AVX-512 (16 AABBs/vector) → AVX2 (8) → NEON (4) → scalar.
/// Every tier is bit-identical to [`batch_aabb_frustum_cull_soa_scalar`].
#[allow(clippy::too_many_arguments)]
pub fn batch_aabb_frustum_cull_soa(
    min_x: &[f32],
    min_y: &[f32],
    min_z: &[f32],
    max_x: &[f32],
    max_y: &[f32],
    max_z: &[f32],
    planes: &[[f32; 4]; 6],
    out_visible_bits: &mut [u64],
) {
    cull_dispatch(
        min_x, min_y, min_z, max_x, max_y, max_z, planes, out_visible_bits,
    );
}

#[cfg(target_arch = "x86_64")]
#[allow(clippy::too_many_arguments)]
fn cull_dispatch(
    min_x: &[f32],
    min_y: &[f32],
    min_z: &[f32],
    max_x: &[f32],
    max_y: &[f32],
    max_z: &[f32],
    planes: &[[f32; 4]; 6],
    out_visible_bits: &mut [u64],
) {
    if std::arch::is_x86_feature_detected!("avx512f")
        && std::arch::is_x86_feature_detected!("avx512bw")
        && std::arch::is_x86_feature_detected!("avx512vl")
    {
        // SAFETY: the full native-512 set was just detected.
        unsafe {
            batch_cull_into_words_avx512(
                min_x, min_y, min_z, max_x, max_y, max_z, planes, out_visible_bits,
            )
        };
        return;
    }
    if std::arch::is_x86_feature_detected!("avx2") {
        // SAFETY: `avx2` was just detected at runtime.
        unsafe {
            batch_cull_into_words_avx2(
                min_x, min_y, min_z, max_x, max_y, max_z, planes, out_visible_bits,
            )
        };
        return;
    }
    batch_cull_into_words_scalar(
        min_x, min_y, min_z, max_x, max_y, max_z, planes, out_visible_bits,
    );
}

#[cfg(target_arch = "aarch64")]
#[allow(clippy::too_many_arguments)]
fn cull_dispatch(
    min_x: &[f32],
    min_y: &[f32],
    min_z: &[f32],
    max_x: &[f32],
    max_y: &[f32],
    max_z: &[f32],
    planes: &[[f32; 4]; 6],
    out_visible_bits: &mut [u64],
) {
    // SAFETY: NEON is architecturally mandatory on AArch64.
    unsafe {
        batch_cull_into_words_neon(
            min_x, min_y, min_z, max_x, max_y, max_z, planes, out_visible_bits,
        )
    };
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
#[allow(clippy::too_many_arguments)]
fn cull_dispatch(
    min_x: &[f32],
    min_y: &[f32],
    min_z: &[f32],
    max_x: &[f32],
    max_y: &[f32],
    max_z: &[f32],
    planes: &[[f32; 4]; 6],
    out_visible_bits: &mut [u64],
) {
    batch_cull_into_words_scalar(
        min_x, min_y, min_z, max_x, max_y, max_z, planes, out_visible_bits,
    );
}

/// AVX2 cull block: 8 AABBs per vector pass over one ≤64-entry block,
/// scalar oracle for the sub-8 remainder.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn cull_block_avx2(
    min_x: &[f32],
    min_y: &[f32],
    min_z: &[f32],
    max_x: &[f32],
    max_y: &[f32],
    max_z: &[f32],
    planes: &[[f32; 4]; 6],
) -> u64 {
    let n = min_x.len();
    let zero = _mm256_setzero_ps();
    // Hoist the per-plane corner-select masks: they depend only on the
    // normal signs, not on the AABB lane.
    let mut sel = [_mm256_setzero_ps(); 18];
    for (j, plane) in planes.iter().enumerate() {
        for (k, &comp) in plane[..3].iter().enumerate() {
            sel[j * 3 + k] =
                _mm256_cmp_ps::<_CMP_NLT_US>(_mm256_set1_ps(comp), zero);
        }
    }
    let mut acc = 0_u64;
    let mut i = 0;
    while i + 8 <= n {
        let lx = _mm256_loadu_ps(min_x.as_ptr().add(i));
        let ly = _mm256_loadu_ps(min_y.as_ptr().add(i));
        let lz = _mm256_loadu_ps(min_z.as_ptr().add(i));
        let hx = _mm256_loadu_ps(max_x.as_ptr().add(i));
        let hy = _mm256_loadu_ps(max_y.as_ptr().add(i));
        let hz = _mm256_loadu_ps(max_z.as_ptr().add(i));
        let mut vis = _mm256_castsi256_ps(_mm256_set1_epi32(-1));
        for (j, plane) in planes.iter().enumerate() {
            let px = _mm256_blendv_ps(lx, hx, sel[j * 3]);
            let py = _mm256_blendv_ps(ly, hy, sel[j * 3 + 1]);
            let pz = _mm256_blendv_ps(lz, hz, sel[j * 3 + 2]);
            let nx = _mm256_set1_ps(plane[0]);
            let ny = _mm256_set1_ps(plane[1]);
            let nz = _mm256_set1_ps(plane[2]);
            // Same association order as the scalar oracle — never FMA.
            let dist = _mm256_add_ps(
                _mm256_add_ps(
                    _mm256_add_ps(_mm256_mul_ps(nx, px), _mm256_mul_ps(ny, py)),
                    _mm256_mul_ps(nz, pz),
                ),
                _mm256_set1_ps(plane[3]),
            );
            let ok = _mm256_cmp_ps::<_CMP_GE_OQ>(dist, zero);
            vis = _mm256_and_ps(vis, ok);
        }
        let m = _mm256_movemask_ps(vis) as u32;
        acc |= u64::from(m) << i;
        i += 8;
    }
    for j in i..n {
        if aabb_visible_at(j, min_x, min_y, min_z, max_x, max_y, max_z, planes) {
            acc |= 1_u64 << j;
        }
    }
    acc
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f,avx512bw,avx512vl")]
unsafe fn cull_block_avx512(
    min_x: &[f32],
    min_y: &[f32],
    min_z: &[f32],
    max_x: &[f32],
    max_y: &[f32],
    max_z: &[f32],
    planes: &[[f32; 4]; 6],
) -> u64 {
    let n = min_x.len();
    let zero = _mm512_setzero_ps();
    let mut sel = [0_u16; 18];
    for (j, plane) in planes.iter().enumerate() {
        for (k, &comp) in plane[..3].iter().enumerate() {
            sel[j * 3 + k] =
                _mm512_cmp_ps_mask::<_CMP_NLT_US>(_mm512_set1_ps(comp), zero);
        }
    }
    let mut acc = 0_u64;
    let mut i = 0;
    while i + 16 <= n {
        let lx = _mm512_loadu_ps(min_x.as_ptr().add(i));
        let ly = _mm512_loadu_ps(min_y.as_ptr().add(i));
        let lz = _mm512_loadu_ps(min_z.as_ptr().add(i));
        let hx = _mm512_loadu_ps(max_x.as_ptr().add(i));
        let hy = _mm512_loadu_ps(max_y.as_ptr().add(i));
        let hz = _mm512_loadu_ps(max_z.as_ptr().add(i));
        let mut ok_all: __mmask16 = 0xffff;
        for (j, plane) in planes.iter().enumerate() {
            let px = _mm512_mask_blend_ps(sel[j * 3], lx, hx);
            let py = _mm512_mask_blend_ps(sel[j * 3 + 1], ly, hy);
            let pz = _mm512_mask_blend_ps(sel[j * 3 + 2], lz, hz);
            let nx = _mm512_set1_ps(plane[0]);
            let ny = _mm512_set1_ps(plane[1]);
            let nz = _mm512_set1_ps(plane[2]);
            let dist = _mm512_add_ps(
                _mm512_add_ps(
                    _mm512_add_ps(_mm512_mul_ps(nx, px), _mm512_mul_ps(ny, py)),
                    _mm512_mul_ps(nz, pz),
                ),
                _mm512_set1_ps(plane[3]),
            );
            ok_all &= _mm512_cmp_ps_mask::<_CMP_GE_OQ>(dist, zero);
        }
        acc |= u64::from(ok_all) << i;
        i += 16;
    }
    for j in i..n {
        if aabb_visible_at(j, min_x, min_y, min_z, max_x, max_y, max_z, planes) {
            acc |= 1_u64 << j;
        }
    }
    acc
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn cull_block_neon(
    min_x: &[f32],
    min_y: &[f32],
    min_z: &[f32],
    max_x: &[f32],
    max_y: &[f32],
    max_z: &[f32],
    planes: &[[f32; 4]; 6],
) -> u64 {
    let n = min_x.len();
    let zero = vdupq_n_f32(0.0);
    let ones = vdupq_n_u32(u32::MAX);
    let mut acc = 0_u64;
    let mut i = 0;
    while i + 4 <= n {
        let lx = vld1q_f32(min_x.as_ptr().add(i));
        let ly = vld1q_f32(min_y.as_ptr().add(i));
        let lz = vld1q_f32(min_z.as_ptr().add(i));
        let hx = vld1q_f32(max_x.as_ptr().add(i));
        let hy = vld1q_f32(max_y.as_ptr().add(i));
        let hz = vld1q_f32(max_z.as_ptr().add(i));
        let mut vis = ones;
        for plane in planes.iter() {
            // mask = (n < 0) → pick min corner; NaN falls through to max,
            // matching `!(n < 0.0)` in the scalar oracle.
            let sx = vcltq_f32(vdupq_n_f32(plane[0]), zero);
            let sy = vcltq_f32(vdupq_n_f32(plane[1]), zero);
            let sz = vcltq_f32(vdupq_n_f32(plane[2]), zero);
            let px = vbslq_f32(sx, lx, hx);
            let py = vbslq_f32(sy, ly, hy);
            let pz = vbslq_f32(sz, lz, hz);
            let nx = vdupq_n_f32(plane[0]);
            let ny = vdupq_n_f32(plane[1]);
            let nz = vdupq_n_f32(plane[2]);
            let dist = vaddq_f32(
                vaddq_f32(
                    vaddq_f32(vmulq_f32(nx, px), vmulq_f32(ny, py)),
                    vmulq_f32(nz, pz),
                ),
                vdupq_n_f32(plane[3]),
            );
            let ok = vcgeq_f32(dist, zero);
            vis = vandq_u32(vis, ok);
        }
        let mut m = 0_u32;
        m |= vgetq_lane_u32::<0>(vis) >> 31;
        m |= (vgetq_lane_u32::<1>(vis) >> 31) << 1;
        m |= (vgetq_lane_u32::<2>(vis) >> 31) << 2;
        m |= (vgetq_lane_u32::<3>(vis) >> 31) << 3;
        acc |= u64::from(m) << i;
        i += 4;
    }
    for j in i..n {
        if aabb_visible_at(j, min_x, min_y, min_z, max_x, max_y, max_z, planes) {
            acc |= 1_u64 << j;
        }
    }
    acc
}

#[cfg(target_arch = "x86_64")]
#[allow(clippy::too_many_arguments)]
unsafe fn batch_cull_into_words_avx512(
    min_x: &[f32],
    min_y: &[f32],
    min_z: &[f32],
    max_x: &[f32],
    max_y: &[f32],
    max_z: &[f32],
    planes: &[[f32; 4]; 6],
    out_visible_bits: &mut [u64],
) {
    let n = min_x
        .len()
        .min(min_y.len())
        .min(min_z.len())
        .min(max_x.len())
        .min(max_y.len())
        .min(max_z.len());
    let words = n.div_ceil(64).min(out_visible_bits.len());
    for (w, out_word) in out_visible_bits.iter_mut().enumerate().take(words) {
        let start = w * 64;
        let cnt = (n - start).min(64);
        *out_word = cull_block_avx512(
            &min_x[start..start + cnt],
            &min_y[start..start + cnt],
            &min_z[start..start + cnt],
            &max_x[start..start + cnt],
            &max_y[start..start + cnt],
            &max_z[start..start + cnt],
            planes,
        );
    }
}

#[cfg(target_arch = "x86_64")]
#[allow(clippy::too_many_arguments)]
unsafe fn batch_cull_into_words_avx2(
    min_x: &[f32],
    min_y: &[f32],
    min_z: &[f32],
    max_x: &[f32],
    max_y: &[f32],
    max_z: &[f32],
    planes: &[[f32; 4]; 6],
    out_visible_bits: &mut [u64],
) {
    let n = min_x
        .len()
        .min(min_y.len())
        .min(min_z.len())
        .min(max_x.len())
        .min(max_y.len())
        .min(max_z.len());
    let words = n.div_ceil(64).min(out_visible_bits.len());
    for (w, out_word) in out_visible_bits.iter_mut().enumerate().take(words) {
        let start = w * 64;
        let cnt = (n - start).min(64);
        *out_word = cull_block_avx2(
            &min_x[start..start + cnt],
            &min_y[start..start + cnt],
            &min_z[start..start + cnt],
            &max_x[start..start + cnt],
            &max_y[start..start + cnt],
            &max_z[start..start + cnt],
            planes,
        );
    }
}

#[cfg(target_arch = "aarch64")]
#[allow(clippy::too_many_arguments)]
unsafe fn batch_cull_into_words_neon(
    min_x: &[f32],
    min_y: &[f32],
    min_z: &[f32],
    max_x: &[f32],
    max_y: &[f32],
    max_z: &[f32],
    planes: &[[f32; 4]; 6],
    out_visible_bits: &mut [u64],
) {
    let n = min_x
        .len()
        .min(min_y.len())
        .min(min_z.len())
        .min(max_x.len())
        .min(max_y.len())
        .min(max_z.len());
    let words = n.div_ceil(64).min(out_visible_bits.len());
    for (w, out_word) in out_visible_bits.iter_mut().enumerate().take(words) {
        let start = w * 64;
        let cnt = (n - start).min(64);
        *out_word = cull_block_neon(
            &min_x[start..start + cnt],
            &min_y[start..start + cnt],
            &min_z[start..start + cnt],
            &max_x[start..start + cnt],
            &max_y[start..start + cnt],
            &max_z[start..start + cnt],
            planes,
        );
    }
}

// ===========================================================================
// Kernel 3: 4-bit palette unpack to u16
// ===========================================================================

/// Unpack 4-bit palette indices packed 16-per-`u64` (low nibble = first
/// output) into `u16` values via `palette`, writing
/// `min(out.len(), packed.len()*16)` entries.
///
/// Dispatch: AVX-512 (8 words/vector) → AVX2 (4) → NEON (2) → scalar
/// SWAR. SIMD tiers vectorise only the nibble expansion; the palette
/// gather is shared scalar code, so every tier emits bit-identical
/// output by construction.
pub fn unpack_palette_4bit_to_u16(packed: &[u64], palette: &[u16], out: &mut [u16]) {
    unpack_palette_dispatch(packed, palette, out);
}

#[cfg(target_arch = "x86_64")]
fn unpack_palette_dispatch(packed: &[u64], palette: &[u16], out: &mut [u16]) {
    if std::arch::is_x86_feature_detected!("avx512bw")
        && std::arch::is_x86_feature_detected!("avx512f")
        && std::arch::is_x86_feature_detected!("avx512vl")
    {
        // SAFETY: the full native-512 set was just detected.
        unsafe {
            unpack_palette_dispatch_avx512(packed, palette, out);
        }
        return;
    }
    if std::arch::is_x86_feature_detected!("avx2") {
        // SAFETY: `avx2` was just detected at runtime.
        unsafe {
            unpack_palette_dispatch_avx2(packed, palette, out);
        }
        return;
    }
    unpack_palette_4bit_to_u16_scalar(packed, palette, out);
}

#[cfg(target_arch = "aarch64")]
fn unpack_palette_dispatch(packed: &[u64], palette: &[u16], out: &mut [u16]) {
    // SAFETY: NEON is architecturally mandatory on AArch64.
    unsafe {
        unpack_palette_dispatch_neon(packed, palette, out);
    }
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn unpack_palette_dispatch(packed: &[u64], palette: &[u16], out: &mut [u16]) {
    unpack_palette_4bit_to_u16_scalar(packed, palette, out);
}

/// Shared tail: scalar SWAR per `u64` then per-nibble for the remainder.
#[inline]
fn unpack_palette_tail(packed: &[u64], palette: &[u16], out: &mut [u16], mut i: usize, n: usize) {
    while i + 16 <= n {
        unpack_palette_u64_scalar(packed[i / 16], palette, &mut out[i..i + 16]);
        i += 16;
    }
    while i < n {
        let idx = ((packed[i / 16] >> (4 * (i % 16))) & 0xf) as u8;
        out[i] = palette_lookup(idx, palette);
        i += 1;
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn unpack_indices_avx2(words: *const u64, idx: *mut u8) {
    // 4 u64 → 64 nibble bytes. lo/hi hold the per-byte low/high nibbles;
    // unpack{lo,hi}_epi8 interleave them per 64-bit lane, and
    // permute2x128 restores u64 order.
    let v = _mm256_loadu_si256(words.cast::<__m256i>());
    let nib = _mm256_set1_epi64x(0x0f0f_0f0f_0f0f_0f0f);
    let lo = _mm256_and_si256(v, nib);
    let hi = _mm256_and_si256(_mm256_srli_epi16::<4>(v), nib);
    let u = _mm256_unpacklo_epi8(lo, hi); // u64 lanes 0,2 outputs
    let w = _mm256_unpackhi_epi8(lo, hi); // u64 lanes 1,3 outputs
    let a = _mm256_permute2x128_si256::<0x20>(u, w); // u64 0,1
    let b = _mm256_permute2x128_si256::<0x31>(u, w); // u64 2,3
    _mm256_storeu_si256(idx.cast::<__m256i>(), a);
    _mm256_storeu_si256(idx.add(32).cast::<__m256i>(), b);
}

#[cfg(target_arch = "x86_64")]
unsafe fn unpack_palette_dispatch_avx2(
    packed: &[u64],
    palette: &[u16],
    out: &mut [u16],
) {
    let n = out.len().min(packed.len().saturating_mul(16));
    let mut i = 0;
    let mut buf = [0_u8; 64];
    while i + 64 <= n {
        // SAFETY: caller verified `avx2`; 4 packed words are in bounds
        // because i+64 <= n <= packed.len()*16.
        unpack_indices_avx2(
            packed.as_ptr().add(i / 16),
            buf.as_mut_ptr(),
        );
        for (k, &ix) in buf.iter().enumerate() {
            out[i + k] = palette_lookup(ix, palette);
        }
        i += 64;
    }
    unpack_palette_tail(packed, palette, out, i, n);
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f,avx512bw,avx512vl")]
unsafe fn unpack_indices_avx512(words: *const u64, idx: *mut u8) {
    // 8 u64 → 128 nibble bytes. Same lo/hi + unpack trick as AVX2, then
    // the four 128-bit result lanes (u64 0,2,4,6 in `u`; 1,3,5,7 in `w`)
    // are deinterleaved with 256-bit permutes.
    let v = _mm512_loadu_si512(words.cast::<__m512i>());
    let nib = _mm512_set1_epi64(0x0f0f_0f0f_0f0f_0f0f);
    let lo = _mm512_and_si512(v, nib);
    let hi = _mm512_and_si512(_mm512_srli_epi16::<4>(v), nib);
    let u = _mm512_unpacklo_epi8(lo, hi);
    let w = _mm512_unpackhi_epi8(lo, hi);
    let u_lo = _mm512_castsi512_si256(u);
    let u_hi = _mm512_extracti64x4_epi64(u, 1);
    let w_lo = _mm512_castsi512_si256(w);
    let w_hi = _mm512_extracti64x4_epi64(w, 1);
    let a = _mm256_permute2x128_si256::<0x20>(u_lo, w_lo); // u64 0,1
    let b = _mm256_permute2x128_si256::<0x31>(u_lo, w_lo); // u64 2,3
    let c = _mm256_permute2x128_si256::<0x20>(u_hi, w_hi); // u64 4,5
    let d = _mm256_permute2x128_si256::<0x31>(u_hi, w_hi); // u64 6,7
    _mm256_storeu_si256(idx.cast::<__m256i>(), a);
    _mm256_storeu_si256(idx.add(32).cast::<__m256i>(), b);
    _mm256_storeu_si256(idx.add(64).cast::<__m256i>(), c);
    _mm256_storeu_si256(idx.add(96).cast::<__m256i>(), d);
}

#[cfg(target_arch = "x86_64")]
unsafe fn unpack_palette_dispatch_avx512(
    packed: &[u64],
    palette: &[u16],
    out: &mut [u16],
) {
    let n = out.len().min(packed.len().saturating_mul(16));
    let mut i = 0;
    let mut buf = [0_u8; 128];
    while i + 128 <= n {
        // SAFETY: caller verified the avx512 set; 8 packed words are in
        // bounds because i+128 <= n <= packed.len()*16.
        unpack_indices_avx512(packed.as_ptr().add(i / 16), buf.as_mut_ptr());
        for (k, &ix) in buf.iter().enumerate() {
            out[i + k] = palette_lookup(ix, palette);
        }
        i += 128;
    }
    // AVX2-sized middle blocks, then the scalar tail.
    if std::arch::is_x86_feature_detected!("avx2") {
        while i + 64 <= n {
            // SAFETY: `avx2` just detected; 4 packed words in bounds.
            unpack_indices_avx2(packed.as_ptr().add(i / 16), buf.as_mut_ptr());
            for k in 0..64 {
                out[i + k] = palette_lookup(buf[k], palette);
            }
            i += 64;
        }
    }
    unpack_palette_tail(packed, palette, out, i, n);
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn unpack_indices_neon(words: *const u64, idx: *mut u8) {
    // 2 u64 (16 bytes) → 32 nibble bytes via vzip1/vzip2 interleave.
    let v = vld1q_u8(words.cast::<u8>());
    let mask = vdupq_n_u8(0x0f);
    let lo = vandq_u8(v, mask);
    let hi = vshrq_n_u8::<4>(v);
    let z1 = vzip1q_u8(lo, hi); // first u64's 16 indices
    let z2 = vzip2q_u8(lo, hi); // second u64's 16 indices
    vst1q_u8(idx, z1);
    vst1q_u8(idx.add(16), z2);
}

#[cfg(target_arch = "aarch64")]
unsafe fn unpack_palette_dispatch_neon(
    packed: &[u64],
    palette: &[u16],
    out: &mut [u16],
) {
    let n = out.len().min(packed.len().saturating_mul(16));
    let mut i = 0;
    let mut buf = [0_u8; 32];
    while i + 32 <= n {
        // SAFETY: caller is on AArch64 (NEON baseline); 2 packed words
        // are in bounds because i+32 <= n <= packed.len()*16.
        unpack_indices_neon(packed.as_ptr().add(i / 16), buf.as_mut_ptr());
        for (k, &ix) in buf.iter().enumerate() {
            out[i + k] = palette_lookup(ix, palette);
        }
        i += 32;
    }
    unpack_palette_tail(packed, palette, out, i, n);
}
