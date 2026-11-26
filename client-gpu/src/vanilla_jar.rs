// SPDX-License-Identifier: LGPL-3.0-or-later
//! Bounded, path-traversal-safe reader for vanilla client JARs and
//! resource-pack `.zip` files — the launcher `--gameDir` asset path.
//!
//! A client JAR (`<gameDir>/versions/<ver>/<ver>.jar`) is an ordinary ZIP
//! archive. This module walks its central directory, extracts only the
//! asset paths a [`ResourcePackLayer`] understands, and fails closed at
//! [`MAX_ARCHIVE_BYTES`] compressed input, [`MAX_ARCHIVE_ENTRIES`] entries,
//! [`MAX_UNPACKED_BYTES`] total output and [`MAX_ENTRY_BYTES`] per file —
//! far above any real 1.21.x client JAR, far below anything that can hold
//! the event loop hostage. Entry names are validated before use, Stored
//! and Deflate methods are supported, and encrypted/ZIP64/multi-disk
//! archives are rejected outright.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::File;
use std::io::{Cursor, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use megu_client_mesh::asset_pipeline::{animation_from_mcmeta, ResourcePackLayer};
use megu_client_mesh::png_decode;
use megu_core::hash::crc32_ieee;
use megu_core::zip_archive::{inflate_raw_rfc1951_with_consumed, ZipError};

/// Total on-disk bytes the archive itself may occupy.
pub const MAX_ARCHIVE_BYTES: u64 = 256 * 1024 * 1024;
/// Central-directory entry count bound.
pub const MAX_ARCHIVE_ENTRIES: usize = 50_000;
/// Sum of uncompressed bytes the reader will materialize.
pub const MAX_UNPACKED_BYTES: u64 = 256 * 1024 * 1024;
/// Per-entry uncompressed cap (matches the asset pipeline's pack-file cap).
pub const MAX_ENTRY_BYTES: u64 = 8 * 1024 * 1024;
/// Cap the number of JARs and their aggregate materialized asset bytes.
pub const MAX_MOD_JARS: usize = 256;
pub const MAX_MOD_ASSET_BYTES: u64 = MAX_UNPACKED_BYTES;
/// Deepest Jar-in-Jar level scanned for assets: the outer `mods/*.jar` is
/// depth 0, its `META-INF/jarjar/*.jar` / `META-INF/jars/*.jar` children are
/// depth 1, and nesting inside those is followed once more (depth 2) before
/// any deeper `jarjar`/`jars` entries are ignored.
pub const MAX_JIJ_DEPTH: usize = 2;
/// Nested JARs opened per outer mod JAR.
pub const MAX_JIJ_JARS_PER_MOD: usize = 64;
/// Uncompressed byte cap for one nested JAR payload.
pub const MAX_JIJ_JAR_BYTES: u64 = 64 * 1024 * 1024;

/// Why a JAR/pack read failed — every variant is a fail-closed stop, never
/// a silent partial load.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JarError {
    /// Filesystem or archive-size violation before parsing began.
    Io { reason: String },
    /// The ZIP structure is corrupt or inconsistent.
    Malformed { reason: String },
    /// A valid-but-unsupported feature (ZIP64, multi-disk, encryption,
    /// non-Stored/Deflate method).
    Unsupported { reason: String },
    /// An entry name fails the traversal rules.
    UnsafePath { path: String },
    /// A budget bound was exceeded.
    BudgetExceeded { reason: String },
}

impl fmt::Display for JarError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { reason } => write!(f, "{reason}"),
            Self::Malformed { reason } => write!(f, "malformed archive: {reason}"),
            Self::Unsupported { reason } => write!(f, "unsupported archive feature: {reason}"),
            Self::UnsafePath { path } => write!(f, "archive entry path {path:?} is unsafe"),
            Self::BudgetExceeded { reason } => write!(f, "archive budget exceeded: {reason}"),
        }
    }
}

impl std::error::Error for JarError {}

/// What one archive read produced — load count plus every skip, so a
/// dropped entry can never look like a clean negative result.
#[derive(Clone, Debug, Default)]
pub struct JarReadReport {
    /// Central-directory entries inspected.
    pub entries_seen: usize,
    /// Entries selected, decompressed and handed to the layer builder.
    pub entries_loaded: usize,
    /// `(entry, reason)` for every selected entry that was rejected —
    /// path, size or decode failure.
    pub skipped: Vec<(String, String)>,
}

/// Per-JAR success/failure ledger for deterministic mod asset discovery.
#[derive(Debug, Default)]
pub struct ModJarLoadReport {
    pub loaded: Vec<PathBuf>,
    pub rejected: Vec<(PathBuf, JarError)>,
    pub entries_seen: usize,
    /// Formatted notes for nested Jar-in-Jar members that were skipped
    /// without failing the outer mod JAR (corrupt nested archive, depth cap,
    /// per-nested-JAR byte cap, ...).
    pub skipped: Vec<String>,
}

/// One decompressed archive member selected by the caller's filter.
#[derive(Clone, Debug)]
pub struct JarEntry {
    /// Archive-internal path (`assets/minecraft/textures/block/stone.png`).
    pub name: String,
    /// Uncompressed contents.
    pub bytes: Vec<u8>,
}

/// Entry-name validation shared by JAR and pack reads: no absolute paths,
/// no parent traversal, no backslash/drive-letter smuggling.
fn validate_entry_name(name: &str) -> Result<(), JarError> {
    let bad = || JarError::UnsafePath {
        path: name.to_owned(),
    };
    if name.is_empty()
        || name.starts_with('/')
        || name.contains('\\')
        || name.contains(':')
        || name.contains('\0')
    {
        return Err(bad());
    }
    for part in name.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return Err(bad());
        }
    }
    Ok(())
}

/// Read selected entries from the top-level client JAR.
pub fn read_selected_entries(
    path: &Path,
    select: impl Fn(&str) -> bool,
) -> Result<(Vec<JarEntry>, JarReadReport), JarError> {
    AssetLayer::open(path, false, select)?.collect(MAX_UNPACKED_BYTES)
}

pub(crate) const MAX_PACK_LAYERS: usize = 64;
/// The input order is Vanilla's low-to-high order. Keep the top of a
/// stack that exceeds the admission limit.
pub(crate) fn prioritized_packs<T>(packs: &[T]) -> &[T] {
    &packs[packs.len().saturating_sub(MAX_PACK_LAYERS)..]
}

pub(crate) const MAX_DECODED_RGBA_BYTES: usize = 128 * 1024 * 1024;
pub(crate) const MAX_VISUAL_ATLAS_DIM: u32 = 8192;

trait ReadSeek: Read + Seek {}
impl<T: Read + Seek> ReadSeek for T {}

enum LayerSource<'a> {
    Archive {
        reader: Box<dyn ReadSeek + 'a>,
        archive_len: u64,
        members: BTreeMap<String, ZipMember>,
    },
    Directory(PathBuf),
}

/// Canonical asset names are independent of whether a pack is a directory,
/// a ZIP rooted at assets/, or a ZIP with one enclosing pack directory.
pub(crate) struct AssetLayer<'a> {
    source: LayerSource<'a>,
    names: Vec<String>,
    entries_seen: usize,
}

impl<'a> AssetLayer<'a> {
    pub(crate) fn open(
        path: &Path,
        pack: bool,
        select: impl Fn(&str) -> bool,
    ) -> Result<Self, JarError> {
        if pack && path.is_dir() {
            return Self::directory(path, select);
        }
        // Check the opened file in archive() before reading its index.
        if !std::fs::metadata(path).map_err(io_reason)?.is_file() {
            return Err(JarError::Malformed {
                reason: "not a regular archive file".into(),
            });
        }
        let file = File::open(path).map_err(io_reason)?;
        if !file.metadata().map_err(io_reason)?.is_file() {
            return Err(JarError::Malformed {
                reason: "not a regular archive file".into(),
            });
        }
        Self::archive(Box::new(file), pack, select)
    }

    pub(crate) fn from_bytes(
        bytes: &'a [u8],
        pack: bool,
        select: impl Fn(&str) -> bool,
    ) -> Result<Self, JarError> {
        Self::archive(Box::new(Cursor::new(bytes)), pack, select)
    }

    fn archive(
        mut reader: Box<dyn ReadSeek + 'a>,
        pack: bool,
        select: impl Fn(&str) -> bool,
    ) -> Result<Self, JarError> {
        let (archive_len, indexed) = index_zip(&mut reader, true)?;
        let entries_seen = indexed.len();
        let wrapper = if pack
            && !indexed
                .iter()
                .any(|m| m.name.starts_with("assets/") || m.name.starts_with("data/"))
        {
            indexed
                .iter()
                .filter_map(|m| {
                    let (root, rest) = m.name.split_once('/')?;
                    (rest.starts_with("assets/") || rest.starts_with("data/")).then_some(root)
                })
                .min()
                .unwrap_or("")
                .to_owned()
        } else {
            String::new()
        };
        let prefix = if wrapper.is_empty() {
            wrapper
        } else {
            format!("{wrapper}/")
        };
        let mut members = BTreeMap::new();
        for member in indexed {
            if let Some(name) = member
                .name
                .strip_prefix(&prefix)
                .filter(|name| select(name))
            {
                members.entry(name.to_owned()).or_insert(member);
            }
        }
        let names = members.keys().cloned().collect();
        Ok(Self {
            source: LayerSource::Archive {
                reader,
                archive_len,
                members,
            },
            names,
            entries_seen,
        })
    }

    fn directory(path: &Path, select: impl Fn(&str) -> bool) -> Result<Self, JarError> {
        let root = std::fs::canonicalize(path).map_err(io_reason)?;
        let mut names = Vec::new();
        let mut entries_seen = 0;
        let mut dirs = vec![(root.clone(), 0)];
        while let Some((dir, depth)) = dirs.pop() {
            for entry in std::fs::read_dir(dir).map_err(io_reason)? {
                let entry = entry.map_err(io_reason)?;
                entries_seen += 1;
                if entries_seen > MAX_ARCHIVE_ENTRIES {
                    return Err(JarError::BudgetExceeded {
                        reason: "directory entry limit exceeded".into(),
                    });
                }
                let ty = entry.file_type().map_err(io_reason)?;
                let path = entry.path();
                if ty.is_dir() && depth < 32 {
                    dirs.push((path, depth + 1));
                } else if ty.is_file() {
                    let name = path
                        .strip_prefix(&root)
                        .expect("directory walk stays under root");
                    let name = name
                        .to_string_lossy()
                        .replace(std::path::MAIN_SEPARATOR, "/");
                    if validate_entry_name(&name).is_ok() && select(&name) {
                        names.push(name);
                    }
                }
            }
        }
        names.sort_unstable();
        Ok(Self {
            source: LayerSource::Directory(root),
            names,
            entries_seen,
        })
    }

    pub(crate) fn names(&self) -> &[String] {
        &self.names
    }

    pub(crate) fn read(&mut self, name: &str, cap: usize) -> Result<Vec<u8>, JarError> {
        match &mut self.source {
            LayerSource::Archive {
                reader,
                archive_len,
                members,
            } => {
                let member = members.get(name).ok_or_else(|| JarError::Malformed {
                    reason: format!("asset not indexed: {name}"),
                })?;
                read_zip_member(reader, *archive_len, member, cap as u64)
            }
            LayerSource::Directory(root) => {
                if self
                    .names
                    .binary_search_by(|key| key.as_str().cmp(name))
                    .is_err()
                {
                    return Err(JarError::Malformed {
                        reason: format!("asset not indexed: {name}"),
                    });
                }
                let path = root.join(name);
                if std::fs::canonicalize(&path).map_err(io_reason)? != path {
                    return Err(JarError::UnsafePath {
                        path: name.to_owned(),
                    });
                }
                read_bounded_file(&path, cap).map_err(io_reason)
            }
        }
    }

    pub(crate) fn collect(mut self, cap: u64) -> Result<(Vec<JarEntry>, JarReadReport), JarError> {
        let mut report = JarReadReport {
            entries_seen: self.entries_seen,
            ..JarReadReport::default()
        };
        let mut remaining = cap;
        let mut entries = Vec::new();
        for name in self.names.clone() {
            match self.read(&name, MAX_ENTRY_BYTES.min(remaining) as usize) {
                Ok(bytes) => {
                    remaining -= bytes.len() as u64;
                    entries.push(JarEntry { name, bytes });
                }
                Err(error) => report.skipped.push((name, error.to_string())),
            }
        }
        report.entries_loaded = entries.len();
        Ok((entries, report))
    }
}

pub(crate) fn read_bounded_file(path: &Path, cap: usize) -> std::io::Result<Vec<u8>> {
    let meta = std::fs::metadata(path)?;
    if !meta.is_file() || meta.len() > cap as u64 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "file exceeds input limit or is not regular",
        ));
    }
    let file = File::open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.len() > cap as u64 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "file exceeds input limit or is not regular",
        ));
    }
    let mut bytes = Vec::new();
    file.take(cap as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > cap {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "file exceeds input limit",
        ));
    }
    Ok(bytes)
}

pub(crate) fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.get(..16)? != b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR" {
        return None;
    }
    let width = u32::from_be_bytes(bytes.get(16..20)?.try_into().ok()?);
    let height = u32::from_be_bytes(bytes.get(20..24)?.try_into().ok()?);
    (width != 0 && height != 0).then_some((width, height))
}

pub(crate) fn check_rgba_budget(width: u32, height: u32, remaining: usize) -> Result<(), String> {
    let size = usize::try_from(width)
        .ok()
        .and_then(|w| w.checked_mul(height as usize))
        .and_then(|pixels| pixels.checked_mul(4));
    if size.is_some_and(|size| size <= remaining) {
        Ok(())
    } else {
        Err("decoded RGBA budget exceeded".into())
    }
}

pub(crate) fn check_png_budget(bytes: &[u8], remaining: usize) -> Result<(u32, u32), String> {
    let (width, height) = png_dimensions(bytes).ok_or("invalid PNG header")?;
    check_rgba_budget(width, height, remaining)?;
    Ok((width, height))
}

pub(crate) fn decode_asset_png(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    let bytes = grayscale_palette(bytes).ok_or("invalid packed grayscale PNG")?;
    png_decode::decode_png_rgba(&bytes).map_err(|error| error.to_string())
}

/// Packed grayscale samples use the same scanlines as indexed PNG. Supply
/// the equivalent palette to the shared decoder, including optional tRNS.
fn grayscale_palette(bytes: &[u8]) -> Option<Cow<'_, [u8]>> {
    let depth = *bytes.get(24)?;
    if !matches!(depth, 1 | 2 | 4) || bytes.get(25) != Some(&0) {
        return Some(Cow::Borrowed(bytes));
    }
    let ihdr = bytes.get(12..29)?;
    if crc32_ieee(ihdr).to_be_bytes() != bytes.get(29..33)? {
        return None;
    }
    let mut converted = bytes.get(..29)?.to_vec();
    converted[25] = 3;
    converted.extend(crc32_ieee(&converted[12..]).to_be_bytes());
    let write_chunk = |out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]| {
        out.extend((data.len() as u32).to_be_bytes());
        let start = out.len();
        out.extend(kind);
        out.extend(data);
        out.extend(crc32_ieee(&out[start..]).to_be_bytes());
    };
    let levels = 1u16 << depth;
    let palette: Vec<_> = (0..levels)
        .flat_map(|i| [(i * 255 / (levels - 1)) as u8; 3])
        .collect();
    write_chunk(&mut converted, b"PLTE", &palette);
    let mut rest = bytes.get(33..)?;
    while !rest.is_empty() {
        let length = u32::from_be_bytes(rest.get(..4)?.try_into().ok()?) as usize;
        let chunk = rest.get(..length.checked_add(12)?)?;
        match chunk.get(4..8)? {
            b"tRNS" => {
                if length != 2 || crc32_ieee(&chunk[4..10]).to_be_bytes() != chunk[10..] {
                    return None;
                }
                let transparent = u16::from_be_bytes(chunk[8..10].try_into().ok()?);
                if transparent >= levels {
                    return None;
                }
                let alpha: Vec<_> = (0..levels)
                    .map(|i| if i == transparent { 0 } else { 255 })
                    .collect();
                write_chunk(&mut converted, b"tRNS", &alpha);
            }
            _ => converted.extend(chunk),
        }
        rest = &rest[chunk.len()..];
    }
    Some(Cow::Owned(converted))
}

pub(crate) fn validate_client_zip(file: &mut File) -> bool {
    let mut magic = [0; 4];
    file.read_exact(&mut magic).is_ok()
        && magic == b"PK\x03\x04"[..]
        && index_zip(file, true).is_ok_and(|(_, entries)| !entries.is_empty())
}

/// `assets/<ns>/<category>/<sub>` -> (`ns`, `category`, `sub`).
fn asset_subpath(name: &str) -> Option<(String, String, String)> {
    let rest = name.strip_prefix("assets/")?;
    let (ns, tail) = rest.split_once('/')?;
    let (category, sub) = tail.split_once('/')?;
    if ns.is_empty() || category.is_empty() || sub.is_empty() {
        return None;
    }
    Some((ns.to_owned(), category.to_owned(), sub.to_owned()))
}

fn valid_mod_namespace(ns: &str) -> bool {
    !ns.is_empty()
        && ns != "."
        && ns != ".."
        && ns.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

fn is_mod_asset(name: &str) -> bool {
    let Some((ns, category, sub)) = asset_subpath(name) else {
        return false;
    };
    if !valid_mod_namespace(&ns) {
        return false;
    }
    match category.as_str() {
        "blockstates" => sub.ends_with(".json"),
        "models" => {
            (sub.starts_with("block/") || sub.starts_with("item/")) && sub.ends_with(".json")
        }
        "textures" => {
            (sub.starts_with("block/") || sub.starts_with("item/"))
                && (sub.ends_with(".png") || sub.ends_with(".png.mcmeta"))
        }
        "lang" => sub.ends_with(".json"),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Recursive Jar-in-Jar extraction
//
// `LazyZipIndex` only opens archives from a `Path` and flattens one level of
// `META-INF/jarjar`/`META-INF/jars` nesting without keeping per-JAR
// provenance, so nested JARs could not be ordered or isolated. The reader
// below is the same bounded contract over any `Read + Seek` source — the
// outer `mods/*.jar` `File` at depth 0 and an in-memory `Cursor` for every
// nested JAR — reusing `megu_core::zip_archive`'s RFC 1951 inflater instead
// of duplicating DEFLATE. EOCD + Central Directory are streamed record by
// record (never the whole archive), every member name passes
// [`validate_entry_name`] fail-closed, ZIP64/multi-disk/encrypted archives
// are rejected, and nested JARs recurse only to [`MAX_JIJ_DEPTH`].
// ---------------------------------------------------------------------------

/// EOCD tail window: 22-byte record plus the maximum 65 535-byte comment.
const JIJ_EOCD_WINDOW: u64 = 65_557;
const JIJ_EOCD_MIN: usize = 22;
const JIJ_CDE_FIXED: usize = 46;
const JIJ_LFH_FIXED: usize = 30;
const JIJ_SIG_LFH: u32 = 0x0403_4b50;
const JIJ_SIG_CDE: u32 = 0x0201_4b50;
const JIJ_SIG_EOCD: u32 = 0x0605_4b50;
const JIJ_METHOD_STORED: u16 = 0;
const JIJ_METHOD_DEFLATE: u16 = 8;

fn u16_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(
        bytes[offset..offset + 8]
            .try_into()
            .expect("fixed ZIP field"),
    )
}

fn zip64_extra(mut extra: &[u8]) -> Option<&[u8]> {
    while extra.len() >= 4 {
        let end = usize::from(u16_at(extra, 2)) + 4;
        let field = extra.get(4..end)?;
        if u16_at(extra, 0) == 1 {
            return Some(field);
        }
        extra = &extra[end..];
    }
    None
}

fn io_reason(err: std::io::Error) -> JarError {
    JarError::Io {
        reason: err.to_string(),
    }
}

fn read_at<R: Read + Seek>(reader: &mut R, offset: u64, buf: &mut [u8]) -> Result<(), JarError> {
    reader
        .seek(SeekFrom::Start(offset))
        .and_then(|_| reader.read_exact(buf))
        .map_err(io_reason)
}

/// One Central Directory member (directory records already dropped).
struct ZipMember {
    name: String,
    method: u16,
    compressed_size: u64,
    uncompressed_size: u64,
    local_offset: u64,
}

/// `META-INF/jarjar/*.jar` (NeoForge) or `META-INF/jars/*.jar` (Fabric),
/// case-insensitive like the loader's own convention. The prefix check is
/// length-exact so `jarjar/` can never alias `jars/`.
fn is_jar_in_jar_entry(name: &str) -> bool {
    const JAR_JAR: &str = "META-INF/jarjar/";
    const JARS: &str = "META-INF/jars/";
    if name.len() <= JAR_JAR.len()
        || !name
            .get(name.len() - 4..)
            .is_some_and(|s| s.eq_ignore_ascii_case(".jar"))
    {
        return false;
    }
    name.get(..JAR_JAR.len())
        .is_some_and(|p| p.eq_ignore_ascii_case(JAR_JAR))
        || name
            .get(..JARS.len())
            .is_some_and(|p| p.eq_ignore_ascii_case(JARS))
}

/// Locate the EOCD record in the assembled tail window: the record's
/// comment length must land exactly on the end of the archive.
fn find_eocd(tail: &[u8]) -> Option<usize> {
    if tail.len() < JIJ_EOCD_MIN {
        return None;
    }
    let mut pos = tail.len() - JIJ_EOCD_MIN;
    loop {
        if u32_at(tail, pos) == JIJ_SIG_EOCD
            && pos + JIJ_EOCD_MIN + u16_at(tail, pos + 20) as usize == tail.len()
        {
            return Some(pos);
        }
        if pos == 0 {
            return None;
        }
        pos -= 1;
    }
}

/// Index every file member of the ZIP stream behind `reader`: scan the EOCD
/// tail window, then stream the Central Directory record by record. Returns
/// the archive length and the validated member list. Fails closed on
/// traversal/unsafe names (whole archive, matching `LazyZipIndex`), ZIP64,
/// multi-disk, encryption, non-UTF-8 names and structural corruption.
fn index_zip<R: Read + Seek>(
    reader: &mut R,
    allow_zip64: bool,
) -> Result<(u64, Vec<ZipMember>), JarError> {
    let len = reader.seek(SeekFrom::End(0)).map_err(io_reason)?;
    if len > MAX_ARCHIVE_BYTES {
        return Err(JarError::BudgetExceeded {
            reason: format!("archive size {len} exceeds {MAX_ARCHIVE_BYTES}"),
        });
    }
    if len < JIJ_EOCD_MIN as u64 {
        return Err(JarError::Malformed {
            reason: "not a ZIP archive".to_owned(),
        });
    }
    let window = len.min(JIJ_EOCD_WINDOW);
    let mut tail = vec![0u8; window as usize];
    read_at(reader, len - window, &mut tail)?;
    let Some(eocd) = find_eocd(&tail) else {
        return Err(JarError::Malformed {
            reason: "no end-of-central-directory record".to_owned(),
        });
    };
    let rec = &tail[eocd..];
    if u16_at(rec, 4) != 0 || u16_at(rec, 6) != 0 || u16_at(rec, 8) != u16_at(rec, 10) {
        return Err(JarError::Unsupported {
            reason: "multi-disk archive".to_owned(),
        });
    }
    let central_record = len - window + eocd as u64;
    let mut total = u64::from(u16_at(rec, 10));
    let mut cd_size = u64::from(u32_at(rec, 12));
    let mut cd_offset = u64::from(u32_at(rec, 16));
    let mut cd_limit = central_record;
    if total == u64::from(u16::MAX)
        || cd_size == u64::from(u32::MAX)
        || cd_offset == u64::from(u32::MAX)
    {
        if !allow_zip64 {
            return Err(JarError::Unsupported {
                reason: "ZIP64 archive".into(),
            });
        }
        let locator_offset = central_record
            .checked_sub(20)
            .ok_or_else(|| JarError::Malformed {
                reason: "missing ZIP64 locator".into(),
            })?;
        let mut locator = [0; 20];
        read_at(reader, locator_offset, &mut locator)?;
        cd_limit = u64_at(&locator, 8);
        if u32_at(&locator, 0) != 0x0706_4b50
            || u32_at(&locator, 4) != 0
            || u32_at(&locator, 16) != 1
            || cd_limit > locator_offset.saturating_sub(56)
        {
            return Err(JarError::Malformed {
                reason: "invalid ZIP64 locator".into(),
            });
        }
        let mut zip64 = [0; 56];
        read_at(reader, cd_limit, &mut zip64)?;
        if u32_at(&zip64, 0) != 0x0606_4b50
            || u32_at(&zip64, 16) != 0
            || u32_at(&zip64, 20) != 0
            || u64_at(&zip64, 24) != u64_at(&zip64, 32)
            || u64_at(&zip64, 4) < 44
            || cd_limit
                .checked_add(12)
                .and_then(|end| end.checked_add(u64_at(&zip64, 4)))
                != Some(locator_offset)
        {
            return Err(JarError::Malformed {
                reason: "invalid ZIP64 central directory record".into(),
            });
        }
        total = u64_at(&zip64, 32);
        cd_size = u64_at(&zip64, 40);
        cd_offset = u64_at(&zip64, 48);
    }
    const MAX_CENTRAL_DIRECTORY_BYTES: u64 = 16 * 1024 * 1024;
    if cd_size > MAX_CENTRAL_DIRECTORY_BYTES {
        return Err(JarError::BudgetExceeded {
            reason: "central directory byte limit exceeded".into(),
        });
    }
    if total > MAX_ARCHIVE_ENTRIES as u64 {
        return Err(JarError::BudgetExceeded {
            reason: format!("archive has {total} entries (cap {MAX_ARCHIVE_ENTRIES})"),
        });
    }
    let cd_end = cd_offset
        .checked_add(cd_size)
        .filter(|&end| end <= cd_limit)
        .ok_or_else(|| JarError::Malformed {
            reason: "central directory outside archive".to_owned(),
        })?;

    let mut members = Vec::with_capacity(total.min(256) as usize);
    let mut cursor = cd_offset;
    for _ in 0..total {
        if cursor + JIJ_CDE_FIXED as u64 > cd_end {
            return Err(JarError::Malformed {
                reason: "central directory truncated".to_owned(),
            });
        }
        let mut fixed = [0u8; JIJ_CDE_FIXED];
        read_at(reader, cursor, &mut fixed)?;
        cursor += JIJ_CDE_FIXED as u64;
        if u32_at(&fixed, 0) != JIJ_SIG_CDE {
            return Err(JarError::Malformed {
                reason: "bad central directory signature".to_owned(),
            });
        }
        let flags = u16_at(&fixed, 8);
        if flags & 1 != 0 {
            return Err(JarError::Unsupported {
                reason: "encrypted entry".to_owned(),
            });
        }
        let method = u16_at(&fixed, 10);
        let mut compressed_size = u64::from(u32_at(&fixed, 20));
        let mut uncompressed_size = u64::from(u32_at(&fixed, 24));
        let name_len = u64::from(u16_at(&fixed, 28));
        let extra_len = u64::from(u16_at(&fixed, 30));
        let comment_len = u64::from(u16_at(&fixed, 32));
        let mut disk_start = u32::from(u16_at(&fixed, 34));
        let mut local_offset = u64::from(u32_at(&fixed, 42));
        let record_end = cursor
            .checked_add(name_len + extra_len + comment_len)
            .filter(|&end| end <= cd_end)
            .ok_or_else(|| JarError::Malformed {
                reason: "entry record overruns central directory".to_owned(),
            })?;
        let mut name_bytes = vec![0u8; name_len as usize];
        read_at(reader, cursor, &mut name_bytes)?;
        if [uncompressed_size, compressed_size, local_offset].contains(&u64::from(u32::MAX))
            || disk_start == u32::from(u16::MAX)
        {
            if !allow_zip64 {
                return Err(JarError::Unsupported {
                    reason: "ZIP64 entry".into(),
                });
            }
            let mut extra = vec![0; extra_len as usize];
            read_at(reader, cursor + name_len, &mut extra)?;
            let extra = zip64_extra(&extra).ok_or_else(|| JarError::Malformed {
                reason: "missing ZIP64 entry fields".into(),
            })?;
            let mut fields = Cursor::new(extra);
            for field in [
                &mut uncompressed_size,
                &mut compressed_size,
                &mut local_offset,
            ] {
                if *field == u64::from(u32::MAX) {
                    let mut bytes = [0; 8];
                    fields.read_exact(&mut bytes).map_err(io_reason)?;
                    *field = u64::from_le_bytes(bytes);
                }
            }
            if disk_start == u32::from(u16::MAX) {
                let mut bytes = [0; 4];
                fields.read_exact(&mut bytes).map_err(io_reason)?;
                disk_start = u32::from_le_bytes(bytes);
            }
        }
        if disk_start != 0 {
            return Err(JarError::Unsupported {
                reason: "multi-disk archive".into(),
            });
        }
        if local_offset
            .checked_add(JIJ_LFH_FIXED as u64)
            .and_then(|off| off.checked_add(compressed_size))
            .is_none_or(|end| end > cd_offset)
        {
            return Err(JarError::Malformed {
                reason: "member outside archive data".into(),
            });
        }
        cursor = record_end;
        let name = String::from_utf8(name_bytes).map_err(|_| JarError::Malformed {
            reason: "entry name is not UTF-8".to_owned(),
        })?;
        if name.ends_with('/') {
            continue;
        }
        validate_entry_name(&name)?;
        members.push(ZipMember {
            name,
            method,
            compressed_size,
            uncompressed_size,
            local_offset,
        });
    }
    if cursor != cd_end {
        return Err(JarError::Malformed {
            reason: "central directory size or entry count mismatch".into(),
        });
    }
    Ok((cd_offset, members))
}

/// Decompress one indexed member, bounded by `max_bytes` on both the
/// declared uncompressed size and the actual inflated output. The compressed
/// read is capped at a generous multiple of the output cap so a tiny
/// declared output cannot smuggle a huge payload through.
fn read_zip_member<R: Read + Seek>(
    reader: &mut R,
    archive_len: u64,
    member: &ZipMember,
    max_bytes: u64,
) -> Result<Vec<u8>, JarError> {
    match member.method {
        JIJ_METHOD_STORED | JIJ_METHOD_DEFLATE => {}
        other => {
            return Err(JarError::Unsupported {
                reason: format!("compression method {other}"),
            })
        }
    }
    if member.uncompressed_size > max_bytes {
        return Err(JarError::BudgetExceeded {
            reason: format!(
                "entry is {} bytes (cap {max_bytes})",
                member.uncompressed_size
            ),
        });
    }
    if member.compressed_size
        > max_bytes
            .saturating_mul(2)
            .saturating_add(1024 * 1024)
    {
        return Err(JarError::BudgetExceeded {
            reason: format!(
                "entry compressed size {} exceeds sane bound",
                member.compressed_size
            ),
        });
    }
    let mut header = [0u8; JIJ_LFH_FIXED];
    read_at(reader, member.local_offset, &mut header)?;
    if u32_at(&header, 0) != JIJ_SIG_LFH {
        return Err(JarError::Malformed {
            reason: "bad local file header signature".to_owned(),
        });
    }
    let data_offset = member
        .local_offset
        .checked_add(JIJ_LFH_FIXED as u64)
        .and_then(|off| off.checked_add(u16_at(&header, 26) as u64 + u16_at(&header, 28) as u64))
        .ok_or_else(|| JarError::Malformed {
            reason: "local data offset overflow".to_owned(),
        })?;
    data_offset
        .checked_add(member.compressed_size)
        .filter(|&end| end <= archive_len)
        .ok_or_else(|| JarError::Malformed {
            reason: "entry data outside archive".to_owned(),
        })?;
    let mut compressed = vec![0u8; member.compressed_size as usize];
    read_at(reader, data_offset, &mut compressed)?;
    match member.method {
        JIJ_METHOD_STORED => {
            if compressed.len() as u64 != member.uncompressed_size {
                return Err(JarError::Malformed {
                    reason: "stored entry size mismatch".to_owned(),
                });
            }
            Ok(compressed)
        }
        _ => {
            let (out, consumed) =
                inflate_raw_rfc1951_with_consumed(&compressed, member.uncompressed_size as usize)
                    .map_err(|err| match err {
                    ZipError::OutputCapExceeded | ZipError::EntryTooLarge { .. } => {
                        JarError::BudgetExceeded {
                            reason: "inflated entry exceeds output cap".to_owned(),
                        }
                    }
                    other => JarError::Malformed {
                        reason: format!("deflate: {other}"),
                    },
                })?;
            if out.len() as u64 != member.uncompressed_size || consumed != compressed.len() {
                return Err(JarError::Malformed {
                    reason: "inflated size mismatch".to_owned(),
                });
            }
            Ok(out)
        }
    }
}

/// Per-outer-JAR collection state: budget left, deterministic layer order
/// (inner JARs before the JAR that nests them), nested-JAR count and every
/// non-fatal skip note.
struct JarCollect {
    layers: Vec<ResourcePackLayer>,
    entries_seen: usize,
    jars_opened: usize,
    remaining: u64,
    skipped: Vec<String>,
}

/// Charge `amount` bytes against the aggregate mod-asset budget. Budget
/// violations are always fatal for the whole outer JAR — matching the
/// pre-Jar-in-Jar aggregate accounting — while structural problems inside a
/// nested JAR only skip that nested JAR.
fn charge_budget(ctx: &mut JarCollect, amount: u64) -> Result<(), JarError> {
    if amount > ctx.remaining {
        return Err(JarError::BudgetExceeded {
            reason: "aggregate mod assets exceed budget".to_owned(),
        });
    }
    ctx.remaining -= amount;
    Ok(())
}

/// Index `reader`'s archive, then emit [`ResourcePackLayer`]s for its
/// `is_mod_asset` members and recurse into its sorted Jar-in-Jar members
/// first so inner layers sit earlier (lower priority) than this level's.
/// `strict` preserves the historical outer-JAR contract: any skipped asset
/// entry fails the whole mod JAR. Nested levels record skips instead.
fn collect_zip_assets<R: Read + Seek>(
    reader: &mut R,
    depth: usize,
    label: &str,
    strict: bool,
    ctx: &mut JarCollect,
) -> Result<(), JarError> {
    let (archive_len, members) = index_zip(reader, false)?;
    ctx.entries_seen += members.len();

    let mut nested: Vec<&ZipMember> = members
        .iter()
        .filter(|member| is_jar_in_jar_entry(&member.name))
        .collect();
    let assets: Vec<&ZipMember> = members
        .iter()
        .filter(|member| is_mod_asset(&member.name))
        .collect();
    nested.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then(a.local_offset.cmp(&b.local_offset))
    });

    for member in nested {
        if depth >= MAX_JIJ_DEPTH {
            ctx.skipped.push(format!(
                "{label}: {}: jar-in-jar depth exceeds {MAX_JIJ_DEPTH}",
                member.name
            ));
            continue;
        }
        if ctx.jars_opened >= MAX_JIJ_JARS_PER_MOD {
            ctx.skipped.push(format!(
                "{label}: {}: more than {MAX_JIJ_JARS_PER_MOD} nested JARs",
                member.name
            ));
            continue;
        }
        if member.uncompressed_size > MAX_JIJ_JAR_BYTES {
            ctx.skipped.push(format!(
                "{label}: {}: nested JAR exceeds {MAX_JIJ_JAR_BYTES}-byte cap",
                member.name
            ));
            continue;
        }
        // The nested JAR's own bytes are materialized in memory, so they
        // count against the same aggregate budget as the assets inside.
        charge_budget(ctx, member.uncompressed_size)?;
        ctx.jars_opened += 1;
        match read_zip_member(reader, archive_len, member, MAX_JIJ_JAR_BYTES) {
            Ok(bytes) => {
                let mut cursor = Cursor::new(bytes);
                match collect_zip_assets(&mut cursor, depth + 1, &member.name, false, ctx) {
                    Ok(()) => {}
                    Err(err @ JarError::BudgetExceeded { .. }) => return Err(err),
                    Err(err) => ctx.skipped.push(format!("{}: {err}", member.name)),
                }
            }
            Err(err @ JarError::BudgetExceeded { .. }) => return Err(err),
            Err(err) => ctx.skipped.push(format!("{}: {err}", member.name)),
        }
    }

    let mut entries = Vec::new();
    let mut read_report = JarReadReport::default();
    for member in assets {
        if member.uncompressed_size > MAX_ENTRY_BYTES {
            let reason = format!(
                "entry exceeds {MAX_ENTRY_BYTES}-byte cap ({})",
                member.uncompressed_size
            );
            if strict {
                read_report.skipped.push((member.name.clone(), reason));
            } else {
                ctx.skipped.push(format!("{label}: {}: {reason}", member.name));
            }
            continue;
        }
        // Charge the declared size before materializing so a JAR full of
        // asset entries cannot allocate past the aggregate budget. The
        // reader verifies the inflated length equals this declaration.
        charge_budget(ctx, member.uncompressed_size)?;
        match read_zip_member(reader, archive_len, member, MAX_ENTRY_BYTES) {
            Ok(bytes) => entries.push(JarEntry {
                name: member.name.clone(),
                bytes,
            }),
            Err(err) => {
                let reason = err.to_string();
                if strict {
                    read_report.skipped.push((member.name.clone(), reason));
                } else {
                    ctx.skipped
                        .push(format!("{label}: {}: {reason}", member.name));
                }
            }
        }
    }
    let layer = layer_from_entries(
        label.to_owned(),
        &entries,
        &mut read_report,
        ctx.remaining as usize,
    );
    if strict {
        if let Some((name, reason)) = read_report.skipped.first() {
            return Err(JarError::Malformed {
                reason: format!("{name}: {reason}"),
            });
        }
    } else {
        for (name, reason) in read_report.skipped {
            ctx.skipped.push(format!("{label}: {name}: {reason}"));
        }
    }
    // Unpacked asset bytes were charged per entry above; only the decoded
    // sprite payloads still need accounting.
    let decoded = layer
        .sprites
        .values()
        .map(|(_, _, rgba)| rgba.len() as u64)
        .sum::<u64>();
    charge_budget(ctx, decoded)?;
    let has_content = !layer.blockstates.is_empty()
        || !layer.models.is_empty()
        || !layer.sprites.is_empty()
        || !layer.animations.is_empty();
    if has_content {
        ctx.layers.push(layer);
    }
    Ok(())
}

/// Read one `mods/*.jar` including every reachable Jar-in-Jar level. Returns
/// the layers in priority order (deepest/inner JARs first, outer JAR last so
/// its assets win key collisions), the inspected entry count, the nested-JAR
/// skip notes, and the aggregate bytes charged against `MAX_MOD_ASSET_BYTES`.
fn load_single_mod_jar(
    jar: &Path,
    budget: u64,
) -> Result<(Vec<ResourcePackLayer>, usize, Vec<String>, u64), JarError> {
    let size = std::fs::metadata(jar)
        .map_err(|err| JarError::Io {
            reason: format!("cannot stat {}: {err}", jar.display()),
        })?
        .len();
    if size > MAX_ARCHIVE_BYTES {
        return Err(JarError::BudgetExceeded {
            reason: format!("archive size {size} exceeds {MAX_ARCHIVE_BYTES}"),
        });
    }
    let mut file = File::open(jar).map_err(|err| JarError::Io {
        reason: format!("cannot read {}: {err}", jar.display()),
    })?;
    let label = jar
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let mut ctx = JarCollect {
        layers: Vec::new(),
        entries_seen: 0,
        jars_opened: 0,
        remaining: budget,
        skipped: Vec::new(),
    };
    collect_zip_assets(&mut file, 0, &label, true, &mut ctx)?;
    Ok((
        ctx.layers,
        ctx.entries_seen,
        ctx.skipped,
        budget - ctx.remaining,
    ))
}

/// Load regular `mods/*.jar` entries by filename, isolating corrupt JARs.
/// Later JARs override earlier JARs, and user packs override all mod layers.
/// Bundled mods nest their real content under `META-INF/jarjar/*.jar`
/// (NeoForge) or `META-INF/jars/*.jar` (Fabric); those nested JARs are
/// scanned recursively to [`MAX_JIJ_DEPTH`] and emit their own layers in
/// sorted entry-name order, so an asset-less wrapper JAR like
/// `create-aeronautics-bundled-*.jar` still contributes every nested
/// namespace's blockstates, models, textures and animations.
pub fn load_mod_jar_layers(mods_dir: &Path) -> (Vec<ResourcePackLayer>, ModJarLoadReport) {
    let mut layers = Vec::new();
    let mut report = ModJarLoadReport::default();
    let dir = match std::fs::read_dir(mods_dir) {
        Ok(dir) => dir,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return (layers, report),
        Err(err) => {
            report.rejected.push((
                mods_dir.to_path_buf(),
                JarError::Io {
                    reason: err.to_string(),
                },
            ));
            return (layers, report);
        }
    };
    let mut jars = Vec::new();
    for entry in dir {
        match entry {
            Ok(entry) if entry.path().extension().is_some_and(|e| e == "jar") => {
                match entry.file_type() {
                    Ok(ty) if ty.is_file() => jars.push(entry.path()),
                    Ok(_) => report.rejected.push((
                        entry.path(),
                        JarError::Unsupported {
                            reason: "mod JAR must be a regular file".to_owned(),
                        },
                    )),
                    Err(err) => report.rejected.push((
                        entry.path(),
                        JarError::Io {
                            reason: err.to_string(),
                        },
                    )),
                }
            }
            Ok(_) => {}
            Err(err) => report.rejected.push((
                mods_dir.to_path_buf(),
                JarError::Io {
                    reason: err.to_string(),
                },
            )),
        }
    }
    jars.sort();
    let mut total_assets = 0u64;
    for (index, jar) in jars.into_iter().enumerate() {
        if index >= MAX_MOD_JARS {
            report.rejected.push((
                jar,
                JarError::BudgetExceeded {
                    reason: format!("more than {MAX_MOD_JARS} mod JARs"),
                },
            ));
            continue;
        }
        match load_single_mod_jar(&jar, MAX_MOD_ASSET_BYTES.saturating_sub(total_assets)) {
            Ok((mut jar_layers, seen, skipped, spent)) => {
                total_assets += spent;
                report.entries_seen += seen;
                report.skipped.extend(skipped);
                report.loaded.push(jar);
                layers.append(&mut jar_layers);
            }
            Err(err) => report.rejected.push((jar, err)),
        }
    }
    (layers, report)
}

/// Decode the `.rgba` sprite format (`u32le width`, `u32le height`,
/// `width*height*4` RGBA bytes) — the same on-disk contract
/// `client-port`'s `decode_rgba_sprite` enforces.
fn decode_rgba_sprite(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    if bytes.len() < 8 {
        return Err("sprite header truncated (need 8 bytes)".to_owned());
    }
    let width = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let height = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    if width == 0 || height == 0 || width > 4096 || height > 4096 {
        return Err(format!("implausible sprite dimensions {width}x{height}"));
    }
    let need = u64::from(width) * u64::from(height) * 4;
    if bytes[8..].len() as u64 != need {
        return Err(format!(
            "sprite payload is {} bytes, expected {need}",
            bytes[8..].len()
        ));
    }
    Ok((width, height, bytes[8..].to_vec()))
}

/// Fold decompressed `assets/`-shaped entries into a [`ResourcePackLayer`]
/// under the same key rules `ResourcePackLayer::from_dir` uses:
/// `blockstates/*.json` and `models/**/*.json` stay raw text,
/// `textures/**/*.png|*.rgba` decode into `sprites` within the byte budget.
/// Invalid textures are skipped so other layers can still supply them.
fn layer_from_entries(
    name: String,
    entries: &[JarEntry],
    report: &mut JarReadReport,
    mut remaining: usize,
) -> ResourcePackLayer {
    let mut layer = ResourcePackLayer {
        name,
        ..ResourcePackLayer::default()
    };
    // `.png.mcmeta` texts resolved once every sprite is decoded — an
    // animation's frame size is validated against the sprite's geometry.
    let mut pending_meta: Vec<(String, String, String)> = Vec::new();
    for entry in entries {
        let Some((ns, category, sub)) = asset_subpath(&entry.name) else {
            continue;
        };
        match category.as_str() {
            "blockstates" | "models" => {
                let Some(stem) = sub.strip_suffix(".json") else {
                    continue;
                };
                match String::from_utf8(entry.bytes.clone()) {
                    Ok(text) => {
                        let key = format!("{ns}:{stem}");
                        if category == "blockstates" {
                            layer.blockstates.insert(key, text);
                        } else {
                            layer.models.insert(key, text);
                        }
                    }
                    Err(_) => report
                        .skipped
                        .push((entry.name.clone(), "not valid UTF-8".to_owned())),
                }
            }
            "textures" => {
                if let Some(stem) = sub.strip_suffix(".png") {
                    let key = format!("{ns}:{stem}");
                    if let Err(reason) = check_png_budget(&entry.bytes, remaining) {
                        report.skipped.push((entry.name.clone(), reason));
                        continue;
                    }
                    match decode_asset_png(&entry.bytes) {
                        Ok((width, height, rgba)) => {
                            remaining -= rgba.len();
                            layer.sprites.insert(key, (width, height, rgba));
                        }
                        Err(error) => report.skipped.push((entry.name.clone(), error.to_string())),
                    }
                } else if let Some(stem) = sub.strip_suffix(".rgba") {
                    if entry.bytes.len().saturating_sub(8) > remaining {
                        report
                            .skipped
                            .push((entry.name.clone(), "decoded RGBA budget exceeded".into()));
                        continue;
                    }
                    match decode_rgba_sprite(&entry.bytes) {
                        Ok((width, height, rgba)) => {
                            remaining -= rgba.len();
                            layer
                                .sprites
                                .insert(format!("{ns}:{stem}"), (width, height, rgba));
                        }
                        Err(reason) => report.skipped.push((entry.name.clone(), reason)),
                    }
                } else if let Some(stem) = sub
                    .strip_suffix(".mcmeta")
                    .and_then(|s| s.strip_suffix(".png").or_else(|| s.strip_suffix(".rgba")))
                {
                    match String::from_utf8(entry.bytes.clone()) {
                        Ok(text) => {
                            pending_meta.push((format!("{ns}:{stem}"), entry.name.clone(), text))
                        }
                        Err(_) => report
                            .skipped
                            .push((entry.name.clone(), "mcmeta not valid UTF-8".to_owned())),
                    }
                }
            }
            _ => {}
        }
    }
    for (sprite_key, display, text) in pending_meta {
        let Some(&(width, height, _)) = layer.sprites.get(&sprite_key) else {
            report.skipped.push((
                display,
                format!("animation has no sibling sprite {sprite_key:?}"),
            ));
            continue;
        };
        match animation_from_mcmeta(&text, width, height) {
            Ok(Some(animation)) => {
                layer.animations.insert(sprite_key, animation);
            }
            Ok(None) => {}
            Err(reason) => report.skipped.push((display, reason)),
        }
    }
    layer
}

/// Read the vanilla client JAR at `jar_path` and build its blockstates,
/// block/item models and sprites. Mod models frequently inherit a vanilla
/// parent such as `minecraft:block/cube_all`, which must be available in
/// the same model stack for their texture references to resolve.
pub fn vanilla_jar_texture_layer(
    jar_path: &Path,
) -> Result<(ResourcePackLayer, JarReadReport), JarError> {
    let (entries, mut report) = read_selected_entries(jar_path, is_mod_asset)?;
    let name = jar_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("client.jar")
        .to_owned();
    Ok((
        layer_from_entries(name, &entries, &mut report, MAX_DECODED_RGBA_BYTES),
        report,
    ))
}

/// Read a resource-pack `.zip` the way `<gameDir>/resourcepacks/*.zip`
/// stores one: `assets/<ns>/{blockstates,models,textures}` into a
/// [`ResourcePackLayer`]. A `foo/` top-level wrapper directory is unwrapped
/// the same way the shaderpack loader tolerates packed roots.
pub fn zip_pack_layer(zip_path: &Path) -> Result<(ResourcePackLayer, JarReadReport), JarError> {
    let (entries, mut report) =
        AssetLayer::open(zip_path, true, is_mod_asset)?.collect(MAX_UNPACKED_BYTES)?;
    let name = zip_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("resource-pack.zip")
        .to_owned();
    Ok((
        layer_from_entries(name, &entries, &mut report, MAX_DECODED_RGBA_BYTES),
        report,
    ))
}

/// Merge several reports into one: entries/counts sum, skips concatenate.
pub fn merge_reports(into: &mut JarReadReport, other: JarReadReport) {
    into.entries_seen += other.entries_seen;
    into.entries_loaded += other.entries_loaded;
    into.skipped.extend(other.skipped);
}

/// Read `<gameDir>/options.txt` and return the configured resource-pack
/// file names (`resourcePacks:["file/a.zip","file/b"]`), in the order the
/// file lists them — lowest priority first, exactly like vanilla's
/// bottom-to-top pack stack. Missing/invalid fields yield an empty list;
/// a missing file is not an error.
pub fn configured_resource_packs(game_dir: &Path) -> Vec<String> {
    let Some(text) = read_bounded_file(&game_dir.join("options.txt"), 1024 * 1024)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
    else {
        return Vec::new();
    };
    text.lines()
        .find_map(|line| line.trim().strip_prefix("resourcePacks:"))
        .and_then(|value| megu_core::json::parse_json(value).ok())
        .and_then(|value| match value {
            megu_core::json::JsonValue::Array(values) => Some(values),
            _ => None,
        })
        .into_iter()
        .flatten()
        .filter_map(|value| {
            let file = value.as_str()?.strip_prefix("file/")?;
            (!file.contains('/') && validate_entry_name(file).is_ok()).then(|| file.to_owned())
        })
        .collect()
}

/// Representative source texture for a protocol-767 entity. Dynamic held
/// items, display entities and lightning have no fixed atlas source.
pub fn entity_texture_path_for_id(entity_type_id: u16) -> Option<&'static str> {
    use megu_client_mesh::entity_rig::classify_entity_rig;
    let name = classify_entity_rig(entity_type_id)?
        .name
        .strip_prefix("minecraft:")?;
    Some(match name {
        "allay" => "assets/minecraft/textures/entity/allay/allay.png",
        "area_effect_cloud"
        | "interaction"
        | "marker"
        | "block_display"
        | "item_display"
        | "text_display"
        | "item"
        | "ominous_item_spawner"
        | "lightning_bolt" => return None,
        "armadillo" => "assets/minecraft/textures/entity/armadillo.png",
        "armor_stand" => "assets/minecraft/textures/entity/armorstand/wood.png",
        "arrow" => "assets/minecraft/textures/entity/projectiles/arrow.png",
        "spectral_arrow" => "assets/minecraft/textures/entity/projectiles/spectral_arrow.png",
        "axolotl" => "assets/minecraft/textures/entity/axolotl/axolotl_lucy.png",
        "bat" => "assets/minecraft/textures/entity/bat.png",
        "bee" => "assets/minecraft/textures/entity/bee/bee.png",
        "blaze" => "assets/minecraft/textures/entity/blaze.png",
        "boat" | "chest_boat" => "assets/minecraft/textures/entity/boat/oak.png",
        "bogged" => "assets/minecraft/textures/entity/skeleton/bogged.png",
        "breeze" => "assets/minecraft/textures/entity/breeze/breeze.png",
        "breeze_wind_charge" | "wind_charge" => {
            "assets/minecraft/textures/entity/projectiles/wind_charge.png"
        }
        "camel" => "assets/minecraft/textures/entity/camel/camel.png",
        "cat" => "assets/minecraft/textures/entity/cat/tabby.png",
        "ocelot" => "assets/minecraft/textures/entity/cat/ocelot.png",
        "cave_spider" => "assets/minecraft/textures/entity/spider/cave_spider.png",
        "chest_minecart"
        | "command_block_minecart"
        | "furnace_minecart"
        | "hopper_minecart"
        | "minecart"
        | "spawner_minecart"
        | "tnt_minecart" => "assets/minecraft/textures/entity/minecart.png",
        "chicken" => "assets/minecraft/textures/entity/chicken.png",
        "cod" => "assets/minecraft/textures/entity/fish/cod.png",
        "cow" => "assets/minecraft/textures/entity/cow/cow.png",
        "creeper" => "assets/minecraft/textures/entity/creeper/creeper.png",
        "falling_block" => return None,
        "tnt" => "assets/minecraft/textures/block/tnt_side.png",
        "dolphin" => "assets/minecraft/textures/entity/dolphin.png",
        "donkey" | "mule" => "assets/minecraft/textures/entity/horse/donkey.png",
        "dragon_fireball" => "assets/minecraft/textures/entity/enderdragon/dragon_fireball.png",
        "fireball" | "small_fireball" => "assets/minecraft/textures/item/fire_charge.png",
        "zombie" | "giant" => "assets/minecraft/textures/entity/zombie/zombie.png",
        "drowned" => "assets/minecraft/textures/entity/zombie/drowned.png",
        "husk" => "assets/minecraft/textures/entity/zombie/husk.png",
        "zombie_villager" => "assets/minecraft/textures/entity/zombie_villager/zombie_villager.png",
        "egg" => "assets/minecraft/textures/item/egg.png",
        "elder_guardian" => "assets/minecraft/textures/entity/guardian_elder.png",
        "end_crystal" => "assets/minecraft/textures/entity/end_crystal/end_crystal.png",
        "ender_dragon" => "assets/minecraft/textures/entity/enderdragon/dragon.png",
        "ender_pearl" => "assets/minecraft/textures/item/ender_pearl.png",
        "enderman" => "assets/minecraft/textures/entity/enderman/enderman.png",
        "endermite" => "assets/minecraft/textures/entity/endermite.png",
        "evoker" => "assets/minecraft/textures/entity/illager/evoker.png",
        "illusioner" => "assets/minecraft/textures/entity/illager/illusioner.png",
        "pillager" => "assets/minecraft/textures/entity/illager/pillager.png",
        "vindicator" => "assets/minecraft/textures/entity/illager/vindicator.png",
        "evoker_fangs" => "assets/minecraft/textures/entity/illager/evoker_fangs.png",
        "experience_bottle" => "assets/minecraft/textures/item/experience_bottle.png",
        "experience_orb" => "assets/minecraft/textures/entity/experience_orb.png",
        "eye_of_ender" => "assets/minecraft/textures/item/ender_eye.png",
        "firework_rocket" => "assets/minecraft/textures/item/firework_rocket.png",
        "fox" => "assets/minecraft/textures/entity/fox/fox.png",
        "frog" => "assets/minecraft/textures/entity/frog/temperate_frog.png",
        "ghast" => "assets/minecraft/textures/entity/ghast/ghast.png",
        "item_frame" => "assets/minecraft/textures/block/item_frame.png",
        "glow_item_frame" => "assets/minecraft/textures/block/glow_item_frame.png",
        "glow_squid" => "assets/minecraft/textures/entity/squid/glow_squid.png",
        "goat" => "assets/minecraft/textures/entity/goat/goat.png",
        "guardian" => "assets/minecraft/textures/entity/guardian.png",
        "hoglin" => "assets/minecraft/textures/entity/hoglin/hoglin.png",
        "horse" => "assets/minecraft/textures/entity/horse/horse_chestnut.png",
        "iron_golem" => "assets/minecraft/textures/entity/iron_golem/iron_golem.png",
        "leash_knot" => "assets/minecraft/textures/entity/lead_knot.png",
        "llama" | "trader_llama" => "assets/minecraft/textures/entity/llama/creamy.png",
        "llama_spit" => "assets/minecraft/textures/entity/llama/spit.png",
        "magma_cube" => "assets/minecraft/textures/entity/slime/magmacube.png",
        "mooshroom" => "assets/minecraft/textures/entity/cow/red_mooshroom.png",
        "painting" => "assets/minecraft/textures/painting/kebab.png",
        "panda" => "assets/minecraft/textures/entity/panda/panda.png",
        "parrot" => "assets/minecraft/textures/entity/parrot/parrot_red_blue.png",
        "phantom" => "assets/minecraft/textures/entity/phantom.png",
        "pig" => "assets/minecraft/textures/entity/pig/pig.png",
        "piglin" => "assets/minecraft/textures/entity/piglin/piglin.png",
        "piglin_brute" => "assets/minecraft/textures/entity/piglin/piglin_brute.png",
        "zombified_piglin" => "assets/minecraft/textures/entity/piglin/zombified_piglin.png",
        "polar_bear" => "assets/minecraft/textures/entity/bear/polarbear.png",
        "potion" => "assets/minecraft/textures/item/splash_potion.png",
        "pufferfish" => "assets/minecraft/textures/entity/fish/pufferfish.png",
        "rabbit" => "assets/minecraft/textures/entity/rabbit/black.png",
        "ravager" => "assets/minecraft/textures/entity/illager/ravager.png",
        "salmon" => "assets/minecraft/textures/entity/fish/salmon.png",
        "sheep" => "assets/minecraft/textures/entity/sheep/sheep.png",
        "shulker" => "assets/minecraft/textures/entity/shulker/shulker.png",
        "shulker_bullet" => "assets/minecraft/textures/entity/shulker/spark.png",
        "silverfish" => "assets/minecraft/textures/entity/silverfish.png",
        "skeleton" => "assets/minecraft/textures/entity/skeleton/skeleton.png",
        "skeleton_horse" => "assets/minecraft/textures/entity/horse/horse_skeleton.png",
        "slime" => "assets/minecraft/textures/entity/slime/slime.png",
        "sniffer" => "assets/minecraft/textures/entity/sniffer/sniffer.png",
        "snow_golem" => "assets/minecraft/textures/entity/snow_golem.png",
        "snowball" => "assets/minecraft/textures/item/snowball.png",
        "trident" => "assets/minecraft/textures/entity/trident.png",
        "tropical_fish" => "assets/minecraft/textures/entity/fish/tropical_a.png",
        "turtle" => "assets/minecraft/textures/entity/turtle/big_sea_turtle.png",
        "vex" => "assets/minecraft/textures/entity/illager/vex.png",
        "villager" => "assets/minecraft/textures/entity/villager/villager.png",
        "wandering_trader" => "assets/minecraft/textures/entity/wandering_trader.png",
        "witch" => "assets/minecraft/textures/entity/witch.png",
        "spider" => "assets/minecraft/textures/entity/spider/spider.png",
        "squid" => "assets/minecraft/textures/entity/squid/squid.png",
        "stray" => "assets/minecraft/textures/entity/skeleton/stray.png",
        "strider" => "assets/minecraft/textures/entity/strider/strider.png",
        "tadpole" => "assets/minecraft/textures/entity/tadpole/tadpole.png",
        "wither" => "assets/minecraft/textures/entity/wither/wither.png",
        "warden" => "assets/minecraft/textures/entity/warden/warden.png",
        "wither_skeleton" => "assets/minecraft/textures/entity/skeleton/wither_skeleton.png",
        "wither_skull" => "assets/minecraft/textures/entity/wither/wither.png",
        "wolf" => "assets/minecraft/textures/entity/wolf/wolf.png",
        "zoglin" => "assets/minecraft/textures/entity/hoglin/zoglin.png",
        "zombie_horse" => "assets/minecraft/textures/entity/horse/horse_zombie.png",
        "player" => "assets/minecraft/textures/entity/player/wide/steve.png",
        "fishing_bobber" => "assets/minecraft/textures/entity/fishing_hook.png",
        _ => return None,
    })
}

/// Texture path for a vanilla block-entity renderer (BER) by normalized
/// renderer name. Names may be supplied with or without `minecraft:`.
pub fn ber_texture_path_for_name(name: &str) -> Option<&'static str> {
    Some(match name.strip_prefix("minecraft:").unwrap_or(name) {
        "chest" => "assets/minecraft/textures/entity/chest/normal.png",
        "trapped_chest" => "assets/minecraft/textures/entity/chest/trapped.png",
        "ender_chest" => "assets/minecraft/textures/entity/chest/ender.png",
        "shulker" => "assets/minecraft/textures/entity/shulker/shulker.png",
        "bed" => "assets/minecraft/textures/entity/bed/red.png",
        "sign" => "assets/minecraft/textures/entity/signs/oak.png",
        "hanging_sign" => "assets/minecraft/textures/entity/signs/hanging/oak.png",
        "banner" => "assets/minecraft/textures/entity/banner_base.png",
        "bell" => "assets/minecraft/textures/entity/bell/bell_body.png",
        "enchanting_table" => "assets/minecraft/textures/entity/enchanting_table_book.png",
        "beacon" => "assets/minecraft/textures/entity/beacon_beam.png",
        "conduit" => "assets/minecraft/textures/entity/conduit/base.png",
        "decorated_pot" => "assets/minecraft/textures/entity/decorated_pot/decorated_pot_base.png",
        _ => return None,
    })
}

/// Sprite paths used by the protocol-767 particle palette. Animated entries
/// are represented as a frame sequence; nonanimated particles have one path.
pub fn particle_texture_paths_for_id(particle_id: u16) -> &'static [&'static str] {
    const EMPTY: &[&str] = &[];
    const GENERIC: &[&str] = &[
        "assets/minecraft/textures/particle/generic_7.png",
        "assets/minecraft/textures/particle/generic_6.png",
        "assets/minecraft/textures/particle/generic_5.png",
        "assets/minecraft/textures/particle/generic_4.png",
        "assets/minecraft/textures/particle/generic_3.png",
        "assets/minecraft/textures/particle/generic_2.png",
        "assets/minecraft/textures/particle/generic_1.png",
        "assets/minecraft/textures/particle/generic_0.png",
    ];
    const FLAME: &[&str] = &["assets/minecraft/textures/particle/flame.png"];
    const SOUL_FLAME: &[&str] = &["assets/minecraft/textures/particle/soul_fire_flame.png"];
    const BUBBLE: &[&str] = &["assets/minecraft/textures/particle/bubble.png"];
    const HEART: &[&str] = &["assets/minecraft/textures/particle/heart.png"];
    const NOTE: &[&str] = &["assets/minecraft/textures/particle/note.png"];
    const EXPLOSION: &[&str] = &[
        "assets/minecraft/textures/particle/explosion_0.png",
        "assets/minecraft/textures/particle/explosion_1.png",
        "assets/minecraft/textures/particle/explosion_2.png",
        "assets/minecraft/textures/particle/explosion_3.png",
        "assets/minecraft/textures/particle/explosion_4.png",
        "assets/minecraft/textures/particle/explosion_5.png",
        "assets/minecraft/textures/particle/explosion_6.png",
        "assets/minecraft/textures/particle/explosion_7.png",
        "assets/minecraft/textures/particle/explosion_8.png",
        "assets/minecraft/textures/particle/explosion_9.png",
        "assets/minecraft/textures/particle/explosion_10.png",
        "assets/minecraft/textures/particle/explosion_11.png",
        "assets/minecraft/textures/particle/explosion_12.png",
        "assets/minecraft/textures/particle/explosion_13.png",
        "assets/minecraft/textures/particle/explosion_14.png",
        "assets/minecraft/textures/particle/explosion_15.png",
    ];
    const GUST: &[&str] = &[
        "assets/minecraft/textures/particle/gust_0.png",
        "assets/minecraft/textures/particle/gust_1.png",
        "assets/minecraft/textures/particle/gust_2.png",
        "assets/minecraft/textures/particle/gust_3.png",
        "assets/minecraft/textures/particle/gust_4.png",
        "assets/minecraft/textures/particle/gust_5.png",
        "assets/minecraft/textures/particle/gust_6.png",
        "assets/minecraft/textures/particle/gust_7.png",
        "assets/minecraft/textures/particle/gust_8.png",
        "assets/minecraft/textures/particle/gust_9.png",
        "assets/minecraft/textures/particle/gust_10.png",
        "assets/minecraft/textures/particle/gust_11.png",
    ];
    const SONIC: &[&str] = &[
        "assets/minecraft/textures/particle/sonic_boom_0.png",
        "assets/minecraft/textures/particle/sonic_boom_1.png",
        "assets/minecraft/textures/particle/sonic_boom_2.png",
        "assets/minecraft/textures/particle/sonic_boom_3.png",
        "assets/minecraft/textures/particle/sonic_boom_4.png",
        "assets/minecraft/textures/particle/sonic_boom_5.png",
        "assets/minecraft/textures/particle/sonic_boom_6.png",
        "assets/minecraft/textures/particle/sonic_boom_7.png",
        "assets/minecraft/textures/particle/sonic_boom_8.png",
        "assets/minecraft/textures/particle/sonic_boom_9.png",
        "assets/minecraft/textures/particle/sonic_boom_10.png",
        "assets/minecraft/textures/particle/sonic_boom_11.png",
        "assets/minecraft/textures/particle/sonic_boom_12.png",
        "assets/minecraft/textures/particle/sonic_boom_13.png",
        "assets/minecraft/textures/particle/sonic_boom_14.png",
        "assets/minecraft/textures/particle/sonic_boom_15.png",
    ];
    const SCULK: &[&str] = &[
        "assets/minecraft/textures/particle/sculk_soul_0.png",
        "assets/minecraft/textures/particle/sculk_soul_1.png",
        "assets/minecraft/textures/particle/sculk_soul_2.png",
        "assets/minecraft/textures/particle/sculk_soul_3.png",
        "assets/minecraft/textures/particle/sculk_soul_4.png",
        "assets/minecraft/textures/particle/sculk_soul_5.png",
        "assets/minecraft/textures/particle/sculk_soul_6.png",
        "assets/minecraft/textures/particle/sculk_soul_7.png",
        "assets/minecraft/textures/particle/sculk_soul_8.png",
        "assets/minecraft/textures/particle/sculk_soul_9.png",
        "assets/minecraft/textures/particle/sculk_soul_10.png",
    ];
    const CHERRY: &[&str] = &[
        "assets/minecraft/textures/particle/cherry_0.png",
        "assets/minecraft/textures/particle/cherry_1.png",
        "assets/minecraft/textures/particle/cherry_2.png",
        "assets/minecraft/textures/particle/cherry_3.png",
        "assets/minecraft/textures/particle/cherry_4.png",
        "assets/minecraft/textures/particle/cherry_5.png",
        "assets/minecraft/textures/particle/cherry_6.png",
        "assets/minecraft/textures/particle/cherry_7.png",
        "assets/minecraft/textures/particle/cherry_8.png",
        "assets/minecraft/textures/particle/cherry_9.png",
        "assets/minecraft/textures/particle/cherry_10.png",
        "assets/minecraft/textures/particle/cherry_11.png",
    ];
    const EFFECT: &[&str] = &[
        "assets/minecraft/textures/particle/effect_0.png",
        "assets/minecraft/textures/particle/effect_1.png",
        "assets/minecraft/textures/particle/effect_2.png",
        "assets/minecraft/textures/particle/effect_3.png",
        "assets/minecraft/textures/particle/effect_4.png",
        "assets/minecraft/textures/particle/effect_5.png",
        "assets/minecraft/textures/particle/effect_6.png",
        "assets/minecraft/textures/particle/effect_7.png",
    ];
    const SPARK: &[&str] = &[
        "assets/minecraft/textures/particle/spark_0.png",
        "assets/minecraft/textures/particle/spark_1.png",
        "assets/minecraft/textures/particle/spark_2.png",
        "assets/minecraft/textures/particle/spark_3.png",
        "assets/minecraft/textures/particle/spark_4.png",
        "assets/minecraft/textures/particle/spark_5.png",
        "assets/minecraft/textures/particle/spark_6.png",
        "assets/minecraft/textures/particle/spark_7.png",
    ];
    const DRIP_HANG: &[&str] = &["assets/minecraft/textures/particle/drip_hang.png"];
    const DRIP_FALL: &[&str] = &["assets/minecraft/textures/particle/drip_fall.png"];
    const DRIP_LAND: &[&str] = &["assets/minecraft/textures/particle/drip_land.png"];
    const GLITTER: &[&str] = &[
        "assets/minecraft/textures/particle/glitter_0.png",
        "assets/minecraft/textures/particle/glitter_1.png",
        "assets/minecraft/textures/particle/glitter_2.png",
        "assets/minecraft/textures/particle/glitter_3.png",
        "assets/minecraft/textures/particle/glitter_4.png",
        "assets/minecraft/textures/particle/glitter_5.png",
        "assets/minecraft/textures/particle/glitter_6.png",
        "assets/minecraft/textures/particle/glitter_7.png",
    ];
    const SGA: &[&str] = &[
        "assets/minecraft/textures/particle/sga_a.png",
        "assets/minecraft/textures/particle/sga_b.png",
        "assets/minecraft/textures/particle/sga_c.png",
        "assets/minecraft/textures/particle/sga_d.png",
        "assets/minecraft/textures/particle/sga_e.png",
        "assets/minecraft/textures/particle/sga_f.png",
        "assets/minecraft/textures/particle/sga_g.png",
        "assets/minecraft/textures/particle/sga_h.png",
        "assets/minecraft/textures/particle/sga_i.png",
        "assets/minecraft/textures/particle/sga_j.png",
        "assets/minecraft/textures/particle/sga_k.png",
        "assets/minecraft/textures/particle/sga_l.png",
        "assets/minecraft/textures/particle/sga_m.png",
        "assets/minecraft/textures/particle/sga_n.png",
        "assets/minecraft/textures/particle/sga_o.png",
        "assets/minecraft/textures/particle/sga_p.png",
        "assets/minecraft/textures/particle/sga_q.png",
        "assets/minecraft/textures/particle/sga_r.png",
        "assets/minecraft/textures/particle/sga_s.png",
        "assets/minecraft/textures/particle/sga_t.png",
        "assets/minecraft/textures/particle/sga_u.png",
        "assets/minecraft/textures/particle/sga_v.png",
        "assets/minecraft/textures/particle/sga_w.png",
        "assets/minecraft/textures/particle/sga_x.png",
        "assets/minecraft/textures/particle/sga_y.png",
        "assets/minecraft/textures/particle/sga_z.png",
    ];
    use megu_client_mesh::ber_instancing::VanillaParticleKind as K;
    let Some(kind) = u8::try_from(particle_id).ok().and_then(K::from_protocol_id) else {
        return EMPTY;
    };
    match kind.registry_name() {
        "flame" | "small_flame" => FLAME,
        "soul_fire_flame" => SOUL_FLAME,
        "bubble" | "bubble_column_up" => BUBBLE,
        "bubble_pop" => &[
            "assets/minecraft/textures/particle/bubble_pop_0.png",
            "assets/minecraft/textures/particle/bubble_pop_1.png",
            "assets/minecraft/textures/particle/bubble_pop_2.png",
            "assets/minecraft/textures/particle/bubble_pop_3.png",
            "assets/minecraft/textures/particle/bubble_pop_4.png",
        ],
        "heart" => HEART,
        "splash" => &[
            "assets/minecraft/textures/particle/splash_0.png",
            "assets/minecraft/textures/particle/splash_1.png",
            "assets/minecraft/textures/particle/splash_2.png",
            "assets/minecraft/textures/particle/splash_3.png",
        ],
        "note" => NOTE,
        "explosion" | "explosion_emitter" => EXPLOSION,
        "gust" | "gust_emitter_large" | "gust_emitter_small" => GUST,
        "small_gust" => &[
            "assets/minecraft/textures/particle/small_gust_0.png",
            "assets/minecraft/textures/particle/small_gust_1.png",
            "assets/minecraft/textures/particle/small_gust_2.png",
            "assets/minecraft/textures/particle/small_gust_3.png",
            "assets/minecraft/textures/particle/small_gust_4.png",
            "assets/minecraft/textures/particle/small_gust_5.png",
            "assets/minecraft/textures/particle/small_gust_6.png",
        ],
        "sonic_boom" => SONIC,
        "sculk_soul" => SCULK,
        "cherry_leaves" => CHERRY,
        "effect" | "instant_effect" | "entity_effect" => EFFECT,
        "crit" | "damage_indicator" => &["assets/minecraft/textures/particle/critical_hit.png"],
        "electric_spark" | "enchanted_hit" | "end_rod" => SPARK,
        "dripping_lava"
        | "dripping_water"
        | "dripping_honey"
        | "dripping_obsidian_tear"
        | "dripping_dripstone_lava"
        | "dripping_dripstone_water" => DRIP_HANG,
        "falling_lava"
        | "falling_water"
        | "falling_honey"
        | "falling_obsidian_tear"
        | "falling_dripstone_lava"
        | "falling_dripstone_water" => DRIP_FALL,
        "landing_lava" | "landing_honey" | "landing_obsidian_tear" => DRIP_LAND,
        "wax_on" | "wax_off" | "glow" | "glow_squid_ink" => GLITTER,
        "enchant" => SGA,
        "trial_spawner_detected_player" => &[
            "assets/minecraft/textures/particle/trial_spawner_detection_0.png",
            "assets/minecraft/textures/particle/trial_spawner_detection_1.png",
            "assets/minecraft/textures/particle/trial_spawner_detection_2.png",
            "assets/minecraft/textures/particle/trial_spawner_detection_3.png",
            "assets/minecraft/textures/particle/trial_spawner_detection_4.png",
        ],
        "trial_spawner_detected_player_ominous" => &[
            "assets/minecraft/textures/particle/trial_spawner_detection_ominous_0.png",
            "assets/minecraft/textures/particle/trial_spawner_detection_ominous_1.png",
            "assets/minecraft/textures/particle/trial_spawner_detection_ominous_2.png",
            "assets/minecraft/textures/particle/trial_spawner_detection_ominous_3.png",
            "assets/minecraft/textures/particle/trial_spawner_detection_ominous_4.png",
        ],
        "ominous_spawning" => &["assets/minecraft/textures/particle/ominous_spawning.png"],
        "angry_villager"
        | "block"
        | "block_marker"
        | "cloud"
        | "dragon_breath"
        | "dust"
        | "dust_color_transition"
        | "elder_guardian"
        | "falling_dust"
        | "firework"
        | "fishing"
        | "infested"
        | "sculk_charge"
        | "sculk_charge_pop"
        | "soul"
        | "flash"
        | "happy_villager"
        | "composter"
        | "item"
        | "vibration"
        | "item_slime"
        | "item_cobweb"
        | "item_snowball"
        | "large_smoke"
        | "lava"
        | "mycelium"
        | "poof"
        | "portal"
        | "rain"
        | "smoke"
        | "white_smoke"
        | "spit"
        | "squid_ink"
        | "sweep_attack"
        | "totem_of_undying"
        | "underwater"
        | "witch"
        | "current_down"
        | "nautilus"
        | "dolphin"
        | "campfire_cosy_smoke"
        | "campfire_signal_smoke"
        | "falling_nectar"
        | "falling_spore_blossom"
        | "ash"
        | "crimson_spore"
        | "warped_spore"
        | "spore_blossom_air"
        | "reverse_portal"
        | "white_ash"
        | "snowflake"
        | "shriek"
        | "egg_crack"
        | "dust_plume"
        | "vault_connection"
        | "dust_pillar"
        | "raid_omen"
        | "trial_omen" => GENERIC,
        _ => GENERIC,
    }
}

/// CPU-side tightly packed atlas pixels and normalized UV rectangles.
#[derive(Clone, Debug)]
pub struct VanillaEntityTextureAtlas {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    entity_uvs: Vec<Option<[f32; 4]>>,
    ber_uvs: BTreeMap<String, [f32; 4]>,
}

impl VanillaEntityTextureAtlas {
    pub fn entity_uv(&self, entity_type_id: u16) -> Option<[f32; 4]> {
        self.entity_uvs
            .get(entity_type_id as usize)
            .copied()
            .flatten()
    }
    pub fn ber_uv(&self, name: &str) -> Option<[f32; 4]> {
        let path = ber_texture_path_for_name(name)?;
        self.ber_uvs.get(path).copied()
    }
}

/// CPU-side particle atlas. `normalized_age` selects a frame across the
/// particle's authored sequence, clamped to `[0, 1]`.
#[derive(Clone, Debug)]
pub struct VanillaParticleTextureAtlas {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    frames: Vec<Vec<[f32; 4]>>,
}

impl VanillaParticleTextureAtlas {
    pub fn particle_uv(&self, particle_id: u16, normalized_age: f32) -> Option<[f32; 4]> {
        let frames = self.frames.get(particle_id as usize)?;
        if frames.is_empty() {
            return None;
        }
        let t = if normalized_age.is_finite() {
            normalized_age.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let frame = ((t * frames.len() as f32).floor() as usize).min(frames.len() - 1);
        Some(frames[frame])
    }
}

fn procedural_pixels(path: &str, particle: bool) -> (u32, u32, Vec<u8>) {
    let mut hash = 0x811c9dc5u32;
    for b in path.bytes() {
        hash = (hash ^ u32::from(b)).wrapping_mul(0x01000193);
    }
    let color = [(hash >> 16) as u8, (hash >> 8) as u8, hash as u8];
    let (w, h) = (16, 16);
    let mut pixels = vec![0; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let dx = x as f32 - 7.5;
            let dy = y as f32 - 7.5;
            let d = (dx * dx + dy * dy).sqrt();
            let spark = particle
                && (path.contains("spark")
                    || path.contains("critical_hit")
                    || path.contains("explosion")
                    || path.contains("sonic_boom"));
            let spark_mask =
                dx.abs() <= 1.25 || dy.abs() <= 1.25 || (dx.abs() - dy.abs()).abs() <= 1.1;
            let alpha = if particle && spark {
                if spark_mask && d <= 8.0 {
                    255
                } else {
                    0
                }
            } else if particle {
                ((8.0 - d).clamp(0.0, 1.0) * 255.0) as u8
            } else {
                255
            };
            let i = (y * w + x) * 4;
            let shade = if particle {
                255
            } else if (x + y) % 5 == 0 {
                160
            } else {
                255
            };
            pixels[i] = (u16::from(color[0]) * shade / 255) as u8;
            pixels[i + 1] = (u16::from(color[1]) * shade / 255) as u8;
            pixels[i + 2] = (u16::from(color[2]) * shade / 255) as u8;
            pixels[i + 3] = alpha;
        }
    }
    (w as u32, h as u32, pixels)
}

type DecodedSprite = (u32, u32, Vec<u8>);
type VisualAtlas = (u32, u32, Vec<u8>, BTreeMap<String, [f32; 4]>);

const MAX_TEXTURE_ATLAS_BYTES: usize = 64 * 1024 * 1024;

fn pack_texture_map(sprites: BTreeMap<String, DecodedSprite>) -> VisualAtlas {
    let width = sprites
        .values()
        .map(|(w, _, _)| w + 2)
        .max()
        .unwrap_or(1)
        .max(2048)
        .next_power_of_two()
        .min(MAX_VISUAL_ATLAS_DIM);
    let height_limit =
        MAX_VISUAL_ATLAS_DIM.min((MAX_TEXTURE_ATLAS_BYTES / (width as usize * 4)) as u32);
    let mut sprites: Vec<_> = sprites.into_iter().collect();
    // Taller cells first keep shelves dense. The map's order breaks ties.
    sprites.sort_by_key(|(_, (_, h, _))| std::cmp::Reverse(*h));
    let mut placed = Vec::new();
    let (mut x, mut y, mut shelf) = (0, 0, 0);
    for (key, (w, h, pixels)) in sprites {
        let (cw, ch) = (w + 2, h + 2);
        if cw > width || ch > height_limit {
            continue;
        }
        let (next_x, next_y, next_shelf) = if x + cw > width {
            (0, y + shelf, 0)
        } else {
            (x, y, shelf)
        };
        if next_y + next_shelf.max(ch) > height_limit {
            continue;
        }
        placed.push((key, w, h, pixels, next_x, next_y));
        (x, y, shelf) = (next_x + cw, next_y, next_shelf.max(ch));
    }
    let height = (y + shelf).max(1);
    let mut rgba = vec![0; (width * height * 4) as usize];
    let mut rects = BTreeMap::new();
    for (key, w, h, pixels, x, y) in placed {
        let stride = width as usize * 4;
        let row_bytes = w as usize * 4;
        for row in 0..h + 2 {
            let source = row.saturating_sub(1).min(h - 1) as usize * row_bytes;
            let dst = (y + row) as usize * stride + (x + 1) as usize * 4;
            rgba[dst..dst + row_bytes].copy_from_slice(&pixels[source..source + row_bytes]);
            rgba.copy_within(dst..dst + 4, dst - 4);
            rgba.copy_within(dst + row_bytes - 4..dst + row_bytes, dst + row_bytes);
        }
        rects.insert(
            key,
            [
                (x + 1) as f32 / width as f32,
                (y + 1) as f32 / height as f32,
                (x + w + 1) as f32 / width as f32,
                (y + h + 1) as f32 / height as f32,
            ],
        );
    }
    (width, height, rgba, rects)
}

#[derive(Clone, Copy)]
enum AssetInput<'a> {
    Path(&'a Path),
    Bytes(&'a [u8]),
}

impl<'a> AssetInput<'a> {
    fn open(self, pack: bool, select: impl Fn(&str) -> bool) -> Result<AssetLayer<'a>, JarError> {
        match self {
            Self::Path(path) => AssetLayer::open(path, pack, select),
            Self::Bytes(bytes) => AssetLayer::from_bytes(bytes, pack, select),
        }
    }
}

fn textures_from_stack(
    base: Option<AssetInput<'_>>,
    packs: &[AssetInput<'_>],
    paths: BTreeSet<String>,
    particle: bool,
) -> BTreeMap<String, DecodedSprite> {
    let mut sprites = BTreeMap::new();
    let mut remaining = MAX_TEXTURE_ATLAS_BYTES;
    for (input, pack) in packs
        .iter()
        .rev()
        .map(|&input| (input, true))
        .chain(base.into_iter().map(|input| (input, false)))
    {
        let Ok(mut layer) = input.open(pack, |name| {
            paths.contains(name) && !sprites.contains_key(name)
        }) else {
            continue;
        };
        for name in layer.names().to_vec() {
            let Ok(bytes) = layer.read(&name, MAX_ENTRY_BYTES as usize) else {
                continue;
            };
            let Ok((w, h)) = check_png_budget(&bytes, remaining) else {
                continue;
            };
            // Allow room for both gutters inside an atlas with the same byte budget.
            if w > MAX_VISUAL_ATLAS_DIM - 2
                || h > MAX_VISUAL_ATLAS_DIM - 2
                || check_rgba_budget(
                    (w + 2).max(2048).next_power_of_two(),
                    h + 2,
                    MAX_TEXTURE_ATLAS_BYTES,
                )
                .is_err()
            {
                continue;
            }
            if let Ok(decoded) = decode_asset_png(&bytes) {
                remaining -= decoded.2.len();
                sprites.insert(name, decoded);
            }
        }
    }
    for path in paths {
        sprites
            .entry(path.clone())
            .or_insert_with(|| procedural_pixels(&path, particle));
    }
    sprites
}

const BER_NAMES: &[&str] = &[
    "chest",
    "trapped_chest",
    "ender_chest",
    "shulker",
    "bed",
    "sign",
    "hanging_sign",
    "banner",
    "bell",
    "enchanting_table",
    "beacon",
    "conduit",
    "decorated_pot",
];

/// Decode fixed entity and BER textures, then apply packs in priority order.
pub fn load_vanilla_entity_atlas_from_jar_bytes(
    jar: &[u8],
    packs: &[&[u8]],
) -> VanillaEntityTextureAtlas {
    let packs: Vec<_> = prioritized_packs(packs)
        .iter()
        .map(|p| AssetInput::Bytes(p))
        .collect();
    load_entity_atlas(Some(AssetInput::Bytes(jar)), &packs)
}

fn load_entity_atlas(
    base: Option<AssetInput<'_>>,
    packs: &[AssetInput<'_>],
) -> VanillaEntityTextureAtlas {
    let model_textures = representative_block_textures(base, packs);
    let entity_path = |id| {
        let default = entity_texture_path_for_id(id)?;
        Some(model_textures.get(default).map_or(default, String::as_str))
    };
    let paths = (0..130)
        .filter_map(entity_path)
        .chain(
            BER_NAMES
                .iter()
                .filter_map(|name| ber_texture_path_for_name(name)),
        )
        .map(str::to_owned)
        .collect();
    let sprites = textures_from_stack(base, packs, paths, false);
    let (width, height, rgba, rects) = pack_texture_map(sprites);
    let entity_uvs = (0..130)
        .map(|id| entity_path(id).and_then(|path| rects.get(path).copied()))
        .collect();
    let ber_uvs = BER_NAMES
        .iter()
        .filter_map(|name| {
            let path = ber_texture_path_for_name(name)?;
            Some((path.to_owned(), *rects.get(path)?))
        })
        .collect();
    VanillaEntityTextureAtlas {
        width,
        height,
        rgba,
        entity_uvs,
        ber_uvs,
    }
}

/// The fixed-size entity atlas supports a single representative texture.
/// Read the TNT side and no-map frame backs from the block model's textures.
fn representative_block_textures(
    base: Option<AssetInput<'_>>,
    packs: &[AssetInput<'_>],
) -> BTreeMap<&'static str, String> {
    use megu_client_mesh::model_baker::RawModelJson;
    let models = [
        (
            "assets/minecraft/models/block/tnt.json",
            "side",
            "assets/minecraft/textures/block/tnt_side.png",
        ),
        (
            "assets/minecraft/models/block/item_frame.json",
            "back",
            "assets/minecraft/textures/block/item_frame.png",
        ),
        (
            "assets/minecraft/models/block/glow_item_frame.json",
            "back",
            "assets/minecraft/textures/block/glow_item_frame.png",
        ),
    ];
    let mut paths = BTreeMap::new();
    for (input, pack) in packs
        .iter()
        .rev()
        .map(|&input| (input, true))
        .chain(base.into_iter().map(|input| (input, false)))
    {
        let Ok(mut layer) = input.open(pack, |name| {
            models.iter().any(|&(model, _, _)| model == name)
        }) else {
            continue;
        };
        for (name, slot, fallback) in models {
            if paths.contains_key(fallback)
                || layer
                    .names()
                    .binary_search_by(|key| key.as_str().cmp(name))
                    .is_err()
            {
                continue;
            }
            let model = layer
                .read(name, 64 * 1024)
                .ok()
                .and_then(|b| String::from_utf8(b).ok())
                .and_then(|text| RawModelJson::parse(&text).ok());
            if let Some(path) = model
                .as_ref()
                .and_then(|model| model_texture_path(&model.textures, slot))
            {
                paths.insert(fallback, path);
            }
        }
        if paths.len() == models.len() {
            break;
        }
    }
    paths
}

fn model_texture_path<'a>(
    textures: &'a BTreeMap<String, String>,
    mut slot: &'a str,
) -> Option<String> {
    // A texture alias can have at most one edge per map entry.
    for _ in 0..textures.len() {
        let value = textures.get(slot)?;
        if let Some(alias) = value.strip_prefix('#') {
            slot = alias;
        } else {
            return texture_resource_path(value, "");
        }
    }
    None
}

/// Decode particle frames in the order specified by the winning particle definition.
pub fn load_vanilla_particle_atlas_from_jar_bytes(
    jar: &[u8],
    packs: &[&[u8]],
) -> VanillaParticleTextureAtlas {
    let packs: Vec<_> = prioritized_packs(packs)
        .iter()
        .map(|p| AssetInput::Bytes(p))
        .collect();
    load_particle_atlas(Some(AssetInput::Bytes(jar)), &packs)
}

pub(crate) fn load_visual_atlases_from_paths(
    jar: Option<&Path>,
    packs: &[PathBuf],
) -> (VanillaEntityTextureAtlas, VanillaParticleTextureAtlas) {
    let base = jar.map(AssetInput::Path);
    let packs: Vec<_> = prioritized_packs(packs)
        .iter()
        .map(|path| AssetInput::Path(path))
        .collect();
    (
        load_entity_atlas(base, &packs),
        load_particle_atlas(base, &packs),
    )
}

fn particle_definition_path(id: u8) -> Option<String> {
    use megu_client_mesh::ber_instancing::VanillaParticleKind;
    let name = match VanillaParticleKind::from_protocol_id(id)?.registry_name() {
        "trial_spawner_detected_player" => "trial_spawner_detection",
        "trial_spawner_detected_player_ominous" => "trial_spawner_detection_ominous",
        name => name,
    };
    Some(format!("assets/minecraft/particles/{name}.json"))
}

fn parse_particle_frames(bytes: &[u8]) -> Option<Vec<String>> {
    let json = megu_core::json::parse_json(std::str::from_utf8(bytes).ok()?).ok()?;
    let frames = json.get("textures")?.as_array()?;
    if frames.is_empty() || frames.len() > 64 {
        return None;
    }
    frames
        .iter()
        .map(|frame| texture_resource_path(frame.as_str()?, "particle/"))
        .collect()
}

fn texture_resource_path(id: &str, family: &str) -> Option<String> {
    let (namespace, path) = id.split_once(':').unwrap_or(("minecraft", id));
    if !valid_mod_namespace(namespace)
        || path.len() > 256
        || validate_entry_name(path).is_err()
        || !path
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"/_.-".contains(&b))
    {
        return None;
    }
    Some(format!("assets/{namespace}/textures/{family}{path}.png"))
}

fn load_particle_atlas(
    base: Option<AssetInput<'_>>,
    packs: &[AssetInput<'_>],
) -> VanillaParticleTextureAtlas {
    let mut frames: Vec<Vec<String>> = (0..=108)
        .map(|id| {
            particle_texture_paths_for_id(id)
                .iter()
                .map(|p| (*p).to_owned())
                .collect()
        })
        .collect();
    let definitions: BTreeMap<_, _> = (0..=108)
        .filter_map(|id| Some((particle_definition_path(id)?, usize::from(id))))
        .collect();
    for (input, pack) in base
        .into_iter()
        .map(|input| (input, false))
        .chain(packs.iter().map(|&input| (input, true)))
    {
        let Ok(mut layer) = input.open(pack, |name| definitions.contains_key(name)) else {
            continue;
        };
        for name in layer.names().to_vec() {
            if let Some(sequence) = layer
                .read(&name, 64 * 1024)
                .ok()
                .and_then(|bytes| parse_particle_frames(&bytes))
            {
                frames[definitions[&name]] = sequence;
            }
        }
    }
    let paths = frames.iter().flatten().cloned().collect();
    let sprites = textures_from_stack(base, packs, paths, true);
    let (width, height, rgba, rects) = pack_texture_map(sprites);
    let frames = frames
        .into_iter()
        .map(|sequence| {
            sequence
                .into_iter()
                .filter_map(|path| rects.get(&path).copied())
                .collect()
        })
        .collect();
    VanillaParticleTextureAtlas {
        width,
        height,
        rgba,
        frames,
    }
}

// ---------------------------------------------------------------------------
// Clean-room RFC 1951 inflate (shared shape with client-port's shaderpack
// reader; private there, so this lane keeps its own copy)
// ---------------------------------------------------------------------------

/// Inflate `input` (raw DEFLATE) into at most `limit` bytes. Output is
/// capped at `limit` so a hostile stream cannot expand unboundedly.
#[cfg(test)]
mod tests {
    use super::*;
    use megu_client_mesh::asset_pipeline::CompiledAssetBundle;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_TEMP_DIR: AtomicUsize = AtomicUsize::new(0);

    fn test_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "megu-mod-{label}-{}-{}",
            std::process::id(),
            NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::create_dir_all(&dir).expect("test directory created");
        dir
    }

    // One valid 1x1 RGBA PNG with the non-fallback pixel [66,160,208,255].
    const CREATE_PNG: [u8; 70] = [
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82,
        0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 31, 21, 196,
        137, 0, 0, 0, 13, 73, 68, 65, 84, 120, 156, 99, 112, 90, 112,
        225, 63, 0, 5, 140, 2, 178, 39, 213, 187, 120, 0, 0, 0,
        0, 73, 69, 78, 68, 174, 66, 96, 130,
    ];

    fn create_jar(model: &[u8]) -> Vec<u8> {
        stored_zip(&[
            (
                "assets/create/blockstates/andesite_casing.json",
                br#"{"variants":{"":{"model":"create:block/andesite_casing"}}}"#,
            ),
            ("assets/create/models/block/andesite_casing.json", model),
            (
                "assets/create/textures/block/andesite_casing.png",
                &CREATE_PNG,
            ),
            (
                "assets/create/lang/en_us.json",
                br#"{"block.create.andesite_casing":"Andesite Casing"}"#,
            ),
            ("META-INF/MANIFEST.MF", b"mod metadata"),
        ])
    }

    #[test]
    fn mod_jars_layer_in_filename_order_and_render_create_texture() {
        let root = test_dir("create");
        let mods = root.join("mods");
        std::fs::create_dir_all(&mods).unwrap();
        let model = br##"{"loader":"create:kinetic","render_type":"cutout","textures":{"particle":"create:block/andesite_casing"}}"##;
        std::fs::write(mods.join("z-create-test.jar"), create_jar(model)).unwrap();
        std::fs::write(mods.join("a-create-test.jar"), create_jar(model)).unwrap();
        let vanilla_path = root.join("vanilla.jar");
        std::fs::write(
            &vanilla_path,
            stored_zip(&[("assets/minecraft/textures/block/stone.png", &CREATE_PNG)]),
        )
        .unwrap();
        let (vanilla, _) = vanilla_jar_texture_layer(&vanilla_path).unwrap();
        let (layers, report) = load_mod_jar_layers(&mods);
        assert_eq!(report.loaded.len(), 2);
        assert!(report.rejected.is_empty(), "{:?}", report.rejected);
        assert_eq!(report.loaded[0].file_name().unwrap(), "a-create-test.jar");
        assert_eq!(report.loaded[1].file_name().unwrap(), "z-create-test.jar");
        assert_eq!(layers[0].blockstates.len(), 1);
        assert_eq!(layers[0].models.len(), 1);
        assert_eq!(layers[0].sprites.len(), 1);
        let bundle = CompiledAssetBundle::build(&[vanilla, layers[0].clone(), layers[1].clone()])
            .expect("mod and vanilla bundle");
        assert!(bundle.atlas.placement("minecraft:block/stone").is_some());
        let quads = bundle.bake_block_with_uvs("create:andesite_casing", &[], [0, 0, 0])
            .expect("Create loader fallback geometry");
        assert_eq!(quads.len(), 6);
        assert!(quads.iter().all(|quad| quad.texture == "create:block/andesite_casing"));
        let p = bundle.atlas.placement("create:block/andesite_casing").unwrap();
        let offset = ((p.y * bundle.atlas.width + p.x) * 4) as usize;
        assert_eq!(&bundle.atlas.rgba[offset..offset + 4], &[66, 160, 208, 255]);
        // Later user resource packs retain priority over the mod's texture.
        let mut override_layer = ResourcePackLayer::default();
        override_layer.sprites.insert(
            "create:block/andesite_casing".into(),
            (1, 1, vec![10, 20, 30, 255]),
        );
        let overridden = CompiledAssetBundle::build(&[layers[1].clone(), override_layer]).unwrap();
        let p = overridden.atlas.placement("create:block/andesite_casing").unwrap();
        let offset = ((p.y * overridden.atlas.width + p.x) * 4) as usize;
        assert_eq!(&overridden.atlas.rgba[offset..offset + 4], &[10, 20, 30, 255]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mod_model_inherits_geometry_from_vanilla_client_jar() {
        let root = test_dir("parent");
        let mods = root.join("mods");
        std::fs::create_dir_all(&mods).unwrap();
        std::fs::write(mods.join("create-test.jar"), create_jar(
            br##"{"parent":"minecraft:block/cube_all","textures":{"all":"create:block/andesite_casing"}}"##,
        )).unwrap();
        let vanilla_path = root.join("vanilla.jar");
        std::fs::write(&vanilla_path, stored_zip(&[
            ("assets/minecraft/models/block/cube_all.json", br##"{"elements":[{"from":[0,0,0],"to":[16,16,16],"faces":{"down":{"texture":"#all"},"up":{"texture":"#all"},"north":{"texture":"#all"},"south":{"texture":"#all"},"west":{"texture":"#all"},"east":{"texture":"#all"}}}]}"##),
        ])).unwrap();
        let (vanilla, _) = vanilla_jar_texture_layer(&vanilla_path).unwrap();
        assert!(vanilla.models.contains_key("minecraft:block/cube_all"));
        let (mods, report) = load_mod_jar_layers(&mods);
        assert!(report.rejected.is_empty(), "{:?}", report.rejected);
        let bundle = CompiledAssetBundle::build(&[vanilla, mods[0].clone()]).unwrap();
        let quads = bundle.bake_block_with_uvs("create:andesite_casing", &[], [0, 0, 0]).unwrap();
        assert_eq!(quads.len(), 6);
        assert!(quads.iter().all(|quad| quad.texture == "create:block/andesite_casing"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn corrupt_unsafe_and_oversized_mod_jars_do_not_break_vanilla() {
        let root = test_dir("hostile");
        let mods = root.join("mods");
        std::fs::create_dir_all(&mods).unwrap();
        std::fs::write(mods.join("a-corrupt.jar"), b"not a zip").unwrap();
        std::fs::write(
            mods.join("b-traversal.jar"),
            stored_zip(&[("assets/create/models/../block/bad.json", b"{}")]),
        )
        .unwrap();
        let mut huge = stored_zip(&[("assets/create/textures/block/bad.png", b"x")]);
        let cd_offset = huge
            .windows(4)
            .position(|window| window == b"PK\x01\x02")
            .unwrap();
        huge[cd_offset + 24..cd_offset + 28]
            .copy_from_slice(&((MAX_ENTRY_BYTES + 1) as u32).to_le_bytes());
        std::fs::write(mods.join("c-oversized.jar"), huge).unwrap();
        std::fs::write(mods.join("z-good.jar"), create_jar(
            br##"{"textures":{"all":"create:block/andesite_casing"},"elements":[{"from":[0,0,0],"to":[16,16,16],"faces":{"north":{"texture":"#all"}}}]}"##,
        )).unwrap();
        let (layers, report) = load_mod_jar_layers(&mods);
        assert_eq!(report.rejected.len(), 3, "{:?}", report.rejected);
        assert_eq!(report.loaded.len(), 1);
        assert_eq!(layers.len(), 1);
        let mut vanilla = ResourcePackLayer::default();
        vanilla.sprites.insert(
            "minecraft:block/stone".into(),
            (1, 1, vec![100, 120, 140, 255]),
        );
        let bundle = CompiledAssetBundle::build(&[vanilla, layers[0].clone()]).unwrap();
        assert!(bundle.atlas.placement("minecraft:block/stone").is_some());
        assert!(bundle.atlas.placement("create:block/andesite_casing").is_some());
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Minimal stored-ZIP writer for unit tests (no compression needed to
    /// prove the reader's structure handling).
    fn stored_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let owned: Vec<(String, Vec<u8>, bool)> = entries
            .iter()
            .map(|(name, data)| ((*name).to_owned(), data.to_vec(), false))
            .collect();
        test_zip(&owned)
    }

    /// Raw RFC 1951 stream made only of stored blocks — the DEFLATE method a
    /// `test_zip` entry marked `deflate` carries inside the archive.
    fn raw_deflate_stored(data: &[u8]) -> Vec<u8> {
        let blocks: Vec<&[u8]> = if data.is_empty() {
            vec![&[][..]]
        } else {
            data.chunks(65_535).collect()
        };
        let mut out = Vec::with_capacity(data.len() + blocks.len() * 5);
        for (index, chunk) in blocks.iter().enumerate() {
            out.push(if index + 1 == blocks.len() { 0x01 } else { 0x00 });
            let len = chunk.len() as u16;
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&(!len).to_le_bytes());
            out.extend_from_slice(chunk);
        }
        out
    }

    /// ZIP writer for unit tests: `(name, contents, deflate)` per entry.
    /// `deflate` entries carry a raw stored-block RFC 1951 stream so the
    /// reader's inflate path is exercised without a compressor dependency.
    fn test_zip(entries: &[(String, Vec<u8>, bool)]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, data, deflate) in entries {
            let deflated;
            let (method, payload): (u16, &[u8]) = if *deflate {
                deflated = raw_deflate_stored(data);
                (8, &deflated)
            } else {
                (0, data.as_slice())
            };
            let lh_off = out.len() as u32;
            out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
            out.extend_from_slice(&20u16.to_le_bytes()); // version
            out.extend_from_slice(&0u16.to_le_bytes()); // flags
            out.extend_from_slice(&method.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes()); // time
            out.extend_from_slice(&0u16.to_le_bytes()); // date
            out.extend_from_slice(&0u32.to_le_bytes()); // crc
            out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes()); // extra
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(payload);

            central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&method.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u32.to_le_bytes());
            central.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u32.to_le_bytes());
            central.extend_from_slice(&lh_off.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let cd_offset = out.len() as u32;
        out.extend_from_slice(&central);
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(central.len() as u32).to_le_bytes());
        out.extend_from_slice(&cd_offset.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    /// Minimal valid 1x1 RGBA PNG with a caller-chosen texel — CRCs are
    /// computed for real so `decode_png_rgba` accepts it.
    fn solid_png(r: u8, g: u8, b: u8, a: u8) -> Vec<u8> {
        fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
            let mut out = Vec::new();
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            out.extend_from_slice(kind);
            out.extend_from_slice(data);
            let crc = megu_core::hash::crc32_ieee_update(
                megu_core::hash::crc32_ieee_update(0, kind),
                data,
            );
            out.extend_from_slice(&crc.to_be_bytes());
            out
        }
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&1u32.to_be_bytes());
        ihdr.extend_from_slice(&1u32.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
        let idat = megu_core::zip_archive::zlib_compress_stored(&[0, r, g, b, a]);
        let mut png = vec![137, 80, 78, 71, 13, 10, 26, 10];
        png.extend(chunk(b"IHDR", &ihdr));
        png.extend(chunk(b"IDAT", &idat));
        png.extend(chunk(b"IEND", &[]));
        png
    }

    /// A self-contained mod JAR for namespace `ns` exposing one block
    /// `ns:<block>` (blockstate + model + texture + lang), the same shape a
    /// real NeoForge/Fabric library JAR carries.
    fn namespace_jar(ns: &str, block: &str, png: &[u8]) -> Vec<u8> {
        let blockstate = format!(
            "{{\"variants\":{{\"\":{{\"model\":\"{ns}:block/{block}\"}}}}}}"
        );
        let model = format!(
            "{{\"textures\":{{\"all\":\"{ns}:block/{block}\"}},\"elements\":[{{\"from\":[0,0,0],\"to\":[16,16,16],\"faces\":{{\"down\":{{\"texture\":\"#all\"}},\"up\":{{\"texture\":\"#all\"}},\"north\":{{\"texture\":\"#all\"}},\"south\":{{\"texture\":\"#all\"}},\"west\":{{\"texture\":\"#all\"}},\"east\":{{\"texture\":\"#all\"}}}}}}]}}"
        );
        let lang = format!("{{\"block.{ns}.{block}\":\"{block}\"}}");
        test_zip(&[
            (
                format!("assets/{ns}/blockstates/{block}.json"),
                blockstate.into_bytes(),
                false,
            ),
            (
                format!("assets/{ns}/models/block/{block}.json"),
                model.into_bytes(),
                false,
            ),
            (
                format!("assets/{ns}/textures/block/{block}.png"),
                png.to_vec(),
                false,
            ),
            (
                format!("assets/{ns}/lang/en_us.json"),
                lang.into_bytes(),
                false,
            ),
            (
                "META-INF/MANIFEST.MF".to_owned(),
                b"nested mod".to_vec(),
                false,
            ),
        ])
    }

    /// Sprite texel at the atlas placement of `key`, for override assertions.
    fn atlas_texel(bundle: &CompiledAssetBundle, key: &str) -> [u8; 4] {
        let placement = bundle.atlas.placement(key).expect("sprite placed");
        let offset = ((placement.y * bundle.atlas.width + placement.x) * 4) as usize;
        bundle.atlas.rgba[offset..offset + 4].try_into().unwrap()
    }

    #[test]
    fn wrapped_pack_and_quoted_options_names_resolve_to_assets() {
        let dir = std::env::temp_dir().join(format!("megu-ziptest-wrapped-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("options.txt"),
            "resourcePacks:[\"vanilla\",\"file/one,two.zip\",\"file/last.zip\"]",
        )
        .unwrap();
        assert_eq!(configured_resource_packs(&dir), ["one,two.zip", "last.zip"]);
        let zip = dir.join("one,two.zip");
        std::fs::write(
            &zip,
            stored_zip(&[("root/assets/example/models/block/one.json", b"{}")]),
        )
        .unwrap();
        let (layer, report) = zip_pack_layer(&zip).unwrap();
        assert_eq!(
            layer.models.get("example:block/one").map(String::as_str),
            Some("{}")
        );
        assert_eq!(report.entries_loaded, 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn central_directory_extracts_selected_stored_entries() {
        let zip = stored_zip(&[
            (
                "assets/minecraft/textures/block/stone.png",
                b"PNG".as_slice(),
            ),
            (
                "assets/minecraft/textures/item/apple.png",
                b"APP".as_slice(),
            ),
            ("META-INF/MANIFEST.MF", b"MF".as_slice()),
        ]);
        let dir = std::env::temp_dir().join(format!("megu-ziptest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let jar = dir.join("test.jar");
        std::fs::write(&jar, &zip).unwrap();
        let (entries, report) = read_selected_entries(&jar, |name| name.ends_with(".png")).unwrap();
        assert_eq!(report.entries_seen, 3);
        assert_eq!(report.entries_loaded, 2);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "assets/minecraft/textures/block/stone.png");
        assert_eq!(entries[0].bytes, b"PNG");
        std::fs::remove_file(&jar).ok();
    }

    #[test]
    fn traversal_entry_is_recorded_not_followed() {
        let zip = stored_zip(&[
            ("../evil.png", b"E".as_slice()),
            ("assets/minecraft/textures/block/ok.png", b"K".as_slice()),
        ]);
        let dir = std::env::temp_dir().join(format!("megu-ziptest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let jar = dir.join("evil.jar");
        std::fs::write(&jar, &zip).unwrap();
        let res = read_selected_entries(&jar, |name| name.ends_with(".png"));
        match res {
            Err(err) => {
                let msg = format!("{err:?}");
                assert!(
                    msg.contains("UnsafePath") || msg.contains("evil.png"),
                    "expected fail-closed traversal error, got: {msg}"
                );
            }
            Ok((entries, report)) => {
                assert_eq!(entries.len(), 1);
                assert_eq!(report.skipped.len(), 1);
            }
        }
        std::fs::remove_file(&jar).ok();
    }

    /// Mirrors `create-aeronautics-bundled-1.21.1-1.3.3.jar`: an outer JAR
    /// with zero top-level assets nesting `aeronautics`, `offroad` and
    /// `simulated` under `META-INF/jarjar/*.jar` (NeoForge). All three
    /// namespaces' blockstates, models and textures must reach the bundle.
    #[test]
    fn bundled_neoforge_jarjar_loads_all_nested_namespaces() {
        let root = test_dir("jarjar");
        let mods = root.join("mods");
        std::fs::create_dir_all(&mods).unwrap();
        let outer = test_zip(&[
            (
                "META-INF/MANIFEST.MF".to_owned(),
                b"Create Aeronautics bundled".to_vec(),
                false,
            ),
            (
                "META-INF/jarjar/aeronautics.jar".to_owned(),
                namespace_jar("aeronautics", "rotor", &CREATE_PNG),
                false,
            ),
            (
                "META-INF/jarjar/offroad.jar".to_owned(),
                namespace_jar("offroad", "wheel", &solid_png(200, 30, 30, 255)),
                true, // Deflate-compressed nested JAR, like real jarjar output.
            ),
            (
                "META-INF/jarjar/simulated.jar".to_owned(),
                namespace_jar("simulated", "engine", &solid_png(30, 200, 30, 255)),
                false,
            ),
        ]);
        std::fs::write(
            mods.join("create-aeronautics-bundled-1.21.1-1.3.3.jar"),
            outer,
        )
        .unwrap();

        let (layers, report) = load_mod_jar_layers(&mods);
        assert_eq!(report.loaded.len(), 1, "{:?}", report.rejected);
        assert!(report.rejected.is_empty(), "{:?}", report.rejected);
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        // Sorted nested-JAR order; the asset-less wrapper emits no layer.
        assert_eq!(layers.len(), 3);
        assert_eq!(layers[0].name, "META-INF/jarjar/aeronautics.jar");
        assert_eq!(layers[1].name, "META-INF/jarjar/offroad.jar");
        assert_eq!(layers[2].name, "META-INF/jarjar/simulated.jar");
        for layer in &layers {
            assert_eq!(layer.blockstates.len(), 1, "{}", layer.name);
            assert_eq!(layer.models.len(), 1, "{}", layer.name);
            assert_eq!(layer.sprites.len(), 1, "{}", layer.name);
        }

        let bundle = CompiledAssetBundle::build(&layers).expect("bundled jarjar bundle");
        for (ns, block) in [
            ("aeronautics", "rotor"),
            ("offroad", "wheel"),
            ("simulated", "engine"),
        ] {
            let id = format!("{ns}:{block}");
            let quads = bundle
                .bake_block_with_uvs(&id, &[], [0, 0, 0])
                .unwrap_or_else(|err| panic!("{id} should bake: {err}"));
            assert!(!quads.is_empty(), "{id} produced no quads");
            assert!(bundle.atlas.placement(&format!("{ns}:block/{block}")).is_some());
        }
        // The deflated `offroad` nested JAR decoded to its own texel.
        assert_eq!(atlas_texel(&bundle, "offroad:block/wheel"), [200, 30, 30, 255]);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Fabric nests libraries under `META-INF/jars/*.jar`. The nested JAR's
    /// assets load alongside the wrapper's own top-level assets, and the
    /// wrapper layer — pushed last — wins shared-key collisions.
    #[test]
    fn fabric_jars_dir_nested_assets_load_with_outer_override() {
        let root = test_dir("fabric-jars");
        let mods = root.join("mods");
        std::fs::create_dir_all(&mods).unwrap();

        let inner_png = solid_png(10, 20, 30, 255);
        let outer_png = solid_png(90, 80, 70, 255);
        let inner = namespace_jar("fibers", "strand", &inner_png);
        // Same texture key in both JARs; the outer wrapper must win.
        let mut outer_entries: Vec<(String, Vec<u8>, bool)> = vec![
            ("META-INF/jars/fibers-lib.jar".to_owned(), inner, false),
            (
                "assets/fibers/blockstates/strand.json".to_owned(),
                br#"{"variants":{"":{"model":"fibers:block/strand"}}}"#.to_vec(),
                false,
            ),
            (
                "assets/fibers/textures/block/strand.png".to_owned(),
                outer_png,
                false,
            ),
        ];
        // Give the wrapper its own distinct block too, proving merge.
        outer_entries.extend(namespace_jar_entries(
            "fibers",
            "bolt",
            &solid_png(1, 2, 3, 255),
        ));
        let outer = test_zip(&outer_entries);
        std::fs::write(mods.join("fibers-bundled.jar"), outer).unwrap();

        let (layers, report) = load_mod_jar_layers(&mods);
        assert!(report.rejected.is_empty(), "{:?}", report.rejected);
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        assert_eq!(report.loaded.len(), 1);
        assert_eq!(layers.len(), 2, "{:?}", layers.iter().map(|l| &l.name).collect::<Vec<_>>());
        assert_eq!(layers[0].name, "META-INF/jars/fibers-lib.jar");
        assert_eq!(layers[1].name, "fibers-bundled.jar");

        let bundle = CompiledAssetBundle::build(&layers).expect("fabric bundle");
        // Inner-JAR block bakes; the shared texture resolves to the OUTER
        // wrapper's texel because its layer is later in the stack.
        let quads = bundle
            .bake_block_with_uvs("fibers:strand", &[], [0, 0, 0])
            .expect("inner strand bakes");
        assert!(!quads.is_empty());
        assert_eq!(atlas_texel(&bundle, "fibers:block/strand"), [90, 80, 70, 255]);
        let bolt = bundle
            .bake_block_with_uvs("fibers:bolt", &[], [0, 0, 0])
            .expect("outer bolt bakes");
        assert!(!bolt.is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    /// `(name, contents, deflate)` entries of [`namespace_jar`] without the
    /// trailing manifest, for merging into a hand-built wrapper JAR.
    fn namespace_jar_entries(ns: &str, block: &str, png: &[u8]) -> Vec<(String, Vec<u8>, bool)> {
        let blockstate = format!(
            "{{\"variants\":{{\"\":{{\"model\":\"{ns}:block/{block}\"}}}}}}"
        );
        let model = format!(
            "{{\"textures\":{{\"all\":\"{ns}:block/{block}\"}},\"elements\":[{{\"from\":[0,0,0],\"to\":[16,16,16],\"faces\":{{\"down\":{{\"texture\":\"#all\"}},\"up\":{{\"texture\":\"#all\"}},\"north\":{{\"texture\":\"#all\"}},\"south\":{{\"texture\":\"#all\"}},\"west\":{{\"texture\":\"#all\"}},\"east\":{{\"texture\":\"#all\"}}}}}}]}}"
        );
        vec![
            (
                format!("assets/{ns}/blockstates/{block}.json"),
                blockstate.into_bytes(),
                false,
            ),
            (
                format!("assets/{ns}/models/block/{block}.json"),
                model.into_bytes(),
                false,
            ),
            (
                format!("assets/{ns}/textures/block/{block}.png"),
                png.to_vec(),
                false,
            ),
        ]
    }

    /// Nesting deeper than [`MAX_JIJ_DEPTH`] stops cleanly, and a corrupt or
    /// traversal-carrying nested JAR is skipped without breaking the valid
    /// siblings inside the same wrapper or other mod JARs.
    #[test]
    fn jarjar_depth_cap_and_hostile_nested_jars_are_isolated() {
        let root = test_dir("jarjar-hostile");
        let mods = root.join("mods");
        std::fs::create_dir_all(&mods).unwrap();

        // level3 -> level2 -> level1 -> outer: only depths 1 and 2 load.
        let level3 = namespace_jar("deep3", "core", &CREATE_PNG);
        let level2 = test_zip(
            &[("META-INF/jarjar/level3.jar".to_owned(), level3, false)]
                .into_iter()
                .chain(namespace_jar_entries("deep2", "gear", &CREATE_PNG))
                .collect::<Vec<_>>(),
        );
        let level1 = test_zip(
            &[("META-INF/jarjar/level2.jar".to_owned(), level2, false)]
                .into_iter()
                .chain(namespace_jar_entries("deep1", "shaft", &CREATE_PNG))
                .collect::<Vec<_>>(),
        );

        let traversal = stored_zip(&[("assets/evil/../block/bad.json", b"{}")]);
        let broken = b"this is not a zip archive at all".to_vec();
        let outer = test_zip(&[
            ("META-INF/jarjar/level1.jar".to_owned(), level1, false),
            ("META-INF/jarjar/traversal.jar".to_owned(), traversal, false),
            ("META-INF/jarjar/broken.jar".to_owned(), broken, false),
            (
                "META-INF/jarjar/valid.jar".to_owned(),
                namespace_jar("good", "part", &CREATE_PNG),
                false,
            ),
        ]);
        std::fs::write(mods.join("nested-hostile.jar"), outer).unwrap();
        // A plain sibling mod JAR must be unaffected either way.
        std::fs::write(
            mods.join("zzz-sibling.jar"),
            create_jar(
                br##"{"textures":{"all":"create:block/andesite_casing"},"elements":[{"from":[0,0,0],"to":[16,16,16],"faces":{"north":{"texture":"#all"}}}]}"##,
            ),
        )
        .unwrap();

        let (layers, report) = load_mod_jar_layers(&mods);
        assert_eq!(report.loaded.len(), 2, "{:?}", report.rejected);
        assert!(report.rejected.is_empty(), "{:?}", report.rejected);
        // Depth-3 JAR ignored, traversal + corrupt nested JARs recorded.
        assert!(
            report
                .skipped
                .iter()
                .any(|note| note.contains("level3.jar") && note.contains("depth")),
            "{:?}",
            report.skipped
        );
        assert!(
            report.skipped.iter().any(|note| note.contains("traversal.jar")),
            "{:?}",
            report.skipped
        );
        assert!(
            report.skipped.iter().any(|note| note.contains("broken.jar")),
            "{:?}",
            report.skipped
        );

        let bundle = CompiledAssetBundle::build(&layers).expect("hostile bundle");
        for id in [
            "deep1:shaft",
            "deep2:gear",
            "good:part",
            "create:andesite_casing",
        ] {
            assert!(
                bundle
                    .bake_block_with_uvs(id, &[], [0, 0, 0])
                    .is_ok_and(|quads| !quads.is_empty()),
                "{id} should bake"
            );
        }
        assert!(bundle.atlas.placement("deep3:block/core").is_none());
        std::fs::remove_dir_all(root).unwrap();
    }
}
