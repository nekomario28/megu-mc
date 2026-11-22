// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
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
//!
//! The DEFLATE decoder is the same clean-room RFC 1951 implementation
//! `client-port`'s shaderpack loader carries (private there, so this lane
//! keeps its own copy rather than widening another crate's API).

use std::fmt;
use std::path::Path;

use mc_spine_client_mesh::asset_pipeline::ResourcePackLayer;
use mc_spine_client_mesh::png_decode;

/// Total on-disk bytes the archive itself may occupy.
pub const MAX_ARCHIVE_BYTES: u64 = 256 * 1024 * 1024;
/// Central-directory entry count bound.
pub const MAX_ARCHIVE_ENTRIES: usize = 50_000;
/// Sum of uncompressed bytes the reader will materialize.
pub const MAX_UNPACKED_BYTES: u64 = 256 * 1024 * 1024;
/// Per-entry uncompressed cap (matches the asset pipeline's pack-file cap).
pub const MAX_ENTRY_BYTES: u64 = 8 * 1024 * 1024;

const EOCD_SEARCH_WINDOW: usize = 66_000;

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

fn u16le(data: &[u8], off: usize) -> Result<u16, JarError> {
    data.get(off..off + 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .ok_or_else(|| JarError::Malformed {
            reason: "truncated record".to_owned(),
        })
}

fn u32le(data: &[u8], off: usize) -> Result<u32, JarError> {
    data.get(off..off + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or_else(|| JarError::Malformed {
            reason: "truncated record".to_owned(),
        })
}

/// Read `path` and decompress the central-directory entries whose names
/// pass `select`. The archive itself is read once, size-checked before and
/// after the read, and only matching entries are inflated — a hostile
/// archive can never make the client decompress unrelated payloads.
pub fn read_selected_entries(
    path: &Path,
    select: impl Fn(&str) -> bool,
) -> Result<(Vec<JarEntry>, JarReadReport), JarError> {
    let meta = std::fs::metadata(path).map_err(|e| JarError::Io {
        reason: format!("cannot stat {}: {e}", path.display()),
    })?;
    if meta.len() > MAX_ARCHIVE_BYTES {
        return Err(JarError::BudgetExceeded {
            reason: format!(
                "{} is {} bytes (limit {MAX_ARCHIVE_BYTES})",
                path.display(),
                meta.len()
            ),
        });
    }
    let zip = std::fs::read(path).map_err(|e| JarError::Io {
        reason: format!("cannot read {}: {e}", path.display()),
    })?;
    if zip.len() as u64 > MAX_ARCHIVE_BYTES {
        return Err(JarError::BudgetExceeded {
            reason: format!("{} exceeds {MAX_ARCHIVE_BYTES} bytes", path.display()),
        });
    }
    let mut report = JarReadReport::default();
    let entries = read_central_directory(&zip, select, &mut report)?;
    Ok((entries, report))
}

fn read_central_directory(
    zip: &[u8],
    select: impl Fn(&str) -> bool,
    report: &mut JarReadReport,
) -> Result<Vec<JarEntry>, JarError> {
    let malformed = |reason: &str| JarError::Malformed {
        reason: reason.to_owned(),
    };
    // Locate EOCD by scanning the tail window backwards.
    let start = zip.len().saturating_sub(EOCD_SEARCH_WINDOW);
    let mut eocd = None;
    for i in (start..zip.len().saturating_sub(3)).rev() {
        if zip[i..i + 4] == [0x50, 0x4b, 0x05, 0x06] {
            eocd = Some(i);
            break;
        }
    }
    let eocd = eocd.ok_or_else(|| malformed("no end-of-central-directory record"))?;
    let disk = u16le(zip, eocd + 4)?;
    let cd_disk = u16le(zip, eocd + 6)?;
    if disk != 0 || cd_disk != 0 {
        return Err(JarError::Unsupported {
            reason: "multi-disk archive".to_owned(),
        });
    }
    let total_entries = u16le(zip, eocd + 10)? as usize;
    let cd_offset = u32le(zip, eocd + 16)? as usize;
    if total_entries == 0xFFFF || u32le(zip, eocd + 12)? == 0xFFFF_FFFF || cd_offset == 0xFFFF_FFFF
    {
        return Err(JarError::Unsupported {
            reason: "ZIP64 archive".to_owned(),
        });
    }
    if total_entries > MAX_ARCHIVE_ENTRIES {
        return Err(JarError::BudgetExceeded {
            reason: format!("{total_entries} entries (limit {MAX_ARCHIVE_ENTRIES})"),
        });
    }
    report.entries_seen = total_entries;

    let mut out = Vec::new();
    let mut total_unpacked = 0u64;
    let mut off = cd_offset;
    for _ in 0..total_entries {
        if u32le(zip, off)? != 0x0201_4b50 {
            return Err(malformed("bad central-directory signature"));
        }
        let flags = u16le(zip, off + 8)?;
        let method = u16le(zip, off + 10)?;
        let csize = u32le(zip, off + 20)? as usize;
        let usize_ = u32le(zip, off + 24)? as u64;
        let name_len = u16le(zip, off + 28)? as usize;
        let extra_len = u16le(zip, off + 30)? as usize;
        let comment_len = u16le(zip, off + 32)? as usize;
        let lh_off = u32le(zip, off + 42)? as usize;
        if flags & 0x1 != 0 {
            return Err(JarError::Unsupported {
                reason: "encrypted entry".to_owned(),
            });
        }
        if csize == 0xFFFF_FFFF || usize_ == u64::from(u32::MAX) {
            return Err(JarError::Unsupported {
                reason: "ZIP64 entry".to_owned(),
            });
        }
        let name_bytes = zip
            .get(off + 46..off + 46 + name_len)
            .ok_or_else(|| malformed("truncated entry name"))?;
        let name = std::str::from_utf8(name_bytes)
            .map_err(|_| malformed("non-UTF-8 entry name"))?
            .to_owned();
        off += 46 + name_len + extra_len + comment_len;

        if name.ends_with('/') || !select(&name) {
            continue;
        }
        // Validate *before* touching the name further; a traversal entry is
        // a record in `skipped`, not a load abort — the rest of the pack is
        // still honest. (`validate_entry_name` only ever reports
        // `UnsafePath`.)
        if validate_entry_name(&name).is_err() {
            report
                .skipped
                .push((name, "unsafe archive path".to_owned()));
            continue;
        }
        if usize_ > MAX_ENTRY_BYTES {
            report.skipped.push((
                name,
                format!("entry exceeds {MAX_ENTRY_BYTES}-byte cap ({usize_})"),
            ));
            continue;
        }
        if usize_ > MAX_UNPACKED_BYTES.saturating_sub(total_unpacked) {
            return Err(JarError::BudgetExceeded {
                reason: format!("unpacked bytes exceed {MAX_UNPACKED_BYTES}"),
            });
        }
        // Local header: re-read name/extra lengths to find the data.
        if u32le(zip, lh_off)? != 0x0403_4b50 {
            return Err(malformed("bad local-file-header signature"));
        }
        let l_name_len = u16le(zip, lh_off + 26)? as usize;
        let l_extra_len = u16le(zip, lh_off + 28)? as usize;
        let data_start = lh_off
            .checked_add(30)
            .and_then(|v| v.checked_add(l_name_len))
            .and_then(|v| v.checked_add(l_extra_len))
            .ok_or_else(|| malformed("header offset overflow"))?;
        let data = zip
            .get(data_start..data_start + csize)
            .ok_or_else(|| malformed("truncated entry data"))?;
        let bytes = match method {
            0 => {
                if data.len() as u64 != usize_ {
                    return Err(malformed("stored entry size mismatch"));
                }
                data.to_vec()
            }
            8 => match inflate(data, usize_ as usize) {
                Ok(bytes) => bytes,
                Err(reason) => {
                    report.skipped.push((name, reason));
                    continue;
                }
            },
            m => {
                return Err(JarError::Unsupported {
                    reason: format!("compression method {m}"),
                })
            }
        };
        total_unpacked += bytes.len() as u64;
        out.push(JarEntry { name, bytes });
        report.entries_loaded += 1;
    }
    Ok(out)
}

/// `assets/<ns>/textures/<sub>` -> (`ns`, `sub`) when `sub` sits inside one
/// of `dirs`. Shared by the JAR and pack-zip entry filters.
fn texture_subpath(name: &str, dirs: &[&str]) -> Option<(String, String)> {
    let rest = name.strip_prefix("assets/")?;
    let (ns, tail) = rest.split_once('/')?;
    let tail = tail.strip_prefix("textures/")?;
    let (dir, sub) = tail.split_once('/')?;
    if dirs.contains(&dir) && !sub.is_empty() {
        Some((ns.to_owned(), sub.to_owned()))
    } else {
        None
    }
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
/// `textures/**/*.png|*.rgba` decode into `sprites` (an undecodable `.png`
/// becomes the `(0,0,bytes)` marker `CompiledAssetBundle::build` re-checks
/// into `skipped_errors`). `.mcmeta` animation files are counted in
/// `skipped` — zip layers keep their sprites but not animation timing.
fn layer_from_entries(
    name: String,
    entries: &[JarEntry],
    report: &mut JarReadReport,
) -> ResourcePackLayer {
    let mut layer = ResourcePackLayer {
        name,
        ..ResourcePackLayer::default()
    };
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
                    match png_decode::decode_png_rgba(&entry.bytes) {
                        Ok((width, height, rgba)) => {
                            layer.sprites.insert(key, (width, height, rgba));
                        }
                        Err(_) => {
                            layer.sprites.insert(key, (0, 0, entry.bytes.clone()));
                        }
                    }
                } else if let Some(stem) = sub.strip_suffix(".rgba") {
                    match decode_rgba_sprite(&entry.bytes) {
                        Ok((width, height, rgba)) => {
                            layer
                                .sprites
                                .insert(format!("{ns}:{stem}"), (width, height, rgba));
                        }
                        Err(reason) => report.skipped.push((entry.name.clone(), reason)),
                    }
                } else if sub.ends_with(".mcmeta") {
                    report.skipped.push((
                        entry.name.clone(),
                        "zip-layer .mcmeta animations are not applied".to_owned(),
                    ));
                }
            }
            _ => {}
        }
    }
    layer
}

/// Read the vanilla client JAR at `jar_path` and build the
/// [`ResourcePackLayer`] of `textures/block/**.png` + `textures/item/**.png`
/// sprites it contains (any `assets/<ns>` namespace, `minecraft:` in
/// practice). Non-texture assets — sounds, lang, shaders — are ignored at
/// the filter stage and never decompressed.
pub fn vanilla_jar_texture_layer(
    jar_path: &Path,
) -> Result<(ResourcePackLayer, JarReadReport), JarError> {
    let (entries, mut report) = read_selected_entries(jar_path, |name| {
        texture_subpath(name, &["block", "item"])
            .is_some_and(|(_, sub)| sub.ends_with(".png") || sub.ends_with(".png.mcmeta"))
    })?;
    let name = jar_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("client.jar")
        .to_owned();
    Ok((layer_from_entries(name, &entries, &mut report), report))
}

/// Read a resource-pack `.zip` the way `<gameDir>/resourcepacks/*.zip`
/// stores one: `assets/<ns>/{blockstates,models,textures}` into a
/// [`ResourcePackLayer`]. A `foo/` top-level wrapper directory is unwrapped
/// the same way the shaderpack loader tolerates packed roots.
pub fn zip_pack_layer(zip_path: &Path) -> Result<(ResourcePackLayer, JarReadReport), JarError> {
    let (mut entries, mut report) =
        read_selected_entries(zip_path, |name| asset_subpath(name).is_some())?;
    // Single top-level directory wrapper: `mypack/assets/...` ->
    // `assets/...` (real archives often nest the pack root one level).
    if !entries.iter().any(|e| e.name.starts_with("assets/")) {
        for entry in &mut entries {
            if let Some((_, rest)) = entry.name.split_once('/') {
                if rest.starts_with("assets/") {
                    entry.name = rest.to_owned();
                }
            }
        }
    }
    let name = zip_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("resource-pack.zip")
        .to_owned();
    Ok((layer_from_entries(name, &entries, &mut report), report))
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
    let options = game_dir.join("options.txt");
    let Ok(text) = std::fs::read_to_string(&options) else {
        return Vec::new();
    };
    let mut packs = Vec::new();
    for line in text.lines() {
        let Some(value) = line.trim().strip_prefix("resourcePacks:") else {
            continue;
        };
        // options.txt stores a JSON array: ["vanilla","file/pack.zip"].
        for token in value
            .trim()
            .trim_start_matches('[')
            .trim_end_matches(']')
            .split(',')
        {
            let token = token.trim().trim_matches('"');
            if let Some(file) = token.strip_prefix("file/") {
                if !file.is_empty() {
                    packs.push(file.to_owned());
                }
            }
        }
    }
    packs
}

// ---------------------------------------------------------------------------
// Clean-room RFC 1951 inflate (shared shape with client-port's shaderpack
// reader; private there, so this lane keeps its own copy)
// ---------------------------------------------------------------------------

/// Inflate `input` (raw DEFLATE) into at most `limit` bytes. Output is
/// capped at `limit` so a hostile stream cannot expand unboundedly.
fn inflate(input: &[u8], limit: usize) -> Result<Vec<u8>, String> {
    let mut br = BitReader::new(input);
    let mut out: Vec<u8> = Vec::new();
    loop {
        let bfinal = br.bits(1)?;
        let btype = br.bits(2)?;
        match btype {
            0 => {
                br.align_byte();
                let len = br.aligned_u16()?;
                let nlen = br.aligned_u16()?;
                if len != !nlen {
                    return Err("deflate stored-block LEN/NLEN mismatch".to_owned());
                }
                for _ in 0..len {
                    let b = br.aligned_byte()?;
                    if out.len() >= limit {
                        return Err("deflate output exceeds declared size".to_owned());
                    }
                    out.push(b);
                }
            }
            1 | 2 => {
                let (lit, dist) = if btype == 1 {
                    (fixed_litlen_huffman(), fixed_dist_huffman())
                } else {
                    read_dynamic_tables(&mut br)?
                };
                loop {
                    let sym = lit.decode(&mut br)?;
                    match sym {
                        0..=255 => {
                            if out.len() >= limit {
                                return Err("deflate output exceeds declared size".to_owned());
                            }
                            out.push(sym as u8);
                        }
                        256 => break,
                        257..=285 => {
                            let idx = (sym - 257) as usize;
                            let length =
                                LENGTH_BASE[idx] as usize + br.bits(LENGTH_EXTRA[idx])? as usize;
                            let dsym = dist.decode(&mut br)? as usize;
                            if dsym >= DIST_BASE.len() {
                                return Err("invalid distance symbol".to_owned());
                            }
                            let distance =
                                DIST_BASE[dsym] as usize + br.bits(DIST_EXTRA[dsym])? as usize;
                            if distance == 0 || distance > out.len() {
                                return Err("back-reference before output start".to_owned());
                            }
                            let start = out.len() - distance;
                            for i in 0..length {
                                if out.len() >= limit {
                                    return Err("deflate output exceeds declared size".to_owned());
                                }
                                let b = out[start + i];
                                out.push(b);
                            }
                        }
                        _ => return Err("invalid literal/length symbol".to_owned()),
                    }
                }
            }
            _ => return Err("reserved deflate block type".to_owned()),
        }
        if bfinal == 1 {
            break;
        }
    }
    Ok(out)
}

const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LENGTH_EXTRA: [u32; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u32; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];
const CLEN_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

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

    fn bits(&mut self, n: u32) -> Result<u32, String> {
        debug_assert!(n <= 32);
        while self.len < n {
            let b = *self
                .data
                .get(self.pos)
                .ok_or_else(|| "deflate stream truncated".to_owned())?;
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

    fn aligned_byte(&mut self) -> Result<u8, String> {
        if self.len >= 8 {
            let b = self.buf as u8;
            self.buf >>= 8;
            self.len -= 8;
            Ok(b)
        } else {
            self.bits(8).map(|v| v as u8)
        }
    }

    fn aligned_u16(&mut self) -> Result<u16, String> {
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
    fn build(lengths: &[u8]) -> Result<Self, String> {
        let mut counts = [0u16; 16];
        for &l in lengths {
            if l > 15 {
                return Err("invalid huffman table".to_owned());
            }
            counts[l as usize] += 1;
        }
        if counts[0] as usize == lengths.len() {
            return Err("invalid huffman table".to_owned());
        }
        // Check for an over-subscribed code.
        let mut left: i32 = 1;
        for &c in counts.iter().skip(1) {
            left <<= 1;
            left -= i32::from(c);
            if left < 0 {
                return Err("invalid huffman table".to_owned());
            }
        }
        let mut offs = [0usize; 16];
        for i in 1..15 {
            offs[i + 1] = offs[i] + counts[i] as usize;
        }
        let mut symbols = vec![0u16; lengths.len()];
        for (sym, &l) in lengths.iter().enumerate() {
            if l != 0 {
                symbols[offs[l as usize]] = sym as u16;
                offs[l as usize] += 1;
            }
        }
        Ok(Self { counts, symbols })
    }

    fn decode(&self, br: &mut BitReader<'_>) -> Result<u16, String> {
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
                    .ok_or_else(|| "invalid huffman code".to_owned());
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        Err("invalid huffman code".to_owned())
    }
}

fn fixed_litlen_huffman() -> Huffman {
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
    match Huffman::build(&lengths) {
        Ok(h) => h,
        Err(_) => unreachable!("fixed deflate table is well-formed"),
    }
}

fn fixed_dist_huffman() -> Huffman {
    match Huffman::build(&[5u8; 30]) {
        Ok(h) => h,
        Err(_) => unreachable!("fixed deflate table is well-formed"),
    }
}

/// Read a dynamic-Huffman block header and build the literal/length and
/// distance tables.
fn read_dynamic_tables(br: &mut BitReader<'_>) -> Result<(Huffman, Huffman), String> {
    let hlit = br.bits(5)? as usize + 257;
    let hdist = br.bits(5)? as usize + 1;
    let hclen = br.bits(4)? as usize + 4;
    if hlit > 286 || hdist > 30 {
        return Err("dynamic table size out of range".to_owned());
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
                    return Err("repeat with no previous length".to_owned());
                }
                let prev = lengths[i - 1];
                let rep = 3 + br.bits(2)? as usize;
                for _ in 0..rep {
                    if i >= lengths.len() {
                        return Err("repeat overflows table".to_owned());
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
                    return Err("zero-run overflows table".to_owned());
                }
            }
            _ => return Err("invalid code-length symbol".to_owned()),
        }
    }
    let lit = Huffman::build(&lengths[..hlit])?;
    let dist = Huffman::build(&lengths[hlit..])?;
    Ok((lit, dist))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal stored-ZIP writer for unit tests (no compression needed to
    /// prove the reader's structure handling).
    fn stored_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, data) in entries {
            let lh_off = out.len() as u32;
            out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
            out.extend_from_slice(&20u16.to_le_bytes()); // version
            out.extend_from_slice(&0u16.to_le_bytes()); // flags
            out.extend_from_slice(&0u16.to_le_bytes()); // method stored
            out.extend_from_slice(&0u16.to_le_bytes()); // time
            out.extend_from_slice(&0u16.to_le_bytes()); // date
            out.extend_from_slice(&0u32.to_le_bytes()); // crc
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes()); // extra
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(data);

            central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u32.to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
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
        let dir = std::env::temp_dir().join(format!("mc-spine-ziptest-{}", std::process::id()));
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
        let dir = std::env::temp_dir().join(format!("mc-spine-ziptest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let jar = dir.join("evil.jar");
        std::fs::write(&jar, &zip).unwrap();
        let (entries, report) = read_selected_entries(&jar, |name| name.ends_with(".png")).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(report.skipped.len(), 1);
        std::fs::remove_file(&jar).ok();
    }
}
