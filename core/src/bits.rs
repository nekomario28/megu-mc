// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Minecraft packed bit storage and nibble light arrays.

/// Number of light bytes per section.
pub const LIGHT_BYTES: usize = 2048;
/// Number of packed light nibbles per section.
pub const LIGHT_NIBBLES: usize = 4096;

/// Number of entries in a non-straddling 64-bit storage word.
pub const fn entries_per_long(bits_per_entry: u8) -> usize {
    if bits_per_entry == 0 || bits_per_entry > 16 {
        0
    } else {
        64 / bits_per_entry as usize
    }
}

/// Number of words needed to store `len` values.
pub const fn long_count(bits_per_entry: u8, len: usize) -> usize {
    let per = entries_per_long(bits_per_entry);
    if per == 0 {
        0
    } else {
        len.div_ceil(per)
    }
}

/// Get one packed value, returning `None` for invalid widths, indices or data.
pub fn get(data: &[u64], bits_per_entry: u8, index: usize) -> Option<u32> {
    let per = entries_per_long(bits_per_entry);
    if per == 0 {
        return None;
    }
    let word = *data.get(index / per)?;
    let shift = (index % per) * usize::from(bits_per_entry);
    Some(((word >> shift) & ((1u64 << bits_per_entry) - 1)) as u32)
}

/// Set one packed value. Returns false for invalid inputs or a value too wide.
pub fn set(data: &mut [u64], bits_per_entry: u8, index: usize, value: u32) -> bool {
    let per = entries_per_long(bits_per_entry);
    if per == 0 || u64::from(value) >= (1u64 << bits_per_entry) {
        return false;
    }
    let Some(word) = data.get_mut(index / per) else {
        return false;
    };
    let shift = (index % per) * usize::from(bits_per_entry);
    let mask = (1u64 << bits_per_entry) - 1;
    *word = (*word & !(mask << shift)) | (u64::from(value) << shift);
    true
}

/// Pack values in Minecraft's non-straddling word layout.
pub fn pack_from_u16(values: &[u16], bits_per_entry: u8) -> Vec<u64> {
    let per = entries_per_long(bits_per_entry);
    if per == 0 {
        return Vec::new();
    }
    let mut out = vec![0; long_count(bits_per_entry, values.len())];
    for (index, &value) in values.iter().enumerate() {
        let _ = set(&mut out, bits_per_entry, index, u32::from(value));
    }
    out
}

/// Unpack as many values as fit in `out`; invalid widths leave it unchanged.
pub fn unpack_to_u16(data: &[u64], bits_per_entry: u8, len: usize, out: &mut [u16]) {
    let per = entries_per_long(bits_per_entry);
    if per == 0 {
        return;
    }
    let count = len.min(out.len()).min(data.len().saturating_mul(per));
    if bits_per_entry == 4 && count >= 16 {
        let whole = count / 16 * 16;
        const PALETTE: [u16; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
        crate::simd_voxel::unpack_palette_4bit_to_u16(
            &data[..whole / 16],
            &PALETTE,
            &mut out[..whole],
        );
        for (i, slot) in out.iter_mut().enumerate().take(count).skip(whole) {
            *slot = get(data, bits_per_entry, i).unwrap_or(0) as u16;
        }
        return;
    }
    for (i, slot) in out.iter_mut().take(count).enumerate() {
        *slot = get(data, bits_per_entry, i).unwrap_or(0) as u16;
    }
}

/// Index in a section's x-fastest, y-major nibble layout.
pub const fn section_index(x: usize, y: usize, z: usize) -> usize {
    (y << 8) | (z << 4) | x
}

/// Packed block/sky light values, low nibble first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LightArray([u8; LIGHT_BYTES]);

impl LightArray {
    /// A zero-filled light array.
    pub const fn new_zero() -> Self {
        Self([0; LIGHT_BYTES])
    }
    /// Wrap exactly 2048 bytes.
    pub const fn from_bytes(bytes: [u8; LIGHT_BYTES]) -> Self {
        Self(bytes)
    }
    /// Borrow the packed bytes.
    pub const fn as_bytes(&self) -> &[u8; LIGHT_BYTES] {
        &self.0
    }
    /// Mutably borrow the packed bytes.
    pub fn as_bytes_mut(&mut self) -> &mut [u8; LIGHT_BYTES] {
        &mut self.0
    }
    /// Read a value from section coordinates; coordinates wrap to nibbles.
    pub fn get(&self, x: usize, y: usize, z: usize) -> u8 {
        self.get_idx(section_index(x, y, z))
    }
    /// Write a value from section coordinates, truncating to four bits.
    pub fn set(&mut self, x: usize, y: usize, z: usize, value: u8) {
        self.set_idx(section_index(x, y, z), value);
    }
    /// Read one nibble by linear index; out-of-range indices return zero.
    pub fn get_idx(&self, index: usize) -> u8 {
        if index >= LIGHT_NIBBLES {
            return 0;
        }
        let byte = self.0[index / 2];
        if index & 1 == 0 {
            byte & 0x0f
        } else {
            byte >> 4
        }
    }
    /// Set one nibble by linear index; out-of-range indices are ignored.
    pub fn set_idx(&mut self, index: usize, value: u8) {
        if index >= LIGHT_NIBBLES {
            return;
        }
        let byte = &mut self.0[index / 2];
        let value = value & 0x0f;
        if index & 1 == 0 {
            *byte = (*byte & 0xf0) | value;
        } else {
            *byte = (*byte & 0x0f) | (value << 4);
        }
    }
    /// Merge by taking the maximum of each corresponding nibble.
    pub fn max_merge_from(&mut self, other: &Self) {
        let caps = crate::hw_adaptive_engine::detect_host_hardware_caps();
        let strategy = crate::hw_adaptive_engine::select_compute_strategy(&caps);
        self.0 =
            crate::hw_adaptive_engine::max_nibble_arrays_adaptive(&self.0, &other.0, &strategy);
    }
    /// Whether every packed byte is zero.
    pub fn is_all_zero(&self) -> bool {
        self.0.iter().all(|&byte| byte == 0)
    }
}

impl Default for LightArray {
    fn default() -> Self {
        Self::new_zero()
    }
}
