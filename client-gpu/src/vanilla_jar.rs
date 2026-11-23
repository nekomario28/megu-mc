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
use std::path::{Path, PathBuf};

use megu_client_mesh::asset_pipeline::{animation_from_mcmeta, ResourcePackLayer};
use megu_client_mesh::png_decode;
use megu_core::zip_archive::{LazyZipIndex, ZipError};

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
    let size = std::fs::metadata(path).map_err(|e| JarError::Io {
        reason: format!("cannot stat {}: {e}", path.display()),
    })?.len();
    if size > MAX_ARCHIVE_BYTES {
        return Err(JarError::BudgetExceeded {
            reason: format!("archive size {size} exceeds {MAX_ARCHIVE_BYTES}"),
        });
    }
    let index = LazyZipIndex::open_all(path).map_err(|e| zip_to_jar(path, e))?;
    if index.entries().len() > MAX_ARCHIVE_ENTRIES {
        return Err(JarError::BudgetExceeded {
            reason: format!("archive has {} entries (cap {MAX_ARCHIVE_ENTRIES})", index.entries().len()),
        });
    }
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
        "models" => (sub.starts_with("block/") || sub.starts_with("item/")) && sub.ends_with(".json"),
        "textures" => (sub.starts_with("block/") || sub.starts_with("item/"))
            && (sub.ends_with(".png") || sub.ends_with(".png.mcmeta")),
        "lang" => sub.ends_with(".json"),
        _ => false,
    }
}

/// Load regular `mods/*.jar` entries by filename, isolating corrupt JARs.
/// Later JARs override earlier JARs, and user packs override all mod layers.
pub fn load_mod_jar_layers(mods_dir: &Path) -> (Vec<ResourcePackLayer>, ModJarLoadReport) {
    let mut layers = Vec::new();
    let mut report = ModJarLoadReport::default();
    let dir = match std::fs::read_dir(mods_dir) {
        Ok(dir) => dir,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return (layers, report),
        Err(err) => {
            report.rejected.push((mods_dir.to_path_buf(), JarError::Io {
                reason: err.to_string(),
            }));
            return (layers, report);
        }
    };
    let mut jars = Vec::new();
    for entry in dir {
        match entry {
            Ok(entry) if entry.path().extension().is_some_and(|e| e == "jar") => {
                match entry.file_type() {
                    Ok(ty) if ty.is_file() => jars.push(entry.path()),
                    Ok(_) => report.rejected.push((entry.path(), JarError::Unsupported {
                        reason: "mod JAR must be a regular file".to_owned(),
                    })),
                    Err(err) => report.rejected.push((entry.path(), JarError::Io {
                        reason: err.to_string(),
                    })),
                }
            }
            Ok(_) => {}
            Err(err) => report.rejected.push((mods_dir.to_path_buf(), JarError::Io {
                reason: err.to_string(),
            })),
        }
    }
    jars.sort();
    let mut total_assets = 0u64;
    for (index, jar) in jars.into_iter().enumerate() {
        if index >= MAX_MOD_JARS {
            report.rejected.push((jar, JarError::BudgetExceeded {
                reason: format!("more than {MAX_MOD_JARS} mod JARs"),
            }));
            continue;
        }
        let result = read_selected_entries(&jar, is_mod_asset).and_then(|(entries, mut read_report)| {
            report.entries_seen += read_report.entries_seen;
            if let Some((name, reason)) = read_report.skipped.first() {
                return Err(JarError::Malformed {
                    reason: format!("{name}: {reason}"),
                });
            }
            let unpacked = entries.iter().map(|entry| entry.bytes.len() as u64).sum::<u64>();
            if unpacked > MAX_MOD_ASSET_BYTES.saturating_sub(total_assets) {
                return Err(JarError::BudgetExceeded {
                    reason: format!("aggregate mod assets exceed {MAX_MOD_ASSET_BYTES} bytes"),
                });
            }
            let layer = layer_from_entries(jar.file_name().unwrap_or_default().to_string_lossy().into_owned(), &entries, &mut read_report);
            if let Some((name, reason)) = read_report.skipped.first() {
                return Err(JarError::Malformed {
                    reason: format!("{name}: {reason}"),
                });
            }
            let decoded = layer.sprites.values().map(|(_, _, rgba)| rgba.len() as u64).sum::<u64>();
            let charge = unpacked.saturating_add(decoded);
            if charge > MAX_MOD_ASSET_BYTES.saturating_sub(total_assets) {
                return Err(JarError::BudgetExceeded {
                    reason: format!("aggregate decoded mod assets exceed {MAX_MOD_ASSET_BYTES} bytes"),
                });
            }
            total_assets += charge;
            Ok(layer)
        });
        match result {
            Ok(layer) => {
                report.loaded.push(jar);
                layers.push(layer);
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
        // A (0, 0, _) sprite is an undecodable-PNG marker already counted
        // in `skipped`; there is no strip to animate.
        if width == 0 && height == 0 {
            continue;
        }
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
            ("assets/create/blockstates/andesite_casing.json", br#"{"variants":{"":{"model":"create:block/andesite_casing"}}}"#),
            ("assets/create/models/block/andesite_casing.json", model),
            ("assets/create/textures/block/andesite_casing.png", &CREATE_PNG),
            ("assets/create/lang/en_us.json", br#"{"block.create.andesite_casing":"Andesite Casing"}"#),
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
        std::fs::write(&vanilla_path, stored_zip(&[
            ("assets/minecraft/textures/block/stone.png", &CREATE_PNG),
        ])).unwrap();
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
        override_layer.sprites.insert("create:block/andesite_casing".into(),
            (1, 1, vec![10, 20, 30, 255]));
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
        std::fs::write(mods.join("b-traversal.jar"), stored_zip(&[
            ("assets/create/models/../block/bad.json", b"{}"),
        ])).unwrap();
        let mut huge = stored_zip(&[
            ("assets/create/textures/block/bad.png", b"x"),
        ]);
        let cd_offset = huge.windows(4).position(|window| window == b"PK\x01\x02").unwrap();
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
        vanilla.sprites.insert("minecraft:block/stone".into(),
            (1, 1, vec![100, 120, 140, 255]));
        let bundle = CompiledAssetBundle::build(&[vanilla, layers[0].clone()]).unwrap();
        assert!(bundle.atlas.placement("minecraft:block/stone").is_some());
        assert!(bundle.atlas.placement("create:block/andesite_casing").is_some());
        std::fs::remove_dir_all(root).unwrap();
    }

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
}
