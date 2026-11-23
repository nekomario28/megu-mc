//! Clean-room RFC 1950/1951 inflate — the single decoder shared by the PNG,
//! shader-pack ZIP, resource-pack ZIP, and protocol readers. Output is
//! always bounded by an explicit byte limit so a hostile stream cannot
//! expand unboundedly.

use std::fmt;

/// Reason an inflate failed, in a form every caller can wrap into its own
/// error enum.
#[derive(Debug)]
pub struct InflateError {
    reason: String,
}

impl InflateError {
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl fmt::Display for InflateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.reason)
    }
}

impl std::error::Error for InflateError {}

fn err<T>(reason: impl Into<String>) -> Result<T, InflateError> {
    Err(InflateError {
        reason: reason.into(),
    })
}

/// Inflates raw DEFLATE `input` into at most `limit` bytes.
pub fn inflate(input: &[u8], limit: usize) -> Result<Vec<u8>, InflateError> {
    let mut br = BitReader::new(input);
    inflate_deflate(&mut br, limit)
}

/// Inflates a zlib stream (RFC 1950) into at most `limit` bytes and verifies
/// the Adler-32 trailer. When `limit` is the exact expected size, a stream
/// that expands even one byte past it is rejected instead of decompressing
/// into a bomb.
pub fn inflate_zlib(input: &[u8], limit: usize) -> Result<Vec<u8>, InflateError> {
    if input.len() < 2 {
        return err("zlib header truncated");
    }
    let cmf = input[0];
    let flg = input[1];
    if cmf & 0x0F != 8 {
        return err("unsupported zlib compression method");
    }
    if cmf >> 4 > 7 {
        return err("zlib window size descriptor out of range");
    }
    if (u32::from(cmf) << 8 | u32::from(flg)) % 31 != 0 {
        return err("bad zlib header check bits");
    }
    if flg & 0x20 != 0 {
        return err("preset dictionary not supported");
    }
    let deflate = &input[2..];
    let mut br = BitReader::new(deflate);
    let out = inflate_deflate(&mut br, limit)?;
    // The Adler-32 sits on the first byte boundary after the last DEFLATE
    // bit; the bit reader may already hold those bytes in its buffer, so
    // the position is computed from consumed-bit accounting.
    let next_bit = br
        .pos
        .saturating_mul(8)
        .saturating_sub(usize::try_from(br.len).unwrap_or(usize::MAX));
    let adler_byte = next_bit.saturating_add(7) / 8;
    let adler_bytes = deflate
        .get(adler_byte..adler_byte.saturating_add(4))
        .filter(|s| s.len() == 4)
        .ok_or_else(|| InflateError {
            reason: "zlib stream truncated before Adler-32".into(),
        })?;
    if adler_byte.saturating_add(4) != deflate.len() {
        return err("trailing bytes after zlib trailer");
    }
    let want = u32::from_be_bytes([adler_bytes[0], adler_bytes[1], adler_bytes[2], adler_bytes[3]]);
    if adler32(&out) != want {
        return err("Adler-32 checksum mismatch");
    }
    Ok(out)
}

const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115,
    131, 163, 195, 227, 258,
];
const LENGTH_EXTRA: [u32; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u32; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12,
    13, 13,
];
const CLEN_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

/// Inflates raw DEFLATE blocks until the BFINAL block ends, producing at
/// most `limit` bytes.
fn inflate_deflate(br: &mut BitReader<'_>, limit: usize) -> Result<Vec<u8>, InflateError> {
    let mut out: Vec<u8> = Vec::with_capacity(limit.min(1 << 26));
    loop {
        let bfinal = br.bits(1)?;
        let btype = br.bits(2)?;
        match btype {
            0 => {
                br.align_byte();
                let len = br.aligned_u16()?;
                let nlen = br.aligned_u16()?;
                if len != !nlen {
                    return err("stored block LEN/NLEN mismatch");
                }
                let len = usize::from(len);
                if out.len() + len > limit {
                    return err("deflate output exceeds expected size");
                }
                for _ in 0..len {
                    out.push(br.aligned_byte()?);
                }
            }
            1 | 2 => {
                let (lit, dist) = if btype == 1 {
                    (fixed_litlen_huffman()?, fixed_dist_huffman()?)
                } else {
                    read_dynamic_tables(br)?
                };
                loop {
                    let sym = lit.decode(br)?;
                    match sym {
                        0..=255 => {
                            if out.len() >= limit {
                                return err("deflate output exceeds expected size");
                            }
                            out.push(sym as u8);
                        }
                        256 => break,
                        257..=285 => {
                            let idx = usize::from(sym - 257);
                            let length = usize::from(LENGTH_BASE[idx])
                                + br.bits(LENGTH_EXTRA[idx])? as usize;
                            let dsym = usize::from(dist.decode(br)?);
                            if dsym >= DIST_BASE.len() {
                                return err("invalid distance symbol");
                            }
                            let distance = usize::from(DIST_BASE[dsym])
                                + br.bits(DIST_EXTRA[dsym])? as usize;
                            if distance > out.len() {
                                return err("back-reference before output start");
                            }
                            if out.len() + length > limit {
                                return err("deflate output exceeds expected size");
                            }
                            let start = out.len() - distance;
                            for i in 0..length {
                                let b = out[start + i];
                                out.push(b);
                            }
                        }
                        _ => return err("invalid literal/length symbol"),
                    }
                }
            }
            _ => return err("reserved deflate block type"),
        }
        if bfinal == 1 {
            break;
        }
    }
    Ok(out)
}

/// LSB-first bit reader over a byte slice.
struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    buf: u64,
    len: u32,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            pos: 0,
            buf: 0,
            len: 0,
        }
    }

    fn bits(&mut self, n: u32) -> Result<u32, InflateError> {
        debug_assert!(n <= 32);
        while self.len < n {
            let b = *self
                .data
                .get(self.pos)
                .ok_or_else(|| InflateError {
                    reason: "deflate stream truncated".into(),
                })?;
            self.pos += 1;
            self.buf |= u64::from(b) << self.len;
            self.len += 8;
        }
        let v = (self.buf & ((1u64 << n) - 1)) as u32;
        self.buf >>= n;
        self.len -= n;
        Ok(v)
    }

    fn align_byte(&mut self) {
        let drop = self.len % 8;
        self.buf >>= drop;
        self.len -= drop;
    }

    fn aligned_byte(&mut self) -> Result<u8, InflateError> {
        if self.len >= 8 {
            let b = self.buf as u8;
            self.buf >>= 8;
            self.len -= 8;
            Ok(b)
        } else {
            self.bits(8).map(|v| v as u8)
        }
    }

    fn aligned_u16(&mut self) -> Result<u16, InflateError> {
        let lo = self.aligned_byte()?;
        let hi = self.aligned_byte()?;
        Ok(u16::from_le_bytes([lo, hi]))
    }
}

/// Canonical Huffman table decoded puff-style: `counts[n]` is how many
/// symbols have code length `n`, `symbols` lists them in code order.
struct Huffman {
    counts: [u16; 16],
    symbols: Vec<u16>,
}

impl Huffman {
    /// A table with no codes at all — used for the distance table of a
    /// literal-only dynamic block. Any decode attempt fails closed.
    fn empty() -> Self {
        Self {
            counts: [0; 16],
            symbols: Vec::new(),
        }
    }

    fn build(lengths: &[u8]) -> Result<Self, InflateError> {
        let mut counts = [0u16; 16];
        for &l in lengths {
            if l > 15 {
                return err("invalid huffman code length");
            }
            counts[usize::from(l)] += 1;
        }
        if counts[0] as usize == lengths.len() {
            // All lengths zero: degenerate empty table.
            return err("empty huffman table");
        }
        // Check for an over-subscribed code.
        let mut left: i32 = 1;
        for &c in counts.iter().skip(1) {
            left <<= 1;
            left -= i32::from(c);
            if left < 0 {
                return err("over-subscribed huffman code");
            }
        }
        let mut offs = [0usize; 16];
        for i in 1..15 {
            offs[i + 1] = offs[i] + counts[i] as usize;
        }
        let mut symbols = vec![0u16; lengths.len()];
        for (sym, &l) in lengths.iter().enumerate() {
            if l != 0 {
                symbols[offs[usize::from(l)]] = sym as u16;
                offs[usize::from(l)] += 1;
            }
        }
        Ok(Self { counts, symbols })
    }

    fn decode(&self, br: &mut BitReader<'_>) -> Result<u16, InflateError> {
        let mut code: i32 = 0;
        let mut first: i32 = 0;
        let mut index: i32 = 0;
        for len in 1..16usize {
            code |= br.bits(1)? as i32;
            let count = i32::from(self.counts[len]);
            if code - first < count {
                return self
                    .symbols
                    .get((index + (code - first)) as usize)
                    .copied()
                    .ok_or_else(|| InflateError {
                        reason: "invalid huffman code".into(),
                    });
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        err("invalid huffman code")
    }
}

fn fixed_litlen_huffman() -> Result<Huffman, InflateError> {
    let mut lengths = [0u8; 288];
    for (i, l) in lengths.iter_mut().enumerate() {
        *l = match i {
            0..=143 => 8,
            144..=255 => 9,
            256..=279 => 7,
            _ => 8,
        };
    }
    // Lengths are compile-time constants; build cannot fail.
    Huffman::build(&lengths)
}

fn fixed_dist_huffman() -> Result<Huffman, InflateError> {
    // Lengths are compile-time constants; build cannot fail.
    Huffman::build(&[5u8; 30])
}

/// Reads a dynamic-Huffman block header and builds the literal/length and
/// distance tables.
fn read_dynamic_tables(br: &mut BitReader<'_>) -> Result<(Huffman, Huffman), InflateError> {
    let hlit = br.bits(5)? as usize + 257;
    let hdist = br.bits(5)? as usize + 1;
    let hclen = br.bits(4)? as usize + 4;
    if hlit > 286 || hdist > 30 {
        return err("dynamic table size out of range");
    }
    let mut clen_lengths = [0u8; 19];
    for &idx in CLEN_ORDER.iter().take(hclen) {
        clen_lengths[idx] = br.bits(3)? as u8;
    }
    let clen = Huffman::build(&clen_lengths)?;
    let mut lengths = vec![0u8; hlit + hdist];
    let mut i = 0;
    while i < lengths.len() {
        let sym = clen.decode(br)?;
        match sym {
            0..=15 => {
                lengths[i] = sym as u8;
                i += 1;
            }
            16 => {
                if i == 0 {
                    return err("repeat with no previous length");
                }
                let prev = lengths[i - 1];
                let rep = 3 + br.bits(2)? as usize;
                for _ in 0..rep {
                    if i >= lengths.len() {
                        return err("repeat overflows table");
                    }
                    lengths[i] = prev;
                    i += 1;
                }
            }
            17 | 18 => {
                let rep = if sym == 17 {
                    3 + br.bits(3)? as usize
                } else {
                    11 + br.bits(7)? as usize
                };
                i += rep;
                if i > lengths.len() {
                    return err("zero-run overflows table");
                }
            }
            _ => return err("invalid code-length symbol"),
        }
    }
    let lit = Huffman::build(&lengths[..hlit])?;
    // RFC 1951 permits an all-zero distance table ("one distance code of
    // zero bits means no distance codes used at all") for blocks made
    // only of literals. Keep an empty table that fails only if a distance
    // symbol is actually requested.
    let dist = if lengths[hlit..].iter().all(|&l| l == 0) {
        Huffman::empty()
    } else {
        Huffman::build(&lengths[hlit..])?
    };
    Ok((lit, dist))
}

/// Adler-32 (RFC 1950) over `data`, used to verify the zlib trailer.
fn adler32(data: &[u8]) -> u32 {
    // NMAX-style chunking keeps the u64 accumulators far from overflow.
    let (mut a, mut b) = (1u64, 0u64);
    for chunk in data.chunks(5552) {
        for &byte in chunk {
            a += u64::from(byte);
            b += a;
        }
        a %= 65521;
        b %= 65521;
    }
    ((b << 16) | a) as u32
}
