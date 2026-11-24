// SPDX-License-Identifier: LGPL-3.0-or-later

//! Unified client asset pipeline (Kernel 1): ordered resource-pack layers
//! in, one compiled [`CompiledAssetBundle`] out — ready for `client-gpu`.
//!
//! [`ResourcePackLayer`] is one resource pack's worth of *raw* asset
//! payloads: `blockstates/*.json` and `models/**/*.json` documents kept as
//! text (parsed at [`CompiledAssetBundle::build`] time so a single corrupt
//! file fails closed into [`CompiledAssetBundle::skipped_errors`] instead of
//! killing the pack), decoded RGBA8 sprites, and resolved
//! [`SpriteAnimation`] tables. [`ResourcePackLayer::from_dir`] loads a
//! vanilla-shaped `assets/<namespace>/...` directory tree with pure `std`:
//! every path is canonicalized and must stay inside the pack root (symlink
//! escapes are rejected), and every file is capped at
//! [`MAX_ASSET_FILE_BYTES`].
//!
//! On-disk sprite formats are [`RGBA8_SPRITE_FORMAT`] (`.rgba`: an 8-byte
//! little-endian `width`/`height` header followed by `width * height * 4`
//! row-major RGBA8 texels) and real `.png` files, decoded by
//! [`crate::png_decode::decode_png_rgba`] — a bounded clean-room decoder
//! that fails closed on corrupt or oversized images.
//!
//! [`CompiledAssetBundle::build`] merges the layer stack lowest-to-highest
//! priority: blockstates and sprites are highest-priority-wins per id,
//! models form a [`ResourcePackModelStack`] so pack overlay + `parent`
//! chains resolve exactly like `model_baker` intends, and every sprite is
//! packed by [`AtlasBuilder::for_blocks`] with [`ATLAS_PADDING_TEXELS`]
//! border. Sprites that are not fully opaque get [`PaddingMode::ClampEdge`]
//! gutters (cutout/translucent edges replicate instead of wrapping); fully
//! opaque sprites keep [`PaddingMode::WrapTile`] so tiled block textures
//! sample seamlessly. The atlas then gets a full gamma-correct mip chain via
//! [`build_mipmaps_linear`].
//!
//! [`CompiledAssetBundle::bake_block_with_uvs`] resolves a block state
//! through `blockstate::bake_blockstate` (variants, multipart, `x`/`y`,
//! `uvlock`, `weight`) and remaps every quad's vanilla `0..16` UVs into the
//! packed atlas, producing [`AtlasBakedQuad`]s that carry ready-to-upload
//! atlas UVs plus the sprite's [`AtlasTileRect`] for tile-safe sampling.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use crate::atlas::{
    build_mipmaps_linear, remap_baked_quad_uvs, AnimFrame, AtlasBuilder, AtlasError, AtlasMip,
    AtlasTileRect, PaddingMode, PackedAtlas, SpriteAnimation,
};
use crate::model_baker::blockstate::{self, BlockStateDef};
use crate::model_baker::{
    bake_model_with_uvlock, BakedModel, BakedModelQuad, ModelError, RawModelJson,
    ResourcePackModelStack,
};
use crate::section::Direction;

// ---------------------------------------------------------------------------
// Limits + on-disk format
// ---------------------------------------------------------------------------

/// Per-file cap for [`ResourcePackLayer::from_dir`]: 8 MiB. Applies to every
/// file the loader reads (JSON documents, `.rgba`/`.png` sprites, `.mcmeta`
/// animations) before and after the read so a swapped-in oversized file
/// still fails closed. `.png` files are capped on their *compressed* size;
/// [`crate::png_decode`] separately bounds the decoded output.
pub const MAX_ASSET_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// Border reserved around every packed sprite, in texels. Two texels keeps
/// mip level 1+ samples off neighbouring sprites for the common 16x16 tile
/// case while staying cheap.
pub const ATLAS_PADDING_TEXELS: u32 = 2;

/// `.rgba` sprite file layout: bytes `0..4` are the little-endian `u32`
/// width, bytes `4..8` the little-endian `u32` height, and the remaining
/// `width * height * 4` bytes are row-major RGBA8 texels. The payload length
/// must match the header exactly — trailing or truncated data is rejected.
pub const RGBA8_SPRITE_FORMAT: &str =
    "u32le width, u32le height, then width*height*4 row-major RGBA8 texels";

/// Hard cap on `.mcmeta` JSON nesting, shared shape with the other
/// clean-room parsers in this tree.
const MAX_META_JSON_DEPTH: usize = 64;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Every way the asset pipeline can fail. All variants are terminal for the
/// operation they belong to: [`ResourcePackLayer::from_dir`] fails the whole
/// load, [`CompiledAssetBundle::build`] only fails on atlas-level problems
/// (per-entry problems land in `skipped_errors`), and the bake calls fail
/// the single block/model they were asked for.
#[derive(Debug)]
pub enum AssetPipelineError {
    /// Filesystem failure while loading a pack directory.
    Io {
        /// Path being accessed when the error occurred.
        path: String,
        /// Underlying I/O error.
        source: io::Error,
    },
    /// [`ResourcePackLayer::from_dir`] target exists but is not a directory.
    NotADirectory {
        /// Offending path.
        path: String,
    },
    /// A canonicalized path escaped its allowed root — a symlink traversal
    /// or a non-canonical intermediate component.
    UnsafePath {
        /// Offending path as supplied by the filesystem walk.
        path: String,
    },
    /// A pack file exceeded [`MAX_ASSET_FILE_BYTES`].
    FileTooLarge {
        /// Pack-relative path of the offending file.
        path: String,
        /// Observed size in bytes.
        bytes: u64,
    },
    /// A `.rgba` sprite or `.mcmeta`/JSON pack file is malformed: bad
    /// header, wrong payload length, invalid UTF-8, or an animation
    /// table the [`SpriteAnimation`] constructors reject. (Undecodable
    /// `.png` textures do not fail the load — they are recorded in
    /// [`CompiledAssetBundle::skipped_errors`] at build time.)
    InvalidPackFile {
        /// Pack-relative path of the offending file.
        path: String,
        /// Exact reason the file was rejected.
        reason: String,
    },
    /// A model or blockstate bake failed while resolving quads.
    Model(ModelError),
    /// Atlas packing, mipmapping or quad UV remapping failed.
    Atlas(AtlasError),
    /// [`CompiledAssetBundle::bake_block_with_uvs`] named a block with no
    /// merged blockstate definition.
    MissingBlockState(String),
}

impl fmt::Display for AssetPipelineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "I/O error at {path}: {source}"),
            Self::NotADirectory { path } => write!(f, "{path} is not a directory"),
            Self::UnsafePath { path } => {
                write!(f, "path {path} escapes the resource pack root")
            }
            Self::FileTooLarge { path, bytes } => {
                write!(f, "pack file {path} is {bytes} bytes (limit {MAX_ASSET_FILE_BYTES})")
            }
            Self::InvalidPackFile { path, reason } => {
                write!(f, "invalid pack file {path}: {reason}")
            }
            Self::Model(err) => write!(f, "model bake failed: {err}"),
            Self::Atlas(err) => write!(f, "atlas pipeline failed: {err}"),
            Self::MissingBlockState(id) => {
                write!(f, "no blockstate definition for block {id:?}")
            }
        }
    }
}

impl Error for AssetPipelineError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Model(err) => Some(err),
            Self::Atlas(err) => Some(err),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Resource pack layer
// ---------------------------------------------------------------------------

/// One resource pack's raw asset payloads. `blockstates`/`models` keep the
/// JSON documents as text — [`CompiledAssetBundle::build`] parses them so a
/// corrupt file is recorded per entry rather than aborting the load.
/// `sprites` maps a normalized `namespace:path` id to `(width, height,
/// RGBA8)`; `animations` maps the same id space to resolved
/// [`SpriteAnimation`] tables. Layers are ordered lowest-to-highest
/// priority: for every id the last layer defining it wins.
///
/// A `.png` texture that fails to decode is kept as a `(0, 0, bytes)`
/// marker holding the raw file bytes — zero-size sprites are never
/// valid — so [`CompiledAssetBundle::build`] can record the decode
/// failure in `skipped_errors` instead of the load aborting.
#[derive(Clone, Debug, Default)]
pub struct ResourcePackLayer {
    /// Human-readable pack name (the directory name for [`from_dir`]).
    pub name: String,
    /// `namespace:block` -> raw `blockstates/*.json` text.
    pub blockstates: BTreeMap<String, String>,
    /// `namespace:path` -> raw `models/**/*.json` text.
    pub models: BTreeMap<String, String>,
    /// `namespace:path` -> `(width, height, RGBA8 texels)`.
    pub sprites: BTreeMap<String, (u32, u32, Vec<u8>)>,
    /// `namespace:path` -> resolved `.mcmeta` animation table.
    pub animations: BTreeMap<String, SpriteAnimation>,
}

impl ResourcePackLayer {
    /// Loads a vanilla-shaped pack directory:
    ///
    /// * `assets/<ns>/blockstates/**/*.json` -> [`blockstates`](Self::blockstates)
    /// * `assets/<ns>/models/**/*.json` -> [`models`](Self::models)
    /// * `assets/<ns>/textures/**/*.rgba` -> [`sprites`](Self::sprites)
    /// * `assets/<ns>/textures/**/*.png` -> [`sprites`](Self::sprites)
    /// * `assets/<ns>/textures/**/*.rgba.mcmeta` or `*.png.mcmeta` ->
    ///   [`animations`](Self::animations)
    ///
    /// `.rgba` sprites use [`RGBA8_SPRITE_FORMAT`]; `.png` sprites are
    /// decoded by [`crate::png_decode::decode_png_rgba`] straight into the
    /// same RGBA8 shape — an undecodable `.png` is kept as a `(0, 0, bytes)`
    /// marker and reported through
    /// [`CompiledAssetBundle::skipped_errors`] at build time rather than
    /// failing the load. Unrelated directories and files are ignored. An
    /// `.mcmeta` must sit next to its `.rgba`/`.png` sibling — the sprite's
    /// dimensions define the animation's frame geometry.
    ///
    /// Fail-closed: any I/O error, symlink escaping the pack root, file over
    /// [`MAX_ASSET_FILE_BYTES`], non-UTF-8 JSON, malformed `.rgba` or
    /// `.mcmeta` rejects the whole load with an [`AssetPipelineError`]. A
    /// directory without `assets/` yields an empty layer.
    pub fn from_dir(path: &Path) -> Result<Self, AssetPipelineError> {
        let root = fs::canonicalize(path).map_err(|source| AssetPipelineError::Io {
            path: path.display().to_string(),
            source,
        })?;
        if !root.is_dir() {
            return Err(AssetPipelineError::NotADirectory {
                path: root.display().to_string(),
            });
        }
        let name = root
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|n| !n.is_empty())
            .unwrap_or("resource-pack")
            .to_owned();
        let mut layer = ResourcePackLayer {
            name,
            ..ResourcePackLayer::default()
        };
        let assets = root.join("assets");
        match fs::symlink_metadata(&assets) {
            Ok(_) => {}
            Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(layer),
            Err(source) => {
                return Err(AssetPipelineError::Io {
                    path: assets.display().to_string(),
                    source,
                })
            }
        }
        let assets = fs::canonicalize(&assets).map_err(|source| AssetPipelineError::Io {
            path: assets.display().to_string(),
            source,
        })?;
        if !assets.is_dir() || !assets.starts_with(&root) {
            return Err(AssetPipelineError::UnsafePath {
                path: assets.display().to_string(),
            });
        }
        // (sprite id, display path, mcmeta text) resolved after the walk so
        // sprite dimensions are known regardless of entry order.
        let mut pending_meta: Vec<(String, String, String)> = Vec::new();
        for ns_entry in sorted_entries(&assets)? {
            let ns_raw = ns_entry.path();
            let ns_dir = fs::canonicalize(&ns_raw).map_err(|source| AssetPipelineError::Io {
                path: ns_raw.display().to_string(),
                source,
            })?;
            if !ns_dir.starts_with(&assets) {
                return Err(AssetPipelineError::UnsafePath {
                    path: ns_raw.display().to_string(),
                });
            }
            if !ns_dir.is_dir() {
                continue;
            }
            let ns_name = ns_entry.file_name();
            let ns = ns_name
                .to_str()
                .ok_or_else(|| AssetPipelineError::InvalidPackFile {
                    path: ns_raw.display().to_string(),
                    reason: "namespace is not valid UTF-8".to_owned(),
                })?;
            if !valid_namespace(ns) {
                return Err(AssetPipelineError::InvalidPackFile {
                    path: ns_raw.display().to_string(),
                    reason: format!("invalid namespace {ns:?}"),
                });
            }
            for category in ["blockstates", "models", "textures"] {
                let candidate = ns_dir.join(category);
                let base = match fs::canonicalize(&candidate) {
                    Ok(base) => base,
                    Err(source) if source.kind() == io::ErrorKind::NotFound => continue,
                    Err(source) => {
                        return Err(AssetPipelineError::Io {
                            path: candidate.display().to_string(),
                            source,
                        })
                    }
                };
                // `candidate` is already canonical (ns_dir is); a mismatch
                // means the category itself is a symlink — reject it rather
                // than let e.g. `models -> textures` cross-load files.
                if base != candidate || !base.is_dir() {
                    return Err(AssetPipelineError::UnsafePath {
                        path: candidate.display().to_string(),
                    });
                }
                let mut files = Vec::new();
                collect_files(&base, &mut files)?;
                for (rel, file) in files {
                    let display = format!("assets/{ns}/{category}/{rel}");
                    let ext = file.extension().and_then(|e| e.to_str());
                    match (category, ext) {
                        ("blockstates" | "models", Some("json")) => {
                            let text = read_text_capped(&file, &display)?;
                            let stem = rel
                                .strip_suffix(".json")
                                .expect(".json extension was matched");
                            let key = format!("{ns}:{stem}");
                            if category == "blockstates" {
                                layer.blockstates.insert(key, text);
                            } else {
                                layer.models.insert(key, text);
                            }
                        }
                        ("textures", Some("rgba")) => {
                            let bytes = read_capped(&file, &display)?;
                            let (width, height, rgba) = decode_rgba_sprite(&bytes, &display)?;
                            let stem = rel
                                .strip_suffix(".rgba")
                                .expect(".rgba extension was matched");
                            layer
                                .sprites
                                .insert(format!("{ns}:{stem}"), (width, height, rgba));
                        }
                        ("textures", Some("png")) => {
                            let bytes = read_capped(&file, &display)?;
                            let stem = rel
                                .strip_suffix(".png")
                                .expect(".png extension was matched");
                            let key = format!("{ns}:{stem}");
                            match crate::png_decode::decode_png_rgba(&bytes) {
                                Ok((width, height, rgba)) => {
                                    layer.sprites.insert(key, (width, height, rgba));
                                }
                                // A corrupt or oversized PNG must not kill
                                // the whole pack load: keep the raw bytes as
                                // a (0, 0, _) marker — zero-size sprites are
                                // never valid — and let
                                // `CompiledAssetBundle::build` re-decode it
                                // into `skipped_errors` with the real reason.
                                Err(_) => {
                                    layer.sprites.insert(key, (0, 0, bytes));
                                }
                            }
                        }
                        ("textures", Some("mcmeta")) => {
                            // Only `<name>.rgba.mcmeta` / `<name>.png.mcmeta`
                            // animates a sprite we can decode; `*.mcmeta`
                            // files for anything else are ignored.
                            let Some(stem) = rel
                                .strip_suffix(".mcmeta")
                                .and_then(|s| {
                                    s.strip_suffix(".rgba").or_else(|| s.strip_suffix(".png"))
                                })
                            else {
                                continue;
                            };
                            let text = read_text_capped(&file, &display)?;
                            pending_meta.push((format!("{ns}:{stem}"), display, text));
                        }
                        _ => {}
                    }
                }
            }
        }
        for (sprite_key, display, text) in pending_meta {
            let Some(&(width, height, _)) = layer.sprites.get(&sprite_key) else {
                return Err(AssetPipelineError::InvalidPackFile {
                    path: display,
                    reason: format!("animation has no sibling sprite {sprite_key:?}"),
                });
            };
            // A (0, 0, _) sprite is an undecodable `.png` marker; the sprite
            // itself lands in `skipped_errors` at build time and there is
            // no geometry to animate.
            if width == 0 && height == 0 {
                continue;
            }
            if let Some(animation) = animation_from_mcmeta(&text, width, height)
                .map_err(|reason| AssetPipelineError::InvalidPackFile {
                    path: display.clone(),
                    reason,
                })?
            {
                layer.animations.insert(sprite_key, animation);
            }
        }
        Ok(layer)
    }
}

/// Namespace ids follow the resource-location charset; anything else is
/// rejected before it can produce a malformed map key.
fn valid_namespace(ns: &str) -> bool {
    !ns.is_empty()
        && ns != "."
        && ns != ".."
        && ns
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// `read_dir` entries sorted by file name for deterministic loads.
fn sorted_entries(dir: &Path) -> Result<Vec<fs::DirEntry>, AssetPipelineError> {
    let mut entries: Vec<fs::DirEntry> = fs::read_dir(dir)
        .map_err(|source| AssetPipelineError::Io {
            path: dir.display().to_string(),
            source,
        })?
        .collect::<Result<_, _>>()
        .map_err(|source| AssetPipelineError::Io {
            path: dir.display().to_string(),
            source,
        })?;
    entries.sort_by_key(fs::DirEntry::file_name);
    Ok(entries)
}

/// Recursively collects `(slash-separated relative path, canonical path)`
/// for every regular file under `base`. Every entry is canonicalized and
/// must stay inside `base`, so a symlink pointing outside the category —
/// and therefore outside the pack — is rejected.
fn collect_files(
    base: &Path,
    out: &mut Vec<(String, PathBuf)>,
) -> Result<(), AssetPipelineError> {
    let mut stack = vec![base.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in sorted_entries(&dir)? {
            let raw = entry.path();
            let canon = fs::canonicalize(&raw).map_err(|source| AssetPipelineError::Io {
                path: raw.display().to_string(),
                source,
            })?;
            if !canon.starts_with(base) {
                return Err(AssetPipelineError::UnsafePath {
                    path: raw.display().to_string(),
                });
            }
            if canon.is_dir() {
                stack.push(canon);
            } else if canon.is_file() {
                let rel = canon
                    .strip_prefix(base)
                    .map_err(|_| AssetPipelineError::UnsafePath {
                        path: raw.display().to_string(),
                    })?;
                out.push((rel_to_slash(rel, &raw)?, canon));
            }
        }
    }
    Ok(())
}

/// Renders a canonical relative path with `/` separators. Only `Normal`
/// components can appear under a canonicalized base; anything else is a
/// traversal artefact and fails closed.
fn rel_to_slash(rel: &Path, display: &Path) -> Result<String, AssetPipelineError> {
    let mut parts = Vec::new();
    for component in rel.components() {
        match component {
            Component::Normal(part) => {
                let text = part.to_str().ok_or_else(|| AssetPipelineError::InvalidPackFile {
                    path: display.display().to_string(),
                    reason: "non-UTF-8 path component".to_owned(),
                })?;
                parts.push(text);
            }
            _ => {
                return Err(AssetPipelineError::UnsafePath {
                    path: display.display().to_string(),
                })
            }
        }
    }
    Ok(parts.join("/"))
}

/// Reads `path` after verifying it is not over [`MAX_ASSET_FILE_BYTES`],
/// then re-checks the bytes actually read (the size check and the read are
/// not atomic).
fn read_capped(path: &Path, display: &str) -> Result<Vec<u8>, AssetPipelineError> {
    let meta = fs::metadata(path).map_err(|source| AssetPipelineError::Io {
        path: path.display().to_string(),
        source,
    })?;
    if meta.len() > MAX_ASSET_FILE_BYTES {
        return Err(AssetPipelineError::FileTooLarge {
            path: display.to_owned(),
            bytes: meta.len(),
        });
    }
    let bytes = fs::read(path).map_err(|source| AssetPipelineError::Io {
        path: path.display().to_string(),
        source,
    })?;
    if bytes.len() as u64 > MAX_ASSET_FILE_BYTES {
        return Err(AssetPipelineError::FileTooLarge {
            path: display.to_owned(),
            bytes: bytes.len() as u64,
        });
    }
    Ok(bytes)
}

/// [`read_capped`] plus strict UTF-8, for the JSON documents.
fn read_text_capped(path: &Path, display: &str) -> Result<String, AssetPipelineError> {
    let bytes = read_capped(path, display)?;
    String::from_utf8(bytes).map_err(|_| AssetPipelineError::InvalidPackFile {
        path: display.to_owned(),
        reason: "file is not valid UTF-8".to_owned(),
    })
}

/// Decodes the [`RGBA8_SPRITE_FORMAT`] payload. Fails closed on a truncated
/// header, zero or atlas-impossible dimensions, or a payload length that
/// does not exactly match `width * height * 4`.
fn decode_rgba_sprite(
    bytes: &[u8],
    display: &str,
) -> Result<(u32, u32, Vec<u8>), AssetPipelineError> {
    let bad = |reason: &str| AssetPipelineError::InvalidPackFile {
        path: display.to_owned(),
        reason: reason.to_owned(),
    };
    if bytes.len() < 8 {
        return Err(bad("sprite header truncated (need 8 bytes)"));
    }
    let width = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let height = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    if width == 0 || height == 0 {
        return Err(bad("sprite has a zero width or height"));
    }
    if width > crate::atlas::MAX_ATLAS_DIM || height > crate::atlas::MAX_ATLAS_DIM {
        return Err(bad("sprite dimensions exceed the atlas limit"));
    }
    let expected = u64::from(width) * u64::from(height) * 4;
    if bytes.len() as u64 - 8 != expected {
        return Err(bad("pixel payload length does not match width*height*4"));
    }
    Ok((width, height, bytes[8..].to_vec()))
}

// ---------------------------------------------------------------------------
// Compiled asset bundle
// ---------------------------------------------------------------------------

/// One baked quad with UVs already remapped into the packed atlas: the
/// vanilla `0..16` texture units of [`BakedModelQuad`] become normalized
/// atlas coordinates in [`uv`](Self::uv), and [`tile`](Self::tile) carries
/// the sprite's [`AtlasTileRect`] so shaders can recover tile-safe
/// (`fract`-wrapped) sampling for animated or wrapped UVs.
#[derive(Clone, Debug, PartialEq)]
pub struct AtlasBakedQuad {
    /// Outward-counterclockwise vertex positions in block-local `0..=1`
    /// space after element + blockstate rotation.
    pub positions: [[f32; 3]; 4],
    /// Face normal after element + blockstate rotation.
    pub normal: [f32; 3],
    /// Resolved texture id (`namespace:path`) — also the key into
    /// [`CompiledAssetBundle::animations`].
    pub texture: String,
    /// Per-vertex UVs in normalized atlas space, ready to upload.
    pub uv: [[f32; 2]; 4],
    /// Tile-safe sampling rect of `texture`'s sprite inside the atlas.
    pub tile: AtlasTileRect,
    /// Blockstate-rotated `cullface` direction, if the face declared one.
    pub cullface: Option<Direction>,
    /// Tint layer; `-1` means untinted.
    pub tintindex: i32,
    /// Element `shade` flag.
    pub shade: bool,
}

/// The compiled product of [`CompiledAssetBundle::build`]: one packed atlas
/// with its linear mip chain, merged blockstate definitions, the model
/// overlay stack, merged animation tables, and a per-entry failure log.
#[derive(Clone, Debug)]
pub struct CompiledAssetBundle {
    /// Packed block atlas (`padding = ATLAS_PADDING_TEXELS`).
    pub atlas: PackedAtlas,
    /// Gamma-correct linear-sRGB mip chain below the atlas base image, down
    /// to 1x1.
    pub mipmaps: Vec<AtlasMip>,
    /// Merged `namespace:block` -> [`BlockStateDef`], highest-priority-wins.
    pub blockstates: BTreeMap<String, BlockStateDef>,
    /// Per-layer model documents pushed lowest-to-highest priority.
    pub model_stack: ResourcePackModelStack,
    /// Merged `namespace:path` -> [`SpriteAnimation`], highest-priority-wins.
    pub animations: BTreeMap<String, SpriteAnimation>,
    /// `(entry id, exact reason)` for every pack entry that failed to parse
    /// or pack — the load never panics on a single broken file.
    pub skipped_errors: Vec<(String, String)>,
}

impl CompiledAssetBundle {
    /// Compiles `layers` (ordered lowest-to-highest priority) into one
    /// bundle. Model and blockstate JSON that fails to parse, and sprites
    /// the atlas builder rejects, are recorded in
    /// [`skipped_errors`](Self::skipped_errors) and skipped — lower
    /// priority layers still supply the id. Only atlas-level failures
    /// (empty atlas, atlas overflow) abort the build.
    pub fn build(layers: &[ResourcePackLayer]) -> Result<Self, AssetPipelineError> {
        let mut skipped_errors = Vec::new();
        let mut model_stack = ResourcePackModelStack::new();
        let mut blockstates: BTreeMap<String, BlockStateDef> = BTreeMap::new();
        let mut sprites: BTreeMap<String, (u32, u32, Vec<u8>)> = BTreeMap::new();
        let mut animations: BTreeMap<String, SpriteAnimation> = BTreeMap::new();
        let mut custom_loader_ids = BTreeMap::new();

        for layer in layers {
            let mut models = BTreeMap::new();
            for (id, text) in &layer.models {
                match RawModelJson::parse(text) {
                    Ok(model) => {
                        let id = normalize_asset_id(id);
                        // Unknown Forge/NeoForge keys are accepted by RawModelJson.
                        // Track custom loaders for a geometric fallback after
                        // all model parents and resource-pack overrides exist.
                        let loader = text.contains("\"loader\"")
                            && parse_meta_json(text)
                                .ok()
                                .and_then(|json| {
                                    json.as_object().map(|map| {
                                        matches!(map.get("loader"), Some(MetaJson::Str(_)))
                                    })
                                })
                                .unwrap_or(false);
                        if loader {
                            custom_loader_ids.insert(id.clone(), ());
                        } else {
                            custom_loader_ids.remove(&id);
                        }
                        models.insert(id, model);
                    }
                    Err(err) => skipped_errors.push((id.clone(), err.to_string())),
                }
            }
            model_stack.push_pack(models);
            for (id, text) in &layer.blockstates {
                match BlockStateDef::parse(text) {
                    Ok(def) => {
                        blockstates.insert(normalize_asset_id(id), def);
                    }
                    Err(err) => skipped_errors.push((id.clone(), err.to_string())),
                }
            }
            for (id, sprite) in &layer.sprites {
                sprites.insert(normalize_asset_id(id), sprite.clone());
            }
            for (id, animation) in &layer.animations {
                animations.insert(normalize_asset_id(id), animation.clone());
            }
        }

        // Custom kinetic loaders can supply their mesh at runtime. If a
        // regular parent/elements chain already bakes, preserve its geometry.
        // Otherwise render an explicitly approximate, textured full cube.
        // Only a sprite actually supplied by the pack can trigger fallback.
        let mut fallback_models = BTreeMap::new();
        for id in custom_loader_ids.keys() {
            if bake_model_with_uvlock(&model_stack, id, 0, 0, false)
                .is_ok_and(|baked| !baked.quads.is_empty())
            {
                continue;
            }
            let Some(model) = model_stack.get(id) else {
                continue;
            };
            if model.elements.as_ref().is_some_and(|elements| !elements.is_empty()) {
                continue;
            }
            let texture = ["all", "side", "particle", "texture", "0"]
                .into_iter()
                .filter_map(|key| model.textures.get(key))
                .chain(model.textures.values())
                .filter(|name| !name.starts_with('#'))
                .find(|name| sprites.contains_key(&normalize_asset_id(name)));
            let Some(texture) = texture else {
                continue;
            };
            let mut fallback = RawModelJson::parse(
                r##"{"elements":[{"from":[0,0,0],"to":[16,16,16],"faces":{"down":{"texture":"#all"},"up":{"texture":"#all"},"north":{"texture":"#all"},"south":{"texture":"#all"},"west":{"texture":"#all"},"east":{"texture":"#all"}}}]}"##,
            ).expect("static fallback cube JSON is valid");
            fallback.textures.insert("all".to_owned(), normalize_asset_id(texture));
            fallback_models.insert(id.clone(), fallback);
        }
        if !fallback_models.is_empty() {
            model_stack.push_pack(fallback_models);
        }

        let mut builder = AtlasBuilder::for_blocks();
        builder
            .set_padding(ATLAS_PADDING_TEXELS)
            .map_err(AssetPipelineError::Atlas)?;
        for (name, (width, height, rgba)) in sprites {
            // `from_dir` keeps undecodable `.png` files as (0, 0, bytes)
            // markers so the load itself stays alive; resolve them here so
            // the failure is recorded with the real decode reason.
            let (width, height, rgba) = if width == 0 && height == 0 {
                match crate::png_decode::decode_png_rgba(&rgba) {
                    Ok(decoded) => decoded,
                    Err(err) => {
                        skipped_errors.push((name, err.to_string()));
                        continue;
                    }
                }
            } else {
                (width, height, rgba)
            };
            let mode = sprite_padding_mode(&rgba);
            if let Err(err) =
                builder.add_sprite_with_mode(name.clone(), width, height, rgba, mode)
            {
                skipped_errors.push((name, err.to_string()));
            }
        }
        let atlas = builder.build().map_err(AssetPipelineError::Atlas)?;
        let mipmaps = build_mipmaps_linear(&atlas, atlas.max_mip_levels(), None)
            .map_err(AssetPipelineError::Atlas)?;

        Ok(Self {
            atlas,
            mipmaps,
            blockstates,
            model_stack,
            animations,
            skipped_errors,
        })
    }

    /// Bakes `block_id`'s blockstate for the concrete state `props`
    /// (`property -> value`) at `block_pos`, then remaps every quad's UVs
    /// into [`atlas`](Self::atlas). `block_id` resolves like a resource
    /// location: exact key first, then a `minecraft:`-prefixed guess for
    /// namespace-less ids.
    ///
    /// Fails closed: unknown block -> [`AssetPipelineError::MissingBlockState`],
    /// model/blockstate resolution problems -> [`AssetPipelineError::Model`],
    /// a quad texture missing from the atlas ->
    /// [`AssetPipelineError::Atlas`].
    pub fn bake_block_with_uvs(
        &self,
        block_id: &str,
        props: &[(&str, &str)],
        block_pos: [i32; 3],
    ) -> Result<Vec<AtlasBakedQuad>, AssetPipelineError> {
        let def = find_named(&self.blockstates, block_id)
            .ok_or_else(|| AssetPipelineError::MissingBlockState(block_id.to_owned()))?;
        let state_props: BTreeMap<String, String> = props
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        let baked = blockstate::bake_blockstate(&self.model_stack, def, &state_props, block_pos)
            .map_err(AssetPipelineError::Model)?;
        self.bake_quads_with_uvs(&baked)
    }

    /// Bakes a model id directly (item models, or blocks without a
    /// blockstate indirection) with explicit blockstate rotation and the
    /// `uvlock` flag, then remaps UVs into the atlas like
    /// [`bake_block_with_uvs`](Self::bake_block_with_uvs).
    pub fn bake_model_with_uvs(
        &self,
        model_id: &str,
        blockstate_rot_x_deg: i32,
        blockstate_rot_y_deg: i32,
        uvlock: bool,
    ) -> Result<Vec<AtlasBakedQuad>, AssetPipelineError> {
        let baked = bake_model_with_uvlock(
            &self.model_stack,
            model_id,
            blockstate_rot_x_deg,
            blockstate_rot_y_deg,
            uvlock,
        )
        .map_err(AssetPipelineError::Model)?;
        self.bake_quads_with_uvs(&baked)
    }

    /// The [`SpriteAnimation`] registered for `texture`, resolved like the
    /// atlas sprite lookup: exact id, then the path part of a
    /// `namespace:path` reference, then a `minecraft:`-prefixed guess.
    pub fn animation_for(&self, texture: &str) -> Option<&SpriteAnimation> {
        find_named(&self.animations, texture)
    }

    /// Remaps every quad of `baked` into atlas UVs.
    fn bake_quads_with_uvs(
        &self,
        baked: &BakedModel,
    ) -> Result<Vec<AtlasBakedQuad>, AssetPipelineError> {
        let mut out = Vec::with_capacity(baked.quads.len());
        for quad in &baked.quads {
            out.push(self.atlas_baked_quad(quad)?);
        }
        Ok(out)
    }

    /// One [`BakedModelQuad`] -> [`AtlasBakedQuad`]: tile-safe rect plus
    /// normalized atlas UVs. Fails closed when the texture is not packed.
    fn atlas_baked_quad(&self, quad: &BakedModelQuad) -> Result<AtlasBakedQuad, AssetPipelineError> {
        let tile = self.atlas.tile_rect(&quad.texture).ok_or_else(|| {
            AssetPipelineError::Atlas(AtlasError::TextureNotInAtlas(quad.texture.clone()))
        })?;
        let uv = remap_baked_quad_uvs(&self.atlas, quad).map_err(AssetPipelineError::Atlas)?;
        Ok(AtlasBakedQuad {
            positions: quad.positions,
            normal: quad.normal,
            texture: quad.texture.clone(),
            uv,
            tile,
            cullface: quad.cullface,
            tintindex: quad.tintindex,
            shade: quad.shade,
        })
    }
}

/// `namespace:path` normalization shared with `model_baker`: a bare
/// `block/cube` means `minecraft:block/cube`.
fn normalize_asset_id(raw: &str) -> String {
    if raw.contains(':') {
        raw.to_owned()
    } else {
        format!("minecraft:{raw}")
    }
}

/// Look up exact namespaced ids, defaulting bare names to `minecraft:`.
fn find_named<'a, T>(map: &'a BTreeMap<String, T>, name: &str) -> Option<&'a T> {
    if let Some(value) = map.get(name) {
        return Some(value);
    }
    if !name.contains(':') {
        return map.get(&format!("minecraft:{name}"));
    }
    None
}

/// Padding-mode heuristic: a sprite whose alpha is entirely `255` is
/// treated as a tiled opaque texture and keeps [`PaddingMode::WrapTile`]
/// gutters; any transparency (cutout holes or translucency) gets
/// [`PaddingMode::ClampEdge`] so mip edges replicate the sprite border
/// instead of wrapping texels from the opposite side.
fn sprite_padding_mode(rgba: &[u8]) -> PaddingMode {
    let fully_opaque = rgba
        .as_chunks::<4>()
        .0
        .iter()
        .all(|px| px[3] == u8::MAX);
    if fully_opaque {
        PaddingMode::WrapTile
    } else {
        PaddingMode::ClampEdge
    }
}

// ---------------------------------------------------------------------------
// `.mcmeta` animation parsing (minimal clean-room JSON + animation section)
// ---------------------------------------------------------------------------

/// Parsed JSON value for `.mcmeta` documents. Numbers keep `f64`; the
/// animation schema only needs objects, arrays, strings, numbers, booleans
/// and null.
#[derive(Debug)]
enum MetaJson {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<MetaJson>),
    Obj(BTreeMap<String, MetaJson>),
}

impl MetaJson {
    fn as_object(&self) -> Option<&BTreeMap<String, MetaJson>> {
        match self {
            Self::Obj(map) => Some(map),
            _ => None,
        }
    }

    fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Num(n) => Some(*n),
            _ => None,
        }
    }

    fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(b) => Some(*b),
            _ => None,
        }
    }

    fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }
}

/// Bounds for [`parse_meta_json`]: the historical [`MAX_META_JSON_DEPTH`]
/// cap. The canonical reader counts the root value at depth 0 and rejects
/// any value at `max_depth`, which is exactly the retired parser's contract.
const META_JSON_LIMITS: megu_core::json::JsonLimits = megu_core::json::JsonLimits {
    max_depth: MAX_META_JSON_DEPTH,
    max_nodes: usize::MAX,
    max_bytes: usize::MAX,
};

fn parse_meta_json(text: &str) -> Result<MetaJson, String> {
    let value = megu_core::json::parse_json_with_limits(text, META_JSON_LIMITS, false)
        .map_err(|e| format!("{} at byte {}", e.message, e.offset))?;
    meta_json_from_json(&value)
}

/// Adapt the canonical [`megu_core::json::JsonValue`] tree into
/// [`MetaJson`]: document-ordered members fold into a `BTreeMap` (last
/// duplicate key wins, as before) and non-finite numbers fail closed.
fn meta_json_from_json(value: &megu_core::json::JsonValue) -> Result<MetaJson, String> {
    Ok(match value {
        megu_core::json::JsonValue::Null => MetaJson::Null,
        megu_core::json::JsonValue::Bool(b) => MetaJson::Bool(*b),
        megu_core::json::JsonValue::Number(n) => {
            if !n.is_finite() {
                return Err("number is out of range at byte 0".to_string());
            }
            MetaJson::Num(*n)
        }
        megu_core::json::JsonValue::String(s) => MetaJson::Str(s.clone()),
        megu_core::json::JsonValue::Array(items) => MetaJson::Arr(
            items
                .iter()
                .map(meta_json_from_json)
                .collect::<Result<_, String>>()?,
        ),
        megu_core::json::JsonValue::Object(members) => {
            let mut map = BTreeMap::new();
            for (key, member) in members {
                map.insert(key.clone(), meta_json_from_json(member)?);
            }
            MetaJson::Obj(map)
        }
    })
}

/// Reads a strict non-negative `u32` integer field.
fn meta_u32(value: &MetaJson, field: &str) -> Result<u32, String> {
    let n = value
        .as_f64()
        .ok_or_else(|| format!("{field} must be an integer"))?;
    if n.fract() != 0.0 || !(0.0..=f64::from(u32::MAX)).contains(&n) {
        return Err(format!("{field} must be an integer in 0..={}", u32::MAX));
    }
    Ok(n as u32)
}

/// Maps one `frames[]` entry — either a bare strip index (duration =
/// `frametime`) or `{"index": n, "time": t}` (duration `t` ticks, default
/// `frametime`) — to an [`AnimFrame`].
fn meta_anim_frame(item: &MetaJson, frametime: u32) -> Result<AnimFrame, String> {
    match item {
        MetaJson::Num(_) => Ok(AnimFrame {
            index: meta_u32(item, "frame index")?,
            ticks: frametime,
        }),
        MetaJson::Obj(map) => {
            let index_value = map
                .get("index")
                .ok_or_else(|| "frame object requires an 'index'".to_owned())?;
            let index = meta_u32(index_value, "frame index")?;
            let ticks = match map.get("time") {
                None | Some(MetaJson::Null) => frametime,
                Some(value) => {
                    let ticks = meta_u32(value, "frame time")?;
                    if ticks == 0 {
                        return Err("frame time must be at least 1 tick".to_owned());
                    }
                    ticks
                }
            };
            Ok(AnimFrame { index, ticks })
        }
        _ => Err("frame must be an integer or an object".to_owned()),
    }
}

/// Resolves a `.mcmeta` document against the sibling sprite's geometry:
/// `frame_size` is the sprite width and `strip_height` its height, so only
/// vertical strips of square frames are supported (the layout
/// [`SpriteAnimation`] models). `width`/`height` declarations, when present,
/// must equal the square frame size; grid (`width x height` cell) layouts
/// are rejected fail-closed.
///
/// Returns `Ok(None)` when the document carries no `animation` section —
/// the same "not animated" outcome vanilla gives a bare `.mcmeta`.
/// Resolve one `.mcmeta` text against its sibling sprite's geometry —
/// shared by the directory loader and the zip/JAR layer loaders.
/// `Ok(None)` means the file carries no `"animation"` section.
pub fn animation_from_mcmeta(
    text: &str,
    frame_size: u32,
    strip_height: u32,
) -> Result<Option<SpriteAnimation>, String> {
    let root = parse_meta_json(text)?;
    let Some(section) = root.as_object().and_then(|obj| obj.get("animation")) else {
        return Ok(None);
    };
    if section.is_null() {
        return Ok(None);
    }
    let section = section
        .as_object()
        .ok_or_else(|| format!("animation must be an object, got {}", describe_json(section)))?;
    for key in ["width", "height"] {
        if let Some(value) = section.get(key) {
            let declared = meta_u32(value, &format!("animation.{key}"))?;
            if declared != frame_size {
                return Err(format!(
                    "animation.{key} = {declared} does not match the square frame size {frame_size}"
                ));
            }
        }
    }
    let frametime = match section.get("frametime") {
        None | Some(MetaJson::Null) => 1,
        Some(value) => {
            let ticks = meta_u32(value, "animation.frametime")?;
            if ticks == 0 {
                return Err("animation.frametime must be at least 1 tick".to_owned());
            }
            ticks
        }
    };
    let interpolate = match section.get("interpolate") {
        None | Some(MetaJson::Null) => false,
        Some(value) => value
            .as_bool()
            .ok_or_else(|| "animation.interpolate must be a boolean".to_owned())?,
    };
    let animation = match section.get("frames") {
        None | Some(MetaJson::Null) => {
            SpriteAnimation::strip(frame_size, strip_height, frametime, interpolate)
        }
        Some(MetaJson::Arr(items)) => {
            let mut frames = Vec::with_capacity(items.len());
            for (index, item) in items.iter().enumerate() {
                frames.push(
                    meta_anim_frame(item, frametime)
                        .map_err(|reason| format!("animation.frames[{index}]: {reason}"))?,
                );
            }
            SpriteAnimation::with_frames(frame_size, strip_height, frames, interpolate)
        }
        Some(other) => {
            return Err(format!(
                "animation.frames must be an array, got {}",
                describe_json(other)
            ))
        }
    };
    animation.map(Some).map_err(|err| err.to_string())
}

/// Short human description of a [`MetaJson`] value for type-error
/// diagnostics.
fn describe_json(value: &MetaJson) -> String {
    match value {
        MetaJson::Null => "null".to_owned(),
        MetaJson::Bool(b) => format!("boolean {b}"),
        MetaJson::Num(n) => format!("number {n}"),
        MetaJson::Str(s) => format!("string {s:?}"),
        MetaJson::Arr(_) => "array".to_owned(),
        MetaJson::Obj(_) => "object".to_owned(),
    }
}
