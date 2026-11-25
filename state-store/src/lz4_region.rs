// SPDX-License-Identifier: LGPL-3.0-or-later
//! Compact `.mlz4` chunk-region container backed by the canonical
//! `megu_core::lz4` block codec and `megu_core::hash::crc32_ieee` checksums.
//!
//! Chunk payload bytes are opaque. This makes conversion to and from the
//! region container lossless for already encoded Anvil chunk records.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

pub const MLZ4_MAGIC: &[u8; 4] = b"MLZ4";
pub const MLZ4_VERSION: u16 = 1;
pub const REGION_SIDE: usize = 32;
pub const REGION_CHUNK_COUNT: usize = REGION_SIDE * REGION_SIDE;
pub const MAX_CHUNK_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_REGION_DECOMPRESSED_BYTES: usize = 128 * 1024 * 1024;
const HEADER_BYTES: usize = 8;
const ENTRY_BYTES: usize = 20;
const TABLE_BYTES: usize = REGION_CHUNK_COUNT * ENTRY_BYTES;
const PAYLOAD_OFFSET: usize = HEADER_BYTES + TABLE_BYTES;

#[derive(Debug)]
pub enum Lz4Error {
    Truncated(&'static str),
    InvalidOffset(usize),
    OutputLimit {
        limit: usize,
    },
    LengthOverflow,
    InvalidMagic,
    UnsupportedVersion(u16),
    InvalidTable(&'static str),
    ChunkTooLarge {
        length: usize,
        limit: usize,
    },
    AggregateSizeLimitExceeded {
        limit: usize,
        attempted: usize,
    },
    ChecksumMismatch {
        slot: usize,
        expected: u32,
        actual: u32,
    },
    Io(io::Error),
}

impl fmt::Display for Lz4Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated(what) => write!(f, "truncated LZ4 {what}"),
            Self::InvalidOffset(offset) => write!(f, "invalid LZ4 match offset {offset}"),
            Self::OutputLimit { limit } => write!(f, "LZ4 output exceeds {limit} byte limit"),
            Self::LengthOverflow => f.write_str("LZ4 length overflow"),
            Self::InvalidMagic => f.write_str("invalid MLZ4 magic"),
            Self::UnsupportedVersion(version) => write!(f, "unsupported MLZ4 version {version}"),
            Self::InvalidTable(why) => write!(f, "invalid MLZ4 chunk table: {why}"),
            Self::ChunkTooLarge { length, limit } => {
                write!(f, "chunk length {length} exceeds {limit} byte limit")
            }
            Self::AggregateSizeLimitExceeded { limit, attempted } => write!(
                f,
                "aggregate decompressed length {attempted} exceeds {limit} byte budget"
            ),
            Self::ChecksumMismatch {
                slot,
                expected,
                actual,
            } => write!(
                f,
                "MLZ4 CRC32 mismatch for slot {slot}: expected {expected:08x}, got {actual:08x}"
            ),
            Self::Io(error) => error.fmt(f),
        }
    }
}

impl Error for Lz4Error {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for Lz4Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

pub type Lz4RegionError = Lz4Error;

/// Compresses one input as a standard LZ4 block (without a frame wrapper).
///
/// This is a compatibility shim over the single canonical implementation in
/// [`megu_core::lz4`]; the region format never carries a private codec copy.
pub fn lz4_compress_block(src: &[u8]) -> Vec<u8> {
    megu_core::lz4::compress_block(src)
}

/// Decompresses an LZ4 block and refuses to emit more than `max_uncompressed`.
///
/// The public error shape predates the canonical codec extraction, so this
/// shim maps the canonical fail-closed errors back onto it without weakening
/// any bound or truncation check.
pub fn lz4_decompress_block(src: &[u8], max_uncompressed: usize) -> Result<Vec<u8>, Lz4Error> {
    if src.is_empty() {
        return Err(Lz4Error::Truncated("token"));
    }
    megu_core::lz4::decompress_block(src, max_uncompressed).map_err(|error| match error {
        megu_core::lz4::Lz4Error::Truncated => Lz4Error::Truncated("block stream"),
        megu_core::lz4::Lz4Error::BadOffset => Lz4Error::InvalidOffset(0),
        megu_core::lz4::Lz4Error::TooLarge => Lz4Error::OutputLimit {
            limit: max_uncompressed,
        },
        megu_core::lz4::Lz4Error::LengthMismatch => {
            Lz4Error::InvalidTable("decoded payload length disagrees with table")
        }
    })
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Lz4Region {
    chunks: BTreeMap<usize, Vec<u8>>,
}

impl Lz4Region {
    /// Builds a region from slot-indexed, opaque chunk payloads (0..1024).
    pub fn from_chunk_map(chunks: BTreeMap<usize, Vec<u8>>) -> Result<Self, Lz4Error> {
        for (&slot, payload) in &chunks {
            if slot >= REGION_CHUNK_COUNT {
                return Err(Lz4Error::InvalidTable("slot index is outside 0..1024"));
            }
            if payload.len() > MAX_CHUNK_BYTES {
                return Err(Lz4Error::ChunkTooLarge {
                    length: payload.len(),
                    limit: MAX_CHUNK_BYTES,
                });
            }
        }
        Ok(Self { chunks })
    }

    pub fn to_chunk_map(&self) -> BTreeMap<usize, Vec<u8>> {
        self.chunks.clone()
    }

    pub fn chunk(&self, slot: usize) -> Option<&[u8]> {
        self.chunks.get(&slot).map(Vec::as_slice)
    }

    pub fn set_chunk(&mut self, slot: usize, payload: Vec<u8>) -> Result<(), Lz4Error> {
        if slot >= REGION_CHUNK_COUNT {
            return Err(Lz4Error::InvalidTable("slot index is outside 0..1024"));
        }
        if payload.len() > MAX_CHUNK_BYTES {
            return Err(Lz4Error::ChunkTooLarge {
                length: payload.len(),
                limit: MAX_CHUNK_BYTES,
            });
        }
        self.chunks.insert(slot, payload);
        Ok(())
    }

    pub fn remove_chunk(&mut self, slot: usize) -> Option<Vec<u8>> {
        self.chunks.remove(&slot)
    }

    pub fn encode(&self) -> Result<Vec<u8>, Lz4Error> {
        let mut out = vec![0u8; PAYLOAD_OFFSET];
        out[0..4].copy_from_slice(MLZ4_MAGIC);
        out[4..6].copy_from_slice(&MLZ4_VERSION.to_le_bytes());
        out[6..8].copy_from_slice(&(REGION_CHUNK_COUNT as u16).to_le_bytes());

        for (&slot, payload) in &self.chunks {
            let compressed = lz4_compress_block(payload);
            let offset = u64::try_from(out.len()).map_err(|_| Lz4Error::LengthOverflow)?;
            let compressed_len =
                u32::try_from(compressed.len()).map_err(|_| Lz4Error::LengthOverflow)?;
            let uncompressed_len =
                u32::try_from(payload.len()).map_err(|_| Lz4Error::LengthOverflow)?;
            let entry = HEADER_BYTES + slot * ENTRY_BYTES;
            put_u64(&mut out[entry..entry + 8], offset);
            put_u32(&mut out[entry + 8..entry + 12], compressed_len);
            put_u32(&mut out[entry + 12..entry + 16], uncompressed_len);
            put_u32(
                &mut out[entry + 16..entry + 20],
                megu_core::hash::crc32_ieee(payload),
            );
            out.extend_from_slice(&compressed);
        }
        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, Lz4Error> {
        Self::decode_with_budget(bytes, MAX_REGION_DECOMPRESSED_BYTES)
    }

    pub fn decode_with_budget(bytes: &[u8], max_total_bytes: usize) -> Result<Self, Lz4Error> {
        if bytes.len() < PAYLOAD_OFFSET {
            return Err(Lz4Error::Truncated("region header/table"));
        }
        if bytes.get(0..4) != Some(MLZ4_MAGIC.as_slice()) {
            return Err(Lz4Error::InvalidMagic);
        }
        let version = get_u16(&bytes[4..6]);
        if version != MLZ4_VERSION {
            return Err(Lz4Error::UnsupportedVersion(version));
        }
        if get_u16(&bytes[6..8]) as usize != REGION_CHUNK_COUNT {
            return Err(Lz4Error::InvalidTable("slot count is not 1024"));
        }

        let mut ranges = Vec::new();
        let mut entries = Vec::new();
        let mut total_declared_bytes = 0usize;
        for slot in 0..REGION_CHUNK_COUNT {
            let entry = HEADER_BYTES + slot * ENTRY_BYTES;
            let offset = get_u64(&bytes[entry..entry + 8]);
            let compressed_len = get_u32(&bytes[entry + 8..entry + 12]) as usize;
            let uncompressed_len = get_u32(&bytes[entry + 12..entry + 16]) as usize;
            let crc = get_u32(&bytes[entry + 16..entry + 20]);
            if offset == 0 && compressed_len == 0 && uncompressed_len == 0 && crc == 0 {
                continue;
            }
            if offset < PAYLOAD_OFFSET as u64 || compressed_len == 0 {
                return Err(Lz4Error::InvalidTable(
                    "invalid populated entry offset or length",
                ));
            }
            if uncompressed_len > MAX_CHUNK_BYTES {
                return Err(Lz4Error::ChunkTooLarge {
                    length: uncompressed_len,
                    limit: MAX_CHUNK_BYTES,
                });
            }
            let attempted_total = total_declared_bytes
                .checked_add(uncompressed_len)
                .ok_or(Lz4Error::LengthOverflow)?;
            if attempted_total > max_total_bytes {
                return Err(Lz4Error::AggregateSizeLimitExceeded {
                    limit: max_total_bytes,
                    attempted: attempted_total,
                });
            }
            total_declared_bytes = attempted_total;
            let start = usize::try_from(offset).map_err(|_| Lz4Error::LengthOverflow)?;
            let end = start
                .checked_add(compressed_len)
                .ok_or(Lz4Error::LengthOverflow)?;
            if end > bytes.len() {
                return Err(Lz4Error::InvalidTable("payload extends past end of file"));
            }
            ranges.push((start, end));
            entries.push((slot, start, end, uncompressed_len, crc));
        }
        ranges.sort_unstable();
        if ranges.windows(2).any(|pair| pair[0].1 > pair[1].0) {
            return Err(Lz4Error::InvalidTable("payload ranges overlap"));
        }

        let mut chunks = BTreeMap::new();
        let mut total_decoded_bytes = 0usize;
        for (slot, start, end, expected_len, expected_crc) in entries {
            let attempted_total = total_decoded_bytes
                .checked_add(expected_len)
                .ok_or(Lz4Error::LengthOverflow)?;
            if attempted_total > max_total_bytes {
                return Err(Lz4Error::AggregateSizeLimitExceeded {
                    limit: max_total_bytes,
                    attempted: attempted_total,
                });
            }
            let payload = lz4_decompress_block(&bytes[start..end], expected_len)?;
            if payload.len() != expected_len {
                return Err(Lz4Error::InvalidTable(
                    "decoded payload length disagrees with table",
                ));
            }
            let actual_crc = megu_core::hash::crc32_ieee(&payload);
            if actual_crc != expected_crc {
                return Err(Lz4Error::ChecksumMismatch {
                    slot,
                    expected: expected_crc,
                    actual: actual_crc,
                });
            }
            total_decoded_bytes = attempted_total;
            chunks.insert(slot, payload);
        }
        Ok(Self { chunks })
    }

    pub fn read_all_with_budget(bytes: &[u8], max_total_bytes: usize) -> Result<Self, Lz4Error> {
        Self::decode_with_budget(bytes, max_total_bytes)
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self, Lz4Error> {
        Self::load_with_budget(path, MAX_REGION_DECOMPRESSED_BYTES)
    }

    pub fn load_with_budget(
        path: impl AsRef<Path>,
        max_total_bytes: usize,
    ) -> Result<Self, Lz4Error> {
        Self::decode_with_budget(&fs::read(path)?, max_total_bytes)
    }

    pub fn open_with_budget(
        path: impl AsRef<Path>,
        max_total_bytes: usize,
    ) -> Result<Self, Lz4Error> {
        Self::load_with_budget(path, max_total_bytes)
    }

    /// Writes a sibling temporary file, syncs it, then atomically renames it.
    pub fn save_atomic(&self, path: impl AsRef<Path>) -> Result<(), Lz4Error> {
        let path = path.as_ref();
        let temporary = sibling_temp_path(path);
        let bytes = self.encode()?;
        let mut file = File::create(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)?;
        if let Some(parent) = path.parent() {
            if let Ok(directory) = File::open(parent) {
                let _ = directory.sync_all();
            }
        }
        Ok(())
    }
}

fn sibling_temp_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".tmp");
    PathBuf::from(name)
}

fn put_u64(dst: &mut [u8], value: u64) {
    dst.copy_from_slice(&value.to_le_bytes());
}

fn put_u32(dst: &mut [u8], value: u32) {
    dst.copy_from_slice(&value.to_le_bytes());
}

fn get_u64(src: &[u8]) -> u64 {
    u64::from_le_bytes([
        src[0], src[1], src[2], src[3], src[4], src[5], src[6], src[7],
    ])
}

fn get_u32(src: &[u8]) -> u32 {
    u32::from_le_bytes([src[0], src[1], src[2], src[3]])
}

fn get_u16(src: &[u8]) -> u16 {
    u16::from_le_bytes([src[0], src[1]])
}
