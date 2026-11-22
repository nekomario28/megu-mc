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
use mc_spine_core::zip_archive::{LazyZipIndex, ZipError};

/// Total on-disk bytes the archive itself may occupy.
pub const MAX_ARCHIVE_BYTES: u64 = 256 * 1024 * 1024;
/// Central-directory entry count bound.
pub const MAX_ARCHIVE_ENTRIES: usize = 50_000;
/// Sum of uncompressed bytes the reader will materialize.
pub const MAX_UNPACKED_BYTES: u64 = 256 * 1024 * 1024;
/// Per-entry uncompressed cap (matches the asset pipeline's pack-file cap).
pub const MAX_ENTRY_BYTES: u64 = 8 * 1024 * 1024;

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

/// Read `path` and decompress the central-directory entries whose names
/// pass `select`. The archive itself is read once, size-checked before and
/// after the read, and only matching entries are inflated — a hostile
/// archive can never make the client decompress unrelated payloads.
/// Read `path` and decompress the central-directory entries whose names
/// pass `select`. Backed by [`LazyZipIndex`]: only the EOCD tail window
/// and the Central Directory are indexed, then each selected entry is
/// fetched and inflated on demand — the whole archive is never loaded
/// and zero temporary files are created. Entries are size-checked
/// before and after inflation, so a hostile archive can never make the
/// client decompress unrelated payloads.
pub fn read_selected_entries(
    path: &Path,
    select: impl Fn(&str) -> bool,
) -> Result<(Vec<JarEntry>, JarReadReport), JarError> {
    let index = LazyZipIndex::open_all(path).map_err(|e| zip_to_jar(path, e))?;
    let mut report = JarReadReport {
        entries_seen: index.entries().len(),
        ..JarReadReport::default()
    };
    let mut out = Vec::new();
    let mut total_unpacked = 0u64;
    for meta in index.entries() {
        let name = &meta.name;
        if name.ends_with('/') || !select(name) {
            continue;
        }
        // A traversal entry is a record in `skipped`, not a load abort —
        // the rest of the pack is still honest.
        if validate_entry_name(name).is_err() {
            report
                .skipped
                .push((name.clone(), "unsafe archive path".to_owned()));
            continue;
        }
        if meta.uncompressed_size > MAX_ENTRY_BYTES {
            report.skipped.push((
                name.clone(),
                format!(
                    "entry exceeds {MAX_ENTRY_BYTES}-byte cap ({})",
                    meta.uncompressed_size
                ),
            ));
            continue;
        }
        if meta.uncompressed_size > MAX_UNPACKED_BYTES.saturating_sub(total_unpacked) {
            return Err(JarError::BudgetExceeded {
                reason: format!("unpacked bytes exceed {MAX_UNPACKED_BYTES}"),
            });
        }
        match index.read_entry(name, MAX_ENTRY_BYTES as usize) {
            Ok(bytes) => {
                total_unpacked += bytes.len() as u64;
                out.push(JarEntry {
                    name: name.clone(),
                    bytes,
                });
                report.entries_loaded += 1;
            }
            Err(e) => report.skipped.push((name.clone(), e.to_string())),
        }
    }
    Ok((out, report))
}

fn zip_to_jar(path: &Path, e: ZipError) -> JarError {
    match e {
        ZipError::Io(io) => JarError::Io {
            reason: format!("cannot read {}: {io}", path.display()),
        },
        ZipError::MultiDisk => JarError::Unsupported {
            reason: "multi-disk archive".to_owned(),
        },
        ZipError::Encrypted(name) => JarError::Unsupported {
            reason: format!("encrypted entry {name}"),
        },
        ZipError::UnsupportedMethod(m) => JarError::Unsupported {
            reason: format!("compression method {m}"),
        },
        ZipError::InvalidEntryName(name) => JarError::UnsafePath { path: name },
        ZipError::EntryTooLarge {
            uncompressed_size, ..
        } => JarError::BudgetExceeded {
            reason: format!("entry is {uncompressed_size} bytes"),
        },
        ZipError::OutputCapExceeded => JarError::BudgetExceeded {
            reason: "inflated entry exceeds output cap".to_owned(),
        },
        other => JarError::Malformed {
            reason: other.to_string(),
        },
    }
}

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
