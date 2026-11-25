// SPDX-License-Identifier: LGPL-3.0-or-later
#![forbid(unsafe_code)]
//! Content-addressed Merkle trees and an LZ4-compressed CAS blob store.
//!
//! The Merkle tree authenticates ordered `(key, leaf digest)` pairs.  Leaf and
//! internal node hashes are domain-separated (`0x00` versus `0x01`), so a leaf
//! encoding can never be reinterpreted as an internal node in a second-preimage
//! attack.  [`MerkleTree::diff_keys`] prunes equal subtrees and is therefore
//! proportional to `O(k log N)` for `k` positional leaf changes; trees with a
//! different number of leaves use an ordered merge fallback so inserted and
//! removed keys remain exact rather than probabilistic.
//!
//! [`CasBlobStore`] is deliberately small and synchronous: it is the canonical
//! in-memory deduplicating primitive higher-level snapshot, pack-sync, and
//! replay layers can shard or persist as needed.  Blob identity is the SHA-256
//! of the uncompressed bytes, while storage uses the workspace's bounded LZ4
//! block codec.

use std::collections::HashMap;

use crate::crypto::sha256;
use crate::lz4::{compress_block, decompress_block, Lz4Error};

/// A 32-byte SHA-256 digest.
pub type Digest32 = [u8; 32];

/// Hash the authenticated tree leaf for `key` and the caller-supplied content
/// digest.  The key is part of the preimage so swapping identical content
/// across keys is visible.
pub fn leaf_node_hash(key: u64, leaf_digest: Digest32) -> Digest32 {
    let mut preimage = [0u8; 1 + 8 + 32];
    preimage[0] = 0x00;
    preimage[1..9].copy_from_slice(&key.to_be_bytes());
    preimage[9..].copy_from_slice(&leaf_digest);
    sha256(&preimage)
}

/// Hash an internal Merkle node from its left and right child node hashes.
pub fn internal_node_hash(left: Digest32, right: Digest32) -> Digest32 {
    let mut preimage = [0u8; 1 + 32 + 32];
    preimage[0] = 0x01;
    preimage[1..33].copy_from_slice(&left);
    preimage[33..].copy_from_slice(&right);
    sha256(&preimage)
}

/// Balanced binary SHA-256 Merkle tree over sorted unique keys.
///
/// Odd nodes at a level are promoted unchanged to the next level.  Because
/// leaves and internal nodes are domain-separated, promotion does not create a
/// leaf/internal ambiguity; it only preserves the exact binary layout needed
/// for positional subtree comparison.
#[derive(Clone, Debug)]
pub struct MerkleTree {
    leaves: Vec<(u64, Digest32)>,
    /// Bottom-up node-hash levels; `levels[0]` is the authenticated leaf row
    /// and the final level has exactly one root node for a non-empty tree.
    levels: Vec<Vec<Digest32>>,
}

impl MerkleTree {
    /// Build a canonical tree from any ordering.  Keys are sorted; duplicate
    /// keys keep the first digest supplied, making construction deterministic
    /// without silently authenticating two values for one key.
    pub fn new(leaves: &[(u64, Digest32)]) -> Self {
        let mut owned = leaves.to_vec();
        owned.sort_by_key(|leaf| leaf.0);
        owned.dedup_by(|a, b| a.0 == b.0);
        Self::from_sorted(&owned)
    }

    /// Build a tree from already sorted, strictly unique keys.
    ///
    /// Panics if the ordering contract is violated; use [`Self::new`] for
    /// untrusted or unsorted input.
    pub fn from_sorted(leaves: &[(u64, Digest32)]) -> Self {
        assert!(
            leaves.windows(2).all(|pair| pair[0].0 < pair[1].0),
            "MerkleTree leaves must be sorted by strictly unique key"
        );
        let mut row: Vec<Digest32> = leaves
            .iter()
            .map(|&(key, digest)| leaf_node_hash(key, digest))
            .collect();
        let mut levels = Vec::new();
        if !row.is_empty() {
            while row.len() > 1 {
                let mut next = Vec::with_capacity(row.len().div_ceil(2));
                for pair in row.chunks(2) {
                    if pair.len() == 2 {
                        next.push(internal_node_hash(pair[0], pair[1]));
                    } else {
                        next.push(pair[0]);
                    }
                }
                levels.push(row);
                row = next;
            }
            levels.push(row);
        }
        Self {
            leaves: leaves.to_vec(),
            levels,
        }
    }

    /// Number of authenticated leaves.
    pub fn len(&self) -> usize {
        self.leaves.len()
    }

    /// Whether the tree has no leaves.
    pub fn is_empty(&self) -> bool {
        self.leaves.is_empty()
    }

    /// Root digest.  The empty-tree root is canonically defined as 32 zero
    /// bytes; all non-empty nodes remain domain-separated SHA-256 digests.
    pub fn root(&self) -> Digest32 {
        self.levels
            .last()
            .and_then(|row| row.first())
            .copied()
            .unwrap_or([0u8; 32])
    }

    /// Append every key whose presence or leaf digest differs between trees.
    ///
    /// `out` is cleared first and contains sorted unique keys on return.  For
    /// equal-length trees — including ordinary content mutation snapshots — the
    /// comparison prunes equal roots/subtrees and visits only the paths to the
    /// changed positions.  Equal-length insertion/deletion pairs are validated
    /// by key after positional pruning.  When lengths differ, shape-changing
    /// insertion/removal is resolved with a deterministic ordered merge;
    /// correctness is preferred over pretending positional trees with different
    /// shapes share compatible subtrees.
    pub fn diff_keys(&self, other: &MerkleTree, out_diff: &mut Vec<u64>) {
        out_diff.clear();
        if self.root() == other.root() {
            return;
        }
        if self.leaves.len() == other.leaves.len() && !self.leaves.is_empty() {
            let top_level = self.levels.len() - 1;
            self.diff_positional(other, top_level, 0, out_diff);
        } else {
            merge_diff(&self.leaves, &other.leaves, out_diff);
        }
        out_diff.sort_unstable();
        out_diff.dedup();
        // Positional recursion can over-report unchanged keys when a deletion
        // and insertion in the same cardinality shift their positions.  The
        // final ordered-key validation removes those positional artifacts while
        // keeping the mutation-only path at O(k log N): unchanged subtrees were
        // already pruned before any key lookup happens here.
        out_diff.retain(|&key| self.digest_for_key(key) != other.digest_for_key(key));
    }

    fn digest_for_key(&self, key: u64) -> Option<Digest32> {
        self.leaves
            .binary_search_by_key(&key, |leaf| leaf.0)
            .ok()
            .map(|index| self.leaves[index].1)
    }

    fn diff_positional(
        &self,
        other: &MerkleTree,
        level: usize,
        index: usize,
        out_diff: &mut Vec<u64>,
    ) {
        if self.levels[level][index] == other.levels[level][index] {
            return;
        }
        if level == 0 {
            let (akey, adigest) = self.leaves[index];
            let (bkey, bdigest) = other.leaves[index];
            if akey == bkey {
                if adigest != bdigest {
                    out_diff.push(akey);
                }
            } else {
                out_diff.push(akey);
                out_diff.push(bkey);
            }
            return;
        }
        let left = index * 2;
        self.diff_positional(other, level - 1, left, out_diff);
        let right = left + 1;
        if right < self.levels[level - 1].len() {
            self.diff_positional(other, level - 1, right, out_diff);
        }
    }
}

fn merge_diff(left: &[(u64, Digest32)], right: &[(u64, Digest32)], out_diff: &mut Vec<u64>) {
    let mut a = 0usize;
    let mut b = 0usize;
    while a < left.len() && b < right.len() {
        let (akey, adigest) = left[a];
        let (bkey, bdigest) = right[b];
        if akey == bkey {
            if adigest != bdigest {
                out_diff.push(akey);
            }
            a += 1;
            b += 1;
        } else if akey < bkey {
            out_diff.push(akey);
            a += 1;
        } else {
            out_diff.push(bkey);
            b += 1;
        }
    }
    out_diff.extend(left[a..].iter().map(|leaf| leaf.0));
    out_diff.extend(right[b..].iter().map(|leaf| leaf.0));
}

#[derive(Debug)]
struct CasEntry {
    compressed: Vec<u8>,
    raw_len: usize,
}

/// In-memory content-addressed blob store with LZ4-compressed storage.
#[derive(Debug, Default)]
pub struct CasBlobStore {
    blobs: HashMap<Digest32, CasEntry>,
    stored_bytes: usize,
}

impl CasBlobStore {
    /// Create an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Store `raw` and return the digest of its uncompressed bytes.
    ///
    /// Re-putting identical bytes is a digest lookup and does not compress or
    /// store a second copy.
    pub fn put(&mut self, raw: &[u8]) -> Digest32 {
        let digest = sha256(raw);
        if self.blobs.contains_key(&digest) {
            return digest;
        }
        let compressed = compress_block(raw);
        self.stored_bytes += compressed.len();
        self.blobs.insert(
            digest,
            CasEntry {
                compressed,
                raw_len: raw.len(),
            },
        );
        digest
    }

    /// Fetch and decompress a blob, bounded by `max_bytes`.
    ///
    /// Missing digests return `Ok(None)`.  A blob whose recorded uncompressed
    /// size exceeds `max_bytes` is rejected with [`Lz4Error::TooLarge`] before
    /// decompression begins, so hostile or accidental compression bombs cannot
    /// amplify memory use.  Codec corruption is fail-closed and propagated.
    pub fn get(&self, digest: &Digest32, max_bytes: usize) -> Result<Option<Vec<u8>>, Lz4Error> {
        let Some(entry) = self.blobs.get(digest) else {
            return Ok(None);
        };
        if entry.raw_len > max_bytes {
            return Err(Lz4Error::TooLarge);
        }
        let raw = decompress_block(&entry.compressed, max_bytes.min(entry.raw_len))?;
        if raw.len() != entry.raw_len {
            return Err(Lz4Error::LengthMismatch);
        }
        Ok(Some(raw))
    }

    /// Whether `digest` is present.
    pub fn contains(&self, digest: &Digest32) -> bool {
        self.blobs.contains_key(digest)
    }

    /// Number of distinct stored blobs.
    pub fn unique_blobs(&self) -> usize {
        self.blobs.len()
    }

    /// Total compressed payload bytes currently stored.
    ///
    /// This intentionally excludes container overhead: it is the byte count a
    /// replication or persistence layer would have to move for the unique blob
    /// set.
    pub fn stored_bytes(&self) -> usize {
        self.stored_bytes
    }
}
