// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Security & functional tests for the generic content-addressed custom
//! media registry (`mod-adapter-core/src/custom_media.rs`): fail-closed
//! PNG/JPEG header inspection, hard dimension/size bounds, per-player
//! quotas, SHA-256 dedup with client-known-hash skip, canvas lifecycle,
//! and chunked `MDIA` encode → out-of-order reassembly with tamper
//! rejection.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use mod_adapter_core::custom_media::{
    encode_media_chunks, inspect_image_header, CanvasFacing, CustomMediaRegistry,
    DiskBackedMediaStore, MediaChunkReassembler, MediaErrorKind, MediaFormat, MediaPos,
    CanvasPlacement, MDIA_CHUNK_PAYLOAD, MAX_MEDIA_BYTES,
};
use mod_adapter_core::mod_sync::{digest_hex, sha256, sha256_hex};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Minimal structurally-valid PNG: signature + 13-byte IHDR + IEND.
/// CRC fields are zeroed — the registry validates the header, it does not
/// decode pixels.
fn png_bytes(width: u32, height: u32) -> Vec<u8> {
    let mut v = vec![0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];
    v.extend_from_slice(&13u32.to_be_bytes());
    v.extend_from_slice(b"IHDR");
    v.extend_from_slice(&width.to_be_bytes());
    v.extend_from_slice(&height.to_be_bytes());
    v.extend_from_slice(&[8, 2, 0, 0, 0]); // bit depth, truecolor, comp, filter, interlace
    v.extend_from_slice(&[0, 0, 0, 0]); // CRC (unchecked)
    v.extend_from_slice(&0u32.to_be_bytes());
    v.extend_from_slice(b"IEND");
    v.extend_from_slice(&[0xAE, 0x42, 0x60, 0x82]);
    v
}

/// Minimal structurally-valid baseline JPEG: SOI + APP0 + SOF0 + EOI.
fn jpeg_bytes(width: u16, height: u16) -> Vec<u8> {
    let mut v = vec![0xFF, 0xD8];
    // APP0 segment, length 16 (includes the 2 length bytes).
    v.extend_from_slice(&[0xFF, 0xE0, 0x00, 0x10]);
    v.extend_from_slice(b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0");
    // SOF0: length 11 = 2 len + 1 precision + 2 height + 2 width + 1 ncomp + 3 comp.
    v.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x0B, 0x08]);
    v.extend_from_slice(&height.to_be_bytes());
    v.extend_from_slice(&width.to_be_bytes());
    v.extend_from_slice(&[0x01, 0x01, 0x11, 0x00]); // 1 component, 4:4:4-ish sampling
    v.extend_from_slice(&[0xFF, 0xD9]);
    v
}

fn canvas_for(id: u64, sha: [u8; 32]) -> CanvasPlacement {
    CanvasPlacement {
        id,
        dim: "minecraft:overworld".to_string(),
        pos: MediaPos::new(10, 64, -20),
        facing: CanvasFacing::North,
        width_blocks: 2,
        height_blocks: 1,
        media_sha256: sha,
    }
}

// ---------------------------------------------------------------------------
// Scratch dirs for disk-backed store tests
// ---------------------------------------------------------------------------

static DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "mc-spine-media-{}-{}-{tag}",
            std::process::id(),
            DIR_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A valid PNG padded with inert trailing bytes to an exact size, so LRU
/// byte accounting is exercised deterministically.
fn sized_png(target: usize, pad: u8) -> Vec<u8> {
    let mut v = png_bytes(8, 8);
    assert!(v.len() < target, "fixture smaller than target");
    v.resize(target, pad);
    v
}

// ---------------------------------------------------------------------------
// Header inspection
// ---------------------------------------------------------------------------

#[test]
fn valid_png_and_jpeg_parse_exact_dimensions() {
    let png = png_bytes(320, 200);
    let (fmt, w, h) = inspect_image_header(&png).expect("valid PNG accepted");
    assert_eq!(fmt, MediaFormat::Png);
    assert_eq!((w, h), (320, 200));

    let jpg = jpeg_bytes(640, 480);
    let (fmt, w, h) = inspect_image_header(&jpg).expect("valid JPEG accepted");
    assert_eq!(fmt, MediaFormat::Jpeg);
    assert_eq!((w, h), (640, 480));
}

#[test]
fn hostile_payloads_rejected_fail_closed() {
    for (name, bytes) in [
        ("svg", b"<svg xmlns=\"http://www.w3.org/2000/svg\"><script>alert(1)</script></svg>".to_vec()),
        ("html", b"<!DOCTYPE html><html><body>hi</body></html>".to_vec()),
        ("script", b"#!/bin/sh\nrm -rf /\n".to_vec()),
        ("pe-exe", b"MZ\x90\x00\x03\x00\x00\x00".to_vec()),
        ("random", (0u8..=255).cycle().take(4096).collect::<Vec<u8>>()),
        ("empty", Vec::new()),
    ] {
        let err = inspect_image_header(&bytes).unwrap_err();
        assert_eq!(err.kind(), MediaErrorKind::UnsupportedFormat, "{name}");
    }
}

#[test]
fn oversized_and_zero_dimensions_rejected() {
    // 2048x2048 IHDR exceeds both the 1024px edge limit and the pixel cap.
    let err = inspect_image_header(&png_bytes(2048, 2048)).unwrap_err();
    assert_eq!(err.kind(), MediaErrorKind::InvalidDimensions);
    // Zero width.
    let err = inspect_image_header(&png_bytes(0, 10)).unwrap_err();
    assert_eq!(err.kind(), MediaErrorKind::InvalidDimensions);
    // Over pixel cap but under edge cap is impossible (1024*1024 == cap),
    // so check the boundary itself is accepted: 1024x1024.
    assert!(inspect_image_header(&png_bytes(1024, 1024)).is_ok());
}

#[test]
fn oversized_payload_rejected() {
    let mut big = png_bytes(1, 1);
    big.resize(MAX_MEDIA_BYTES + 1, 0xAB);
    let err = inspect_image_header(&big).unwrap_err();
    assert_eq!(err.kind(), MediaErrorKind::PayloadTooLarge);

    let mut reg = CustomMediaRegistry::new();
    let err = reg.register_asset("u", "t", &big).unwrap_err();
    assert_eq!(err.kind(), MediaErrorKind::PayloadTooLarge);
}

#[test]
fn truncated_jpeg_rejected() {
    let jpg = jpeg_bytes(100, 100);
    for cut in [2usize, 5, jpg.len() - 5] {
        let err = inspect_image_header(&jpg[..cut]).unwrap_err();
        assert_eq!(err.kind(), MediaErrorKind::UnsupportedFormat, "cut={cut}");
    }
    // SOI followed by garbage that is not a marker.
    let mut bad = vec![0xFF, 0xD8, 0x00, 0x11, 0x08];
    bad.extend_from_slice(&[0u8; 32]);
    assert!(inspect_image_header(&bad).is_err());
}

#[test]
fn hostile_names_rejected() {
    let mut reg = CustomMediaRegistry::new();
    let png = png_bytes(4, 4);
    for title in ["../evil", "a\u{7}b", ""] {
        let err = reg.register_asset("u", title, &png).unwrap_err();
        assert_eq!(err.kind(), MediaErrorKind::InvalidName, "title={title:?}");
    }
    let err = reg.register_asset("bad\u{1}name", "ok", &png).unwrap_err();
    assert_eq!(err.kind(), MediaErrorKind::InvalidName);
    assert_eq!(reg.asset_count(), 0);
}

// ---------------------------------------------------------------------------
// Registry: quota, dedup, canvas lifecycle
// ---------------------------------------------------------------------------

#[test]
fn register_dedups_by_sha256_across_uploaders() {
    let mut reg = CustomMediaRegistry::new();
    let png = png_bytes(8, 8);
    let m1 = reg.register_asset("alice", "first", &png).expect("alice upload");
    let m2 = reg.register_asset("bob", "second", &png).expect("bob dedup hit");
    assert_eq!(m1.sha256, m2.sha256);
    assert_eq!(m2.uploader, "alice", "dedup keeps first uploader");
    assert_eq!(reg.asset_count(), 1, "identical bytes store one blob");
}

#[test]
fn per_player_quota_enforced() {
    // Asset-count quota: one slot only.
    let mut reg = CustomMediaRegistry::with_quotas(1, u64::MAX);
    reg.register_asset("u", "one", &png_bytes(4, 4)).expect("first asset");
    let err = reg.register_asset("u", "two", &png_bytes(5, 5)).unwrap_err();
    assert_eq!(err.kind(), MediaErrorKind::QuotaExceeded);
    // A different uploader has an independent quota.
    reg.register_asset("v", "one", &png_bytes(6, 6)).expect("other uploader");

    // Byte quota: smallest fixture is ~45 bytes; cap below that.
    let mut reg = CustomMediaRegistry::with_quotas(32, 10);
    let err = reg.register_asset("u", "t", &png_bytes(4, 4)).unwrap_err();
    assert_eq!(err.kind(), MediaErrorKind::QuotaExceeded);
}

#[test]
fn canvas_lifecycle_and_missing_for_client() {
    let mut reg = CustomMediaRegistry::new();
    let png = png_bytes(16, 16);
    let meta = reg.register_asset("alice", "mural", &png).expect("register");

    // Unknown asset digest rejected.
    let mut bad = canvas_for(1, [0xAB; 32]);
    let err = reg.place_canvas(bad.clone()).unwrap_err();
    assert_eq!(err.kind(), MediaErrorKind::UnknownAsset);

    // Valid placement.
    reg.place_canvas(canvas_for(1, meta.sha256)).expect("place");
    // Duplicate id rejected.
    let err = reg.place_canvas(canvas_for(1, meta.sha256)).unwrap_err();
    assert_eq!(err.kind(), MediaErrorKind::CanvasConflict);
    // Zero extent rejected.
    bad.id = 2;
    bad.media_sha256 = meta.sha256;
    bad.width_blocks = 0;
    let err = reg.place_canvas(bad).unwrap_err();
    assert_eq!(err.kind(), MediaErrorKind::InvalidDimensions);

    // Client that already holds the digest needs nothing.
    let known: HashSet<[u8; 32]> = HashSet::from([meta.sha256]);
    assert!(reg.missing_for_client(&known).is_empty());

    // Client holding nothing needs the asset.
    let missing = reg.missing_for_client(&HashSet::new());
    assert_eq!(missing.len(), 1);
    assert_eq!(missing[0].sha256, meta.sha256);

    // Unplaced assets are not transferred: register a second asset with no
    // canvas and confirm it is absent from the missing set.
    let png2 = png_bytes(24, 24);
    let meta2 = reg.register_asset("alice", "spare", &png2).expect("register 2");
    let missing = reg.missing_for_client(&HashSet::new());
    assert_eq!(missing.len(), 1);
    assert_eq!(missing[0].sha256, meta.sha256);
    assert_ne!(missing[0].sha256, meta2.sha256);

    // Removal works and drops the asset from the sync set.
    assert!(reg.remove_canvas(1));
    assert!(!reg.remove_canvas(1));
    assert!(reg.missing_for_client(&HashSet::new()).is_empty());
    assert!(reg.canvas(1).is_none());
}

#[test]
fn missing_for_client_in_view_skips_out_of_view_and_other_dims() {
    let mut reg = CustomMediaRegistry::new();
    let near = reg.register_asset("u", "near", &png_bytes(8, 8)).unwrap();
    let far = reg.register_asset("u", "far", &png_bytes(16, 16)).unwrap();
    let other_dim = reg
        .register_asset("u", "other", &png_bytes(24, 24))
        .unwrap();

    // In view: pos (-10, 64, -20) -> chunk (-1, -2) via div_euclid(16).
    let mut c = canvas_for(1, near.sha256);
    c.pos = MediaPos::new(-10, 64, -20);
    reg.place_canvas(c).unwrap();
    // Same dimension but ~64 chunks away in z -> outside view distance 1.
    let mut c = canvas_for(2, far.sha256);
    c.pos = MediaPos::new(-10, 64, 1000);
    reg.place_canvas(c).unwrap();
    // Same spot as the near canvas but a different dimension -> excluded.
    let mut c = canvas_for(3, other_dim.sha256);
    c.dim = "minecraft:the_nether".to_string();
    c.pos = MediaPos::new(-10, 64, -20);
    reg.place_canvas(c).unwrap();

    // Client chunk (0, -2), view distance 1 chunk: only the near painting
    // is inside the Chebyshev window ((-1,-2) vs (0,-2): dx=1, dz=0).
    let missing = reg.missing_for_client_in_view(
        "minecraft:overworld",
        0,
        -2,
        1,
        &HashSet::new(),
    );
    assert_eq!(missing.len(), 1);
    assert_eq!(missing[0].sha256, near.sha256);

    // The unfiltered variant still sees every placed canvas's assets.
    assert_eq!(reg.missing_for_client(&HashSet::new()).len(), 3);

    // A client already holding the in-view digest needs nothing, and a
    // client in a different dimension sees none of these canvases.
    let known = HashSet::from([near.sha256]);
    assert!(
        reg.missing_for_client_in_view("minecraft:overworld", 0, -2, 1, &known)
            .is_empty()
    );
    assert!(
        reg.missing_for_client_in_view("minecraft:the_end", 0, -2, 64, &HashSet::new())
            .is_empty()
    );

    // View distance 0 still matches the client's own chunk: from client
    // chunk (-1, -2) the near canvas and a second far-asset canvas placed
    // in that same chunk are in view; the distant far canvas is not.
    let mut same_chunk = canvas_for(4, far.sha256);
    same_chunk.pos = MediaPos::new(-9, 64, -21); // chunk (-1, -2)
    reg.place_canvas(same_chunk).unwrap();
    let missing = reg.missing_for_client_in_view(
        "minecraft:overworld",
        -1,
        -2,
        0,
        &HashSet::new(),
    );
    assert_eq!(missing.len(), 2);
    assert!(missing.iter().any(|m| m.sha256 == near.sha256));
    assert!(missing.iter().any(|m| m.sha256 == far.sha256));
}

// ---------------------------------------------------------------------------
// MDIA chunked wire framing
// ---------------------------------------------------------------------------

/// Build a valid PNG padded past one 16 KiB chunk so multi-chunk framing
/// is exercised (header-only validation makes trailing bytes inert).
fn chunkable_png() -> Vec<u8> {
    let mut v = png_bytes(64, 48);
    v.resize(MDIA_CHUNK_PAYLOAD * 2 + 100, 0x5A);
    v
}

#[test]
fn mdia_encode_reassemble_out_of_order() {
    let bytes = chunkable_png();
    let digest = sha256(&bytes);
    let frames = encode_media_chunks(&digest, &bytes).expect("encode");
    assert_eq!(frames.len(), 3, "payload spans three 16 KiB chunks");
    for f in &frames {
        assert_eq!(&f[..4], b"MDIA");
        assert!(f.len() <= 4 + 32 + 2 + 2 + 2 + MDIA_CHUNK_PAYLOAD);
    }

    // Feed strictly out of order: last, first, middle.
    let mut re = MediaChunkReassembler::with_expected_sha256(digest);
    assert_eq!(re.push(&frames[2]).unwrap(), false);
    assert_eq!(re.push(&frames[0]).unwrap(), false);
    assert_eq!(re.push(&frames[1]).unwrap(), true, "final chunk completes");
    assert!(re.is_complete());
    assert_eq!(re.assembled_bytes().expect("assembled"), bytes.as_slice());
}

#[test]
fn mdia_rejects_tampered_and_malformed_chunks() {
    let bytes = chunkable_png();
    let digest = sha256(&bytes);
    let frames = encode_media_chunks(&digest, &bytes).expect("encode");

    // Tampered payload byte -> SHA-256 mismatch at completion.
    let mut tampered = frames.clone();
    let last = tampered.len() - 1;
    let n = tampered[last].len();
    tampered[last][n - 1] ^= 0xFF;
    let mut re = MediaChunkReassembler::new();
    for f in &tampered {
        if let Err(e) = re.push(f) {
            assert_eq!(e.kind(), MediaErrorKind::DigestMismatch);
            return;
        }
    }
    panic!("tampered stream unexpectedly accepted");
}

#[test]
fn mdia_rejects_hash_mismatch_duplicate_and_bad_magic() {
    let bytes = chunkable_png();
    let digest = sha256(&bytes);
    let frames = encode_media_chunks(&digest, &bytes).expect("encode");

    // Chunk carrying a different asset hash -> ChunkMismatch.
    let mut re = MediaChunkReassembler::with_expected_sha256([0x11; 32]);
    let err = re.push(&frames[0]).unwrap_err();
    assert_eq!(err.kind(), MediaErrorKind::ChunkMismatch);

    // Duplicate index rejected.
    let mut re = MediaChunkReassembler::new();
    re.push(&frames[0]).unwrap();
    let err = re.push(&frames[0]).unwrap_err();
    assert_eq!(err.kind(), MediaErrorKind::DuplicateChunk);

    // Bad magic rejected.
    let mut re = MediaChunkReassembler::new();
    let mut bad = frames[0].clone();
    bad[0] = b'X';
    let err = re.push(&bad).unwrap_err();
    assert_eq!(err.kind(), MediaErrorKind::MalformedChunk);

    // Out-of-range index rejected (idx == total).
    let mut re = MediaChunkReassembler::new();
    let mut oob = frames[0].clone();
    // total_chunks field at offset 38, set idx=total=3 → idx(2) total(2):
    // rewrite idx to 3 while total stays 3.
    oob[36..38].copy_from_slice(&3u16.to_be_bytes());
    let err = re.push(&oob).unwrap_err();
    assert_eq!(err.kind(), MediaErrorKind::MalformedChunk);

    // Non-final chunk with truncated payload rejected.
    let mut re = MediaChunkReassembler::new();
    let mut short = frames[0].clone();
    // Declared payload_len smaller than actual → length mismatch first;
    // instead truncate the frame and fix payload_len to match, making it
    // a structurally-valid but non-full non-final chunk.
    let keep = MDIA_CHUNK_PAYLOAD / 2;
    short.truncate(4 + 32 + 2 + 2 + 2 + keep);
    short[40..42].copy_from_slice(&(keep as u16).to_be_bytes());
    let err = re.push(&short).unwrap_err();
    assert_eq!(err.kind(), MediaErrorKind::MalformedChunk);
}

#[test]
fn mdia_rejects_oversized_total_chunks() {
    // total_chunks = 33 exceeds MAX_TOTAL_CHUNKS even though idx < total.
    let bytes = chunkable_png();
    let digest = sha256(&bytes);
    let frames = encode_media_chunks(&digest, &bytes).expect("encode");
    let mut re = MediaChunkReassembler::new();
    let mut f = frames[0].clone();
    f[38..40].copy_from_slice(&33u16.to_be_bytes());
    // idx 0 < 33 so it reaches the total bound check.
    let err = re.push(&f).unwrap_err();
    assert_eq!(err.kind(), MediaErrorKind::MalformedChunk);
}

#[test]
fn encode_rejects_empty_and_oversized() {
    let digest = [0u8; 32];
    assert!(encode_media_chunks(&digest, &[]).is_err());
    let big = vec![0u8; MAX_MEDIA_BYTES + 1];
    assert!(encode_media_chunks(&digest, &big).is_err());
}

// ---------------------------------------------------------------------------
// DiskBackedMediaStore: bounded hot-RAM LRU over durable blobs
// ---------------------------------------------------------------------------

#[test]
fn disk_store_lru_cap_bounds_ram_and_disk_stays_durable() {
    let dir = TestDir::new("store-lru");
    let root = dir.path("blobs");
    let mut store = DiskBackedMediaStore::with_ram_cap(&root, 200).expect("open store");

    // Three distinct 100-byte images against a 200-byte RAM budget.
    let img_a = sized_png(100, 0x11);
    let img_b = sized_png(100, 0x22);
    let img_c = sized_png(100, 0x33);
    let a = store.put_bytes(&img_a).expect("put a");
    let b = store.put_bytes(&img_b).expect("put b");
    let c = store.put_bytes(&img_c).expect("put c");
    assert_ne!(a, b);
    assert_ne!(b, c);

    // RAM stayed within budget; every blob is durable on disk regardless.
    assert!(store.cached_ram_bytes() <= 200);
    assert_eq!(store.cached_ram_bytes(), 200, "two 100B blobs fit the cap");
    for (digest, img) in [(a, &img_a), (b, &img_b), (c, &img_c)] {
        let disk = root.join(format!("{}.bin", sha256_hex(img)));
        assert_eq!(disk.file_name().unwrap().to_str().unwrap(),
            format!("{}.bin", digest_hex(&digest)));
        assert!(store.contains(&digest));
    }

    // The LRU-evicted blob reloads transparently from disk with identical
    // bytes, and the reload itself is promoted within the byte budget.
    for digest in [a, b, c, a] {
        let got = store.get_bytes(&digest).expect("get").expect("present");
        let want = match digest {
            d if d == a => &img_a,
            d if d == b => &img_b,
            _ => &img_c,
        };
        assert_eq!(&got, want);
        assert!(store.cached_ram_bytes() <= 200);
    }

    // Unknown digest: absent, not an error.
    assert!(store.get_bytes(&[0xEE; 32]).unwrap().is_none());
}

#[test]
fn disk_store_rejects_oversize_and_non_image_payloads() {
    let dir = TestDir::new("store-validate");
    let mut store = DiskBackedMediaStore::new(dir.path("blobs")).expect("open store");
    assert!(store.put_bytes(b"").is_err());
    let err = store.put_bytes(b"<svg>nope</svg>").unwrap_err();
    assert_eq!(err.kind(), MediaErrorKind::UnsupportedFormat);
    let mut big = png_bytes(1, 1);
    big.resize(MAX_MEDIA_BYTES + 1, 0x00);
    let err = store.put_bytes(&big).unwrap_err();
    assert_eq!(err.kind(), MediaErrorKind::PayloadTooLarge);
    assert_eq!(store.cached_ram_bytes(), 0);
}

#[test]
fn disk_store_fails_closed_on_tampered_disk_blob() {
    let dir = TestDir::new("store-tamper");
    let root = dir.path("blobs");
    let mut store = DiskBackedMediaStore::with_ram_cap(&root, 200).expect("open store");

    let img_a = sized_png(100, 0x11);
    let img_b = sized_png(100, 0x22);
    let img_c = sized_png(100, 0x33);
    let a = store.put_bytes(&img_a).unwrap();
    let b = store.put_bytes(&img_b).unwrap();
    store.put_bytes(&img_c).unwrap();
    // Cap 200 / 3x100B: `a` is the LRU eviction — served only from disk.
    assert_eq!(store.cached_count(), 2);
    assert!(store.cached_ram_bytes() <= 200);

    // Tamper with the durable copy: the SHA-256 re-verify must refuse it.
    let path = root.join(format!("{}.bin", sha256_hex(&img_a)));
    std::fs::write(&path, b"forged bytes, same length-ish").unwrap();
    let err = store.get_bytes(&a).unwrap_err();
    assert_eq!(err.kind(), MediaErrorKind::DigestMismatch);
    // Hot-cached siblings are unaffected.
    assert_eq!(
        store.get_bytes(&b).unwrap().unwrap(),
        img_b,
        "untampered hot blob still serves"
    );
}

#[cfg(unix)]
#[test]
fn disk_store_refuses_symlinked_blob_paths() {
    let dir = TestDir::new("store-symlink");
    let root = dir.path("blobs");
    let mut store = DiskBackedMediaStore::new(&root).expect("open store");

    // A digest whose "blob" is actually a symlink must fail closed, never
    // follow the link.
    let ghost = [0xEEu8; 32];
    let ghost_path = root.join(format!("{}.bin", digest_hex(&ghost)));
    std::os::unix::fs::symlink(dir.path("elsewhere"), &ghost_path).unwrap();
    let err = store.get_bytes(&ghost).unwrap_err();
    assert_eq!(err.kind(), MediaErrorKind::Storage);
    assert!(!store.contains(&ghost));

    // A symlinked store root is rejected at open time.
    let real = dir.path("real-root");
    std::fs::create_dir_all(&real).unwrap();
    let link = dir.path("link-root");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let err = DiskBackedMediaStore::new(&link).unwrap_err();
    assert_eq!(err.kind(), MediaErrorKind::Storage);
}
