// SPDX-License-Identifier: LGPL-3.0-or-later

use std::collections::{BTreeMap, HashMap, HashSet};

use megu_core::crypto::sha256;
use megu_core::lz4::Lz4Error;
use megu_core::merkle_cas::{
    internal_node_hash, leaf_node_hash, CasBlobStore, Digest32, MerkleTree,
};
use megu_core::search_index::{SearchDocId, TrigramIndex};
use megu_core::sparse_set::SparseSet;
use megu_core::timing_wheel::{TimerHandle, TimingWheel, TIMING_WHEEL_SPAN_TICKS};
use megu_core::union_find::DisjointSet;

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

fn random_below(state: &mut u64, upper: u64) -> u64 {
    splitmix64(state) % upper
}

#[test]
fn trigram_index_ranks_multilingual_queries_and_reuses_output() {
    let mut index = TrigramIndex::new();
    assert!(index.add_document(
        SearchDocId(0),
        "Crafting Table",
        ["crafting", "クラフティングテーブル", "craft station"]
    ));
    assert!(index.add_document(
        SearchDocId(1),
        "Diamond Pickaxe",
        ["ダイヤモンドのツルハシ", "daiya pickaxe"]
    ));
    assert!(index.add_document(
        SearchDocId(2),
        "settings.renderDistance",
        ["render distance", "描画距離", "video setting"]
    ));
    assert!(index.add_document(SearchDocId(3), "abc x bcd x cde", ["fuzzy trigram islands"]));
    assert!(index.add_document(SearchDocId(4), "abcdef fast path", ["exact letters"]));
    assert!(index.add_document(SearchDocId(5), "worktable", ["bench"]));

    let english = [
        "stone brick",
        "copper ingot",
        "redstone torch",
        "oak plank",
        "iron helmet",
        "golden carrot",
        "potion of leaping",
        "netherite scrap",
    ];
    let japanese = [
        "石",
        "銅",
        "レッドストーン",
        "オーク",
        "鉄",
        "金",
        "ポーション",
        "ネザライト",
    ];
    for raw in 10..5_310u32 {
        let slot = raw as usize % english.len();
        let primary = format!("{} variant {}", english[slot], raw);
        let kana = format!("{}アイテム{}", japanese[slot], raw);
        let romaji = format!("romaji-item-{}", raw);
        assert!(index.add_document(SearchDocId(raw), &primary, [kana.as_str(), romaji.as_str()]));
    }
    assert!(index.len() > 5_000);
    assert!(!index.add_document(SearchDocId(0), "duplicate", [] as [&str; 0]));

    let mut hits = Vec::with_capacity(16);
    index.query("CRAFT", 10, &mut hits);
    assert_eq!(hits.first().map(|hit| hit.doc), Some(SearchDocId(0)));
    let prefix_score = hits[0].score;
    assert!(
        prefix_score >= 4_000,
        "prefix band expected: {prefix_score}"
    );

    index.query("table", 10, &mut hits);
    let crafting = hits
        .iter()
        .find(|hit| hit.doc == SearchDocId(0))
        .expect("word-boundary hit");
    let worktable = hits
        .iter()
        .find(|hit| hit.doc == SearchDocId(5))
        .expect("substring hit");
    assert!((3_000..4_000).contains(&crafting.score));
    assert!((2_000..3_000).contains(&worktable.score));
    assert!(crafting.score > worktable.score);

    index.query("クラフテ", 5, &mut hits);
    assert!(hits.iter().any(|hit| hit.doc == SearchDocId(0)));
    index.query("描画", 5, &mut hits);
    assert!(hits.iter().any(|hit| hit.doc == SearchDocId(2)));

    // `abcde` is a subsequence but not a substring in document 3.  Its three
    // trigrams exist as separated islands, exercising posting-list intersection
    // while the scorer must still rank the true substring in document 4 first.
    index.query("abcde", 10, &mut hits);
    let fuzzy_pos = hits
        .iter()
        .position(|hit| hit.doc == SearchDocId(3))
        .expect("subsequence candidate");
    let substring_pos = hits
        .iter()
        .position(|hit| hit.doc == SearchDocId(4))
        .expect("substring candidate");
    assert!(substring_pos < fuzzy_pos);
    assert!(hits[fuzzy_pos].score < 2_000);
    assert!(hits[substring_pos].score >= 2_000);

    index.query("ct", 10, &mut hits);
    assert!(hits.iter().any(|hit| hit.doc == SearchDocId(0)));
    index.query("crt", 10, &mut hits);
    let fuzzy_craft = hits
        .iter()
        .find(|hit| hit.doc == SearchDocId(0))
        .expect("skipped-character fuzzy fallback");
    assert!(fuzzy_craft.score < 2_000);
    index.query("石", 10, &mut hits);
    assert!(!hits.is_empty(), "one-character scan must find kana/kanji aliases");
    assert_eq!(hits.len(), hits.iter().collect::<HashSet<_>>().len());

    index.query("render", 8, &mut hits);
    assert!(!hits.is_empty());
    let retained_capacity = hits.capacity();
    for _ in 0..64 {
        index.query("render", 8, &mut hits);
        assert_eq!(hits.capacity(), retained_capacity);
        assert!(hits.len() <= 8);
    }
}

#[test]
fn timing_wheel_matches_btreemap_oracle_with_random_cancellation() {
    let mut rng = 0x1234_5678_9abc_def0;
    let mut wheel = TimingWheel::new();
    let mut oracle: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
    let mut handles = Vec::new();
    let mut cancelled = HashSet::new();

    for payload in 0..10_000usize {
        let delay = random_below(&mut rng, 200_000);
        let due = delay.max(1); // scheduling is at current tick zero; zero clamps to next advance.
        let handle = wheel.schedule_after(delay, payload);
        handles.push((payload, handle));
        oracle.entry(due).or_default().push(payload);
    }
    assert_eq!(wheel.len(), 10_000);

    for &(payload, handle) in &handles {
        if random_below(&mut rng, 5) == 0 {
            assert!(wheel.cancel(handle), "first cancellation must succeed");
            assert!(
                !wheel.cancel(handle),
                "repeat cancellation sees a stale handle"
            );
            cancelled.insert(payload);
        }
    }
    assert_eq!(wheel.len(), 10_000 - cancelled.len());

    let mut fired = Vec::new();
    for tick in 1..=200_000u64 {
        fired.clear();
        wheel.advance_into(&mut fired);
        let mut expected: Vec<usize> = oracle
            .remove(&tick)
            .unwrap_or_default()
            .into_iter()
            .filter(|payload| !cancelled.contains(payload))
            .collect();
        expected.sort_unstable();
        fired.sort_unstable();
        assert_eq!(fired, expected, "firing mismatch at tick {tick}");
    }
    assert!(oracle.is_empty());
    assert!(wheel.is_empty());
    assert!(
        !wheel.cancel(handles[0].1),
        "fired handles are generation-stale"
    );
}

#[test]
fn timing_wheel_handles_schedule_and_cancel_while_time_is_running() {
    let mut rng = 0x8421_5f3a_77aa_1099;
    let mut wheel = TimingWheel::new();
    let mut oracle: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
    let mut live_handles = Vec::new();
    let mut cancelled = HashSet::new();
    let mut next_payload = 0u64;

    for tick in 1..=5_000u64 {
        // Insert before advancing so due times exercise slots ahead of every
        // possible non-zero current-tick alignment.
        for _ in 0..random_below(&mut rng, 3) {
            let delay = random_below(&mut rng, 1_000);
            let payload = next_payload;
            next_payload += 1;
            let handle = wheel.schedule_after(delay, payload);
            oracle
                .entry(tick - 1 + delay.max(1))
                .or_default()
                .push(payload);
            live_handles.push((payload, handle));
        }
        if !live_handles.is_empty() && random_below(&mut rng, 4) == 0 {
            let victim = random_below(&mut rng, live_handles.len() as u64) as usize;
            let (payload, handle) = live_handles.swap_remove(victim);
            assert!(wheel.cancel(handle));
            cancelled.insert(payload);
        }

        let mut fired = Vec::new();
        wheel.advance_into(&mut fired);
        for payload in &fired {
            if let Some(position) = live_handles
                .iter()
                .position(|(live_payload, _)| live_payload == payload)
            {
                live_handles.swap_remove(position);
            }
        }
        let mut expected: Vec<u64> = oracle
            .remove(&tick)
            .unwrap_or_default()
            .into_iter()
            .filter(|payload| !cancelled.contains(payload))
            .collect();
        expected.sort_unstable();
        fired.sort_unstable();
        assert_eq!(fired, expected, "dynamic mismatch at tick {tick}");
    }
    for tick in 5_001..=6_000u64 {
        let mut fired = Vec::new();
        wheel.advance_into(&mut fired);
        let mut expected: Vec<u64> = oracle
            .remove(&tick)
            .unwrap_or_default()
            .into_iter()
            .filter(|payload| !cancelled.contains(payload))
            .collect();
        expected.sort_unstable();
        fired.sort_unstable();
        assert_eq!(fired, expected, "dynamic drain mismatch at tick {tick}");
    }
    assert!(oracle.is_empty());
    assert!(wheel.is_empty());
}

#[test]
fn timing_wheel_overflow_timers_fire_at_their_exact_tick() {
    let mut wheel = TimingWheel::new();
    let delay = TIMING_WHEEL_SPAN_TICKS + 123;
    let handle: TimerHandle = wheel.schedule_after(delay, 77u32);
    let target = delay;
    let mut fired = Vec::new();
    for tick in 1..target {
        fired.clear();
        wheel.advance_into(&mut fired);
        assert!(
            fired.is_empty(),
            "overflow timer fired early at tick {tick}"
        );
    }
    wheel.advance_into(&mut fired);
    assert_eq!(fired, vec![77]);
    assert!(wheel.is_empty());
    assert!(!wheel.cancel(handle));

    let stale = wheel.schedule_after(10, 88u32);
    wheel.clear();
    assert!(wheel.schedule_after(0, 99u32) != stale);
    assert!(
        !wheel.cancel(stale),
        "clear must invalidate pre-clear handles"
    );
    fired.clear();
    wheel.advance_into(&mut fired);
    assert_eq!(fired, vec![99]);
}

#[test]
fn disjoint_set_matches_bfs_connectivity_and_component_sizes() {
    let mut rng = 0xfeed_face_cafe_beef;
    let n = 240usize;
    let mut dsu = DisjointSet::new(n);
    let mut adjacency = vec![Vec::<usize>::new(); n];

    for _ in 0..1_000 {
        let a = random_below(&mut rng, n as u64) as u32;
        let b = random_below(&mut rng, n as u64) as u32;
        if a != b {
            let already_connected = dsu.connected(a, b);
            let merged = dsu.union(a, b);
            assert_eq!(merged, !already_connected);
            adjacency[a as usize].push(b as usize);
            adjacency[b as usize].push(a as usize);
        }
    }

    let mut component_of = vec![usize::MAX; n];
    let mut oracle_sizes = Vec::new();
    for start in 0..n {
        if component_of[start] != usize::MAX {
            continue;
        }
        let component = oracle_sizes.len();
        let mut stack = vec![start];
        let mut size = 0u32;
        component_of[start] = component;
        while let Some(node) = stack.pop() {
            size += 1;
            for &next in &adjacency[node] {
                if component_of[next] == usize::MAX {
                    component_of[next] = component;
                    stack.push(next);
                }
            }
        }
        oracle_sizes.push(size);
    }
    assert_eq!(dsu.component_count(), oracle_sizes.len());

    for node in 0..n as u32 {
        assert_eq!(
            dsu.component_size(node),
            oracle_sizes[component_of[node as usize]],
            "component size mismatch for {node}"
        );
    }
    for _ in 0..500 {
        let a = random_below(&mut rng, n as u64) as usize;
        let b = random_below(&mut rng, n as u64) as usize;
        assert_eq!(
            dsu.connected(a as u32, b as u32),
            component_of[a] == component_of[b],
            "connectivity mismatch for {a} / {b}"
        );
    }

    dsu.reset();
    assert_eq!(dsu.component_count(), n);
    assert!(dsu.component_size(0) == 1);
    let extra = dsu.push();
    assert_eq!(extra, n as u32);
    assert!(dsu.union(0, extra));
    assert!(!dsu.union(0, extra));
    assert!(dsu.connected(0, extra));
    assert_eq!(dsu.component_size(0), 2);
}

fn leaf_digest(seed: u64) -> Digest32 {
    sha256(format!("leaf-{seed}").as_bytes())
}

#[test]
fn merkle_tree_domain_separates_and_pinpoints_sparse_mutations() {
    let key = 42u64;
    let digest = leaf_digest(key);
    let mut leaf_preimage = Vec::new();
    leaf_preimage.push(0x00);
    leaf_preimage.extend_from_slice(&key.to_be_bytes());
    leaf_preimage.extend_from_slice(&digest);
    assert_eq!(leaf_node_hash(key, digest), sha256(&leaf_preimage));
    assert_ne!(leaf_node_hash(key, digest), digest);

    let left = leaf_node_hash(1, leaf_digest(1));
    let right = leaf_node_hash(2, leaf_digest(2));
    let mut internal_preimage = Vec::new();
    internal_preimage.push(0x01);
    internal_preimage.extend_from_slice(&left);
    internal_preimage.extend_from_slice(&right);
    assert_eq!(internal_node_hash(left, right), sha256(&internal_preimage));
    assert_ne!(
        internal_node_hash(left, right),
        leaf_node_hash(1, leaf_digest(1))
    );

    let leaves: Vec<(u64, Digest32)> = (0..4_096u64).map(|key| (key, leaf_digest(key))).collect();
    let original = MerkleTree::from_sorted(&leaves);
    assert_ne!(original.root(), [0u8; 32]);

    let mut mutated_leaves = leaves.clone();
    for key in [17u64, 1_024, 4_095] {
        mutated_leaves[key as usize].1 = sha256(format!("mutated-{key}").as_bytes());
    }
    let mutated = MerkleTree::from_sorted(&mutated_leaves);
    assert_ne!(original.root(), mutated.root());
    let mut diff = Vec::new();
    original.diff_keys(&mutated, &mut diff);
    assert_eq!(diff, vec![17, 1_024, 4_095]);
    mutated.diff_keys(&original, &mut diff);
    assert_eq!(diff, vec![17, 1_024, 4_095]);

    // Shape-changing edits fall back to exact ordered key merging rather than
    // mistaking shifted positions for mutations.
    let shorter = MerkleTree::new(&leaves[..4_095]);
    original.diff_keys(&shorter, &mut diff);
    assert_eq!(diff, vec![4_095]);

    // Removing key 2 while adding key 4 keeps the same cardinality but shifts
    // key 3 by one position.  Positional recursion alone would over-report 3;
    // key validation must reduce the result to the exact removed/added pair.
    let shifted_a = MerkleTree::new(&[
        (1, leaf_digest(1)),
        (2, leaf_digest(2)),
        (3, leaf_digest(3)),
    ]);
    let shifted_b = MerkleTree::new(&[
        (1, leaf_digest(1)),
        (3, leaf_digest(3)),
        (4, leaf_digest(4)),
    ]);
    shifted_a.diff_keys(&shifted_b, &mut diff);
    assert_eq!(diff, vec![2, 4]);
}

#[test]
fn cas_blob_store_deduplicates_and_enforces_decompression_caps() {
    let mut store = CasBlobStore::new();
    let compressible = vec![b'm'; 16_384];
    let mut binary = Vec::new();
    for index in 0..4_096u32 {
        binary.extend_from_slice(&index.to_le_bytes());
    }

    let digest_a = store.put(&compressible);
    assert_eq!(digest_a, sha256(&compressible));
    assert_eq!(store.put(&compressible), digest_a);
    let digest_b = store.put(&binary);
    assert_eq!(store.put(&binary), digest_b);
    let missing = sha256(b"not present");

    assert_eq!(store.unique_blobs(), 2);
    assert!(store.contains(&digest_a));
    assert!(store.contains(&digest_b));
    assert!(!store.contains(&missing));
    assert!(
        store.stored_bytes() < compressible.len() + binary.len(),
        "compressible CAS corpus should occupy fewer stored bytes"
    );

    assert_eq!(
        store.get(&digest_a, compressible.len()),
        Ok(Some(compressible.clone()))
    );
    assert_eq!(store.get(&digest_b, binary.len()), Ok(Some(binary)));
    assert_eq!(store.get(&missing, usize::MAX), Ok(None));
    assert_eq!(
        store.get(&digest_a, compressible.len() - 1),
        Err(Lz4Error::TooLarge)
    );
    assert_eq!(store.get(&digest_a, 0), Err(Lz4Error::TooLarge));
}

#[test]
fn sparse_set_matches_hashmap_across_randomized_operation_stream() {
    let mut rng = 0x0ddc_0ffe_e15e_dead;
    let mut set = SparseSet::new();
    let mut oracle = HashMap::<u32, u64>::new();

    for step in 0..10_000u64 {
        let id = random_below(&mut rng, 600) as u32;
        match random_below(&mut rng, 5) {
            0..=2 => {
                let value = step.wrapping_mul(0x1_0000_0001) ^ u64::from(id);
                assert_eq!(set.insert(id, value), oracle.insert(id, value));
            }
            3 => {
                assert_eq!(set.remove(id), oracle.remove(&id));
            }
            _ => {
                let expected = oracle.get(&id).copied();
                assert_eq!(set.get(id).copied(), expected);
                if let Some(value) = set.get_mut(id) {
                    *value = value.wrapping_add(1);
                    *oracle.get_mut(&id).expect("oracle mirror") += 1;
                }
            }
        }

        if step % 317 == 0 {
            assert_sparse_set_parity(&set, &oracle);
        }
    }
    assert_sparse_set_parity(&set, &oracle);

    for (_, value) in set.iter_mut() {
        *value = value.wrapping_add(7);
    }
    for value in oracle.values_mut() {
        *value = value.wrapping_add(7);
    }
    assert_sparse_set_parity(&set, &oracle);

    let old_len = set.len();
    let first_id = set.ids()[0];
    assert_eq!(set.remove(first_id), oracle.remove(&first_id));
    assert_eq!(set.len(), old_len - 1);
    assert_sparse_set_parity(&set, &oracle);
    set.clear();
    oracle.clear();
    assert!(set.is_empty());
    assert_sparse_set_parity(&set, &oracle);
}

fn assert_sparse_set_parity(set: &SparseSet<u64>, oracle: &HashMap<u32, u64>) {
    assert_eq!(set.len(), oracle.len());
    assert_eq!(set.is_empty(), oracle.is_empty());
    assert_eq!(set.ids().len(), set.as_slice().len());
    let mut seen = HashSet::new();
    for (position, (id, value)) in set.iter().enumerate() {
        assert!(seen.insert(id), "duplicate dense id {id}");
        assert_eq!(set.ids()[position], id);
        assert!(std::ptr::eq(value, &set.as_slice()[position]));
        assert_eq!(oracle.get(&id), Some(value));
        assert_eq!(set.get(id), Some(value));
    }
    for (&id, value) in oracle {
        assert_eq!(set.get(id), Some(value));
    }
}
