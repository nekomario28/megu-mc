// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Bounded, path-traversal-safe ZIP reader used to pull `sounds.json` and
//! `.ogg` payloads straight out of Mod JARs / resource-pack archives into
//! memory — zero files are ever extracted to disk.
//!
//! All EOCD/Central-Directory parsing, entry-name validation, ZIP64,
//! multi-disk, encryption, traversal, and decompression-bomb checks are
//! delegated to the canonical [`megu_core::zip_archive::LazyZipIndex`]; this
//! module keeps only the archive-size sanity cap and the CRC-32 verification
//! the asset pipeline relies on.
//!
//! Safety contract (fail-closed):
//! * multi-disk archives, encrypted entries, and unknown compression
//!   methods are rejected by the core index;
//! * entry names with `..` components, absolute prefixes, backslashes, or
//!   NUL bytes fail the whole open, so lookups can never escape the
//!   archive;
//! * CRC32 of the decoded payload is verified before bytes are returned.

use std::path::{Path, PathBuf};

/// Whole archive sanity cap (2 GiB).
const MAX_ARCHIVE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Parsed, safety-indexed ZIP/JAR archive. Cheap to construct; entries are
/// decompressed on demand via [`Self::read_entry`].
pub struct ZipArchive {
    path: PathBuf,
    index: megu_core::zip_archive::LazyZipIndex,
}

impl ZipArchive {
    /// Open and index `path`. Fails cleanly on unreadable, truncated,
    /// multi-disk, encrypted, traversing, or oversized archives — never
    /// panics.
    pub fn open(path: &Path) -> Result<Self, String> {
        let len = std::fs::metadata(path)
            .map_err(|e| format!("cannot stat {}: {e}", path.display()))?
            .len();
        if len > MAX_ARCHIVE_BYTES {
            return Err(format!("{} exceeds the 2 GiB archive cap", path.display()));
        }
        let index = megu_core::zip_archive::LazyZipIndex::open_all(path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Self {
            path: path.to_path_buf(),
            index,
        })
    }

    /// All indexed (safe) entry names.
    pub fn entry_names(&self) -> impl Iterator<Item = &str> {
        self.index.entries().iter().map(|e| e.name.as_str())
    }

    /// Archive path this index was opened from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// `true` when `name` is an indexed entry (no IO).
    pub fn contains(&self, name: &str) -> bool {
        self.index.entries().iter().any(|e| e.name == name)
    }

    /// Compressed size in bytes of an indexed entry — a cheap size hint
    /// for decode-queue budgeting; `None` when the entry is absent. The
    /// index is already in memory, so this performs no IO.
    pub fn entry_comp_size(&self, name: &str) -> Option<u64> {
        self.index
            .entries()
            .iter()
            .find(|e| e.name == name && e.archive_id == 0)
            .or_else(|| self.index.entries().iter().find(|e| e.name == name))
            .map(|e| e.compressed_size)
    }

    /// Decompress `name` into memory, bounded to `max_out` bytes. Returns
    /// `Ok(None)` when the entry is absent; `Err` when the entry exists but
    /// cannot be decoded (truncated archive, unknown compression method,
    /// CRC mismatch, corrupt deflate stream). Never touches the filesystem
    /// beyond this archive and never writes.
    pub fn read_entry(&self, name: &str, max_out: usize) -> Result<Option<Vec<u8>>, String> {
        // Mirror the core lookup preference: top-level members win over
        // same-named nested-JAR children.
        let meta = self
            .index
            .entries()
            .iter()
            .find(|e| e.name == name && e.archive_id == 0)
            .or_else(|| self.index.entries().iter().find(|e| e.name == name));
        let Some(meta) = meta else {
            return Ok(None);
        };
        let crc32 = meta.crc32;
        let data = self
            .index
            .read_entry(name, max_out)
            .map_err(|e| format!("zip entry {name}: {e}"))?;
        if megu_core::hash::crc32_ieee(&data) != crc32 {
            return Err(format!("zip entry {name}: CRC32 mismatch"));
        }
        Ok(Some(data))
    }
}
