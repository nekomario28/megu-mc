//! RFC 1950/1951 inflate front-end — the single decoder shared by the PNG,
//! shader-pack ZIP, resource-pack ZIP, and protocol readers. The raw
//! RFC 1951 Deflate kernel is [`megu_core::zip_archive::inflate_raw_rfc1951`];
//! this module keeps only the RFC 1950 zlib wrapper (header check + Adler-32
//! trailer verification) and the crate-local error type. Output is always
//! bounded by an explicit byte limit so a hostile stream cannot expand
//! unboundedly.

use std::fmt;

use megu_core::zip_archive::{inflate_raw_rfc1951, ZipError};

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

/// Maps the canonical kernel's failure onto the historical reason strings so
/// downstream callers see unchanged diagnostics.
fn from_zip(e: ZipError) -> InflateError {
    let reason = match e {
        ZipError::OutputCapExceeded => "deflate output exceeds expected size".to_owned(),
        ZipError::Truncated => "deflate stream truncated".to_owned(),
        ZipError::Inflate(msg) => match msg {
            "huffman code length > 15" => "invalid huffman code length".to_owned(),
            "HLIT out of range" | "HDIST out of range" => {
                "dynamic table size out of range".to_owned()
            }
            "repeat overruns code lengths" => "repeat overflows table".to_owned(),
            "zero run overruns code lengths" => "zero-run overflows table".to_owned(),
            "bad code-length symbol" => "invalid code-length symbol".to_owned(),
            "distance symbol out of range" => "invalid distance symbol".to_owned(),
            "length symbol out of range" => "invalid literal/length symbol".to_owned(),
            "reserved block type" => "reserved deflate block type".to_owned(),
            other => other.to_owned(),
        },
        other => other.to_string(),
    };
    InflateError { reason }
}

/// Inflates raw DEFLATE `input` into at most `limit` bytes.
pub fn inflate(input: &[u8], limit: usize) -> Result<Vec<u8>, InflateError> {
    inflate_raw_rfc1951(input, limit).map_err(from_zip)
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
    // The Adler-32 sits on the first byte boundary after the last DEFLATE
    // bit. The canonical kernel does not report the consumed-bit position,
    // so the boundary is pinned instead: the deflate payload is exactly
    // `deflate[..len - 4]` — decoding it must succeed while the one-byte
    // shorter prefix must fail to terminate.
    let data_len = match deflate.len().checked_sub(4) {
        Some(len) => len,
        None => return err("zlib stream truncated before Adler-32"),
    };
    let (data, adler_bytes) = deflate.split_at(data_len);
    let out = inflate(data, limit)?;
    if inflate(&deflate[..data_len.saturating_sub(1)], limit).is_ok() {
        return err("trailing bytes after zlib trailer");
    }
    let want = u32::from_be_bytes([
        adler_bytes[0],
        adler_bytes[1],
        adler_bytes[2],
        adler_bytes[3],
    ]);
    if adler32(&out) != want {
        return err("Adler-32 checksum mismatch");
    }
    Ok(out)
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
