// SPDX-License-Identifier: LGPL-3.0-or-later
#![forbid(unsafe_code)]
//! `megu_core::lz4` — the single canonical bounded LZ4 *block* codec.
//!
//! Blocks carry no frame header and no length prefix; callers keep the
//! expected output size (or a hard cap) and pass it to
//! [`decompress_block`] / [`decompress_block_exact`]. Every token-derived
//! length and offset is bounds-checked and decoding aborts the moment the
//! output would exceed `max_output` — *before* the bytes are written — so
//! a hostile stream cannot amplify into a memory denial of service and a
//! truncated frame is never mistaken for a clean end-of-block.
//!
//! The compressor is a greedy single-hash-table design: it favours decode
//! speed and small code over maximal ratio, and honours the LZ4
//! end-of-block contract (no match may start inside the last
//! [`LZ4_MFLIMIT`] bytes, and the final [`LZ4_LAST_LITERALS`] bytes are
//! always literals) so output is decodable by any conforming LZ4 block
//! decoder, not just this one.

use std::fmt;

/// Largest backward match distance the LZ4 block format encodes.
const LZ4_MAX_OFFSET: usize = u16::MAX as usize;
/// Minimum encoded match length; the token stores `len - MIN_MATCH`.
const LZ4_MIN_MATCH: usize = 4;
/// A match may not start inside the last 12 bytes of the block.
const LZ4_MFLIMIT: usize = 12;
/// A match may not extend into the final 5 bytes; they stay literals.
const LZ4_LAST_LITERALS: usize = 5;
/// Hash table width for the greedy compressor.
const LZ4_HASH_BITS: u32 = 16;

/// Why a [`decompress_block`] call rejected a stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lz4Error {
    /// The stream ended before the token-implied structure was complete.
    Truncated,
    /// A match offset was zero or pointed before the block's output start.
    BadOffset,
    /// Decoded output would exceed the caller's output bound.
    TooLarge,
    /// The stream decoded cleanly but not to the exact length the caller
    /// required (`decompress_block_exact` only).
    LengthMismatch,
}

impl fmt::Display for Lz4Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => f.write_str("truncated LZ4 stream"),
            Self::BadOffset => f.write_str("LZ4 match offset out of range"),
            Self::TooLarge => f.write_str("LZ4 output exceeds the bound"),
            Self::LengthMismatch => f.write_str("LZ4 output length differs from expected"),
        }
    }
}

impl std::error::Error for Lz4Error {}

#[inline]
fn lz4_hash4(b: &[u8]) -> usize {
    let v = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    (v.wrapping_mul(2_654_435_761) >> (32 - LZ4_HASH_BITS)) as usize
}

/// Append `extra` as a nibble-style extended length (`0..` unbounded).
fn lz4_emit_len(out: &mut Vec<u8>, mut extra: usize) {
    while extra >= 255 {
        out.push(255);
        extra -= 255;
    }
    out.push(extra as u8);
}

/// Emit one LZ4 sequence: `literals`, then `offset`/`match_len` when a
/// match follows (omitted for the trailing literals-only sequence).
fn lz4_emit_sequence(
    out: &mut Vec<u8>,
    literals: &[u8],
    offset: usize,
    match_len: usize,
    with_match: bool,
) {
    let lit_token = literals.len().min(15) as u8;
    let match_extra = if with_match {
        (match_len - LZ4_MIN_MATCH).min(15) as u8
    } else {
        0
    };
    out.push((lit_token << 4) | match_extra);
    if literals.len() >= 15 {
        lz4_emit_len(out, literals.len() - 15);
    }
    out.extend_from_slice(literals);
    if with_match {
        out.extend_from_slice(&(offset as u16).to_le_bytes());
        if match_len - LZ4_MIN_MATCH >= 15 {
            lz4_emit_len(out, match_len - LZ4_MIN_MATCH - 15);
        }
    }
}

/// Compress `src` into a standalone LZ4 *block* (no frame header).
///
/// Output is always a valid LZ4 block even when `src` is incompressible
/// (it degenerates to literals); callers compare sizes before storing.
#[must_use]
pub fn compress_block(src: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(src.len() / 2 + 32);
    compress_block_into(src, &mut out);
    out
}

/// Compress `src`, appending the LZ4 block to `out`.
pub fn compress_block_into(src: &[u8], out: &mut Vec<u8>) {
    let n = src.len();
    // Blocks too small to hold a match, and inputs beyond the u32
    // position space of the hash table, degrade to a literals-only
    // sequence — still a perfectly valid LZ4 block.
    if n <= LZ4_MFLIMIT || n > u32::MAX as usize {
        lz4_emit_sequence(out, src, 0, 0, false);
        return;
    }

    let mut table = vec![u32::MAX; 1 << LZ4_HASH_BITS];
    let mut anchor = 0usize;
    let mut i = 0usize;
    let mflimit = n - LZ4_MFLIMIT;
    let match_end_limit = n - LZ4_LAST_LITERALS;

    while i <= mflimit {
        let h = lz4_hash4(&src[i..]);
        let cand = table[h];
        table[h] = i as u32;
        if cand == u32::MAX {
            i += 1;
            continue;
        }
        let cand = cand as usize;
        if i - cand > LZ4_MAX_OFFSET || src[cand..cand + LZ4_MIN_MATCH] != src[i..i + LZ4_MIN_MATCH]
        {
            i += 1;
            continue;
        }
        let mut match_len = LZ4_MIN_MATCH;
        while i + match_len < match_end_limit && src[cand + match_len] == src[i + match_len] {
            match_len += 1;
        }
        lz4_emit_sequence(out, &src[anchor..i], i - cand, match_len, true);
        i += match_len;
        anchor = i;
    }
    lz4_emit_sequence(out, &src[anchor..n], 0, 0, false);
}

/// Decompress a standalone LZ4 *block*, failing closed.
///
/// `max_output` is a hard cap on decoded bytes: the decoder rejects with
/// [`Lz4Error::TooLarge`] the moment a token would push output past it,
/// before those bytes are written, so malicious streams cannot amplify
/// into a memory denial of service.
pub fn decompress_block(src: &[u8], max_output: usize) -> Result<Vec<u8>, Lz4Error> {
    let mut out = Vec::with_capacity(src.len().min(max_output));
    decompress_block_into(src, &mut out, max_output)?;
    Ok(out)
}

/// Decompress a standalone LZ4 *block* into `out`, appending.
///
/// The block is self-contained: match offsets may only reference bytes
/// this call appended, never earlier contents of `out`. `max_output`
/// bounds the total length of `out` (pre-existing bytes included), and
/// every bound is checked *before* the corresponding write. On error the
/// partial output appended so far stays in `out`.
pub fn decompress_block_into(
    src: &[u8],
    out: &mut Vec<u8>,
    max_output: usize,
) -> Result<(), Lz4Error> {
    let base = out.len();
    let mut i = 0usize;
    while i < src.len() {
        let token = src[i];
        i += 1;

        let mut lit_len = usize::from(token >> 4);
        if lit_len == 15 {
            loop {
                let b = *src.get(i).ok_or(Lz4Error::Truncated)?;
                i += 1;
                lit_len = lit_len
                    .checked_add(usize::from(b))
                    .ok_or(Lz4Error::TooLarge)?;
                if lit_len > max_output {
                    return Err(Lz4Error::TooLarge);
                }
                if b != 255 {
                    break;
                }
            }
        }
        let lit_end = i.checked_add(lit_len).ok_or(Lz4Error::TooLarge)?;
        if lit_end > src.len() {
            return Err(Lz4Error::Truncated);
        }
        if out.len().checked_add(lit_len).ok_or(Lz4Error::TooLarge)? > max_output {
            return Err(Lz4Error::TooLarge);
        }
        out.extend_from_slice(&src[i..lit_end]);
        i = lit_end;

        if i == src.len() {
            break; // last sequence: literals only, no match trailer
        }
        let offset_end = i.checked_add(2).ok_or(Lz4Error::Truncated)?;
        if offset_end > src.len() {
            return Err(Lz4Error::Truncated);
        }
        let offset = usize::from(u16::from_le_bytes([src[i], src[i + 1]]));
        i = offset_end;
        if offset == 0 || offset > out.len() - base {
            return Err(Lz4Error::BadOffset);
        }

        let mut match_len = usize::from(token & 0x0f);
        if match_len == 15 {
            loop {
                let b = *src.get(i).ok_or(Lz4Error::Truncated)?;
                i += 1;
                match_len = match_len
                    .checked_add(usize::from(b))
                    .ok_or(Lz4Error::TooLarge)?;
                if out.len() + match_len + LZ4_MIN_MATCH > max_output {
                    return Err(Lz4Error::TooLarge);
                }
                if b != 255 {
                    break;
                }
            }
        }
        match_len += LZ4_MIN_MATCH;
        if out.len() + match_len > max_output {
            return Err(Lz4Error::TooLarge);
        }

        // Copy the match; extending from within handles overlap correctly
        // by re-slicing the just-appended region (offset < match_len is
        // the RLE case).
        let start = out.len() - offset;
        let mut remaining = match_len;
        while remaining > 0 {
            let avail = out.len() - start;
            let take = remaining.min(avail);
            out.extend_from_within(start..start + take);
            remaining -= take;
        }
    }
    Ok(())
}

/// Decompress a standalone LZ4 *block* that must decode to exactly
/// `expected_len` bytes.
///
/// Equivalent to [`decompress_block`] with `max_output = expected_len`
/// plus a final length check: a stream that decodes cleanly but short is
/// rejected with [`Lz4Error::LengthMismatch`].
pub fn decompress_block_exact(src: &[u8], expected_len: usize) -> Result<Vec<u8>, Lz4Error> {
    let out = decompress_block(src, expected_len)?;
    if out.len() == expected_len {
        Ok(out)
    } else {
        Err(Lz4Error::LengthMismatch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic incompressible bytes (FNV-style byte chain).
    fn noise(n: usize) -> Vec<u8> {
        let mut state = 0xcbf2_9ce4_8422_2325u64;
        (0..n)
            .map(|_| {
                state ^= state >> 13;
                state = state.wrapping_mul(0x0000_0100_0000_01b3);
                (state >> 32) as u8
            })
            .collect()
    }

    #[test]
    fn roundtrip_varied_payloads() {
        let mut payload = b"the quick brown fox jumps over the lazy dog. ".repeat(400);
        payload.extend_from_slice(&noise(1_024));
        payload.extend_from_slice(&vec![0u8; 777]);
        assert_eq!(decompress_block(&compress_block(&payload), payload.len() + 16).unwrap(), payload);
        assert_eq!(decompress_block_exact(&compress_block(&payload), payload.len()).unwrap(), payload);
    }

    #[test]
    fn roundtrip_overlapping_rle_matches() {
        // distance 1, 2 and 3 repeats exercise the overlap copy path.
        for (unit, reps) in [(&b"a"[..], 3_000), (&b"ab"[..], 1_500), (&b"abc"[..], 1_000)] {
            let data: Vec<u8> = unit.iter().copied().cycle().take(unit.len() * reps).collect();
            assert_eq!(decompress_block(&compress_block(&data), data.len()).unwrap(), data);
        }
    }

    #[test]
    fn empty_and_tiny_inputs() {
        for n in 0..=LZ4_MFLIMIT + 4 {
            let data = noise(n);
            assert_eq!(decompress_block_exact(&compress_block(&data), n).unwrap(), data);
        }
    }

    #[test]
    fn incompressible_roundtrip() {
        let data = noise(65_535);
        assert_eq!(decompress_block_exact(&compress_block(&data), data.len()).unwrap(), data);
    }

    #[test]
    fn truncated_stream_never_yields_garbage() {
        let data = b"hello hello hello hello, world!".repeat(64);
        let compressed = compress_block(&data);
        for cut in 1..compressed.len() {
            // A cut landing exactly on a sequence boundary parses as a
            // complete shorter block, so the contract is "error, or a
            // strict prefix of the payload" — never corrupt bytes, never
            // a panic, never the full output.
            if let Ok(out) = decompress_block(&compressed[..cut], data.len()) {
                assert!(out.len() < data.len(), "cut at {cut} decoded fully");
                assert!(data.starts_with(&out), "cut at {cut} yielded garbage");
            }
        }
        // A mid-literal cut is always rejected as truncated.
        let mid_literal = compressed.len() - 1;
        assert_eq!(
            decompress_block(&compressed[..mid_literal], data.len()),
            Err(Lz4Error::Truncated)
        );
    }

    #[test]
    fn rejects_bad_offsets() {
        // offset = 0
        assert_eq!(
            decompress_block(&[0x00, 0x00, 0x00], 64),
            Err(Lz4Error::BadOffset)
        );
        // offset points before block start (1 literal, offset 5)
        assert_eq!(
            decompress_block(&[0x10, b'x', 0x05, 0x00], 64),
            Err(Lz4Error::BadOffset)
        );
    }

    #[test]
    fn enforces_output_cap_before_writing() {
        // Crafted zip bomb: one literal 'a', then a match at offset 1
        // with a huge extended length.
        let mut bomb = vec![0x1f, b'a', 0x01, 0x00];
        bomb.extend_from_slice(&[0xff; 40]);
        bomb.push(0x00); // match_len = 4 + 15 + 40*255
        assert_eq!(decompress_block(&bomb, 100), Err(Lz4Error::TooLarge));

        // A valid stream decoded under a too-small cap also fails closed.
        let data = vec![7u8; 4_096];
        let compressed = compress_block(&data);
        assert_eq!(decompress_block(&compressed, 4_095), Err(Lz4Error::TooLarge));
        // But exactly-sized bounds are accepted.
        assert_eq!(decompress_block(&compressed, 4_096).unwrap(), data);
    }

    #[test]
    fn exact_decode_rejects_short_output() {
        let data = b"payload payload payload".repeat(32);
        let compressed = compress_block(&data);
        assert_eq!(
            decompress_block_exact(&compressed, data.len() + 1),
            Err(Lz4Error::LengthMismatch)
        );
    }

    #[test]
    fn into_variants_append_and_bounds_include_base() {
        let data = b"shared spine".repeat(64);
        let mut packed = Vec::new();
        compress_block_into(&data, &mut packed);
        assert_eq!(packed, compress_block(&data));

        let mut out = vec![0xee; 8];
        decompress_block_into(&packed, &mut out, 8 + data.len()).unwrap();
        assert_eq!(&out[..8], &[0xee; 8]);
        assert_eq!(&out[8..], data.as_slice());

        // Offsets may not reach into the pre-existing bytes.
        assert_eq!(
            decompress_block_into(&[0x10, b'x', 0x02, 0x00], &mut vec![0xaa; 4], 64),
            Err(Lz4Error::BadOffset)
        );
        // The cap counts pre-existing bytes too.
        assert_eq!(
            decompress_block_into(&packed, &mut Vec::new(), data.len() - 1),
            Err(Lz4Error::TooLarge)
        );
    }
}
