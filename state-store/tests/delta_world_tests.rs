// SPDX-License-Identifier: LGPL-3.0-or-later
//! ALPHA6-S1-DELTA-WORLD-SAVE integration tests: zero-byte base chunks,
//! compact invertible deltas, tombstone preservation, immutable time-travel
//! generations, corruption/epoch fail-closed behavior, atomic relative-path
//! persistence, and pin-aware blob GC.

use megu_state_store::delta_world::{
    apply_chunk_delta, compute_chunk_delta, decode_chunk_delta, encode_chunk_delta,
    invert_chunk_delta, BlockDeltaEdit, ChunkDelta, ChunkStorage, CommitGenerationOptions,
    DeltaError, DeltaGenerationStore, DeltaStoreError, UnknownBlockEntry,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

static DIR_COUNTER: AtomicU64 = AtomicU64::new(0);
static CWD_LOCK: Mutex<()> = Mutex::new(());

struct TestDir(PathBuf);

impl TestDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "megu-a6s1-delta-{}-{}-{tag}",
            std::process::id(),
            DIR_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(std::fs::canonicalize(&dir).unwrap())
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

fn identified_delta(
    base: &[u32],
    current: &[u32],
    epoch: &str,
    dim_id: i32,
    cx: i32,
    cz: i32,
) -> ChunkDelta {
    compute_chunk_delta(base, current).with_identity(epoch, dim_id, cx, cz)
}

fn commit_options(
    seed: u64,
    epoch: &str,
    label: &str,
    tick: u64,
    chunks: Vec<ChunkDelta>,
) -> CommitGenerationOptions {
    CommitGenerationOptions::new(seed, epoch, label, tick, chunks)
}

fn files_under(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if !root.exists() {
        return out;
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let file_type = entry.file_type().unwrap();
            if file_type.is_dir() {
                stack.push(entry.path());
            } else if file_type.is_file() {
                out.push(entry.path());
            }
        }
    }
    out.sort();
    out
}

fn blob_files(store_root: &Path) -> Vec<PathBuf> {
    files_under(&store_root.join("blobs"))
}

fn flip_byte(path: &Path, offset: usize) {
    let mut bytes = std::fs::read(path).unwrap();
    assert!(offset < bytes.len());
    bytes[offset] ^= 0x01;
    std::fs::write(path, bytes).unwrap();
}

#[test]
fn unchanged_chunk_is_zero_delta_bytes_and_apply_is_identity() {
    let base = vec![42u32; 4096];
    let current = base.clone();
    let delta = compute_chunk_delta(&base, &current).with_identity("epoch-a", 0, 3, -4);

    assert!(delta.is_zero_bytes());
    assert_eq!(delta.stored_delta_bytes(), 0);
    assert!(delta.edits.is_empty());
    assert_eq!(apply_chunk_delta(&base, &delta).unwrap(), base);

    let inverted = invert_chunk_delta(&delta);
    assert!(inverted.is_zero_bytes());
    assert_eq!(apply_chunk_delta(&base, &inverted).unwrap(), base);
}

#[test]
fn sparse_edits_are_dramatically_smaller_than_full_state_storage() {
    let base = vec![7u32; 4096];
    let mut current = base.clone();
    current[17] = 900;
    current[409] = 901;
    current[4000] = 902;
    let delta = identified_delta(&base, &current, "epoch-a", 0, 0, 0);

    let encoded = encode_chunk_delta(&delta);
    let decoded = decode_chunk_delta(&encoded).unwrap();
    assert_eq!(decoded, delta);
    assert!(
        encoded.len() < 128,
        "three sparse edits should encode in a small header-plus-triples payload, got {} bytes",
        encoded.len()
    );
    assert!(
        encoded.len() * 100 < base.len() * 4,
        "sparse encoding must be dramatically smaller than full u32 state storage"
    );
}

#[test]
fn contiguous_edits_use_run_encoding_when_it_is_beneficial() {
    let base = vec![0u32; 4096];
    let mut contiguous = base.clone();
    for (index, slot) in contiguous.iter_mut().enumerate().take(164).skip(100) {
        *slot = 1000 + index as u32;
    }
    let mut scattered = base.clone();
    for index in (100..228).step_by(2) {
        scattered[index] = 1000 + index as u32;
    }
    let contiguous_delta = identified_delta(&base, &contiguous, "epoch-a", 0, 0, 0);
    let scattered_delta = identified_delta(&base, &scattered, "epoch-a", 0, 0, 0);
    assert_eq!(contiguous_delta.edits.len(), scattered_delta.edits.len());

    let contiguous_len = encode_chunk_delta(&contiguous_delta).len();
    let scattered_len = encode_chunk_delta(&scattered_delta).len();
    let expected_saving = 2 * contiguous_delta.edits.len() - 8;
    assert!(
        contiguous_len + expected_saving == scattered_len,
        "one contiguous run should save exactly two bytes per edit minus run overhead \
         (contiguous {contiguous_len}, scattered {scattered_len}, saving {expected_saving})"
    );
    assert_eq!(
        decode_chunk_delta(&encode_chunk_delta(&contiguous_delta)).unwrap(),
        contiguous_delta
    );
}

#[test]
fn full_index_space_contiguous_run_round_trips_without_length_overflow() {
    let base = vec![0u32; 65_536];
    let current: Vec<u32> = (0..65_536u32).map(|index| index + 1).collect();
    let delta = identified_delta(&base, &current, "epoch-a", 0, 0, 0);
    assert_eq!(delta.edits.len(), 65_536);

    let encoded = encode_chunk_delta(&delta);
    let decoded = decode_chunk_delta(&encoded).unwrap();
    assert_eq!(decoded, delta);
    assert_eq!(apply_chunk_delta(&base, &decoded).unwrap(), current);
}

#[test]
fn apply_and_invert_round_trip_exactly() {
    let base: Vec<u32> = (0..4096u32).map(|index| index % 17).collect();
    let mut current = base.clone();
    current[0] = 10_000;
    current[512] = 20_000;
    current[4095] = 30_000;
    let delta = identified_delta(&base, &current, "epoch-a", -2, 8, 9);

    let forward = apply_chunk_delta(&base, &delta).unwrap();
    assert_eq!(forward, current);

    let inverse = invert_chunk_delta(&delta);
    let rolled_back = apply_chunk_delta(&forward, &inverse).unwrap();
    assert_eq!(rolled_back, base);

    let encoded = encode_chunk_delta(&inverse);
    assert_eq!(decode_chunk_delta(&encoded).unwrap(), inverse);
}

#[test]
fn apply_fails_closed_when_base_preimage_does_not_match() {
    let base = vec![1u32; 16];
    let mut current = base.clone();
    current[3] = 9;
    let mut delta = identified_delta(&base, &current, "epoch-a", 0, 0, 0);
    delta.edits[0].base_state = 2;

    assert!(matches!(
        apply_chunk_delta(&base, &delta),
        Err(DeltaError::BaseStateMismatch {
            local_idx: 3,
            expected_base: 2,
            actual_base: 1,
        })
    ));
}

#[test]
fn unknown_tombstone_entries_survive_codec_and_store_round_trips() {
    let base = vec![0u32; 4096];
    let mut current = base.clone();
    current[64] = 77;
    let mut delta = identified_delta(&base, &current, "epoch-a", 0, -1, 2);
    let tombstone = UnknownBlockEntry {
        local_idx: 64,
        namespaced_id: "create:shaft".to_string(),
        properties_json: r#"{"axis":"x","waterlogged":"false"}"#.to_string(),
        nbt_bytes: vec![0x0a, 0x00, 0x00, 0x08, 0x00, 0x03, b'm', b'o', b'd', 0x00],
    };
    delta.set_unknown_block(tombstone.clone());

    let decoded = decode_chunk_delta(&encode_chunk_delta(&delta)).unwrap();
    assert_eq!(decoded.unknown_blocks, vec![tombstone.clone()]);
    assert_eq!(decoded.unknown_blocks[0].namespaced_id, "create:shaft");
    assert_eq!(apply_chunk_delta(&base, &decoded).unwrap(), current);

    let inverted = invert_chunk_delta(&decoded);
    assert_eq!(inverted.unknown_blocks, vec![tombstone.clone()]);

    let dir = TestDir::new("tombstone-store");
    let root = dir.path("store");
    let mut store = DeltaGenerationStore::open(&root).unwrap();
    let generation = store
        .commit_generation(commit_options(
            77,
            "epoch-a",
            "pre-mod-install",
            10,
            vec![delta],
        ))
        .unwrap();
    drop(store);

    let reopened = DeltaGenerationStore::open(&root).unwrap();
    let checkout = reopened.checkout_generation(generation.gen_id).unwrap();
    assert_eq!(checkout.chunks.len(), 1);
    assert_eq!(checkout.chunks[0].unknown_blocks, vec![tombstone]);
    assert_eq!(
        apply_chunk_delta(&base, &checkout.chunks[0]).unwrap(),
        current
    );
}

#[test]
fn unchanged_chunk_committed_to_store_writes_no_delta_blob() {
    let dir = TestDir::new("zero-byte-store");
    let root = dir.path("store");
    let base = vec![5u32; 4096];
    let delta = identified_delta(&base, &base, "epoch-a", 0, 0, 0);
    let mut store = DeltaGenerationStore::open(&root).unwrap();
    let generation = store
        .commit_generation(commit_options(99, "epoch-a", "initial", 1, vec![delta]))
        .unwrap();

    assert_eq!(generation.chunks.len(), 1);
    assert_eq!(
        generation.chunks[0].storage,
        ChunkStorage::DeterministicBase
    );
    assert_eq!(generation.chunks[0].encoded_len, 0);
    assert!(generation.chunks[0].is_zero_bytes());
    assert!(
        blob_files(&root).is_empty(),
        "a deterministic-base chunk must not allocate any delta blob: {:?}",
        blob_files(&root)
    );

    let checkout = store.checkout_generation(generation.gen_id).unwrap();
    assert_eq!(checkout.chunks.len(), 1);
    assert!(checkout.chunks[0].is_zero_bytes());
}

#[test]
fn multi_generation_time_travel_branch_diff_and_merkle_determinism_are_exact() {
    let dir = TestDir::new("time-travel");
    let root = dir.path("store");
    let mut store = DeltaGenerationStore::open(&root).unwrap();
    let base = vec![0u32; 4096];

    let mut state1 = base.clone();
    state1[10] = 100;
    state1[11] = 101;
    let delta1 = identified_delta(&base, &state1, "epoch-a", 0, 0, 0);
    let options1 = commit_options(12345, "epoch-a", "pre-boss", 100, vec![delta1]);
    let gen1 = store.commit_generation(options1.clone()).unwrap();

    // Recommitting byte-identical content is a deterministic no-op: same id,
    // same Merkle root, same content-addressed generation.
    let duplicate = store.commit_generation(options1).unwrap();
    assert_eq!(duplicate.gen_id, gen1.gen_id);
    assert_eq!(duplicate.merkle_root, gen1.merkle_root);

    let mut state2 = state1.clone();
    state2[11] = 0; // reverted to deterministic base
    state2[20] = 200;
    let delta2 = identified_delta(&base, &state2, "epoch-a", 0, 0, 0);
    let gen2 = store
        .commit_generation(
            commit_options(12345, "epoch-a", "post-boss", 200, vec![delta2])
                .with_parent(gen1.gen_id),
        )
        .unwrap();

    let mut state3 = state2.clone();
    state3[30] = 300;
    let delta3 = identified_delta(&base, &state3, "epoch-a", 0, 0, 0);
    let gen3 = store
        .commit_generation(
            commit_options(12345, "epoch-a", "post-build", 300, vec![delta3])
                .with_parent(gen2.gen_id),
        )
        .unwrap();

    assert_eq!(gen2.parent_gen, Some(gen1.gen_id));
    assert_eq!(gen3.parent_gen, Some(gen2.gen_id));
    assert_ne!(gen1.merkle_root, gen2.merkle_root);
    assert_ne!(gen2.merkle_root, gen3.merkle_root);

    // Time-travel to gen 1 reconstructs its exact overlay even though gen 3 is
    // the active head in the same epoch.
    let checkout1 = store.checkout_generation(gen1.gen_id).unwrap();
    assert_eq!(checkout1.record, gen1);
    assert_eq!(checkout1.chunks.len(), 1);
    assert_eq!(
        apply_chunk_delta(&base, &checkout1.chunks[0]).unwrap(),
        state1
    );

    // Branch gen 4 directly from gen 1.
    let mut state4 = state1.clone();
    state4[11] = 105;
    state4[40] = 400;
    let delta4 = identified_delta(&base, &state4, "epoch-a", 0, 0, 0);
    let gen4 = store
        .commit_generation(
            commit_options(12345, "epoch-a", "branch-experiment", 400, vec![delta4])
                .with_parent(gen1.gen_id),
        )
        .unwrap();
    assert_eq!(gen4.parent_gen, Some(gen1.gen_id));

    let diff = store.diff_generations(gen1.gen_id, gen4.gen_id).unwrap();
    assert_eq!(diff.chunks.len(), 1);
    assert_eq!(
        diff.chunks[0].block_diffs,
        vec![
            megu_state_store::delta_world::BlockStateDiff {
                local_idx: 11,
                generation_a_state: 101,
                generation_b_state: 105,
            },
            megu_state_store::delta_world::BlockStateDiff {
                local_idx: 40,
                generation_a_state: 0,
                generation_b_state: 400,
            },
        ]
    );
    assert!(diff.chunks[0].unknown_diffs.is_empty());
    assert!(store
        .diff_generations(gen1.gen_id, gen1.gen_id)
        .unwrap()
        .is_empty());

    drop(store);
    let reopened = DeltaGenerationStore::open(&root).unwrap();
    assert_eq!(
        reopened.generation(gen1.gen_id).unwrap().merkle_root,
        gen1.merkle_root
    );
    assert_eq!(
        reopened.generation(gen4.gen_id).unwrap().parent_gen,
        Some(gen1.gen_id)
    );
    let checkout4 = reopened.checkout_generation(gen4.gen_id).unwrap();
    assert_eq!(
        apply_chunk_delta(&base, &checkout4.chunks[0]).unwrap(),
        state4
    );
}

#[test]
fn generation_diff_reports_unknown_tombstone_changes_exactly() {
    let dir = TestDir::new("unknown-diff");
    let root = dir.path("store");
    let base = vec![0u32; 64];
    let mut current = base.clone();
    current[4] = 44;
    let mut delta1 = identified_delta(&base, &current, "epoch-a", 0, 0, 0);
    let tombstone = UnknownBlockEntry {
        local_idx: 4,
        namespaced_id: "example:removed_machine".to_string(),
        properties_json: r#"{"facing":"north"}"#.to_string(),
        nbt_bytes: b"opaque-nbt-v1".to_vec(),
    };
    delta1.set_unknown_block(tombstone.clone());
    let delta2 = identified_delta(&base, &current, "epoch-a", 0, 0, 0);

    let mut store = DeltaGenerationStore::open(&root).unwrap();
    let gen1 = store
        .commit_generation(commit_options(
            1,
            "epoch-a",
            "with-tombstone",
            1,
            vec![delta1],
        ))
        .unwrap();
    let gen2 = store
        .commit_generation(
            commit_options(1, "epoch-a", "without-tombstone", 2, vec![delta2])
                .with_parent(gen1.gen_id),
        )
        .unwrap();

    let diff = store.diff_generations(gen1.gen_id, gen2.gen_id).unwrap();
    assert!(diff.chunks[0].block_diffs.is_empty());
    assert_eq!(diff.chunks[0].unknown_diffs.len(), 1);
    assert_eq!(
        diff.chunks[0].unknown_diffs[0].generation_a,
        Some(tombstone)
    );
    assert_eq!(diff.chunks[0].unknown_diffs[0].generation_b, None);
}

#[test]
fn blob_and_manifest_corruption_are_rejected_fail_closed() {
    let dir = TestDir::new("corruption");
    let root = dir.path("store");
    let base = vec![0u32; 4096];
    let mut current = base.clone();
    current[9] = 909;
    let delta = identified_delta(&base, &current, "epoch-a", 0, 0, 0);
    let mut store = DeltaGenerationStore::open(&root).unwrap();
    let generation = store
        .commit_generation(commit_options(
            5,
            "epoch-a",
            "integrity",
            1,
            vec![delta.clone()],
        ))
        .unwrap();

    // The standalone codec's CRC32 rejects a payload bit flip before any field
    // is trusted.
    let mut encoded = encode_chunk_delta(&delta);
    let payload_offset = encoded.len() - 5;
    encoded[payload_offset] ^= 0x01;
    assert!(matches!(
        decode_chunk_delta(&encoded),
        Err(DeltaError::CrcMismatch { .. })
    ));

    // In the content-addressed store the same bit flip is caught by the
    // SHA-256 object identity before decode.
    let blobs = blob_files(&root);
    assert_eq!(blobs.len(), 1);
    flip_byte(&blobs[0], payload_offset.min(encoded.len() - 5));
    assert!(matches!(
        store.checkout_generation(generation.gen_id),
        Err(DeltaStoreError::BlobIntegrity { .. })
    ));

    // A bit flip in the atomic generation index is CRC-rejected on open.
    let dir2 = TestDir::new("manifest-corruption");
    let root2 = dir2.path("store");
    let mut store2 = DeltaGenerationStore::open(&root2).unwrap();
    store2
        .commit_generation(commit_options(5, "epoch-a", "integrity", 1, vec![delta]))
        .unwrap();
    drop(store2);
    let manifest = root2.join("manifest.mgm1");
    let manifest_len = std::fs::metadata(&manifest).unwrap().len() as usize;
    flip_byte(&manifest, manifest_len - 5);
    assert!(matches!(
        DeltaGenerationStore::open(&root2),
        Err(DeltaStoreError::ManifestMalformed(reason)) if reason.contains("CRC32 mismatch")
    ));
}

#[test]
fn worldgen_epoch_mismatch_is_fail_closed_without_explicit_migration() {
    let dir = TestDir::new("epoch-mismatch");
    let root = dir.path("store");
    let base = vec![0u32; 4096];
    let mut state1 = base.clone();
    state1[1] = 11;
    let mut state2 = base.clone();
    state2[2] = 22;

    let mut store = DeltaGenerationStore::open(&root).unwrap();
    let gen1 = store
        .commit_generation(commit_options(
            8,
            "epoch-a",
            "pre-migration",
            1,
            vec![identified_delta(&base, &state1, "epoch-a", 0, 0, 0)],
        ))
        .unwrap();

    let rejected = store.commit_generation(
        commit_options(
            8,
            "epoch-b",
            "migrated",
            2,
            vec![identified_delta(&base, &state2, "epoch-b", 0, 0, 0)],
        )
        .with_parent(gen1.gen_id),
    );
    assert!(matches!(
        rejected,
        Err(DeltaStoreError::EpochMismatch { .. })
    ));

    let gen2 = store
        .commit_generation(
            commit_options(
                8,
                "epoch-b",
                "migrated",
                2,
                vec![identified_delta(&base, &state2, "epoch-b", 0, 0, 0)],
            )
            .with_parent(gen1.gen_id)
            .allow_epoch_migration(true),
        )
        .unwrap();

    assert!(matches!(
        store.checkout_generation(gen1.gen_id),
        Err(DeltaStoreError::EpochMismatch { .. })
    ));
    assert!(matches!(
        store.diff_generations(gen1.gen_id, gen2.gen_id),
        Err(DeltaStoreError::EpochMismatch { .. })
    ));

    let migrated_checkout = store
        .checkout_generation_with_migration(gen1.gen_id, true)
        .unwrap();
    assert_eq!(migrated_checkout.record.worldgen_epoch, "epoch-a");
    let migration_diff = store
        .diff_generations_with_migration(gen1.gen_id, gen2.gen_id, true)
        .unwrap();
    assert_eq!(migration_diff.chunks.len(), 1);
}

#[test]
fn atomic_store_writes_work_for_absolute_and_bare_relative_directories() {
    // Absolute store path.
    let dir = TestDir::new("atomic-paths");
    let absolute_root = dir.path("absolute-store");
    let base = vec![0u32; 16];
    let mut current = base.clone();
    current[0] = 1;
    let delta = identified_delta(&base, &current, "epoch-a", 0, 0, 0);
    let mut absolute_store = DeltaGenerationStore::open(&absolute_root).unwrap();
    let absolute_generation = absolute_store
        .commit_generation(commit_options(
            1,
            "epoch-a",
            "absolute",
            1,
            vec![delta.clone()],
        ))
        .unwrap();
    assert!(absolute_root.join("manifest.mgm1").is_file());
    assert!(absolute_root.join("generations").is_dir());
    assert!(blob_files(&absolute_root).len() == 1);
    drop(absolute_store);
    assert!(DeltaGenerationStore::open(&absolute_root)
        .unwrap()
        .checkout_generation(absolute_generation.gen_id)
        .is_ok());

    // Bare relative store path (no parent component). The process-wide cwd is
    // held only inside this test and restored before the guard is dropped.
    let _cwd_guard = CWD_LOCK.lock().unwrap();
    let original_cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(&dir.0).unwrap();
    {
        let relative_root = PathBuf::from("relative-store");
        if relative_root.exists() {
            std::fs::remove_dir_all(&relative_root).unwrap();
        }
        let mut relative_store = DeltaGenerationStore::open(&relative_root).unwrap();
        let relative_generation = relative_store
            .commit_generation(commit_options(1, "epoch-a", "relative", 2, vec![delta]))
            .unwrap();
        assert!(relative_root.join("manifest.mgm1").is_file());
        drop(relative_store);
        let reopened = DeltaGenerationStore::open(&relative_root).unwrap();
        reopened
            .checkout_generation(relative_generation.gen_id)
            .unwrap();
        std::fs::remove_dir_all(&relative_root).unwrap();
    }
    std::env::set_current_dir(original_cwd).unwrap();
    drop(_cwd_guard);
}

#[test]
fn gc_prunes_only_blobs_not_referenced_by_pinned_or_active_generations() {
    let dir = TestDir::new("generation-gc");
    let root = dir.path("store");
    let base = vec![0u32; 4096];
    let mut state1 = base.clone();
    state1[1] = 101;
    let mut state2 = state1.clone();
    state2[2] = 202;

    let mut store = DeltaGenerationStore::open(&root).unwrap();
    let gen1 = store
        .commit_generation(commit_options(
            4,
            "epoch-a",
            "pinned-base",
            1,
            vec![identified_delta(&base, &state1, "epoch-a", 0, 0, 0)],
        ))
        .unwrap();
    let gen2 = store
        .commit_generation(
            commit_options(
                4,
                "epoch-a",
                "disposable-tip",
                2,
                vec![identified_delta(&base, &state2, "epoch-a", 0, 0, 0)],
            )
            .with_parent(gen1.gen_id)
            .pinned(false),
        )
        .unwrap();
    assert!(store.is_pinned(gen1.gen_id));
    assert!(!store.is_pinned(gen2.gen_id));
    assert_eq!(blob_files(&root).len(), 2);

    // While gen2 is active it is retained even though it is unpinned.
    let report = store.gc_unreferenced_blobs().unwrap();
    assert_eq!(report.scanned_blobs, 2);
    assert_eq!(report.retained_blobs, 2);
    assert_eq!(report.removed_blobs, 0);

    // Moving the active head back to pinned gen1 makes gen2 collectable.
    store.activate_generation(gen1.gen_id, false).unwrap();
    let report = store.gc_unreferenced_blobs().unwrap();
    assert_eq!(report.scanned_blobs, 2);
    assert_eq!(report.retained_blobs, 1);
    assert_eq!(report.removed_blobs, 1);
    assert!(report.removed_bytes > 0);
    assert_eq!(blob_files(&root).len(), 1);

    assert!(store.checkout_generation(gen1.gen_id).is_ok());
    assert!(matches!(
        store.checkout_generation(gen2.gen_id),
        Err(DeltaStoreError::MissingBlob { .. })
    ));

    // Because generation identity is content-addressed, recommitting the exact
    // same overlay resurrects the same generation id and reinstalls its blob.
    let delta2 = identified_delta(&base, &state2, "epoch-a", 0, 0, 0);
    let resurrected = store
        .commit_generation(
            commit_options(4, "epoch-a", "disposable-tip", 2, vec![delta2])
                .with_parent(gen1.gen_id)
                .pinned(false),
        )
        .unwrap();
    assert_eq!(resurrected.gen_id, gen2.gen_id);
    assert!(store.checkout_generation(gen2.gen_id).is_ok());
}

#[test]
fn codec_rejects_schema_magic_truncation_duplicate_and_noop_malformations() {
    let base = vec![0u32; 16];
    let mut current = base.clone();
    current[1] = 2;
    let delta = identified_delta(&base, &current, "epoch-a", 0, 0, 0);
    let encoded = encode_chunk_delta(&delta);

    let mut bad_magic = encoded.clone();
    bad_magic[0] = b'X';
    assert_eq!(decode_chunk_delta(&bad_magic), Err(DeltaError::BadMagic));

    // Keep the CRC valid while changing the schema so the schema branch, not
    // the CRC branch, is exercised.
    let mut bad_schema = encoded.clone();
    bad_schema[4..6].copy_from_slice(&2u16.to_be_bytes());
    let crc_position = bad_schema.len() - 4;
    let crc = megu_core::hash::crc32_ieee(&bad_schema[..crc_position]);
    bad_schema[crc_position..].copy_from_slice(&crc.to_be_bytes());
    assert_eq!(
        decode_chunk_delta(&bad_schema),
        Err(DeltaError::UnsupportedSchemaVersion(2))
    );

    // Removing payload bytes but leaving four trailer-shaped bytes is a CRC
    // mismatch; truncating below even the trailer/magic minimum is the codec's
    // explicit Truncated branch. Both are fail-closed.
    assert!(matches!(
        decode_chunk_delta(&encoded[..encoded.len() - 2]),
        Err(DeltaError::CrcMismatch { .. })
    ));
    assert_eq!(
        decode_chunk_delta(&encoded[..5]),
        Err(DeltaError::Truncated)
    );

    let duplicate = ChunkDelta {
        schema_version: 1,
        worldgen_epoch: "epoch-a".to_string(),
        dim_id: 0,
        cx: 0,
        cz: 0,
        edits: vec![
            BlockDeltaEdit {
                local_idx: 1,
                base_state: 0,
                new_state: 2,
            },
            BlockDeltaEdit {
                local_idx: 1,
                base_state: 0,
                new_state: 3,
            },
        ],
        unknown_blocks: Vec::new(),
    };
    assert!(matches!(
        apply_chunk_delta(&base, &duplicate),
        Err(DeltaError::DuplicateIndex { local_idx: 1 })
    ));
}

#[test]
fn root_commits_cannot_switch_store_seed_or_epoch_without_explicit_migration() {
    let dir = TestDir::new("root-world-identity");
    let root = dir.path("store");
    let base = vec![0u32; 16];
    let mut state = base.clone();
    state[0] = 7;
    let delta = || identified_delta(&base, &state, "epoch-a", 0, 0, 0);

    let mut store = DeltaGenerationStore::open(&root).unwrap();
    store
        .commit_generation(commit_options(1, "epoch-a", "root", 1, vec![delta()]))
        .unwrap();

    assert!(matches!(
        store.commit_generation(commit_options(2, "epoch-a", "wrong-seed", 2, vec![delta()])),
        Err(DeltaStoreError::SeedMismatch {
            expected: 1,
            actual: 2
        })
    ));

    let epoch_b_delta = || identified_delta(&base, &state, "epoch-b", 0, 0, 0);
    assert!(matches!(
        store.commit_generation(commit_options(
            1,
            "epoch-b",
            "wrong-epoch",
            2,
            vec![epoch_b_delta()]
        )),
        Err(DeltaStoreError::EpochMismatch { .. })
    ));
    assert!(matches!(
        store.commit_generation(
            commit_options(
                2,
                "epoch-b",
                "wrong-seed-and-epoch",
                2,
                vec![epoch_b_delta()]
            )
            .allow_epoch_migration(true)
        ),
        Err(DeltaStoreError::SeedMismatch {
            expected: 1,
            actual: 2
        })
    ));

    let migrated = store
        .commit_generation(
            commit_options(1, "epoch-b", "explicit-migration", 3, vec![epoch_b_delta()])
                .allow_epoch_migration(true),
        )
        .unwrap();
    assert_eq!(store.active_generation(), Some(migrated.gen_id));
}

#[test]
fn activating_a_generation_whose_blob_was_collected_fails_closed() {
    let dir = TestDir::new("missing-blob-activation");
    let root = dir.path("store");
    let base = vec![0u32; 16];
    let mut state_a = base.clone();
    state_a[0] = 11;
    let mut state_b = state_a.clone();
    state_b[1] = 22;

    let mut store = DeltaGenerationStore::open(&root).unwrap();
    let gen_a = store
        .commit_generation(
            commit_options(
                1,
                "epoch-a",
                "collectable-root",
                1,
                vec![identified_delta(&base, &state_a, "epoch-a", 0, 0, 0)],
            )
            .pinned(false),
        )
        .unwrap();
    let gen_b = store
        .commit_generation(
            commit_options(
                1,
                "epoch-a",
                "pinned-tip",
                2,
                vec![identified_delta(&base, &state_b, "epoch-a", 0, 0, 0)],
            )
            .with_parent(gen_a.gen_id),
        )
        .unwrap();
    assert_eq!(store.active_generation(), Some(gen_b.gen_id));

    let report = store.gc_unreferenced_blobs().unwrap();
    assert_eq!(report.removed_blobs, 1);
    assert!(matches!(
        store.activate_generation(gen_a.gen_id, false),
        Err(DeltaStoreError::MissingBlob { .. })
    ));
    assert_eq!(store.active_generation(), Some(gen_b.gen_id));
}

#[test]
fn decoder_rejects_noncanonical_split_contiguous_runs() {
    let mut out = Vec::new();
    out.extend_from_slice(b"MDW1");
    out.extend_from_slice(&1u16.to_be_bytes());
    out.push(1); // run encoding
    out.push(0); // reserved
    out.extend_from_slice(&1u16.to_be_bytes());
    out.extend_from_slice(b"e");
    out.extend_from_slice(&0i32.to_be_bytes());
    out.extend_from_slice(&0i32.to_be_bytes());
    out.extend_from_slice(&0i32.to_be_bytes());
    out.extend_from_slice(&8u32.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(&2u32.to_be_bytes());
    for start in [0u16, 4] {
        out.extend_from_slice(&start.to_be_bytes());
        out.extend_from_slice(&3u16.to_be_bytes()); // four edits, len-1 on wire
        for _ in 0..4 {
            out.extend_from_slice(&0u32.to_be_bytes());
            out.extend_from_slice(&1u32.to_be_bytes());
        }
    }
    let crc = megu_core::hash::crc32_ieee(&out);
    out.extend_from_slice(&crc.to_be_bytes());

    assert!(matches!(
        decode_chunk_delta(&out),
        Err(DeltaError::InvalidRun(_))
    ));
}

#[test]
fn set_unknown_block_repairs_unsorted_entries_without_duplicates() {
    let entry = |local_idx, nbt: &str| UnknownBlockEntry {
        local_idx,
        namespaced_id: "example:unknown".to_string(),
        properties_json: String::new(),
        nbt_bytes: nbt.as_bytes().to_vec(),
    };
    let mut delta = ChunkDelta::new("epoch-a", 0, 0, 0);
    delta.unknown_blocks = vec![entry(2, "old-two"), entry(1, "one")];

    delta.set_unknown_block(entry(2, "new-two"));

    assert_eq!(
        delta.unknown_blocks,
        vec![entry(1, "one"), entry(2, "new-two")]
    );
    assert!(megu_state_store::delta_world::validate_chunk_delta(&delta).is_ok());
}
