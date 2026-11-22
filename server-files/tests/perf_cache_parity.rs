// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Zero-side-effect parity proofs for `perf_cache`: every cache must return
//! byte-identical results to the uncached path, invalidate on every
//! discriminator change, and fail closed on corrupt input — never panic,
//! never serve stale bytes.

use megu_core::version::TargetVersion;
use megu_server_files::{
    look_dir_from_yaw, CacheLookup, CacheMissReason, ChunkWireKey, DirectionalChunkQueue,
    SharedChunkWireCache, StartupBinaryCache, StartupInputFile, StartupInputFingerprint,
    CACHE_SCHEMA_VERSION, STARTUP_CACHE_MAGIC,
};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

static DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A per-test scratch dir under the system temp dir (TMPDIR-honoring).
struct TestDir(PathBuf);

impl TestDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "megu-perf-cache-{}-{}-{tag}",
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

fn stray_temps(dir: &TestDir) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(&dir.0)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".tmp"))
        .collect();
    names.sort();
    names
}

// ---------------------------------------------------------------------------
// SharedChunkWireCache
// ---------------------------------------------------------------------------

fn key(dimension: &str, cx: i32, cz: i32, revision: u64, target: TargetVersion) -> ChunkWireKey {
    ChunkWireKey {
        dimension: dimension.to_owned(),
        cx,
        cz,
        chunk_revision: revision,
        anti_xray_enabled: false,
        compression_threshold: 256,
        protocol_version: target.protocol(),
    }
}

/// Deterministic stand-in for "encode + compress the chunk packet": the
/// bytes only depend on the key, like a real encoder's output.
fn fake_encode(k: &ChunkWireKey) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(k.dimension.as_bytes());
    out.extend_from_slice(&k.cx.to_be_bytes());
    out.extend_from_slice(&k.cz.to_be_bytes());
    out.extend_from_slice(&k.chunk_revision.to_be_bytes());
    out.push(k.anti_xray_enabled as u8);
    out.extend_from_slice(&k.compression_threshold.to_be_bytes());
    out.extend_from_slice(&k.protocol_version.to_be_bytes());
    out
}

#[test]
fn fifty_players_in_one_base_share_a_single_encoded_buffer() {
    let mut cache = SharedChunkWireCache::new(1 << 20);
    let k = key("minecraft:overworld", 3, -7, 42, TargetVersion::V1_21_1);
    let uncached = fake_encode(&k);

    let mut encodes = 0u32;
    let mut first: Option<Arc<[u8]>> = None;
    for player in 0..50 {
        let packet = cache.get_or_insert_with(k.clone(), || {
            encodes += 1;
            fake_encode(&k)
        });
        // Byte-identical to the uncached encode: the cache adds nothing.
        assert_eq!(&packet[..], &uncached[..], "player {player} packet bytes");
        if let Some(first) = &first {
            // One buffer, zero per-player copies or re-compression.
            assert!(
                Arc::ptr_eq(first, &packet),
                "player {player} shares the buffer"
            );
        } else {
            first = Some(packet);
        }
    }
    assert_eq!(encodes, 1, "encode ran once for 50 sends");

    let stats = cache.stats();
    assert_eq!(stats.misses, 1);
    assert_eq!(stats.hits, 49);
    assert_eq!(stats.evictions, 0);
    assert_eq!(stats.resident_entries, 1);
    assert_eq!(stats.resident_bytes, uncached.len());
}

#[test]
fn every_key_discriminator_forces_a_fresh_encode() {
    let mut cache = SharedChunkWireCache::new(1 << 20);
    let base = key("minecraft:overworld", 0, 0, 7, TargetVersion::V1_21_1);
    let mut encodes = 0u32;
    let mut encode = |k: &ChunkWireKey| {
        encodes += 1;
        fake_encode(k)
    };

    let a = cache.get_or_insert_with(base.clone(), || encode(&base));

    // A block/light/block-entity edit bumps the revision: new key, new miss.
    let mut edited = base.clone();
    edited.chunk_revision += 1;
    let b = cache.get_or_insert_with(edited.clone(), || encode(&edited));
    assert!(!Arc::ptr_eq(&a, &b));
    assert_ne!(&a[..], &b[..]);

    // Flipping anti-xray must never serve the unobfuscated (or vice versa)
    // packet: that would leak ores or strip protection.
    let mut xray = base.clone();
    xray.anti_xray_enabled = true;
    let c = cache.get_or_insert_with(xray.clone(), || encode(&xray));
    assert_ne!(&a[..], &c[..]);

    // Compression threshold is part of the wire framing.
    let mut uncompressed = base.clone();
    uncompressed.compression_threshold = -1;
    let d = cache.get_or_insert_with(uncompressed.clone(), || encode(&uncompressed));
    assert_ne!(&a[..], &d[..]);

    // A 767 client and a 777 client in the same base never share a packet.
    let mut newer = base.clone();
    newer.protocol_version = TargetVersion::V26_3.protocol();
    let e = cache.get_or_insert_with(newer.clone(), || encode(&newer));
    assert_ne!(&a[..], &e[..]);

    assert_eq!(encodes, 5);
    let stats = cache.stats();
    assert_eq!(stats.misses, 5);
    assert_eq!(stats.hits, 0);
    assert_eq!(stats.resident_entries, 5);

    // Zero cross-mode leakage: every variant still hits its own buffer.
    for variant in [base, edited, xray, uncompressed, newer] {
        let cached = cache.get(&variant).unwrap();
        assert_eq!(&cached[..], &fake_encode(&variant)[..]);
    }
    assert_eq!(cache.stats().hits, 5);
}

#[test]
fn invalidate_chunk_drops_every_mode_of_that_chunk_only() {
    let mut cache = SharedChunkWireCache::new(1 << 20);
    let mut keys = Vec::new();
    for rev in 0..2u64 {
        for protocol in [767, 777] {
            let mut k = key("minecraft:overworld", 5, 5, rev, TargetVersion::V1_21_1);
            k.protocol_version = protocol;
            k.anti_xray_enabled = rev == 1;
            cache.get_or_insert_with(k.clone(), || fake_encode(&k));
            keys.push(k);
        }
    }
    // A neighbor chunk and another dimension at the same coords stay cached.
    let neighbor = key("minecraft:overworld", 6, 5, 0, TargetVersion::V1_21_1);
    cache.get_or_insert_with(neighbor.clone(), || fake_encode(&neighbor));
    let mut same_coords_other_dim = neighbor.clone();
    same_coords_other_dim.dimension = "minecraft:the_nether".to_owned();
    same_coords_other_dim.cx = 5;
    cache.get_or_insert_with(same_coords_other_dim.clone(), || {
        fake_encode(&same_coords_other_dim)
    });

    assert_eq!(cache.invalidate_chunk("minecraft:overworld", 5, 5), 4);
    assert_eq!(cache.len(), 2);
    for k in &keys {
        assert!(cache.get(k).is_none(), "invalidated key still cached");
    }
    assert!(cache.get(&neighbor).is_some());
    assert!(cache.get(&same_coords_other_dim).is_some());

    // Invalidating what is not there is a no-op.
    assert_eq!(cache.invalidate_chunk("minecraft:overworld", 5, 5), 0);
}

#[test]
fn max_bytes_lru_evicts_oldest_and_keeps_the_cap() {
    // Each fake packet for these keys is ~30 bytes; cap holds three.
    let packet_len =
        fake_encode(&key("minecraft:overworld", 0, 0, 0, TargetVersion::V1_21_1)).len();
    let mut cache = SharedChunkWireCache::new(packet_len * 3);

    let keys: Vec<ChunkWireKey> = (0..5)
        .map(|i| key("minecraft:overworld", i, 0, 0, TargetVersion::V1_21_1))
        .collect();
    for k in &keys {
        let k2 = k.clone();
        cache.get_or_insert_with(k.clone(), || fake_encode(&k2));
    }

    let stats = cache.stats();
    assert!(stats.resident_bytes <= packet_len * 3);
    assert_eq!(stats.resident_entries, 3);
    assert_eq!(stats.evictions, 2);
    // Oldest two evicted; newest three resident.
    assert!(cache.get(&keys[0]).is_none());
    assert!(cache.get(&keys[1]).is_none());
    assert!(cache.get(&keys[2]).is_some());
    assert!(cache.get(&keys[3]).is_some());
    assert!(cache.get(&keys[4]).is_some());
}

#[test]
fn lru_hit_protects_hot_entry_from_eviction() {
    let packet_len =
        fake_encode(&key("minecraft:overworld", 0, 0, 0, TargetVersion::V1_21_1)).len();
    let mut cache = SharedChunkWireCache::new(packet_len * 2);
    let hot = key("minecraft:overworld", 0, 0, 0, TargetVersion::V1_21_1);
    cache.get_or_insert_with(hot.clone(), || fake_encode(&hot));
    let cold1 = key("minecraft:overworld", 1, 0, 0, TargetVersion::V1_21_1);
    cache.get_or_insert_with(cold1.clone(), || fake_encode(&cold1));

    // Refresh the oldest entry so it is no longer least-recently-used.
    assert!(cache.get(&hot).is_some());

    let cold2 = key("minecraft:overworld", 2, 0, 0, TargetVersion::V1_21_1);
    cache.get_or_insert_with(cold2.clone(), || fake_encode(&cold2));

    assert!(cache.get(&hot).is_some(), "recently hit entry evicted");
    assert!(
        cache.get(&cold1).is_none(),
        "LRU victim was not the coldest"
    );
    assert!(cache.get(&cold2).is_some());
    assert_eq!(cache.stats().evictions, 1);
}

#[test]
fn oversized_packet_is_served_but_never_stored() {
    let mut cache = SharedChunkWireCache::new(8);
    let k = key("minecraft:overworld", 0, 0, 0, TargetVersion::V1_21_1);
    assert!(fake_encode(&k).len() > 8);

    let a = cache.get_or_insert_with(k.clone(), || fake_encode(&k));
    let b = cache.get_or_insert_with(k.clone(), || fake_encode(&k));
    // The caller still gets the correct bytes both times...
    assert_eq!(&a[..], &fake_encode(&k)[..]);
    assert_eq!(&b[..], &a[..]);
    // ...but nothing was retained, so the cap can never be exceeded.
    assert_eq!(cache.len(), 0);
    assert_eq!(cache.stats().resident_bytes, 0);
    assert_eq!(cache.stats().misses, 2);
}

// ---------------------------------------------------------------------------
// DirectionalChunkQueue
// ---------------------------------------------------------------------------

fn vanilla_square(pcx: i32, pcz: i32, d: u8) -> HashSet<(i32, i32)> {
    let d = i32::from(d);
    let mut set = HashSet::new();
    for dz in -d..=d {
        for dx in -d..=d {
            set.insert((pcx + dx, pcz + dz));
        }
    }
    set
}

#[test]
fn queue_emits_exactly_the_vanilla_square_with_zero_drops() {
    for d in [0u8, 2, 4, 8, 12] {
        let queue = DirectionalChunkQueue::new(100, -64, 0.4, 0.9, d);
        let emitted: Vec<(i32, i32)> = queue.collect();
        let expected = vanilla_square(100, -64, d);
        let side = 2 * usize::from(d) + 1;

        assert_eq!(emitted.len(), side * side, "d={d} count");
        let unique: HashSet<(i32, i32)> = emitted.iter().copied().collect();
        assert_eq!(unique.len(), emitted.len(), "d={d} duplicate chunks");
        assert_eq!(unique, expected, "d={d} emitted set != vanilla square");
    }
}

#[test]
fn own_chunk_and_ring_one_send_first() {
    let d = 8u8;
    let (pcx, pcz) = (10, 20);
    let queue = DirectionalChunkQueue::new(pcx, pcz, 0.0, 1.0, d);
    let emitted = queue.ordered().to_vec();

    assert_eq!(emitted[0], (pcx, pcz), "own chunk is not first");
    // All of ring 1 (Chebyshev <= 1) precedes every ring-2+ chunk.
    let ring1: HashSet<(i32, i32)> = (1..=8).map(|i| emitted[i]).collect();
    let mut ring1_expected = vanilla_square(pcx, pcz, 1);
    ring1_expected.remove(&(pcx, pcz));
    assert_eq!(ring1, ring1_expected);
    for chunk in &emitted[9..] {
        let dx = chunk.0 - pcx;
        let dz = chunk.1 - pcz;
        assert!(
            dx.abs() > 1 || dz.abs() > 1,
            "ring-1 chunk {chunk:?} leaked past position 9"
        );
    }
    // Squared distance is monotonically non-decreasing across the emission.
    let mut last = 0i64;
    for chunk in &emitted {
        let dx = i64::from(chunk.0 - pcx);
        let dz = i64::from(chunk.1 - pcz);
        let dist2 = dx * dx + dz * dz;
        assert!(dist2 >= last, "distance order regressed at {chunk:?}");
        last = dist2;
    }
}

#[test]
fn facing_forward_streams_ahead_chunks_before_behind_at_same_radius() {
    let d = 8u8;
    let (pcx, pcz) = (0, 0);
    let emitted = DirectionalChunkQueue::new(pcx, pcz, 0.0, 1.0, d)
        .ordered()
        .to_vec();
    let pos = |c: (i32, i32)| emitted.iter().position(|&p| p == c).unwrap();

    // Facing +Z: for every radius, the chunk dead ahead precedes the one
    // dead behind, and every ahead-row chunk precedes its mirror behind.
    for r in 1..=i32::from(d) {
        assert!(
            pos((pcx, pcz + r)) < pos((pcx, pcz - r)),
            "radius {r}: ahead chunk does not precede behind chunk"
        );
        for dx in 1..=r {
            assert!(
                pos((pcx + dx, pcz + r)) < pos((pcx + dx, pcz - r)),
                "radius {r} dx {dx}: ahead flank not before behind flank"
            );
        }
    }

    // Facing -X flips the axis: ahead is now west.
    let emitted = DirectionalChunkQueue::new(pcx, pcz, -1.0, 0.0, d)
        .ordered()
        .to_vec();
    let pos = |c: (i32, i32)| emitted.iter().position(|&p| p == c).unwrap();
    for r in 1..=i32::from(d) {
        assert!(pos((pcx - r, pcz)) < pos((pcx + r, pcz)));
    }
}

#[test]
fn yaw_mapping_matches_vanilla_and_drives_the_queue() {
    // Vanilla: yaw 0 -> +Z (south), 90 -> -X (west), 180 -> -Z, 270 -> +X.
    for (yaw, want) in [
        (0.0f64, (0.0f64, 1.0f64)),
        (90.0, (-1.0, 0.0)),
        (180.0, (0.0, -1.0)),
        (270.0, (1.0, 0.0)),
        (-90.0, (1.0, 0.0)),
        (360.0, (0.0, 1.0)),
    ] {
        let (x, z) = look_dir_from_yaw(yaw);
        assert!(
            (x - want.0).abs() < 1e-9 && (z - want.1).abs() < 1e-9,
            "yaw {yaw}"
        );
    }

    // yaw 0 must order like an explicit +Z look vector.
    let a = DirectionalChunkQueue::from_yaw(4, -9, 0.0, 4)
        .ordered()
        .to_vec();
    let b = DirectionalChunkQueue::new(4, -9, 0.0, 1.0, 4)
        .ordered()
        .to_vec();
    assert_eq!(a, b);
}

#[test]
fn degenerate_look_vector_still_covers_the_whole_square_deterministically() {
    for look in [
        (0.0, 0.0),
        (f64::NAN, 0.5),
        (f64::INFINITY, f64::NEG_INFINITY),
    ] {
        let emitted: Vec<(i32, i32)> =
            DirectionalChunkQueue::new(1, 2, look.0, look.1, 4).collect();
        let unique: HashSet<(i32, i32)> = emitted.iter().copied().collect();
        assert_eq!(unique, vanilla_square(1, 2, 4), "look {look:?}");
        // Same inputs => identical order, twice.
        let again: Vec<(i32, i32)> = DirectionalChunkQueue::new(1, 2, look.0, look.1, 4).collect();
        assert_eq!(emitted, again);
    }
}

// ---------------------------------------------------------------------------
// StartupBinaryCache
// ---------------------------------------------------------------------------

fn packs() -> Vec<String> {
    vec![
        "vanilla".to_owned(),
        "file/megu-base.zip".to_owned(),
        "file/extra-tweaks.zip".to_owned(),
    ]
}

fn input_files() -> Vec<StartupInputFile> {
    vec![
        StartupInputFile::from_contents(
            "world/datapacks/megu-base/data/mc/recipes/ingot.json",
            b"{\"type\":\"minecraft:crafting_shaped\"}",
        ),
        StartupInputFile::from_contents(
            "world/datapacks/megu-base/pack.mcmeta",
            b"{\"pack\":{\"pack_format\":48}}",
        ),
        StartupInputFile::from_contents("server.properties", b"view-distance=10\n"),
    ]
}

fn fingerprint_for(target: TargetVersion, files: &[StartupInputFile]) -> StartupInputFingerprint {
    StartupBinaryCache::compute_fingerprint(
        target.data_version(),
        target.protocol(),
        &packs(),
        files,
    )
}

#[test]
fn identical_inputs_hit_with_byte_identical_payload() {
    let dir = TestDir::new("hit");
    let path = dir.path("startup.sbc1");
    let fp = fingerprint_for(TargetVersion::V1_21_1, &input_files());
    let payload: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();

    // First boot: the file does not exist — a miss, not an error.
    match StartupBinaryCache::load_if_fresh(&path, &fp) {
        CacheLookup::Miss(CacheMissReason::Io(std::io::ErrorKind::NotFound)) => {}
        other => panic!("expected Io(NotFound) miss, got {other:?}"),
    }

    StartupBinaryCache::save(&path, &fp, &payload).unwrap();
    assert_eq!(
        stray_temps(&dir),
        Vec::<String>::new(),
        "atomic write leaked a temp"
    );

    for round in 0..3 {
        match StartupBinaryCache::load_if_fresh(&path, &fp) {
            CacheLookup::Hit(bytes) => {
                assert_eq!(bytes, payload, "round {round}: payload bytes differ")
            }
            other => panic!("round {round}: expected Hit, got {other:?}"),
        }
    }
}

#[test]
fn one_byte_edit_in_one_input_file_invalidates() {
    let dir = TestDir::new("edit");
    let path = dir.path("startup.sbc1");
    let files = input_files();
    let fp = fingerprint_for(TargetVersion::V1_21_1, &files);
    StartupBinaryCache::save(&path, &fp, b"registry-bytes").unwrap();

    let mut edited = files.clone();
    let mut bytes = b"{\"type\":\"minecraft:crafting_shaped\"}".to_vec();
    bytes[10] = b'x';
    edited[0] = StartupInputFile::from_contents(&files[0].relative_path, &bytes);
    let fp2 = fingerprint_for(TargetVersion::V1_21_1, &edited);
    assert_ne!(fp, fp2);

    match StartupBinaryCache::load_if_fresh(&path, &fp2) {
        CacheLookup::Miss(CacheMissReason::FingerprintMismatch) => {}
        other => panic!("expected FingerprintMismatch, got {other:?}"),
    }
    // The original fingerprint still hits: the cache binds to its inputs.
    assert!(matches!(
        StartupBinaryCache::load_if_fresh(&path, &fp),
        CacheLookup::Hit(_)
    ));
}

#[test]
fn pack_order_change_invalidates_even_with_identical_files() {
    let dir = TestDir::new("packs");
    let path = dir.path("startup.sbc1");
    let files = input_files();
    let fp = fingerprint_for(TargetVersion::V1_21_1, &files);
    StartupBinaryCache::save(&path, &fp, b"payload").unwrap();

    // `/datapack enable ... before` flips override precedence: same names,
    // same files, different semantics => must miss.
    let reordered = vec![packs()[0].clone(), packs()[2].clone(), packs()[1].clone()];
    let fp2 = StartupBinaryCache::compute_fingerprint(
        TargetVersion::V1_21_1.data_version(),
        TargetVersion::V1_21_1.protocol(),
        &reordered,
        &files,
    );
    assert_ne!(fp, fp2);
    match StartupBinaryCache::load_if_fresh(&path, &fp2) {
        CacheLookup::Miss(CacheMissReason::FingerprintMismatch) => {}
        other => panic!("expected FingerprintMismatch, got {other:?}"),
    }

    // Enumerating files in a different order must NOT change the fingerprint.
    let mut shuffled = files.clone();
    shuffled.swap(0, 2);
    let fp3 = fingerprint_for(TargetVersion::V1_21_1, &shuffled);
    assert_eq!(fp, fp3, "file enumeration order leaked into fingerprint");
}

#[test]
fn data_or_protocol_version_change_invalidates() {
    let dir = TestDir::new("version");
    let path = dir.path("startup.sbc1");
    let files = input_files();
    let fp_1211 = fingerprint_for(TargetVersion::V1_21_1, &files);
    StartupBinaryCache::save(&path, &fp_1211, b"payload").unwrap();

    // Data version 3955 -> 5023 (protocol 767 -> 777): a new game version
    // must never load the old build's registries.
    let fp_263 = fingerprint_for(TargetVersion::V26_3, &files);
    assert_ne!(fp_1211, fp_263);
    match StartupBinaryCache::load_if_fresh(&path, &fp_263) {
        CacheLookup::Miss(CacheMissReason::FingerprintMismatch) => {}
        other => panic!("expected FingerprintMismatch, got {other:?}"),
    }

    // Same data version but a protocol bump also misses.
    let fp_proto_only = StartupBinaryCache::compute_fingerprint(
        TargetVersion::V1_21_1.data_version(),
        TargetVersion::V26_3.protocol(),
        &packs(),
        &files,
    );
    match StartupBinaryCache::load_if_fresh(&path, &fp_proto_only) {
        CacheLookup::Miss(CacheMissReason::FingerprintMismatch) => {}
        other => panic!("expected FingerprintMismatch, got {other:?}"),
    }
}

#[test]
fn every_single_bit_flip_in_the_envelope_is_a_clean_miss() {
    let dir = TestDir::new("bits");
    let path = dir.path("startup.sbc1");
    let fp = fingerprint_for(TargetVersion::V1_21_1, &input_files());
    let payload: Vec<u8> = (0..257u32).map(|i| (i * 31 % 256) as u8).collect();
    StartupBinaryCache::save(&path, &fp, &payload).unwrap();
    let golden = std::fs::read(&path).unwrap();
    assert_eq!(&golden[..4], &STARTUP_CACHE_MAGIC);

    // Flip every bit of every byte: magic, schema, fingerprint, length,
    // payload and CRC alike must all be caught.
    for byte in 0..golden.len() {
        for bit in 0..8 {
            let mut corrupted = golden.clone();
            corrupted[byte] ^= 1 << bit;
            std::fs::write(&path, &corrupted).unwrap();
            match StartupBinaryCache::load_if_fresh(&path, &fp) {
                CacheLookup::Miss(CacheMissReason::Corrupt) => {}
                other => {
                    panic!("bit flip at byte {byte} bit {bit} was not a Corrupt miss: {other:?}")
                }
            }
        }
    }

    // The untouched envelope still hits afterwards.
    std::fs::write(&path, &golden).unwrap();
    match StartupBinaryCache::load_if_fresh(&path, &fp) {
        CacheLookup::Hit(bytes) => assert_eq!(bytes, payload),
        other => panic!("golden envelope no longer hits: {other:?}"),
    }
}

#[test]
fn truncated_and_wrong_schema_files_fail_closed() {
    let dir = TestDir::new("corrupt");
    let path = dir.path("startup.sbc1");
    let fp = fingerprint_for(TargetVersion::V1_21_1, &input_files());
    StartupBinaryCache::save(&path, &fp, b"payload-123").unwrap();
    let golden = std::fs::read(&path).unwrap();

    // Every truncation length is a miss, never a panic or a partial read.
    for cut in 0..golden.len() {
        std::fs::write(&path, &golden[..cut]).unwrap();
        match StartupBinaryCache::load_if_fresh(&path, &fp) {
            CacheLookup::Miss(CacheMissReason::Corrupt) => {}
            other => panic!("truncation to {cut} bytes gave {other:?}"),
        }
    }

    // An intact envelope under a future schema reports its real reason.
    let foreign = StartupBinaryCache::encode_envelope(CACHE_SCHEMA_VERSION + 1, &fp, b"x");
    std::fs::write(&path, &foreign).unwrap();
    match StartupBinaryCache::load_if_fresh(&path, &fp) {
        CacheLookup::Miss(CacheMissReason::SchemaMismatch { found }) => {
            assert_eq!(found, CACHE_SCHEMA_VERSION + 1)
        }
        other => panic!("expected SchemaMismatch, got {other:?}"),
    }
}
