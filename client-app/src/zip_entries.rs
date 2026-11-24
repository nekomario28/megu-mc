// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Bounded, path-traversal-safe ZIP reader used to pull `sounds.json` and
//! `.ogg` payloads straight out of Mod JARs / resource-pack archives into
//! memory — zero files are ever extracted to disk.
//!
//! EOCD/Central-Directory parsing, ZIP64, multi-disk, encryption, and
//! decompression-bomb checks are delegated to the canonical
//! [`megu_core::zip_archive::LazyZipIndex`]. That index fails the whole open
//! on the first unsafe entry name; real-world JARs occasionally carry a
//! single `../` traversal probe alongside thousands of valid members, so on
//! open failure this module falls back to [`tolerant_index`], which walks the
//! same Central Directory but *drops* unsafe entries during indexing — the
//! pre-refactor contract — while keeping every other check fail-closed.
//!
//! Safety contract (fail-closed):
//! * multi-disk archives, ZIP64 indicators, and oversized central directories
//!   are rejected;
//! * entry names with `..` components, absolute prefixes, backslashes, or
//!   NUL bytes are dropped during indexing, so lookups can never escape the
//!   archive;
//! * encrypted entries and compressed payloads over the read bound are
//!   dropped/errored before any allocation;
//! * CRC32 of the decoded payload is verified before bytes are returned.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Whole archive sanity cap (2 GiB).
const MAX_ARCHIVE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

// Tolerant-fallback constants (mirror the canonical index's budgets).
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

/// Parsed, safety-indexed ZIP/JAR archive. Cheap to construct; entries are
/// decompressed on demand via [`Self::read_entry`].
pub struct ZipArchive {
    path: PathBuf,
    index: Index,
}

enum Index {
    /// Canonical lazy index: EOCD + Central Directory only, nested-JAR
    /// descent, in-memory or on-demand payload reads.
    Core(megu_core::zip_archive::LazyZipIndex),
    /// Fallback for archives the canonical index rejects wholesale because
    /// of one or more unsafe entry names: same Central Directory walk, but
    /// unsafe entries are dropped during indexing. Does not descend into
    /// `META-INF/jars/*.jar` nested archives.
    Tolerant(TolerantIndex),
}

/// One indexed central-directory entry in the tolerant fallback.
struct TolerantEntry {
    name: String,
    method: u16,
    crc32: u32,
    comp_size: u32,
    uncomp_size: u32,
    local_off: u64,
}

struct TolerantIndex {
    entries: Vec<TolerantEntry>,
}

fn rd_u16(b: &[u8], off: usize) -> u32 {
    u32::from(u16::from_le_bytes([b[off], b[off + 1]]))
}

fn rd_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

/// `name` is indexable when it is a safe relative archive path: `/`
/// separators only, no `..`/`.`/empty components, no backslash, drive, NUL,
/// or `~` home prefix. Delegates the component rules to the canonical
/// validator and adds the legacy length/home-prefix caps on top.
fn tolerant_safe_name(name: &str) -> bool {
    name.len() <= MAX_NAME_LEN
        && !name.starts_with("~/")
        && megu_core::zip_archive::validate_entry_name(name).is_ok()
}

/// Walk EOCD + Central Directory like the pre-refactor reader: every record
/// is bounds-checked, but unsafe/encrypted/sentinel entries are skipped
/// instead of aborting the whole archive.
fn tolerant_index(path: &Path, len: u64) -> Result<TolerantIndex, String> {
    let mut file =
        File::open(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    if len < 22 {
        return Err(format!("{} is too small to be a zip", path.display()));
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
        if let Ok(name) = std::str::from_utf8(&cd[pos + 46..name_end]) {
            if tolerant_safe_name(name)
                && flags & 1 == 0
                && u64::from(comp_size) <= MAX_COMP_BYTES
                && comp_size != 0xFFFF_FFFF
                && uncomp_size != 0xFFFF_FFFF
            {
                entries.push(TolerantEntry {
                    name: name.to_string(),
                    method: method as u16,
                    crc32,
                    comp_size,
                    uncomp_size,
                    local_off: u64::from(local_off),
                });
            }
        }
        pos = rec_end;
    }
    Ok(TolerantIndex { entries })
}

/// Tolerant-path payload read: seek the Local File Header, bound the read to
/// the declared compressed size, decode Stored/Deflate via the canonical
/// inflater, and verify declared size + CRC-32 before returning bytes.
fn tolerant_read_entry(
    path: &Path,
    index: &TolerantIndex,
    name: &str,
    max_out: usize,
) -> Result<Option<Vec<u8>>, String> {
    let Some(entry) = index.entries.iter().find(|e| e.name == name) else {
        return Ok(None);
    };
    if u64::from(entry.uncomp_size) > max_out as u64 {
        return Err(format!(
            "zip entry {name} decompressed size {} exceeds bound {max_out}",
            entry.uncomp_size
        ));
    }
    let mut file = File::open(path)
        .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    let mut hdr = [0u8; 30];
    file.seek(SeekFrom::Start(entry.local_off))
        .map_err(|e| format!("seek {}: {e}", path.display()))?;
    file.read_exact(&mut hdr)
        .map_err(|e| format!("read {}: {e}", path.display()))?;
    if rd_u32(&hdr, 0) != LOCAL_SIG {
        return Err(format!("{}: bad local header for {name}", path.display()));
    }
    let data_off = entry.local_off
        + 30
        + u64::from(rd_u16(&hdr, 26))
        + u64::from(rd_u16(&hdr, 28));
    let mut comp = vec![0u8; entry.comp_size as usize];
    file.seek(SeekFrom::Start(data_off))
        .map_err(|e| format!("seek {}: {e}", path.display()))?;
    file.read_exact(&mut comp)
        .map_err(|e| format!("read {}: {e}", path.display()))?;

    // Data descriptors carry CRC after the payload — the central directory
    // already supplied it, so flag bit 3 needs no extra parse.
    let data = match entry.method {
        0 => comp,
        8 => megu_core::zip_archive::inflate_raw_rfc1951(&comp, max_out)
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
    if megu_core::hash::crc32_ieee(&data) != entry.crc32 {
        return Err(format!("zip entry {name}: CRC32 mismatch"));
    }
    Ok(Some(data))
}

impl ZipArchive {
    /// Open and index `path`. Fails cleanly on unreadable, truncated,
    /// multi-disk, ZIP64, or oversized archives — never panics. Unsafe entry
    /// names are dropped during indexing rather than failing the open, so a
    /// single traversal probe cannot hide the archive's valid members.
    pub fn open(path: &Path) -> Result<Self, String> {
        let len = std::fs::metadata(path)
            .map_err(|e| format!("cannot stat {}: {e}", path.display()))?
            .len();
        if len > MAX_ARCHIVE_BYTES {
            return Err(format!("{} exceeds the 2 GiB archive cap", path.display()));
        }
        let index = match megu_core::zip_archive::LazyZipIndex::open_all(path) {
            Ok(core) => Index::Core(core),
            Err(core_err) => match tolerant_index(path, len) {
                Ok(tolerant) => Index::Tolerant(tolerant),
                Err(_) => return Err(format!("{}: {core_err}", path.display())),
            },
        };
        Ok(Self {
            path: path.to_path_buf(),
            index,
        })
    }

    /// All indexed (safe) entry names.
    pub fn entry_names(&self) -> impl Iterator<Item = &str> {
        let iter: Box<dyn Iterator<Item = &str> + '_> = match &self.index {
            Index::Core(index) => Box::new(index.entries().iter().map(|e| e.name.as_str())),
            Index::Tolerant(index) => {
                Box::new(index.entries.iter().map(|e| e.name.as_str()))
            }
        };
        iter
    }

    /// Archive path this index was opened from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// `true` when `name` is an indexed entry (no IO).
    pub fn contains(&self, name: &str) -> bool {
        match &self.index {
            Index::Core(index) => index.entries().iter().any(|e| e.name == name),
            Index::Tolerant(index) => index.entries.iter().any(|e| e.name == name),
        }
    }

    /// Compressed size in bytes of an indexed entry — a cheap size hint
    /// for decode-queue budgeting; `None` when the entry is absent. The
    /// index is already in memory, so this performs no IO.
    pub fn entry_comp_size(&self, name: &str) -> Option<u64> {
        match &self.index {
            Index::Core(index) => index
                .entries()
                .iter()
                .find(|e| e.name == name && e.archive_id == 0)
                .or_else(|| index.entries().iter().find(|e| e.name == name))
                .map(|e| e.compressed_size),
            Index::Tolerant(index) => index
                .entries
                .iter()
                .find(|e| e.name == name)
                .map(|e| u64::from(e.comp_size)),
        }
    }

    /// Decompress `name` into memory, bounded to `max_out` bytes. Returns
    /// `Ok(None)` when the entry is absent or was dropped as unsafe; `Err`
    /// when the entry exists but cannot be decoded (truncated archive,
    /// unknown compression method, CRC mismatch, corrupt deflate stream).
    /// Never touches the filesystem beyond this archive and never writes.
    pub fn read_entry(&self, name: &str, max_out: usize) -> Result<Option<Vec<u8>>, String> {
        match &self.index {
            Index::Core(index) => {
                // Mirror the core lookup preference: top-level members win
                // over same-named nested-JAR children.
                let meta = index
                    .entries()
                    .iter()
                    .find(|e| e.name == name && e.archive_id == 0)
                    .or_else(|| index.entries().iter().find(|e| e.name == name));
                let Some(meta) = meta else {
                    return Ok(None);
                };
                let crc32 = meta.crc32;
                let data = index
                    .read_entry(name, max_out)
                    .map_err(|e| format!("zip entry {name}: {e}"))?;
                if megu_core::hash::crc32_ieee(&data) != crc32 {
                    return Err(format!("zip entry {name}: CRC32 mismatch"));
                }
                Ok(Some(data))
            }
            Index::Tolerant(index) => tolerant_read_entry(&self.path, index, name, max_out),
        }
    }
}
