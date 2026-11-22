// SPDX-License-Identifier: LGPL-3.0-or-later

use mc_spine_state_store::chunk_packet_cache::{ChunkPacketCache, ChunkPacketKey};
use std::sync::Arc;

fn key(dim: u8, cx: i32, cz: i32, compression_threshold: i32) -> ChunkPacketKey {
    ChunkPacketKey {
        dim,
        cx,
        cz,
        compression_threshold,
    }
}

fn populate(cache: &mut ChunkPacketCache, key: ChunkPacketKey, bytes: usize) {
    let (frame, hit) = cache.get_or_encode(key, || vec![key.dim; bytes]);
    assert!(!hit);
    assert_eq!(frame.len(), bytes);
}

#[test]
fn cache_hit_reuses_identical_arc_without_encoding_again() {
    let mut cache = ChunkPacketCache::default();
    let packet_key = key(0, -1, 2, 64);
    let (first, hit) = cache.get_or_encode(packet_key, || vec![0x27, 1, 2, 3]);
    assert!(!hit);

    let (second, hit) = cache.get_or_encode(packet_key, || -> Vec<u8> {
        panic!("encoder must not run for a hit")
    });
    assert!(hit);
    assert_eq!(first.as_ref(), second.as_ref());
    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(cache.stats().hits, 1);
    assert_eq!(cache.stats().misses, 1);
    assert_eq!(cache.stats().bytes_used, 4);
}

#[test]
fn interior_bump_invalidates_only_its_chunk_across_thresholds() {
    let mut cache = ChunkPacketCache::default();
    for threshold in [-1, 64, 256] {
        populate(&mut cache, key(0, -1, 0, threshold), 4);
    }
    populate(&mut cache, key(0, 0, 0, 64), 4);
    populate(&mut cache, key(1, -1, 0, 64), 4);

    assert_eq!(cache.bump_block(0, -8, 8), 1);
    assert_eq!(cache.len(), 2);
    assert_eq!(cache.stats().invalidations, 3);

    let (_, hit) = cache.get_or_encode(key(0, -1, 0, -1), || vec![1]);
    assert!(!hit);
    let (_, hit) = cache.get_or_encode(key(0, 0, 0, 64), || vec![1]);
    assert!(hit);
    let (_, hit) = cache.get_or_encode(key(1, -1, 0, 64), || vec![1]);
    assert!(hit);
    assert_eq!(cache.bump_chunk(0, -1, 0), 2);
}

#[test]
fn block_seams_invalidate_both_sides_including_negative_coordinates() {
    let mut cache = ChunkPacketCache::default();
    populate(&mut cache, key(0, 0, 0, 64), 4);
    populate(&mut cache, key(0, 1, 0, 64), 4);
    populate(&mut cache, key(0, 2, 0, 64), 4);
    cache.bump_block(0, 15, 8);
    assert_eq!(cache.len(), 1);
    assert_eq!(cache.stats().invalidations, 2);

    populate(&mut cache, key(0, -1, -1, 64), 4);
    populate(&mut cache, key(0, -2, -1, 64), 4);
    cache.bump_block(0, -16, -8);
    assert_eq!(cache.len(), 1);
    assert_eq!(cache.stats().invalidations, 4);
}

#[test]
fn corner_edit_invalidates_diagonal_and_axis_neighbors() {
    let mut cache = ChunkPacketCache::default();
    for (cx, cz) in [(0, 0), (-1, 0), (0, -1), (-1, -1), (1, 1)] {
        populate(&mut cache, key(0, cx, cz, 64), 3);
    }
    cache.bump_block(0, 0, 0);
    assert_eq!(cache.len(), 1);
    assert_eq!(cache.stats().invalidations, 4);
}

#[test]
fn lru_evicts_least_recent_entry_and_respects_both_caps() {
    let mut cache = ChunkPacketCache::new(2, 6);
    let a = key(0, 0, 0, 64);
    let b = key(0, 1, 0, 64);
    let c = key(0, 2, 0, 64);
    populate(&mut cache, a, 3);
    populate(&mut cache, b, 3);
    assert!(cache.get_or_encode(a, || vec![0]).1);
    populate(&mut cache, c, 3);
    assert_eq!(cache.len(), 2);
    assert_eq!(cache.bytes_used(), 6);
    assert_eq!(cache.stats().evictions, 1);
    assert!(cache.get_or_encode(a, || vec![0]).1);
    assert!(!cache.get_or_encode(b, || vec![0]).1);
    assert!(cache.len() <= 2);
    assert!(cache.bytes_used() <= 6);

    let mut bytes_only = ChunkPacketCache::new(4, 5);
    populate(&mut bytes_only, a, 3);
    populate(&mut bytes_only, b, 3);
    assert_eq!(bytes_only.len(), 1);
    assert_eq!(bytes_only.bytes_used(), 3);

    let mut entries_only = ChunkPacketCache::new(2, 100);
    populate(&mut entries_only, a, 3);
    populate(&mut entries_only, b, 3);
    populate(&mut entries_only, c, 3);
    assert_eq!(entries_only.len(), 2);
    assert_eq!(entries_only.bytes_used(), 6);
    assert_eq!(entries_only.stats().evictions, 1);
}

#[test]
fn oversized_frame_is_returned_without_eviction_or_storage() {
    let mut cache = ChunkPacketCache::new(2, 5);
    let a = key(0, 0, 0, 64);
    let huge = key(0, 1, 0, 64);
    populate(&mut cache, a, 4);
    let (frame, hit) = cache.get_or_encode(huge, || vec![0x27; 6]);
    assert!(!hit);
    assert_eq!(frame.len(), 6);
    assert_eq!(cache.len(), 1);
    assert_eq!(cache.bytes_used(), 4);
    assert_eq!(cache.stats().evictions, 0);
    assert!(cache.get_or_encode(a, || vec![1]).1);
    assert!(!cache.get_or_encode(huge, || vec![0x27; 6]).1);

    let mut disabled = ChunkPacketCache::new(0, 32);
    populate(&mut disabled, a, 2);
    assert!(disabled.is_empty());
    assert_eq!(disabled.bytes_used(), 0);
}

#[test]
fn explicit_chunk_bump_covers_block_entities_and_lighting() {
    let mut cache = ChunkPacketCache::default();
    for threshold in [-1, 64, 256] {
        populate(&mut cache, key(2, 3, 4, threshold), 5);
    }
    assert_eq!(cache.bump_chunk(2, 3, 4), 1);
    assert_eq!(cache.len(), 0);
    assert_eq!(cache.stats().invalidations, 3);

    let (_, first_hit) = cache.get_or_encode(key(2, 3, 4, 64), || vec![0x27]);
    assert!(!first_hit);
    assert_eq!(cache.bump_chunk(2, 3, 4), 2);
    let (_, second_hit) = cache.get_or_encode(key(2, 3, 4, 64), || vec![0x28]);
    assert!(!second_hit);
}
