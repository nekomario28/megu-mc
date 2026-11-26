// SPDX-License-Identifier: LGPL-3.0-or-later
//! Alpha 6 hierarchical region Merkle sync (`RMK1`) and zero-trust blind
//! relay framing (`ZTR1`).
//!
//! ## `RMK1`: 32x32 region Merkle sync
//!
//! [`RegionMerkleTree`] commits one Anvil region — a 32x32 grid of 1,024
//! chunk-content digests — into a balanced quadtree. The leaf row is the
//! caller-supplied chunk digest row; internal nodes are domain-separated
//! SHA-256 commitments bound to `(worldgen_epoch, dim_id, rx, rz)`, the node
//! level and the node index. Updating one chunk therefore rewrites exactly
//! the five internal ancestors on its root path instead of rebuilding the
//! whole region.
//!
//! Levels are counted from the root: level 0 has 1 node, level 1 has 4,
//! level 2 has 16, level 3 has 64, level 4 has 256 and level 5 is the
//! 1,024-chunk leaf row. Indices are row-major with X as the fastest axis,
//! matching Anvil chunk order. Quadrants are ordered north-west, north-east,
//! south-west, south-east (Minecraft Z increases southward).
//!
//! [`negotiate_region_diff`] is a deterministic sans-IO model of the
//! logarithmic descent encoded by [`RegionMerklePacket`]: equal roots produce
//! one `ServerRegionMatch`, while mismatched roots exchange only the children
//! of mismatched quadrants until the leaf row identifies the exact dirty
//! world-chunk coordinates.
//!
//! ## `ZTR1`: public-key blind relay
//!
//! [`BlindRelayFrame`] routes opaque AEAD ciphertext between 32-byte
//! [`NodeId`] public keys. The relay never sees plaintext, key material,
//! socket addresses or IP literals in this sans-IO contract; transport
//! endpoints are opaque integer handles owned by the caller. Migration is
//! anchored to the public key through a strictly increasing
//! `migration_epoch`, so changing networks or migrating a host does not
//! change the session identity. Replay or downgrade of an epoch is rejected
//! before routing.
//!
//! [`BlindRelayRouter`] enforces fixed frame shape, payload/frame caps,
//! per-session frame and byte token buckets, and the relay anti-amplification
//! invariant that forwarded bytes never exceed ingress bytes.

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::time::Instant;

use megu_core::crypto::Sha256;
use megu_core::hash::crc32_ieee;

use crate::friend_session::TokenBucket;
use crate::wire::{ProtoError, WireReader};

/// A 32-byte digest or public-key-sized byte string.
pub type Digest32 = [u8; 32];

/// Width of one Anvil region in chunks.
pub const REGION_WIDTH: usize = 32;
/// Number of chunk leaves in one Anvil region.
pub const REGION_CHUNK_COUNT: usize = REGION_WIDTH * REGION_WIDTH;
/// Depth of the leaf row below the root.
pub const REGION_TREE_DEPTH: u8 = 5;
/// Number of rows in the region quadtree, including the leaf row.
pub const REGION_TREE_LEVELS: u8 = REGION_TREE_DEPTH + 1;
/// Root row number.
pub const REGION_ROOT_LEVEL: u8 = 0;
/// Leaf row number.
pub const REGION_LEAF_LEVEL: u8 = REGION_TREE_DEPTH;

/// Canonical `RMK1` frame magic.
pub const REGION_MERKLE_MAGIC: [u8; 4] = *b"RMK1";
/// Short alias for [`REGION_MERKLE_MAGIC`].
pub const RMK1_MAGIC: [u8; 4] = REGION_MERKLE_MAGIC;
/// Only schema version currently defined.
pub const REGION_MERKLE_SCHEMA_VERSION: u16 = 1;
/// Maximum world-generation epoch bytes carried by `RMK1`.
pub const MAX_RMK1_EPOCH_BYTES: usize = 128;
/// Maximum region roots in one `ClientRegionRoots` packet.
pub const MAX_RMK1_REGION_ROOTS: usize = 1024;
/// Maximum subtree nodes in one subtree-hash packet.
pub const MAX_RMK1_SUBTREE_NODES: usize = REGION_CHUNK_COUNT;
/// Absolute upper bound on one `RMK1` packet.
pub const MAX_RMK1_PACKET_BYTES: usize = 64 * 1024;

const TAG_CLIENT_REGION_ROOTS: u8 = 0x01;
const TAG_SERVER_REGION_MATCH: u8 = 0x02;
const TAG_SERVER_SUBTREE_HASHES: u8 = 0x03;
const TAG_CLIENT_SUBTREE_HASHES: u8 = 0x04;
const TAG_EPOCH_MISMATCH: u8 = 0x05;

const RMK1_MIN_PACKET_BYTES: usize = 4 + 2 + 1 + 4;
const RMK1_NODE_DOMAIN: &[u8] = b"MEGU-RMK1-NODE-v1\0";

fn level_width(level: u8) -> Option<usize> {
    if level <= REGION_LEAF_LEVEL {
        Some(1usize << usize::from(level))
    } else {
        None
    }
}

fn level_offset(level: u8) -> usize {
    debug_assert!(level <= REGION_LEAF_LEVEL);
    let level = usize::from(level);
    ((1usize << (2 * level)) - 1) / 3
}

fn node_count_at_level(level: u8) -> Option<usize> {
    let width = level_width(level)?;
    Some(width * width)
}

fn valid_node_index(level: u8, index: u16) -> bool {
    node_count_at_level(level).is_some_and(|count| usize::from(index) < count)
}

fn flat_node_index(level: u8, index: u16) -> Option<usize> {
    if valid_node_index(level, index) {
        Some(level_offset(level) + usize::from(index))
    } else {
        None
    }
}

/// Return the four row-major child node indices of an internal node.
fn child_indices(level: u8, index: u16) -> Option<[u16; 4]> {
    if level >= REGION_LEAF_LEVEL || !valid_node_index(level, index) {
        return None;
    }
    let parent_width = level_width(level)?;
    let child_width = parent_width * 2;
    let index = usize::from(index);
    let px = index % parent_width;
    let pz = index / parent_width;
    let base = (pz * 2) * child_width + px * 2;
    Some([
        base as u16,
        (base + 1) as u16,
        (base + child_width) as u16,
        (base + child_width + 1) as u16,
    ])
}

fn hash_internal_node(
    worldgen_epoch: &str,
    dim_id: i32,
    rx: i32,
    rz: i32,
    level: u8,
    index: u16,
    children: &[Digest32; 4],
) -> Digest32 {
    let mut hasher = Sha256::new();
    hasher.update(RMK1_NODE_DOMAIN);
    hasher.update(&(worldgen_epoch.len() as u32).to_be_bytes());
    hasher.update(worldgen_epoch.as_bytes());
    hasher.update(&dim_id.to_be_bytes());
    hasher.update(&rx.to_be_bytes());
    hasher.update(&rz.to_be_bytes());
    hasher.update(&[level]);
    hasher.update(&index.to_be_bytes());
    for child in children {
        hasher.update(child);
    }
    hasher.finalize()
}

/// Errors produced by the region tree, diff negotiation or `RMK1` codec.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegionMerkleError {
    /// Local chunk coordinate was outside `0..32`.
    InvalidLocalChunkCoordinate { lx: u8, lz: u8 },
    /// The tree row was outside `0..=5` or a node index exceeded its row.
    InvalidNode { level: u8, index: u16 },
    /// A chunk digest slice was not exactly 1,024 entries.
    InvalidChunkHashCount { count: usize },
    /// Region multiplication or addition overflowed world chunk coordinates.
    CoordinateOverflow,
    /// The two trees are committed to different world-generation epochs.
    EpochMismatch {
        /// Epoch of the reference tree.
        expected: String,
        /// Epoch of the compared tree.
        actual: String,
    },
    /// The two trees are for different dimensions.
    DimensionMismatch { expected: i32, actual: i32 },
    /// The two trees are for different region coordinates.
    RegionMismatch {
        expected: (i32, i32),
        actual: (i32, i32),
    },
    /// Packet magic was not `RMK1`.
    BadMagic,
    /// Packet schema was not version 1.
    UnsupportedSchemaVersion(u16),
    /// Packet tag byte has no defined meaning.
    UnknownTag(u8),
    /// CRC-32 seal did not match the preceding bytes.
    CrcMismatch,
    /// Input ended before a required field.
    Truncated,
    /// Input had bytes the packet grammar does not consume.
    TrailingGarbage,
    /// Input exceeded the absolute packet bound.
    Oversize { length: usize, max: usize },
    /// World-generation epoch was empty or exceeded the wire cap.
    InvalidEpochLength { length: usize, max: usize },
    /// Region-root count exceeded the bounded codec cap.
    TooManyRegionRoots { count: usize, max: usize },
    /// Subtree node count exceeded the bounded codec cap.
    TooManySubtreeNodes { count: usize, max: usize },
    /// A canonical ordering rule was violated, or an entry appeared twice.
    UnsortedOrDuplicate(&'static str),
    /// Low-level bounded reader failure.
    Wire(ProtoError),
}

impl fmt::Display for RegionMerkleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLocalChunkCoordinate { lx, lz } => {
                write!(f, "local chunk coordinate ({lx}, {lz}) is outside 0..32")
            }
            Self::InvalidNode { level, index } => {
                write!(f, "invalid region Merkle node level {level} index {index}")
            }
            Self::InvalidChunkHashCount { count } => {
                write!(f, "chunk hash count {count} is not {REGION_CHUNK_COUNT}")
            }
            Self::CoordinateOverflow => {
                write!(f, "region coordinate overflows world chunk coordinate")
            }
            Self::EpochMismatch { expected, actual } => {
                write!(
                    f,
                    "worldgen epoch mismatch: expected {expected}, got {actual}"
                )
            }
            Self::DimensionMismatch { expected, actual } => {
                write!(f, "dimension mismatch: expected {expected}, got {actual}")
            }
            Self::RegionMismatch { expected, actual } => {
                write!(f, "region mismatch: expected {expected:?}, got {actual:?}")
            }
            Self::BadMagic => write!(f, "bad RMK1 magic"),
            Self::UnsupportedSchemaVersion(v) => write!(f, "unsupported RMK1 schema version {v}"),
            Self::UnknownTag(tag) => write!(f, "unknown RMK1 packet tag {tag:#04x}"),
            Self::CrcMismatch => write!(f, "RMK1 CRC-32 mismatch"),
            Self::Truncated => write!(f, "truncated RMK1 packet"),
            Self::TrailingGarbage => write!(f, "trailing bytes after RMK1 packet"),
            Self::Oversize { length, max } => {
                write!(f, "RMK1 packet of {length} bytes exceeds bound {max}")
            }
            Self::InvalidEpochLength { length, max } => {
                write!(
                    f,
                    "worldgen epoch of {length} bytes is empty or exceeds bound {max}"
                )
            }
            Self::TooManyRegionRoots { count, max } => {
                write!(f, "{count} region roots exceeds bound {max}")
            }
            Self::TooManySubtreeNodes { count, max } => {
                write!(f, "{count} subtree nodes exceeds bound {max}")
            }
            Self::UnsortedOrDuplicate(what) => {
                write!(f, "unsorted or duplicate {what} in RMK1 packet")
            }
            Self::Wire(error) => write!(f, "RMK1 wire error: {error}"),
        }
    }
}

impl std::error::Error for RegionMerkleError {}

impl From<ProtoError> for RegionMerkleError {
    fn from(value: ProtoError) -> Self {
        Self::Wire(value)
    }
}

/// A 32x32-chunk spatial region Merkle tree.
///
/// Chunk inputs are already digests of chunk content. The tree stores those
/// digests as the leaf row and domain-separates every internal node. Because
/// internal preimages include the node position and region identity, equal
/// content in different regions, dimensions, epochs or quadrants commits to
/// different roots.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegionMerkleTree {
    worldgen_epoch: String,
    dim_id: i32,
    rx: i32,
    rz: i32,
    /// Concatenated rows from level 0 (root) through level 5 (leaves).
    nodes: Vec<Digest32>,
}

impl RegionMerkleTree {
    /// Construct a region whose 1,024 chunk leaves are the canonical empty
    /// digest (`[0; 32]`). Callers normally populate leaves with
    /// [`Self::update_chunk_hash`] or construct a populated tree with
    /// [`Self::with_chunk_hashes`].
    pub fn new(worldgen_epoch: impl Into<String>, dim_id: i32, rx: i32, rz: i32) -> Self {
        let leaves = vec![[0u8; 32]; REGION_CHUNK_COUNT];
        Self::build(worldgen_epoch.into(), dim_id, rx, rz, leaves)
    }

    /// Construct a fully populated tree from exactly 1,024 row-major chunk
    /// digests (`lz * 32 + lx`).
    pub fn with_chunk_hashes(
        worldgen_epoch: impl Into<String>,
        dim_id: i32,
        rx: i32,
        rz: i32,
        chunk_hashes: &[[u8; 32]],
    ) -> Result<Self, RegionMerkleError> {
        if chunk_hashes.len() != REGION_CHUNK_COUNT {
            return Err(RegionMerkleError::InvalidChunkHashCount {
                count: chunk_hashes.len(),
            });
        }
        Ok(Self::build(
            worldgen_epoch.into(),
            dim_id,
            rx,
            rz,
            chunk_hashes.to_vec(),
        ))
    }

    fn build(worldgen_epoch: String, dim_id: i32, rx: i32, rz: i32, leaves: Vec<Digest32>) -> Self {
        debug_assert_eq!(leaves.len(), REGION_CHUNK_COUNT);
        let total_nodes = level_offset(REGION_LEAF_LEVEL) + REGION_CHUNK_COUNT;
        let mut tree = Self {
            worldgen_epoch,
            dim_id,
            rx,
            rz,
            nodes: vec![[0u8; 32]; total_nodes],
        };
        let leaf_offset = level_offset(REGION_LEAF_LEVEL);
        tree.nodes[leaf_offset..].copy_from_slice(&leaves);
        tree.recompute_internal_nodes();
        tree
    }

    /// World-generation epoch committed into every internal node.
    pub fn worldgen_epoch(&self) -> &str {
        &self.worldgen_epoch
    }

    /// Numeric dimension id committed into every internal node.
    pub fn dim_id(&self) -> i32 {
        self.dim_id
    }

    /// Anvil region X coordinate.
    pub fn rx(&self) -> i32 {
        self.rx
    }

    /// Anvil region Z coordinate.
    pub fn rz(&self) -> i32 {
        self.rz
    }

    /// Root commitment for all 1,024 chunks in the region.
    pub fn root(&self) -> Digest32 {
        self.nodes[0]
    }

    /// Number of nodes stored across all six rows.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Row-major leaf digest at local chunk coordinate `(lx, lz)`.
    pub fn leaf_hash(&self, lx: u8, lz: u8) -> Option<&Digest32> {
        if lx as usize >= REGION_WIDTH || lz as usize >= REGION_WIDTH {
            return None;
        }
        let index = usize::from(lz) * REGION_WIDTH + usize::from(lx);
        Some(&self.nodes[level_offset(REGION_LEAF_LEVEL) + index])
    }

    /// Node digest at a quadtree row/index, or `None` when the coordinates do
    /// not name a node.
    pub fn node_hash(&self, level: u8, index: u16) -> Option<&Digest32> {
        flat_node_index(level, index).map(|flat| &self.nodes[flat])
    }

    /// The four canonical child `(index, hash)` pairs for an internal node.
    pub fn child_node_hashes(&self, level: u8, index: u16) -> Option<[(u16, Digest32); 4]> {
        let children = child_indices(level, index)?;
        let mut out = [(0u16, [0u8; 32]); 4];
        for (slot, child) in children.into_iter().enumerate() {
            let hash = *self.node_hash(level + 1, child)?;
            out[slot] = (child, hash);
        }
        Some(out)
    }

    /// Replace one leaf digest and recompute exactly the five internal
    /// ancestors on the path to the root. All other nodes retain their bytes.
    pub fn update_chunk_hash(
        &mut self,
        lx: u8,
        lz: u8,
        chunk_hash: Digest32,
    ) -> Result<(), RegionMerkleError> {
        if lx as usize >= REGION_WIDTH || lz as usize >= REGION_WIDTH {
            return Err(RegionMerkleError::InvalidLocalChunkCoordinate { lx, lz });
        }
        let mut child_index = usize::from(lz) * REGION_WIDTH + usize::from(lx);
        let leaf_offset = level_offset(REGION_LEAF_LEVEL);
        self.nodes[leaf_offset + child_index] = chunk_hash;

        for level in (0..usize::from(REGION_TREE_DEPTH)).rev() {
            let child_level = u8::try_from(level + 1).unwrap_or(REGION_LEAF_LEVEL);
            let child_width = level_width(child_level).unwrap_or(REGION_WIDTH);
            let parent_width = child_width / 2;
            let cx = child_index % child_width;
            let cz = child_index / child_width;
            let parent_index = (cz / 2) * parent_width + (cx / 2);
            let base = (cz & !1) * child_width + (cx & !1);
            let child_flat_base = level_offset(child_level);
            let children = [
                self.nodes[child_flat_base + base],
                self.nodes[child_flat_base + base + 1],
                self.nodes[child_flat_base + base + child_width],
                self.nodes[child_flat_base + base + child_width + 1],
            ];
            let level_u8 = u8::try_from(level).unwrap_or(REGION_TREE_DEPTH);
            let parent_index_u16 = u16::try_from(parent_index).unwrap_or(u16::MAX);
            let digest = hash_internal_node(
                &self.worldgen_epoch,
                self.dim_id,
                self.rx,
                self.rz,
                level_u8,
                parent_index_u16,
                &children,
            );
            self.nodes[level_offset(level_u8) + parent_index] = digest;
            child_index = parent_index;
        }
        Ok(())
    }

    fn recompute_internal_nodes(&mut self) {
        for level in (0..usize::from(REGION_TREE_DEPTH)).rev() {
            let level_u8 = u8::try_from(level).unwrap_or(REGION_TREE_DEPTH);
            let width = level_width(level_u8).unwrap_or(1);
            for parent_index in 0..width * width {
                let parent_index_u16 = u16::try_from(parent_index).unwrap_or(u16::MAX);
                let Some(indices) = child_indices(level_u8, parent_index_u16) else {
                    continue;
                };
                let child_level = level_u8 + 1;
                let child_flat_base = level_offset(child_level);
                let children = [
                    self.nodes[child_flat_base + usize::from(indices[0])],
                    self.nodes[child_flat_base + usize::from(indices[1])],
                    self.nodes[child_flat_base + usize::from(indices[2])],
                    self.nodes[child_flat_base + usize::from(indices[3])],
                ];
                self.nodes[level_offset(level_u8) + parent_index] = hash_internal_node(
                    &self.worldgen_epoch,
                    self.dim_id,
                    self.rx,
                    self.rz,
                    level_u8,
                    parent_index_u16,
                    &children,
                );
            }
        }
    }

    /// Fail unless two trees authenticate the same epoch, dimension and
    /// region. Epoch disagreement is deliberately reported first because it
    /// is a hard world-format boundary.
    pub fn validate_compatible(&self, other: &Self) -> Result<(), RegionMerkleError> {
        if self.worldgen_epoch != other.worldgen_epoch {
            return Err(RegionMerkleError::EpochMismatch {
                expected: self.worldgen_epoch.clone(),
                actual: other.worldgen_epoch.clone(),
            });
        }
        if self.dim_id != other.dim_id {
            return Err(RegionMerkleError::DimensionMismatch {
                expected: self.dim_id,
                actual: other.dim_id,
            });
        }
        if (self.rx, self.rz) != (other.rx, other.rz) {
            return Err(RegionMerkleError::RegionMismatch {
                expected: (self.rx, self.rz),
                actual: (other.rx, other.rz),
            });
        }
        Ok(())
    }

    /// Convert a valid local chunk coordinate to world chunk coordinates.
    pub fn world_chunk_coord(&self, lx: u8, lz: u8) -> Result<(i32, i32), RegionMerkleError> {
        if lx as usize >= REGION_WIDTH || lz as usize >= REGION_WIDTH {
            return Err(RegionMerkleError::InvalidLocalChunkCoordinate { lx, lz });
        }
        let x = self
            .rx
            .checked_mul(REGION_WIDTH as i32)
            .and_then(|base| base.checked_add(i32::from(lx)))
            .ok_or(RegionMerkleError::CoordinateOverflow)?;
        let z = self
            .rz
            .checked_mul(REGION_WIDTH as i32)
            .and_then(|base| base.checked_add(i32::from(lz)))
            .ok_or(RegionMerkleError::CoordinateOverflow)?;
        Ok((x, z))
    }

    /// Exact world-chunk coordinates whose leaf digests differ. Equal roots
    /// prune the entire region without visiting the 1,024 leaves; mismatched
    /// roots descend only unequal quadrants.
    pub fn diff_chunk_coords(&self, other: &Self) -> Result<Vec<(i32, i32)>, RegionMerkleError> {
        self.validate_compatible(other)?;
        if self.root() == other.root() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        let mut stack = vec![(REGION_ROOT_LEVEL, 0u16)];
        while let Some((level, index)) = stack.pop() {
            let mine = self.node_hash(level, index);
            let theirs = other.node_hash(level, index);
            if mine == theirs {
                continue;
            }
            if level == REGION_LEAF_LEVEL {
                let lx = u8::try_from(usize::from(index) % REGION_WIDTH).unwrap_or(u8::MAX);
                let lz = u8::try_from(usize::from(index) / REGION_WIDTH).unwrap_or(u8::MAX);
                out.push(self.world_chunk_coord(lx, lz)?);
                continue;
            }
            if let Some(children) = child_indices(level, index) {
                for child in children.into_iter().rev() {
                    stack.push((level + 1, child));
                }
            }
        }
        out.sort_unstable();
        Ok(out)
    }
}

/// `RMK1` packet payload. All integer fields are big-endian and the complete
/// packet is sealed by a trailing IEEE CRC-32 over every preceding byte.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegionMerklePacket {
    /// C2S region-root summary: 32 bytes per 1,024 chunks.
    ClientRegionRoots {
        /// Client world-generation epoch.
        worldgen_epoch: String,
        /// Numeric dimension id.
        dim_id: i32,
        /// `(rx, rz, root)` triples.
        regions: Vec<(i32, i32, Digest32)>,
    },
    /// S2C positive match: all 1,024 chunks in this region are identical.
    ServerRegionMatch {
        /// Numeric dimension id.
        dim_id: i32,
        /// Region X coordinate.
        rx: i32,
        /// Region Z coordinate.
        rz: i32,
    },
    /// S2C hashes for nodes on one quadtree row.
    ServerSubtreeHashes {
        /// Numeric dimension id.
        dim_id: i32,
        /// Region X coordinate.
        rx: i32,
        /// Region Z coordinate.
        rz: i32,
        /// Row represented by `nodes` (0 root through 5 leaves).
        level: u8,
        /// `(node_index, node_hash)` pairs.
        nodes: Vec<(u16, Digest32)>,
    },
    /// C2S hashes for the subset of row nodes still mismatched.
    ClientSubtreeHashes {
        /// Numeric dimension id.
        dim_id: i32,
        /// Region X coordinate.
        rx: i32,
        /// Region Z coordinate.
        rz: i32,
        /// Row represented by `nodes` (0 root through 5 leaves).
        level: u8,
        /// `(node_index, node_hash)` pairs.
        nodes: Vec<(u16, Digest32)>,
    },
    /// S2C fail-closed rejection when the world-generation epochs differ.
    EpochMismatch {
        /// Numeric dimension id.
        dim_id: i32,
        /// Server-authoritative epoch.
        expected_worldgen_epoch: String,
        /// Epoch supplied by the client.
        actual_worldgen_epoch: String,
    },
}

impl RegionMerklePacket {
    /// Encode this packet with the `RMK1` magic, schema version and CRC seal.
    pub fn encode(&self) -> Result<Vec<u8>, RegionMerkleError> {
        encode_region_merkle_packet(self)
    }

    /// Decode one exact `RMK1` packet. Truncation, trailing bytes, semantic
    /// bound violations and CRC mismatches all fail closed.
    pub fn decode(bytes: &[u8]) -> Result<Self, RegionMerkleError> {
        decode_region_merkle_packet(bytes)
    }
}

fn validate_wire_epoch(epoch: &str) -> Result<(), RegionMerkleError> {
    if epoch.is_empty() || epoch.len() > MAX_RMK1_EPOCH_BYTES {
        return Err(RegionMerkleError::InvalidEpochLength {
            length: epoch.len(),
            max: MAX_RMK1_EPOCH_BYTES,
        });
    }
    Ok(())
}

fn write_epoch(out: &mut Vec<u8>, epoch: &str) -> Result<(), RegionMerkleError> {
    validate_wire_epoch(epoch)?;
    out.extend_from_slice(&(epoch.len() as u16).to_be_bytes());
    out.extend_from_slice(epoch.as_bytes());
    Ok(())
}

fn write_subtree(
    out: &mut Vec<u8>,
    dim_id: i32,
    rx: i32,
    rz: i32,
    level: u8,
    nodes: &[(u16, Digest32)],
) -> Result<(), RegionMerkleError> {
    validate_subtree_nodes(level, nodes)?;
    out.extend_from_slice(&dim_id.to_be_bytes());
    out.extend_from_slice(&rx.to_be_bytes());
    out.extend_from_slice(&rz.to_be_bytes());
    out.push(level);
    out.extend_from_slice(&(nodes.len() as u16).to_be_bytes());
    for &(index, hash) in nodes {
        out.extend_from_slice(&index.to_be_bytes());
        out.extend_from_slice(&hash);
    }
    Ok(())
}

fn validate_region_roots(regions: &[(i32, i32, Digest32)]) -> Result<(), RegionMerkleError> {
    if regions.len() > MAX_RMK1_REGION_ROOTS {
        return Err(RegionMerkleError::TooManyRegionRoots {
            count: regions.len(),
            max: MAX_RMK1_REGION_ROOTS,
        });
    }
    if regions
        .windows(2)
        .any(|pair| (pair[0].0, pair[0].1) >= (pair[1].0, pair[1].1))
    {
        return Err(RegionMerkleError::UnsortedOrDuplicate("region coordinates"));
    }
    Ok(())
}

fn validate_subtree_nodes(level: u8, nodes: &[(u16, Digest32)]) -> Result<(), RegionMerkleError> {
    if level > REGION_LEAF_LEVEL {
        return Err(RegionMerkleError::InvalidNode { level, index: 0 });
    }
    if nodes.len() > MAX_RMK1_SUBTREE_NODES {
        return Err(RegionMerkleError::TooManySubtreeNodes {
            count: nodes.len(),
            max: MAX_RMK1_SUBTREE_NODES,
        });
    }
    if nodes.windows(2).any(|pair| pair[0].0 >= pair[1].0) {
        return Err(RegionMerkleError::UnsortedOrDuplicate(
            "subtree node indices",
        ));
    }
    for &(index, _) in nodes {
        if !valid_node_index(level, index) {
            return Err(RegionMerkleError::InvalidNode { level, index });
        }
    }
    Ok(())
}

fn finish_rmk1(mut body: Vec<u8>) -> Result<Vec<u8>, RegionMerkleError> {
    if body.len() + 4 > MAX_RMK1_PACKET_BYTES {
        return Err(RegionMerkleError::Oversize {
            length: body.len() + 4,
            max: MAX_RMK1_PACKET_BYTES,
        });
    }
    let crc = crc32_ieee(&body);
    body.extend_from_slice(&crc.to_be_bytes());
    Ok(body)
}

/// Encode an [`RegionMerklePacket`] as exact `RMK1` bytes.
pub fn encode_region_merkle_packet(
    packet: &RegionMerklePacket,
) -> Result<Vec<u8>, RegionMerkleError> {
    let mut body = Vec::with_capacity(128);
    body.extend_from_slice(&REGION_MERKLE_MAGIC);
    body.extend_from_slice(&REGION_MERKLE_SCHEMA_VERSION.to_be_bytes());
    match packet {
        RegionMerklePacket::ClientRegionRoots {
            worldgen_epoch,
            dim_id,
            regions,
        } => {
            validate_region_roots(regions)?;
            body.push(TAG_CLIENT_REGION_ROOTS);
            write_epoch(&mut body, worldgen_epoch)?;
            body.extend_from_slice(&dim_id.to_be_bytes());
            body.extend_from_slice(&(regions.len() as u16).to_be_bytes());
            for &(rx, rz, root) in regions {
                body.extend_from_slice(&rx.to_be_bytes());
                body.extend_from_slice(&rz.to_be_bytes());
                body.extend_from_slice(&root);
            }
        }
        RegionMerklePacket::ServerRegionMatch { dim_id, rx, rz } => {
            body.push(TAG_SERVER_REGION_MATCH);
            body.extend_from_slice(&dim_id.to_be_bytes());
            body.extend_from_slice(&rx.to_be_bytes());
            body.extend_from_slice(&rz.to_be_bytes());
        }
        RegionMerklePacket::ServerSubtreeHashes {
            dim_id,
            rx,
            rz,
            level,
            nodes,
        } => {
            body.push(TAG_SERVER_SUBTREE_HASHES);
            write_subtree(&mut body, *dim_id, *rx, *rz, *level, nodes)?;
        }
        RegionMerklePacket::ClientSubtreeHashes {
            dim_id,
            rx,
            rz,
            level,
            nodes,
        } => {
            body.push(TAG_CLIENT_SUBTREE_HASHES);
            write_subtree(&mut body, *dim_id, *rx, *rz, *level, nodes)?;
        }
        RegionMerklePacket::EpochMismatch {
            dim_id,
            expected_worldgen_epoch,
            actual_worldgen_epoch,
        } => {
            body.push(TAG_EPOCH_MISMATCH);
            body.extend_from_slice(&dim_id.to_be_bytes());
            write_epoch(&mut body, expected_worldgen_epoch)?;
            write_epoch(&mut body, actual_worldgen_epoch)?;
        }
    }
    finish_rmk1(body)
}

fn read_epoch(reader: &mut WireReader<'_>) -> Result<String, RegionMerkleError> {
    let len = usize::from(reader.read_u16()?);
    if len == 0 || len > MAX_RMK1_EPOCH_BYTES {
        return Err(RegionMerkleError::InvalidEpochLength {
            length: len,
            max: MAX_RMK1_EPOCH_BYTES,
        });
    }
    let bytes = reader.take(len)?;
    String::from_utf8(bytes.to_vec()).map_err(|_| ProtoError::InvalidUtf8.into())
}

type SubtreeFields = (i32, i32, i32, u8, Vec<(u16, Digest32)>);

fn read_subtree(reader: &mut WireReader<'_>) -> Result<SubtreeFields, RegionMerkleError> {
    let dim_id = reader.read_i32()?;
    let rx = reader.read_i32()?;
    let rz = reader.read_i32()?;
    let level = reader.read_u8()?;
    let count = usize::from(reader.read_u16()?);
    if count > MAX_RMK1_SUBTREE_NODES {
        return Err(RegionMerkleError::TooManySubtreeNodes {
            count,
            max: MAX_RMK1_SUBTREE_NODES,
        });
    }
    let mut nodes = Vec::with_capacity(count);
    for _ in 0..count {
        let index = reader.read_u16()?;
        let hash_bytes = reader.take(32)?;
        let mut hash = [0u8; 32];
        hash.copy_from_slice(hash_bytes);
        if level > REGION_LEAF_LEVEL || !valid_node_index(level, index) {
            return Err(RegionMerkleError::InvalidNode { level, index });
        }
        nodes.push((index, hash));
    }
    validate_subtree_nodes(level, &nodes)?;
    Ok((dim_id, rx, rz, level, nodes))
}

/// Decode exact `RMK1` bytes into a [`RegionMerklePacket`].
pub fn decode_region_merkle_packet(bytes: &[u8]) -> Result<RegionMerklePacket, RegionMerkleError> {
    if bytes.len() < RMK1_MIN_PACKET_BYTES {
        return Err(RegionMerkleError::Truncated);
    }
    if bytes.len() > MAX_RMK1_PACKET_BYTES {
        return Err(RegionMerkleError::Oversize {
            length: bytes.len(),
            max: MAX_RMK1_PACKET_BYTES,
        });
    }
    if bytes[..4] != REGION_MERKLE_MAGIC {
        return Err(RegionMerkleError::BadMagic);
    }
    let schema_version = u16::from_be_bytes([bytes[4], bytes[5]]);
    if schema_version != REGION_MERKLE_SCHEMA_VERSION {
        return Err(RegionMerkleError::UnsupportedSchemaVersion(schema_version));
    }
    let crc_offset = bytes.len() - 4;
    let stored_crc = u32::from_be_bytes([
        bytes[crc_offset],
        bytes[crc_offset + 1],
        bytes[crc_offset + 2],
        bytes[crc_offset + 3],
    ]);
    if crc32_ieee(&bytes[..crc_offset]) != stored_crc {
        return Err(RegionMerkleError::CrcMismatch);
    }
    let body = &bytes[6..crc_offset];
    let tag = body[0];
    let mut reader = WireReader::new(&body[1..]);
    let packet = match tag {
        TAG_CLIENT_REGION_ROOTS => {
            let worldgen_epoch = read_epoch(&mut reader)?;
            let dim_id = reader.read_i32()?;
            let count = usize::from(reader.read_u16()?);
            if count > MAX_RMK1_REGION_ROOTS {
                return Err(RegionMerkleError::TooManyRegionRoots {
                    count,
                    max: MAX_RMK1_REGION_ROOTS,
                });
            }
            let mut regions = Vec::with_capacity(count);
            for _ in 0..count {
                let rx = reader.read_i32()?;
                let rz = reader.read_i32()?;
                let root_bytes = reader.take(32)?;
                let mut root = [0u8; 32];
                root.copy_from_slice(root_bytes);
                regions.push((rx, rz, root));
            }
            validate_region_roots(&regions)?;
            RegionMerklePacket::ClientRegionRoots {
                worldgen_epoch,
                dim_id,
                regions,
            }
        }
        TAG_SERVER_REGION_MATCH => RegionMerklePacket::ServerRegionMatch {
            dim_id: reader.read_i32()?,
            rx: reader.read_i32()?,
            rz: reader.read_i32()?,
        },
        TAG_SERVER_SUBTREE_HASHES => {
            let (dim_id, rx, rz, level, nodes) = read_subtree(&mut reader)?;
            RegionMerklePacket::ServerSubtreeHashes {
                dim_id,
                rx,
                rz,
                level,
                nodes,
            }
        }
        TAG_CLIENT_SUBTREE_HASHES => {
            let (dim_id, rx, rz, level, nodes) = read_subtree(&mut reader)?;
            RegionMerklePacket::ClientSubtreeHashes {
                dim_id,
                rx,
                rz,
                level,
                nodes,
            }
        }
        TAG_EPOCH_MISMATCH => RegionMerklePacket::EpochMismatch {
            dim_id: reader.read_i32()?,
            expected_worldgen_epoch: read_epoch(&mut reader)?,
            actual_worldgen_epoch: read_epoch(&mut reader)?,
        },
        other => return Err(RegionMerkleError::UnknownTag(other)),
    };
    if reader.remaining() != 0 {
        return Err(RegionMerkleError::TrailingGarbage);
    }
    Ok(packet)
}

/// Result of a deterministic logarithmic region-diff negotiation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegionDiffNegotiation {
    /// Exact dirty world-chunk coordinates in canonical ascending `(x, z)` order.
    pub dirty_chunks: Vec<(i32, i32)>,
    /// Server messages emitted after the initial root comparison.
    pub server_packets: Vec<RegionMerklePacket>,
    /// Client responses carrying only still-mismatched row nodes.
    pub client_packets: Vec<RegionMerklePacket>,
    /// Whether the compared region roots matched.
    pub matched: bool,
}

impl RegionDiffNegotiation {
    /// Total subtree/match messages modelled after the client root summary.
    pub fn message_count(&self) -> usize {
        self.server_packets.len() + self.client_packets.len()
    }
}

/// Deterministically model the `RMK1` hierarchical descent between a client
/// and server tree for one region. The model emits complete packets rather
/// than comparing hidden state: each server row disclosure is answered by the
/// client subset that remains mismatched, and the leaf row yields exact
/// world-chunk coordinates.
pub fn negotiate_region_diff(
    client: &RegionMerkleTree,
    server: &RegionMerkleTree,
) -> Result<RegionDiffNegotiation, RegionMerkleError> {
    validate_wire_epoch(server.worldgen_epoch())?;
    validate_wire_epoch(client.worldgen_epoch())?;
    server.validate_compatible(client)?;
    let dim_id = server.dim_id();
    let rx = server.rx();
    let rz = server.rz();
    if client.root() == server.root() {
        return Ok(RegionDiffNegotiation {
            dirty_chunks: Vec::new(),
            server_packets: vec![RegionMerklePacket::ServerRegionMatch { dim_id, rx, rz }],
            client_packets: Vec::new(),
            matched: true,
        });
    }

    let mut server_packets = Vec::new();
    let mut client_packets = Vec::new();
    let mut frontier = vec![0u16];
    let mut dirty_chunks = Vec::new();

    for level in REGION_ROOT_LEVEL..REGION_LEAF_LEVEL {
        let child_level = level + 1;
        let mut server_nodes = Vec::new();
        let mut seen = BTreeSet::new();
        for &parent in &frontier {
            let Some(children) = server.child_node_hashes(level, parent) else {
                return Err(RegionMerkleError::InvalidNode {
                    level,
                    index: parent,
                });
            };
            for (child_index, child_hash) in children {
                if seen.insert(child_index) {
                    server_nodes.push((child_index, child_hash));
                }
            }
        }
        server_nodes.sort_by_key(|&(index, _)| index);
        server_packets.push(RegionMerklePacket::ServerSubtreeHashes {
            dim_id,
            rx,
            rz,
            level: child_level,
            nodes: server_nodes.clone(),
        });

        let mut mismatched = Vec::new();
        for (index, server_hash) in server_nodes {
            let client_hash = client.node_hash(child_level, index).copied().ok_or(
                RegionMerkleError::InvalidNode {
                    level: child_level,
                    index,
                },
            )?;
            if client_hash != server_hash {
                mismatched.push((index, client_hash));
            }
        }
        client_packets.push(RegionMerklePacket::ClientSubtreeHashes {
            dim_id,
            rx,
            rz,
            level: child_level,
            nodes: mismatched.clone(),
        });

        if child_level == REGION_LEAF_LEVEL {
            for (index, _) in &mismatched {
                let lx = u8::try_from(usize::from(*index) % REGION_WIDTH).unwrap_or(u8::MAX);
                let lz = u8::try_from(usize::from(*index) / REGION_WIDTH).unwrap_or(u8::MAX);
                dirty_chunks.push(server.world_chunk_coord(lx, lz)?);
            }
        } else {
            frontier = mismatched.iter().map(|&(index, _)| index).collect();
        }
    }
    dirty_chunks.sort_unstable();
    Ok(RegionDiffNegotiation {
        dirty_chunks,
        server_packets,
        client_packets,
        matched: false,
    })
}

/// Canonical `ZTR1` frame magic.
pub const BLIND_RELAY_MAGIC: [u8; 4] = *b"ZTR1";
/// Short alias for [`BLIND_RELAY_MAGIC`].
pub const ZTR1_MAGIC: [u8; 4] = BLIND_RELAY_MAGIC;
/// Only schema version currently defined.
pub const BLIND_RELAY_SCHEMA_VERSION: u16 = 1;
/// Absolute cap on the opaque AEAD payload in one frame.
pub const MAX_BLIND_PAYLOAD_BYTES: usize = 16 * 1024;
/// Fixed non-payload byte count in a `ZTR1` frame.
pub const BLIND_RELAY_FRAME_OVERHEAD_BYTES: usize = 90;
/// Absolute cap on a complete encoded `ZTR1` frame.
pub const MAX_BLIND_FRAME_BYTES: usize = BLIND_RELAY_FRAME_OVERHEAD_BYTES + MAX_BLIND_PAYLOAD_BYTES;

/// A zero-trust node identity: the exact 32-byte Ed25519 or X25519 public key.
///
/// The relay compares these bytes as opaque public identities. They are never
/// interpreted as network locations and no private key is present in this
/// module.
#[derive(Clone, Copy, Debug, Default, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct NodeId([u8; 32]);

impl NodeId {
    /// Wrap a 32-byte public key.
    pub const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Borrow the exact public-key bytes.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl From<[u8; 32]> for NodeId {
    fn from(value: [u8; 32]) -> Self {
        Self::new(value)
    }
}

/// Opaque transport endpoint handle owned by the embedding relay runtime.
///
/// This is deliberately not a socket address: the protocol identity remains
/// the [`NodeId`], while the integer only tells local sans-IO integration
/// which currently connected transport to write to.
pub type RelayEndpoint = u64;

/// One opaque authenticated ciphertext frame routed by public-key identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlindRelayFrame {
    /// Session agreed by the two endpoints during authenticated setup.
    pub session_id: u64,
    /// Sender's 32-byte public-key identity.
    pub source: NodeId,
    /// Recipient's 32-byte public-key identity.
    pub destination: NodeId,
    /// Sender identity migration epoch. Replayed or downgraded epochs are
    /// rejected by [`BlindRelayRouter`].
    pub migration_epoch: u32,
    /// Opaque AEAD ciphertext, including whatever nonce/tag encoding the
    /// end-to-end cipher suite chooses. The relay never parses it.
    pub payload: Vec<u8>,
}

impl BlindRelayFrame {
    /// Encoded byte length without heap encoding.
    pub fn encoded_len(&self) -> usize {
        BLIND_RELAY_FRAME_OVERHEAD_BYTES + self.payload.len()
    }

    /// Encode this frame with `ZTR1` magic, schema version and CRC seal.
    pub fn encode(&self) -> Result<Vec<u8>, BlindRelayError> {
        encode_blind_relay_frame(self)
    }

    /// Decode one exact `ZTR1` frame.
    pub fn decode(bytes: &[u8]) -> Result<Self, BlindRelayError> {
        decode_blind_relay_frame(bytes)
    }
}

/// Errors produced by `ZTR1` encoding, decoding or routing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlindRelayError {
    /// Frame magic was not `ZTR1`.
    BadMagic,
    /// Frame schema was not version 1.
    UnsupportedSchemaVersion(u16),
    /// CRC-32 seal did not match the preceding bytes.
    CrcMismatch,
    /// Input ended before a required field.
    Truncated,
    /// Input had bytes the frame grammar does not consume.
    TrailingGarbage,
    /// Payload exceeded the applicable cap.
    PayloadTooLarge { length: usize, max: usize },
    /// Complete frame exceeded the applicable cap.
    FrameTooLarge { length: usize, max: usize },
    /// A node public key is not registered on the relay.
    UnknownNode(NodeId),
    /// Ingress bytes arrived on an endpoint the relay has not registered.
    UnknownEndpoint(RelayEndpoint),
    /// The frame claims a source different from the authenticated ingress identity.
    IngressIdentityMismatch {
        /// Endpoint the bytes arrived on.
        endpoint: RelayEndpoint,
        /// Authenticated node currently bound to that endpoint.
        authenticated: NodeId,
        /// Source node claimed inside the frame.
        claimed: NodeId,
    },
    /// A transport endpoint handle was bound to a second node identity.
    EndpointAlreadyRegistered(RelayEndpoint),
    /// The session id does not exist.
    UnknownSession(u64),
    /// Session endpoints do not match the frame source/destination pair.
    SessionNodeMismatch { session_id: u64 },
    /// A peer was registered twice instead of migrated.
    PeerAlreadyRegistered(NodeId),
    /// A session id was opened twice.
    DuplicateSession(u64),
    /// Session endpoints were identical.
    InvalidNodePair,
    /// Migration epoch was replayed or downgraded.
    StaleMigrationEpoch { current: u32, presented: u32 },
    /// Per-session frame/byte token bucket rejected the frame.
    RateLimited,
    /// Relay output would exceed ingress bytes, violating the `<= 1` ratio.
    AmplificationViolation {
        input_bytes: usize,
        output_bytes: usize,
    },
    /// Low-level bounded reader failure.
    Wire(ProtoError),
}

impl fmt::Display for BlindRelayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadMagic => write!(f, "bad ZTR1 magic"),
            Self::UnsupportedSchemaVersion(v) => write!(f, "unsupported ZTR1 schema version {v}"),
            Self::CrcMismatch => write!(f, "ZTR1 CRC-32 mismatch"),
            Self::Truncated => write!(f, "truncated ZTR1 frame"),
            Self::TrailingGarbage => write!(f, "trailing bytes after ZTR1 frame"),
            Self::PayloadTooLarge { length, max } => {
                write!(f, "blind payload of {length} bytes exceeds bound {max}")
            }
            Self::FrameTooLarge { length, max } => {
                write!(f, "blind frame of {length} bytes exceeds bound {max}")
            }
            Self::UnknownNode(_) => write!(f, "frame references an unregistered node"),
            Self::UnknownEndpoint(endpoint) => {
                write!(f, "ingress endpoint {endpoint} is not registered")
            }
            Self::IngressIdentityMismatch { endpoint, .. } => {
                write!(f, "frame source does not match authenticated ingress endpoint {endpoint}")
            }
            Self::EndpointAlreadyRegistered(endpoint) => {
                write!(f, "relay endpoint {endpoint} is already registered")
            }
            Self::UnknownSession(session) => write!(f, "unknown blind relay session {session}"),
            Self::SessionNodeMismatch { session_id } => {
                write!(f, "frame endpoints do not match blind relay session {session_id}")
            }
            Self::PeerAlreadyRegistered(_) => write!(f, "blind relay peer is already registered"),
            Self::DuplicateSession(session) => write!(f, "duplicate blind relay session {session}"),
            Self::InvalidNodePair => write!(f, "blind relay session endpoints are identical"),
            Self::StaleMigrationEpoch { current, presented } => write!(
                f,
                "stale migration epoch {presented}; current epoch is {current}"
            ),
            Self::RateLimited => write!(f, "blind relay session rate limit exceeded"),
            Self::AmplificationViolation {
                input_bytes,
                output_bytes,
            } => write!(
                f,
                "blind relay amplification violation: {output_bytes} output bytes for {input_bytes} input bytes"
            ),
            Self::Wire(error) => write!(f, "ZTR1 wire error: {error}"),
        }
    }
}

impl std::error::Error for BlindRelayError {}

impl From<ProtoError> for BlindRelayError {
    fn from(value: ProtoError) -> Self {
        Self::Wire(value)
    }
}

/// Encode exact `ZTR1` bytes for a [`BlindRelayFrame`].
pub fn encode_blind_relay_frame(frame: &BlindRelayFrame) -> Result<Vec<u8>, BlindRelayError> {
    if frame.payload.len() > MAX_BLIND_PAYLOAD_BYTES {
        return Err(BlindRelayError::PayloadTooLarge {
            length: frame.payload.len(),
            max: MAX_BLIND_PAYLOAD_BYTES,
        });
    }
    let mut out = Vec::with_capacity(frame.encoded_len());
    out.extend_from_slice(&BLIND_RELAY_MAGIC);
    out.extend_from_slice(&BLIND_RELAY_SCHEMA_VERSION.to_be_bytes());
    out.extend_from_slice(&frame.session_id.to_be_bytes());
    out.extend_from_slice(frame.source.as_bytes());
    out.extend_from_slice(frame.destination.as_bytes());
    out.extend_from_slice(&frame.migration_epoch.to_be_bytes());
    out.extend_from_slice(&(frame.payload.len() as u32).to_be_bytes());
    out.extend_from_slice(&frame.payload);
    let crc = crc32_ieee(&out);
    out.extend_from_slice(&crc.to_be_bytes());
    Ok(out)
}

#[derive(Clone, Copy, Debug)]
struct BlindRelayHeader {
    session_id: u64,
    source: NodeId,
    destination: NodeId,
    migration_epoch: u32,
}

/// Parse the fixed-size routing header without checksumming or copying the
/// opaque payload. The full CRC-sealed decoder remains the only way to expose
/// a payload frame.
fn parse_blind_relay_header(bytes: &[u8]) -> Result<BlindRelayHeader, BlindRelayError> {
    if bytes.len() < BLIND_RELAY_FRAME_OVERHEAD_BYTES {
        return Err(BlindRelayError::Truncated);
    }
    if bytes.len() > MAX_BLIND_FRAME_BYTES {
        return Err(BlindRelayError::FrameTooLarge {
            length: bytes.len(),
            max: MAX_BLIND_FRAME_BYTES,
        });
    }
    if bytes[..4] != BLIND_RELAY_MAGIC {
        return Err(BlindRelayError::BadMagic);
    }
    let schema_version = u16::from_be_bytes([bytes[4], bytes[5]]);
    if schema_version != BLIND_RELAY_SCHEMA_VERSION {
        return Err(BlindRelayError::UnsupportedSchemaVersion(schema_version));
    }
    let payload_len = u32::from_be_bytes([bytes[82], bytes[83], bytes[84], bytes[85]]) as usize;
    if payload_len > MAX_BLIND_PAYLOAD_BYTES {
        return Err(BlindRelayError::PayloadTooLarge {
            length: payload_len,
            max: MAX_BLIND_PAYLOAD_BYTES,
        });
    }
    let expected_len = BLIND_RELAY_FRAME_OVERHEAD_BYTES
        .checked_add(payload_len)
        .ok_or(BlindRelayError::FrameTooLarge {
            length: usize::MAX,
            max: MAX_BLIND_FRAME_BYTES,
        })?;
    if bytes.len() < expected_len {
        return Err(BlindRelayError::Truncated);
    }
    if bytes.len() > expected_len {
        return Err(BlindRelayError::TrailingGarbage);
    }

    let session_id = u64::from_be_bytes(bytes[6..14].try_into().expect("fixed header"));
    let mut source = [0u8; 32];
    source.copy_from_slice(&bytes[14..46]);
    let mut destination = [0u8; 32];
    destination.copy_from_slice(&bytes[46..78]);
    let migration_epoch = u32::from_be_bytes(bytes[78..82].try_into().expect("fixed header"));
    Ok(BlindRelayHeader {
        session_id,
        source: NodeId::new(source),
        destination: NodeId::new(destination),
        migration_epoch,
    })
}

/// Decode exact `ZTR1` bytes into a [`BlindRelayFrame`].
pub fn decode_blind_relay_frame(bytes: &[u8]) -> Result<BlindRelayFrame, BlindRelayError> {
    if bytes.len() < BLIND_RELAY_FRAME_OVERHEAD_BYTES {
        return Err(BlindRelayError::Truncated);
    }
    if bytes.len() > MAX_BLIND_FRAME_BYTES {
        return Err(BlindRelayError::FrameTooLarge {
            length: bytes.len(),
            max: MAX_BLIND_FRAME_BYTES,
        });
    }
    if bytes[..4] != BLIND_RELAY_MAGIC {
        return Err(BlindRelayError::BadMagic);
    }
    let schema_version = u16::from_be_bytes([bytes[4], bytes[5]]);
    if schema_version != BLIND_RELAY_SCHEMA_VERSION {
        return Err(BlindRelayError::UnsupportedSchemaVersion(schema_version));
    }
    let payload_len = u32::from_be_bytes([bytes[82], bytes[83], bytes[84], bytes[85]]) as usize;
    if payload_len > MAX_BLIND_PAYLOAD_BYTES {
        return Err(BlindRelayError::PayloadTooLarge {
            length: payload_len,
            max: MAX_BLIND_PAYLOAD_BYTES,
        });
    }
    let expected_len = BLIND_RELAY_FRAME_OVERHEAD_BYTES + payload_len;
    if bytes.len() < expected_len {
        return Err(BlindRelayError::Truncated);
    }
    if bytes.len() > expected_len {
        return Err(BlindRelayError::TrailingGarbage);
    }
    let crc_offset = bytes.len() - 4;
    let stored_crc = u32::from_be_bytes([
        bytes[crc_offset],
        bytes[crc_offset + 1],
        bytes[crc_offset + 2],
        bytes[crc_offset + 3],
    ]);
    if crc32_ieee(&bytes[..crc_offset]) != stored_crc {
        return Err(BlindRelayError::CrcMismatch);
    }

    let mut reader = WireReader::new(&bytes[6..crc_offset]);
    let session_id = reader.read_u64()?;
    let mut source = [0u8; 32];
    source.copy_from_slice(reader.take(32)?);
    let mut destination = [0u8; 32];
    destination.copy_from_slice(reader.take(32)?);
    let migration_epoch = reader.read_u32()?;
    let declared_len = reader.read_u32()? as usize;
    let payload = reader.take(declared_len)?.to_vec();
    if reader.remaining() != 0 {
        return Err(BlindRelayError::TrailingGarbage);
    }
    Ok(BlindRelayFrame {
        session_id,
        source: NodeId::new(source),
        destination: NodeId::new(destination),
        migration_epoch,
        payload,
    })
}

/// Relay admission and anti-amplification policy.
///
/// The amplification ratio is fixed at `<= 1`; deployments can tighten size
/// and bandwidth caps but cannot widen the absolute `ZTR1` maxima.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlindRelayPolicy {
    /// Deployment payload cap, effectively clamped to [`MAX_BLIND_PAYLOAD_BYTES`].
    pub max_payload_bytes: usize,
    /// Deployment complete-frame cap, effectively clamped to [`MAX_BLIND_FRAME_BYTES`].
    pub max_frame_bytes: usize,
    /// Sustained per-session frame rate; one second is also the burst capacity.
    pub frames_per_second: u32,
    /// Sustained per-session byte rate; one second is also the burst capacity.
    pub bytes_per_second: u32,
}

impl Default for BlindRelayPolicy {
    fn default() -> Self {
        Self {
            max_payload_bytes: MAX_BLIND_PAYLOAD_BYTES,
            max_frame_bytes: MAX_BLIND_FRAME_BYTES,
            frames_per_second: 64,
            bytes_per_second: 64 * 1024,
        }
    }
}

impl BlindRelayPolicy {
    /// Fixed anti-amplification predicate: output must never exceed input.
    pub const fn amplification_permitted(input_bytes: usize, output_bytes: usize) -> bool {
        output_bytes <= input_bytes
    }

    fn effective_payload_cap(&self) -> usize {
        self.max_payload_bytes.min(MAX_BLIND_PAYLOAD_BYTES)
    }

    fn effective_frame_cap(&self) -> usize {
        self.max_frame_bytes.min(MAX_BLIND_FRAME_BYTES)
    }

    /// Validate semantic frame size under this deployment policy and return
    /// the encoded size.
    pub fn validate_frame(&self, frame: &BlindRelayFrame) -> Result<usize, BlindRelayError> {
        let payload_cap = self.effective_payload_cap();
        if frame.payload.len() > payload_cap {
            return Err(BlindRelayError::PayloadTooLarge {
                length: frame.payload.len(),
                max: payload_cap,
            });
        }
        let frame_len = frame.encoded_len();
        let frame_cap = self.effective_frame_cap();
        if frame_len > frame_cap {
            return Err(BlindRelayError::FrameTooLarge {
                length: frame_len,
                max: frame_cap,
            });
        }
        Ok(frame_len)
    }

    /// Check only the size-related wire prefix fields before the decoder
    /// checksums or copies a payload. This keeps oversized ingress cheap.
    fn validate_wire_sizes(&self, bytes: &[u8]) -> Result<(), BlindRelayError> {
        let frame_cap = self.effective_frame_cap();
        if bytes.len() > frame_cap {
            return Err(BlindRelayError::FrameTooLarge {
                length: bytes.len(),
                max: frame_cap,
            });
        }
        if bytes.len() < BLIND_RELAY_FRAME_OVERHEAD_BYTES {
            return Err(BlindRelayError::Truncated);
        }
        let payload_len = u32::from_be_bytes([bytes[82], bytes[83], bytes[84], bytes[85]]) as usize;
        let payload_cap = self.effective_payload_cap();
        if payload_len > payload_cap {
            return Err(BlindRelayError::PayloadTooLarge {
                length: payload_len,
                max: payload_cap,
            });
        }
        let expected_len = BLIND_RELAY_FRAME_OVERHEAD_BYTES
            .checked_add(payload_len)
            .ok_or(BlindRelayError::FrameTooLarge {
                length: usize::MAX,
                max: frame_cap,
            })?;
        if expected_len > frame_cap {
            return Err(BlindRelayError::FrameTooLarge {
                length: expected_len,
                max: frame_cap,
            });
        }
        Ok(())
    }
}

fn is_size_reject(error: &BlindRelayError) -> bool {
    matches!(
        error,
        BlindRelayError::PayloadTooLarge { .. } | BlindRelayError::FrameTooLarge { .. }
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PeerRegistration {
    endpoint: RelayEndpoint,
    migration_epoch: u32,
}

#[derive(Clone, Debug)]
struct BlindSessionState {
    first: NodeId,
    second: NodeId,
    frames: TokenBucket,
    bytes: TokenBucket,
}

impl BlindSessionState {
    fn matches(&self, source: NodeId, destination: NodeId) -> bool {
        (self.first == source && self.second == destination)
            || (self.first == destination && self.second == source)
    }

    fn allow(&mut self, now: Instant, frame_bytes: usize) -> bool {
        let frame_cost = u32::try_from(frame_bytes).unwrap_or(u32::MAX);
        let frames_ok = {
            let mut probe = self.frames.clone();
            probe.try_consume(now, 1)
        };
        let bytes_ok = {
            let mut probe = self.bytes.clone();
            probe.try_consume(now, frame_cost)
        };
        if !(frames_ok && bytes_ok) {
            return false;
        }
        self.frames.try_consume(now, 1);
        self.bytes.try_consume(now, frame_cost);
        true
    }
}

/// Counters for relay decisions. No plaintext or key material is exposed here.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BlindRelayMetrics {
    /// Frames admitted and forwarded.
    pub frames_forwarded: u64,
    /// Forwarded wire bytes.
    pub bytes_forwarded: u64,
    /// Frames rejected because token buckets or size caps refused them.
    pub rate_or_size_rejects: u64,
    /// Frames rejected because their migration epoch was stale or not yet migrated.
    pub stale_epoch_rejects: u64,
    /// Frames rejected because identity/session shape failed validation.
    pub identity_rejects: u64,
}

/// A single sans-IO relay forwarding decision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlindRelayDelivery {
    /// Recipient public identity.
    pub destination: NodeId,
    /// Opaque local endpoint handle for the recipient.
    pub endpoint: RelayEndpoint,
    /// Canonical frame being forwarded.
    pub frame: BlindRelayFrame,
    /// Exact canonical wire bytes to write. Their length never exceeds the
    /// ingress length supplied to the router.
    pub wire_bytes: Vec<u8>,
}

/// Pure sans-IO zero-trust blind relay router.
///
/// The router has no decryption keys, no plaintext access API, no socket
/// addresses and no cryptographic identity secrets. It only checks public-key
/// membership, session shape, migration epochs, size and token-budget policy,
/// then asks the embedding transport to forward opaque bytes.
#[derive(Clone, Debug)]
pub struct BlindRelayRouter {
    policy: BlindRelayPolicy,
    peers: HashMap<NodeId, PeerRegistration>,
    endpoints: HashMap<RelayEndpoint, NodeId>,
    sessions: HashMap<u64, BlindSessionState>,
    metrics: BlindRelayMetrics,
}

impl BlindRelayRouter {
    /// Construct an empty router.
    pub fn new(policy: BlindRelayPolicy) -> Self {
        Self {
            policy,
            peers: HashMap::new(),
            endpoints: HashMap::new(),
            sessions: HashMap::new(),
            metrics: BlindRelayMetrics::default(),
        }
    }

    /// Active policy.
    pub fn policy(&self) -> &BlindRelayPolicy {
        &self.policy
    }

    /// Relay metrics.
    pub fn metrics(&self) -> &BlindRelayMetrics {
        &self.metrics
    }

    /// Register a connected public-key identity at an opaque endpoint handle.
    pub fn register_peer(
        &mut self,
        node_id: NodeId,
        endpoint: RelayEndpoint,
        migration_epoch: u32,
    ) -> Result<(), BlindRelayError> {
        if self.peers.contains_key(&node_id) {
            return Err(BlindRelayError::PeerAlreadyRegistered(node_id));
        }
        if self.endpoints.contains_key(&endpoint) {
            return Err(BlindRelayError::EndpointAlreadyRegistered(endpoint));
        }
        self.endpoints.insert(endpoint, node_id);
        self.peers.insert(
            node_id,
            PeerRegistration {
                endpoint,
                migration_epoch,
            },
        );
        Ok(())
    }

    /// Remove a peer only when `endpoint` is still its current registration.
    /// A close event delayed across a host migration therefore cannot tear
    /// down the replacement endpoint or its sessions.
    pub fn unregister_peer(&mut self, node_id: NodeId, endpoint: RelayEndpoint) -> bool {
        match self.peers.get(&node_id) {
            Some(peer) if peer.endpoint == endpoint => self.remove_peer(node_id),
            _ => false,
        }
    }

    /// Administratively remove a peer and every session involving it,
    /// regardless of endpoint generation. Transport close paths should use
    /// [`Self::unregister_peer`] instead.
    pub fn remove_peer(&mut self, node_id: NodeId) -> bool {
        let removed = if let Some(peer) = self.peers.remove(&node_id) {
            self.endpoints.remove(&peer.endpoint);
            true
        } else {
            false
        };
        if removed {
            self.sessions
                .retain(|_, session| session.first != node_id && session.second != node_id);
        }
        removed
    }

    /// Current endpoint and epoch for a registered public key.
    pub fn peer_registration(&self, node_id: NodeId) -> Option<(RelayEndpoint, u32)> {
        self.peers
            .get(&node_id)
            .map(|peer| (peer.endpoint, peer.migration_epoch))
    }

    /// Move a public-key identity to a new endpoint. The epoch must strictly
    /// increase; equality is a replay and lower values are downgrades.
    pub fn migrate_peer_endpoint(
        &mut self,
        node_id: NodeId,
        new_migration_epoch: u32,
        new_endpoint: RelayEndpoint,
    ) -> Result<(), BlindRelayError> {
        let (old_endpoint, current_epoch) = {
            let peer = self
                .peers
                .get(&node_id)
                .ok_or(BlindRelayError::UnknownNode(node_id))?;
            (peer.endpoint, peer.migration_epoch)
        };
        if new_migration_epoch <= current_epoch {
            return Err(BlindRelayError::StaleMigrationEpoch {
                current: current_epoch,
                presented: new_migration_epoch,
            });
        }
        if self
            .endpoints
            .get(&new_endpoint)
            .is_some_and(|owner| *owner != node_id)
        {
            return Err(BlindRelayError::EndpointAlreadyRegistered(new_endpoint));
        }
        if let Some(peer) = self.peers.get_mut(&node_id) {
            peer.migration_epoch = new_migration_epoch;
            peer.endpoint = new_endpoint;
        }
        self.endpoints.remove(&old_endpoint);
        self.endpoints.insert(new_endpoint, node_id);
        Ok(())
    }

    /// Open a bidirectional session between two already registered public-key
    /// identities. Bandwidth buckets begin full at `now`.
    pub fn open_session(
        &mut self,
        session_id: u64,
        first: NodeId,
        second: NodeId,
        now: Instant,
    ) -> Result<(), BlindRelayError> {
        if first == second {
            return Err(BlindRelayError::InvalidNodePair);
        }
        if self.sessions.contains_key(&session_id) {
            return Err(BlindRelayError::DuplicateSession(session_id));
        }
        if !self.peers.contains_key(&first) {
            return Err(BlindRelayError::UnknownNode(first));
        }
        if !self.peers.contains_key(&second) {
            return Err(BlindRelayError::UnknownNode(second));
        }
        self.sessions.insert(
            session_id,
            BlindSessionState {
                first,
                second,
                frames: TokenBucket::new(
                    self.policy.frames_per_second,
                    self.policy.frames_per_second,
                    now,
                ),
                bytes: TokenBucket::new(
                    self.policy.bytes_per_second,
                    self.policy.bytes_per_second,
                    now,
                ),
            },
        );
        Ok(())
    }

    /// Close a session by id.
    pub fn close_session(&mut self, session_id: u64) -> bool {
        self.sessions.remove(&session_id).is_some()
    }

    /// Route an already decoded frame that arrived on `ingress_endpoint`.
    /// The endpoint must currently be bound to the frame's claimed source
    /// public key; otherwise the frame is rejected before any session budget
    /// is spent. The ingress size is the canonical encoded size.
    pub fn route_frame(
        &mut self,
        ingress_endpoint: RelayEndpoint,
        frame: &BlindRelayFrame,
        now: Instant,
    ) -> Result<BlindRelayDelivery, BlindRelayError> {
        let input_bytes = frame.encoded_len();
        self.route_frame_inner(ingress_endpoint, frame, input_bytes, now)
    }

    /// Decode and route one exact wire frame that arrived on
    /// `ingress_endpoint`, applying deployment size caps before checksum or
    /// payload copying and preserving ingress length for the
    /// anti-amplification check.
    pub fn route_wire_frame(
        &mut self,
        ingress_endpoint: RelayEndpoint,
        bytes: &[u8],
        now: Instant,
    ) -> Result<BlindRelayDelivery, BlindRelayError> {
        if let Err(error) = self.policy.validate_wire_sizes(bytes) {
            if is_size_reject(&error) {
                self.metrics.rate_or_size_rejects += 1;
            }
            return Err(error);
        }
        let header = parse_blind_relay_header(bytes)?;
        self.precheck_wire_admission(ingress_endpoint, &header, bytes.len(), now)?;
        let frame = decode_blind_relay_frame(bytes)?;
        self.route_frame_inner(ingress_endpoint, &frame, bytes.len(), now)
    }

    /// Check endpoint identity, session shape, epoch and available budget
    /// before the CRC-sealed payload is copied. This probes cloned buckets;
    /// the authoritative debit still happens only after the complete frame
    /// validates in `route_frame_inner`.
    fn precheck_wire_admission(
        &mut self,
        ingress_endpoint: RelayEndpoint,
        header: &BlindRelayHeader,
        frame_bytes: usize,
        now: Instant,
    ) -> Result<(), BlindRelayError> {
        let authenticated_source = match self.endpoints.get(&ingress_endpoint).copied() {
            Some(node) => node,
            None => {
                self.metrics.identity_rejects += 1;
                return Err(BlindRelayError::UnknownEndpoint(ingress_endpoint));
            }
        };
        if authenticated_source != header.source {
            self.metrics.identity_rejects += 1;
            return Err(BlindRelayError::IngressIdentityMismatch {
                endpoint: ingress_endpoint,
                authenticated: authenticated_source,
                claimed: header.source,
            });
        }
        let Some(source_peer) = self.peers.get(&header.source).copied() else {
            self.metrics.identity_rejects += 1;
            return Err(BlindRelayError::UnknownNode(header.source));
        };
        if !self.peers.contains_key(&header.destination) {
            self.metrics.identity_rejects += 1;
            return Err(BlindRelayError::UnknownNode(header.destination));
        }
        if header.migration_epoch != source_peer.migration_epoch {
            self.metrics.stale_epoch_rejects += 1;
            return Err(BlindRelayError::StaleMigrationEpoch {
                current: source_peer.migration_epoch,
                presented: header.migration_epoch,
            });
        }
        let Some(session) = self.sessions.get(&header.session_id) else {
            self.metrics.identity_rejects += 1;
            return Err(BlindRelayError::UnknownSession(header.session_id));
        };
        if !session.matches(header.source, header.destination) {
            self.metrics.identity_rejects += 1;
            return Err(BlindRelayError::SessionNodeMismatch {
                session_id: header.session_id,
            });
        }
        let mut probe = session.clone();
        if !probe.allow(now, frame_bytes) {
            self.metrics.rate_or_size_rejects += 1;
            return Err(BlindRelayError::RateLimited);
        }
        Ok(())
    }

    fn route_frame_inner(
        &mut self,
        ingress_endpoint: RelayEndpoint,
        frame: &BlindRelayFrame,
        input_bytes: usize,
        now: Instant,
    ) -> Result<BlindRelayDelivery, BlindRelayError> {
        let authenticated_source = match self.endpoints.get(&ingress_endpoint).copied() {
            Some(node) => node,
            None => {
                self.metrics.identity_rejects += 1;
                return Err(BlindRelayError::UnknownEndpoint(ingress_endpoint));
            }
        };
        if authenticated_source != frame.source {
            self.metrics.identity_rejects += 1;
            return Err(BlindRelayError::IngressIdentityMismatch {
                endpoint: ingress_endpoint,
                authenticated: authenticated_source,
                claimed: frame.source,
            });
        }
        let frame_bytes = match self.policy.validate_frame(frame) {
            Ok(len) => len,
            Err(error) => {
                self.metrics.rate_or_size_rejects += 1;
                return Err(error);
            }
        };
        if input_bytes < frame_bytes {
            self.metrics.rate_or_size_rejects += 1;
            return Err(BlindRelayError::Truncated);
        }

        let Some(source_peer) = self.peers.get(&frame.source).copied() else {
            self.metrics.identity_rejects += 1;
            return Err(BlindRelayError::UnknownNode(frame.source));
        };
        let Some(destination_peer) = self.peers.get(&frame.destination).copied() else {
            self.metrics.identity_rejects += 1;
            return Err(BlindRelayError::UnknownNode(frame.destination));
        };
        if frame.migration_epoch != source_peer.migration_epoch {
            self.metrics.stale_epoch_rejects += 1;
            return Err(BlindRelayError::StaleMigrationEpoch {
                current: source_peer.migration_epoch,
                presented: frame.migration_epoch,
            });
        }
        let Some(session) = self.sessions.get_mut(&frame.session_id) else {
            self.metrics.identity_rejects += 1;
            return Err(BlindRelayError::UnknownSession(frame.session_id));
        };
        if !session.matches(frame.source, frame.destination) {
            self.metrics.identity_rejects += 1;
            return Err(BlindRelayError::SessionNodeMismatch {
                session_id: frame.session_id,
            });
        }
        if !session.allow(now, frame_bytes) {
            self.metrics.rate_or_size_rejects += 1;
            return Err(BlindRelayError::RateLimited);
        }

        let wire_bytes = encode_blind_relay_frame(frame)?;
        if !BlindRelayPolicy::amplification_permitted(input_bytes, wire_bytes.len()) {
            return Err(BlindRelayError::AmplificationViolation {
                input_bytes,
                output_bytes: wire_bytes.len(),
            });
        }
        self.metrics.frames_forwarded += 1;
        self.metrics.bytes_forwarded += wire_bytes.len() as u64;
        Ok(BlindRelayDelivery {
            destination: frame.destination,
            endpoint: destination_peer.endpoint,
            frame: frame.clone(),
            wire_bytes,
        })
    }
}
