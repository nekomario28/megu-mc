// SPDX-License-Identifier: LGPL-3.0-or-later
//! `PipelineCacheStore` coverage: atomic save/load roundtrips,
//! driver-update and shader-change invalidation, single-bit corruption
//! rejection (fail-closed — a bad blob never reaches the driver), and
//! stale-entry pruning.

use std::path::PathBuf;

use megu_client_gpu::pipeline_cache_store::{
    PipelineCacheKey, PipelineCacheStore, DEFAULT_MAX_ENTRIES, PIPELINE_CACHE_MAGIC,
};

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "megu-pcache-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn key(driver: &str, shader: &[u8], version: &str) -> PipelineCacheKey {
    PipelineCacheKey {
        vendor_id: 0x8086,
        device_id: 0xa7a1,
        backend: "Vulkan".to_string(),
        driver_info: driver.to_string(),
        shader_source_sha256: megu_core::crypto::sha256(shader),
        megu_version: version.to_string(),
    }
}

fn blob(tag: u8, len: usize) -> Vec<u8> {
    (0..len).map(|i| tag.wrapping_add(i as u8)).collect()
}

fn entry_path(dir: &TempDir, key: &PipelineCacheKey) -> PathBuf {
    dir.0.join(PipelineCacheStore::key_filename(key))
}

#[test]
fn save_then_load_roundtrips_blob() {
    let dir = TempDir::new("roundtrip");
    let store = PipelineCacheStore::open(dir.0.clone(), DEFAULT_MAX_ENTRIES).unwrap();
    let key = key("Mesa 26.1.0", b"shader-v1", "0.1.0-alpha.4");
    let payload = blob(0x5a, 1024);

    assert!(store.load(&key).is_none(), "cold cache must miss");
    store.save_atomic(&key, &payload).unwrap();
    assert_eq!(store.load(&key).as_deref(), Some(payload.as_slice()));
    // Deterministic filename round-trip.
    assert!(entry_path(&dir, &key).is_file());
}

#[test]
fn empty_and_large_blobs_roundtrip() {
    let dir = TempDir::new("sizes");
    let store = PipelineCacheStore::open(dir.0.clone(), DEFAULT_MAX_ENTRIES).unwrap();
    let key = key("driver", b"shader", "v1");

    store.save_atomic(&key, &[]).unwrap();
    assert_eq!(store.load(&key).as_deref(), Some(&[][..]));

    let large = blob(0x11, 4 * 1024 * 1024 + 13);
    store.save_atomic(&key, &large).unwrap();
    assert_eq!(store.load(&key).as_deref(), Some(large.as_slice()));
}

#[test]
fn driver_update_invalidates_entry() {
    let dir = TempDir::new("driver");
    let store = PipelineCacheStore::open(dir.0.clone(), DEFAULT_MAX_ENTRIES).unwrap();
    let old = key("Mesa 26.1.0", b"shader", "v1");
    let updated = key("Mesa 26.2.0", b"shader", "v1");

    store.save_atomic(&old, &blob(1, 256)).unwrap();
    assert!(store.load(&updated).is_none(), "driver bump must miss");
    // ...and saving under the new driver must prune the stale entry.
    store.save_atomic(&updated, &blob(2, 256)).unwrap();
    assert!(!entry_path(&dir, &old).exists());
    assert!(entry_path(&dir, &updated).exists());
}

#[test]
fn shader_change_invalidates_entry() {
    let dir = TempDir::new("shader");
    let store = PipelineCacheStore::open(dir.0.clone(), DEFAULT_MAX_ENTRIES).unwrap();
    let v1 = key("driver", b"shader-v1", "v1");
    let v2 = key("driver", b"shader-v2", "v1");

    store.save_atomic(&v1, &blob(1, 256)).unwrap();
    assert!(store.load(&v2).is_none(), "shader hash change must miss");
}

#[test]
fn version_change_invalidates_entry() {
    let dir = TempDir::new("version");
    let store = PipelineCacheStore::open(dir.0.clone(), DEFAULT_MAX_ENTRIES).unwrap();
    let old = key("driver", b"shader", "v1");
    let upgraded = key("driver", b"shader", "v2");

    store.save_atomic(&old, &blob(1, 256)).unwrap();
    assert!(store.load(&upgraded).is_none(), "client upgrade must miss");
    store.save_atomic(&upgraded, &blob(2, 256)).unwrap();
    assert!(!entry_path(&dir, &old).exists());
}

#[test]
fn single_bit_corruption_fails_closed() {
    let dir = TempDir::new("corrupt");
    let store = PipelineCacheStore::open(dir.0.clone(), DEFAULT_MAX_ENTRIES).unwrap();
    let key = key("driver", b"shader", "v1");
    let payload = blob(0x77, 512);
    store.save_atomic(&key, &payload).unwrap();
    let path = entry_path(&dir, &key);

    // Flip one bit near the end (inside the blob payload).
    let mut bytes = std::fs::read(&path).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0x01;
    std::fs::write(&path, &bytes).unwrap();

    assert!(store.load(&key).is_none(), "corrupt blob must never load");
    assert!(!path.exists(), "corrupt envelope must be evicted");
}

#[test]
fn truncated_and_bad_magic_envelopes_fail_closed() {
    let dir = TempDir::new("header");
    let store = PipelineCacheStore::open(dir.0.clone(), DEFAULT_MAX_ENTRIES).unwrap();
    let key = key("driver", b"shader", "v1");
    store.save_atomic(&key, &blob(3, 300)).unwrap();
    let path = entry_path(&dir, &key);

    // Truncate mid-blob.
    let mut bytes = std::fs::read(&path).unwrap();
    bytes.truncate(bytes.len() - 7);
    std::fs::write(&path, &bytes).unwrap();
    assert!(store.load(&key).is_none());

    // Bad magic.
    store.save_atomic(&key, &blob(3, 300)).unwrap();
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[0] = b'X';
    std::fs::write(&path, &bytes).unwrap();
    assert!(store.load(&key).is_none());
    assert_eq!(&PIPELINE_CACHE_MAGIC, b"MGPC");
}

#[test]
fn pruning_keeps_at_most_max_entries() {
    let dir = TempDir::new("prune");
    let store = PipelineCacheStore::open(dir.0.clone(), 2).unwrap();

    // Three distinct shader entries under one driver identity: after the
    // third save the store must hold at most 2 files, and the freshest
    // entry always survives.
    let keys: Vec<PipelineCacheKey> = (0..3u8)
        .map(|i| key("driver", &[b's', i], "v1"))
        .collect();
    for (i, k) in keys.iter().enumerate() {
        store.save_atomic(k, &blob(i as u8, 64)).unwrap();
    }
    let files: Vec<_> = std::fs::read_dir(&dir.0)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".mgpc"))
        .collect();
    assert!(files.len() <= 2, "max_entries=2 violated: {} files", files.len());
    assert_eq!(
        store.load(&keys[2]).as_deref(),
        Some(blob(2u8, 64).as_slice())
    );
}
