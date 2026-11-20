//! Independent protocol-767, non-straddling palette encoder and strict decoder.

pub const CELLS: usize = 4096;

pub fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 128 {
        out.push(value as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn take_varint(input: &[u8], offset: &mut usize) -> u64 {
    let mut value = 0u64;
    for shift in (0..35).step_by(7) {
        let next = *input.get(*offset).expect("truncated varint");
        *offset += 1;
        value |= ((next & 0x7f) as u64) << shift;
        if next & 0x80 == 0 {
            return value;
        }
    }
    panic!("overlong varint");
}

fn encode_palette(out: &mut Vec<u8>, values: &[u32], min_bits: u8, max_bits: u8, direct_bits: u8) {
    let mut entries = Vec::new();
    let mut indices = Vec::with_capacity(values.len());
    for &value in values {
        if let Some(index) = entries.iter().position(|&entry| entry == value) {
            indices.push(index as u16);
        } else {
            if entries.len() == 1usize << max_bits {
                break;
            }
            indices.push(entries.len() as u16);
            entries.push(value);
        }
    }

    if entries.len() == 1 && indices.len() == values.len() {
        out.push(0);
        put_varint(out, entries[0] as u64);
        put_varint(out, 0);
        return;
    }

    let direct = indices.len() < values.len();
    let bits = if direct {
        direct_bits
    } else {
        ((usize::BITS - (entries.len() - 1).leading_zeros()) as u8).max(min_bits)
    };
    out.push(bits);
    if !direct {
        put_varint(out, entries.len() as u64);
        for entry in entries {
            put_varint(out, entry as u64);
        }
    }

    let per_word = 64 / bits as usize;
    put_varint(out, values.len().div_ceil(per_word) as u64);
    for (word_index, group) in values.chunks(per_word).enumerate() {
        let mut word = 0u64;
        for (slot, &value) in group.iter().enumerate() {
            let stored = if direct {
                value as u64
            } else {
                indices[word_index * per_word + slot] as u64
            };
            word |= stored << (slot * bits as usize);
        }
        out.extend_from_slice(&word.to_be_bytes());
    }
}

pub fn encode(states: &[u32; CELLS], biomes: &[i64; 64], out: &mut Vec<u8>) {
    let non_air = states
        .iter()
        .filter(|&&state| !matches!(state, 0 | 12958 | 12959))
        .count();
    out.extend_from_slice(&(non_air as u16).to_be_bytes());
    encode_palette(out, states, 4, 8, 15);
    write_biomes(out, biomes);
}

pub fn write_biomes(out: &mut Vec<u8>, biomes: &[i64]) {
    assert_eq!(biomes.len(), 64);
    let mut entries = [0u32; 8];
    let mut indices = [0u8; 64];
    let mut distinct = 0usize;
    let mut direct = false;
    for (cell, &biome) in biomes.iter().enumerate() {
        let value = u32::try_from(biome).expect("biome ID must be nonnegative");
        let index = entries[..distinct].iter().position(|&v| v == value);
        if let Some(index) = index {
            indices[cell] = index as u8;
        } else if distinct < entries.len() {
            indices[cell] = distinct as u8;
            entries[distinct] = value;
            distinct += 1;
        } else {
            direct = true;
            break;
        }
    }
    if distinct == 1 && !direct {
        out.push(0);
        put_varint(out, u64::from(entries[0]));
        put_varint(out, 0);
        return;
    }
    let bits = if direct {
        6
    } else {
        ((usize::BITS - (distinct - 1).leading_zeros()) as u8).max(1)
    };
    out.push(bits);
    if !direct {
        put_varint(out, distinct as u64);
        for &entry in &entries[..distinct] {
            put_varint(out, entry as u64);
        }
    }
    let per = 64 / bits as usize;
    put_varint(out, 64usize.div_ceil(per) as u64);
    for word_index in 0..64usize.div_ceil(per) {
        let mut word = 0u64;
        for cell in word_index * per..((word_index + 1) * per).min(64) {
            let stored = if direct {
                biomes[cell] as u64
            } else {
                indices[cell] as u64
            };
            word |= stored << ((cell - word_index * per) * bits as usize);
        }
        out.extend_from_slice(&word.to_be_bytes());
    }
}

fn decode_palette(input: &[u8], offset: &mut usize, size: usize, max_bits: u8) -> Vec<u32> {
    let bits = *input.get(*offset).expect("missing palette bits");
    *offset += 1;
    assert!(bits == 0 || (bits <= max_bits && bits >= 1) || bits == 15 || bits == 6);
    if bits == 0 {
        let value = u32::try_from(take_varint(input, offset)).expect("palette value too large");
        assert_eq!(take_varint(input, offset), 0);
        return vec![value; size];
    }
    let entries = if bits <= max_bits {
        let count = take_varint(input, offset) as usize;
        assert!(count >= 2 && count <= 1usize << max_bits);
        (0..count)
            .map(|_| u32::try_from(take_varint(input, offset)).expect("palette entry too large"))
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let per_word = 64 / bits as usize;
    let count = take_varint(input, offset) as usize;
    assert_eq!(count, size.div_ceil(per_word));
    let mut values = Vec::with_capacity(size);
    let mask = (1u64 << bits) - 1;
    for _ in 0..count {
        let bytes: [u8; 8] = input
            .get(*offset..*offset + 8)
            .expect("truncated palette word")
            .try_into()
            .unwrap();
        *offset += 8;
        let word = u64::from_be_bytes(bytes);
        for slot in 0..per_word {
            if values.len() == size {
                break;
            }
            let raw = ((word >> (slot * bits as usize)) & mask) as usize;
            values.push(if entries.is_empty() {
                raw as u32
            } else {
                *entries.get(raw).expect("palette index out of bounds")
            });
        }
    }
    values
}

pub fn decode(input: &[u8]) -> ([u32; CELLS], [i64; 64], u16) {
    let non_air = u16::from_be_bytes(
        input
            .get(..2)
            .expect("missing non-air count")
            .try_into()
            .unwrap(),
    );
    let mut offset = 2;
    let states: [u32; CELLS] = decode_palette(input, &mut offset, CELLS, 8)
        .try_into()
        .unwrap();
    let biomes: [i64; 64] = decode_palette(input, &mut offset, 64, 3)
        .into_iter()
        .map(i64::from)
        .collect::<Vec<_>>()
        .try_into()
        .unwrap();
    assert_eq!(offset, input.len(), "trailing wire data");
    assert_eq!(
        non_air as usize,
        states
            .iter()
            .filter(|&&s| !matches!(s, 0 | 12958 | 12959))
            .count()
    );
    (states, biomes, non_air)
}
