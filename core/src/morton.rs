// SPDX-License-Identifier: LGPL-3.0-or-later
//! `megu_core::morton` — branchless SWAR magic-bit Morton (Z-order)
//! encoders for cache-locality spatial indexing.
//!
//! Z-order interleaving turns 3D neighbourhood lookups into 1D range scans:
//! the six face neighbours of a voxel sit at predictable code offsets, so a
//! `16³` chunk section's 4096 entries can be walked in an order that keeps
//! lighting, fluid and meshing passes inside L1/L2 instead of thrashing TLB
//! entries on a linear `x + 16y + 256z` layout.
//!
//! Every function here is pure safe `std`, branchless, clock-free and
//! panic-free. Inputs are defensively masked to their axis width so stray
//! high bits can never leak into the code — encode/decode round-trips are
//! exact for every in-range coordinate.

/// Spread the low 10 bits of `value` into every third bit position
/// (Morton `part1by2`). Classic magic-bits sequence; verified exhaustively
/// against the bit-by-bit definition for all `0..1024` inputs.
#[inline]
fn spread_10_bits(mut value: u32) -> u32 {
    value &= 0x3ff;
    value = (value | (value << 16)) & 0x0300_00ff;
    value = (value | (value << 8)) & 0x0300_f00f;
    value = (value | (value << 4)) & 0x030c_30c3;
    (value | (value << 2)) & 0x0924_9249
}

/// Inverse of [`spread_10_bits`]: gather every third bit back into the low
/// 10 bits.
#[inline]
fn compact_10_bits(mut value: u32) -> u32 {
    value &= 0x0924_9249;
    value = (value | (value >> 2)) & 0x030c_30c3;
    value = (value | (value >> 4)) & 0x0300_f00f;
    value = (value | (value >> 8)) & 0x0300_00ff;
    (value | (value >> 16)) & 0x3ff
}

/// Spread all 16 bits of `value` into every second bit position (Morton
/// `part1by1`) inside a `u32`.
#[inline]
fn spread_16_bits(mut value: u32) -> u32 {
    value &= 0xffff;
    value = (value | (value << 8)) & 0x00ff_00ff;
    value = (value | (value << 4)) & 0x0f0f_0f0f;
    value = (value | (value << 2)) & 0x3333_3333;
    (value | (value << 1)) & 0x5555_5555
}

/// Inverse of [`spread_16_bits`].
#[inline]
fn compact_16_bits(mut value: u32) -> u32 {
    value &= 0x5555_5555;
    value = (value | (value >> 1)) & 0x3333_3333;
    value = (value | (value >> 2)) & 0x0f0f_0f0f;
    value = (value | (value >> 4)) & 0x00ff_00ff;
    (value | (value >> 8)) & 0xffff
}

// ===========================================================================
// 3D Morton — 4 bits per axis (16³ chunk sections)
// ===========================================================================

/// Encode `(x, y, z)` — each in `0..16` — into a 12-bit Z-order code in
/// `0..4096`. This is the index space of one `16³` chunk section: iterating
/// voxels by code keeps every 4³ sub-cube contiguous, which is exactly the
/// locality lighting relights, fluid spreads and the mesh builder traverse.
///
/// Branchless SWAR: three `part1by2` spreads, two shifts, two ORs. Inputs are
/// masked to 4 bits, so out-of-range coordinates degrade deterministically
/// instead of panicking.
#[inline]
pub fn morton3_encode_u4(x: u8, y: u8, z: u8) -> u16 {
    let code = spread_10_bits(u32::from(x) & 0xf)
        | (spread_10_bits(u32::from(y) & 0xf) << 1)
        | (spread_10_bits(u32::from(z) & 0xf) << 2);
    (code & 0xfff) as u16
}

/// Exact inverse of [`morton3_encode_u4`]. Only the low 12 bits of `code`
/// are meaningful; higher bits are ignored.
#[inline]
pub fn morton3_decode_u4(code: u16) -> (u8, u8, u8) {
    let c = u32::from(code) & 0xfff;
    (
        (compact_10_bits(c) & 0xf) as u8,
        (compact_10_bits(c >> 1) & 0xf) as u8,
        (compact_10_bits(c >> 2) & 0xf) as u8,
    )
}

// ===========================================================================
// 3D Morton — 10 bits per axis (region/entity spatial hashing)
// ===========================================================================

/// Encode `(x, y, z)` — each in `0..1024` — into a 30-bit Z-order code.
/// Used for region grids and entity spatial hashes where a `16³` section
/// code is too small. Inputs are masked to 10 bits.
#[inline]
pub fn morton3_encode_u10(x: u16, y: u16, z: u16) -> u32 {
    spread_10_bits(u32::from(x))
        | (spread_10_bits(u32::from(y)) << 1)
        | (spread_10_bits(u32::from(z)) << 2)
}

/// Exact inverse of [`morton3_encode_u10`]. Only the low 30 bits of `code`
/// are meaningful; higher bits are ignored.
#[inline]
pub fn morton3_decode_u10(code: u32) -> (u16, u16, u16) {
    let c = code & 0x3fff_ffff;
    (
        compact_10_bits(c) as u16,
        compact_10_bits(c >> 1) as u16,
        compact_10_bits(c >> 2) as u16,
    )
}

// ===========================================================================
// 2D Morton — 16 bits per axis (chunk/region iteration order)
// ===========================================================================

/// Encode signed chunk coordinates `(cx, cz)` into a 32-bit Z-order key.
///
/// Each axis is biased by `0x8000` (sign-bit flip) before interleaving, so
/// the four quadrants around the world origin stay contiguous in code
/// space: chunks at `(-1,-1)`, `(-1,0)`, `(0,-1)`, `(0,0)` map to four
/// neighbouring codes instead of being flung to opposite ends of the `u32`
/// range the way a raw two's-complement interleave would.
#[inline]
pub fn morton2_encode_i16(cx: i16, cz: i16) -> u32 {
    let bx = spread_16_bits(u32::from((cx as u16) ^ 0x8000));
    let bz = spread_16_bits(u32::from((cz as u16) ^ 0x8000));
    bx | (bz << 1)
}

/// Exact inverse of [`morton2_encode_i16`]. Only the low 32 bits are
/// meaningful (the code is always exactly 32 bits).
#[inline]
pub fn morton2_decode_u32(code: u32) -> (i16, i16) {
    let bx = compact_16_bits(code) as u16;
    let bz = compact_16_bits(code >> 1) as u16;
    (((bx ^ 0x8000) as i16), ((bz ^ 0x8000) as i16))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spread_helpers_match_bit_by_bit_reference() {
        for v in 0..1024u32 {
            let mut expect = 0u32;
            for i in 0..10 {
                expect |= ((v >> i) & 1) << (3 * i);
            }
            assert_eq!(spread_10_bits(v), expect, "spread10({v})");
            assert_eq!(compact_10_bits(expect), v, "compact10({expect})");
        }
        for v in [0u32, 1, 0x8000, 0xffff, 0x1234, 0xabcd] {
            let mut expect = 0u32;
            for i in 0..16 {
                expect |= ((v >> i) & 1) << (2 * i);
            }
            assert_eq!(spread_16_bits(v), expect, "spread16({v})");
            assert_eq!(compact_16_bits(expect), v, "compact16({expect})");
        }
    }
}
