// SPDX-License-Identifier: LGPL-3.0-or-later
#![forbid(unsafe_code)]
//! Canonical, dependency-free checksums and hash primitives.

/// Reflected IEEE 802.3 CRC-32 lookup table (polynomial `0xedb88320`).
pub const CRC32_IEEE_TABLE: [u32; 256] = crc32_table();

/// Slice-by-8 extension of [`CRC32_IEEE_TABLE`]: `CRC32_IEEE_TABLE8[n][b]`
/// is the single-byte CRC of `b` carried through `n` trailing zero bytes,
/// letting [`crc32_ieee_update`] consume eight bytes per iteration for
/// ~4-6x fewer table lookups with bit-identical results.
const CRC32_IEEE_TABLE8: [[u32; 256]; 8] = crc32_table8();

const fn crc32_table() -> [u32; 256] {
    let mut table = [0; 256];
    let mut i = 0;
    while i < table.len() {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 {
                0xedb8_8320 ^ (crc >> 1)
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

const fn crc32_table8() -> [[u32; 256]; 8] {
    let mut tables = [CRC32_IEEE_TABLE; 8];
    let mut t = 1;
    while t < 8 {
        let mut i = 0;
        while i < 256 {
            tables[t][i] =
                (tables[t - 1][i] >> 8) ^ tables[0][(tables[t - 1][i] & 0xff) as usize];
            i += 1;
        }
        t += 1;
    }
    tables
}

/// Advance unfinalized CRC `state` over one aligned 8-byte block.
#[inline]
fn crc32_block8(state: u32, block: &[u8; 8]) -> u32 {
    let x = u32::from_le_bytes([block[0], block[1], block[2], block[3]]) ^ state;
    let y = u32::from_le_bytes([block[4], block[5], block[6], block[7]]);
    CRC32_IEEE_TABLE8[7][(x & 0xff) as usize]
        ^ CRC32_IEEE_TABLE8[6][((x >> 8) & 0xff) as usize]
        ^ CRC32_IEEE_TABLE8[5][((x >> 16) & 0xff) as usize]
        ^ CRC32_IEEE_TABLE8[4][(x >> 24) as usize]
        ^ CRC32_IEEE_TABLE8[3][(y & 0xff) as usize]
        ^ CRC32_IEEE_TABLE8[2][((y >> 8) & 0xff) as usize]
        ^ CRC32_IEEE_TABLE8[1][((y >> 16) & 0xff) as usize]
        ^ CRC32_IEEE_TABLE8[0][(y >> 24) as usize]
}

/// IEEE 802.3 CRC-32 of `data`, initialized and finalized with `!0`.
#[inline]
pub fn crc32_ieee(data: &[u8]) -> u32 {
    crc32_ieee_update(0, data)
}

/// Continue an IEEE CRC-32 from its previously finalized value.
///
/// The seed and return value are finalized CRCs: `crc32_ieee_update(0, data)`
/// equals [`crc32_ieee`](data), and updating a CRC with another slice is
/// identical to hashing the concatenation.
#[inline]
pub fn crc32_ieee_update(crc: u32, data: &[u8]) -> u32 {
    let mut state = !crc;
    let (blocks, rest) = data.as_chunks::<8>();
    for block in blocks {
        state = crc32_block8(state, block);
    }
    for &byte in rest {
        state =
            CRC32_IEEE_TABLE8[0][((state ^ u32::from(byte)) & 0xff) as usize] ^ (state >> 8);
    }
    !state
}

/// Canonical FNV-1a 64-bit offset basis.
pub const FNV1A64_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
/// Canonical FNV-1a 64-bit prime.
pub const FNV1A64_PRIME: u64 = 0x0000_0100_0000_01b3;

/// FNV-1a 64-bit hash of a byte slice.
pub const fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut state = FNV1A64_OFFSET_BASIS;
    let mut i = 0;
    while i < bytes.len() {
        state = (state ^ bytes[i] as u64).wrapping_mul(FNV1A64_PRIME);
        i += 1;
    }
    state
}

/// FNV-1a 64-bit hash of a UTF-8 string's bytes.
pub const fn fnv1a64_str(s: &str) -> u64 {
    fnv1a64(s.as_bytes())
}

/// Incremental FNV-1a 64-bit accumulator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fnv1a64(u64);

impl Default for Fnv1a64 {
    fn default() -> Self {
        Self::new()
    }
}

impl Fnv1a64 {
    /// Start from the canonical offset basis.
    pub const fn new() -> Self {
        Self(FNV1A64_OFFSET_BASIS)
    }
    /// Start from an arbitrary running FNV state.
    pub const fn with_seed(seed: u64) -> Self {
        Self(seed)
    }
    /// Add bytes to the running hash.
    pub fn update(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 = (self.0 ^ u64::from(byte)).wrapping_mul(FNV1A64_PRIME);
        }
    }
    /// Add one byte.
    pub fn update_u8(&mut self, value: u8) {
        self.update(&[value]);
    }
    /// Add a little-endian `u16`.
    pub fn update_u16(&mut self, value: u16) {
        self.update(&value.to_le_bytes());
    }
    /// Add a little-endian `u32`.
    pub fn update_u32(&mut self, value: u32) {
        self.update(&value.to_le_bytes());
    }
    /// Add a little-endian `u64`.
    pub fn update_u64(&mut self, value: u64) {
        self.update(&value.to_le_bytes());
    }
    /// Add an `i32` in little-endian two's-complement form.
    pub fn update_i32(&mut self, value: i32) {
        self.update_u32(value as u32);
    }
    /// Add an `i64` in little-endian two's-complement form.
    pub fn update_i64(&mut self, value: i64) {
        self.update_u64(value as u64);
    }
    /// Add the IEEE-754 bit pattern in little-endian order.
    pub fn update_f32_bits(&mut self, value: f32) {
        self.update_u32(value.to_bits());
    }
    /// Add the string's UTF-8 bytes without a length prefix.
    pub fn update_str(&mut self, value: &str) {
        self.update(value.as_bytes());
    }
    /// Consume the accumulator and return its current hash.
    pub const fn finish(self) -> u64 {
        self.0
    }
    /// Return the current hash without consuming the accumulator.
    pub const fn value(&self) -> u64 {
        self.0
    }
}
