// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Zero-dependency Minecraft 1.21.1 (Protocol 767) TCP wire codec, RFC 1950 zlib,
//! and Handshake -> Login -> Configuration -> Play state machine.

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mc_spine_proto::{PacketDir, PacketPhase, ProtocolCodec, RecipeSyncMode};

/// Protocol 767 / Minecraft 1.21.1 — the single canonical version constant,
/// sourced from `mc-spine-proto` rather than a local literal.
pub const PROTOCOL_767: i32 = mc_spine_proto::ProtocolVersion::V1_21_1.number();
pub const MAX_PACKET_SIZE: usize = 16 * 1024 * 1024;
const MAX_NBT_DEPTH: usize = 64;
const MAX_NBT_ARRAY: usize = 1_048_576;

// ---------------------------------------------------------------------------
// Wire reader / writer primitives
// ---------------------------------------------------------------------------

pub struct WireReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> WireReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    pub fn rest(&self) -> &'a [u8] {
        &self.data[self.pos..]
    }

    pub fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or_else(|| "wire length overflow".to_string())?;
        if end > self.data.len() {
            return Err(format!(
                "truncated wire field: needed {n} bytes, {} remaining",
                self.remaining()
            ));
        }
        let slice = &self.data[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    pub fn read_u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    pub fn read_bool(&mut self) -> Result<bool, String> {
        Ok(self.read_u8()? != 0)
    }

    pub fn read_i16(&mut self) -> Result<i16, String> {
        Ok(i16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }

    pub fn read_u16(&mut self) -> Result<u16, String> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }

    pub fn read_i32(&mut self) -> Result<i32, String> {
        Ok(i32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn read_i64(&mut self) -> Result<i64, String> {
        Ok(i64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub fn read_u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub fn read_f32(&mut self) -> Result<f32, String> {
        Ok(f32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn read_f64(&mut self) -> Result<f64, String> {
        Ok(f64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }

    /// Delegated to `mc_spine_proto`'s canonical reader — identical bounds
    /// (final byte `& 0xf0` rejected, five bytes max); the `String` error
    /// surface this module's callers use is kept.
    pub fn read_varint(&mut self) -> Result<i32, String> {
        let mut r = mc_spine_proto::WireReader::new(self.rest());
        let value = r.read_varint().map_err(|e| e.to_string())?;
        self.pos += r.position();
        Ok(value)
    }

    /// Delegated to `mc_spine_proto`'s canonical reader — identical bounds
    /// (final byte `& 0xfe` rejected, ten bytes max).
    pub fn read_varlong(&mut self) -> Result<i64, String> {
        let mut r = mc_spine_proto::WireReader::new(self.rest());
        let value = r.read_varlong().map_err(|e| e.to_string())?;
        self.pos += r.position();
        Ok(value)
    }

    pub fn read_string(&mut self, max_bytes: usize) -> Result<String, String> {
        let len = usize::try_from(self.read_varint()?)
            .map_err(|_| "negative string length".to_string())?;
        if len > max_bytes {
            return Err(format!("string length {len} exceeds bound {max_bytes}"));
        }
        let raw = self.take(len)?;
        std::str::from_utf8(raw)
            .map(str::to_string)
            .map_err(|_| "invalid UTF-8 string".to_string())
    }

    pub fn read_uuid(&mut self) -> Result<[u8; 16], String> {
        Ok(self.take(16)?.try_into().unwrap())
    }

    pub fn read_position(&mut self) -> Result<[i32; 3], String> {
        let packed = self.read_i64()?;
        Ok(unpack_position(packed))
    }

    pub fn read_bitset(&mut self, max_longs: usize) -> Result<Vec<u64>, String> {
        let count = usize::try_from(self.read_varint()?)
            .map_err(|_| "negative bitset length".to_string())?;
        if count > max_longs {
            return Err(format!("bitset length {count} exceeds bound {max_longs}"));
        }
        let mut words = Vec::with_capacity(count);
        for _ in 0..count {
            words.push(self.read_u64()?);
        }
        Ok(words)
    }

    /// 1.21.5+ `java.util.BitSet` wire form: `VarInt` byte count then
    /// little-endian bytes (`byte[i/8] >> (i%8)`), normalized to the u64-word
    /// representation [`bitset_get`] reads.
    pub fn read_bitset_bytes(&mut self, max_bytes: usize) -> Result<Vec<u64>, String> {
        let count = usize::try_from(self.read_varint()?)
            .map_err(|_| "negative bitset byte length".to_string())?;
        if count > max_bytes {
            return Err(format!(
                "bitset byte length {count} exceeds bound {max_bytes}"
            ));
        }
        let bytes = self.take(count)?;
        let mut words = Vec::with_capacity(count.div_ceil(8));
        for chunk in bytes.chunks(8) {
            let mut word = [0u8; 8];
            word[..chunk.len()].copy_from_slice(chunk);
            words.push(u64::from_le_bytes(word));
        }
        Ok(words)
    }

    pub fn read_nbt(&mut self) -> Result<NbtValue, String> {
        let tag = self.read_u8()?;
        self.read_nbt_payload(tag, 0)
    }

    fn read_nbt_string(&mut self) -> Result<String, String> {
        let len = usize::from(self.read_u16()?);
        let raw = self.take(len)?;
        Ok(String::from_utf8_lossy(raw).into_owned())
    }

    fn read_nbt_payload(&mut self, tag: u8, depth: usize) -> Result<NbtValue, String> {
        if depth >= MAX_NBT_DEPTH {
            return Err("NBT nesting depth exceeded".to_string());
        }
        match tag {
            0 => Ok(NbtValue::End),
            1 => Ok(NbtValue::Byte(self.read_u8()? as i8)),
            2 => Ok(NbtValue::Short(self.read_i16()?)),
            3 => Ok(NbtValue::Int(self.read_i32()?)),
            4 => Ok(NbtValue::Long(self.read_i64()?)),
            5 => Ok(NbtValue::Float(self.read_f32()?)),
            6 => Ok(NbtValue::Double(self.read_f64()?)),
            7 => {
                let len = usize::try_from(self.read_i32()?)
                    .map_err(|_| "negative NBT byte array".to_string())?;
                if len > MAX_NBT_ARRAY {
                    return Err("NBT byte array exceeds bound".to_string());
                }
                let _ = self.take(len)?;
                Ok(NbtValue::ArrayLen(len))
            }
            8 => Ok(NbtValue::String(self.read_nbt_string()?)),
            9 => {
                let elem_tag = self.read_u8()?;
                let len = usize::try_from(self.read_i32()?)
                    .map_err(|_| "negative NBT list".to_string())?;
                if len > MAX_NBT_ARRAY {
                    return Err("NBT list exceeds bound".to_string());
                }
                let mut items = Vec::with_capacity(len.min(64));
                for _ in 0..len {
                    items.push(self.read_nbt_payload(elem_tag, depth + 1)?);
                }
                Ok(NbtValue::List(items))
            }
            10 => {
                let mut map = HashMap::new();
                loop {
                    let child = self.read_u8()?;
                    if child == 0 {
                        return Ok(NbtValue::Compound(map));
                    }
                    let key = self.read_nbt_string()?;
                    let value = self.read_nbt_payload(child, depth + 1)?;
                    map.insert(key, value);
                }
            }
            11 => {
                let len = usize::try_from(self.read_i32()?)
                    .map_err(|_| "negative NBT int array".to_string())?;
                if len > MAX_NBT_ARRAY {
                    return Err("NBT int array exceeds bound".to_string());
                }
                let _ = self.take(len * 4)?;
                Ok(NbtValue::ArrayLen(len))
            }
            12 => {
                let len = usize::try_from(self.read_i32()?)
                    .map_err(|_| "negative NBT long array".to_string())?;
                if len > MAX_NBT_ARRAY {
                    return Err("NBT long array exceeds bound".to_string());
                }
                let _ = self.take(len * 8)?;
                Ok(NbtValue::ArrayLen(len))
            }
            other => Err(format!("unknown NBT tag {other}")),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum NbtValue {
    End,
    Byte(i8),
    Short(i16),
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    String(String),
    List(Vec<NbtValue>),
    Compound(HashMap<String, NbtValue>),
    ArrayLen(usize),
}

impl NbtValue {
    pub fn as_i32(&self) -> Option<i32> {
        match *self {
            Self::Byte(v) => Some(i32::from(v)),
            Self::Short(v) => Some(i32::from(v)),
            Self::Int(v) => Some(v),
            Self::Long(v) => i32::try_from(v).ok(),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        self.as_i32().map(|v| v != 0)
    }

    pub fn extract_text(&self) -> String {
        match self {
            Self::String(s) => s.clone(),
            Self::Compound(map) => {
                let mut out = String::new();
                if let Some(Self::String(text)) = map.get("text") {
                    out.push_str(text);
                } else if let Some(Self::String(trans)) = map.get("translate") {
                    out.push_str(trans);
                }
                if let Some(Self::List(extra)) = map.get("extra") {
                    for part in extra {
                        out.push_str(&part.extract_text());
                    }
                }
                out
            }
            Self::List(items) => items.iter().map(Self::extract_text).collect(),
            _ => String::new(),
        }
    }
}

/// Delegated to `mc_spine_proto`'s canonical `WireWriter` — `from_vec`
/// appends in place, so no copy of the caller's buffer is made.
pub fn write_varint(buf: &mut Vec<u8>, value: i32) {
    let mut w = mc_spine_proto::WireWriter::from_vec(std::mem::take(buf));
    w.write_varint(value);
    *buf = w.into_vec();
}

pub fn write_varlong(buf: &mut Vec<u8>, value: i64) {
    let mut w = mc_spine_proto::WireWriter::from_vec(std::mem::take(buf));
    w.write_varlong(value);
    *buf = w.into_vec();
}

pub fn write_string(buf: &mut Vec<u8>, s: &str) {
    write_varint(buf, s.len() as i32);
    buf.extend_from_slice(s.as_bytes());
}

/// `x:26 | z:26 | y:12` packing, delegated to `mc_spine_proto::BlockPos`.
pub fn pack_position(pos: [i32; 3]) -> i64 {
    let [x, y, z] = pos;
    mc_spine_proto::BlockPos::new(x, y, z).pack()
}

/// Inverse of [`pack_position`], sign-extending each field.
pub fn unpack_position(packed: i64) -> [i32; 3] {
    let p = mc_spine_proto::BlockPos::unpack(packed);
    [p.x, p.y, p.z]
}

pub fn bitset_get(words: &[u64], index: usize) -> bool {
    let word = index / 64;
    let bit = index % 64;
    words.get(word).is_some_and(|w| (w >> bit) & 1 != 0)
}

pub fn bitset_count_ones(words: &[u64]) -> usize {
    words.iter().map(|w| w.count_ones() as usize).sum()
}

pub fn parse_uuid(input: &str) -> Result<[u8; 16], String> {
    let hex: String = input.chars().filter(|&c| c != '-').collect();
    if hex.len() != 32 {
        return Err(format!("invalid UUID length: '{input}'"));
    }
    let mut out = [0u8; 16];
    for (idx, slot) in out.iter_mut().enumerate() {
        let part = &hex[idx * 2..idx * 2 + 2];
        *slot = u8::from_str_radix(part, 16).map_err(|_| format!("invalid UUID hex: '{input}'"))?;
    }
    Ok(out)
}

/// Vanilla Minecraft offline-mode UUID v3 (`MD5("OfflinePlayer:" + name)`).
pub fn offline_uuid(username: &str) -> [u8; 16] {
    let mut input = Vec::with_capacity(14 + username.len());
    input.extend_from_slice(b"OfflinePlayer:");
    input.extend_from_slice(username.as_bytes());
    let mut digest = md5(&input);
    digest[6] = (digest[6] & 0x0f) | 0x30;
    digest[8] = (digest[8] & 0x3f) | 0x80;
    digest
}

fn md5(data: &[u8]) -> [u8; 16] {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    const K: [u32; 64] = [
        0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613,
        0xfd469501, 0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193,
        0xa679438e, 0x49b40821, 0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d,
        0x02441453, 0xd8a1e681, 0xe7d3fbc8, 0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed,
        0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a, 0xfffa3942, 0x8771f681, 0x6d9d6122,
        0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70, 0x289b7ec6, 0xeaa127fa,
        0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665, 0xf4292244,
        0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
        0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb,
        0xeb86d391,
    ];

    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_le_bytes());

    let mut a0: u32 = 0x67452301;
    let mut b0: u32 = 0xefcdab89;
    let mut c0: u32 = 0x98badcfe;
    let mut d0: u32 = 0x10325476;

    for chunk in msg.as_chunks::<64>().0 {
        let mut m = [0u32; 16];
        for (i, word) in m.iter_mut().enumerate() {
            *word = u32::from_le_bytes(chunk[i * 4..i * 4 + 4].try_into().unwrap());
        }
        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for i in 0..64 {
            let (f, g) = match i {
                0..=15 => ((b & c) | ((!b) & d), i),
                16..=31 => ((d & b) | ((!d) & c), (5 * i + 1) % 16),
                32..=47 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | (!d)), (7 * i) % 16),
            };
            let temp = d;
            d = c;
            c = b;
            let sum = a.wrapping_add(f).wrapping_add(K[i]).wrapping_add(m[g]);
            b = b.wrapping_add(sum.rotate_left(S[i]));
            a = temp;
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }

    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&a0.to_le_bytes());
    out[4..8].copy_from_slice(&b0.to_le_bytes());
    out[8..12].copy_from_slice(&c0.to_le_bytes());
    out[12..16].copy_from_slice(&d0.to_le_bytes());
    out
}

// ---------------------------------------------------------------------------
// Pure-Rust RFC 1950 Zlib / RFC 1951 DEFLATE (zero external dependencies)
// ---------------------------------------------------------------------------

pub fn adler32(data: &[u8]) -> u32 {
    const MOD: u32 = 65_521;
    let mut s1 = 1u32;
    let mut s2 = 0u32;
    for &b in data {
        s1 = (s1 + u32::from(b)) % MOD;
        s2 = (s2 + s1) % MOD;
    }
    (s2 << 16) | s1
}

/// Encode bytes into a valid RFC 1950 zlib stream using RFC 1951 stored blocks.
pub fn zlib_compress_stored(input: &[u8]) -> Vec<u8> {
    let block_count = input.len().div_ceil(65_535).max(1);
    let mut out = Vec::with_capacity(2 + input.len() + block_count * 5 + 4);
    // CMF=0x78 (DEFLATE, 32K window), FLG=0x01 (FCHECK valid, no dict, fastest level)
    out.push(0x78);
    out.push(0x01);

    if input.is_empty() {
        out.push(0x01); // BFINAL=1, BTYPE=00 (stored)
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(!0u16).to_le_bytes());
    } else {
        let mut offset = 0;
        while offset < input.len() {
            let end = (offset + 65_535).min(input.len());
            let chunk = &input[offset..end];
            let is_final = end == input.len();
            out.push(u8::from(is_final));
            let len = chunk.len() as u16;
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&(!len).to_le_bytes());
            out.extend_from_slice(chunk);
            offset = end;
        }
    }

    out.extend_from_slice(&adler32(input).to_be_bytes());
    out
}

/// Decode an RFC 1950 zlib stream with an explicit uncompressed byte budget.
pub fn zlib_decompress(input: &[u8], budget: usize) -> Result<Vec<u8>, String> {
    if input.len() < 6 {
        return Err("zlib stream too short".to_string());
    }
    let cmf = input[0];
    let flg = input[1];
    if cmf & 0x0f != 8 || (cmf >> 4) > 7 {
        return Err("unsupported zlib compression method/window".to_string());
    }
    if ((u16::from(cmf) << 8) | u16::from(flg)) % 31 != 0 {
        return Err("invalid zlib header checksum".to_string());
    }
    if flg & 0x20 != 0 {
        return Err("preset zlib dictionaries are not supported".to_string());
    }

    let mut bits = BitReader {
        input: &input[2..],
        used: 0,
    };
    let mut output = Vec::new();
    inflate_deflate(&mut bits, &mut output, budget)?;
    bits.align();

    let trailer = bits.take_bytes(4)?;
    if !bits.input.is_empty() {
        return Err("trailing bytes after zlib trailer".to_string());
    }
    let expected = u32::from_be_bytes(trailer.try_into().unwrap());
    let actual = adler32(&output);
    if actual != expected {
        return Err(format!(
            "zlib Adler-32 mismatch: expected 0x{expected:08x}, got 0x{actual:08x}"
        ));
    }
    Ok(output)
}

struct BitReader<'a> {
    input: &'a [u8],
    used: u8,
}

impl<'a> BitReader<'a> {
    fn read(&mut self, n: u8) -> Result<u16, String> {
        let mut value = 0u16;
        let mut read = 0u8;
        while read < n {
            let byte = *self
                .input
                .first()
                .ok_or_else(|| "truncated DEFLATE stream".to_string())?;
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

    fn take_bytes(&mut self, n: usize) -> Result<&'a [u8], String> {
        let (head, tail) = self
            .input
            .split_at_checked(n)
            .ok_or_else(|| "truncated DEFLATE bytes".to_string())?;
        self.input = tail;
        Ok(head)
    }
}

struct Huffman {
    counts: [u16; 16],
    symbols: Vec<u16>,
    max_width: usize,
}

impl Huffman {
    fn new(lengths: &[u8], allow_single: bool) -> Result<Self, String> {
        let mut counts = [0u16; 16];
        let mut max_width = 0usize;
        for &length in lengths {
            let width = usize::from(length);
            if width != 0 {
                let slot = counts
                    .get_mut(width)
                    .ok_or_else(|| "Huffman code width overflow".to_string())?;
                *slot += 1;
                max_width = max_width.max(width);
            }
        }
        let mut remaining = 1i32;
        let mut offsets = [0usize; 16];
        let mut total = 0usize;
        for width in 1..=max_width {
            remaining = 2 * remaining - i32::from(counts[width]);
            if remaining < 0 {
                return Err("oversubscribed Huffman table".to_string());
            }
            offsets[width] = total;
            total += usize::from(counts[width]);
        }
        if remaining != 0 && !(allow_single && total == 1 && max_width == 1) {
            return Err("incomplete Huffman table".to_string());
        }
        let mut symbols = vec![0u16; total];
        for (symbol, &length) in lengths.iter().enumerate() {
            if length != 0 {
                let offset = &mut offsets[usize::from(length)];
                symbols[*offset] = symbol as u16;
                *offset += 1;
            }
        }
        Ok(Self {
            counts,
            symbols,
            max_width,
        })
    }

    fn read(&self, bits: &mut BitReader<'_>) -> Result<u16, String> {
        let (mut code, mut first, mut offset) = (0u16, 0u16, 0usize);
        for width in 1..=self.max_width {
            code = (code << 1) | bits.read(1)?;
            let count = self.counts[width];
            if code >= first && code - first < count {
                return Ok(self.symbols[offset + usize::from(code - first)]);
            }
            first = (first + count) << 1;
            offset += usize::from(count);
        }
        Err("invalid Huffman symbol".to_string())
    }
}

fn fixed_trees() -> Result<(Huffman, Option<Huffman>), String> {
    let mut lengths = [8u8; 288];
    lengths[144..256].fill(9);
    lengths[256..280].fill(7);
    Ok((
        Huffman::new(&lengths, false)?,
        Some(Huffman::new(&[5u8; 32], false)?),
    ))
}

fn dynamic_trees(bits: &mut BitReader<'_>) -> Result<(Huffman, Option<Huffman>), String> {
    let literals = usize::from(bits.read(5)?) + 257;
    let distances = usize::from(bits.read(5)?) + 1;
    let code_count = usize::from(bits.read(4)?) + 4;
    if literals > 286 {
        return Err("invalid dynamic literal count".to_string());
    }
    const ORDER: [usize; 19] = [
        16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
    ];
    let mut codes = [0u8; 19];
    for &symbol in &ORDER[..code_count] {
        codes[symbol] = bits.read(3)? as u8;
    }
    let tree = Huffman::new(&codes, false)?;
    let mut lengths = Vec::with_capacity(literals + distances);
    while lengths.len() < literals + distances {
        let (length, count) = match tree.read(bits)? {
            n @ 0..=15 => (n as u8, 1),
            16 => (
                *lengths
                    .last()
                    .ok_or_else(|| "repeat code without previous length".to_string())?,
                usize::from(bits.read(2)?) + 3,
            ),
            17 => (0, usize::from(bits.read(3)?) + 3),
            18 => (0, usize::from(bits.read(7)?) + 11),
            _ => return Err("invalid code length symbol".to_string()),
        };
        let end = lengths.len() + count;
        if end > literals + distances {
            return Err("code lengths overflow".to_string());
        }
        lengths.resize(end, length);
    }
    if lengths[256] == 0 {
        return Err("missing end-of-block symbol".to_string());
    }
    let (lit_lens, dist_lens) = lengths.split_at(literals);
    let dist_tree = if dist_lens.iter().all(|&l| l == 0) {
        None
    } else {
        Some(Huffman::new(dist_lens, true)?)
    };
    Ok((Huffman::new(lit_lens, true)?, dist_tree))
}

const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const DISTANCE_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];

fn inflate_deflate(
    bits: &mut BitReader<'_>,
    out: &mut Vec<u8>,
    budget: usize,
) -> Result<(), String> {
    loop {
        let final_block = bits.read(1)? != 0;
        match bits.read(2)? {
            0 => {
                bits.align();
                let len = bits.read(16)?;
                let nlen = bits.read(16)?;
                if len != !nlen {
                    return Err("stored DEFLATE block length complement mismatch".to_string());
                }
                let raw = bits.take_bytes(usize::from(len))?;
                if raw.len() > budget.saturating_sub(out.len()) {
                    return Err("DEFLATE output exceeds byte budget".to_string());
                }
                out.extend_from_slice(raw);
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
                            if out.len() >= budget {
                                return Err("DEFLATE output exceeds byte budget".to_string());
                            }
                            out.push(byte as u8);
                        }
                        256 => break,
                        symbol @ 257..=285 => {
                            let idx = usize::from(symbol - 257);
                            let extra_len = if symbol < 285 {
                                idx.saturating_sub(4) / 4
                            } else {
                                0
                            };
                            let length =
                                usize::from(LENGTH_BASE[idx] + bits.read(extra_len as u8)?);
                            let dist_code = usize::from(
                                distances
                                    .as_ref()
                                    .ok_or_else(|| "missing distance tree".to_string())?
                                    .read(bits)?,
                            );
                            let base = *DISTANCE_BASE
                                .get(dist_code)
                                .ok_or_else(|| "invalid distance symbol".to_string())?;
                            let extra_dist = dist_code.saturating_sub(2) / 2;
                            let distance = usize::from(base + bits.read(extra_dist as u8)?);
                            if distance == 0 || distance > out.len() {
                                return Err("DEFLATE back-reference out of range".to_string());
                            }
                            if length > budget.saturating_sub(out.len()) {
                                return Err("DEFLATE output exceeds byte budget".to_string());
                            }
                            for _ in 0..length {
                                out.push(out[out.len() - distance]);
                            }
                        }
                        _ => return Err("invalid DEFLATE literal/length symbol".to_string()),
                    }
                }
            }
            _ => return Err("reserved DEFLATE block type 3".to_string()),
        }
        if final_block {
            return Ok(());
        }
    }
}

// ---------------------------------------------------------------------------
// Packet framing (uncompressed and compressed)
// ---------------------------------------------------------------------------

pub fn encode_packet(pid: i32, payload: &[u8], compression: Option<usize>) -> Vec<u8> {
    let mut raw = Vec::with_capacity(5 + payload.len());
    write_varint(&mut raw, pid);
    raw.extend_from_slice(payload);

    let mut frame = Vec::new();
    match compression {
        None => {
            write_varint(&mut frame, raw.len() as i32);
            frame.extend_from_slice(&raw);
        }
        Some(threshold) => {
            let mut body = Vec::new();
            if raw.len() < threshold {
                write_varint(&mut body, 0);
                body.extend_from_slice(&raw);
            } else {
                write_varint(&mut body, raw.len() as i32);
                body.extend_from_slice(&zlib_compress_stored(&raw));
            }
            write_varint(&mut frame, body.len() as i32);
            frame.extend_from_slice(&body);
        }
    }
    frame
}

pub fn decode_packet_body(
    body: &[u8],
    compression: Option<usize>,
) -> Result<(i32, Vec<u8>), String> {
    let uncompressed = match compression {
        None => body.to_vec(),
        Some(_threshold) => {
            let mut r = WireReader::new(body);
            let data_len = usize::try_from(r.read_varint()?)
                .map_err(|_| "negative compressed data length".to_string())?;
            if data_len == 0 {
                if r.remaining() > MAX_PACKET_SIZE {
                    return Err("uncompressed frame exceeds max packet size".to_string());
                }
                r.rest().to_vec()
            } else {
                if data_len > MAX_PACKET_SIZE {
                    return Err("compressed packet exceeds max packet size".to_string());
                }
                let out = zlib_decompress(r.rest(), data_len)?;
                if out.len() != data_len {
                    return Err(format!(
                        "zlib decoded length {} does not match header {}",
                        out.len(),
                        data_len
                    ));
                }
                out
            }
        }
    };
    let mut r = WireReader::new(&uncompressed);
    let pid = r.read_varint()?;
    let payload = r.rest().to_vec();
    Ok((pid, payload))
}

// ---------------------------------------------------------------------------
// Paletted container & Chunk / Light decoders
// ---------------------------------------------------------------------------

pub fn decode_paletted_container(
    reader: &mut WireReader<'_>,
    entry_count: usize,
    indirect_max: u8,
    min_bits: u8,
    direct_bits: u8,
) -> Result<Vec<u32>, String> {
    let bits = reader.read_u8()?;
    if bits == 0 {
        let single = u32::try_from(reader.read_varint()?)
            .map_err(|_| "negative singleton palette id".to_string())?;
        let data_len = usize::try_from(reader.read_varint()?)
            .map_err(|_| "negative singleton long count".to_string())?;
        if data_len != 0 {
            let _ = reader.take(data_len * 8)?;
        }
        return Ok(vec![single; entry_count]);
    }

    let palette = if bits <= indirect_max {
        if bits < min_bits {
            return Err(format!("palette bits {bits} below minimum {min_bits}"));
        }
        let plen = usize::try_from(reader.read_varint()?)
            .map_err(|_| "negative palette length".to_string())?;
        let mut values = Vec::with_capacity(plen);
        for _ in 0..plen {
            values.push(
                u32::try_from(reader.read_varint()?)
                    .map_err(|_| "negative palette entry".to_string())?,
            );
        }
        Some(values)
    } else {
        if bits != direct_bits && bits > 32 {
            return Err(format!("invalid direct palette width {bits}"));
        }
        None
    };

    let data_len = usize::try_from(reader.read_varint()?)
        .map_err(|_| "negative palette data long count".to_string())?;
    let per_word = 64 / usize::from(bits);
    let expected_len = entry_count.div_ceil(per_word);
    if data_len != expected_len {
        return Err(format!(
            "palette long count {data_len} != expected {expected_len} for {bits} bits"
        ));
    }

    let mask = (1u64 << bits) - 1;
    let mut words = Vec::with_capacity(data_len);
    for _ in 0..data_len {
        words.push(reader.read_u64()?);
    }

    let mut out = Vec::with_capacity(entry_count);
    for i in 0..entry_count {
        let word = words[i / per_word];
        let shift = (i % per_word) * usize::from(bits);
        let raw = ((word >> shift) & mask) as u32;
        let state = match &palette {
            Some(pal) => *pal
                .get(raw as usize)
                .ok_or_else(|| format!("palette index {raw} out of range {}", pal.len()))?,
            None => raw,
        };
        out.push(state);
    }
    Ok(out)
}

#[derive(Clone, Debug)]
pub struct DecodedSection {
    pub non_air: i16,
    pub states: Box<[u32; 4096]>,
    pub biomes: [u32; 64],
}

#[derive(Clone, Debug)]
pub struct DecodedLightData {
    pub sky_mask: Vec<u64>,
    pub block_mask: Vec<u64>,
    pub empty_sky_mask: Vec<u64>,
    pub empty_block_mask: Vec<u64>,
    pub sky_arrays: Vec<[u8; 2048]>,
    pub block_arrays: Vec<[u8; 2048]>,
}

#[derive(Clone, Debug)]
pub struct DecodedChunk {
    pub cx: i32,
    pub cz: i32,
    pub sections: Vec<DecodedSection>,
    pub light: DecodedLightData,
}

fn decode_light_section_payload(
    reader: &mut WireReader<'_>,
    light_section_count: usize,
    byte_array_masks: bool,
) -> Result<DecodedLightData, String> {
    let read_mask = |r: &mut WireReader<'_>| -> Result<Vec<u64>, String> {
        if byte_array_masks {
            r.read_bitset_bytes(light_section_count.div_ceil(8))
        } else {
            r.read_bitset(light_section_count.div_ceil(64))
        }
    };
    let sky_mask = read_mask(reader)?;
    let block_mask = read_mask(reader)?;
    let empty_sky_mask = read_mask(reader)?;
    let empty_block_mask = read_mask(reader)?;

    let sky_count = usize::try_from(reader.read_varint()?)
        .map_err(|_| "negative sky light array count".to_string())?;
    if sky_count != bitset_count_ones(&sky_mask) {
        return Err(format!(
            "sky light array count {sky_count} != mask popcount {}",
            bitset_count_ones(&sky_mask)
        ));
    }
    let mut sky_arrays = Vec::with_capacity(sky_count);
    for _ in 0..sky_count {
        let len = reader.read_varint()?;
        if len != 2048 {
            return Err(format!("expected 2048-byte sky light array, got {len}"));
        }
        let mut arr = [0u8; 2048];
        arr.copy_from_slice(reader.take(2048)?);
        sky_arrays.push(arr);
    }

    let block_count = usize::try_from(reader.read_varint()?)
        .map_err(|_| "negative block light array count".to_string())?;
    if block_count != bitset_count_ones(&block_mask) {
        return Err(format!(
            "block light array count {block_count} != mask popcount {}",
            bitset_count_ones(&block_mask)
        ));
    }
    let mut block_arrays = Vec::with_capacity(block_count);
    for _ in 0..block_count {
        let len = reader.read_varint()?;
        if len != 2048 {
            return Err(format!("expected 2048-byte block light array, got {len}"));
        }
        let mut arr = [0u8; 2048];
        arr.copy_from_slice(reader.take(2048)?);
        block_arrays.push(arr);
    }

    Ok(DecodedLightData {
        sky_mask,
        block_mask,
        empty_sky_mask,
        empty_block_mask,
        sky_arrays,
        block_arrays,
    })
}

/// Version-sensitive knobs for `level_chunk_with_light` decoding: the
/// block-state bound a version pack declares (`registry.state_count()`),
/// whether the heightmaps field is the 1.21.5+ array-of-`Heightmap`-records
/// form (`false` is the 767 single-NBT-compound form), and whether the four
/// light masks are the 1.21.5+ `java.util.BitSet` byte-array form (`false`
/// is the 767 `VarInt` + `u64` word form).
#[derive(Clone, Copy, Debug)]
pub struct ChunkDecodeProfile {
    pub state_count: usize,
    pub heightmaps_are_records: bool,
    pub byte_array_light_masks: bool,
}

impl ChunkDecodeProfile {
    /// The pinned 1.21.1 profile: kernel `STATE_COUNT` + NBT heightmaps +
    /// long-array light masks.
    pub fn v767() -> Self {
        Self {
            state_count: mc_spine_kernel::STATE_COUNT as usize,
            heightmaps_are_records: false,
            byte_array_light_masks: false,
        }
    }
}

pub fn decode_chunk_packet(
    body: &[u8],
    section_count: usize,
    biome_count: usize,
) -> Result<DecodedChunk, String> {
    decode_chunk_packet_profiled(body, section_count, biome_count, ChunkDecodeProfile::v767())
}

pub fn decode_chunk_packet_profiled(
    body: &[u8],
    section_count: usize,
    biome_count: usize,
    profile: ChunkDecodeProfile,
) -> Result<DecodedChunk, String> {
    let mut reader = WireReader::new(body);
    let cx = reader.read_i32()?;
    let cz = reader.read_i32()?;
    if profile.heightmaps_are_records {
        // 1.21.5+ form: VarInt count of {type VarInt, packed long array}.
        let count = usize::try_from(reader.read_varint()?)
            .map_err(|_| "negative heightmap count".to_string())?;
        for _ in 0..count {
            let _hm_type = reader.read_varint()?;
            let longs = usize::try_from(reader.read_varint()?)
                .map_err(|_| "negative heightmap long count".to_string())?;
            let _ = reader.take(longs * 8)?;
        }
    } else {
        let _heightmaps = reader.read_nbt()?;
    }

    let data_len = usize::try_from(reader.read_varint()?)
        .map_err(|_| "negative chunk data length".to_string())?;
    let section_bytes = reader.take(data_len)?;
    let mut sec_reader = WireReader::new(section_bytes);

    let biome_direct_bits = if biome_count <= 1 {
        0
    } else {
        (usize::BITS - (biome_count - 1).leading_zeros()) as u8
    };
    let state_bound = u32::try_from(profile.state_count.saturating_sub(1)).unwrap_or(u32::MAX);
    let state_direct_bits = (u32::BITS - state_bound.leading_zeros()) as u8;

    let mut sections = Vec::with_capacity(section_count);
    for _ in 0..section_count {
        let non_air = sec_reader.read_i16()?;
        let states_vec = decode_paletted_container(&mut sec_reader, 4096, 8, 4, state_direct_bits)?;
        let biomes_vec = decode_paletted_container(&mut sec_reader, 64, 3, 1, biome_direct_bits)?;
        let mut states = Box::new([0u32; 4096]);
        states.copy_from_slice(&states_vec);
        let mut biomes = [0u32; 64];
        biomes.copy_from_slice(&biomes_vec);
        sections.push(DecodedSection {
            non_air,
            states,
            biomes,
        });
    }
    if sec_reader.remaining() != 0 {
        return Err(format!(
            "unparsed {} bytes in chunk section payload",
            sec_reader.remaining()
        ));
    }

    let block_entity_count = usize::try_from(reader.read_varint()?)
        .map_err(|_| "negative block entity count".to_string())?;
    for _ in 0..block_entity_count {
        let _packed_xz = reader.read_u8()?;
        let _y = reader.read_i16()?;
        let _kind = reader.read_varint()?;
        let _nbt = reader.read_nbt()?;
    }

    let light = decode_light_section_payload(
        &mut reader,
        section_count + 2,
        profile.byte_array_light_masks,
    )?;
    Ok(DecodedChunk {
        cx,
        cz,
        sections,
        light,
    })
}

pub fn decode_update_light_packet(
    body: &[u8],
    section_count: usize,
    byte_array_masks: bool,
) -> Result<(i32, i32, DecodedLightData), String> {
    let mut reader = WireReader::new(body);
    let cx = reader.read_varint()?;
    let cz = reader.read_varint()?;
    let light = decode_light_section_payload(&mut reader, section_count + 2, byte_array_masks)?;
    Ok((cx, cz, light))
}

pub fn decode_section_blocks_update(body: &[u8]) -> Result<Vec<([i32; 3], u32)>, String> {
    let mut reader = WireReader::new(body);
    let section_pos = reader.read_i64()?;
    let sx = (section_pos >> 42) as i32;
    let sz = ((section_pos << 22) >> 42) as i32;
    let sy = ((section_pos << 44) >> 44) as i32;

    let count = usize::try_from(reader.read_varint()?)
        .map_err(|_| "negative section update count".to_string())?;
    let mut updates = Vec::with_capacity(count);
    for _ in 0..count {
        let record = reader.read_varlong()? as u64;
        let state = (record >> 12) as u32;
        let lx = ((record >> 8) & 0x0f) as i32;
        let lz = ((record >> 4) & 0x0f) as i32;
        let ly = (record & 0x0f) as i32;
        updates.push(([sx * 16 + lx, sy * 16 + ly, sz * 16 + lz], state));
    }
    Ok(updates)
}

// ---------------------------------------------------------------------------
// Dimension metadata & Play events
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DimensionMeta {
    pub name: String,
    pub min_y: i32,
    pub height: i32,
    pub has_skylight: bool,
}

impl Default for DimensionMeta {
    fn default() -> Self {
        Self {
            name: "minecraft:overworld".to_string(),
            min_y: -64,
            height: 384,
            has_skylight: true,
        }
    }
}

impl DimensionMeta {
    pub fn section_count(&self) -> usize {
        (self.height / 16).max(1) as usize
    }

    pub fn min_section_y(&self) -> i32 {
        self.min_y.div_euclid(16)
    }

    fn builtin(name: &str) -> Option<(i32, i32, bool)> {
        match name {
            "minecraft:overworld" | "minecraft:overworld_caves" => Some((-64, 384, true)),
            "minecraft:the_nether" | "minecraft:the_end" => Some((0, 256, false)),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct TeleportSync {
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub yaw: f32,
    pub pitch: f32,
    /// Relative-flag bitmask — a `u8` on the 767 wire but a full `Int` on
    /// 26.x (which adds `DELTA_*`/`ROTATE_DELTA` bits above `0x10`).
    pub flags: i32,
    pub teleport_id: i32,
}

#[derive(Clone, Debug)]
pub enum PlayEvent {
    JoinGame {
        entity_id: i32,
        dimension: DimensionMeta,
    },
    Respawn {
        dimension: DimensionMeta,
    },
    KeepAlive(i64),
    Teleport(TeleportSync),
    ChunkBatchStart,
    ChunkData(Box<DecodedChunk>),
    ChunkBatchFinished(i32),
    UpdateLight {
        cx: i32,
        cz: i32,
        light: DecodedLightData,
    },
    BlockUpdate {
        pos: [i32; 3],
        state_id: u32,
    },
    SectionBlocksUpdate(Vec<([i32; 3], u32)>),
    UnloadChunk {
        cx: i32,
        cz: i32,
    },
    SetCenterChunk {
        cx: i32,
        cz: i32,
    },
    SetHealth {
        health: f32,
        food: i32,
        saturation: f32,
    },
    /// `system_chat` — extracted plain text plus the overlay (action-bar)
    /// flag; the NBT component itself stays in `protocol`'s decoder.
    SystemChat { text: String, overlay: bool },
    Disconnect(String),
    Other(i32),
}

// ---------------------------------------------------------------------------
// TCP Protocol 767 Client
// ---------------------------------------------------------------------------

pub struct HandshakeReport {
    pub login_ok: bool,
    pub config_ok: bool,
    pub compression_threshold: Option<usize>,
    pub uuid: [u8; 16],
    pub server_username: String,
    pub biome_count: usize,
}

pub struct ProtocolClient {
    stream: TcpStream,
    compression: Option<usize>,
    read_buf: Vec<u8>,
    /// Version-selected packet table (`ProtocolVersion::V1_21_1` or
    /// `ProtocolVersion::V26_3`) — every packet id below resolves through it
    /// by phase/direction/name instead of a hardcoded hex literal.
    codec: Arc<dyn ProtocolCodec>,
    dimension_types_by_id: HashMap<i32, (i32, i32, bool)>,
    dimension_types_by_name: HashMap<String, (i32, i32, bool)>,
    /// Upper bound on global block-state ids for `level_chunk_with_light`
    /// decoding (`mc_spine_kernel::STATE_COUNT` with the pinned assets);
    /// `sim` reseeds it from `WorldState::state_count` so `--version-dir`
    /// packs bound the global palette correctly.
    pub block_state_count: usize,
    pub dimension: DimensionMeta,
    pub biome_count: usize,
    /// The protocol version this client offered in its handshake
    /// (`V1_21_1`/`767` by default, `V26_3`/`777` via `--mc-version`).
    pub protocol_version: mc_spine_proto::ProtocolVersion,
    pub packets_rx: u64,
    pub packets_tx: u64,
}

impl ProtocolClient {
    /// True when the selected table carries the 26.x wire profile
    /// (`RecipeBookDisplays` is the proto's 26.x-generation marker): the
    /// post-1.21.5 login/play shapes — Game-Profile login success with a
    /// session id, VarInt game modes + `online_mode` in spawn info, echoed
    /// `accept_teleportation`, Int teleport flags with velocity fields, the
    /// heightmap record array, and the `player_input` flags byte.
    fn wire_26x(&self) -> bool {
        matches!(
            self.codec.capabilities().recipe_sync_mode,
            RecipeSyncMode::RecipeBookDisplays
        )
    }

    /// Resolve an inbound `pid` to its protocol-table name for the active
    /// version — packet ids differ between 767 and 777 (e.g. play
    /// `keep_alive` is `0x26` vs `0x2d`), so dispatch goes through names.
    fn s2c_name(&self, phase: PacketPhase, pid: i32) -> Option<String> {
        let id = u32::try_from(pid).ok()?;
        self.codec
            .table()
            .packet_name(phase, PacketDir::Clientbound, id)
            .map(str::to_string)
    }

    /// Resolve an outbound packet name to its id for the active version.
    fn c2s_id(&self, phase: PacketPhase, name: &'static str) -> Result<i32, String> {
        self.codec
            .table()
            .packet_id(phase, PacketDir::Serverbound, name)
            .map(|id| id as i32)
            .ok_or_else(|| {
                format!(
                    "protocol table for {:?} lacks c2s {phase:?}/{name}",
                    self.protocol_version
                )
            })
    }

    fn send_named(
        &mut self,
        phase: PacketPhase,
        name: &'static str,
        payload: &[u8],
    ) -> Result<(), String> {
        let pid = self.c2s_id(phase, name)?;
        self.send_raw(pid, payload)
    }

    /// Connect, handshake, and ride through Login + Configuration to Play.
    ///
    /// Returns `(Self, HandshakeReport)` with the negotiated profile on
    /// success, or a descriptive error on any wire/parse failure.
    pub fn connect(
        host: &str,
        port: u16,
        username: &str,
        uuid: [u8; 16],
        view_distance: i8,
        timeout: Duration,
    ) -> Result<(Self, HandshakeReport), String> {
        Self::connect_for_version(
            host,
            port,
            username,
            uuid,
            view_distance,
            timeout,
            mc_spine_proto::ProtocolVersion::V1_21_1,
        )
    }

    /// `connect` at an explicit `ProtocolVersion`: the live `_minecraft._tcp`
    /// SRV lookup resolves the socket address while the Handshake still
    /// carries the original `host`/`port` — the vanilla
    /// `ServerNameResolver` rule.
    pub fn connect_for_version(
        host: &str,
        port: u16,
        username: &str,
        uuid: [u8; 16],
        view_distance: i8,
        timeout: Duration,
        protocol_version: mc_spine_proto::ProtocolVersion,
    ) -> Result<(Self, HandshakeReport), String> {
        Self::connect_resolved(
            host,
            port,
            username,
            uuid,
            view_distance,
            timeout,
            protocol_version,
            |h, p| {
                mc_spine_proto::resolve_server_address(h, p).map_err(|e| e.to_string())
            },
        )
    }

    /// `connect_for_version` with the `_minecraft._tcp` SRV / address lookup
    /// injected, so loopback tests can prove both the SRV redirect and the
    /// original-host-preservation rule without live DNS.
    pub fn connect_resolved(
        host: &str,
        port: u16,
        username: &str,
        uuid: [u8; 16],
        view_distance: i8,
        timeout: Duration,
        protocol_version: mc_spine_proto::ProtocolVersion,
        resolve: impl FnOnce(&str, u16) -> Result<Vec<SocketAddr>, String>,
    ) -> Result<(Self, HandshakeReport), String> {
        let codec = mc_spine_proto::VersionRouter::with_v767_and_v777()
            .select(protocol_version.number())
            .map_err(|e| e.to_string())?;
        let addrs = resolve(host, port)?;
        if addrs.is_empty() {
            return Err(format!(
                "resolve_server_address({host}:{port}) yielded no addresses"
            ));
        }
        let addr = format!("{host}:{port}");
        let stream = TcpStream::connect(addrs.as_slice())
            .map_err(|e| format!("failed to connect to {addr}: {e}"))?;
        let _ = stream.set_nodelay(true);
        let _ = stream.set_read_timeout(Some(timeout));
        let _ = stream.set_write_timeout(Some(timeout));

        let mut client = Self {
            stream,
            compression: None,
            read_buf: Vec::new(),
            codec,
            dimension_types_by_id: HashMap::new(),
            dimension_types_by_name: HashMap::new(),
            block_state_count: mc_spine_kernel::STATE_COUNT as usize,
            dimension: DimensionMeta::default(),
            biome_count: 64,
            protocol_version,
            packets_rx: 0,
            packets_tx: 0,
        };

        // 1. Handshake — original host/port even when the SRV lookup
        //    redirected the TCP target (vanilla ServerNameResolver rule).
        let mut hs = Vec::new();
        write_varint(&mut hs, client.protocol_version.number());
        write_string(&mut hs, host);
        hs.extend_from_slice(&port.to_be_bytes());
        write_varint(&mut hs, 2);
        client.send_named(PacketPhase::Handshake, "handshake", &hs)?;

        // 2. Login Start
        let mut login = Vec::new();
        write_string(&mut login, username);
        login.extend_from_slice(&uuid);
        client.send_named(PacketPhase::Login, "login_start", &login)?;

        // 3. Login loop -> Set Compression / Login Success
        let deadline = Instant::now() + timeout;
        let (server_uuid, server_username) = loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(|| "login timed out".to_string())?;
            let Some((pid, body)) = client.recv_raw(remaining)? else {
                return Err("login timed out waiting for server".to_string());
            };
            match client.s2c_name(PacketPhase::Login, pid).as_deref() {
                Some("disconnect") | Some("login_disconnect") => {
                    let msg = WireReader::new(&body)
                        .read_string(32_767)
                        .unwrap_or_else(|_| "login disconnect".to_string());
                    return Err(format!("disconnected during login: {msg}"));
                }
                Some("encryption_request") => {
                    return Err(
                        "server requested online-mode encryption; offline-mode server required"
                            .to_string(),
                    );
                }
                Some("set_compression") | Some("login_compression") => {
                    let threshold = WireReader::new(&body).read_varint()?;
                    client.compression = usize::try_from(threshold).ok();
                }
                Some("login_success") | Some("login_finished") => {
                    let mut r = WireReader::new(&body);
                    let got_uuid = r.read_uuid()?;
                    let got_name = r.read_string(64)?;
                    if client.wire_26x() {
                        // 26.x Game Profile properties (name, value, optional
                        // signature) then a Session ID UUID.
                        let props = usize::try_from(r.read_varint()?)
                            .map_err(|_| "negative profile property count".to_string())?;
                        for _ in 0..props {
                            let _pname = r.read_string(1024)?;
                            let _pvalue = r.read_string(32_767)?;
                            if r.read_bool()? {
                                let _sig = r.read_string(1024)?;
                            }
                        }
                        let _session_id = r.read_uuid()?;
                    }
                    client.send_named(PacketPhase::Login, "login_acknowledged", &[])?;
                    break (got_uuid, got_name);
                }
                _ => {}
            }
        };

        // 4. Configuration loop
        let mut sent_client_info = false;
        let client_info_payload =
            build_client_information(view_distance, client.codec.capabilities());
        let config_deadline = Instant::now() + timeout;
        loop {
            let remaining = config_deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(|| "configuration timed out".to_string())?;
            let Some((pid, body)) = client.recv_raw(remaining)? else {
                return Err("configuration timed out waiting for server".to_string());
            };
            match client.s2c_name(PacketPhase::Configuration, pid).as_deref() {
                // Cookie Request — or the legacy 8-byte keepalive fallback on
                // pid 0x00 in the 767 table.
                Some("cookie_request") => {
                    let mut r = WireReader::new(&body);
                    if let Ok(key) = r.read_string(1024) {
                        if r.remaining() == 0 {
                            let mut resp = Vec::new();
                            write_string(&mut resp, &key);
                            resp.push(0); // has_payload = false
                            client.send_named(PacketPhase::Configuration, "cookie_response", &resp)?;
                            continue;
                        }
                    }
                    if body.len() == 8 {
                        client.send_named(PacketPhase::Configuration, "keep_alive", &body)?;
                    }
                }
                Some("keep_alive") => {
                    client.send_named(PacketPhase::Configuration, "keep_alive", &body)?;
                }
                Some("disconnect") => {
                    let msg = WireReader::new(&body)
                        .read_nbt()
                        .map(|v| v.extract_text())
                        .unwrap_or_else(|_| "configuration disconnect".to_string());
                    return Err(format!("disconnected during configuration: {msg}"));
                }
                Some("registry_data") => {
                    client.parse_registry_data(&body)?;
                }
                Some("select_known_packs") => {
                    if !sent_client_info {
                        client.send_named(
                            PacketPhase::Configuration,
                            "client_information",
                            &client_info_payload,
                        )?;
                        sent_client_info = true;
                    }
                    let mut packs = Vec::new();
                    write_varint(&mut packs, 0);
                    client.send_named(PacketPhase::Configuration, "select_known_packs", &packs)?;
                }
                Some("finish_configuration") => {
                    if !sent_client_info {
                        client.send_named(
                            PacketPhase::Configuration,
                            "client_information",
                            &client_info_payload,
                        )?;
                    }
                    client.send_named(PacketPhase::Configuration, "finish_configuration", &[])?;
                    break;
                }
                _ => {}
            }
        }

        let report = HandshakeReport {
            login_ok: true,
            config_ok: true,
            compression_threshold: client.compression,
            uuid: server_uuid,
            server_username,
            biome_count: client.biome_count,
        };
        Ok((client, report))
    }

    fn parse_registry_data(&mut self, body: &[u8]) -> Result<(), String> {
        let mut r = WireReader::new(body);
        let registry_name = r.read_string(256)?;
        let count = usize::try_from(r.read_varint()?)
            .map_err(|_| "negative registry entry count".to_string())?;
        if count > 65_536 {
            return Err("registry entry count exceeds bound".to_string());
        }
        if registry_name == "minecraft:worldgen/biome" {
            self.biome_count = count.max(1);
        }
        if registry_name == "minecraft:dimension_type" {
            self.dimension_types_by_id.clear();
            self.dimension_types_by_name.clear();
        }
        for idx in 0..count {
            let entry_name = r.read_string(256)?;
            let has_nbt = r.read_bool()?;
            let entry_nbt = if has_nbt { Some(r.read_nbt()?) } else { None };
            if registry_name == "minecraft:dimension_type" {
                let meta = entry_nbt
                    .as_ref()
                    .and_then(|v| match v {
                        NbtValue::Compound(map) => {
                            let min_y = map.get("min_y")?.as_i32()?;
                            let height = map.get("height")?.as_i32()?;
                            let has_skylight = map.get("has_skylight")?.as_bool()?;
                            Some((min_y, height, has_skylight))
                        }
                        _ => None,
                    })
                    .or_else(|| DimensionMeta::builtin(&entry_name));
                if let Some(m) = meta {
                    self.dimension_types_by_id.insert(idx as i32, m);
                    self.dimension_types_by_name.insert(entry_name, m);
                }
            }
        }
        Ok(())
    }

    /// Shared `login`/`respawn` tail: dimension + seed + game modes + flags
    /// + death location + portal cooldown (+ `sea_level` from 1.21.2). The
    /// 26.x profile switches the two game modes to VarInt enums.
    fn parse_spawn_info(&self, r: &mut WireReader<'_>) -> Result<(i32, String), String> {
        let dim_type_id = r.read_varint()?;
        let dim_name = r.read_string(256)?;
        let _hashed_seed = r.read_i64()?;
        if self.wire_26x() {
            let _game_mode = r.read_varint()?;
            let _prev_game_mode = r.read_varint()?; // 0 = undefined, else id+1
        } else {
            let _game_mode = r.read_u8()?;
            let _prev_game_mode = r.read_u8()?;
        }
        let _is_debug = r.read_bool()?;
        let _is_flat = r.read_bool()?;
        if r.read_bool()? {
            let _death_dim = r.read_string(256)?;
            let _death_pos = r.read_i64()?;
        }
        let _portal_cooldown = r.read_varint()?;
        if self.codec.capabilities().relative_movement_flags {
            let _sea_level = r.read_varint()?;
        }
        Ok((dim_type_id, dim_name))
    }

    fn resolve_dimension(&self, dim_type_id: i32, dim_name: String) -> DimensionMeta {
        let (min_y, height, has_skylight) = self
            .dimension_types_by_id
            .get(&dim_type_id)
            .copied()
            .or_else(|| self.dimension_types_by_name.get(&dim_name).copied())
            .or_else(|| DimensionMeta::builtin(&dim_name))
            .unwrap_or((-64, 384, true));
        DimensionMeta {
            name: dim_name,
            min_y,
            height,
            has_skylight,
        }
    }

    pub fn send_raw(&mut self, pid: i32, payload: &[u8]) -> Result<(), String> {
        let frame = encode_packet(pid, payload, self.compression);
        self.stream
            .write_all(&frame)
            .map_err(|e| format!("TCP write failed: {e}"))?;
        self.packets_tx += 1;
        Ok(())
    }

    /// Bare `accept_teleportation` (teleport id only) — the 767 wire form.
    /// 26.x requires the echoed resulting pose, so version-aware callers use
    /// `send_accept_teleportation` instead.
    pub fn send_confirm_teleport(&mut self, teleport_id: i32) -> Result<(), String> {
        let mut p = Vec::with_capacity(5);
        write_varint(&mut p, teleport_id);
        self.send_named(PacketPhase::Play, "accept_teleportation", &p)
    }

    /// Confirm a teleport in the active version's wire shape: 767 sends the
    /// bare teleport id; 26.x echoes the resulting pose and, like vanilla,
    /// immediately follows with `move_player_pos_rot`.
    pub fn send_accept_teleportation(
        &mut self,
        teleport_id: i32,
        pos: [f64; 3],
        yaw: f32,
        pitch: f32,
        on_ground: bool,
    ) -> Result<(), String> {
        let mut p = Vec::with_capacity(33);
        write_varint(&mut p, teleport_id);
        if !self.wire_26x() {
            return self.send_named(PacketPhase::Play, "accept_teleportation", &p);
        }
        p.extend_from_slice(&pos[0].to_be_bytes());
        p.extend_from_slice(&pos[1].to_be_bytes());
        p.extend_from_slice(&pos[2].to_be_bytes());
        p.extend_from_slice(&yaw.to_be_bytes());
        p.extend_from_slice(&pitch.to_be_bytes());
        self.send_named(PacketPhase::Play, "accept_teleportation", &p)?;
        self.send_player_position_rotation(pos[0], pos[1], pos[2], yaw, pitch, on_ground)
    }

    pub fn send_chunk_batch_received(&mut self, chunks_per_tick: f32) -> Result<(), String> {
        self.send_named(
            PacketPhase::Play,
            "chunk_batch_received",
            &chunks_per_tick.to_be_bytes(),
        )
    }

    pub fn send_keep_alive(&mut self, id: i64) -> Result<(), String> {
        self.send_named(PacketPhase::Play, "keep_alive", &id.to_be_bytes())
    }

    /// `player_input` in the active version's shape: 767's float impulse +
    /// jump/unmount flag byte; 26.x's single `Input` flag byte
    /// (`caps.player_input_flags`). Flag bits match vanilla `Input`.
    pub fn send_player_input(
        &mut self,
        sideways: f32,
        forward: f32,
        jump: bool,
        unmount: bool,
    ) -> Result<(), String> {
        let mut p = Vec::with_capacity(9);
        if self.codec.capabilities().player_input_flags {
            let mut bits = 0u8;
            if forward > 0.0 {
                bits |= 0x01;
            }
            if forward < 0.0 {
                bits |= 0x02;
            }
            if sideways < 0.0 {
                bits |= 0x04;
            }
            if sideways > 0.0 {
                bits |= 0x08;
            }
            if jump {
                bits |= 0x10;
            }
            if unmount {
                bits |= 0x20;
            }
            p.push(bits);
        } else {
            p.extend_from_slice(&sideways.to_be_bytes());
            p.extend_from_slice(&forward.to_be_bytes());
            p.push(u8::from(jump) | (u8::from(unmount) << 1));
        }
        self.send_named(PacketPhase::Play, "player_input", &p)
    }

    pub fn send_player_position(
        &mut self,
        x: f64,
        y: f64,
        z: f64,
        on_ground: bool,
    ) -> Result<(), String> {
        // 767 ends with a bare on-ground boolean, 26.x with a flag byte whose
        // 0x01 bit is on-ground — the encoding is identical for a client that
        // never reports horizontal collision.
        let mut p = Vec::with_capacity(25);
        p.extend_from_slice(&x.to_be_bytes());
        p.extend_from_slice(&y.to_be_bytes());
        p.extend_from_slice(&z.to_be_bytes());
        p.push(u8::from(on_ground));
        self.send_named(PacketPhase::Play, "move_player_pos", &p)
    }

    pub fn send_player_position_rotation(
        &mut self,
        x: f64,
        y: f64,
        z: f64,
        yaw: f32,
        pitch: f32,
        on_ground: bool,
    ) -> Result<(), String> {
        let mut p = Vec::with_capacity(33);
        p.extend_from_slice(&x.to_be_bytes());
        p.extend_from_slice(&y.to_be_bytes());
        p.extend_from_slice(&z.to_be_bytes());
        p.extend_from_slice(&yaw.to_be_bytes());
        p.extend_from_slice(&pitch.to_be_bytes());
        p.push(u8::from(on_ground));
        self.send_named(PacketPhase::Play, "move_player_pos_rot", &p)
    }

    pub fn recv_play(&mut self, timeout: Duration) -> Result<Option<PlayEvent>, String> {
        let Some((pid, body)) = self.recv_raw(timeout)? else {
            return Ok(None);
        };
        self.decode_play_event(pid, &body).map(Some)
    }

    fn decode_play_event(&mut self, pid: i32, body: &[u8]) -> Result<PlayEvent, String> {
        match self.s2c_name(PacketPhase::Play, pid).as_deref() {
            Some("login") => {
                let mut r = WireReader::new(body);
                let entity_id = r.read_i32()?;
                let _is_hardcore = r.read_bool()?;
                let dim_count = usize::try_from(r.read_varint()?)
                    .map_err(|_| "negative dimension count".to_string())?;
                for _ in 0..dim_count {
                    let _ = r.read_string(256)?;
                }
                let _max_players = r.read_varint()?;
                let _view_distance = r.read_varint()?;
                let _sim_distance = r.read_varint()?;
                let _reduced_debug = r.read_bool()?;
                let _respawn_screen = r.read_bool()?;
                let _limited_crafting = r.read_bool()?;
                let (dim_type_id, dim_name) = self.parse_spawn_info(&mut r)?;
                if self.wire_26x() {
                    let _online_mode = r.read_bool()?;
                }
                let _enforces_secure_chat = r.read_bool()?;
                let dimension = self.resolve_dimension(dim_type_id, dim_name);
                self.dimension = dimension.clone();
                Ok(PlayEvent::JoinGame {
                    entity_id,
                    dimension,
                })
            }
            Some("respawn") => {
                let mut r = WireReader::new(body);
                let (dim_type_id, dim_name) = self.parse_spawn_info(&mut r)?;
                let _data_kept = r.read_u8()?;
                let dimension = self.resolve_dimension(dim_type_id, dim_name);
                self.dimension = dimension.clone();
                Ok(PlayEvent::Respawn { dimension })
            }
            Some("keep_alive") => {
                let id = WireReader::new(body).read_i64()?;
                Ok(PlayEvent::KeepAlive(id))
            }
            Some("player_position") => {
                let mut r = WireReader::new(body);
                let (teleport_id, x, y, z, yaw, pitch, flags) = if self.wire_26x() {
                    // 26.x: teleport id first, then position/velocity doubles,
                    // yaw/pitch and an Int relative-flag bitmask.
                    let teleport_id = r.read_varint()?;
                    let x = r.read_f64()?;
                    let y = r.read_f64()?;
                    let z = r.read_f64()?;
                    let _vx = r.read_f64()?;
                    let _vy = r.read_f64()?;
                    let _vz = r.read_f64()?;
                    let yaw = r.read_f32()?;
                    let pitch = r.read_f32()?;
                    let flags = r.read_i32()?;
                    (teleport_id, x, y, z, yaw, pitch, flags)
                } else {
                    let x = r.read_f64()?;
                    let y = r.read_f64()?;
                    let z = r.read_f64()?;
                    let yaw = r.read_f32()?;
                    let pitch = r.read_f32()?;
                    let flags = i32::from(r.read_u8()?);
                    let teleport_id = r.read_varint()?;
                    (teleport_id, x, y, z, yaw, pitch, flags)
                };
                Ok(PlayEvent::Teleport(TeleportSync {
                    x,
                    y,
                    z,
                    yaw,
                    pitch,
                    flags,
                    teleport_id,
                }))
            }
            Some("chunk_batch_start") => Ok(PlayEvent::ChunkBatchStart),
            Some("level_chunk_with_light") => {
                let chunk = decode_chunk_packet_profiled(
                    body,
                    self.dimension.section_count(),
                    self.biome_count,
                    ChunkDecodeProfile {
                        state_count: self.block_state_count,
                        heightmaps_are_records: self.wire_26x(),
                        byte_array_light_masks: self.wire_26x(),
                    },
                )?;
                Ok(PlayEvent::ChunkData(Box::new(chunk)))
            }
            Some("chunk_batch_finished") => {
                let batch_size = WireReader::new(body).read_varint()?;
                Ok(PlayEvent::ChunkBatchFinished(batch_size))
            }
            Some("light_update") => {
                let (cx, cz, light) = decode_update_light_packet(
                    body,
                    self.dimension.section_count(),
                    self.wire_26x(),
                )?;
                Ok(PlayEvent::UpdateLight { cx, cz, light })
            }
            Some("block_update") => {
                let mut r = WireReader::new(body);
                let pos = r.read_position()?;
                let state_id = u32::try_from(r.read_varint()?)
                    .map_err(|_| "negative block state id".to_string())?;
                Ok(PlayEvent::BlockUpdate { pos, state_id })
            }
            Some("section_blocks_update") => {
                let updates = decode_section_blocks_update(body)?;
                Ok(PlayEvent::SectionBlocksUpdate(updates))
            }
            Some("forget_level_chunk") => {
                let mut r = WireReader::new(body);
                // ChunkPos is a big-endian i64: cz in high 32 bits, cx in low 32 bits.
                let cz = r.read_i32()?;
                let cx = r.read_i32()?;
                Ok(PlayEvent::UnloadChunk { cx, cz })
            }
            Some("set_chunk_cache_center") => {
                let mut r = WireReader::new(body);
                let cx = r.read_varint()?;
                let cz = r.read_varint()?;
                Ok(PlayEvent::SetCenterChunk { cx, cz })
            }
            Some("set_health") => {
                let mut r = WireReader::new(body);
                let health = r.read_f32()?;
                let food = r.read_varint()?;
                let saturation = r.read_f32()?;
                Ok(PlayEvent::SetHealth {
                    health,
                    food,
                    saturation,
                })
            }
            Some("system_chat") => {
                let mut r = WireReader::new(body);
                let text = r
                    .read_nbt()
                    .map(|v| v.extract_text())
                    .unwrap_or_default();
                let overlay = r.read_bool()?;
                Ok(PlayEvent::SystemChat { text, overlay })
            }
            Some("disconnect") => {
                let msg = WireReader::new(body)
                    .read_nbt()
                    .map(|v| v.extract_text())
                    .unwrap_or_else(|_| "disconnected by server".to_string());
                Ok(PlayEvent::Disconnect(msg))
            }
            _ => Ok(PlayEvent::Other(pid)),
        }
    }

    fn recv_raw(&mut self, timeout: Duration) -> Result<Option<(i32, Vec<u8>)>, String> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(frame) = self.try_extract_frame()? {
                self.packets_rx += 1;
                return decode_packet_body(&frame, self.compression).map(Some);
            }
            let remaining = match deadline.checked_duration_since(Instant::now()) {
                Some(rem) if !rem.is_zero() => rem,
                _ => return Ok(None),
            };
            let _ = self.stream.set_read_timeout(Some(remaining));
            let mut tmp = [0u8; 8192];
            match self.stream.read(&mut tmp) {
                Ok(0) => return Err("connection closed by peer".to_string()),
                Ok(n) => self.read_buf.extend_from_slice(&tmp[..n]),
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    return Ok(None);
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(format!("TCP read error: {e}")),
            }
        }
    }

    fn try_extract_frame(&mut self) -> Result<Option<Vec<u8>>, String> {
        if self.read_buf.is_empty() {
            return Ok(None);
        }
        let mut value = 0u32;
        for (i, &byte) in self.read_buf.iter().take(5).enumerate() {
            if i == 4 && byte > 0x0f {
                return Err("packet length varint overflow".to_string());
            }
            value |= u32::from(byte & 0x7f) << (7 * i);
            let header_len = i + 1;
            if byte & 0x80 == 0 {
                let packet_len = value as usize;
                if packet_len > MAX_PACKET_SIZE {
                    return Err(format!("packet length {packet_len} exceeds limit"));
                }
                let total = header_len + packet_len;
                if self.read_buf.len() < total {
                    return Ok(None);
                }
                let frame = self.read_buf[header_len..total].to_vec();
                self.read_buf.drain(..total);
                return Ok(Some(frame));
            }
        }
        if self.read_buf.len() >= 5 {
            return Err("packet length varint exceeds 5 bytes".to_string());
        }
        Ok(None)
    }
}

fn build_client_information(
    view_distance: i8,
    caps: &mc_spine_proto::VersionCapabilities,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(18);
    write_string(&mut out, "en_us");
    out.push(view_distance as u8);
    write_varint(&mut out, 0); // chat_mode: enabled
    out.push(1); // chat_colors: true
    out.push(0x7f); // skin_parts: all
    write_varint(&mut out, 1); // main_hand: right
    out.push(0); // enable_text_filtering: false
    out.push(1); // allow_server_listings: true
    if caps.split_particle_status {
        write_varint(&mut out, 0); // particle_status: all (1.21.2+)
    }
    out
}
