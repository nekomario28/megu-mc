// SPDX-License-Identifier: LGPL-3.0-or-later
//! RFC 1952 framing and RFC 1951 decoding for the dependency-free NBT API.
//!
//! The inner DEFLATE decoder and CRC32 are the `megu_core` canonical
//! primitives; this module keeps only the gzip member/header/trailer
//! framing that core intentionally does not cover.

use crate::Error;
use megu_core::zip_archive::ZipError;
use Error::{ByteBudget, InvalidGzip};

pub(crate) fn decode(mut input: &[u8], budget: usize) -> Result<Vec<u8>, Error> {
    if input.is_empty() {
        return Err(InvalidGzip);
    }
    let mut output = Vec::new();
    while !input.is_empty() {
        let payload = member_payload(input)?;
        let remaining = budget - output.len();
        let (member, deflate_len) = inflate_member(payload, remaining)?;
        let trailer = payload
            .get(deflate_len..deflate_len + 8)
            .ok_or(InvalidGzip)?;
        let crc = u32::from_le_bytes(trailer[..4].try_into().map_err(|_| InvalidGzip)?);
        let size = u32::from_le_bytes(trailer[4..].try_into().map_err(|_| InvalidGzip)?);
        if crc != megu_core::hash::crc32_ieee(&member) || size != member.len() as u32 {
            return Err(InvalidGzip);
        }
        output.extend_from_slice(&member);
        input = &payload[deflate_len + 8..];
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
        let length = u16::from_le_bytes(take(&mut rest, 2)?.try_into().map_err(|_| InvalidGzip)?);
        take(&mut rest, usize::from(length))?;
    }
    for flag in [8, 16] {
        if flags & flag != 0 {
            let end = rest.iter().position(|&b| b == 0).ok_or(InvalidGzip)?;
            take(&mut rest, end + 1)?;
        }
    }
    if flags & 2 != 0 {
        let expected = megu_core::hash::crc32_ieee(&header[..header.len() - rest.len()]) as u16;
        let actual = u16::from_le_bytes(take(&mut rest, 2)?.try_into().map_err(|_| InvalidGzip)?);
        if actual != expected {
            return Err(InvalidGzip);
        }
    }
    Ok(rest)
}

fn map_inflate(e: ZipError) -> Error {
    match e {
        ZipError::OutputCapExceeded => ByteBudget,
        _ => InvalidGzip,
    }
}

/// Decode one member's RFC 1951 stream out of `payload` (deflate data
/// followed by the 8-byte trailer and any subsequent members). Returns the
/// member bytes and the exact byte length of the deflate stream.
///
/// The core decoder reports the exact number of bytes consumed, leaving the
/// gzip trailer and subsequent members untouched.
fn inflate_member(payload: &[u8], max_out: usize) -> Result<(Vec<u8>, usize), Error> {
    megu_core::zip_archive::inflate_raw_rfc1951_with_consumed(payload, max_out).map_err(map_inflate)
}
