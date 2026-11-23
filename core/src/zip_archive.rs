// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Lazy, zero-dependency ZIP/JAR indexing for the mod loader.
//!
//! [`LazyZipIndex`] is the archive substrate the asset pipeline uses when it
//! needs sounds, models or configs out of mod JARs: it never extracts to disk
//! and never reads the whole archive at index time. Opening a JAR seeks to the
//! tail, scans a bounded window for the End Of Central Directory record (ZIP64
//! EOCD locator + EOCD included), reads only the Central Directory bytes, and
//! validates every entry name fail-closed. Entry payloads are fetched later by
//! [`LazyZipIndex::read_entry`], which seeks to exactly one Local File Header
//! and reads only that entry's compressed bytes.
//!
//! One level of Jar-in-Jar is resolved at index time: entries under
//! `META-INF/jarjar/*.jar` (NeoForge) and `META-INF/jars/*.jar` (Fabric) that
//! are Stored are indexed in place so their children share the parent file
//! descriptor with zero copies; Deflate-compressed nested JARs up to
//! [`MAX_NESTED_JAR_BYTES`] are inflated to memory so their Central Directory
//! can be parsed, still without touching disk.
//!
//! Fail-closed contract: path traversal, NUL bytes, encrypted entries,
//! multi-disk archives, unsupported compression methods, oversized
//! uncompressed payloads ([`MAX_OGG_BYTES`]-style caller caps) and malformed
//! deflate streams are all hard errors, never silent skips of safety checks.
//!
//! [`inflate_raw_rfc1951`] is a clean-room pure-`std` RFC 1951 Deflate
//! decompressor covering stored, fixed-Huffman and dynamic-Huffman blocks with
//! a strict output byte cap so ZIP bombs cannot amplify memory use.

#![forbid(unsafe_code)]

use std::error::Error;
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Mutex;

/// Tail window scanned for the EOCD record: 22-byte EOCD plus the maximum
/// 65 535-byte ZIP comment.
pub const EOCD_SEARCH_WINDOW: u64 = 65_557;

/// Default per-entry uncompressed cap for asset reads (e.g. `.ogg` files).
pub const MAX_OGG_BYTES: usize = 64 * 1024 * 1024;

/// Maximum uncompressed size of a single nested JAR indexed in memory.
pub const MAX_NESTED_JAR_BYTES: u64 = 32 * 1024 * 1024;

/// Maximum Central Directory entries accepted per archive level.
pub const MAX_ENTRIES: usize = 65_536;

/// Total uncompressed bytes held in memory across all deflated nested JARs.
const MAX_TOTAL_NESTED_BYTES: u64 = 128 * 1024 * 1024;

/// Backward scan chunk for the EOCD search so a comment-free archive only
/// costs one small tail read.
const EOCD_CHUNK: u64 = 4_096;

const SIG_LFH: u32 = 0x0403_4b50;
const SIG_CDE: u32 = 0x0201_4b50;
const SIG_EOCD: u32 = 0x0605_4b50;
const SIG_Z64_EOCD: u32 = 0x0606_4b50;
const SIG_Z64_LOC: u32 = 0x0706_4b50;
const ZIP64_EXTRA_ID: u16 = 0x0001;
const EOCD_MIN: usize = 22;
const Z64_LOC_LEN: usize = 20;
const Z64_EOCD_LEN: usize = 56;
const LFH_FIXED: usize = 30;
const CDE_FIXED: usize = 46;

const METHOD_STORED: u16 = 0;
const METHOD_DEFLATE: u16 = 8;

/// Why archive or entry handling failed. Every variant is fail-closed: the
/// caller gets no partial data on error.
#[derive(Debug)]
pub enum ZipError {
    /// Underlying filesystem failure.
    Io(io::Error),
    /// No EOCD record found in the tail window.
    NotZipArchive,
    /// Multi-disk (spanned/split) archives are rejected.
    MultiDisk,
    /// A record or the entry payload ended early.
    Truncated,
    /// Structurally invalid record or stream.
    Malformed(&'static str),
    /// Entry name failed [`validate_entry_name`].
    InvalidEntryName(String),
    /// General-purpose flag bit 0 (encryption) was set.
    Encrypted(String),
    /// Compression method other than Stored (0) or Deflate (8).
    UnsupportedMethod(u16),
    /// `read_entry` name lookup missed.
    EntryNotFound(String),
    /// Entry exceeds the caller's uncompressed byte cap.
    EntryTooLarge {
        /// Declared uncompressed size.
        uncompressed_size: u64,
        /// Caller-supplied cap.
        max_bytes: u64,
    },
    /// RFC 1951 stream was malformed.
    Inflate(&'static str),
    /// Inflated output would exceed `max_out`.
    OutputCapExceeded,
    /// The archive mutex was poisoned by a panic elsewhere.
    LockPoisoned,
}

impl fmt::Display for ZipError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "archive I/O: {e}"),
            Self::NotZipArchive => f.write_str("no EOCD record found; not a ZIP archive"),
            Self::MultiDisk => f.write_str("multi-disk archives are not supported"),
            Self::Truncated => f.write_str("archive truncated"),
            Self::Malformed(what) => write!(f, "malformed archive: {what}"),
            Self::InvalidEntryName(name) => write!(f, "invalid entry name {name:?}"),
            Self::Encrypted(name) => write!(f, "entry {name:?} is encrypted"),
            Self::UnsupportedMethod(m) => write!(f, "unsupported compression method {m}"),
            Self::EntryNotFound(name) => write!(f, "entry {name:?} not found"),
            Self::EntryTooLarge {
                uncompressed_size,
                max_bytes,
            } => write!(
                f,
                "entry uncompressed size {uncompressed_size} exceeds cap {max_bytes}"
            ),
            Self::Inflate(what) => write!(f, "malformed deflate stream: {what}"),
            Self::OutputCapExceeded => f.write_str("inflated output exceeds byte cap"),
            Self::LockPoisoned => f.write_str("archive lock poisoned"),
        }
    }
}

impl Error for ZipError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for ZipError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// Adler-32 checksum used by zlib-wrapped Deflate streams.
pub fn adler32(data: &[u8]) -> u32 {
    const MOD: u32 = 65_521;
    let (mut a, mut b) = (1u32, 0u32);
    for chunk in data.chunks(5_552) {
        for &byte in chunk {
            a += u32::from(byte);
            b += a;
        }
        a %= MOD;
        b %= MOD;
    }
    (b << 16) | a
}

/// Wrap `data` in a zlib stream containing RFC 1951 stored blocks.
pub fn zlib_compress_stored(data: &[u8]) -> Vec<u8> {
    let blocks = data.len().max(1).div_ceil(65_535);
    let mut out = Vec::with_capacity(2 + data.len() + blocks * 5 + 4);
    out.extend_from_slice(&[0x78, 0x01]);
    if data.is_empty() {
        out.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
    } else {
        let count = data.len().div_ceil(65_535);
        for (index, chunk) in data.chunks(65_535).enumerate() {
            out.push(u8::from(index + 1 == count));
            let len = chunk.len() as u16;
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&(!len).to_le_bytes());
            out.extend_from_slice(chunk);
        }
    }
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

/// Inflate a zlib stream, validating its header, dictionary flag and Adler-32.
pub fn zlib_decompress_capped(input: &[u8], max_out: usize) -> Result<Vec<u8>, ZipError> {
    if input.len() < 6 {
        return Err(ZipError::Truncated);
    }
    let cmf = input[0];
    let flg = input[1];
    if cmf & 0x0f != 8 || cmf >> 4 > 7 || (u16::from(cmf) * 256 + u16::from(flg)) % 31 != 0 {
        return Err(ZipError::Malformed("invalid zlib header"));
    }
    if flg & 0x20 != 0 {
        return Err(ZipError::Malformed("zlib preset dictionary is unsupported"));
    }
    let payload_end = input.len() - 4;
    let output = inflate_raw_rfc1951(&input[2..payload_end], max_out)?;
    let stored = u32::from_be_bytes([
        input[payload_end],
        input[payload_end + 1],
        input[payload_end + 2],
        input[payload_end + 3],
    ]);
    if adler32(&output) != stored {
        return Err(ZipError::Malformed("zlib Adler-32 mismatch"));
    }
    Ok(output)
}

/// Reject a ZIP entry name that could escape the archive root or confuse
/// downstream path handling.
///
/// Rejects: empty names, leading `/` or any empty `//` component, `\0`,
/// Windows separators/drives (`\`, `:`), and `.`/`..` path components.
pub fn validate_entry_name(name: &str) -> Result<(), ZipError> {
    let bad = || ZipError::InvalidEntryName(name.to_owned());
    if name.is_empty() {
        return Err(bad());
    }
    if name.bytes().any(|b| b == b'\\' || b == b':' || b == 0) {
        return Err(bad());
    }
    for component in name.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return Err(bad());
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// RFC 1951 raw Deflate decompressor (stored / fixed / dynamic Huffman)
// ---------------------------------------------------------------------------

const LEN_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LEN_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12,
    13, 13,
];
const CL_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];
const MAX_BITS: usize = 15;

/// LSB-first bit reader over the compressed byte stream.
struct BitReader<'a> {
    buf: &'a [u8],
    pos: usize,
    bitbuf: u32,
    bitcnt: u32,
}

impl<'a> BitReader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self {
            buf,
            pos: 0,
            bitbuf: 0,
            bitcnt: 0,
        }
    }

    /// Read `n` bits (0..=16), LSB first.
    fn bits(&mut self, n: u32) -> Result<u32, ZipError> {
        while self.bitcnt < n {
            let byte = *self.buf.get(self.pos).ok_or(ZipError::Truncated)?;
            self.pos += 1;
            self.bitbuf |= (byte as u32) << self.bitcnt;
            self.bitcnt += 8;
        }
        let value = self.bitbuf & ((1u32 << n) - 1);
        self.bitbuf >>= n;
        self.bitcnt -= n;
        Ok(value)
    }

    /// Drop bits so the stream is on the next byte boundary (stored blocks).
    fn align_byte(&mut self) {
        let drop = self.bitcnt % 8;
        self.bitbuf >>= drop;
        self.bitcnt -= drop;
    }
}

/// Canonical Huffman decoder: per-length code counts plus symbols in canonical
/// order, decoded bit-by-bit per RFC 1951 §3.2.2.
struct Huffman {
    count: [u16; MAX_BITS + 1],
    symbol: Vec<u16>,
}

impl Huffman {
    fn build(lengths: &[u8]) -> Result<Self, ZipError> {
        let mut count = [0u16; MAX_BITS + 1];
        for &len in lengths {
            if len as usize > MAX_BITS {
                return Err(ZipError::Inflate("huffman code length > 15"));
            }
            count[len as usize] += 1;
        }
        count[0] = 0;
        let mut left: i32 = 1;
        for &c in count.iter().skip(1) {
            left <<= 1;
            left -= c as i32;
            if left < 0 {
                return Err(ZipError::Inflate("over-subscribed huffman code"));
            }
        }
        let mut offsets = [0usize; MAX_BITS + 1];
        for len in 1..MAX_BITS {
            offsets[len + 1] = offsets[len] + count[len] as usize;
        }
        let total: usize = count[1..].iter().map(|&c| c as usize).sum();
        let mut symbol = vec![0u16; total];
        let mut cursor = offsets;
        for (sym, &len) in lengths.iter().enumerate() {
            if len != 0 {
                symbol[cursor[len as usize]] = sym as u16;
                cursor[len as usize] += 1;
            }
        }
        Ok(Self { count, symbol })
    }

    fn decode(&self, reader: &mut BitReader<'_>) -> Result<u16, ZipError> {
        let mut code: i32 = 0;
        let mut first: i32 = 0;
        let mut index: i32 = 0;
        for len in 1..=MAX_BITS {
            code |= reader.bits(1)? as i32;
            let count = self.count[len] as i32;
            if code - first < count {
                return Ok(self.symbol[(index + code - first) as usize]);
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        Err(ZipError::Inflate("invalid huffman code"))
    }
}

/// Fixed literal/length + distance tables from RFC 1951 §3.2.6.
fn fixed_tables() -> Result<(Huffman, Huffman), ZipError> {
    let mut litlen = [0u8; 288];
    litlen[..144].fill(8);
    litlen[144..256].fill(9);
    litlen[256..280].fill(7);
    litlen[280..].fill(8);
    let dist = [5u8; 30];
    Ok((Huffman::build(&litlen)?, Huffman::build(&dist)?))
}

/// Parse the dynamic block header (RFC 1951 §3.2.7) into litlen/dist tables.
fn dynamic_tables(reader: &mut BitReader<'_>) -> Result<(Huffman, Huffman), ZipError> {
    let hlit = reader.bits(5)? as usize + 257;
    if hlit > 286 {
        return Err(ZipError::Inflate("HLIT out of range"));
    }
    let hdist = reader.bits(5)? as usize + 1;
    if hdist > 32 {
        return Err(ZipError::Inflate("HDIST out of range"));
    }
    let hclen = reader.bits(4)? as usize + 4;
    let mut cl_lengths = [0u8; 19];
    for &slot in CL_ORDER.iter().take(hclen) {
        cl_lengths[slot] = reader.bits(3)? as u8;
    }
    let cl = Huffman::build(&cl_lengths)?;

    let total = hlit + hdist;
    let mut lengths = vec![0u8; total];
    let mut i = 0usize;
    while i < total {
        match cl.decode(reader)? {
            sym @ 0..=15 => {
                lengths[i] = sym as u8;
                i += 1;
            }
            16 => {
                if i == 0 {
                    return Err(ZipError::Inflate("repeat with no previous length"));
                }
                let prev = lengths[i - 1];
                let rep = 3 + reader.bits(2)? as usize;
                if i + rep > total {
                    return Err(ZipError::Inflate("repeat overruns code lengths"));
                }
                for _ in 0..rep {
                    lengths[i] = prev;
                    i += 1;
                }
            }
            17 => {
                let rep = 3 + reader.bits(3)? as usize;
                if i + rep > total {
                    return Err(ZipError::Inflate("zero run overruns code lengths"));
                }
                i += rep;
            }
            18 => {
                let rep = 11 + reader.bits(7)? as usize;
                if i + rep > total {
                    return Err(ZipError::Inflate("zero run overruns code lengths"));
                }
                i += rep;
            }
            _ => return Err(ZipError::Inflate("bad code-length symbol")),
        }
    }
    Ok((
        Huffman::build(&lengths[..hlit])?,
        Huffman::build(&lengths[hlit..])?,
    ))
}

/// Decode one compressed block's symbol stream into `out`, honoring `max_out`.
fn inflate_codes(
    reader: &mut BitReader<'_>,
    litlen: &Huffman,
    dist: &Huffman,
    out: &mut Vec<u8>,
    max_out: usize,
) -> Result<(), ZipError> {
    loop {
        let sym = litlen.decode(reader)?;
        if sym < 256 {
            if out.len() >= max_out {
                return Err(ZipError::OutputCapExceeded);
            }
            out.push(sym as u8);
        } else if sym == 256 {
            return Ok(());
        } else {
            let idx = (sym - 257) as usize;
            if idx >= LEN_BASE.len() {
                return Err(ZipError::Inflate("length symbol out of range"));
            }
            let len =
                LEN_BASE[idx] as usize + reader.bits(LEN_EXTRA[idx] as u32)? as usize;
            let dsym = dist.decode(reader)? as usize;
            if dsym >= DIST_BASE.len() {
                return Err(ZipError::Inflate("distance symbol out of range"));
            }
            let d = DIST_BASE[dsym] as usize + reader.bits(DIST_EXTRA[dsym] as u32)? as usize;
            if d > out.len() {
                return Err(ZipError::Inflate("back-reference before output start"));
            }
            if out.len() + len > max_out {
                return Err(ZipError::OutputCapExceeded);
            }
            for _ in 0..len {
                let byte = out[out.len() - d];
                out.push(byte);
            }
        }
    }
}

/// Decompress a raw RFC 1951 Deflate stream into at most `max_out` bytes.
///
/// Fails closed on truncated input, reserved block types, over-subscribed
/// Huffman tables, invalid back-references, and any output beyond the cap —
/// the bomb-cap guard for untrusted archives.
pub fn inflate_raw_rfc1951(src: &[u8], max_out: usize) -> Result<Vec<u8>, ZipError> {
    let mut reader = BitReader::new(src);
    let mut out = Vec::new();
    loop {
        let bfinal = reader.bits(1)?;
        match reader.bits(2)? {
            0 => {
                reader.align_byte();
                let len = reader.bits(16)?;
                let nlen = reader.bits(16)?;
                if (len ^ nlen) != 0xFFFF {
                    return Err(ZipError::Inflate("stored block LEN/NLEN mismatch"));
                }
                let len = len as usize;
                if out.len() + len > max_out {
                    return Err(ZipError::OutputCapExceeded);
                }
                out.reserve(len);
                for _ in 0..len {
                    out.push(reader.bits(8)? as u8);
                }
            }
            1 => {
                let (litlen, dist) = fixed_tables()?;
                inflate_codes(&mut reader, &litlen, &dist, &mut out, max_out)?;
            }
            2 => {
                let (litlen, dist) = dynamic_tables(&mut reader)?;
                inflate_codes(&mut reader, &litlen, &dist, &mut out, max_out)?;
            }
            _ => return Err(ZipError::Inflate("reserved block type")),
        }
        if bfinal != 0 {
            return Ok(out);
        }
    }
}

// ---------------------------------------------------------------------------
// Lazy ZIP index
// ---------------------------------------------------------------------------

/// Indexed metadata for one archive member.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ZipEntryMeta {
    /// Which indexed archive level holds the entry: `0` is the file on disk;
    /// `>0` selects a nested-JAR source in descending order of registration.
    pub archive_id: u16,
    /// Validated entry name (no traversal, NULs, or Windows separators).
    pub name: String,
    /// Absolute offset of the entry's Local File Header — a file offset for
    /// file-backed sources, or an offset into the inflated buffer for
    /// memory-backed (deflated nested JAR) sources.
    pub local_header_offset: u64,
    /// Compressed byte count from the Central Directory.
    pub compressed_size: u64,
    /// Uncompressed byte count from the Central Directory.
    pub uncompressed_size: u64,
    /// Compression method: `0` Stored, `8` Deflate.
    pub compression_method: u16,
    /// `Some((offset, len))` when the entry sits inside a 1-level nested JAR:
    /// the byte range of that nested JAR's payload within the parent file.
    /// For Stored nested JARs this is the verbatim jar — the zero-copy range
    /// that lets child entries index straight into the parent descriptor; for
    /// Deflate nested JARs it locates the compressed blob. `None` for
    /// top-level members.
    pub nested_range: Option<(u64, u64)>,
}

fn u16le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn u64le(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes([
        b[o], b[o + 1], b[o + 2], b[o + 3], b[o + 4], b[o + 5], b[o + 6], b[o + 7],
    ])
}

/// Uniform random-access byte source over either a file range (offsets added
/// to a base) or an in-memory slice, so the same index pipeline serves the
/// top-level archive, stored nested JARs, and inflated nested JARs.
trait RangeRead {
    fn read_exact_at(&mut self, off: u64, buf: &mut [u8]) -> Result<(), ZipError>;
    fn len(&self) -> u64;
}

/// File-backed range; all reads are counted toward index telemetry.
struct FileRange<'a> {
    file: &'a mut File,
    counted: &'a mut u64,
    base: u64,
    len: u64,
}

impl RangeRead for FileRange<'_> {
    fn read_exact_at(&mut self, off: u64, buf: &mut [u8]) -> Result<(), ZipError> {
        match off.checked_add(buf.len() as u64) {
            Some(end) if end <= self.len => {}
            _ => return Err(ZipError::Truncated),
        }
        self.file.seek(SeekFrom::Start(self.base + off))?;
        self.file.read_exact(buf)?;
        *self.counted += buf.len() as u64;
        Ok(())
    }
    fn len(&self) -> u64 {
        self.len
    }
}

struct SliceRange<'a> {
    buf: &'a [u8],
}

impl RangeRead for SliceRange<'_> {
    fn read_exact_at(&mut self, off: u64, buf: &mut [u8]) -> Result<(), ZipError> {
        let start = usize::try_from(off).map_err(|_| ZipError::Truncated)?;
        let end = start.checked_add(buf.len()).ok_or(ZipError::Truncated)?;
        let slice = self.buf.get(start..end).ok_or(ZipError::Truncated)?;
        buf.copy_from_slice(slice);
        Ok(())
    }
    fn len(&self) -> u64 {
        self.buf.len() as u64
    }
}

/// Central Directory location and extent resolved from EOCD (or ZIP64 EOCD).
struct CentralInfo {
    cd_offset: u64,
    cd_size: u64,
    total_entries: u64,
}

/// A raw Central Directory record before filtering/descent.
struct RawEntry {
    name: String,
    method: u16,
    comp_size: u64,
    uncomp_size: u64,
    local_off: u64,
}

/// Scan an assembled tail window for an EOCD record whose comment length
/// matches the bytes that follow it. Returns the position within `tail`.
fn find_eocd_pos(tail: &[u8]) -> Option<usize> {
    if tail.len() < EOCD_MIN {
        return None;
    }
    let mut pos = tail.len() - EOCD_MIN;
    loop {
        if u32le(tail, pos) == SIG_EOCD {
            let comment_len = u16le(tail, pos + 20) as usize;
            if pos + EOCD_MIN + comment_len == tail.len() {
                return Some(pos);
            }
        }
        if pos == 0 {
            return None;
        }
        pos -= 1;
    }
}

/// Read the EOCD tail (chunked so comment-free archives cost ~4 KiB), handle
/// the ZIP64 locator + EOCD when size/count fields are saturated, and return
/// the Central Directory coordinates.
fn resolve_central(src: &mut dyn RangeRead) -> Result<CentralInfo, ZipError> {
    let len = src.len();
    if len < EOCD_MIN as u64 {
        return Err(ZipError::NotZipArchive);
    }
    let window = len.min(EOCD_SEARCH_WINDOW);
    let mut tail: Vec<u8> = Vec::new();
    let eocd_pos = loop {
        if tail.len() as u64 >= window {
            return Err(ZipError::NotZipArchive);
        }
        let want = EOCD_CHUNK.min(window - tail.len() as u64);
        let off = len - tail.len() as u64 - want;
        let mut chunk = vec![0u8; want as usize];
        src.read_exact_at(off, &mut chunk)?;
        chunk.extend_from_slice(&tail);
        tail = chunk;
        if let Some(pos) = find_eocd_pos(&tail) {
            break pos;
        }
    };
    let eocd_abs = len - tail.len() as u64 + eocd_pos as u64;
    let rec = &tail[eocd_pos..];

    let disk_no = u16le(rec, 4);
    let cd_disk = u16le(rec, 6);
    let disk_entries = u16le(rec, 8);
    let total_entries = u16le(rec, 10);
    let cd_size32 = u32le(rec, 12);
    let cd_offset32 = u32le(rec, 16);

    if disk_no != 0 || cd_disk != 0 || disk_entries != total_entries {
        return Err(ZipError::MultiDisk);
    }

    let needs_zip64 = total_entries == u16::MAX
        || cd_size32 == u32::MAX
        || cd_offset32 == u32::MAX;

    let (cd_offset, cd_size, entries) = if needs_zip64 {
        if eocd_abs < Z64_LOC_LEN as u64 {
            return Err(ZipError::Malformed("missing ZIP64 EOCD locator"));
        }
        let mut loc = [0u8; Z64_LOC_LEN];
        src.read_exact_at(eocd_abs - Z64_LOC_LEN as u64, &mut loc)?;
        if u32le(&loc, 0) != SIG_Z64_LOC {
            return Err(ZipError::Malformed("missing ZIP64 EOCD locator"));
        }
        if u32le(&loc, 4) != 0 || u32le(&loc, 16) != 1 {
            return Err(ZipError::MultiDisk);
        }
        let z64_off = u64le(&loc, 8);
        let mut z = [0u8; Z64_EOCD_LEN];
        src.read_exact_at(z64_off, &mut z)?;
        if u32le(&z, 0) != SIG_Z64_EOCD {
            return Err(ZipError::Malformed("bad ZIP64 EOCD signature"));
        }
        let entries_disk = u64le(&z, 24);
        let entries_total = u64le(&z, 32);
        if u32le(&z, 16) != 0 || u32le(&z, 20) != 0 || entries_disk != entries_total {
            return Err(ZipError::MultiDisk);
        }
        (u64le(&z, 48), u64le(&z, 40), entries_total)
    } else {
        (
            cd_offset32 as u64,
            cd_size32 as u64,
            total_entries as u64,
        )
    };

    if entries > MAX_ENTRIES as u64 {
        return Err(ZipError::Malformed("central directory entry count exceeds cap"));
    }
    match cd_offset.checked_add(cd_size) {
        Some(end) if end <= len => {}
        _ => return Err(ZipError::Malformed("central directory outside archive")),
    }
    Ok(CentralInfo {
        cd_offset,
        cd_size,
        total_entries: entries,
    })
}

/// Apply a ZIP64 extended-information extra field (0x0001): the u64 values
/// appear in fixed order, one per saturated 32-bit Central Directory field.
fn apply_zip64_extra(
    extra: &[u8],
    uncomp: &mut u64,
    comp: &mut u64,
    off: &mut u64,
    disk: &mut u32,
) -> Result<(), ZipError> {
    let mut p = 0usize;
    while p + 4 <= extra.len() {
        let id = u16le(extra, p);
        let size = u16le(extra, p + 2) as usize;
        let body = extra
            .get(p + 4..p + 4 + size)
            .ok_or(ZipError::Malformed("extra field overruns record"))?;
        if id == ZIP64_EXTRA_ID {
            let mut q = 0usize;
            let mut take = |slot: &mut u64, saturated: bool| -> Result<(), ZipError> {
                if saturated {
                    let v = body
                        .get(q..q + 8)
                        .map(u64le_at)
                        .ok_or(ZipError::Malformed("short ZIP64 extra field"))?;
                    *slot = v;
                    q += 8;
                }
                Ok(())
            };
            take(uncomp, *uncomp == u32::MAX as u64)?;
            take(comp, *comp == u32::MAX as u64)?;
            take(off, *off == u32::MAX as u64)?;
            if *disk == u16::MAX as u32 {
                let v = body
                    .get(q..q + 4)
                    .ok_or(ZipError::Malformed("short ZIP64 extra field"))?;
                *disk = u32le(v, 0);
            }
            return Ok(());
        }
        p += 4 + size;
    }
    // No 0x0001 field: values must already fit their 32-bit forms.
    if *uncomp == u32::MAX as u64 || *comp == u32::MAX as u64 || *off == u32::MAX as u64 {
        return Err(ZipError::Malformed("missing ZIP64 extra field"));
    }
    Ok(())
}

fn u64le_at(b: &[u8]) -> u64 {
    u64le(b, 0)
}

/// Parse Central Directory bytes into validated raw entries. Directory
/// records (names ending in `/`) are skipped; every file name must pass
/// [`validate_entry_name`].
fn parse_central(cd: &[u8], expected: usize) -> Result<Vec<RawEntry>, ZipError> {
    let mut entries = Vec::new();
    let mut p = 0usize;
    while p < cd.len() {
        let fixed = cd
            .get(p..p + CDE_FIXED)
            .ok_or(ZipError::Malformed("central directory record truncated"))?;
        if u32le(fixed, 0) != SIG_CDE {
            return Err(ZipError::Malformed("bad central directory signature"));
        }
        let flags = u16le(fixed, 8);
        let method = u16le(fixed, 10);
        let mut comp = u32le(fixed, 20) as u64;
        let mut uncomp = u32le(fixed, 24) as u64;
        let name_len = u16le(fixed, 28) as usize;
        let extra_len = u16le(fixed, 30) as usize;
        let comment_len = u16le(fixed, 32) as usize;
        let mut disk = u16le(fixed, 34) as u32;
        let mut local_off = u32le(fixed, 42) as u64;

        let name_bytes = cd
            .get(p + CDE_FIXED..p + CDE_FIXED + name_len)
            .ok_or(ZipError::Malformed("entry name overruns record"))?;
        let extra = cd
            .get(p + CDE_FIXED + name_len..p + CDE_FIXED + name_len + extra_len)
            .ok_or(ZipError::Malformed("extra field overruns record"))?;
        let next = p
            .checked_add(CDE_FIXED + name_len + extra_len + comment_len)
            .filter(|&n| n <= cd.len())
            .ok_or(ZipError::Malformed("comment overruns record"))?;

        let name = String::from_utf8(name_bytes.to_vec())
            .map_err(|_| ZipError::Malformed("entry name is not UTF-8"))?;
        if flags & 1 != 0 {
            return Err(ZipError::Encrypted(name));
        }
        apply_zip64_extra(extra, &mut uncomp, &mut comp, &mut local_off, &mut disk)?;
        if disk != 0 {
            return Err(ZipError::MultiDisk);
        }
        if !name.ends_with('/') {
            validate_entry_name(&name)?;
            entries.push(RawEntry {
                name,
                method,
                comp_size: comp,
                uncomp_size: uncomp,
                local_off,
            });
        }
        p = next;
    }
    if entries.len() > expected {
        return Err(ZipError::Malformed("more entries than EOCD declares"));
    }
    Ok(entries)
}

/// Read an archive level's Central Directory: locate EOCD in the tail window,
/// resolve ZIP64 when needed, then read and parse only the CD bytes.
fn read_archive_index(src: &mut dyn RangeRead) -> Result<Vec<RawEntry>, ZipError> {
    let info = resolve_central(src)?;
    let mut cd = vec![0u8; info.cd_size as usize];
    src.read_exact_at(info.cd_offset, &mut cd)?;
    parse_central(&cd, info.total_entries as usize)
}

/// Read a Local File Header and return the offset of the entry's data.
fn local_data_offset(src: &mut dyn RangeRead, local_off: u64) -> Result<u64, ZipError> {
    let mut hdr = [0u8; LFH_FIXED];
    src.read_exact_at(local_off, &mut hdr)?;
    if u32le(&hdr, 0) != SIG_LFH {
        return Err(ZipError::Malformed("bad local file header signature"));
    }
    let name_len = u16le(&hdr, 26) as u64;
    let extra_len = u16le(&hdr, 28) as u64;
    local_off
        .checked_add(LFH_FIXED as u64 + name_len + extra_len)
        .ok_or(ZipError::Malformed("local data offset overflow"))
}

/// `META-INF/jarjar/*.jar` (NeoForge) or `META-INF/jars/*.jar` (Fabric).
fn is_nested_jar(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    (lower.strip_prefix("meta-inf/jarjar/").is_some()
        || lower.strip_prefix("meta-inf/jars/").is_some())
        && lower.ends_with(".jar")
}

/// Byte source behind an [`ZipEntryMeta::archive_id`].
enum ArchiveSource {
    /// Reads hit the on-disk file at absolute offsets. Used for the top-level
    /// archive and for stored nested JARs (child offsets pre-adjusted).
    ParentFile,
    /// Reads hit a fully inflated deflated nested JAR held in memory.
    Inflated(Vec<u8>),
}

/// Lazy index over a ZIP/JAR: EOCD + Central Directory read at open, entry
/// payloads seeked to on demand. Never extracts to disk.
pub struct LazyZipIndex {
    file: Mutex<File>,
    file_len: u64,
    sources: Vec<ArchiveSource>,
    entries: Vec<ZipEntryMeta>,
    bytes_read_for_index: u64,
}

impl LazyZipIndex {
    /// Index every archive member.
    pub fn open_all(path: &Path) -> Result<Self, ZipError> {
        Self::open_filtered(path, |_| true)
    }

    /// Index archive members whose names pass `filter`.
    ///
    /// Reads only the EOCD tail window and the Central Directory — never the
    /// whole archive. Descends one level into `META-INF/jarjar/*.jar` /
    /// `META-INF/jars/*.jar`: stored nested JARs are indexed in place via the
    /// parent descriptor; deflated ones (≤ [`MAX_NESTED_JAR_BYTES`]) are
    /// inflated to memory, still with zero disk extraction.
    pub fn open_filtered(
        path: &Path,
        filter: impl Fn(&str) -> bool,
    ) -> Result<Self, ZipError> {
        let mut file = File::open(path)?;
        let file_len = file.metadata()?.len();
        let mut counted = 0u64;
        let raw = {
            let mut range = FileRange {
                file: &mut file,
                counted: &mut counted,
                base: 0,
                len: file_len,
            };
            read_archive_index(&mut range)?
        };

        let mut index = Self {
            file: Mutex::new(file),
            file_len,
            sources: vec![ArchiveSource::ParentFile],
            entries: Vec::new(),
            bytes_read_for_index: counted,
        };
        let mut total_nested = 0u64;

        for entry in raw {
            if entry.local_off + LFH_FIXED as u64 > file_len {
                return Err(ZipError::Malformed("local header outside archive"));
            }
            if is_nested_jar(&entry.name)
                && index.sources.len() <= u16::MAX as usize
                && entry.method == METHOD_STORED
                && entry.comp_size == entry.uncomp_size
            {
                let (data_off, children) = {
                    let mut range = FileRange {
                        file: index.file.get_mut().map_err(|_| ZipError::LockPoisoned)?,
                        counted: &mut counted,
                        base: 0,
                        len: file_len,
                    };
                    let data_off = local_data_offset(&mut range, entry.local_off)?;
                    if data_off + entry.uncomp_size > file_len {
                        return Err(ZipError::Malformed("nested jar outside archive"));
                    }
                    let mut nested = FileRange {
                        file: range.file,
                        counted: range.counted,
                        base: data_off,
                        len: entry.uncomp_size,
                    };
                    (data_off, read_archive_index(&mut nested)?)
                };
                let archive_id = index.sources.len() as u16;
                index.sources.push(ArchiveSource::ParentFile);
                if filter(&entry.name) {
                    index.entries.push(ZipEntryMeta {
                        archive_id: 0,
                        name: entry.name.clone(),
                        local_header_offset: entry.local_off,
                        compressed_size: entry.comp_size,
                        uncompressed_size: entry.uncomp_size,
                        compression_method: entry.method,
                        nested_range: Some((data_off, entry.uncomp_size)),
                    });
                }
                for child in children {
                    if child.local_off + LFH_FIXED as u64 > entry.uncomp_size {
                        return Err(ZipError::Malformed("nested local header outside jar"));
                    }
                    if filter(&child.name) {
                        index.entries.push(ZipEntryMeta {
                            archive_id,
                            name: child.name,
                            local_header_offset: data_off + child.local_off,
                            compressed_size: child.comp_size,
                            uncompressed_size: child.uncomp_size,
                            compression_method: child.method,
                            nested_range: Some((data_off, entry.uncomp_size)),
                        });
                    }
                }
            } else if is_nested_jar(&entry.name)
                && index.sources.len() <= u16::MAX as usize
                && entry.method == METHOD_DEFLATE
                && entry.uncomp_size <= MAX_NESTED_JAR_BYTES
                && total_nested + entry.uncomp_size <= MAX_TOTAL_NESTED_BYTES
            {
                total_nested += entry.uncomp_size;
                let (data_off, inflated, children) = {
                    let mut range = FileRange {
                        file: index.file.get_mut().map_err(|_| ZipError::LockPoisoned)?,
                        counted: &mut counted,
                        base: 0,
                        len: file_len,
                    };
                    let data_off = local_data_offset(&mut range, entry.local_off)?;
                    if data_off + entry.comp_size > file_len {
                        return Err(ZipError::Malformed("nested jar outside archive"));
                    }
                    let mut comp = vec![0u8; entry.comp_size as usize];
                    range.read_exact_at(data_off, &mut comp)?;
                    let inflated =
                        inflate_raw_rfc1951(&comp, entry.uncomp_size as usize)?;
                    if inflated.len() as u64 != entry.uncomp_size {
                        return Err(ZipError::Malformed("nested jar size mismatch"));
                    }
                    let children = {
                        let mut slice = SliceRange { buf: &inflated };
                        read_archive_index(&mut slice)?
                    };
                    (data_off, inflated, children)
                };
                let archive_id = index.sources.len() as u16;
                index.sources.push(ArchiveSource::Inflated(inflated));
                if filter(&entry.name) {
                    index.entries.push(ZipEntryMeta {
                        archive_id: 0,
                        name: entry.name.clone(),
                        local_header_offset: entry.local_off,
                        compressed_size: entry.comp_size,
                        uncompressed_size: entry.uncomp_size,
                        compression_method: entry.method,
                        nested_range: Some((data_off, entry.comp_size)),
                    });
                }
                for child in children {
                    if child.local_off + LFH_FIXED as u64 > entry.uncomp_size {
                        return Err(ZipError::Malformed("nested local header outside jar"));
                    }
                    if filter(&child.name) {
                        index.entries.push(ZipEntryMeta {
                            archive_id,
                            name: child.name,
                            local_header_offset: child.local_off,
                            compressed_size: child.comp_size,
                            uncompressed_size: child.uncomp_size,
                            compression_method: child.method,
                            nested_range: Some((data_off, entry.comp_size)),
                        });
                    }
                }
            } else if filter(&entry.name) {
                index.entries.push(ZipEntryMeta {
                    archive_id: 0,
                    name: entry.name,
                    local_header_offset: entry.local_off,
                    compressed_size: entry.comp_size,
                    uncompressed_size: entry.uncomp_size,
                    compression_method: entry.method,
                    nested_range: None,
                });
            }
        }
        index.bytes_read_for_index = counted;
        Ok(index)
    }

    /// All indexed members (including nested-JAR children).
    pub fn entries(&self) -> &[ZipEntryMeta] {
        &self.entries
    }

    /// Bytes read while building the index — proves index I/O stays tiny
    /// relative to archive size (EOCD tail + Central Directory + nested
    /// headers only).
    pub fn bytes_read_for_index(&self) -> u64 {
        self.bytes_read_for_index
    }

    /// Fetch and decompress one entry, fail-closed.
    ///
    /// Seeks to the entry's Local File Header, skips the variable
    /// name/extra fields, reads only `compressed_size` bytes, and inflates
    /// when the method is Deflate — all bounded by `max_bytes`.
    pub fn read_entry(&self, name: &str, max_bytes: usize) -> Result<Vec<u8>, ZipError> {
        let entry = self
            .entries
            .iter()
            .find(|e| e.name == name)
            .ok_or_else(|| ZipError::EntryNotFound(name.to_owned()))?;
        if entry.uncompressed_size > max_bytes as u64 {
            return Err(ZipError::EntryTooLarge {
                uncompressed_size: entry.uncompressed_size,
                max_bytes: max_bytes as u64,
            });
        }
        match entry.compression_method {
            METHOD_STORED | METHOD_DEFLATE => {}
            other => return Err(ZipError::UnsupportedMethod(other)),
        }

        let comp = match &self.sources[entry.archive_id as usize] {
            ArchiveSource::ParentFile => {
                let mut file = self.file.lock().map_err(|_| ZipError::LockPoisoned)?;
                let mut hdr = [0u8; LFH_FIXED];
                file.seek(SeekFrom::Start(entry.local_header_offset))?;
                file.read_exact(&mut hdr)?;
                if u32le(&hdr, 0) != SIG_LFH {
                    return Err(ZipError::Malformed("bad local file header signature"));
                }
                let data_off = entry.local_header_offset
                    + LFH_FIXED as u64
                    + u16le(&hdr, 26) as u64
                    + u16le(&hdr, 28) as u64;
                if data_off + entry.compressed_size > self.file_len {
                    return Err(ZipError::Malformed("entry data outside archive"));
                }
                let mut comp = vec![0u8; entry.compressed_size as usize];
                file.seek(SeekFrom::Start(data_off))?;
                file.read_exact(&mut comp)?;
                comp
            }
            ArchiveSource::Inflated(buf) => {
                let end = usize::try_from(entry.local_header_offset)
                    .ok()
                    .and_then(|s| s.checked_add(LFH_FIXED))
                    .ok_or(ZipError::Truncated)?;
                let hdr = buf
                    .get(usize::try_from(entry.local_header_offset).map_err(|_| {
                        ZipError::Truncated
                    })?..end)
                    .ok_or(ZipError::Truncated)?;
                if u32le(hdr, 0) != SIG_LFH {
                    return Err(ZipError::Malformed("bad local file header signature"));
                }
                let data_off = usize::try_from(entry.local_header_offset)
                    .map_err(|_| ZipError::Truncated)?
                    + LFH_FIXED
                    + u16le(hdr, 26) as usize
                    + u16le(hdr, 28) as usize;
                let end = data_off
                    .checked_add(usize::try_from(entry.compressed_size).map_err(|_| {
                        ZipError::Malformed("entry too large for host")
                    })?)
                    .ok_or(ZipError::Truncated)?;
                buf.get(data_off..end)
                    .ok_or(ZipError::Truncated)?
                    .to_vec()
            }
        };

        match entry.compression_method {
            METHOD_STORED => {
                if comp.len() as u64 != entry.uncompressed_size {
                    return Err(ZipError::Malformed("stored entry size mismatch"));
                }
                Ok(comp)
            }
            METHOD_DEFLATE => {
                let out = inflate_raw_rfc1951(&comp, max_bytes)?;
                if out.len() as u64 != entry.uncompressed_size {
                    return Err(ZipError::Malformed("inflated size mismatch"));
                }
                Ok(out)
            }
            other => Err(ZipError::UnsupportedMethod(other)),
        }
    }
}
