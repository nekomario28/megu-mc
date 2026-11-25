// SPDX-License-Identifier: LGPL-3.0-or-later
use megu_state_store::lz4_region::{
    lz4_compress_block, lz4_decompress_block, Lz4Error, Lz4Region, MAX_CHUNK_BYTES,
    MAX_REGION_DECOMPRESSED_BYTES, MLZ4_MAGIC, MLZ4_VERSION, REGION_CHUNK_COUNT,
};
use std::collections::BTreeMap;
use std::fs;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

fn random_bytes(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}

fn assert_roundtrip(input: &[u8]) {
    let compressed = lz4_compress_block(input);
    let decoded = lz4_decompress_block(&compressed, input.len()).expect("decompress");
    assert!(
        decoded == input,
        "roundtrip mismatch: input={}, output={}",
        input.len(),
        decoded.len()
    );
}

fn compressed_zero_block(len: usize) -> Vec<u8> {
    assert!(len >= 5, "test zero blocks need at least one match");
    let match_len = len - 1;
    let match_code = match_len - 4;
    let mut out = Vec::new();
    out.push(0x10 | match_code.min(15) as u8);
    out.push(0);
    out.extend_from_slice(&1u16.to_le_bytes());
    if match_code >= 15 {
        let mut remaining = match_code - 15;
        while remaining >= 255 {
            out.push(255);
            remaining -= 255;
        }
        out.push(remaining as u8);
    }
    out.push(0);
    out
}

fn crc32_zeros(mut len: usize) -> u32 {
    let zeros = [0u8; 64 * 1024];
    let mut crc = 0;
    while len > 0 {
        let take = len.min(zeros.len());
        crc = megu_core::hash::crc32_ieee_update(crc, &zeros[..take]);
        len -= take;
    }
    crc
}

fn zero_chunk_region_archive(lengths: &[usize]) -> Vec<u8> {
    let table_end = 8 + REGION_CHUNK_COUNT * 20;
    let mut archive = vec![0u8; table_end];
    archive[0..4].copy_from_slice(MLZ4_MAGIC);
    archive[4..6].copy_from_slice(&MLZ4_VERSION.to_le_bytes());
    archive[6..8].copy_from_slice(&(REGION_CHUNK_COUNT as u16).to_le_bytes());
    for (slot, &len) in lengths.iter().enumerate() {
        let compressed = compressed_zero_block(len);
        let offset = archive.len() as u64;
        let entry = 8 + slot * 20;
        archive[entry..entry + 8].copy_from_slice(&offset.to_le_bytes());
        archive[entry + 8..entry + 12].copy_from_slice(&(compressed.len() as u32).to_le_bytes());
        archive[entry + 12..entry + 16].copy_from_slice(&(len as u32).to_le_bytes());
        archive[entry + 16..entry + 20].copy_from_slice(&crc32_zeros(len).to_le_bytes());
        archive.extend_from_slice(&compressed);
    }
    archive
}

#[test]
fn block_codec_roundtrips_empty_tiny_random_repetitive_and_megabyte_inputs() {
    assert_roundtrip(&[]);
    for tiny in [
        b"a".as_slice(),
        b"abc",
        b"abcd",
        b"a a a a",
        &[0, 1, 2, 3, 4],
    ] {
        assert_roundtrip(tiny);
    }
    assert_roundtrip(&random_bytes(128 * 1024, 0x4d45_4755));

    // A Minecraft-like section buffer: repeated palette ids, packed light,
    // and a mostly stable block-state index pattern.
    let mut section = Vec::with_capacity(4096 * 4 + 2048);
    for i in 0..4096u32 {
        let palette_index = if i % 97 == 0 { i % 13 } else { 1 };
        section.extend_from_slice(&palette_index.to_le_bytes());
    }
    section.extend((0..2048).map(|i| ((i / 128) % 16) as u8));
    assert_roundtrip(&section);
    assert_roundtrip(&vec![0x5a; 1024 * 1024]);
}

#[test]
fn decompressor_rejects_truncation_bad_offsets_and_bombs_without_panicking() {
    let malformed: Vec<Vec<u8>> = vec![
        vec![],
        vec![0xf0],
        vec![0xf0, 0xff],
        vec![0x00, 0x00],
        vec![0x00, 0x00, 0x00],
        vec![0x10, b'x', 0x00, 0x00],
        vec![0x1f, b'x', 1, 0, 255, 255, 255, 255, 255, 255],
    ];
    for bytes in malformed {
        let result = std::panic::catch_unwind(|| lz4_decompress_block(&bytes, 64));
        assert!(result.is_ok(), "decompress panicked for {bytes:?}");
        assert!(
            result.unwrap().is_err(),
            "accepted malformed block {bytes:?}"
        );
    }

    // Mutate, truncate and synthesize 256 hostile inputs. Every call must
    // return normally; inputs derived from the valid block are expected to
    // fail unless the mutation happens to preserve a valid block encoding.
    let valid = lz4_compress_block(&vec![b'Q'; 8192]);
    let mut checked_errors = 0;
    for mutation in 0..256usize {
        let mut bytes = valid.clone();
        match mutation % 4 {
            0 => bytes.truncate(mutation % bytes.len()),
            1 => {
                let index = mutation % bytes.len();
                bytes[index] ^= (mutation as u8).wrapping_add(1);
            }
            2 => bytes = vec![0x00, 0x00, 0x00], // zero offset
            _ => bytes = vec![0x1f, b'Z', 1, 0, 255, 255, 255, 255, 255], // expansion bomb
        }
        let result = std::panic::catch_unwind(|| lz4_decompress_block(&bytes, 32));
        assert!(result.is_ok(), "decompress panicked at mutation {mutation}");
        if result.unwrap().is_err() {
            checked_errors += 1;
        }
    }
    assert!(
        checked_errors >= 200,
        "only {checked_errors} hostile inputs rejected"
    );
    assert!(matches!(
        lz4_decompress_block(&[0x10, b'x', 5, 0], 64),
        Err(Lz4Error::InvalidOffset(5))
    ));
    assert!(matches!(
        lz4_decompress_block(&[0x10, b'x', 1, 0], 4),
        Err(Lz4Error::OutputLimit { .. })
    ));
}

#[test]
fn region_roundtrip_full_capacity_partial_updates_and_anvil_payload_bytes() {
    let mut chunks = BTreeMap::new();
    for slot in 0..REGION_CHUNK_COUNT {
        let len = 30 + slot % 71;
        chunks.insert(slot, random_bytes(len, slot as u64 + 7));
    }
    // Raw Anvil chunk record bytes (compression marker/length/NBT payload)
    // are treated opaquely and survive migration byte-for-byte.
    let anvil_chunk_record = vec![2, 0, 0, 0, 8, 10, 0, 0, 0, 0, 0, 0, 0];
    chunks.insert(17, anvil_chunk_record.clone());
    let mut region = Lz4Region::from_chunk_map(chunks.clone()).unwrap();
    let encoded = region.encode().unwrap();
    assert_eq!(&encoded[..4], MLZ4_MAGIC);
    assert!(encoded.len() < REGION_CHUNK_COUNT * 4096);
    let decoded = Lz4Region::decode(&encoded).unwrap();
    assert_eq!(decoded.to_chunk_map(), chunks);
    assert_eq!(decoded.chunk(17), Some(anvil_chunk_record.as_slice()));

    region.remove_chunk(0);
    region.set_chunk(1023, b"partial update".to_vec()).unwrap();
    let updated = Lz4Region::decode(&region.encode().unwrap()).unwrap();
    assert_eq!(updated.chunk(0), None);
    assert_eq!(updated.chunk(1023), Some(b"partial update".as_slice()));
}

#[test]
fn region_save_is_atomic_and_detects_crc_corruption() {
    let mut chunks = BTreeMap::new();
    chunks.insert(0, b"valuable chunk bytes".to_vec());
    let region = Lz4Region::from_chunk_map(chunks).unwrap();
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("mlz4-{}-{suffix}.region", std::process::id()));
    region.save_atomic(&path).unwrap();
    assert_eq!(Lz4Region::load(&path).unwrap(), region);
    assert!(!path
        .with_file_name(format!(
            "{}.tmp",
            path.file_name().unwrap().to_string_lossy()
        ))
        .exists());

    let mut corrupt = fs::read(&path).unwrap();
    let decoded = Lz4Region::decode(&corrupt).unwrap();
    assert_eq!(decoded.chunk(0), Some(b"valuable chunk bytes".as_slice()));
    let payload_offset = u64::from_le_bytes(corrupt[8..16].try_into().unwrap()) as usize;
    *corrupt.last_mut().unwrap() ^= 0x80;
    assert!(payload_offset >= 8 + REGION_CHUNK_COUNT * 20);
    assert!(matches!(
        Lz4Region::decode(&corrupt),
        Err(Lz4Error::ChecksumMismatch { slot: 0, .. }) | Err(Lz4Error::Truncated(_))
    ));
    fs::remove_file(path).unwrap();
}

#[test]
fn region_decode_rejects_multi_chunk_aggregate_bomb_at_default_cap() {
    let probe = compressed_zero_block(1024);
    assert_eq!(
        lz4_decompress_block(&probe, 1024).unwrap(),
        vec![0u8; 1024],
        "test archive compressor must emit valid LZ4 blocks"
    );

    let archive = zero_chunk_region_archive(&[MAX_CHUNK_BYTES, MAX_CHUNK_BYTES - 4, 5]);
    assert_eq!(
        MAX_CHUNK_BYTES * 2 + 1,
        MAX_REGION_DECOMPRESSED_BYTES + 1,
        "test archive must cross the default aggregate budget by one byte"
    );
    assert!(matches!(
        Lz4Region::decode(&archive),
        Err(Lz4Error::AggregateSizeLimitExceeded {
            limit: MAX_REGION_DECOMPRESSED_BYTES,
            attempted,
        }) if attempted == MAX_REGION_DECOMPRESSED_BYTES + 1
    ));
}

#[test]
fn aggregate_budget_fails_before_decoding_overflowing_chunk() {
    let mut archive = zero_chunk_region_archive(&[1024, 1024]);
    let second_entry = 8 + 20;
    let second_start =
        u64::from_le_bytes(archive[second_entry..second_entry + 8].try_into().unwrap()) as usize;
    let second_len = u32::from_le_bytes(
        archive[second_entry + 8..second_entry + 12]
            .try_into()
            .unwrap(),
    ) as usize;
    archive[second_start..second_start + second_len].fill(0xff);

    assert!(matches!(
        Lz4Region::read_all_with_budget(&archive, 1024),
        Err(Lz4Error::AggregateSizeLimitExceeded {
            limit: 1024,
            attempted: 2048,
        })
    ));
}

#[test]
fn budget_helpers_enforce_custom_bounds_and_exact_boundary() {
    let payload_len = 64 * 1024;
    let exact_budget = payload_len * 2;
    let mut chunks = BTreeMap::new();
    chunks.insert(0, vec![0x11; payload_len]);
    chunks.insert(1, vec![0x22; payload_len]);
    let region = Lz4Region::from_chunk_map(chunks).unwrap();
    let encoded = region.encode().unwrap();

    let decoded = Lz4Region::read_all_with_budget(&encoded, exact_budget).unwrap();
    assert_eq!(decoded.chunk(0), Some(vec![0x11; payload_len].as_slice()));
    assert_eq!(decoded.chunk(1), Some(vec![0x22; payload_len].as_slice()));
    assert!(matches!(
        Lz4Region::decode_with_budget(&encoded, exact_budget - 1),
        Err(Lz4Error::AggregateSizeLimitExceeded {
            limit,
            attempted,
        }) if limit == exact_budget - 1 && attempted == exact_budget
    ));

    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "mlz4-budget-{}-{suffix}.region",
        std::process::id()
    ));
    fs::write(&path, &encoded).unwrap();
    assert!(Lz4Region::open_with_budget(&path, exact_budget).is_ok());
    assert!(matches!(
        Lz4Region::load_with_budget(&path, exact_budget - 1),
        Err(Lz4Error::AggregateSizeLimitExceeded { .. })
    ));
    fs::remove_file(path).unwrap();
}

#[test]
fn codec_reports_measured_encode_decode_throughput() {
    let input = vec![0x3c; 4 * 1024 * 1024];
    let start = Instant::now();
    let compressed = lz4_compress_block(&input);
    let encode_seconds = start.elapsed().as_secs_f64();
    let start = Instant::now();
    let output = lz4_decompress_block(&compressed, input.len()).unwrap();
    let decode_seconds = start.elapsed().as_secs_f64();
    assert!(output == input, "throughput roundtrip mismatch");
    let mib = input.len() as f64 / (1024.0 * 1024.0);
    eprintln!(
        "MLZ4 throughput: encode {:.1} MiB/s, decode {:.1} MiB/s ({} -> {} bytes)",
        mib / encode_seconds,
        mib / decode_seconds,
        input.len(),
        compressed.len()
    );
}
