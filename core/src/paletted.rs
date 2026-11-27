// SPDX-License-Identifier: LGPL-3.0-or-later
#![forbid(unsafe_code)]
//! `megu_core::paletted` — the one canonical protocol-767 compact section:
//! 4,096 block states in a non-straddling `u64` word layout under the 0-bit
//! uniform, 4-8-bit indirect and 15-bit direct palette encodings vanilla uses.
//!
//! This module is the former `client-kernel/build.rs` splice of the server's
//! `src/paletted.rs`. The kernel used to synthesize a `palette.rs` projection at
//! build time, so the same primitives compiled twice under two visibility
//! regimes and could silently drift. There is one definition now, here, and the
//! kernel consumes it as an ordinary typed dependency.
//!
//! # What this module owns
//!
//! * The representation and every accessor: [`PalettedSection::pack`],
//!   [`PalettedSection::single`], `get`, `set`, `non_air`, `has_hidden`,
//!   `light_class`, `is_uniform`, `uniform_state`, `is_clean_wire`,
//!   `any_palette_entry`, `unpack_into`, `copy_y_slice`, `palette_bits` and
//!   `approx_bytes`.
//! * The block-state container encoder
//!   [`PalettedSection::write_block_states_wire`]: the exact bytes protocol 767
//!   carries for a section after its non-air count.
//! * [`is_air_state`], the pinned air state IDs (`0`, `12_958`, `12_959`).
//!
//! # What stays outside this module, and why
//!
//! * `write_wire` (non-air `i16`, then the container, then the 64-cell
//!   quart-biome palette) stays server-side: its biome half is the server's
//!   `write_palette`, and the biome clock never enters this crate.
//! * `run_paletted_bench` and the worldgen/registry differential tests stay with
//!   the server, which is the only place with `block_registry::breg()` and
//!   `worldgen`. Behaviour pinning for the shared primitives lives in
//!   `core/tests/paletted_tests.rs`; the checked wire decoder keeps its
//!   malformed-input tests in `client-kernel/tests/kernel.rs`.
//!
//! At this revision the server's `src/paletted.rs` still carries its own copy of
//! the moved code; that lane migrates onto this module, at which point
//! `write_wire` and the bench move here as well and the server copy disappears.
//!
//! # Caller-supplied facts
//!
//! The block registry never enters this crate. [`PalettedSection::pack`] and
//! [`PalettedSection::set`] take the two registry predicates they need —
//! `is_opaque` (light opacity 15) and `is_hidden` (the hidden-face mask) —
//! exactly as the server passes them from `BReg`. The kernel snapshot path
//! passes a constant `false` for both because it does not track either fact per
//! section (`client-kernel/src/light.rs` owns light), so the kernel never
//! consults `light_class`/`has_hidden`; those flags exist for the server, and
//! `all_opaque`/`has_hidden` stay inert (`false`) on a kernel-built section.
//!
//! # Arithmetic and allocation contract
//!
//! * Word layout is non-straddling: a word holds `64 / bits` cells and never
//!   splits a cell. All packing and unpacking delegates to [`crate::bits`], the
//!   canonical width-agnostic codec, so the bit math is never duplicated here.
//! * `write_varint`/`varint_len_u64` delegate to [`crate::varint`]. Both agree
//!   with the server's local VarInt loop across the whole non-negative `i32`
//!   range, which is every value a container can carry: at most 256 palette
//!   entries, at most 1,024 packed words and states below `i32::MAX`. A value
//!   above `i32::MAX` would be five bytes from both implementations but would
//!   encode a different (negative) VarInt here; no section path can reach it,
//!   and its `block_states_wire_len` sizing stays exact for every value that
//!   can.
//! * `pack` allocates the palette box and the word box once. Every read path is
//!   copy-free: `get`, `is_uniform`, `uniform_state`, `palette_bits`, `non_air`,
//!   `unpack_into`, `copy_y_slice` and `write_block_states_wire` only read the
//!   existing words in place, and the encoder appends directly into the caller's
//!   `Vec` after one `reserve`. `set` allocates only when the local palette, or
//!   its width, has to grow, and rewrites the existing box otherwise.
//!
//! # Wire invariants
//!
//! A container is the bit width byte, then the local palette (none at 0 and 15
//! bits), then the word count and the words big-endian. `set()` never reclaims a
//! palette entry, so an edited section is no longer byte-identical to a fresh
//! `pack` of its cells; `is_clean_wire` reports exactly that and the caller must
//! repack before writing if it needs canonical bytes. Indirect widths stay in
//! `4..=8`; the 257th palette entry promotes to the 15-bit direct form, the only
//! width where cell values are states rather than palette indices.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SectionLightClass {
    Empty,
    Opaque,
    Mixed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SectionPalette {
    /// 0 bits. Uniform state across all 4096 cells.
    Single(u32),
    /// `bits` in 4..=8, `entries.len() <= (1 << bits)`.
    /// Non-straddling u64 layout: `per = 64 / bits`, `words.len() == 4096usize.div_ceil(per)`.
    Indirect {
        bits: u8,
        entries: Box<[u32]>,
        words: Box<[u64]>,
    },
    /// 15 bits per cell, no local palette: non-straddling (`per = 4`, `1024` words = 8,192 B).
    Direct { words: Box<[u64]> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PalettedSection {
    states: SectionPalette,
    non_air: u16,
    all_opaque: bool,
    has_hidden: bool,
    /// True only while this section's palette is exactly the one `section_encode` would
    /// pick for its cells. `set()` never removes unused palette entries, so an edited
    /// section can no longer be written verbatim; the caller must fall back to the
    /// per-cell encoder in that case. Unmodified sections are byte-identical to
    /// `section_encode` (proved by the server's `run_paletted_bench` over 10,000+
    /// real sections).
    clean_wire: bool,
}

#[inline]
pub const fn is_air_state(st: u32) -> bool {
    matches!(st, 0 | 12958 | 12959)
}

/// Canonical non-straddling accessors: `crate::bits` covers every width
/// `1..=16`, so there is no per-width dispatch to keep in sync — and its
/// 4-bit bulk unpack reaches the SIMD nibble expander automatically.
///
/// Decode the packed cells in `[start, end)` into `dst` (length `end - start`).
/// A word holds `64 / bits` cells and never straddles, so each word emits its
/// cells in one inner pass and the only per-cell work is the shift, mask and
/// `map` lookup - no per-cell `idx / per` division and no per-cell variant
/// dispatch. `end` need not land on a word boundary: the last word emits only
/// the cells in range.
#[inline]
fn decode_cells<F: Fn(u32) -> u32>(
    bits: u8,
    words: &[u64],
    map: F,
    start: usize,
    end: usize,
    dst: &mut [u32],
) {
    let bits = bits as usize;
    let per = 64 / bits;
    let mask = (1u64 << bits) - 1;
    for (w, word) in (start / per..end.div_ceil(per)).map(|w| (w, words[w])) {
        let base = w * per;
        for i in start.saturating_sub(base)..(end - base).min(per) {
            dst[base + i - start] = map(((word >> (i * bits)) & mask) as u32);
        }
    }
}

fn grow_indirect_words(old_bits: u8, old_words: &[u64], new_bits: u8) -> Box<[u64]> {
    let mut indices = [0u16; 4096];
    crate::bits::unpack_to_u16(old_words, old_bits, 4096, &mut indices);
    crate::bits::pack_from_u16(&indices, new_bits).into_boxed_slice()
}

fn promote_indirect8_to_direct(entries: &[u32], old_words: &[u64]) -> Box<[u64]> {
    let mut values = [0u16; 4096];
    crate::bits::unpack_to_u16(old_words, 8, 4096, &mut values);
    for v in &mut values {
        *v = (entries[*v as usize] & 0x7fff) as u16;
    }
    crate::bits::pack_from_u16(&values, 15).into_boxed_slice()
}

fn uniform_words(words: &[u64], bits: u8, val: u64) -> bool {
    let b = bits as usize;
    let per = 64 / b;
    let full_words = 4096 / per;
    let rem = 4096 % per;
    let mut full_pattern = 0u64;
    for i in 0..per {
        full_pattern |= val << (i * b);
    }
    if !words[..full_words].iter().all(|&w| w == full_pattern) {
        return false;
    }
    if rem > 0 {
        let mut tail_pattern = 0u64;
        for i in 0..rem {
            tail_pattern |= val << (i * b);
        }
        let tail_mask = (1u64 << (rem * b)) - 1;
        if (words[full_words] & tail_mask) != tail_pattern {
            return false;
        }
    }
    true
}

#[inline(always)]
fn hash_state(st: u32) -> usize {
    (st.wrapping_mul(0x9E37_79B9) >> 23) as usize & 511
}

impl PalettedSection {
    pub fn pack(
        states: &[u32; 4096],
        is_opaque: &impl Fn(u32) -> bool,
        is_hidden: &impl Fn(u32) -> bool,
    ) -> Self {
        let first = states[0];
        // Compare runs of cells at once so uniform terrain needs one branch per
        // 16 states. For mixed terrain, retain the exact first differing index.
        let uniform = [first; 16];
        let Some(row) = states
            .as_chunks::<16>()
            .0
            .iter()
            .position(|r| *r != uniform)
        else {
            return Self::single(first, is_opaque(first), is_hidden(first));
        };
        let start = row * uniform.len();
        let i = states[start..]
            .iter()
            .position(|&st| st != first)
            .map_or(start, |p| start + p);

        let mut indices = [0u16; 4096];
        let mut entries = [0u32; 256];
        entries[0] = first;
        let mut entries_len = 1usize;
        let mut non_air: u16 = if is_air_state(first) { 0 } else { i as u16 };
        let mut table = [0u64; 512];
        table[hash_state(first)] = (first as u64) | (1u64 << 32);

        let mut prev_st = first;
        let mut prev_idx = 0u16;
        let mut overflow_direct = false;

        for idx in i..4096 {
            let st = states[idx];
            if !is_air_state(st) {
                non_air += 1;
            }
            let pal_idx = if st == prev_st {
                prev_idx
            } else {
                let mut h = hash_state(st);
                let found = loop {
                    let slot = table[h];
                    if slot == 0 {
                        if entries_len == 256 {
                            overflow_direct = true;
                            break 0;
                        }
                        let new_idx = entries_len as u16;
                        entries[entries_len] = st;
                        entries_len += 1;
                        table[h] = (st as u64) | (((new_idx as u64) + 1) << 32);
                        break new_idx;
                    }
                    if (slot as u32) == st {
                        break ((slot >> 32) - 1) as u16;
                    }
                    h = (h + 1) & 511;
                };
                if overflow_direct {
                    break;
                }
                prev_st = st;
                prev_idx = found;
                found
            };
            indices[idx] = pal_idx;
        }

        if !overflow_direct {
            let bits = ((usize::BITS - (entries_len - 1).leading_zeros()) as u8).max(4);
            let words = crate::bits::pack_from_u16(&indices, bits).into_boxed_slice();
            let entries_box = entries[..entries_len].to_vec().into_boxed_slice();
            let mut all_opaque = true;
            let mut has_hidden = false;
            for &st in entries_box.iter() {
                all_opaque &= is_opaque(st);
                has_hidden |= is_hidden(st);
            }
            return Self {
                states: SectionPalette::Indirect {
                    bits,
                    entries: entries_box,
                    words,
                },
                non_air,
                all_opaque,
                has_hidden,
                clean_wire: true,
            };
        }

        let mut values = [0u16; 4096];
        let mut non_air = 0u16;
        let mut all_opaque = true;
        let mut has_hidden = false;
        for (j, &st) in states.iter().enumerate() {
            if !is_air_state(st) {
                non_air += 1;
            }
            all_opaque &= is_opaque(st);
            has_hidden |= is_hidden(st);
            values[j] = (st & 0x7fff) as u16;
        }
        let words = crate::bits::pack_from_u16(&values, 15).into_boxed_slice();
        Self {
            states: SectionPalette::Direct { words },
            non_air,
            all_opaque,
            has_hidden,
            clean_wire: true,
        }
    }

    /// O(1) constructor for a uniform section. No `[u32; 4096]` stack array and no
    /// 4096-cell scan: the all-air / all-stone fast path in `chunk_data_opt` needs only
    /// the palette header to write byte-identical wire.
    pub fn single(st: u32, is_opaque: bool, is_hidden: bool) -> Self {
        Self {
            states: SectionPalette::Single(st),
            non_air: if is_air_state(st) { 0 } else { 4096 },
            all_opaque: is_opaque,
            has_hidden: is_hidden,
            clean_wire: true,
        }
    }

    #[inline]
    pub fn get(&self, idx: usize) -> u32 {
        assert!(idx < 4096, "section cell index {idx} out of bounds");
        match &self.states {
            SectionPalette::Single(st) => *st,
            SectionPalette::Indirect {
                bits,
                entries,
                words,
            } => entries[crate::bits::get(words, *bits, idx).unwrap_or(0) as usize],
            SectionPalette::Direct { words } => crate::bits::get(words, 15, idx).unwrap_or(0),
        }
    }

    pub fn set(
        &mut self,
        idx: usize,
        st: u32,
        is_opaque: &impl Fn(u32) -> bool,
        is_hidden: &impl Fn(u32) -> bool,
    ) {
        assert!(idx < 4096, "section cell index {idx} out of bounds");
        let old = self.get(idx);
        if old == st {
            return;
        }
        // The palette is now a superset of the minimal one `section_encode` would pick:
        // unused entries are never reclaimed. Writing this section verbatim would no
        // longer be byte-identical, so the caller must take the repack path.
        self.clean_wire = false;
        match (is_air_state(old), is_air_state(st)) {
            (true, false) => self.non_air += 1,
            (false, true) => self.non_air -= 1,
            _ => {}
        }
        match &mut self.states {
            SectionPalette::Single(cur) => {
                let cur = *cur;
                let mut words = vec![0u64; crate::bits::long_count(4, 4096)].into_boxed_slice();
                let wrote = crate::bits::set(&mut words, 4, idx, 1);
                debug_assert!(wrote);
                self.states = SectionPalette::Indirect {
                    bits: 4,
                    entries: Box::new([cur, st]),
                    words,
                };
                self.all_opaque &= is_opaque(st);
                self.has_hidden |= is_hidden(st);
                return;
            }
            SectionPalette::Indirect {
                bits,
                entries,
                words,
            } => {
                if let Some(pal_idx) = entries.iter().position(|&e| e == st) {
                    let wrote = crate::bits::set(words, *bits, idx, pal_idx as u32);
                    debug_assert!(wrote);
                } else {
                    let new_idx = entries.len();
                    if new_idx < (1usize << *bits) {
                        let mut next = Vec::with_capacity(new_idx + 1);
                        next.extend_from_slice(entries);
                        next.push(st);
                        *entries = next.into_boxed_slice();
                        let wrote = crate::bits::set(words, *bits, idx, new_idx as u32);
                        debug_assert!(wrote);
                    } else if *bits < 8 {
                        let new_bits = *bits + 1;
                        let mut new_words = grow_indirect_words(*bits, words, new_bits);
                        let mut next = Vec::with_capacity(new_idx + 1);
                        next.extend_from_slice(entries);
                        next.push(st);
                        *entries = next.into_boxed_slice();
                        *bits = new_bits;
                        let wrote = crate::bits::set(&mut new_words, new_bits, idx, new_idx as u32);
                        debug_assert!(wrote);
                        *words = new_words;
                    } else {
                        let mut new_words = promote_indirect8_to_direct(entries, words);
                        let wrote = crate::bits::set(&mut new_words, 15, idx, st & 0x7fff);
                        debug_assert!(wrote);
                        self.states = SectionPalette::Direct { words: new_words };
                    }
                }
            }
            SectionPalette::Direct { words } => {
                let wrote = crate::bits::set(words, 15, idx, st & 0x7fff);
                debug_assert!(wrote);
            }
        }
        if !is_opaque(st) {
            self.all_opaque = false;
        } else if !self.all_opaque && !is_opaque(old) {
            self.all_opaque = self.compute_all_opaque(is_opaque);
        }
        if is_hidden(st) {
            self.has_hidden = true;
        } else if self.has_hidden && is_hidden(old) {
            self.has_hidden = self.compute_has_hidden(is_hidden);
        }
    }

    fn compute_all_opaque(&self, is_opaque: &impl Fn(u32) -> bool) -> bool {
        match &self.states {
            SectionPalette::Single(st) => is_opaque(*st),
            SectionPalette::Indirect {
                bits,
                entries,
                words,
            } => {
                let mut opaque_map = [false; 256];
                let mut any_non_opaque = false;
                for (i, &e) in entries.iter().enumerate() {
                    let op = is_opaque(e);
                    opaque_map[i] = op;
                    any_non_opaque |= !op;
                }
                if !any_non_opaque {
                    return true;
                }
                let mut indices = [0u16; 4096];
                crate::bits::unpack_to_u16(words, *bits, 4096, &mut indices);
                indices.iter().all(|&p| opaque_map[p as usize])
            }
            SectionPalette::Direct { words } => {
                for cell in 0..4096 {
                    if !is_opaque(crate::bits::get(words, 15, cell).unwrap_or(0)) {
                        return false;
                    }
                }
                true
            }
        }
    }

    fn compute_has_hidden(&self, is_hidden: &impl Fn(u32) -> bool) -> bool {
        match &self.states {
            SectionPalette::Single(st) => is_hidden(*st),
            SectionPalette::Indirect {
                bits,
                entries,
                words,
            } => {
                let mut hidden_map = [false; 256];
                let mut any_hidden = false;
                for (i, &e) in entries.iter().enumerate() {
                    let h = is_hidden(e);
                    hidden_map[i] = h;
                    any_hidden |= h;
                }
                if !any_hidden {
                    return false;
                }
                let mut indices = [0u16; 4096];
                crate::bits::unpack_to_u16(words, *bits, 4096, &mut indices);
                indices.iter().any(|&p| hidden_map[p as usize])
            }
            SectionPalette::Direct { words } => {
                for cell in 0..4096 {
                    if is_hidden(crate::bits::get(words, 15, cell).unwrap_or(0)) {
                        return true;
                    }
                }
                false
            }
        }
    }

    #[inline]
    pub fn non_air(&self) -> u16 {
        self.non_air
    }

    #[inline]
    pub fn has_hidden(&self) -> bool {
        self.has_hidden
    }

    #[inline]
    pub fn light_class(&self) -> SectionLightClass {
        if self.non_air == 0 {
            SectionLightClass::Empty
        } else if self.all_opaque {
            SectionLightClass::Opaque
        } else {
            SectionLightClass::Mixed
        }
    }

    pub fn is_uniform(&self) -> Option<u32> {
        match &self.states {
            SectionPalette::Single(st) => Some(*st),
            SectionPalette::Indirect {
                bits,
                entries,
                words,
            } => {
                let p0 = crate::bits::get(words, *bits, 0).unwrap_or(0) as usize;
                if uniform_words(words, *bits, p0 as u64) {
                    Some(entries[p0])
                } else {
                    None
                }
            }
            SectionPalette::Direct { words } => {
                let first = crate::bits::get(words, 15, 0).unwrap_or(0) as u64;
                if uniform_words(words, 15, first) {
                    Some(first as u32)
                } else {
                    None
                }
            }
        }
    }

    /// O(1) uniform-section query: `Some(state)` when the section is `Single(state)`
    /// (all 4,096 cells share one state). Unlike `is_uniform`, never scans the packed
    /// words of an `Indirect`/`Direct` section — the band and light fast paths match
    /// only on the `Single` variant, so they cannot pay a scan per section.
    #[inline]
    pub fn uniform_state(&self) -> Option<u32> {
        match &self.states {
            SectionPalette::Single(st) => Some(*st),
            SectionPalette::Indirect { .. } | SectionPalette::Direct { .. } => None,
        }
    }

    /// Whether the container bytes still match exactly what `section_encode` would
    /// produce for this section's cells, so a caller may write them verbatim. Only
    /// `set()` clears it; every constructor starts clean.
    #[inline]
    pub fn is_clean_wire(&self) -> bool {
        self.clean_wire
    }

    /// Test the section's *palette* instead of scanning all 4096 cells. `Single`/`Indirect`
    /// enumerate their distinct states; `Direct` has no local palette, so it answers
    /// conservatively `true` and the caller must scan the cells. This is what lets the
    /// delivery path skip a full-cell luminance scan in sections with no emitters.
    pub fn any_palette_entry(&self, mut pred: impl FnMut(u32) -> bool) -> bool {
        match &self.states {
            SectionPalette::Single(st) => pred(*st),
            SectionPalette::Indirect { entries, .. } => entries.iter().any(|&st| pred(st)),
            SectionPalette::Direct { .. } => true,
        }
    }

    /// Expand this section into the caller's stack array, with one fast path per
    /// representation. Used by the delivery fallback, which has to feed the legacy
    /// per-cell encoder an ordinary `[u32; 4096]`.
    pub fn unpack_into(&self, out: &mut [u32; 4096]) {
        match &self.states {
            SectionPalette::Single(st) => out.fill(*st),
            SectionPalette::Indirect {
                bits,
                entries,
                words,
            } => {
                if *bits == 4 {
                    // The canonical unpack's 4-bit arm reaches the SIMD
                    // nibble expander; one palette pass maps indices to
                    // states afterwards.
                    let mut indices = [0u16; 4096];
                    crate::bits::unpack_to_u16(words, 4, 4096, &mut indices);
                    for (cell, &idx) in out.iter_mut().zip(indices.iter()) {
                        *cell = entries[idx as usize];
                    }
                } else {
                    decode_cells(*bits, words, |i| entries[i as usize], 0, 4096, out);
                }
            }
            SectionPalette::Direct { words } => {
                decode_cells(15, words, |i| i, 0, 4096, out);
            }
        }
    }

    /// Copy `ly_count` contiguous y-rows (`[ly0, ly0 + ly_count)`) into `dst`, whose
    /// length must be at least `ly_count * 256`. This is the `install_band_rows` shape:
    /// it copies only the rows a band overlaps, and a uniform section is a single
    /// `fill` rather than a 4096-cell unpack.
    pub fn copy_y_slice(&self, ly0: usize, ly_count: usize, dst: &mut [u32]) {
        let need = ly_count * 256;
        assert!(dst.len() >= need, "copy_y_slice destination too small");
        if ly_count == 0 {
            return;
        }
        if let Some(st) = self.is_uniform() {
            dst[..need].fill(st);
            return;
        }
        // Hoist the per-cell `self.get` variant match out of the row walk: a
        // non-uniform section is `Indirect` or `Direct`, and a row is a
        // contiguous packed range, so each row is one `decode_cells` call.
        match &self.states {
            SectionPalette::Single(st) => dst[..need].fill(*st),
            SectionPalette::Indirect {
                bits,
                entries,
                words,
            } => {
                for ly in ly0..ly0 + ly_count {
                    let row = (ly - ly0) * 256;
                    if *bits == 4 {
                        // A row is 16 aligned words at 4 bits, so each row
                        // unpacks through the canonical SIMD nibble path
                        // before the palette map.
                        let mut indices = [0u16; 256];
                        crate::bits::unpack_to_u16(
                            &words[ly * 16..ly * 16 + 16],
                            4,
                            256,
                            &mut indices,
                        );
                        for (cell, &idx) in dst[row..row + 256].iter_mut().zip(indices.iter()) {
                            *cell = entries[idx as usize];
                        }
                    } else {
                        decode_cells(
                            *bits,
                            words,
                            |i| entries[i as usize],
                            ly << 8,
                            (ly << 8) + 256,
                            &mut dst[row..row + 256],
                        );
                    }
                }
            }
            SectionPalette::Direct { words } => {
                for ly in ly0..ly0 + ly_count {
                    let row = (ly - ly0) * 256;
                    decode_cells(
                        15,
                        words,
                        |i| i,
                        ly << 8,
                        (ly << 8) + 256,
                        &mut dst[row..row + 256],
                    );
                }
            }
        }
    }

    #[inline]
    pub fn palette_bits(&self) -> u8 {
        match &self.states {
            SectionPalette::Single(_) => 0,
            SectionPalette::Indirect { bits, .. } => *bits,
            SectionPalette::Direct { .. } => 15,
        }
    }

    #[inline]
    pub fn approx_bytes(&self) -> usize {
        let heap = match &self.states {
            SectionPalette::Single(_) => 0,
            SectionPalette::Indirect { entries, words, .. } => {
                std::mem::size_of_val(&**entries) + std::mem::size_of_val(&**words)
            }
            SectionPalette::Direct { words } => std::mem::size_of_val(&**words),
        };
        std::mem::size_of::<Self>() + heap
    }

    /// Exact byte count `write_block_states_wire` appends. Used only to size a
    /// single `reserve` — capacity sizing, never output bytes.
    fn block_states_wire_len(&self) -> usize {
        match &self.states {
            SectionPalette::Single(st) => 1 + varint_len_u64(*st as u64) + 1,
            SectionPalette::Indirect { entries, words, .. } => {
                1 + varint_len_u64(entries.len() as u64)
                    + entries
                        .iter()
                        .map(|&e| varint_len_u64(e as u64))
                        .sum::<usize>()
                    + varint_len_u64(words.len() as u64)
                    + words.len() * 8
            }
            SectionPalette::Direct { words } => {
                1 + varint_len_u64(words.len() as u64) + words.len() * 8
            }
        }
    }

    pub fn write_block_states_wire(&self, out: &mut Vec<u8>) {
        out.reserve(self.block_states_wire_len());
        match &self.states {
            SectionPalette::Single(st) => {
                out.push(0);
                write_varint(out, *st as i64);
                write_varint(out, 0);
            }
            SectionPalette::Indirect {
                bits,
                entries,
                words,
            } => {
                out.push(*bits);
                write_varint(out, entries.len() as i64);
                for &entry in entries.iter() {
                    write_varint(out, entry as i64);
                }
                write_varint(out, words.len() as i64);
                append_be_words(out, words);
            }
            SectionPalette::Direct { words } => {
                out.push(15);
                write_varint(out, words.len() as i64);
                append_be_words(out, words);
            }
        }
    }
}

/// Append a protocol VarInt for a section header field.
fn write_varint(out: &mut Vec<u8>, value: i64) {
    crate::varint::write_varint_i32(value as i32, out);
}

/// VarInt byte count for a non-negative value — capacity sizing only.
#[inline]
fn varint_len_u64(value: u64) -> usize {
    crate::varint::varint_i32_len(value as i32)
}

/// Append `words` big-endian, staging 256 words per extend: a Direct section
/// is 1,024 words and used to pay one bounds-checked 8-byte `extend_from_slice`
/// per word — now four.
fn append_be_words(out: &mut Vec<u8>, words: &[u64]) {
    const WORDS_PER_CHUNK: usize = 256;
    out.reserve(words.len() * 8);
    let mut buf = [0u8; WORDS_PER_CHUNK * 8];
    for chunk in words.chunks(WORDS_PER_CHUNK) {
        for (slot, &word) in chunk.iter().enumerate() {
            buf[slot * 8..slot * 8 + 8].copy_from_slice(&word.to_be_bytes());
        }
        out.extend_from_slice(&buf[..chunk.len() * 8]);
    }
}
