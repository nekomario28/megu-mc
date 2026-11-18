// Clean-room compact paletted section representation (M3 Slice P2a).
// Non-straddling u64 packing matching protocol 767 `write_palette` (`src/main.rs`).

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SectionLightClass {
    Empty,
    Opaque,
    Mixed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SectionPalette {
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
    Direct {
        words: Box<[u64]>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PalettedSection {
    states: SectionPalette,
    non_air: u16,
    all_opaque: bool,
    has_hidden: bool,
}

#[inline]
pub(crate) const fn is_air_state(st: u32) -> bool {
    matches!(st, 0 | 12958 | 12959)
}

#[inline]
const fn words_for_bits(bits: u8) -> usize {
    match bits {
        4 => 256,
        5 => 342,
        6 => 410,
        7 => 456,
        8 => 512,
        15 => 1024,
        _ => unreachable!(),
    }
}

#[inline(always)]
fn packed_get<const BITS: usize>(words: &[u64], idx: usize) -> u32 {
    let per = const { 64 / BITS };
    let word_idx = idx / per;
    let slot = idx % per;
    let mask = const { (1u64 << BITS) - 1 };
    ((words[word_idx] >> (slot * BITS)) & mask) as u32
}

#[inline(always)]
fn packed_set<const BITS: usize>(words: &mut [u64], idx: usize, val: u64) {
    let per = const { 64 / BITS };
    let word_idx = idx / per;
    let slot = idx % per;
    let shift = slot * BITS;
    let mask = const { (1u64 << BITS) - 1 };
    words[word_idx] = (words[word_idx] & !(mask << shift)) | ((val & mask) << shift);
}

#[inline]
fn indirect_get_word(bits: u8, words: &[u64], idx: usize) -> usize {
    match bits {
        4 => packed_get::<4>(words, idx) as usize,
        5 => packed_get::<5>(words, idx) as usize,
        6 => packed_get::<6>(words, idx) as usize,
        7 => packed_get::<7>(words, idx) as usize,
        8 => packed_get::<8>(words, idx) as usize,
        other => unreachable!("invalid indirect palette bits {other}"),
    }
}

#[inline]
fn indirect_set_word(bits: u8, words: &mut [u64], idx: usize, val: u64) {
    match bits {
        4 => packed_set::<4>(words, idx, val),
        5 => packed_set::<5>(words, idx, val),
        6 => packed_set::<6>(words, idx, val),
        7 => packed_set::<7>(words, idx, val),
        8 => packed_set::<8>(words, idx, val),
        other => unreachable!("invalid indirect palette bits {other}"),
    }
}

fn pack_indices_const<const BITS: usize>(indices: &[u8; 4096]) -> Box<[u64]> {
    let per = const { 64 / BITS };
    let word_count = const { 4096usize.div_ceil(64 / BITS) };
    let mut words = vec![0u64; word_count].into_boxed_slice();
    for (w, chunk) in indices.chunks(per).enumerate() {
        let mut word = 0u64;
        for (i, &idx) in chunk.iter().enumerate() {
            word |= (idx as u64) << (i * BITS);
        }
        words[w] = word;
    }
    words
}

fn unpack_all_const<const BITS: usize>(words: &[u64], out: &mut [u8; 4096]) {
    let per = const { 64 / BITS };
    let mask = const { (1u64 << BITS) - 1 };
    for (w, chunk) in out.chunks_mut(per).enumerate() {
        let mut word = words[w];
        for dst in chunk.iter_mut() {
            *dst = (word & mask) as u8;
            word >>= BITS;
        }
    }
}

fn grow_indirect_words(old_bits: u8, old_words: &[u64], new_bits: u8) -> Box<[u64]> {
    let mut indices = [0u8; 4096];
    match old_bits {
        4 => unpack_all_const::<4>(old_words, &mut indices),
        5 => unpack_all_const::<5>(old_words, &mut indices),
        6 => unpack_all_const::<6>(old_words, &mut indices),
        7 => unpack_all_const::<7>(old_words, &mut indices),
        other => unreachable!("cannot grow from indirect bits {other}"),
    }
    match new_bits {
        5 => pack_indices_const::<5>(&indices),
        6 => pack_indices_const::<6>(&indices),
        7 => pack_indices_const::<7>(&indices),
        8 => pack_indices_const::<8>(&indices),
        other => unreachable!("cannot grow to indirect bits {other}"),
    }
}

fn promote_indirect8_to_direct(entries: &[u32], old_words: &[u64]) -> Box<[u64]> {
    let mut indices = [0u8; 4096];
    unpack_all_const::<8>(old_words, &mut indices);
    let mut words = vec![0u64; 1024].into_boxed_slice();
    for (w, chunk) in indices.chunks_exact(4).enumerate() {
        let mut word = 0u64;
        for (j, &idx) in chunk.iter().enumerate() {
            word |= ((entries[idx as usize] as u64) & 0x7fff) << (j * 15);
        }
        words[w] = word;
    }
    words
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
    pub(crate) fn pack(
        states: &[u32; 4096],
        is_opaque: &impl Fn(u32) -> bool,
        is_hidden: &impl Fn(u32) -> bool,
    ) -> Self {
        let first = states[0];
        let mut i = 1usize;
        while i < 4096 && states[i] == first {
            i += 1;
        }
        if i == 4096 {
            return Self {
                states: SectionPalette::Single(first),
                non_air: if is_air_state(first) { 0 } else { 4096 },
                all_opaque: is_opaque(first),
                has_hidden: is_hidden(first),
            };
        }

        let mut indices = [0u8; 4096];
        let mut entries = [0u32; 256];
        entries[0] = first;
        let mut entries_len = 1usize;
        let mut non_air: u16 = if is_air_state(first) { 0 } else { i as u16 };
        let mut table = [0u64; 512];
        table[hash_state(first)] = (first as u64) | (1u64 << 32);

        let mut prev_st = first;
        let mut prev_idx = 0u8;
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
                        let new_idx = entries_len as u8;
                        entries[entries_len] = st;
                        entries_len += 1;
                        table[h] = (st as u64) | (((new_idx as u64) + 1) << 32);
                        break new_idx;
                    }
                    if (slot as u32) == st {
                        break ((slot >> 32) - 1) as u8;
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
            let words = match bits {
                4 => pack_indices_const::<4>(&indices),
                5 => pack_indices_const::<5>(&indices),
                6 => pack_indices_const::<6>(&indices),
                7 => pack_indices_const::<7>(&indices),
                8 => pack_indices_const::<8>(&indices),
                _ => unreachable!(),
            };
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
            };
        }

        let mut words = vec![0u64; 1024].into_boxed_slice();
        let mut non_air = 0u16;
        let mut all_opaque = true;
        let mut has_hidden = false;
        for (w, chunk) in states.chunks_exact(4).enumerate() {
            let mut word = 0u64;
            for (j, &st) in chunk.iter().enumerate() {
                if !is_air_state(st) {
                    non_air += 1;
                }
                all_opaque &= is_opaque(st);
                has_hidden |= is_hidden(st);
                word |= ((st as u64) & 0x7fff) << (j * 15);
            }
            words[w] = word;
        }
        Self {
            states: SectionPalette::Direct { words },
            non_air,
            all_opaque,
            has_hidden,
        }
    }

    #[inline]
    pub(crate) fn get(&self, idx: usize) -> u32 {
        assert!(idx < 4096, "section cell index {idx} out of bounds");
        match &self.states {
            SectionPalette::Single(st) => *st,
            SectionPalette::Indirect {
                bits,
                entries,
                words,
            } => entries[indirect_get_word(*bits, words, idx)],
            SectionPalette::Direct { words } => packed_get::<15>(words, idx),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn set(
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
        match (is_air_state(old), is_air_state(st)) {
            (true, false) => self.non_air += 1,
            (false, true) => self.non_air -= 1,
            _ => {}
        }
        match &mut self.states {
            SectionPalette::Single(cur) => {
                let cur = *cur;
                let mut words = vec![0u64; words_for_bits(4)].into_boxed_slice();
                packed_set::<4>(&mut words, idx, 1);
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
                    indirect_set_word(*bits, words, idx, pal_idx as u64);
                } else {
                    let new_idx = entries.len();
                    if new_idx < (1usize << *bits) {
                        let mut next = Vec::with_capacity(new_idx + 1);
                        next.extend_from_slice(entries);
                        next.push(st);
                        *entries = next.into_boxed_slice();
                        indirect_set_word(*bits, words, idx, new_idx as u64);
                    } else if *bits < 8 {
                        let new_bits = *bits + 1;
                        let mut new_words = grow_indirect_words(*bits, words, new_bits);
                        let mut next = Vec::with_capacity(new_idx + 1);
                        next.extend_from_slice(entries);
                        next.push(st);
                        *entries = next.into_boxed_slice();
                        *bits = new_bits;
                        indirect_set_word(new_bits, &mut new_words, idx, new_idx as u64);
                        *words = new_words;
                    } else {
                        let mut new_words = promote_indirect8_to_direct(entries, words);
                        packed_set::<15>(&mut new_words, idx, st as u64);
                        self.states = SectionPalette::Direct { words: new_words };
                    }
                }
            }
            SectionPalette::Direct { words } => {
                packed_set::<15>(words, idx, st as u64);
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

    #[allow(dead_code)]
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
                let mut indices = [0u8; 4096];
                match *bits {
                    4 => unpack_all_const::<4>(words, &mut indices),
                    5 => unpack_all_const::<5>(words, &mut indices),
                    6 => unpack_all_const::<6>(words, &mut indices),
                    7 => unpack_all_const::<7>(words, &mut indices),
                    8 => unpack_all_const::<8>(words, &mut indices),
                    other => unreachable!("invalid indirect palette bits {other}"),
                }
                indices.iter().all(|&p| opaque_map[p as usize])
            }
            SectionPalette::Direct { words } => {
                for cell in 0..4096 {
                    if !is_opaque(packed_get::<15>(words, cell)) {
                        return false;
                    }
                }
                true
            }
        }
    }

    #[allow(dead_code)]
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
                let mut indices = [0u8; 4096];
                match *bits {
                    4 => unpack_all_const::<4>(words, &mut indices),
                    5 => unpack_all_const::<5>(words, &mut indices),
                    6 => unpack_all_const::<6>(words, &mut indices),
                    7 => unpack_all_const::<7>(words, &mut indices),
                    8 => unpack_all_const::<8>(words, &mut indices),
                    other => unreachable!("invalid indirect palette bits {other}"),
                }
                indices.iter().any(|&p| hidden_map[p as usize])
            }
            SectionPalette::Direct { words } => {
                for cell in 0..4096 {
                    if is_hidden(packed_get::<15>(words, cell)) {
                        return true;
                    }
                }
                false
            }
        }
    }

    #[inline]
    pub(crate) fn non_air(&self) -> u16 {
        self.non_air
    }

    #[allow(dead_code)]
    #[inline]
    pub(crate) fn has_hidden(&self) -> bool {
        self.has_hidden
    }

    #[allow(dead_code)]
    #[inline]
    pub(crate) fn light_class(&self) -> SectionLightClass {
        if self.non_air == 0 {
            SectionLightClass::Empty
        } else if self.all_opaque {
            SectionLightClass::Opaque
        } else {
            SectionLightClass::Mixed
        }
    }

    #[allow(dead_code)]
    pub(crate) fn is_uniform(&self) -> Option<u32> {
        match &self.states {
            SectionPalette::Single(st) => Some(*st),
            SectionPalette::Indirect {
                bits,
                entries,
                words,
            } => {
                let p0 = indirect_get_word(*bits, words, 0);
                if uniform_words(words, *bits, p0 as u64) {
                    Some(entries[p0])
                } else {
                    None
                }
            }
            SectionPalette::Direct { words } => {
                let first = packed_get::<15>(words, 0) as u64;
                if uniform_words(words, 15, first) {
                    Some(first as u32)
                } else {
                    None
                }
            }
        }
    }

    #[inline]
    pub(crate) fn palette_bits(&self) -> u8 {
        match &self.states {
            SectionPalette::Single(_) => 0,
            SectionPalette::Indirect { bits, .. } => *bits,
            SectionPalette::Direct { .. } => 15,
        }
    }

    #[inline]
    pub(crate) fn approx_bytes(&self) -> usize {
        let heap = match &self.states {
            SectionPalette::Single(_) => 0,
            SectionPalette::Indirect { entries, words, .. } => {
                std::mem::size_of_val(&**entries) + std::mem::size_of_val(&**words)
            }
            SectionPalette::Direct { words } => std::mem::size_of_val(&**words),
        };
        std::mem::size_of::<Self>() + heap
    }

    pub(crate) fn write_block_states_wire(&self, out: &mut Vec<u8>) {
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
                for &word in words.iter() {
                    out.extend_from_slice(&word.to_be_bytes());
                }
            }
            SectionPalette::Direct { words } => {
                out.push(15);
                write_varint(out, words.len() as i64);
                for &word in words.iter() {
                    out.extend_from_slice(&word.to_be_bytes());
                }
            }
        }
    }

    pub(crate) fn write_wire(&self, biomes: &[i64; 64], out: &mut Vec<u8>) {
        out.extend_from_slice(&(self.non_air as i16).to_be_bytes());
        self.write_block_states_wire(out);
        write_palette(out, biomes, 1, 3, 6);
    }
}

pub(crate) fn run_paletted_bench() {
    let reg = crate::block_registry::breg().expect("block registry");
    let hidden = hidden_states();
    let is_opaque = |st: u32| reg.opacity_of(st) == 15;
    let is_hid = |st: u32| hidden.contains(&st);
    let biomes: [i64; 64] = std::array::from_fn(|i| (i % 11) as i64);

    let specs = [
        ("overworld", worldgen::NoisePreset::Overworld, 18i32, 24usize), // 324 * 24 = 7,776
        ("nether", worldgen::NoisePreset::Nether, 10i32, 16usize),       // 100 * 16 = 1,600
        ("end", worldgen::NoisePreset::End, 10i32, 16usize),             // 100 * 16 = 1,600
    ];

    let mut sections_raw: Vec<Box<[u32; 4096]>> = Vec::with_capacity(11_000);
    let mut ow_bytes = 0usize;
    let mut ow_sections = 0usize;
    let mut total_bytes = 0usize;
    let mut hist = [0usize; 16]; // indexed by palette_bits (0,4,5,6,7,8,15)
    let mut wire = Vec::with_capacity(16384);

    for (label, preset, side, expected_secs) in specs {
        let mut gen = worldgen::Gen::load_preset(4242, crate::name_to_state, preset)
            .expect("load worldgen preset");
        let lo = gen.data.world_min_y;
        let hi = lo + gen.data.world_height - 1;
        let mut count = 0usize;
        for cz in 0..side {
            for cx in 0..side {
                let mut sy = lo;
                while sy <= hi {
                    let arr = gen.terrain_section_at(cx, sy, cz, crate::feature::populate);
                    let sec = PalettedSection::pack(&arr, &is_opaque, &is_hid);
                    for i in 0..4096 {
                        assert_eq!(sec.get(i), arr[i], "{label} ({cx},{cz}) sy={sy} cell={i}");
                    }
                    wire.clear();
                    sec.write_wire(&biomes, &mut wire);
                    let ref_wire = section_encode(&arr, &biomes);
                    assert_eq!(wire, ref_wire, "{label} ({cx},{cz}) sy={sy} wire mismatch");

                    let b = sec.approx_bytes();
                    total_bytes += b;
                    if preset == worldgen::NoisePreset::Overworld {
                        ow_bytes += b;
                        ow_sections += 1;
                    }
                    hist[sec.palette_bits() as usize] += 1;
                    sections_raw.push(arr);
                    count += 1;
                    sy += 16;
                }
            }
        }
        assert_eq!(count, (side * side) as usize * expected_secs);
    }

    let total = sections_raw.len();
    assert!(total >= 10_000, "expected >= 10,000 sections, got {total}");

    // Pure pack throughput benchmark across all collected real sections (10 rounds).
    let rounds = 10usize;
    let t0 = Instant::now();
    let mut sink = 0u64;
    for _ in 0..rounds {
        for arr in &sections_raw {
            let sec = PalettedSection::pack(arr, &is_opaque, &is_hid);
            sink = sink.wrapping_add(sec.non_air() as u64 + sec.palette_bits() as u64);
        }
    }
    let elapsed_ns = t0.elapsed().as_secs_f64() * 1e9;
    std::hint::black_box(sink);
    let pack_ns_per_sec = elapsed_ns / (total * rounds) as f64;

    println!("sections_total={total}");
    println!("wire_identity_mismatches=0");
    println!("cell_get_mismatches=0");
    println!(
        "histogram_bits single={} b4={} b5={} b6={} b7={} b8={} direct={}",
        hist[0], hist[4], hist[5], hist[6], hist[7], hist[8], hist[15]
    );
    println!(
        "mean_bytes_per_section={:.1} mean_kib_per_overworld_chunk={:.2} overworld_sections={ow_sections}",
        total_bytes as f64 / total as f64,
        (ow_bytes as f64 / (ow_sections as f64 / 24.0)) / 1024.0
    );
    println!("pack_ns_per_section={pack_ns_per_sec:.1} rounds={rounds}");
}

#[cfg(test)]
mod paletted_tests {
    use super::*;

    fn decode_wire_section(mut wire: &[u8]) -> (i16, [u32; 4096], [i64; 64]) {
        assert!(wire.len() >= 2, "section wire too short");
        let non_air = i16::from_be_bytes([wire[0], wire[1]]);
        wire = &wire[2..];

        let decode_palette = |buf: &mut &[u8], len: usize, max_bits: u8| -> Vec<i64> {
            let bits = buf[0];
            *buf = &buf[1..];
            if bits == 0 {
                let val = read_varint(buf).expect("single palette value");
                let word_count = read_varint(buf).expect("single word count");
                assert_eq!(word_count, 0);
                return vec![val; len];
            }
            let palette = if bits <= max_bits {
                let pal_len = read_varint(buf).expect("palette len") as usize;
                let mut p = Vec::with_capacity(pal_len);
                for _ in 0..pal_len {
                    p.push(read_varint(buf).expect("palette entry"));
                }
                Some(p)
            } else {
                None
            };
            let word_count = read_varint(buf).expect("word count") as usize;
            let per = 64 / bits as usize;
            assert_eq!(word_count, len.div_ceil(per));
            let mask = (1u64 << bits) - 1;
            let mut out = Vec::with_capacity(len);
            for _ in 0..word_count {
                let word = u64::from_be_bytes(buf[..8].try_into().expect("word bytes"));
                *buf = &buf[8..];
                for slot in 0..per {
                    if out.len() == len {
                        break;
                    }
                    let raw = ((word >> (slot * bits as usize)) & mask) as usize;
                    let v = match &palette {
                        Some(p) => p[raw],
                        None => raw as i64,
                    };
                    out.push(v);
                }
            }
            out
        };

        let states_vec = decode_palette(&mut wire, 4096, 8);
        let biomes_vec = decode_palette(&mut wire, 64, 3);
        assert!(wire.is_empty(), "trailing bytes in decoded section: {}", wire.len());
        let states: [u32; 4096] = states_vec
            .into_iter()
            .map(|v| v as u32)
            .collect::<Vec<_>>()
            .try_into()
            .unwrap();
        let biomes: [i64; 64] = biomes_vec.try_into().unwrap();
        (non_air, states, biomes)
    }

    fn test_opaque(st: u32) -> bool {
        st != 0 && st != 12958 && st != 12959 && st % 10 != 7
    }

    fn test_hidden(st: u32) -> bool {
        st == 50 || st == 20_000
    }

    #[test]
    fn paletted_exact_bit_transitions_and_wire_identity() {
        let biomes: [i64; 64] = std::array::from_fn(|i| (i % 7) as i64);
        let cases: &[(usize, u8, u32)] = &[
            (1, 0, 1),
            (2, 4, 1),
            (16, 4, 1),
            (17, 5, 1),
            (32, 5, 1),
            (33, 6, 1),
            (64, 6, 1),
            (65, 7, 1),
            (128, 7, 1),
            (129, 8, 1),
            (256, 8, 1),
            (257, 15, 16_384),
            (512, 15, 20_000),
            (4096, 15, 22_000),
        ];

        for &(distinct, expected_bits, base_offset) in cases {
            let mut states = [0u32; 4096];
            for i in 0..4096 {
                let k = i % distinct;
                states[i] = if k == 0 && distinct > 1 {
                    0
                } else {
                    base_offset + k as u32
                };
            }
            let sec = PalettedSection::pack(&states, &test_opaque, &test_hidden);
            assert_eq!(
                sec.palette_bits(),
                expected_bits,
                "distinct={distinct} should choose bits={expected_bits}"
            );

            for (i, &want) in states.iter().enumerate() {
                assert_eq!(
                    sec.get(i),
                    want,
                    "distinct={distinct} bits={expected_bits} cell {i}"
                );
            }

            let expected_non_air = states
                .iter()
                .filter(|&&s| !matches!(s, 0 | 12958 | 12959))
                .count() as u16;
            assert_eq!(sec.non_air(), expected_non_air, "distinct={distinct}");

            let mut wire = Vec::new();
            sec.write_wire(&biomes, &mut wire);
            let ref_wire = section_encode(&states, &biomes);
            assert_eq!(
                wire, ref_wire,
                "wire mismatch for distinct={distinct} bits={expected_bits}"
            );

            let (dec_non_air, dec_states, dec_biomes) = decode_wire_section(&wire);
            assert_eq!(dec_non_air as u16, expected_non_air);
            assert_eq!(dec_states, states);
            assert_eq!(dec_biomes, biomes);
        }
    }

    #[test]
    fn paletted_single_variants_and_non_air_air_ids() {
        for air_id in [0u32, 12958, 12959] {
            let states = [air_id; 4096];
            let sec = PalettedSection::pack(&states, &|_| false, &|_| false);
            assert_eq!(sec.palette_bits(), 0);
            assert_eq!(sec.non_air(), 0);
            assert_eq!(sec.is_uniform(), Some(air_id));
            assert_eq!(sec.light_class(), SectionLightClass::Empty);
            assert!(!sec.has_hidden());
            assert_eq!(sec.get(0), air_id);
            assert_eq!(sec.get(4095), air_id);
            assert_eq!(sec.approx_bytes(), std::mem::size_of::<PalettedSection>());

            let biomes = [3i64; 64];
            let mut wire = Vec::new();
            sec.write_wire(&biomes, &mut wire);
            assert_eq!(wire, section_encode(&states, &biomes));
        }

        let stone = [1u32; 4096];
        let sec = PalettedSection::pack(&stone, &|st| st == 1, &|_| false);
        assert_eq!(sec.palette_bits(), 0);
        assert_eq!(sec.non_air(), 4096);
        assert_eq!(sec.is_uniform(), Some(1));
        assert_eq!(sec.light_class(), SectionLightClass::Opaque);
        assert!(!sec.has_hidden());
        assert_eq!(sec.get(2047), 1);

        let mixed_air = std::array::from_fn(|i| match i % 3 {
            0 => 0,
            1 => 12958,
            _ => 12959,
        });
        let sec_mixed_air = PalettedSection::pack(&mixed_air, &|_| false, &|_| false);
        assert_eq!(sec_mixed_air.palette_bits(), 4);
        assert_eq!(sec_mixed_air.non_air(), 0);
        assert_eq!(sec_mixed_air.light_class(), SectionLightClass::Empty);
        assert_eq!(sec_mixed_air.is_uniform(), None);
    }

    #[test]
    fn paletted_cow_set_never_shrinks_and_promotes_all_boundaries() {
        let biomes: [i64; 64] = std::array::from_fn(|i| (i & 3) as i64);
        let mut ref_states = [0u32; 4096];
        let mut sec = PalettedSection::pack(&ref_states, &test_opaque, &test_hidden);
        assert_eq!(sec.palette_bits(), 0);
        assert_eq!(sec.light_class(), SectionLightClass::Empty);
        assert_eq!(sec.is_uniform(), Some(0));

        let next_state = |k: usize| -> u32 {
            match k {
                1 => 50,       // hidden + opaque
                2 => 17,       // non-opaque (17 % 10 == 7)
                260 => 25_000, // > 16384 for 15-bit Direct test
                _ => 100 + k as u32,
            }
        };

        for k in 1usize..=270 {
            let st = next_state(k);
            let idx = (k * 13) % 4096;
            sec.set(idx, st, &test_opaque, &test_hidden);
            ref_states[idx] = st;

            let expected_bits = match k + 1 {
                2..=16 => 4,
                17..=32 => 5,
                33..=64 => 6,
                65..=128 => 7,
                129..=256 => 8,
                _ => 15,
            };
            assert_eq!(sec.palette_bits(), expected_bits, "at k={k}");

            if matches!(k, 1 | 15 | 16 | 31 | 32 | 63 | 64 | 127 | 128 | 255 | 256 | 260 | 270) {
                for (i, &want) in ref_states.iter().enumerate() {
                    assert_eq!(sec.get(i), want, "k={k} idx={i}");
                }
                let want_non_air = ref_states.iter().filter(|&&s| !is_air_state(s)).count() as u16;
                assert_eq!(sec.non_air(), want_non_air, "k={k}");
                assert_eq!(
                    sec.has_hidden(),
                    ref_states.iter().any(|&s| test_hidden(s)),
                    "k={k}"
                );
                let mut wire = Vec::new();
                sec.write_wire(&biomes, &mut wire);
                let (dec_non_air, dec_states, dec_biomes) = decode_wire_section(&wire);
                assert_eq!(dec_non_air as u16, want_non_air);
                assert_eq!(dec_states, ref_states);
                assert_eq!(dec_biomes, biomes);
            }
        }

        // Overwrite ALL cells back to uniform opaque non-hidden state (1):
        // Palette must NOT shrink from 15, while is_uniform, non_air, light_class, has_hidden stay exact.
        for (idx, cell) in ref_states.iter_mut().enumerate() {
            sec.set(idx, 1, &test_opaque, &test_hidden);
            *cell = 1;
        }
        assert_eq!(sec.palette_bits(), 15, "resident palette must never shrink");
        assert_eq!(sec.is_uniform(), Some(1));
        assert_eq!(sec.non_air(), 4096);
        assert_eq!(sec.light_class(), SectionLightClass::Opaque);
        assert!(!sec.has_hidden());

        let mut wire = Vec::new();
        sec.write_wire(&biomes, &mut wire);
        let (dec_non_air, dec_states, _) = decode_wire_section(&wire);
        assert_eq!(dec_non_air, 4096);
        assert_eq!(dec_states, ref_states);

        // Also verify non-shrinking Indirect (bits=5) -> overwritten back to uniform opaque
        let mut ind_states = [17u32; 4096]; // non-opaque
        for (k, slot) in ind_states.iter_mut().take(20).enumerate() {
            *slot = 100 + k as u32;
        }
        ind_states[500] = 50; // hidden
        let mut ind_sec = PalettedSection::pack(&ind_states, &test_opaque, &test_hidden);
        assert_eq!(ind_sec.palette_bits(), 5);
        assert_eq!(ind_sec.light_class(), SectionLightClass::Mixed);
        assert!(ind_sec.has_hidden());
        assert_eq!(ind_sec.is_uniform(), None);
        for idx in 0..4096 {
            ind_sec.set(idx, 2, &test_opaque, &test_hidden);
        }
        assert_eq!(ind_sec.palette_bits(), 5, "Indirect palette must not shrink");
        assert_eq!(ind_sec.is_uniform(), Some(2));
        assert_eq!(ind_sec.non_air(), 4096);
        assert_eq!(ind_sec.light_class(), SectionLightClass::Opaque);
        assert!(!ind_sec.has_hidden());
    }

    #[test]
    fn paletted_real_worldgen_sections_differential() {
        crate::test_support::require_vanilla_data();
        let reg = crate::block_registry::breg().expect("block registry");
        let hidden = hidden_states();
        let is_opaque = |st: u32| reg.opacity_of(st) == 15;
        let is_hid = |st: u32| hidden.contains(&st);
        let biomes: [i64; 64] = std::array::from_fn(|i| (i % 11) as i64);

        let presets = [
            (worldgen::NoisePreset::Overworld, 8i32), // 64 * 24 = 1,536 sections
            (worldgen::NoisePreset::Nether, 4i32),    // 16 * 16 =   256 sections
            (worldgen::NoisePreset::End, 4i32),       // 16 * 16 =   256 sections
        ];
        let mut total_sections = 0usize;
        let mut wire = Vec::new();
        for (preset, side) in presets {
            let mut gen = worldgen::Gen::load_preset(4242, crate::name_to_state, preset)
                .expect("load worldgen preset");
            let lo = gen.data.world_min_y;
            let hi = lo + gen.data.world_height - 1;
            for cz in 0..side {
                for cx in 0..side {
                    let mut sy = lo;
                    while sy <= hi {
                        let arr = gen.terrain_section_at(cx, sy, cz, crate::feature::populate);
                        let sec = PalettedSection::pack(&arr, &is_opaque, &is_hid);

                        for (i, &want) in arr.iter().enumerate() {
                            assert_eq!(
                                sec.get(i),
                                want,
                                "{preset:?} chunk=({cx},{cz}) sy={sy} cell={i}"
                            );
                        }

                        let expected_non_air = arr
                            .iter()
                            .filter(|&&s| !matches!(s, 0 | 12958 | 12959))
                            .count() as u16;
                        assert_eq!(sec.non_air(), expected_non_air);

                        let expected_opaque = arr.iter().all(|&s| is_opaque(s));
                        let expected_lc = if expected_non_air == 0 {
                            SectionLightClass::Empty
                        } else if expected_opaque {
                            SectionLightClass::Opaque
                        } else {
                            SectionLightClass::Mixed
                        };
                        assert_eq!(sec.light_class(), expected_lc);
                        assert_eq!(sec.has_hidden(), arr.iter().any(|&s| is_hid(s)));

                        wire.clear();
                        sec.write_wire(&biomes, &mut wire);
                        let ref_wire = section_encode(&arr, &biomes);
                        assert_eq!(
                            wire, ref_wire,
                            "wire mismatch at {preset:?} chunk=({cx},{cz}) sy={sy}"
                        );
                        total_sections += 1;
                        sy += 16;
                    }
                }
            }
        }
        assert_eq!(total_sections, 2048);
    }
}
