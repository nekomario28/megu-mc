// SPDX-License-Identifier: LGPL-3.0-or-later
//! Disk-backed GPU pipeline cache: persists compiled pipeline blobs across
//! launches so repeat startups skip driver-side shader compilation
//! (the `<50 ms` to-3D-world path).
//!
//! Entries are keyed by the full pipeline-identity tuple
//! `(vendor_id, device_id, backend, driver_info, shader_source_sha256,
//! megu_version)` — a driver update, GPU change, shader edit, or client
//! upgrade lands on a fresh filename instead of a stale hit.
//!
//! Each file is an `MGPC` envelope: magic + format version + the canonical
//! key + the SHA-256 of the blob + the blob itself. [`PipelineCacheStore::load`]
//! is fail-closed — a missing file, a key mismatch, a size violation, or a
//! single-bit corruption anywhere in the envelope returns `None` (and evicts
//! the bad file) rather than handing the driver a corrupt binary to feed
//! Vulkan/DX12/Metal.
//!
//! [`PipelineCacheStore::save_atomic`] writes a temp file, `sync_all`s it,
//! then atomically renames over the destination and fsyncs the directory,
//! so a crash mid-write can never leave a torn cache entry. Afterwards it
//! prunes entries whose driver/version identity no longer matches and
//! keeps at most `max_entries` total.
//!
//! The blob is opaque here; the intended producer/consumer is
//! `wgpu::Device::create_pipeline_cache` (`PipelineCacheDescriptor::data`/
//! `get_data`) once `Features::PIPELINE_CACHE` is negotiated.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use megu_core::crypto::{hex_encode, sha256};

/// File magic: `MGPC` (meGu Pipeline Cache).
pub const PIPELINE_CACHE_MAGIC: [u8; 4] = *b"MGPC";
/// Envelope format version; older/newer files fail closed on load.
pub const PIPELINE_CACHE_FORMAT_VERSION: u32 = 1;
/// Largest pipeline blob the store will persist or hand back (64 MiB —
/// far past any real driver cache, guarding against runaway files).
pub const MAX_PIPELINE_CACHE_BLOB_BYTES: u64 = 64 * 1024 * 1024;
/// Default live-entry bound for [`PipelineCacheStore::open`].
pub const DEFAULT_MAX_ENTRIES: usize = 8;

const FILE_PREFIX: &str = "mgpc-";
const FILE_SUFFIX: &str = ".mgpc";
/// Serialized-key ceiling inside an envelope (real keys are < 1 KiB; a
/// larger claim means corruption).
const MAX_KEY_BYTES: u64 = 16 * 1024;
const MAX_STRING_FIELD: u64 = 4096;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Identity of one compiled pipeline cache blob.
///
/// Every field participates in both the on-disk filename and the
/// in-envelope key block, so any change produces a distinct entry rather
/// than a stale hit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PipelineCacheKey {
    /// `wgpu::AdapterInfo::vendor` (backend-specific vendor id).
    pub vendor_id: u32,
    /// `wgpu::AdapterInfo::device` (backend-specific device id).
    pub device_id: u32,
    /// Backend name (`"Vulkan"`, `"Dx12"`, `"Metal"`, ...).
    pub backend: String,
    /// Driver identity (`driver` + `driver_info` joined by `\u{1f}` when
    /// built via [`Self::from_adapter_info`]); any driver update must
    /// change this string.
    pub driver_info: String,
    /// SHA-256 of the shader source the pipelines were built from.
    pub shader_source_sha256: [u8; 32],
    /// Megu client version string.
    pub megu_version: String,
}

impl PipelineCacheKey {
    /// Builds a key from a live `wgpu` adapter. `shader_source` is the
    /// concatenated WGSL (or the pipeline description bytes) the cached
    /// pipelines were compiled from; it is hashed, never stored.
    pub fn from_adapter_info(
        info: &wgpu::AdapterInfo,
        shader_source: &[u8],
        megu_version: impl Into<String>,
    ) -> Self {
        Self {
            vendor_id: info.vendor,
            device_id: info.device,
            backend: format!("{:?}", info.backend),
            driver_info: format!("{}\u{1f}{}", info.driver, info.driver_info),
            shader_source_sha256: sha256(shader_source),
            megu_version: megu_version.into(),
        }
    }

    /// Canonical key serialization (LE integers, length-prefixed
    /// strings): deterministic across platforms and releases.
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(64 + self.backend.len() + self.driver_info.len());
        out.extend_from_slice(&self.vendor_id.to_le_bytes());
        out.extend_from_slice(&self.device_id.to_le_bytes());
        write_len_prefixed(&mut out, self.backend.as_bytes());
        write_len_prefixed(&mut out, self.driver_info.as_bytes());
        out.extend_from_slice(&self.shader_source_sha256);
        write_len_prefixed(&mut out, self.megu_version.as_bytes());
        out
    }

    /// Inverse of [`Self::encode`]; `None` on any truncation or
    /// non-canonical trailing bytes.
    fn decode(bytes: &[u8]) -> Option<Self> {
        let mut cursor = bytes;
        let vendor_id = take_u32(&mut cursor)?;
        let device_id = take_u32(&mut cursor)?;
        let backend = take_string(&mut cursor)?;
        let driver_info = take_string(&mut cursor)?;
        let shader_source_sha256: [u8; 32] = cursor.get(..32)?.try_into().ok()?;
        cursor = &cursor[32..];
        let megu_version = take_string(&mut cursor)?;
        if !cursor.is_empty() {
            return None;
        }
        Some(Self {
            vendor_id,
            device_id,
            backend,
            driver_info,
            shader_source_sha256,
            megu_version,
        })
    }
}

fn write_len_prefixed(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(bytes);
}

fn take_u32(cursor: &mut &[u8]) -> Option<u32> {
    let bytes: [u8; 4] = cursor.get(..4)?.try_into().ok()?;
    *cursor = &cursor[4..];
    Some(u32::from_le_bytes(bytes))
}

fn take_u64(cursor: &mut &[u8]) -> Option<u64> {
    let bytes: [u8; 8] = cursor.get(..8)?.try_into().ok()?;
    *cursor = &cursor[8..];
    Some(u64::from_le_bytes(bytes))
}

fn take_string(cursor: &mut &[u8]) -> Option<String> {
    let len = take_u32(cursor)? as usize;
    if len as u64 > MAX_STRING_FIELD || cursor.len() < len {
        return None;
    }
    let text = String::from_utf8(cursor.get(..len)?.to_vec()).ok()?;
    *cursor = &cursor[len..];
    Some(text)
}

/// One cache directory holding `MGPC` envelope files.
///
/// Not thread-affine; cloning is cheap-free so share one `&` or wrap in
/// `Arc` — concurrent `save_atomic` calls use distinct temp names and
/// last-writer-wins renames, matching the OS's atomicity guarantees.
pub struct PipelineCacheStore {
    dir: PathBuf,
    max_entries: usize,
}

impl PipelineCacheStore {
    /// Opens (creating if needed) a cache directory, keeping at most
    /// `max_entries` live blobs.
    pub fn open(dir: impl Into<PathBuf>, max_entries: usize) -> io::Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        Ok(Self { dir, max_entries })
    }

    /// The cache directory this store owns.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Deterministic filename for `key` (`mgpc-<128-bit key hash>.mgpc`).
    pub fn key_filename(key: &PipelineCacheKey) -> String {
        let digest = sha256(&key.encode());
        format!("{}{}{}", FILE_PREFIX, hex_encode(&digest[..16]), FILE_SUFFIX)
    }

    /// Loads the blob for `key`, or `None` when absent, stale, or damaged.
    ///
    /// Fail-closed: any envelope defect — bad magic, wrong format version,
    /// key mismatch, size violation, truncated blob, or a SHA-256
    /// mismatch (single-bit flip included) — evicts the file and returns
    /// `None`; a corrupt driver binary never reaches the GPU stack.
    pub fn load(&self, key: &PipelineCacheKey) -> Option<Vec<u8>> {
        let path = self.dir.join(Self::key_filename(key));
        let blob = self.try_load(&path, key);
        if blob.is_none() && path.exists() {
            // Corrupt or mismatched envelope: evict so next launch does
            // not re-read the same bad bytes.
            let _ = fs::remove_file(&path);
        }
        blob
    }

    fn try_load(&self, path: &Path, key: &PipelineCacheKey) -> Option<Vec<u8>> {
        let metadata = fs::metadata(path).ok()?;
        let file_len = metadata.len();
        let envelope_min = 4 + 4 + 4 + 32 + 8;
        if file_len < envelope_min || file_len > envelope_min + MAX_KEY_BYTES + MAX_PIPELINE_CACHE_BLOB_BYTES {
            return None;
        }
        let raw = fs::read(path).ok()?;
        let mut cursor = raw.as_slice();
        if cursor.get(..4)? != PIPELINE_CACHE_MAGIC {
            return None;
        }
        cursor = &cursor[4..];
        if take_u32(&mut cursor)? != PIPELINE_CACHE_FORMAT_VERSION {
            return None;
        }
        let key_len = take_u32(&mut cursor)? as u64;
        if key_len > MAX_KEY_BYTES || cursor.len() < key_len as usize {
            return None;
        }
        let key_bytes = &cursor[..key_len as usize];
        cursor = &cursor[key_len as usize..];
        // Byte-identical key check: every identity field must match.
        if key_bytes != key.encode().as_slice() {
            return None;
        }
        let blob_sha: [u8; 32] = cursor.get(..32)?.try_into().ok()?;
        cursor = &cursor[32..];
        let blob_len = take_u64(&mut cursor)?;
        if blob_len > MAX_PIPELINE_CACHE_BLOB_BYTES || blob_len as usize != cursor.len() {
            return None;
        }
        if sha256(cursor) != blob_sha {
            return None;
        }
        Some(cursor.to_vec())
    }

    /// Persists `blob` for `key` atomically (temp write, `sync_all`,
    /// rename, directory fsync), then prunes stale-driver and overflow
    /// entries. `blob` larger than [`MAX_PIPELINE_CACHE_BLOB_BYTES`] is
    /// rejected rather than truncated — a partial pipeline cache is
    /// worse than none.
    pub fn save_atomic(&self, key: &PipelineCacheKey, blob: &[u8]) -> io::Result<()> {
        if blob.len() as u64 > MAX_PIPELINE_CACHE_BLOB_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "pipeline cache blob exceeds 64 MiB cap",
            ));
        }
        let key_bytes = key.encode();
        let mut envelope =
            Vec::with_capacity(4 + 4 + 4 + key_bytes.len() + 32 + 8 + blob.len());
        envelope.extend_from_slice(&PIPELINE_CACHE_MAGIC);
        envelope.extend_from_slice(&PIPELINE_CACHE_FORMAT_VERSION.to_le_bytes());
        envelope.extend_from_slice(&(key_bytes.len() as u32).to_le_bytes());
        envelope.extend_from_slice(&key_bytes);
        envelope.extend_from_slice(&sha256(blob));
        envelope.extend_from_slice(&(blob.len() as u64).to_le_bytes());
        envelope.extend_from_slice(blob);

        let final_path = self.dir.join(Self::key_filename(key));
        let temp_path = self.dir.join(format!(
            "{}.tmp-{}-{}",
            Self::key_filename(key),
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));

        let write_result = (|| -> io::Result<()> {
            {
                let mut file = File::create(&temp_path)?;
                file.write_all(&envelope)?;
                file.sync_all()?;
            }
            fs::rename(&temp_path, &final_path)?;
            // Best-effort directory fsync so the rename itself is durable;
            // not every filesystem exposes directory handles (e.g. some
            // Windows/network FSes), so failure here is non-fatal.
            if let Ok(dir) = File::open(&self.dir) {
                let _ = dir.sync_all();
            }
            Ok(())
        })();
        if write_result.is_err() {
            let _ = fs::remove_file(&temp_path);
        }
        write_result?;
        self.prune(key);
        Ok(())
    }

    /// Removes entries whose driver/version identity differs from
    /// `current` (stale after driver update, GPU swap, or client upgrade),
    /// evicts unparseable files, then trims the oldest survivors past
    /// `max_entries`. All removals are best-effort.
    fn prune(&self, current: &PipelineCacheKey) {
        let mut live: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
        let read_dir = match fs::read_dir(&self.dir) {
            Ok(read_dir) => read_dir,
            Err(_) => return,
        };
        for entry in read_dir.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if !(name.starts_with(FILE_PREFIX) && name.ends_with(FILE_SUFFIX)) {
                continue;
            }
            match self.read_entry_key(&path) {
                Some(stored) if Self::same_driver_identity(&stored, current) => {
                    let modified = entry
                        .metadata()
                        .and_then(|m| m.modified())
                        .unwrap_or(std::time::UNIX_EPOCH);
                    live.push((path, modified));
                }
                _ => {
                    let _ = fs::remove_file(&path);
                }
            }
        }
        live.sort_by_key(|(_, modified)| *modified);
        while live.len() > self.max_entries {
            let (path, _) = live.remove(0);
            let _ = fs::remove_file(&path);
        }
    }

    /// Identity tuple that decides staleness: everything except the
    /// shader hash (a shader change keys a *different* entry; it does not
    /// make this one stale).
    fn same_driver_identity(stored: &PipelineCacheKey, current: &PipelineCacheKey) -> bool {
        stored.vendor_id == current.vendor_id
            && stored.device_id == current.device_id
            && stored.backend == current.backend
            && stored.driver_info == current.driver_info
            && stored.megu_version == current.megu_version
    }

    /// Reads just the envelope header (magic, version, key) — never the
    /// blob — for prune-time identity checks.
    fn read_entry_key(&self, path: &Path) -> Option<PipelineCacheKey> {
        let metadata = fs::metadata(path).ok()?;
        if metadata.len() < 12 || metadata.len() > 12 + MAX_KEY_BYTES + MAX_PIPELINE_CACHE_BLOB_BYTES
        {
            return None;
        }
        let mut file = File::open(path).ok()?;
        let mut header = [0u8; 12];
        file.read_exact(&mut header).ok()?;
        if header[..4] != PIPELINE_CACHE_MAGIC {
            return None;
        }
        if u32::from_le_bytes(header[4..8].try_into().ok()?) != PIPELINE_CACHE_FORMAT_VERSION {
            return None;
        }
        let key_len = u32::from_le_bytes(header[8..12].try_into().ok()?) as u64;
        if key_len > MAX_KEY_BYTES {
            return None;
        }
        let mut key_bytes = vec![0u8; key_len as usize];
        file.read_exact(&mut key_bytes).ok()?;
        PipelineCacheKey::decode(&key_bytes)
    }
}
