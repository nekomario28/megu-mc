// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! RFC 1952 framing and RFC 1951 decoding for the dependency-free NBT API.

use crate::Error;
use Error::{ByteBudget, InvalidGzip};

pub(crate) fn decode(mut input: &[u8], budget: usize) -> Result<Vec<u8>, Error> {
    if input.is_empty() {
        return Err(InvalidGzip);
    }
    let mut output = Vec::new();
    while !input.is_empty() {
        let start = output.len();
        let mut bits = Bits {
            input: member_payload(input)?,
            used: 0,
        };
        inflate(&mut bits, &mut output, start, budget)?;
        bits.align();
        let crc = u32::from_le_bytes(take(&mut bits.input, 4)?.try_into().unwrap());
        let size = u32::from_le_bytes(take(&mut bits.input, 4)?.try_into().unwrap());
        let member = &output[start..];
        if crc != crc32(member) || size != member.len() as u32 {
            return Err(InvalidGzip);
        }
        input = bits.input;
    }
    Ok(output)
}

fn take<'a>(input: &mut &'a [u8], n: usize) -> Result<&'a [u8], Error> {
    let (taken, rest) = input.split_at_checked(n).ok_or(InvalidGzip)?;
    *input = rest;
    Ok(taken)
}

fn member_payload(header: &[u8]) -> Result<&[u8], Error> {
    let mut rest = header;
    let fixed = take(&mut rest, 10)?;
    if fixed[..3] != [0x1f, 0x8b, 8] || fixed[3] & 0xe0 != 0 {
        return Err(InvalidGzip);
    }
    let flags = fixed[3];
    if flags & 4 != 0 {
        let length = u16::from_le_bytes(take(&mut rest, 2)?.try_into().unwrap());
        take(&mut rest, usize::from(length))?;
    }
    for flag in [8, 16] {
        if flags & flag != 0 {
            let end = rest.iter().position(|&b| b == 0).ok_or(InvalidGzip)?;
            take(&mut rest, end + 1)?;
        }
    }
    if flags & 2 != 0 {
        let expected = crc32(&header[..header.len() - rest.len()]) as u16;
        let actual = u16::from_le_bytes(take(&mut rest, 2)?.try_into().unwrap());
        if actual != expected {
            return Err(InvalidGzip);
        }
    }
    Ok(rest)
}

struct Bits<'a> {
    input: &'a [u8],
    used: u8,
}

impl Bits<'_> {
    fn read(&mut self, n: u8) -> Result<u16, Error> {
        let mut value = 0;
        let mut read = 0;
        while read < n {
            let byte = *self.input.first().ok_or(InvalidGzip)?;
            let width = (n - read).min(8 - self.used);
            let mask = (1u16 << width) - 1;
            value |= (u16::from(byte >> self.used) & mask) << read;
            read += width;
            self.used += width;
            if self.used == 8 {
                self.used = 0;
                self.input = &self.input[1..];
            }
        }
        Ok(value)
    }

    fn align(&mut self) {
        if self.used != 0 {
            self.input = &self.input[1..];
            self.used = 0;
        }
    }
}

// Canonical codes are contiguous at a given width, in symbol order.
struct Huffman {
    counts: [u16; 16],
    symbols: Vec<u16>,
    max_width: usize,
}

impl Huffman {
    fn new(lengths: &[u8], allow_single: bool) -> Result<Self, Error> {
        let mut counts = [0u16; 16];
        let mut max_width = 0;
        for &length in lengths {
            let width = usize::from(length);
            if width != 0 {
                *counts.get_mut(width).ok_or(InvalidGzip)? += 1;
                max_width = max_width.max(width);
            }
        }
        let mut remaining = 1i32;
        let mut offsets = [0usize; 16];
        let mut total = 0;
        for width in 1..=max_width {
            remaining = 2 * remaining - i32::from(counts[width]);
            if remaining < 0 {
                return Err(InvalidGzip);
            }
            offsets[width] = total;
            total += usize::from(counts[width]);
        }
        if remaining != 0 && !(allow_single && total == 1 && max_width == 1) {
            return Err(InvalidGzip);
        }
        let mut symbols = vec![0; total];
        for (symbol, &length) in lengths
            .iter()
            .enumerate()
            .filter(|&(_, &length)| length != 0)
        {
            let offset = &mut offsets[usize::from(length)];
            symbols[*offset] = symbol as u16;
            *offset += 1;
        }
        Ok(Self {
            counts,
            symbols,
            max_width,
        })
    }

    fn read(&self, bits: &mut Bits<'_>) -> Result<u16, Error> {
        let (mut code, mut first, mut offset) = (0, 0, 0);
        for width in 1..=self.max_width {
            code = (code << 1) | bits.read(1)?;
            let count = self.counts[width];
            if code >= first && code - first < count {
                return Ok(self.symbols[offset + usize::from(code - first)]);
            }
            first = (first + count) << 1;
            offset += usize::from(count);
        }
        Err(InvalidGzip)
    }
}

fn fixed_trees() -> Result<(Huffman, Option<Huffman>), Error> {
    let mut lengths = [8; 288];
    lengths[144..256].fill(9);
    lengths[256..280].fill(7);
    Ok((
        Huffman::new(&lengths, false)?,
        Some(Huffman::new(&[5; 32], false)?),
    ))
}

fn dynamic_trees(bits: &mut Bits<'_>) -> Result<(Huffman, Option<Huffman>), Error> {
    let literals = usize::from(bits.read(5)?) + 257;
    let distances = usize::from(bits.read(5)?) + 1;
    let code_count = usize::from(bits.read(4)?) + 4;
    if literals > 286 {
        return Err(InvalidGzip);
    }
    const ORDER: [usize; 19] = [
        16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
    ];
    let mut codes = [0; 19];
    for &symbol in &ORDER[..code_count] {
        codes[symbol] = bits.read(3)? as u8;
    }
    let tree = Huffman::new(&codes, false)?;
    let mut lengths = Vec::with_capacity(literals + distances);
    while lengths.len() < literals + distances {
        let (length, count) = match tree.read(bits)? {
            n @ 0..=15 => (n as u8, 1),
            16 => (
                *lengths.last().ok_or(InvalidGzip)?,
                usize::from(bits.read(2)?) + 3,
            ),
            17 => (0, usize::from(bits.read(3)?) + 3),
            18 => (0, usize::from(bits.read(7)?) + 11),
            _ => return Err(InvalidGzip),
        };
        let end = lengths.len() + count;
        if end > literals + distances {
            return Err(InvalidGzip);
        }
        lengths.resize(end, length);
    }
    if lengths[256] == 0 {
        return Err(InvalidGzip);
    }
    let (literals, distances) = lengths.split_at(literals);
    let distances = if distances == [0] {
        None
    } else {
        Some(Huffman::new(distances, true)?)
    };
    Ok((Huffman::new(literals, true)?, distances))
}

const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const DISTANCE_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];

fn inflate(
    bits: &mut Bits<'_>,
    out: &mut Vec<u8>,
    start: usize,
    budget: usize,
) -> Result<(), Error> {
    loop {
        let final_block = bits.read(1)? != 0;
        match bits.read(2)? {
            0 => {
                bits.align();
                let length = bits.read(16)?;
                let complement = bits.read(16)?;
                if length != !complement {
                    return Err(InvalidGzip);
                }
                let data = take(&mut bits.input, usize::from(length))?;
                if data.len() > budget - out.len() {
                    return Err(ByteBudget);
                }
                out.extend_from_slice(data);
            }
            kind @ (1 | 2) => {
                let (literals, distances) = if kind == 1 {
                    fixed_trees()?
                } else {
                    dynamic_trees(bits)?
                };
                loop {
                    match literals.read(bits)? {
                        byte @ 0..=255 => {
                            if out.len() == budget {
                                return Err(ByteBudget);
                            }
                            out.push(byte as u8);
                        }
                        256 => break,
                        symbol @ 257..=285 => {
                            let index = (symbol - 257) as usize;
                            let extra = if symbol < 285 {
                                index.saturating_sub(4) / 4
                            } else {
                                0
                            };
                            let length = usize::from(LENGTH_BASE[index] + bits.read(extra as u8)?);
                            let code = distances.as_ref().ok_or(InvalidGzip)?.read(bits)? as usize;
                            let base = *DISTANCE_BASE.get(code).ok_or(InvalidGzip)?;
                            let extra = code.saturating_sub(2) / 2;
                            let distance = usize::from(base + bits.read(extra as u8)?);
                            if distance > out.len() - start {
                                return Err(InvalidGzip);
                            }
                            if length > budget - out.len() {
                                return Err(ByteBudget);
                            }
                            for _ in 0..length {
                                out.push(out[out.len() - distance]);
                            }
                        }
                        _ => return Err(InvalidGzip),
                    }
                }
            }
            _ => return Err(InvalidGzip),
        }
        if final_block {
            return Ok(());
        }
    }
}

const CRC_TABLE: [u32; 256] = {
    let mut table = [0; 256];
    let mut i = 0;
    while i < table.len() {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = (crc >> 1) ^ if crc & 1 == 1 { 0xedb88320 } else { 0 };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
};

fn crc32(data: &[u8]) -> u32 {
    !data.iter().fold(!0, |crc, byte| {
        (crc >> 8) ^ CRC_TABLE[((crc as u8) ^ byte) as usize]
    })
}
