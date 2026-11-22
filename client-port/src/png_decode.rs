// SPDX-License-Identifier: LGPL-3.0-or-later

//! Clean-room, bounded PNG decoder (RFC 2083) for Minecraft resource packs.
//!
//! [`decode_png_rgba`] turns a `.png` texture into the `(width, height,
//! RGBA8)` triple [`crate::asset_pipeline::ResourcePackLayer`] stores for
//! sprites, with zero third-party dependencies: chunk framing and CRC-32 are
//! parsed directly, IDAT payloads are inflated by a purpose-built RFC
//! 1950/1951 decoder (stored, fixed-Huffman and dynamic-Huffman blocks), and
//! every scanline is unfiltered (all five PNG filter types) before samples
//! are expanded to RGBA8.
//!
//! The decoder is deliberately fail-closed against hostile packs:
//!
//! * [`MAX_PNG_DIM`] caps width and height at 4096 texels.
//! * [`MAX_DECODED_BYTES`] caps the inflated scanline stream at 64 MiB, and
//!   the inflate itself is bounded by the *exact* expected scanline size —
//!   a stream that produces even one byte too many is rejected, so a
//!   zip-bomb PNG cannot expand past its declared geometry. (A 4096x4096
//!   8-bit RGBA image's filtered stream is ~64.1 MiB and is therefore
//!   rejected; real textures are far smaller.)
//! * Every chunk's CRC-32 is verified, IHDR field values and chunk ordering
//!   are validated, unknown *critical* chunks are rejected, and only the
//!   colour types Minecraft and HD packs actually use are decoded: 8-bit
//!   RGBA (6), RGB (2), indexed `PLTE`+`tRNS` at bit depths 1/2/4/8 (3),
//!   8-bit grayscale + `tRNS` (0), and 8-bit grayscale+alpha (4).
//!   16-bit samples, Adam7 interlacing, and unknown colour types fail
//!   closed. Ancillary chunks (gamma, text, ...) are skipped after CRC
//!   verification — colour management is out of scope, matching vanilla.

use std::error::Error;
use std::fmt;

/// Largest accepted PNG width or height, in texels.
pub const MAX_PNG_DIM: u32 = 4096;

/// Largest accepted *inflated* scanline stream, in bytes. The IDAT payload
/// inflates to `height * (1 + row_bytes)` (one filter-type byte per
/// scanline plus the packed row); exceeding this budget — or the exact
/// expected size derived from IHDR, whichever trips first — rejects the
/// image before any unfiltered or RGBA buffer is allocated.
pub const MAX_DECODED_BYTES: u64 = 64 * 1024 * 1024;

const PNG_SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Every way PNG decoding can fail. All variants are terminal for the one
/// image being decoded; [`crate::asset_pipeline::ResourcePackLayer`]
/// records them per file.
#[derive(Debug)]
pub enum PngDecodeError {
    /// Fewer than 8 bytes of input, or the signature is not the RFC 2083
    /// magic.
    BadSignature,
    /// A chunk header, payload or CRC ran past the end of the input.
    Truncated,
    /// A chunk's stored CRC-32 disagrees with its type+data bytes.
    BadCrc {
        /// Chunk type as lossy UTF-8 for diagnostics.
        chunk: String,
    },
    /// IHDR is missing, duplicated, not the first chunk, or carries an
    /// invalid field (bad length, zero dimension, nonzero compression or
    /// filter method, dimension over [`MAX_PNG_DIM`]).
    BadHeader {
        /// Exact reason the header was rejected.
        reason: String,
    },
    /// A well-formed PNG using a feature outside the supported subset:
    /// 16-bit samples, Adam7 interlacing, an unknown colour type, a
    /// disallowed `PLTE`/`tRNS` combination, or a chunk after IDAT.
    Unsupported {
        /// Exact reason the feature was rejected.
        reason: String,
    },
    /// The declared scanline payload would exceed [`MAX_DECODED_BYTES`].
    TooLarge {
        /// Inflated size the image would require, in bytes.
        bytes: u64,
    },
    /// The zlib/DEFLATE stream inside IDAT is malformed, truncated, fails
    /// its Adler-32 check, or expands past the expected scanline size.
    Inflate {
        /// Exact reason the stream was rejected.
        reason: String,
    },
    /// The decompressed scanline payload has the wrong length, a scanline
    /// filter byte is not 0..=4, or an indexed pixel references a palette
    /// entry that does not exist.
    BadImageData {
        /// Exact reason the image data was rejected.
        reason: String,
    },
}

impl fmt::Display for PngDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadSignature => write!(f, "not a PNG (bad signature)"),
            Self::Truncated => write!(f, "PNG chunk data truncated"),
            Self::BadCrc { chunk } => write!(f, "CRC-32 mismatch in {chunk} chunk"),
            Self::BadHeader { reason } => write!(f, "bad PNG header: {reason}"),
            Self::Unsupported { reason } => write!(f, "unsupported PNG feature: {reason}"),
            Self::TooLarge { bytes } => {
                write!(f, "PNG needs {bytes} decoded bytes (limit {MAX_DECODED_BYTES})")
            }
            Self::Inflate { reason } => write!(f, "bad PNG image data stream: {reason}"),
            Self::BadImageData { reason } => write!(f, "bad PNG image data: {reason}"),
        }
    }
}

impl Error for PngDecodeError {}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Decodes `bytes` as a PNG image and returns `(width, height, rgba)` where
/// `rgba` is `width * height * 4` row-major RGBA8 texels — the exact shape
/// [`crate::asset_pipeline::ResourcePackLayer::sprites`] stores.
///
/// Fails closed on anything outside the supported subset described in the
/// module docs; hostile or corrupt input can never allocate more than
/// [`MAX_DECODED_BYTES`] of working memory.
pub fn decode_png_rgba(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), PngDecodeError> {
    let png = parse_chunks(bytes)?;
    let row_bytes = png.row_bytes()?;
    let filtered_len = u64::from(png.height)
        .checked_mul(row_bytes as u64 + 1)
        .ok_or(PngDecodeError::TooLarge {
            bytes: u64::MAX,
        })?;
    if filtered_len > MAX_DECODED_BYTES {
        return Err(PngDecodeError::TooLarge {
            bytes: filtered_len,
        });
    }
    let limit = usize::try_from(filtered_len).map_err(|_| PngDecodeError::TooLarge {
        bytes: filtered_len,
    })?;
    let filtered = inflate_zlib(&png.idat, limit)?;
    if filtered.len() != limit {
        return Err(PngDecodeError::BadImageData {
            reason: format!(
                "decompressed {} bytes, expected {limit}",
                filtered.len()
            ),
        });
    }
    let raw = unfilter_rows(&filtered, &png)?;
    // Colour type 6 at bit depth 8 is already row-major RGBA8: the
    // unfiltered buffer *is* the output, no expansion pass needed.
    if png.color_type == 6 {
        debug_assert_eq!(raw.len(), png.width as usize * png.height as usize * 4);
        return Ok((png.width, png.height, raw));
    }
    let rgba = expand_to_rgba(&png, &raw)?;
    Ok((png.width, png.height, rgba))
}

// ---------------------------------------------------------------------------
// Chunk parsing
// ---------------------------------------------------------------------------

/// Everything the decoder needs from the chunk layer, in validated form.
struct ParsedPng {
    width: u32,
    height: u32,
    bit_depth: u8,
    color_type: u8,
    /// RGB triples; empty when the image has no palette.
    palette: Vec<[u8; 3]>,
    /// Raw `tRNS` payload; interpreted per colour type during expansion.
    trns: Option<Vec<u8>>,
    /// Concatenated IDAT payloads (zlib stream).
    idat: Vec<u8>,
}

impl ParsedPng {
    /// Samples per pixel for the supported colour types.
    fn samples_per_pixel(&self) -> u32 {
        match self.color_type {
            0 | 3 => 1,
            4 => 2,
            2 => 3,
            _ => 4, // 6
        }
    }

    /// Bits per *packed* pixel: for indexed images this is the index size,
    /// not a sample count.
    fn bits_per_pixel(&self) -> u32 {
        self.samples_per_pixel() * u32::from(self.bit_depth)
    }

    /// Bytes per scanline before the filter-type byte.
    fn row_bytes(&self) -> Result<usize, PngDecodeError> {
        let bits = u64::from(self.width) * u64::from(self.bits_per_pixel());
        usize::try_from(bits.div_ceil(8)).map_err(|_| PngDecodeError::TooLarge { bytes: bits })
    }

    /// Bytes per complete pixel for the unfilter predictors, rounded up to
    /// one — 1 for sub-byte indexed rows, the channel count for 8-bit rows.
    fn filter_bpp(&self) -> usize {
        (self.bits_per_pixel() / 8).max(1) as usize
    }
}

/// Parses and validates the whole chunk layer: signature, IHDR, PLTE, tRNS,
/// IDAT collection and IEND. Every chunk's CRC-32 is verified before its
/// payload is used.
fn parse_chunks(bytes: &[u8]) -> Result<ParsedPng, PngDecodeError> {
    if bytes.len() < PNG_SIGNATURE.len() || bytes[..PNG_SIGNATURE.len()] != PNG_SIGNATURE {
        return Err(PngDecodeError::BadSignature);
    }
    let mut pos = PNG_SIGNATURE.len();
    let mut png: Option<ParsedPng> = None;
    let mut palette: Vec<[u8; 3]> = Vec::new();
    let mut trns: Option<Vec<u8>> = None;
    let mut idat: Vec<u8> = Vec::new();
    let mut idat_started = false;
    let mut idat_closed = false;
    let mut chunk_index = 0usize;

    loop {
        let header = bytes.get(pos..pos + 8).ok_or(PngDecodeError::Truncated)?;
        let len = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let kind = [header[4], header[5], header[6], header[7]];
        let data_start = pos + 8;
        let data_end = data_start
            .checked_add(len)
            .ok_or(PngDecodeError::Truncated)?;
        let data = bytes
            .get(data_start..data_end)
            .ok_or(PngDecodeError::Truncated)?;
        let crc_bytes = bytes
            .get(data_end..)
            .and_then(|rest| rest.get(..4))
            .ok_or(PngDecodeError::Truncated)?;
        let want_crc = u32::from_be_bytes([crc_bytes[0], crc_bytes[1], crc_bytes[2], crc_bytes[3]]);
        if crc32_chunk(&kind, data) != want_crc {
            return Err(PngDecodeError::BadCrc {
                chunk: String::from_utf8_lossy(&kind).into_owned(),
            });
        }
        pos = data_end + 4;
        let is_first = chunk_index == 0;
        chunk_index += 1;

        match &kind {
            b"IHDR" => {
                if !is_first || png.is_some() {
                    return Err(PngDecodeError::BadHeader {
                        reason: "IHDR must be the first and only header chunk".to_owned(),
                    });
                }
                png = Some(parse_ihdr(data)?);
            }
            b"PLTE" => {
                if png.is_none() || idat_started || !palette.is_empty() {
                    return Err(PngDecodeError::Unsupported {
                        reason: "misplaced or duplicate PLTE chunk".to_owned(),
                    });
                }
                palette = parse_plte(data, png.as_ref().expect("checked above"))?;
            }
            b"tRNS" => {
                if png.is_none() || idat_started || trns.is_some() {
                    return Err(PngDecodeError::Unsupported {
                        reason: "misplaced or duplicate tRNS chunk".to_owned(),
                    });
                }
                validate_trns(data, png.as_ref().expect("checked above"), &palette)?;
                trns = Some(data.to_vec());
            }
            b"IDAT" => {
                if idat_closed {
                    return Err(PngDecodeError::Unsupported {
                        reason: "IDAT chunks must be consecutive".to_owned(),
                    });
                }
                let info = png.as_ref().ok_or(PngDecodeError::BadHeader {
                    reason: "IDAT before IHDR".to_owned(),
                })?;
                if info.color_type == 3 && palette.is_empty() {
                    return Err(PngDecodeError::BadHeader {
                        reason: "indexed image has no PLTE chunk".to_owned(),
                    });
                }
                idat_started = true;
                idat.extend_from_slice(data);
            }
            b"IEND" => {
                if png.is_none() || !idat_started {
                    return Err(PngDecodeError::BadHeader {
                        reason: "IEND before IHDR/IDAT".to_owned(),
                    });
                }
                if len != 0 {
                    return Err(PngDecodeError::BadHeader {
                        reason: "IEND chunk has a nonzero length".to_owned(),
                    });
                }
                let mut png = png.expect("checked above");
                png.palette = palette;
                png.trns = trns;
                png.idat = idat;
                return Ok(png);
            }
            _ => {
                // A non-IDAT chunk ends the consecutive IDAT run; a later
                // IDAT is then rejected above. Ancillary chunks may legally
                // appear before IDAT, between the IDAT run and IEND, and
                // are skipped after CRC verification.
                if idat_started {
                    idat_closed = true;
                }
                // Chunk type bit 5 clear means "critical": an unrecognised
                // critical chunk makes the image undecodable.
                if kind[0] & 0x20 == 0 {
                    return Err(PngDecodeError::Unsupported {
                        reason: format!(
                            "unrecognised critical chunk {}",
                            String::from_utf8_lossy(&kind)
                        ),
                    });
                }
            }
        }
    }
}

/// Parses and validates the 13-byte IHDR payload.
fn parse_ihdr(data: &[u8]) -> Result<ParsedPng, PngDecodeError> {
    let bad = |reason: &str| PngDecodeError::BadHeader {
        reason: reason.to_owned(),
    };
    if data.len() != 13 {
        return Err(bad("IHDR payload is not 13 bytes"));
    }
    let width = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
    let height = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    let bit_depth = data[8];
    let color_type = data[9];
    let compression = data[10];
    let filter_method = data[11];
    let interlace = data[12];
    if width == 0 || height == 0 {
        return Err(bad("zero width or height"));
    }
    if width > MAX_PNG_DIM || height > MAX_PNG_DIM {
        return Err(bad(&format!(
            "dimensions {width}x{height} exceed {MAX_PNG_DIM}"
        )));
    }
    if compression != 0 {
        return Err(bad("unknown compression method (must be 0)"));
    }
    if filter_method != 0 {
        return Err(bad("unknown filter method (must be 0)"));
    }
    if interlace != 0 {
        return Err(PngDecodeError::Unsupported {
            reason: "Adam7 interlacing is not supported".to_owned(),
        });
    }
    let depth_ok = match color_type {
        // Only the 8-bit forms Minecraft/HD packs use; 16-bit samples
        // would need a downconversion this decoder deliberately omits.
        0 | 2 | 4 | 6 => bit_depth == 8,
        3 => matches!(bit_depth, 1 | 2 | 4 | 8),
        _ => false,
    };
    if !depth_ok {
        return Err(PngDecodeError::Unsupported {
            reason: format!("bit depth {bit_depth} / colour type {color_type} combination"),
        });
    }
    Ok(ParsedPng {
        width,
        height,
        bit_depth,
        color_type,
        palette: Vec::new(),
        trns: None,
        idat: Vec::new(),
    })
}

/// Parses and validates a PLTE payload against the IHDR colour type.
fn parse_plte(data: &[u8], png: &ParsedPng) -> Result<Vec<[u8; 3]>, PngDecodeError> {
    let unsupported = |reason: &str| PngDecodeError::Unsupported {
        reason: reason.to_owned(),
    };
    if matches!(png.color_type, 0 | 4) {
        return Err(unsupported("PLTE is not allowed for this colour type"));
    }
    if data.is_empty() || !data.len().is_multiple_of(3) {
        return Err(unsupported("PLTE length is not a multiple of 3"));
    }
    let entries = data.len() / 3;
    if entries > 256 {
        return Err(unsupported("PLTE has more than 256 entries"));
    }
    if png.color_type == 3 && entries > (1usize << png.bit_depth) {
        return Err(unsupported(
            "PLTE has more entries than the bit depth allows",
        ));
    }
    Ok(data.as_chunks::<3>().0.to_vec())
}

/// Validates a tRNS payload against the IHDR colour type (and palette, for
/// indexed images). The payload itself is stored raw and interpreted during
/// RGBA expansion.
fn validate_trns(
    data: &[u8],
    png: &ParsedPng,
    palette: &[[u8; 3]],
) -> Result<(), PngDecodeError> {
    let unsupported = |reason: &str| PngDecodeError::Unsupported {
        reason: reason.to_owned(),
    };
    let ok = match png.color_type {
        0 => data.len() == 2,
        2 => data.len() == 6,
        3 => {
            !data.is_empty() && !palette.is_empty() && data.len() <= palette.len()
        }
        // Colour types 4 and 6 have an alpha channel already; tRNS is
        // forbidden for them by the spec.
        _ => false,
    };
    if !ok {
        return Err(unsupported("tRNS chunk not valid for this image"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// CRC-32 (PNG polynomial, reflected)
// ---------------------------------------------------------------------------

const fn crc_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

static CRC_TABLE: [u32; 256] = crc_table();

fn crc32_update(mut crc: u32, data: &[u8]) -> u32 {
    for &b in data {
        crc = CRC_TABLE[((crc ^ u32::from(b)) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc
}

/// CRC-32 over `kind || data`, the exact bytes a PNG chunk CRC covers.
fn crc32_chunk(kind: &[u8; 4], data: &[u8]) -> u32 {
    crc32_update(crc32_update(0xFFFF_FFFF, kind), data) ^ 0xFFFF_FFFF
}

// ---------------------------------------------------------------------------
// RFC 1950/1951 inflate (zlib wrapper + DEFLATE)
// ---------------------------------------------------------------------------

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

fn inflate_err(reason: impl Into<String>) -> PngDecodeError {
    PngDecodeError::Inflate {
        reason: reason.into(),
    }
}

/// Inflates a zlib stream (RFC 1950) into at most `limit` bytes and verifies
/// the Adler-32 trailer. `limit` is the exact expected scanline size, so a
/// stream that expands even one byte past the declared image geometry is
/// rejected instead of decompressing into a bomb.
fn inflate_zlib(input: &[u8], limit: usize) -> Result<Vec<u8>, PngDecodeError> {
    if input.len() < 2 {
        return Err(inflate_err("zlib header truncated"));
    }
    let cmf = input[0];
    let flg = input[1];
    if cmf & 0x0F != 8 {
        return Err(inflate_err("unsupported zlib compression method"));
    }
    if cmf >> 4 > 7 {
        return Err(inflate_err("zlib window size descriptor out of range"));
    }
    if (u32::from(cmf) << 8 | u32::from(flg)) % 31 != 0 {
        return Err(inflate_err("bad zlib header check bits"));
    }
    if flg & 0x20 != 0 {
        return Err(inflate_err("preset dictionary not supported"));
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
        .ok_or_else(|| inflate_err("zlib stream truncated before Adler-32"))?;
    let want = u32::from_be_bytes([adler_bytes[0], adler_bytes[1], adler_bytes[2], adler_bytes[3]]);
    if adler32(&out) != want {
        return Err(inflate_err("Adler-32 checksum mismatch"));
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
fn inflate_deflate(br: &mut BitReader<'_>, limit: usize) -> Result<Vec<u8>, PngDecodeError> {
    let mut out: Vec<u8> = Vec::with_capacity(limit);
    loop {
        let bfinal = br.bits(1)?;
        let btype = br.bits(2)?;
        match btype {
            0 => {
                br.align_byte();
                let len = br.aligned_u16()?;
                let nlen = br.aligned_u16()?;
                if len != !nlen {
                    return Err(inflate_err("stored block LEN/NLEN mismatch"));
                }
                let len = usize::from(len);
                if out.len() + len > limit {
                    return Err(inflate_err("deflate output exceeds expected size"));
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
                                return Err(inflate_err(
                                    "deflate output exceeds expected size",
                                ));
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
                                return Err(inflate_err("invalid distance symbol"));
                            }
                            let distance = usize::from(DIST_BASE[dsym])
                                + br.bits(DIST_EXTRA[dsym])? as usize;
                            if distance > out.len() {
                                return Err(inflate_err(
                                    "back-reference before output start",
                                ));
                            }
                            if out.len() + length > limit {
                                return Err(inflate_err(
                                    "deflate output exceeds expected size",
                                ));
                            }
                            let start = out.len() - distance;
                            for i in 0..length {
                                let b = out[start + i];
                                out.push(b);
                            }
                        }
                        _ => return Err(inflate_err("invalid literal/length symbol")),
                    }
                }
            }
            _ => return Err(inflate_err("reserved deflate block type")),
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

    fn bits(&mut self, n: u32) -> Result<u32, PngDecodeError> {
        debug_assert!(n <= 32);
        while self.len < n {
            let b = *self
                .data
                .get(self.pos)
                .ok_or_else(|| inflate_err("deflate stream truncated"))?;
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

    fn aligned_byte(&mut self) -> Result<u8, PngDecodeError> {
        if self.len >= 8 {
            let b = self.buf as u8;
            self.buf >>= 8;
            self.len -= 8;
            Ok(b)
        } else {
            self.bits(8).map(|v| v as u8)
        }
    }

    fn aligned_u16(&mut self) -> Result<u16, PngDecodeError> {
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

    fn build(lengths: &[u8]) -> Result<Self, PngDecodeError> {
        let mut counts = [0u16; 16];
        for &l in lengths {
            if l > 15 {
                return Err(inflate_err("invalid huffman code length"));
            }
            counts[usize::from(l)] += 1;
        }
        if counts[0] as usize == lengths.len() {
            // All lengths zero: degenerate empty table.
            return Err(inflate_err("empty huffman table"));
        }
        // Check for an over-subscribed code.
        let mut left: i32 = 1;
        for &c in counts.iter().skip(1) {
            left <<= 1;
            left -= i32::from(c);
            if left < 0 {
                return Err(inflate_err("over-subscribed huffman code"));
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

    fn decode(&self, br: &mut BitReader<'_>) -> Result<u16, PngDecodeError> {
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
                    .ok_or_else(|| inflate_err("invalid huffman code"));
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        Err(inflate_err("invalid huffman code"))
    }
}

fn fixed_litlen_huffman() -> Result<Huffman, PngDecodeError> {
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

fn fixed_dist_huffman() -> Result<Huffman, PngDecodeError> {
    // Lengths are compile-time constants; build cannot fail.
    Huffman::build(&[5u8; 30])
}

/// Reads a dynamic-Huffman block header and builds the literal/length and
/// distance tables.
fn read_dynamic_tables(br: &mut BitReader<'_>) -> Result<(Huffman, Huffman), PngDecodeError> {
    let hlit = br.bits(5)? as usize + 257;
    let hdist = br.bits(5)? as usize + 1;
    let hclen = br.bits(4)? as usize + 4;
    if hlit > 286 || hdist > 30 {
        return Err(inflate_err("dynamic table size out of range"));
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
                    return Err(inflate_err("repeat with no previous length"));
                }
                let prev = lengths[i - 1];
                let rep = 3 + br.bits(2)? as usize;
                for _ in 0..rep {
                    if i >= lengths.len() {
                        return Err(inflate_err("repeat overflows table"));
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
                    return Err(inflate_err("zero-run overflows table"));
                }
            }
            _ => return Err(inflate_err("invalid code-length symbol")),
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

// ---------------------------------------------------------------------------
// Scanline unfiltering + RGBA expansion
// ---------------------------------------------------------------------------

/// Paeth predictor (RFC 2083 section 6.6).
fn paeth_predictor(a: u8, b: u8, c: u8) -> u8 {
    let (a, b, c) = (i32::from(a), i32::from(b), i32::from(c));
    let p = a + b - c;
    let pa = (p - a).abs();
    let pb = (p - b).abs();
    let pc = (p - c).abs();
    if pa <= pb && pa <= pc {
        a as u8
    } else if pb <= pc {
        b as u8
    } else {
        c as u8
    }
}

/// Reverses the five PNG scanline filters, producing `height * row_bytes`
/// packed sample bytes. Filter type 0 copies, 1 Sub, 2 Up, 3 Average and 4
/// Paeth; all arithmetic is modulo 256.
fn unfilter_rows(
    filtered: &[u8],
    png: &ParsedPng,
) -> Result<Vec<u8>, PngDecodeError> {
    let height = png.height as usize;
    let row_bytes = png.row_bytes()?;
    let bpp = png.filter_bpp();
    let stride = row_bytes + 1;
    debug_assert_eq!(filtered.len(), height * stride);
    let zeros = vec![0u8; row_bytes];
    let mut raw = vec![0u8; height * row_bytes];
    for y in 0..height {
        let row = &filtered[y * stride..y * stride + stride];
        let filter = row[0];
        let src = &row[1..];
        let (done, rest) = raw.split_at_mut(y * row_bytes);
        let cur = &mut rest[..row_bytes];
        let prev: &[u8] = if y == 0 {
            &zeros
        } else {
            &done[(y - 1) * row_bytes..y * row_bytes]
        };
        match filter {
            0 => cur.copy_from_slice(src),
            1 => {
                for i in 0..row_bytes {
                    let left = if i >= bpp { cur[i - bpp] } else { 0 };
                    cur[i] = src[i].wrapping_add(left);
                }
            }
            2 => {
                for i in 0..row_bytes {
                    cur[i] = src[i].wrapping_add(prev[i]);
                }
            }
            3 => {
                for i in 0..row_bytes {
                    let left = if i >= bpp { cur[i - bpp] } else { 0 };
                    cur[i] = src[i].wrapping_add(
                        ((u16::from(left) + u16::from(prev[i])) / 2) as u8,
                    );
                }
            }
            4 => {
                for i in 0..row_bytes {
                    let left = if i >= bpp { cur[i - bpp] } else { 0 };
                    let up_left = if i >= bpp { prev[i - bpp] } else { 0 };
                    cur[i] = src[i].wrapping_add(paeth_predictor(left, prev[i], up_left));
                }
            }
            _ => {
                return Err(PngDecodeError::BadImageData {
                    reason: format!("invalid filter type {filter} on scanline {y}"),
                })
            }
        }
    }
    Ok(raw)
}

/// Expands unfiltered packed samples to row-major RGBA8 for the colour
/// types that are not already RGBA8 (handled by the caller).
fn expand_to_rgba(png: &ParsedPng, raw: &[u8]) -> Result<Vec<u8>, PngDecodeError> {
    let bad = |reason: String| PngDecodeError::BadImageData { reason };
    let width = png.width as usize;
    let height = png.height as usize;
    let row_bytes = png.row_bytes()?;
    let mut out = Vec::with_capacity(width * height * 4);
    // tRNS interpretations, computed once.
    let trns_gray = png
        .trns
        .as_ref()
        .filter(|_| png.color_type == 0)
        .map(|t| u16::from_be_bytes([t[0], t[1]]));
    let trns_rgb = png
        .trns
        .as_ref()
        .filter(|_| png.color_type == 2)
        .map(|t| {
            [
                u16::from_be_bytes([t[0], t[1]]),
                u16::from_be_bytes([t[2], t[3]]),
                u16::from_be_bytes([t[4], t[5]]),
            ]
        });
    for y in 0..height {
        let row = &raw[y * row_bytes..(y + 1) * row_bytes];
        match png.color_type {
            0 => {
                for &gray in row {
                    let alpha = if trns_gray == Some(u16::from(gray)) {
                        0
                    } else {
                        u8::MAX
                    };
                    out.extend_from_slice(&[gray, gray, gray, alpha]);
                }
            }
            2 => {
                for px in row.as_chunks::<3>().0 {
                    let transparent = match trns_rgb {
                        Some(t) => {
                            u16::from(px[0]) == t[0]
                                && u16::from(px[1]) == t[1]
                                && u16::from(px[2]) == t[2]
                        }
                        None => false,
                    };
                    let alpha = if transparent { 0 } else { u8::MAX };
                    out.extend_from_slice(&[px[0], px[1], px[2], alpha]);
                }
            }
            3 => {
                let depth = usize::from(png.bit_depth);
                let mask = (1u16 << depth) - 1;
                for x in 0..width {
                    let bitpos = x * depth;
                    let byte = row[bitpos / 8];
                    let shift = 8 - depth - (bitpos % 8);
                    let index = usize::from((u16::from(byte) >> shift) & mask);
                    let rgb = *png
                        .palette
                        .get(index)
                        .ok_or_else(|| bad(format!("palette index {index} out of range")))?;
                    let alpha = png
                        .trns
                        .as_ref()
                        .and_then(|t| t.get(index))
                        .copied()
                        .unwrap_or(u8::MAX);
                    out.extend_from_slice(&[rgb[0], rgb[1], rgb[2], alpha]);
                }
            }
            4 => {
                for px in row.as_chunks::<2>().0 {
                    out.extend_from_slice(&[px[0], px[0], px[0], px[1]]);
                }
            }
            _ => {
                return Err(bad(format!(
                    "internal error: colour type {} reached expansion",
                    png.color_type
                )))
            }
        }
    }
    Ok(out)
}
