// SPDX-License-Identifier: LGPL-3.0-or-later
//! Strict Minecraft 1.21.1 protocol VarInts, VarLongs, and UTF-8 strings.

use std::fmt;

/// Malformed or truncated Minecraft variable-length data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VarIntError {
    /// Input ended before the integer or string was complete.
    UnexpectedEof,
    /// More than five VarInt or ten VarLong bytes were required, or the top
    /// payload bits overflowed the target integer.
    TooLong,
    /// String length exceeded its caller-supplied cap.
    StringTooLong { len: usize, max: usize },
    /// String payload was not valid UTF-8.
    InvalidUtf8,
    /// A length-prefixed string used a negative VarInt length.
    NegativeLength(i32),
}

impl fmt::Display for VarIntError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedEof => f.write_str("unexpected end of VarInt data"),
            Self::TooLong => f.write_str("VarInt encoding is too long or overflows"),
            Self::StringTooLong { len, max } => write!(f, "string length {len} exceeds cap {max}"),
            Self::InvalidUtf8 => f.write_str("string payload is not valid UTF-8"),
            Self::NegativeLength(len) => write!(f, "negative string length {len}"),
        }
    }
}
impl std::error::Error for VarIntError {}

/// Decode a strict, canonical protocol VarInt.
pub fn read_varint_i32(input: &[u8]) -> Result<(i32, usize), VarIntError> {
    let mut value = 0u32;
    for index in 0..5 {
        let byte = *input.get(index).ok_or(VarIntError::UnexpectedEof)?;
        if index == 4 && byte & 0xf0 != 0 {
            return Err(VarIntError::TooLong);
        }
        value |= u32::from(byte & 0x7f) << (index * 7);
        if byte & 0x80 == 0 {
            let len = index + 1;
            if varint_i32_len(value as i32) != len {
                return Err(VarIntError::TooLong);
            }
            return Ok((value as i32, len));
        }
    }
    Err(VarIntError::TooLong)
}

/// Append a protocol VarInt.
pub fn write_varint_i32(value: i32, out: &mut Vec<u8>) {
    let (bytes, len) = encode_varint_i32(value);
    out.extend_from_slice(&bytes[..len]);
}

/// Encode a protocol VarInt into its maximum-sized stack buffer.
pub fn encode_varint_i32(value: i32) -> ([u8; 5], usize) {
    let mut out = [0; 5];
    let mut val = value as u32;
    let mut len = 0;
    loop {
        let mut byte = (val & 0x7f) as u8;
        val >>= 7;
        if val != 0 {
            byte |= 0x80;
        }
        out[len] = byte;
        len += 1;
        if val == 0 {
            return (out, len);
        }
    }
}

/// Encoded size of a protocol VarInt.
pub const fn varint_i32_len(value: i32) -> usize {
    let val = value as u32;
    if val < (1 << 7) {
        1
    } else if val < (1 << 14) {
        2
    } else if val < (1 << 21) {
        3
    } else if val < (1 << 28) {
        4
    } else {
        5
    }
}

/// Decode a strict, canonical protocol VarLong.
pub fn read_varlong_i64(input: &[u8]) -> Result<(i64, usize), VarIntError> {
    let mut value = 0u64;
    for index in 0..10 {
        let byte = *input.get(index).ok_or(VarIntError::UnexpectedEof)?;
        if index == 9 && byte & 0xfe != 0 {
            return Err(VarIntError::TooLong);
        }
        value |= u64::from(byte & 0x7f) << (index * 7);
        if byte & 0x80 == 0 {
            let len = index + 1;
            if varlong_i64_len(value as i64) != len {
                return Err(VarIntError::TooLong);
            }
            return Ok((value as i64, len));
        }
    }
    Err(VarIntError::TooLong)
}

/// Append a protocol VarLong.
pub fn write_varlong_i64(value: i64, out: &mut Vec<u8>) {
    let (bytes, len) = encode_varlong_i64(value);
    out.extend_from_slice(&bytes[..len]);
}

/// Encode a protocol VarLong into its maximum-sized stack buffer.
pub fn encode_varlong_i64(value: i64) -> ([u8; 10], usize) {
    let mut out = [0; 10];
    let mut val = value as u64;
    let mut len = 0;
    loop {
        let mut byte = (val & 0x7f) as u8;
        val >>= 7;
        if val != 0 {
            byte |= 0x80;
        }
        out[len] = byte;
        len += 1;
        if val == 0 {
            return (out, len);
        }
    }
}

/// Encoded size of a protocol VarLong.
pub const fn varlong_i64_len(value: i64) -> usize {
    let val = value as u64;
    if val < (1 << 7) {
        1
    } else if val < (1 << 14) {
        2
    } else if val < (1 << 21) {
        3
    } else if val < (1 << 28) {
        4
    } else if val < (1 << 35) {
        5
    } else if val < (1 << 42) {
        6
    } else if val < (1 << 49) {
        7
    } else if val < (1 << 56) {
        8
    } else if val < (1 << 63) {
        9
    } else {
        10
    }
}

fn string_payload(input: &[u8], cap: usize) -> Result<(&str, usize), VarIntError> {
    let (signed_len, prefix) = read_varint_i32(input)?;
    if signed_len < 0 {
        return Err(VarIntError::NegativeLength(signed_len));
    }
    let len = signed_len as usize;
    if len > cap {
        return Err(VarIntError::StringTooLong { len, max: cap });
    }
    let end = prefix.checked_add(len).ok_or(VarIntError::TooLong)?;
    let bytes = input.get(prefix..end).ok_or(VarIntError::UnexpectedEof)?;
    let s = std::str::from_utf8(bytes).map_err(|_| VarIntError::InvalidUtf8)?;
    Ok((s, end))
}

/// Read a VarInt-prefixed UTF-8 string with a byte-length cap.
pub fn read_mc_string(input: &[u8], max_bytes: usize) -> Result<(&str, usize), VarIntError> {
    string_payload(input, max_bytes)
}

/// Read a VarInt-prefixed UTF-8 string and cap its UTF-16 code-unit count.
pub fn read_mc_string_utf16_capped(
    input: &[u8],
    max_utf16_units: usize,
) -> Result<(&str, usize), VarIntError> {
    let (s, consumed) = string_payload(input, max_utf16_units.saturating_mul(4))?;
    let units = s.encode_utf16().count();
    if units > max_utf16_units {
        return Err(VarIntError::StringTooLong {
            len: units,
            max: max_utf16_units,
        });
    }
    Ok((s, consumed))
}

/// Append a VarInt-prefixed UTF-8 string.
pub fn write_mc_string(s: &str, out: &mut Vec<u8>) {
    write_varint_i32(s.len() as i32, out);
    out.extend_from_slice(s.as_bytes());
}
