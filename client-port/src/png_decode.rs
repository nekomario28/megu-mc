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

fn inflate_zlib(input: &[u8], limit: usize) -> Result<Vec<u8>, PngDecodeError> {
    crate::inflate::inflate_zlib(input, limit).map_err(|e| PngDecodeError::Inflate {
        reason: e.reason().to_owned(),
    })
}

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
    // Scratch for the 4-bit indexed path's SIMD unpack (identity palette).
    let mut packed_words: Vec<u64> = Vec::new();
    let mut idx16: Vec<u16> = Vec::new();
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
                if depth == 4 {
                    // PNG packs pixel pairs most-significant nibble first, so
                    // each little-endian byte gather is nibble-swapped to give
                    // the kernel its low-nibble-first order. The identity
                    // palette renders the kernel's out-of-range clamp inert:
                    // `idx16[x]` holds the raw index and the bounds check
                    // below still rejects out-of-range entries.
                    const IDENTITY: [u16; 16] = [
                        0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15,
                    ];
                    packed_words.clear();
                    packed_words.extend(row.chunks(8).map(|chunk| {
                        let mut w = 0u64;
                        for (i, &b) in chunk.iter().enumerate() {
                            w |= u64::from(b) << (8 * i);
                        }
                        ((w >> 4) & 0x0f0f_0f0f_0f0f_0f0f)
                            | ((w << 4) & 0xf0f0_f0f0_f0f0_f0f0)
                    }));
                    idx16.resize(width, 0);
                    megu_core::simd_voxel::unpack_palette_4bit_to_u16(
                        &packed_words,
                        &IDENTITY,
                        &mut idx16,
                    );
                    for &index in &idx16 {
                        let index = usize::from(index);
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
                } else {
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
