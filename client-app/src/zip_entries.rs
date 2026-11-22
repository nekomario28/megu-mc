// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Bounded, path-traversal-safe ZIP reader used to pull `sounds.json` and
//! `.ogg` payloads straight out of Mod JARs / resource-pack archives into
//! memory — zero files are ever extracted to disk.
//!
//! Safety contract (fail-closed):
//! * multi-disk archives, ZIP64 indicators, encryption, and unknown
//!   compression methods are rejected/skipped;
//! * central-directory size, entry count, name length, compressed size, and
//!   uncompressed size are all bounded before any allocation;
//! * entry names with `..` components, absolute prefixes, backslashes, or
//!   NUL bytes are dropped during indexing, so lookups can never escape the
//!   archive;
//! * CRC32 of the decoded payload is verified before bytes are returned.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

const EOCD_SIG: u32 = 0x0605_4b50;
const CD_SIG: u32 = 0x0201_4b50;
const LOCAL_SIG: u32 = 0x0403_4b50;

/// EOCD is 22 bytes plus a comment of at most 64 KiB.
const EOCD_SEARCH: u64 = 22 + 65_536;
/// Central directory must fit in memory.
const MAX_CD_BYTES: u64 = 16 * 1024 * 1024;
/// Archive entries we are willing to index.
const MAX_ENTRIES: u32 = 65_535;
/// Entry path length cap (`assets/<ns>/sounds/**` is far below this).
const MAX_NAME_LEN: usize = 512;
/// Compressed payload we will read into memory for one entry.
const MAX_COMP_BYTES: u64 = 96 * 1024 * 1024;
/// Whole archive sanity cap (2 GiB).
const MAX_ARCHIVE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// One indexed central-directory entry.
struct ZipEntry {
    name: String,
    method: u32,
    crc32: u32,
    comp_size: u32,
    uncomp_size: u32,
    local_off: u64,
}

/// Parsed, safety-indexed ZIP/JAR archive. Cheap to construct; entries are
/// decompressed on demand via [`Self::read_entry`].
pub struct ZipArchive {
    path: PathBuf,
    entries: Vec<ZipEntry>,
}

/// `name` is a safe relative archive path when it uses `/` separators, has
/// no `..` components, no drive/absolute prefix, no NUL, and no backslash.
fn safe_entry_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && !name.contains('\0')
        && !name.contains('\\')
        && !name.starts_with('/')
        && !name.starts_with("~/")
        && !name.as_bytes().get(1).is_some_and(|b| *b == b':')
        && name.split('/').all(|part| part != "..")
}

fn rd_u16(b: &[u8], off: usize) -> u32 {
    u32::from(u16::from_le_bytes([b[off], b[off + 1]]))
}

fn rd_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

impl ZipArchive {
    /// Open and index `path`. Fails cleanly on unreadable, truncated,
    /// multi-disk, ZIP64, or oversized archives — never panics.
    pub fn open(path: &Path) -> Result<Self, String> {
        let mut file =
            File::open(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
        let len = file
            .metadata()
            .map_err(|e| format!("cannot stat {}: {e}", path.display()))?
            .len();
        if len < 22 {
            return Err(format!("{} is too small to be a zip", path.display()));
        }
        if len > MAX_ARCHIVE_BYTES {
            return Err(format!("{} exceeds the 2 GiB archive cap", path.display()));
        }

        // EOCD lives in the last ≤66 KiB; scan the tail backwards.
        let tail_len = len.min(EOCD_SEARCH);
        let mut tail = vec![0u8; tail_len as usize];
        file.seek(SeekFrom::Start(len - tail_len))
            .map_err(|e| format!("seek {}: {e}", path.display()))?;
        file.read_exact(&mut tail)
            .map_err(|e| format!("read {}: {e}", path.display()))?;
        let mut eocd = None;
        for i in (0..tail.len().saturating_sub(3)).rev() {
            if rd_u32(&tail[i..], 0) == EOCD_SIG {
                eocd = Some(i);
                break;
            }
        }
        let i = eocd.ok_or_else(|| format!("{}: no end-of-central-directory", path.display()))?;
        if i + 22 > tail.len() {
            return Err(format!("{}: truncated EOCD", path.display()));
        }
        let e = &tail[i..];
        let disk_no = rd_u16(e, 4);
        let cd_disk = rd_u16(e, 6);
        let entries_disk = rd_u16(e, 8);
        let entries_total = rd_u16(e, 10);
        let cd_size = u64::from(rd_u32(e, 12));
        let cd_off = u64::from(rd_u32(e, 16));
        if disk_no != 0 || cd_disk != 0 || entries_disk != entries_total {
            return Err(format!("{}: multi-disk archives unsupported", path.display()));
        }
        if cd_size == 0xFFFF_FFFF || cd_off == 0xFFFF_FFFF || entries_total == 0xFFFF {
            return Err(format!("{}: ZIP64 archives unsupported", path.display()));
        }
        if u64::from(entries_total) > u64::from(MAX_ENTRIES)
            || cd_size > MAX_CD_BYTES
            || cd_off.saturating_add(cd_size) > len
        {
            return Err(format!("{}: central directory out of bounds", path.display()));
        }

        let mut cd = vec![0u8; cd_size as usize];
        file.seek(SeekFrom::Start(cd_off))
            .map_err(|e| format!("seek {}: {e}", path.display()))?;
        file.read_exact(&mut cd)
            .map_err(|e| format!("read {}: {e}", path.display()))?;

        let mut entries = Vec::with_capacity((entries_total as usize).min(4096));
        let mut pos = 0usize;
        for _ in 0..entries_total {
            if pos + 46 > cd.len() || rd_u32(&cd[pos..], 0) != CD_SIG {
                return Err(format!("{}: malformed central directory", path.display()));
            }
            let flags = rd_u16(&cd, pos + 8);
            let method = rd_u16(&cd, pos + 10);
            let crc32 = rd_u32(&cd, pos + 16);
            let comp_size = rd_u32(&cd, pos + 20);
            let uncomp_size = rd_u32(&cd, pos + 24);
            let name_len = rd_u16(&cd, pos + 28) as usize;
            let extra_len = rd_u16(&cd, pos + 30) as usize;
            let comment_len = rd_u16(&cd, pos + 32) as usize;
            let local_off = rd_u32(&cd, pos + 42);
            let name_end = pos + 46 + name_len;
            let rec_end = name_end + extra_len + comment_len;
            if rec_end > cd.len() {
                return Err(format!("{}: malformed central directory", path.display()));
            }
            // ZIP64 sentinel or directory entry → indexable only when sane.
            let name_bytes = &cd[pos + 46..name_end];
            if let Ok(name) = std::str::from_utf8(name_bytes) {
                if safe_entry_name(name)
                    && !name.ends_with('/')
                    && flags & 1 == 0
                    && u64::from(comp_size) <= MAX_COMP_BYTES
                    && comp_size != 0xFFFF_FFFF
                    && uncomp_size != 0xFFFF_FFFF
                {
                    entries.push(ZipEntry {
                        name: name.to_string(),
                        method,
                        crc32,
                        comp_size,
                        uncomp_size,
                        local_off: u64::from(local_off),
                    });
                }
            }
            pos = rec_end;
        }
        Ok(Self {
            path: path.to_path_buf(),
            entries,
        })
    }

    /// All indexed (safe) entry names.
    pub fn entry_names(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|e| e.name.as_str())
    }

    /// Archive path this index was opened from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// `true` when `name` is an indexed entry (no IO).
    pub fn contains(&self, name: &str) -> bool {
        self.entries.iter().any(|e| e.name == name)
    }

    /// Decompress `name` into memory, bounded to `max_out` bytes. Returns
    /// `Ok(None)` when the entry is absent or was dropped as unsafe; `Err`
    /// when the entry exists but cannot be decoded (truncated archive,
    /// unknown compression method, CRC mismatch, corrupt deflate stream).
    /// Never touches the filesystem beyond this archive and never writes.
    pub fn read_entry(&self, name: &str, max_out: usize) -> Result<Option<Vec<u8>>, String> {
        let Some(entry) = self.entries.iter().find(|e| e.name == name) else {
            return Ok(None);
        };
        if u64::from(entry.uncomp_size) > max_out as u64 {
            return Err(format!(
                "zip entry {name} decompressed size {} exceeds bound {max_out}",
                entry.uncomp_size
            ));
        }
        let mut file = File::open(&self.path)
            .map_err(|e| format!("cannot open {}: {e}", self.path.display()))?;
        let mut hdr = [0u8; 30];
        file.seek(SeekFrom::Start(entry.local_off))
            .map_err(|e| format!("seek {}: {e}", self.path.display()))?;
        file.read_exact(&mut hdr)
            .map_err(|e| format!("read {}: {e}", self.path.display()))?;
        if rd_u32(&hdr, 0) != LOCAL_SIG {
            return Err(format!("{}: bad local header for {name}", self.path.display()));
        }
        let data_off = entry.local_off
            + 30
            + u64::from(rd_u16(&hdr, 26))
            + u64::from(rd_u16(&hdr, 28));
        let mut comp = vec![0u8; entry.comp_size as usize];
        file.seek(SeekFrom::Start(data_off))
            .map_err(|e| format!("seek {}: {e}", self.path.display()))?;
        file.read_exact(&mut comp)
            .map_err(|e| format!("read {}: {e}", self.path.display()))?;

        // Data descriptors carry CRC after the payload — the central
        // directory already supplied it, so flag bit 3 needs no extra parse.
        let data = match entry.method {
            0 => comp,
            8 => inflate(&comp, max_out)
                .map_err(|e| format!("zip entry {name}: {e}"))?,
            m => return Err(format!("zip entry {name}: method {m} unsupported")),
        };
        if data.len() != entry.uncomp_size as usize {
            return Err(format!(
                "zip entry {name}: size mismatch {} != {}",
                data.len(),
                entry.uncomp_size
            ));
        }
        if crc32(&data) != entry.crc32 {
            return Err(format!("zip entry {name}: CRC32 mismatch"));
        }
        Ok(Some(data))
    }
}

/// IEEE CRC32 (same polynomial as PNG/zlib) — also used by tests to author
/// clean-room archives.
pub fn crc32(data: &[u8]) -> u32 {
    // Bitwise implementation: a 256-entry table build costs a const lookup
    // anyway, and audio payloads are ≤64 MiB so speed is not the constraint.
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

// ---------------------------------------------------------------------------
// Clean-room RFC 1951 inflate (stored / fixed-Huffman / dynamic-Huffman).
// ---------------------------------------------------------------------------

struct BitReader<'a> {
    data: &'a [u8],
    byte: usize,
    bit: u32,
    acc: u64,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            byte: 0,
            bit: 0,
            acc: 0,
        }
    }

    #[inline]
    fn bits(&mut self, n: u32) -> Result<u32, String> {
        while self.bit < n {
            let Some(&b) = self.data.get(self.byte) else {
                return Err("deflate: unexpected end of stream".to_string());
            };
            self.acc |= u64::from(b) << self.bit;
            self.byte += 1;
            self.bit += 8;
        }
        let v = (self.acc & ((1u64 << n) - 1)) as u32;
        self.acc >>= n;
        self.bit -= n;
        Ok(v)
    }

    /// Discard pending bits and take `n` raw bytes (stored blocks).
    fn take_aligned(&mut self, n: usize) -> Result<&'a [u8], String> {
        self.bit = 0;
        self.acc = 0;
        let end = self.byte.saturating_add(n);
        if end > self.data.len() {
            return Err("deflate: stored block overruns input".to_string());
        }
        let s = &self.data[self.byte..end];
        self.byte = end;
        Ok(s)
    }
}

/// Canonical Huffman decoder: `counts[l]` = symbol count with code length l,
/// `symbols` sorted by (length, symbol).
struct Huffman {
    counts: [u16; 16],
    symbols: Vec<u16>,
}

impl Huffman {
    fn new(lengths: &[u8]) -> Result<Self, String> {
        let mut counts = [0u16; 16];
        for &l in lengths {
            if l > 15 {
                return Err("deflate: code length > 15".to_string());
            }
            if l > 0 {
                counts[l as usize] += 1;
            }
        }
        // Over-subscribed / incomplete-code sanity: Kraft sum must stay ≤ 1.
        let mut left = 1i32;
        for &c in counts.iter().skip(1) {
            left <<= 1;
            left -= i32::from(c);
            if left < 0 {
                return Err("deflate: over-subscribed code".to_string());
            }
        }
        let mut offsets = [0u16; 16];
        for l in 1..15 {
            offsets[l + 1] = offsets[l] + counts[l];
        }
        let mut symbols = vec![0u16; lengths.iter().filter(|&&l| l > 0).count()];
        for (sym, &l) in lengths.iter().enumerate() {
            if l > 0 {
                symbols[offsets[l as usize] as usize] = sym as u16;
                offsets[l as usize] += 1;
            }
        }
        Ok(Self { counts, symbols })
    }

    fn decode(&self, br: &mut BitReader) -> Result<u16, String> {
        let mut code = 0u32;
        let mut first = 0u32;
        let mut index = 0u32;
        for len in 1..16u32 {
            code |= br.bits(1)?;
            let count = u32::from(self.counts[len as usize]);
            if code < first + count {
                return Ok(self.symbols[(index + code - first) as usize]);
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        Err("deflate: invalid code".to_string())
    }
}

const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115,
    131, 163, 195, 227, 258,
];
const LENGTH_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025,
    1537, 2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12,
    12, 13, 13,
];
/// Order in which code-length code lengths are stored.
const CLEN_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

fn fixed_tables() -> (Huffman, Huffman) {
    let mut lit_lens = [0u8; 288];
    for (i, l) in lit_lens.iter_mut().enumerate() {
        *l = match i {
            0..=143 => 8,
            144..=255 => 9,
            256..=279 => 7,
            _ => 8,
        };
    }
    let lit = Huffman::new(&lit_lens).expect("fixed lit table is valid");
    let dist = Huffman::new(&[5u8; 30]).expect("fixed dist table is valid");
    (lit, dist)
}

fn decode_block(
    br: &mut BitReader,
    lit: &Huffman,
    dist: &Huffman,
    out: &mut Vec<u8>,
    max_out: usize,
) -> Result<(), String> {
    loop {
        let sym = lit.decode(br)?;
        match sym {
            0..=255 => {
                if out.len() >= max_out {
                    return Err("deflate: output bound exceeded".to_string());
                }
                out.push(sym as u8);
            }
            256 => return Ok(()),
            257..=285 => {
                let i = (sym - 257) as usize;
                let len =
                    usize::from(LENGTH_BASE[i]) + br.bits(u32::from(LENGTH_EXTRA[i]))? as usize;
                let dsym = dist.decode(br)? as usize;
                if dsym >= DIST_BASE.len() {
                    return Err("deflate: bad distance code".to_string());
                }
                let d =
                    usize::from(DIST_BASE[dsym]) + br.bits(u32::from(DIST_EXTRA[dsym]))? as usize;
                if d > out.len() || d > 32_768 {
                    return Err("deflate: distance too far back".to_string());
                }
                if out.len().saturating_add(len) > max_out {
                    return Err("deflate: output bound exceeded".to_string());
                }
                let start = out.len() - d;
                for k in 0..len {
                    let b = out[start + k];
                    out.push(b);
                }
            }
            _ => return Err("deflate: bad literal/length code".to_string()),
        }
    }
}

/// Inflate a raw DEFLATE stream into `Vec<u8>` capped at `max_out` bytes.
/// Pure-mem, fail-closed: any structural violation returns `Err`.
pub fn inflate(compressed: &[u8], max_out: usize) -> Result<Vec<u8>, String> {
    let mut br = BitReader::new(compressed);
    let mut out = Vec::new();
    loop {
        let final_block = br.bits(1)? == 1;
        match br.bits(2)? {
            0 => {
                let len = br.take_aligned(4)?;
                let n = usize::from(len[0]) | usize::from(len[1]) << 8;
                let nn = usize::from(len[2]) | usize::from(len[3]) << 8;
                if n != (!nn & 0xFFFF) {
                    return Err("deflate: bad stored length".to_string());
                }
                if out.len().saturating_add(n) > max_out {
                    return Err("deflate: output bound exceeded".to_string());
                }
                out.extend_from_slice(br.take_aligned(n)?);
            }
            1 => {
                let (lit, dist) = fixed_tables();
                decode_block(&mut br, &lit, &dist, &mut out, max_out)?;
            }
            2 => {
                let hlit = br.bits(5)? as usize + 257;
                let hdist = br.bits(5)? as usize + 1;
                let hclen = br.bits(4)? as usize + 4;
                if hlit > 286 || hdist > 30 {
                    return Err("deflate: table size out of range".to_string());
                }
                let mut clen = [0u8; 19];
                for &ord in CLEN_ORDER.iter().take(hclen) {
                    clen[ord] = br.bits(3)? as u8;
                }
                let clen_h = Huffman::new(&clen)?;
                let mut lens = vec![0u8; hlit + hdist];
                let mut i = 0usize;
                while i < lens.len() {
                    let sym = clen_h.decode(&mut br)?;
                    match sym {
                        0..=15 => {
                            lens[i] = sym as u8;
                            i += 1;
                        }
                        16 => {
                            if i == 0 {
                                return Err("deflate: repeat without prior length".to_string());
                            }
                            let rep = 3 + br.bits(2)? as usize;
                            if i + rep > lens.len() {
                                return Err("deflate: repeat overflows table".to_string());
                            }
                            let v = lens[i - 1];
                            for _ in 0..rep {
                                lens[i] = v;
                                i += 1;
                            }
                        }
                        17 | 18 => {
                            let rep = if sym == 17 {
                                3 + br.bits(3)? as usize
                            } else {
                                11 + br.bits(7)? as usize
                            };
                            if i + rep > lens.len() {
                                return Err("deflate: zero run overflows table".to_string());
                            }
                            i += rep;
                        }
                        _ => return Err("deflate: bad code-length symbol".to_string()),
                    }
                }
                let lit = Huffman::new(&lens[..hlit])?;
                let dist = Huffman::new(&lens[hlit..])?;
                decode_block(&mut br, &lit, &dist, &mut out, max_out)?;
            }
            _ => return Err("deflate: reserved block type".to_string()),
        }
        if final_block {
            return Ok(out);
        }
    }
}
