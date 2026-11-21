// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Generic content-addressed custom painting/photo media registry and
//! chunked `MDIA` sync — a clean-room foundation for immersive-painting,
//! camera, and custom-canvas mods with strict fail-closed security limits.
//!
//! Design invariants:
//! - **Raster-only.** [`inspect_image_header`] accepts exactly two container
//!   formats: PNG (signature + parsed `IHDR` width/height) and JPEG
//!   (`\xFF\xD8` + parsed baseline `SOF0`/`SOF2` width/height). SVG, HTML,
//!   scripts, executables, and arbitrary bytes are rejected before any byte
//!   is stored.
//! - **Hard bounds.** Width/height `<= 1024`, pixel count
//!   `<= 1_048_576`, raw payload `<= 512 KiB`. Per-player quotas
//!   ([`DEFAULT_MAX_ASSETS_PER_PLAYER`], [`DEFAULT_MAX_BYTES_PER_PLAYER`])
//!   prevent storage-exhaustion griefing.
//! - **Content addressing.** Assets are keyed by their pure-`std` FIPS 180-4
//!   SHA-256 ([`crate::mod_sync::sha256`]); re-uploading identical bytes
//!   deduplicates to a single stored blob and [`CustomMediaRegistry::
//!   missing_for_client`] lets servers skip re-transfer to clients that
//!   already advertise holding a digest.
//! - **Chunked wire.** [`encode_media_chunks`] splits an asset into
//!   `<= 16 KiB` `MDIA` frames and [`MediaChunkReassembler`] reassembles
//!   them out-of-order, enforcing index/hash/total consistency and verifying
//!   the full SHA-256 *and* the image header before accepting.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::mod_sync::{digest_hex, sha256};

// ---------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------

/// Maximum accepted raw image payload: 512 KiB.
pub const MAX_MEDIA_BYTES: usize = 512 * 1024;
/// Maximum accepted width or height in pixels.
pub const MAX_MEDIA_DIMENSION: u32 = 1024;
/// Maximum accepted `width * height` pixel count.
pub const MAX_MEDIA_PIXELS: u64 = 1_048_576;
/// Default per-player cap on distinct newly-stored assets.
pub const DEFAULT_MAX_ASSETS_PER_PLAYER: usize = 32;
/// Default per-player cap on newly-stored asset bytes (4 MiB).
pub const DEFAULT_MAX_BYTES_PER_PLAYER: u64 = 4 * 1024 * 1024;
/// Maximum canvas extent in blocks per axis.
pub const MAX_CANVAS_BLOCKS: u16 = 64;
/// `MDIA` chunk magic.
pub const MDIA_MAGIC: &[u8; 4] = b"MDIA";
/// Maximum payload bytes carried by one `MDIA` chunk (16 KiB).
pub const MDIA_CHUNK_PAYLOAD: usize = 16 * 1024;
/// Fixed `MDIA` frame header size: magic(4) + sha256(32) +
/// chunk_idx(2) + total_chunks(2) + payload_len(2).
pub const MDIA_HEADER_LEN: usize = 4 + 32 + 2 + 2 + 2;
/// Maximum `total_chunks` a reassembler will accept. 32 chunks of 16 KiB
/// exactly cover [`MAX_MEDIA_BYTES`].
pub const MAX_TOTAL_CHUNKS: u16 = 32;
/// Maximum characters in an uploader or dimension name.
pub const MAX_NAME_CHARS: usize = 64;
/// Maximum characters in an asset title.
pub const MAX_TITLE_CHARS: usize = 64;
/// World-coordinate sanity bound applied to canvas positions.
pub const MAX_WORLD_COORD: i32 = 30_000_000;

const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];
const JPEG_SOI: [u8; 2] = [0xFF, 0xD8];

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Classification of a [`MediaError`] so callers and tests can match on the
/// failure mode without parsing message text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaErrorKind {
    /// Bytes are not a recognized PNG/JPEG container or the header is
    /// truncated/malformed (includes SVG/HTML/script/executable payloads).
    UnsupportedFormat,
    /// Parsed dimensions are zero or exceed the hard pixel/dimension bounds.
    InvalidDimensions,
    /// Raw payload exceeds [`MAX_MEDIA_BYTES`].
    PayloadTooLarge,
    /// Uploader/title/dimension string failed validation.
    InvalidName,
    /// Per-player asset-count or byte quota exceeded.
    QuotaExceeded,
    /// Canvas placement references an unknown asset digest.
    UnknownAsset,
    /// Canvas id already in use, or removal of a missing canvas.
    CanvasConflict,
    /// `MDIA` frame failed structural validation (magic, length, bounds).
    MalformedChunk,
    /// Chunk hash/index disagrees with chunks already received.
    ChunkMismatch,
    /// Chunk index was already filled.
    DuplicateChunk,
    /// Reassembled bytes failed SHA-256 or image-header verification.
    DigestMismatch,
    /// On-disk media-store failure: I/O error, symlink, oversize file, or
    /// integrity violation. Corrupt blobs are never served.
    Storage,
}

/// Fail-closed error for every custom-media check. Carries a
/// [`MediaErrorKind`] plus a human-readable reason for operator logs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaError {
    kind: MediaErrorKind,
    reason: String,
}

impl MediaError {
    pub fn kind(&self) -> MediaErrorKind {
        self.kind
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl fmt::Display for MediaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.reason)
    }
}

impl std::error::Error for MediaError {}

fn err(kind: MediaErrorKind, msg: impl Into<String>) -> MediaError {
    MediaError {
        kind,
        reason: msg.into(),
    }
}

// ---------------------------------------------------------------------------
// Image header inspection
// ---------------------------------------------------------------------------

/// Raster container formats accepted by the registry. Anything else —
/// SVG, HTML, scripts, executables, arbitrary bytes — is rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MediaFormat {
    Png,
    Jpeg,
}

/// Validate parsed pixel dimensions against the hard bounds. Fail-closed:
/// zero, oversized, and over-pixel-count images are all rejected.
fn validate_dimensions(width: u32, height: u32) -> Result<(u16, u16), MediaError> {
    if width == 0 || height == 0 {
        return Err(err(
            MediaErrorKind::InvalidDimensions,
            format!("image has zero dimension ({width}x{height})"),
        ));
    }
    if width > MAX_MEDIA_DIMENSION || height > MAX_MEDIA_DIMENSION {
        return Err(err(
            MediaErrorKind::InvalidDimensions,
            format!(
                "image dimension {width}x{height} exceeds {MAX_MEDIA_DIMENSION}px limit"
            ),
        ));
    }
    let pixels = width as u64 * height as u64;
    if pixels > MAX_MEDIA_PIXELS {
        return Err(err(
            MediaErrorKind::InvalidDimensions,
            format!("image {width}x{height} exceeds {MAX_MEDIA_PIXELS} pixel limit"),
        ));
    }
    Ok((width as u16, height as u16))
}

fn be_u16(bytes: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([bytes[at], bytes[at + 1]])
}

fn be_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// Parse a PNG `IHDR`: 8-byte signature, then chunk length(4) == 13,
/// chunk type `IHDR`, width(4), height(4). CRC is not verified — the
/// registry stores opaque raster bytes, it does not decode pixels.
fn inspect_png(bytes: &[u8]) -> Result<(u16, u16), MediaError> {
    if bytes.len() < 8 || bytes[..8] != PNG_SIGNATURE {
        return Err(err(
            MediaErrorKind::UnsupportedFormat,
            "not a PNG signature",
        ));
    }
    // Need signature + length + type + 13-byte IHDR data + CRC.
    if bytes.len() < 8 + 4 + 4 + 13 + 4 {
        return Err(err(
            MediaErrorKind::UnsupportedFormat,
            "truncated PNG IHDR",
        ));
    }
    if be_u32(bytes, 8) != 13 || &bytes[12..16] != b"IHDR" {
        return Err(err(
            MediaErrorKind::UnsupportedFormat,
            "first PNG chunk is not a 13-byte IHDR",
        ));
    }
    let width = be_u32(bytes, 16);
    let height = be_u32(bytes, 20);
    validate_dimensions(width, height)
}

/// Parse a JPEG header: `\xFF\xD8` SOI, then walk marker segments until a
/// baseline `SOF0` (`0xC0`) or progressive `SOF2` (`0xC2`) segment yields
/// width/height. Every other marker is skipped by its declared segment
/// length; any structural anomaly fails closed.
fn inspect_jpeg(bytes: &[u8]) -> Result<(u16, u16), MediaError> {
    if bytes.len() < 4 || bytes[..2] != JPEG_SOI {
        return Err(err(
            MediaErrorKind::UnsupportedFormat,
            "not a JPEG SOI marker",
        ));
    }
    let mut i = 2usize;
    loop {
        // Expect a marker prefix: one or more 0xFF fill bytes then the
        // marker code.
        if i >= bytes.len() || bytes[i] != 0xFF {
            return Err(err(
                MediaErrorKind::UnsupportedFormat,
                "truncated or malformed JPEG marker stream",
            ));
        }
        while i < bytes.len() && bytes[i] == 0xFF {
            i += 1;
        }
        if i >= bytes.len() {
            return Err(err(
                MediaErrorKind::UnsupportedFormat,
                "truncated JPEG marker",
            ));
        }
        let marker = bytes[i];
        i += 1;
        match marker {
            // Standalone markers carrying no length field.
            0x01 | 0xD0..=0xD9 => {
                if marker == 0xD9 {
                    // EOI reached without ever seeing SOF0/SOF2.
                    return Err(err(
                        MediaErrorKind::UnsupportedFormat,
                        "JPEG has no baseline/progressive SOF segment",
                    ));
                }
                continue;
            }
            0xC0 | 0xC2 => {
                // SOF: length(2) precision(1) height(2) width(2) ...
                if i + 2 > bytes.len() {
                    return Err(err(
                        MediaErrorKind::UnsupportedFormat,
                        "truncated JPEG SOF length",
                    ));
                }
                let seg_len = be_u16(bytes, i) as usize;
                // length counts itself; need >= 7 for length+precision+
                // height+width, and the segment must fit in the buffer.
                if seg_len < 7 || i + seg_len > bytes.len() {
                    return Err(err(
                        MediaErrorKind::UnsupportedFormat,
                        "truncated or malformed JPEG SOF segment",
                    ));
                }
                let height = be_u16(bytes, i + 3) as u32;
                let width = be_u16(bytes, i + 5) as u32;
                return validate_dimensions(width, height);
            }
            _ => {
                // All other markers carry a 2-byte big-endian length that
                // includes itself; skip the whole segment.
                if i + 2 > bytes.len() {
                    return Err(err(
                        MediaErrorKind::UnsupportedFormat,
                        "truncated JPEG segment length",
                    ));
                }
                let seg_len = be_u16(bytes, i) as usize;
                if seg_len < 2 || i + seg_len > bytes.len() {
                    return Err(err(
                        MediaErrorKind::UnsupportedFormat,
                        "truncated or malformed JPEG segment",
                    ));
                }
                i += seg_len;
            }
        }
    }
}

/// Inspect a raw image payload and return `(format, width, height)`.
///
/// Fail-closed: only PNG and JPEG (SOF0/SOF2) containers with sane,
/// in-bounds dimensions are accepted. Callers must separately enforce
/// [`MAX_MEDIA_BYTES`] on `bytes.len()`.
pub fn inspect_image_header(bytes: &[u8]) -> Result<(MediaFormat, u16, u16), MediaError> {
    if bytes.len() > MAX_MEDIA_BYTES {
        return Err(err(
            MediaErrorKind::PayloadTooLarge,
            format!("image payload {} bytes exceeds {MAX_MEDIA_BYTES}", bytes.len()),
        ));
    }
    if bytes.starts_with(&PNG_SIGNATURE) {
        let (w, h) = inspect_png(bytes)?;
        return Ok((MediaFormat::Png, w, h));
    }
    if bytes.len() >= 2 && bytes[..2] == JPEG_SOI {
        let (w, h) = inspect_jpeg(bytes)?;
        return Ok((MediaFormat::Jpeg, w, h));
    }
    Err(err(
        MediaErrorKind::UnsupportedFormat,
        "payload is neither PNG nor JPEG",
    ))
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// Immutable metadata for one stored media asset, keyed by content hash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaAssetMeta {
    /// SHA-256 of the exact stored bytes — the content address.
    pub sha256: [u8; 32],
    pub format: MediaFormat,
    pub width: u16,
    pub height: u16,
    pub byte_len: u32,
    pub title: String,
    /// Uploader that first stored these bytes.
    pub uploader: String,
}

/// Block-space anchor for a placed canvas.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MediaPos {
    pub x: i32,
    pub y: i32,
    pub z: i32,
}

impl MediaPos {
    pub const fn new(x: i32, y: i32, z: i32) -> Self {
        Self { x, y, z }
    }
}

/// Which way a placed canvas faces.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CanvasFacing {
    North,
    South,
    East,
    West,
    Up,
    Down,
}

/// A placed custom canvas binding a world position to a stored asset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CanvasPlacement {
    /// Registry-unique canvas id.
    pub id: u64,
    /// Dimension identifier (e.g. `minecraft:overworld`).
    pub dim: String,
    pub pos: MediaPos,
    pub facing: CanvasFacing,
    pub width_blocks: u16,
    pub height_blocks: u16,
    /// SHA-256 of the asset this canvas displays; must be registered.
    pub media_sha256: [u8; 32],
}

#[derive(Debug)]
struct StoredAsset {
    meta: MediaAssetMeta,
    bytes: Vec<u8>,
}

/// Content-addressed registry of custom painting/photo assets plus the
/// canvas placements that reference them. All validation is fail-closed.
#[derive(Debug)]
pub struct CustomMediaRegistry {
    assets: BTreeMap<[u8; 32], StoredAsset>,
    canvases: BTreeMap<u64, CanvasPlacement>,
    /// Per-uploader `(asset_count, byte_total)` charged only for assets the
    /// uploader *newly introduced* — deduplicated re-uploads store nothing
    /// and are not charged.
    uploader_usage: HashMap<String, (usize, u64)>,
    max_assets_per_player: usize,
    max_bytes_per_player: u64,
}

impl Default for CustomMediaRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl CustomMediaRegistry {
    /// Registry with the default per-player quotas.
    pub fn new() -> Self {
        Self::with_quotas(
            DEFAULT_MAX_ASSETS_PER_PLAYER,
            DEFAULT_MAX_BYTES_PER_PLAYER,
        )
    }

    /// Registry with explicit per-player quotas (for tests and operators).
    pub fn with_quotas(max_assets_per_player: usize, max_bytes_per_player: u64) -> Self {
        Self {
            assets: BTreeMap::new(),
            canvases: BTreeMap::new(),
            uploader_usage: HashMap::new(),
            max_assets_per_player,
            max_bytes_per_player,
        }
    }

    /// Number of distinct stored asset blobs.
    pub fn asset_count(&self) -> usize {
        self.assets.len()
    }

    /// Look up stored asset bytes by content hash.
    pub fn asset_bytes(&self, sha256: &[u8; 32]) -> Option<&[u8]> {
        self.assets.get(sha256).map(|a| a.bytes.as_slice())
    }

    /// Look up asset metadata by content hash.
    pub fn asset_meta(&self, sha256: &[u8; 32]) -> Option<&MediaAssetMeta> {
        self.assets.get(sha256).map(|a| &a.meta)
    }

    /// Iterate all stored asset metadata in digest order.
    pub fn assets(&self) -> impl Iterator<Item = &MediaAssetMeta> {
        self.assets.values().map(|a| &a.meta)
    }

    /// Iterate all placed canvases in id order.
    pub fn canvases(&self) -> impl Iterator<Item = &CanvasPlacement> {
        self.canvases.values()
    }

    /// Validate a name-like field (uploader, dimension): non-empty,
    /// bounded, no control characters.
    fn validate_name(field: &str, value: &str, max_chars: usize) -> Result<(), MediaError> {
        if value.is_empty() || value.chars().count() > max_chars {
            return Err(err(
                MediaErrorKind::InvalidName,
                format!("{field} must be 1..={max_chars} characters"),
            ));
        }
        if value.chars().any(|c| c.is_control()) {
            return Err(err(
                MediaErrorKind::InvalidName,
                format!("{field} contains control characters"),
            ));
        }
        Ok(())
    }

    /// Validate an asset title: same rules as names plus rejection of the
    /// `..` path-traversal fragment.
    fn validate_title(title: &str) -> Result<(), MediaError> {
        Self::validate_name("title", title, MAX_TITLE_CHARS)?;
        if title.contains("..") {
            return Err(err(
                MediaErrorKind::InvalidName,
                "title must not contain '..'",
            ));
        }
        Ok(())
    }

    /// Register an image payload under `uploader`/`title`.
    ///
    /// Fail-closed pipeline: name validation → size bound → PNG/JPEG header
    /// parse → dimension bounds → SHA-256 dedup → per-player quota. On a
    /// dedup hit the existing metadata is returned and no quota is charged.
    /// Returns the stored [`MediaAssetMeta`].
    pub fn register_asset(
        &mut self,
        uploader: &str,
        title: &str,
        bytes: &[u8],
    ) -> Result<MediaAssetMeta, MediaError> {
        Self::validate_name("uploader", uploader, MAX_NAME_CHARS)?;
        Self::validate_title(title)?;
        if bytes.is_empty() {
            return Err(err(
                MediaErrorKind::UnsupportedFormat,
                "empty image payload",
            ));
        }
        if bytes.len() > MAX_MEDIA_BYTES {
            return Err(err(
                MediaErrorKind::PayloadTooLarge,
                format!("image payload {} bytes exceeds {MAX_MEDIA_BYTES}", bytes.len()),
            ));
        }
        let (format, width, height) = inspect_image_header(bytes)?;

        let digest = sha256(bytes);
        if let Some(existing) = self.assets.get(&digest) {
            return Ok(existing.meta.clone());
        }

        let usage = self.uploader_usage.entry(uploader.to_string()).or_default();
        if usage.0 + 1 > self.max_assets_per_player {
            return Err(err(
                MediaErrorKind::QuotaExceeded,
                format!(
                    "uploader '{uploader}' exceeds {max} asset quota",
                    max = self.max_assets_per_player
                ),
            ));
        }
        let byte_len = bytes.len() as u64;
        if usage.1 + byte_len > self.max_bytes_per_player {
            return Err(err(
                MediaErrorKind::QuotaExceeded,
                format!(
                    "uploader '{uploader}' exceeds {} byte quota",
                    self.max_bytes_per_player
                ),
            ));
        }
        usage.0 += 1;
        usage.1 += byte_len;

        let meta = MediaAssetMeta {
            sha256: digest,
            format,
            width,
            height,
            byte_len: bytes.len() as u32,
            title: title.to_string(),
            uploader: uploader.to_string(),
        };
        self.assets.insert(
            digest,
            StoredAsset {
                meta: meta.clone(),
                bytes: bytes.to_vec(),
            },
        );
        Ok(meta)
    }

    /// Place a canvas binding `placement.id` in `placement.dim`/`pos` to a
    /// registered asset. Fails closed on unknown digest, duplicate id,
    /// out-of-range extent, or invalid name/coordinate fields.
    pub fn place_canvas(&mut self, placement: CanvasPlacement) -> Result<(), MediaError> {
        if self.canvases.contains_key(&placement.id) {
            return Err(err(
                MediaErrorKind::CanvasConflict,
                format!("canvas id {} already placed", placement.id),
            ));
        }
        if !self.assets.contains_key(&placement.media_sha256) {
            return Err(err(
                MediaErrorKind::UnknownAsset,
                "canvas references unregistered media sha256",
            ));
        }
        Self::validate_name("dim", &placement.dim, MAX_NAME_CHARS)?;
        if placement.width_blocks == 0
            || placement.height_blocks == 0
            || placement.width_blocks > MAX_CANVAS_BLOCKS
            || placement.height_blocks > MAX_CANVAS_BLOCKS
        {
            return Err(err(
                MediaErrorKind::InvalidDimensions,
                format!(
                    "canvas extent {}x{} blocks outside 1..={MAX_CANVAS_BLOCKS}",
                    placement.width_blocks, placement.height_blocks
                ),
            ));
        }
        for (axis, v) in [
            ("x", placement.pos.x),
            ("y", placement.pos.y),
            ("z", placement.pos.z),
        ] {
            if !(-MAX_WORLD_COORD..=MAX_WORLD_COORD).contains(&v) {
                return Err(err(
                    MediaErrorKind::InvalidDimensions,
                    format!("canvas pos {axis}={v} outside world bounds"),
                ));
            }
        }
        self.canvases.insert(placement.id, placement);
        Ok(())
    }

    /// Remove a placed canvas by id. Returns `true` when one was removed.
    pub fn remove_canvas(&mut self, id: u64) -> bool {
        self.canvases.remove(&id).is_some()
    }

    /// Look up a placed canvas by id.
    pub fn canvas(&self, id: u64) -> Option<&CanvasPlacement> {
        self.canvases.get(&id)
    }

    /// Assets referenced by at least one placed canvas that the client has
    /// *not* already advertised holding — i.e. the digest set the server
    /// still needs to transfer. Returned in digest order for deterministic
    /// sync plans.
    pub fn missing_for_client(
        &self,
        client_known_hashes: &HashSet<[u8; 32]>,
    ) -> Vec<MediaAssetMeta> {
        let referenced: BTreeSet<[u8; 32]> = self
            .canvases
            .values()
            .map(|c| c.media_sha256)
            .collect();
        referenced
            .into_iter()
            .filter(|sha| !client_known_hashes.contains(sha))
            .filter_map(|sha| self.assets.get(&sha).map(|a| a.meta.clone()))
            .collect()
    }

    /// View-scoped variant of [`Self::missing_for_client`]: only canvases in
    /// `dim` whose chunk lies within Chebyshev distance
    /// `view_distance_chunks` of the client's chunk contribute their asset
    /// digest. `client_cx`/`client_cz` are chunk coordinates; canvas block
    /// positions are converted with `div_euclid(16)`. Paintings outside the
    /// player's view — or placed in another dimension — cost zero wire
    /// bytes.
    pub fn missing_for_client_in_view(
        &self,
        dim: &str,
        client_cx: i32,
        client_cz: i32,
        view_distance_chunks: i32,
        client_known_hashes: &HashSet<[u8; 32]>,
    ) -> Vec<MediaAssetMeta> {
        let view = i64::from(view_distance_chunks.max(0));
        let client_cx = i64::from(client_cx);
        let client_cz = i64::from(client_cz);
        let mut referenced: BTreeSet<[u8; 32]> = BTreeSet::new();
        for canvas in self.canvases.values() {
            if canvas.dim != dim {
                continue;
            }
            let cx = i64::from(canvas.pos.x).div_euclid(16);
            let cz = i64::from(canvas.pos.z).div_euclid(16);
            if (cx - client_cx).abs() <= view && (cz - client_cz).abs() <= view {
                referenced.insert(canvas.media_sha256);
            }
        }
        referenced
            .into_iter()
            .filter(|sha| !client_known_hashes.contains(sha))
            .filter_map(|sha| self.assets.get(&sha).map(|a| a.meta.clone()))
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Disk-backed blob store with bounded hot-RAM LRU
// ---------------------------------------------------------------------------

/// Default hot-RAM budget for [`DiskBackedMediaStore`]: 32 MiB.
pub const DEFAULT_HOT_RAM_BYTES: usize = 32 * 1024 * 1024;

/// One hot-cached blob plus its LRU stamp.
#[derive(Debug)]
struct HotBlob {
    bytes: Vec<u8>,
    /// Last-use stamp; the smallest stamp is evicted first.
    stamp: u64,
}

fn store_io(context: &str, e: io::Error) -> MediaError {
    err(MediaErrorKind::Storage, format!("{context}: {e}"))
}

/// Content-addressed image blob store backed by an on-disk directory.
///
/// Every verified image is published atomically to
/// `<root>/<hex_sha256>.bin` and may additionally live in a bounded hot-RAM
/// LRU cache ([`Self::with_ram_cap`], default
/// [`DEFAULT_HOT_RAM_BYTES`]). Evicting from RAM never deletes the blob —
/// [`Self::get_bytes`] transparently reloads it from disk, re-verifying the
/// SHA-256 before serving. Disk reads fail closed on symlinks, oversize
/// files, and digest mismatches: corrupt bytes are never returned.
#[derive(Debug)]
pub struct DiskBackedMediaStore {
    root: PathBuf,
    max_ram_bytes: usize,
    hot: HashMap<[u8; 32], HotBlob>,
    /// `stamp -> sha256` giving O(log n) LRU eviction; exactly one live
    /// entry per hot blob.
    order: BTreeMap<u64, [u8; 32]>,
    clock: u64,
    cached_ram_bytes: usize,
}

impl DiskBackedMediaStore {
    /// Store rooted at `root` with the default 32 MiB hot-RAM budget.
    pub fn new<P: AsRef<Path>>(root: P) -> Result<Self, MediaError> {
        Self::with_ram_cap(root, DEFAULT_HOT_RAM_BYTES)
    }

    /// Store rooted at `root` with an explicit hot-RAM byte budget.
    /// `max_ram_bytes == 0` is clamped to 1 byte, which in practice keeps
    /// the cache empty (every blob is larger) — a disk-only mode.
    /// Fails closed when `root` already exists as a symlink.
    pub fn with_ram_cap<P: AsRef<Path>>(root: P, max_ram_bytes: usize) -> Result<Self, MediaError> {
        let root = root.as_ref().to_path_buf();
        if let Ok(meta) = fs::symlink_metadata(&root) {
            if meta.file_type().is_symlink() {
                return Err(err(
                    MediaErrorKind::Storage,
                    "media store root is a symlink; refusing to follow",
                ));
            }
        }
        fs::create_dir_all(&root).map_err(|e| store_io("create media store root", e))?;
        Ok(Self {
            root,
            max_ram_bytes: max_ram_bytes.max(1),
            hot: HashMap::new(),
            order: BTreeMap::new(),
            clock: 0,
            cached_ram_bytes: 0,
        })
    }

    /// Bytes currently held in the hot-RAM LRU; always
    /// `<= max_ram_bytes`.
    pub fn cached_ram_bytes(&self) -> usize {
        self.cached_ram_bytes
    }

    /// Number of blobs currently resident in hot RAM.
    pub fn cached_count(&self) -> usize {
        self.hot.len()
    }

    /// Configured hot-RAM budget in bytes.
    pub fn max_ram_bytes(&self) -> usize {
        self.max_ram_bytes
    }

    fn blob_path(&self, sha256: &[u8; 32]) -> PathBuf {
        self.root.join(format!("{}.bin", digest_hex(sha256)))
    }

    /// Store `bytes` durably under their SHA-256 content address. The same
    /// fail-closed validation as the registry applies (non-empty, size
    /// bound, PNG/JPEG header with in-bounds dimensions). The blob is
    /// published atomically — temp file in the same directory, then rename
    /// — and admitted to the hot-RAM LRU, evicting older blobs as needed.
    /// Returns the digest; re-storing identical bytes is idempotent.
    pub fn put_bytes(&mut self, bytes: &[u8]) -> Result<[u8; 32], MediaError> {
        if bytes.is_empty() {
            return Err(err(
                MediaErrorKind::UnsupportedFormat,
                "empty image payload",
            ));
        }
        if bytes.len() > MAX_MEDIA_BYTES {
            return Err(err(
                MediaErrorKind::PayloadTooLarge,
                format!("image payload {} bytes exceeds {MAX_MEDIA_BYTES}", bytes.len()),
            ));
        }
        inspect_image_header(bytes)?;
        let digest = sha256(bytes);
        let path = self.blob_path(&digest);
        if let Ok(meta) = fs::symlink_metadata(&path) {
            if meta.file_type().is_symlink() {
                return Err(err(
                    MediaErrorKind::Storage,
                    "refusing to write through a symlinked blob path",
                ));
            }
        }
        let tmp = self
            .root
            .join(format!(".{}.tmp.{}", digest_hex(&digest), std::process::id()));
        fs::write(&tmp, bytes).map_err(|e| store_io("write media blob temp file", e))?;
        if let Err(e) = fs::rename(&tmp, &path) {
            let _ = fs::remove_file(&tmp);
            return Err(store_io("publish media blob", e));
        }
        self.hot_insert(digest, bytes.to_vec());
        Ok(digest)
    }

    /// Fetch blob bytes by digest: served from hot RAM on a hit, otherwise
    /// transparently reloaded from disk — SHA-256 re-verified — and
    /// promoted into the LRU while `cached_ram_bytes` stays within budget.
    /// `Ok(None)` when nothing is stored under `sha256`. I/O failures,
    /// symlinks, oversize files, and digest mismatches are fail-closed
    /// `Err`s; corrupt bytes are never served.
    pub fn get_bytes(&mut self, sha256: &[u8; 32]) -> Result<Option<Vec<u8>>, MediaError> {
        if self.hot.contains_key(sha256) {
            self.touch(*sha256);
            let blob = self.hot.get(sha256).expect("hot entry checked above");
            return Ok(Some(blob.bytes.clone()));
        }
        let path = self.blob_path(sha256);
        let meta = match fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(store_io("stat media blob", e)),
        };
        if meta.file_type().is_symlink() {
            return Err(err(
                MediaErrorKind::Storage,
                "blob path is a symlink; refusing to follow",
            ));
        }
        if !meta.is_file() {
            return Err(err(
                MediaErrorKind::Storage,
                "blob path is not a regular file",
            ));
        }
        if meta.len() > MAX_MEDIA_BYTES as u64 {
            return Err(err(
                MediaErrorKind::Storage,
                format!(
                    "on-disk blob {} bytes exceeds {MAX_MEDIA_BYTES}",
                    meta.len()
                ),
            ));
        }
        let bytes = fs::read(&path).map_err(|e| store_io("read media blob", e))?;
        if bytes.len() > MAX_MEDIA_BYTES {
            return Err(err(
                MediaErrorKind::Storage,
                format!(
                    "on-disk blob {} bytes exceeds {MAX_MEDIA_BYTES}",
                    bytes.len()
                ),
            ));
        }
        if crate::mod_sync::sha256(&bytes) != *sha256 {
            return Err(err(
                MediaErrorKind::DigestMismatch,
                "on-disk blob fails SHA-256 verification",
            ));
        }
        self.hot_insert(*sha256, bytes.clone());
        Ok(Some(bytes))
    }

    /// Whether a blob is retrievable — hot or a regular file on disk.
    /// Symlinked paths do not count: [`Self::get_bytes`] refuses them.
    pub fn contains(&self, sha256: &[u8; 32]) -> bool {
        self.hot.contains_key(sha256)
            || fs::symlink_metadata(self.blob_path(sha256))
                .map(|m| m.is_file())
                .unwrap_or(false)
    }

    /// Insert `bytes` into the hot LRU as most-recently-used, then evict
    /// least-recently-used blobs until `cached_ram_bytes <= max_ram_bytes`.
    /// Blobs larger than the whole budget are never cached — disk remains
    /// authoritative for them.
    fn hot_insert(&mut self, sha256: [u8; 32], bytes: Vec<u8>) {
        if bytes.is_empty() || bytes.len() > self.max_ram_bytes {
            return;
        }
        if let Some(old) = self.hot.remove(&sha256) {
            self.cached_ram_bytes = self.cached_ram_bytes.saturating_sub(old.bytes.len());
            self.order.remove(&old.stamp);
        }
        self.clock += 1;
        let len = bytes.len();
        self.hot
            .insert(sha256, HotBlob { bytes, stamp: self.clock });
        self.order.insert(self.clock, sha256);
        self.cached_ram_bytes += len;
        self.evict_lru();
    }

    /// Mark `sha256` most-recently-used. Caller guarantees a hot entry.
    fn touch(&mut self, sha256: [u8; 32]) {
        self.clock += 1;
        if let Some(blob) = self.hot.get_mut(&sha256) {
            let old_stamp = blob.stamp;
            blob.stamp = self.clock;
            self.order.remove(&old_stamp);
            self.order.insert(self.clock, sha256);
        }
    }

    /// Drop least-recently-used hot blobs until the byte budget holds.
    /// Evicted blobs stay durable on disk.
    fn evict_lru(&mut self) {
        while self.cached_ram_bytes > self.max_ram_bytes {
            let Some((&stamp, &sha256)) = self.order.iter().next() else {
                break;
            };
            self.order.remove(&stamp);
            if let Some(blob) = self.hot.remove(&sha256) {
                self.cached_ram_bytes = self.cached_ram_bytes.saturating_sub(blob.bytes.len());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Chunked MDIA wire framing
// ---------------------------------------------------------------------------

fn put_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_be_bytes());
}

/// Split `bytes` into `MDIA` wire frames.
///
/// Frame layout: `MDIA` magic(4) ‖ sha256(32) ‖ chunk_idx u16 BE ‖
/// total_chunks u16 BE ‖ payload_len u16 BE ‖ payload (<=16 KiB).
/// `sha256` must be the SHA-256 of `bytes` (see [`crate::mod_sync::sha256`]).
/// Fails closed on empty/oversized payloads or a chunk count above
/// [`MAX_TOTAL_CHUNKS`].
pub fn encode_media_chunks(sha256: &[u8; 32], bytes: &[u8]) -> Result<Vec<Vec<u8>>, MediaError> {
    if bytes.is_empty() || bytes.len() > MAX_MEDIA_BYTES {
        return Err(err(
            MediaErrorKind::PayloadTooLarge,
            format!("cannot chunk {}-byte payload", bytes.len()),
        ));
    }
    let total = bytes.len().div_ceil(MDIA_CHUNK_PAYLOAD) as u16;
    if total > MAX_TOTAL_CHUNKS {
        return Err(err(
            MediaErrorKind::PayloadTooLarge,
            format!("{total} chunks exceeds {MAX_TOTAL_CHUNKS} limit"),
        ));
    }
    let mut frames = Vec::with_capacity(total as usize);
    for (idx, chunk) in bytes.chunks(MDIA_CHUNK_PAYLOAD).enumerate() {
        let mut frame = Vec::with_capacity(MDIA_HEADER_LEN + chunk.len());
        frame.extend_from_slice(MDIA_MAGIC);
        frame.extend_from_slice(sha256);
        put_u16(&mut frame, idx as u16);
        put_u16(&mut frame, total);
        put_u16(&mut frame, chunk.len() as u16);
        frame.extend_from_slice(chunk);
        frames.push(frame);
    }
    Ok(frames)
}

/// Stateful out-of-order reassembler for `MDIA` frames.
///
/// Enforces: exact frame length, `MDIA` magic, `total_chunks` in
/// `1..=32`, index in range, consistent hash/total across frames, no
/// duplicate indices, full-size payloads on every non-final chunk, and —
/// once all chunks arrive — SHA-256 equality plus a valid in-bounds image
/// header before the payload is accepted.
#[derive(Debug, Default)]
pub struct MediaChunkReassembler {
    expected_sha256: Option<[u8; 32]>,
    sha256: Option<[u8; 32]>,
    total_chunks: Option<u16>,
    chunks: Vec<Option<Vec<u8>>>,
    received: usize,
    assembled: Option<Vec<u8>>,
}

impl MediaChunkReassembler {
    /// Reassembler that accepts whichever asset digest the stream carries.
    pub fn new() -> Self {
        Self::default()
    }

    /// Reassembler pinned to an expected asset digest; the first chunk's
    /// hash (and the final digest) must match it.
    pub fn with_expected_sha256(expected: [u8; 32]) -> Self {
        Self {
            expected_sha256: Some(expected),
            ..Self::default()
        }
    }

    /// Feed one raw `MDIA` frame. Returns `Ok(true)` exactly once, when the
    /// final chunk completes a verified reassembly; the accepted bytes are
    /// then available via [`Self::assembled_bytes`]. Any failure leaves the
    /// reassembler unusable for that stream (fail-closed).
    pub fn push(&mut self, frame: &[u8]) -> Result<bool, MediaError> {
        if self.assembled.is_some() {
            return Err(err(
                MediaErrorKind::MalformedChunk,
                "reassembler already completed",
            ));
        }
        if frame.len() < MDIA_HEADER_LEN {
            return Err(err(
                MediaErrorKind::MalformedChunk,
                format!("MDIA frame {} bytes shorter than header", frame.len()),
            ));
        }
        if &frame[..4] != MDIA_MAGIC {
            return Err(err(
                MediaErrorKind::MalformedChunk,
                "MDIA frame missing magic",
            ));
        }
        let mut sha = [0u8; 32];
        sha.copy_from_slice(&frame[4..36]);
        let idx = be_u16(frame, 36);
        let total = be_u16(frame, 38);
        let payload_len = be_u16(frame, 40) as usize;
        if payload_len > MDIA_CHUNK_PAYLOAD {
            return Err(err(
                MediaErrorKind::MalformedChunk,
                format!("MDIA payload_len {payload_len} exceeds {MDIA_CHUNK_PAYLOAD}"),
            ));
        }
        if frame.len() != MDIA_HEADER_LEN + payload_len {
            return Err(err(
                MediaErrorKind::MalformedChunk,
                format!(
                    "MDIA frame length {} != header+payload_len {}",
                    frame.len(),
                    MDIA_HEADER_LEN + payload_len
                ),
            ));
        }
        if total == 0 || total > MAX_TOTAL_CHUNKS {
            return Err(err(
                MediaErrorKind::MalformedChunk,
                format!("MDIA total_chunks {total} outside 1..={MAX_TOTAL_CHUNKS}"),
            ));
        }
        if idx >= total {
            return Err(err(
                MediaErrorKind::MalformedChunk,
                format!("MDIA chunk_idx {idx} >= total_chunks {total}"),
            ));
        }
        // Every chunk except the last must carry a full 16 KiB payload so
        // a truncated mid-stream payload cannot masquerade as complete.
        let is_last = idx + 1 == total;
        if !is_last && payload_len != MDIA_CHUNK_PAYLOAD {
            return Err(err(
                MediaErrorKind::MalformedChunk,
                format!("non-final MDIA chunk {idx} payload_len {payload_len} != {MDIA_CHUNK_PAYLOAD}"),
            ));
        }
        if is_last && payload_len == 0 {
            return Err(err(
                MediaErrorKind::MalformedChunk,
                "final MDIA chunk has empty payload",
            ));
        }

        if let Some(expected) = self.expected_sha256 {
            if sha != expected {
                return Err(err(
                    MediaErrorKind::ChunkMismatch,
                    "MDIA chunk sha256 disagrees with expected digest",
                ));
            }
        }
        match self.sha256 {
            None => {
                self.sha256 = Some(sha);
                self.total_chunks = Some(total);
                self.chunks = vec![None; total as usize];
            }
            Some(seen_sha) => {
                if seen_sha != sha {
                    return Err(err(
                        MediaErrorKind::ChunkMismatch,
                        "MDIA chunk sha256 disagrees with earlier chunks",
                    ));
                }
            }
        }
        if self.total_chunks != Some(total) {
            return Err(err(
                MediaErrorKind::ChunkMismatch,
                format!(
                    "MDIA total_chunks {total} disagrees with earlier {}",
                    self.total_chunks.unwrap_or(0)
                ),
            ));
        }
        let slot = &mut self.chunks[idx as usize];
        if slot.is_some() {
            return Err(err(
                MediaErrorKind::DuplicateChunk,
                format!("MDIA chunk_idx {idx} received twice"),
            ));
        }
        *slot = Some(frame[MDIA_HEADER_LEN..].to_vec());
        self.received += 1;

        if self.received < total as usize {
            return Ok(false);
        }

        // All chunks present: concatenate and verify content address +
        // image header before accepting.
        let mut assembled = Vec::new();
        let total_len_estimate = self
            .chunks
            .iter()
            .map(|c| c.as_ref().map_or(0, |p| p.len()))
            .sum::<usize>();
        assembled.reserve(total_len_estimate);
        for slot in self.chunks.iter() {
            let payload = slot.as_ref().expect("all slots filled");
            assembled.extend_from_slice(payload);
        }
        if assembled.len() > MAX_MEDIA_BYTES {
            return Err(err(
                MediaErrorKind::PayloadTooLarge,
                format!("reassembled {} bytes exceeds {MAX_MEDIA_BYTES}", assembled.len()),
            ));
        }
        let digest = sha256(&assembled);
        let header_sha = self.sha256.expect("sha pinned on first chunk");
        if digest != header_sha {
            return Err(err(
                MediaErrorKind::DigestMismatch,
                "reassembled bytes fail SHA-256 verification",
            ));
        }
        inspect_image_header(&assembled).map_err(|e| {
            err(
                MediaErrorKind::DigestMismatch,
                format!("reassembled bytes fail image validation: {}", e.reason()),
            )
        })?;
        self.assembled = Some(assembled);
        Ok(true)
    }

    /// `true` once a fully verified payload has been accepted.
    pub fn is_complete(&self) -> bool {
        self.assembled.is_some()
    }

    /// Number of distinct chunk indices received so far.
    pub fn received_chunks(&self) -> usize {
        self.received
    }

    /// The verified reassembled payload, available after `push` returned
    /// `Ok(true)`.
    pub fn assembled_bytes(&self) -> Option<&[u8]> {
        self.assembled.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn be_helpers_roundtrip() {
        assert_eq!(be_u16(&[0x12, 0x34], 0), 0x1234);
        assert_eq!(be_u32(&[0x12, 0x34, 0x56, 0x78], 0), 0x12345678);
    }
}
