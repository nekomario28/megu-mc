// SPDX-License-Identifier: LGPL-3.0-or-later
#![forbid(unsafe_code)]
//! TigerStyle deterministic simulation testing oracle with compressed restart
//! snapshots and Merkle-tree identity verification.
//!
//! The oracle is fully deterministic: one `u64` seed produces the same operation
//! stream, world mutations, item transfers, dimension transitions, mod faults,
//! crashes, and restart checks on every supported platform. Restart archives use
//! the canonical LZ4 block codec; chunk identity is authenticated with the
//! canonical domain-separated SHA-256 Merkle tree, so a one-block reload
//! corruption reports the exact divergent chunk leaf keys.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::coords::{BlockPos, ChunkKey, CoordMap, DimIndex, SectionKey};
use crate::crypto::sha256;
use crate::hash::fnv1a64;
use crate::lz4::{compress_block, decompress_block, Lz4Error};
use crate::merkle_cas::{Digest32, MerkleTree};
use crate::section_rev::SharedSectionRevTable;

/// Number of canonical inventory slots exercised by transfer operations.
pub const INVENTORY_SLOTS: usize = 32;
/// Exact total item count that must be conserved forever.
pub const INITIAL_TOTAL_ITEMS: u64 = INVENTORY_SLOTS as u64 * 37;
/// Highest dimension index registered in the deterministic campaign
/// (`0..=2` vanilla plus `3`, a custom modded dimension).
pub const MAX_REGISTERED_DIM: u32 = 3;

const SNAPSHOT_MAGIC: &[u8; 8] = b"MEGUDST1";
const MAX_SNAPSHOT_CHUNKS: usize = 4096;
const MAX_SNAPSHOT_BLOCKS: usize = 1_048_576;
const MAX_SNAPSHOT_PLAYERS: usize = 4096;
const MAX_SNAPSHOT_REVISIONS: usize = 65_536;
const MAX_SNAPSHOT_MODS: usize = 4096;

/// Deterministic SplitMix64 random source used by the simulation campaign.
#[derive(Clone, Debug)]
pub struct DstRng {
    state: u64,
}

impl DstRng {
    /// Seed the deterministic stream. Every bit of `seed` participates in the first draw.
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// Next SplitMix64 output.
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform deterministic draw in `0..bound`; zero returns zero.
    pub fn below(&mut self, bound: u64) -> u64 {
        if bound == 0 {
            0
        } else {
            self.next_u64() % bound
        }
    }

    /// Current generator state, exposed for reproducibility diagnostics.
    #[must_use]
    pub const fn state(&self) -> u64 {
        self.state
    }
}

/// One deterministic simulation operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DstOp {
    /// Mutate one block in any vanilla or registered modded dimension.
    MutateBlock {
        /// Dimension containing the block.
        dim: DimIndex,
        /// World block position.
        pos: BlockPos,
        /// New canonical block-state id; zero represents air.
        block_id: u32,
    },
    /// Move items between canonical inventory slots. Total count must be conserved.
    TransferItem {
        /// Source slot.
        from_slot: u16,
        /// Destination slot.
        to_slot: u16,
        /// Requested item count; the simulation clamps to source availability.
        count: u32,
    },
    /// Connect or reconnect a player.
    PlayerConnect {
        /// Player identity.
        player_id: u32,
    },
    /// Disconnect a player; dimension membership must be removed immediately.
    PlayerDisconnect {
        /// Player identity.
        player_id: u32,
    },
    /// Move a connected player through a portal to `target_dim`.
    PortalTransit {
        /// Player identity.
        player_id: u32,
        /// Destination dimension.
        target_dim: DimIndex,
    },
    /// Inject a quarantined mod fault and verify the world is untouched.
    InjectModFault {
        /// Faulting mod identity.
        mod_id: u16,
    },
    /// Serialize, compress, tear down, reload, and require exact Merkle identity.
    CrashAndRestart,
}

/// Typed invariant failure reported by the deterministic oracle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DstInvariantViolation {
    /// Item mass changed: an item was duplicated or lost.
    ItemConservation {
        /// Tick at which the violation was observed.
        tick: u64,
        /// Required initial total.
        expected: u64,
        /// Observed total.
        actual: u64,
    },
    /// A player belongs to an unregistered dimension.
    InvalidDimension {
        /// Tick at which the violation was observed.
        tick: u64,
        /// Player identity.
        player_id: u32,
        /// Invalid dimension.
        dim: DimIndex,
    },
    /// Dimension membership and connected player state disagree.
    OrphanPlayer {
        /// Tick at which the violation was observed.
        tick: u64,
        /// Player identity.
        player_id: u32,
    },
    /// A section revision moved backwards.
    RevisionRegression {
        /// Tick at which the violation was observed.
        tick: u64,
        /// Regressed section.
        section: SectionKey,
        /// Previous high-water revision.
        before: u64,
        /// Current lower revision.
        after: u64,
    },
    /// A crash/restart did not reproduce the exact chunk Merkle root.
    RestartMerkleMismatch {
        /// Tick at which the restart ran.
        tick: u64,
        /// Root before crash.
        pre_root: Digest32,
        /// Root after reload.
        post_root: Digest32,
        /// Exact chunk leaf keys whose presence or digest diverged.
        divergent_keys: Vec<u64>,
    },
    /// Canonical snapshot bytes did not round-trip bit-exactly.
    RestartStateMismatch {
        /// Tick at which the restart ran.
        tick: u64,
    },
    /// A compressed restart archive failed the bounded LZ4 decode.
    RestartLz4 {
        /// Tick at which the restart ran.
        tick: u64,
        /// Canonical codec error.
        error: Lz4Error,
    },
    /// Snapshot bytes are malformed.
    SnapshotFormat {
        /// Tick associated with the parse.
        tick: u64,
        /// Static parse rejection reason.
        reason: &'static str,
    },
    /// A quarantined mod fault changed physical world state.
    ModFaultEscaped {
        /// Tick at which the fault ran.
        tick: u64,
        /// Faulting mod identity.
        mod_id: u16,
    },
    /// An operation referenced a slot or entity outside the oracle's contract.
    InvalidOperation {
        /// Tick at which the operation was applied.
        tick: u64,
        /// Static operation description.
        operation: &'static str,
    },
}

impl fmt::Display for DstInvariantViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ItemConservation {
                tick,
                expected,
                actual,
            } => write!(
                f,
                "tick {tick}: item conservation violated: expected {expected}, observed {actual}"
            ),
            Self::InvalidDimension {
                tick,
                player_id,
                dim,
            } => write!(
                f,
                "tick {tick}: player {player_id} entered unregistered dimension {}",
                dim.0
            ),
            Self::OrphanPlayer { tick, player_id } => {
                write!(
                    f,
                    "tick {tick}: player {player_id} has orphan dimension membership"
                )
            }
            Self::RevisionRegression {
                tick,
                section,
                before,
                after,
            } => write!(
                f,
                "tick {tick}: section {section:?} revision regressed {before} -> {after}"
            ),
            Self::RestartMerkleMismatch {
                tick,
                divergent_keys,
                ..
            } => write!(
                f,
                "tick {tick}: restart Merkle root mismatch at chunk keys {divergent_keys:?}"
            ),
            Self::RestartStateMismatch { tick } => {
                write!(f, "tick {tick}: restart snapshot was not bit-exact")
            }
            Self::RestartLz4 { tick, error } => {
                write!(f, "tick {tick}: restart archive LZ4 error: {error}")
            }
            Self::SnapshotFormat { tick, reason } => {
                write!(f, "tick {tick}: malformed DST snapshot: {reason}")
            }
            Self::ModFaultEscaped { tick, mod_id } => {
                write!(f, "tick {tick}: fault from mod {mod_id} escaped quarantine")
            }
            Self::InvalidOperation { tick, operation } => {
                write!(
                    f,
                    "tick {tick}: invalid deterministic operation: {operation}"
                )
            }
        }
    }
}

impl std::error::Error for DstInvariantViolation {}

/// Counters produced by one deterministic campaign.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DstCampaignReport {
    /// Campaign seed.
    pub seed: u64,
    /// Ticks successfully executed.
    pub ticks_executed: usize,
    /// Crash/restart cycles verified.
    pub restarts_verified: usize,
    /// Restart Merkle roots that matched exactly.
    pub merkle_roots_matched: usize,
    /// Per-tick conservation checks performed.
    pub conservation_checks: usize,
    /// Portal operations that changed a connected player's dimension.
    pub cross_dim_ops: usize,
    /// Mod faults confined by quarantine.
    pub mod_faults_isolated: usize,
}

type LocalBlockKey = u64; // order-preserving y, then local z, then local x

#[inline]
fn pack_local_block_key(y: i32, lz: u8, lx: u8) -> LocalBlockKey {
    let ordered_y = u64::from((y as u32) ^ 0x8000_0000);
    (ordered_y << 8) | (u64::from(lz) << 4) | u64::from(lx)
}

#[inline]
fn local_block_key(pos: BlockPos) -> LocalBlockKey {
    pack_local_block_key(pos.y, (pos.z & 15) as u8, (pos.x & 15) as u8)
}

#[inline]
fn split_local_block_key(key: LocalBlockKey) -> (i32, u8, u8) {
    let ordered_y = (key >> 8) as u32;
    let y = (ordered_y ^ 0x8000_0000) as i32;
    (y, ((key >> 4) & 15) as u8, (key & 15) as u8)
}

#[derive(Clone, Copy, Debug, Default)]
struct RevisionTracker {
    current: u64,
    highwater: u64,
}

#[derive(Clone, Copy, Debug)]
struct PlayerState {
    dim: DimIndex,
    connected: bool,
}

struct WorldState {
    chunks: BTreeMap<ChunkKey, BTreeMap<LocalBlockKey, u32>>,
    chunk_digests: BTreeMap<ChunkKey, Digest32>,
    dirty_chunks: BTreeSet<ChunkKey>,
    players: BTreeMap<u32, PlayerState>,
    dimension_members: BTreeMap<DimIndex, BTreeSet<u32>>,
    inventory: [u32; INVENTORY_SLOTS],
    logical_revisions: CoordMap<SectionKey, RevisionTracker>,
    canonical_revision_offsets: CoordMap<SectionKey, u64>,
    revision_table: SharedSectionRevTable,
    quarantined_mods: BTreeSet<u16>,
    tick: u64,
}

impl WorldState {
    fn new() -> Self {
        Self {
            chunks: BTreeMap::new(),
            chunk_digests: BTreeMap::new(),
            dirty_chunks: BTreeSet::new(),
            players: BTreeMap::new(),
            dimension_members: BTreeMap::new(),
            inventory: [37; INVENTORY_SLOTS],
            logical_revisions: CoordMap::default(),
            canonical_revision_offsets: CoordMap::default(),
            revision_table: SharedSectionRevTable::new(),
            quarantined_mods: BTreeSet::new(),
            tick: 0,
        }
    }

    fn mutate_block(&mut self, dim: DimIndex, pos: BlockPos, block_id: u32) {
        let chunk = pos.chunk_key(dim);
        let local = local_block_key(pos);
        let remove_chunk = {
            let blocks = self.chunks.entry(chunk).or_default();
            if block_id == 0 {
                blocks.remove(&local);
            } else {
                blocks.insert(local, block_id);
            }
            blocks.is_empty()
        };
        if remove_chunk {
            self.chunks.remove(&chunk);
            self.chunk_digests.remove(&chunk);
        }
        self.dirty_chunks.insert(chunk);
        let (canonical_primary_revision, _) =
            self.revision_table.bump_block(dim, pos.x, pos.y, pos.z);
        self.bump_logical_revisions(dim, pos, canonical_primary_revision);
    }

    fn bump_logical_revisions(
        &mut self,
        dim: DimIndex,
        pos: BlockPos,
        canonical_primary_revision: u64,
    ) {
        let section = pos.section_key(dim);
        for &dx in border_offsets(pos.x) {
            for &dy in border_offsets(pos.y) {
                for &dz in border_offsets(pos.z) {
                    let (Some(cx), Some(sy), Some(cz)) = (
                        section.cx.checked_add(dx),
                        section.sy.checked_add(dy),
                        section.cz.checked_add(dz),
                    ) else {
                        continue;
                    };
                    let key = SectionKey { dim, cx, sy, cz };
                    let revision = self.logical_revisions.entry(key).or_default();
                    if dx == 0 && dy == 0 && dz == 0 {
                        let offset = self
                            .canonical_revision_offsets
                            .get(&key)
                            .copied()
                            .unwrap_or(0);
                        revision.current = offset.saturating_add(canonical_primary_revision);
                    } else {
                        revision.current = revision.current.saturating_add(1);
                    }
                }
            }
        }
    }

    fn transfer_items(
        &mut self,
        from_slot: u16,
        to_slot: u16,
        count: u32,
    ) -> Result<(), DstInvariantViolation> {
        let from = usize::from(from_slot);
        let to = usize::from(to_slot);
        if from >= INVENTORY_SLOTS || to >= INVENTORY_SLOTS {
            return Err(DstInvariantViolation::InvalidOperation {
                tick: self.tick,
                operation: "transfer slot outside canonical inventory",
            });
        }
        if from == to {
            return Ok(());
        }
        let moved = count.min(self.inventory[from]);
        self.inventory[from] -= moved;
        self.inventory[to] = self.inventory[to].checked_add(moved).ok_or(
            DstInvariantViolation::InvalidOperation {
                tick: self.tick,
                operation: "transfer count overflow",
            },
        )?;
        Ok(())
    }

    fn player_connect(&mut self, player_id: u32) {
        let entry = self.players.entry(player_id).or_insert(PlayerState {
            dim: DimIndex::OVERWORLD,
            connected: false,
        });
        entry.connected = true;
        self.dimension_members
            .entry(entry.dim)
            .or_default()
            .insert(player_id);
    }

    fn player_disconnect(&mut self, player_id: u32) {
        if let Some(player) = self.players.get_mut(&player_id) {
            player.connected = false;
            if let Some(members) = self.dimension_members.get_mut(&player.dim) {
                members.remove(&player_id);
                if members.is_empty() {
                    self.dimension_members.remove(&player.dim);
                }
            }
        }
    }

    fn portal_transit(&mut self, player_id: u32, target_dim: DimIndex) {
        let Some(player) = self.players.get_mut(&player_id) else {
            return;
        };
        if !player.connected || player.dim == target_dim {
            return;
        }
        if let Some(members) = self.dimension_members.get_mut(&player.dim) {
            members.remove(&player_id);
            if members.is_empty() {
                self.dimension_members.remove(&player.dim);
            }
        }
        player.dim = target_dim;
        self.dimension_members
            .entry(target_dim)
            .or_default()
            .insert(player_id);
    }

    fn inject_mod_fault(&mut self, mod_id: u16) -> bool {
        let before = self.physical_fingerprint();
        self.quarantined_mods.insert(mod_id);
        let target = BlockPos::new(
            i32::from(mod_id % 8),
            i32::from(mod_id % 18),
            i32::from((mod_id / 2) % 8),
        );
        let write_escaped = self.mod_write_block(mod_id, DimIndex::OVERWORLD, target, 999);
        let item_escaped = self.mod_write_item(mod_id, usize::from(mod_id) % INVENTORY_SLOTS, 1);
        write_escaped || item_escaped || before != self.physical_fingerprint()
    }

    fn mod_write_block(
        &mut self,
        mod_id: u16,
        dim: DimIndex,
        pos: BlockPos,
        block_id: u32,
    ) -> bool {
        if self.quarantined_mods.contains(&mod_id) {
            return false;
        }
        self.mutate_block(dim, pos, block_id);
        true
    }

    fn mod_write_item(&mut self, mod_id: u16, slot: usize, count: u32) -> bool {
        if self.quarantined_mods.contains(&mod_id) || slot >= INVENTORY_SLOTS {
            return false;
        }
        self.inventory[slot] = self.inventory[slot].saturating_add(count);
        true
    }

    fn physical_fingerprint(&mut self) -> u64 {
        let root = self.merkle_tree().root();
        let mut bytes = Vec::with_capacity(32 + INVENTORY_SLOTS * 4 + self.players.len() * 9);
        bytes.extend_from_slice(&root);
        for count in self.inventory {
            bytes.extend_from_slice(&count.to_le_bytes());
        }
        for (&player_id, player) in &self.players {
            bytes.extend_from_slice(&player_id.to_le_bytes());
            bytes.extend_from_slice(&player.dim.0.to_le_bytes());
            bytes.push(u8::from(player.connected));
        }
        fnv1a64(&bytes)
    }

    fn materialize_chunk_digests(&mut self) {
        let dirty: Vec<ChunkKey> = self.dirty_chunks.iter().copied().collect();
        for key in dirty {
            if let Some(blocks) = self.chunks.get(&key) {
                self.chunk_digests.insert(key, digest_chunk_blocks(blocks));
            } else {
                self.chunk_digests.remove(&key);
            }
        }
        self.dirty_chunks.clear();
    }

    fn merkle_tree(&mut self) -> MerkleTree {
        self.materialize_chunk_digests();
        let leaves: Vec<(u64, Digest32)> = self
            .chunk_digests
            .iter()
            .map(|(&key, &digest)| (chunk_leaf_key(key), digest))
            .collect();
        MerkleTree::new(&leaves)
    }

    fn validate_invariants(&mut self) -> Result<(), DstInvariantViolation> {
        let actual_items = self
            .inventory
            .iter()
            .fold(0u64, |sum, &count| sum + u64::from(count));
        if actual_items != INITIAL_TOTAL_ITEMS {
            return Err(DstInvariantViolation::ItemConservation {
                tick: self.tick,
                expected: INITIAL_TOTAL_ITEMS,
                actual: actual_items,
            });
        }

        for (&dim, members) in &self.dimension_members {
            for &player_id in members {
                let Some(player) = self.players.get(&player_id) else {
                    return Err(DstInvariantViolation::OrphanPlayer {
                        tick: self.tick,
                        player_id,
                    });
                };
                if !player.connected || player.dim != dim {
                    return Err(DstInvariantViolation::OrphanPlayer {
                        tick: self.tick,
                        player_id,
                    });
                }
                if dim.0 > MAX_REGISTERED_DIM {
                    return Err(DstInvariantViolation::InvalidDimension {
                        tick: self.tick,
                        player_id,
                        dim,
                    });
                }
            }
        }
        for (&player_id, player) in &self.players {
            if player.dim.0 > MAX_REGISTERED_DIM {
                return Err(DstInvariantViolation::InvalidDimension {
                    tick: self.tick,
                    player_id,
                    dim: player.dim,
                });
            }
            if player.connected {
                let registered = self
                    .dimension_members
                    .get(&player.dim)
                    .is_some_and(|members| members.contains(&player_id));
                if !registered {
                    return Err(DstInvariantViolation::OrphanPlayer {
                        tick: self.tick,
                        player_id,
                    });
                }
            }
        }

        for (&section, revision) in &mut self.logical_revisions {
            let before = revision.highwater;
            if revision.current < before {
                return Err(DstInvariantViolation::RevisionRegression {
                    tick: self.tick,
                    section,
                    before,
                    after: revision.current,
                });
            }
            revision.highwater = before.max(revision.current);
        }
        Ok(())
    }

    fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(SNAPSHOT_MAGIC);
        push_u64(&mut out, self.tick);
        push_u32(&mut out, INVENTORY_SLOTS as u32);
        for count in self.inventory {
            push_u32(&mut out, count);
        }
        push_u32(&mut out, self.players.len() as u32);
        for (&player_id, player) in &self.players {
            push_u32(&mut out, player_id);
            push_u32(&mut out, player.dim.0);
            out.push(u8::from(player.connected));
        }
        push_u32(&mut out, self.chunks.len() as u32);
        for (chunk, blocks) in &self.chunks {
            push_u32(&mut out, chunk.dim.0);
            push_i32(&mut out, chunk.cx);
            push_i32(&mut out, chunk.cz);
            push_u32(&mut out, blocks.len() as u32);
            for (&packed_local, &block_id) in blocks {
                let (y, lz, lx) = split_local_block_key(packed_local);
                push_i32(&mut out, y);
                out.push(lz);
                out.push(lx);
                push_u32(&mut out, block_id);
            }
        }
        push_u32(&mut out, self.logical_revisions.len() as u32);
        let mut revisions: Vec<(SectionKey, RevisionTracker)> = self
            .logical_revisions
            .iter()
            .map(|(&section, &revision)| (section, revision))
            .collect();
        revisions.sort_unstable_by_key(|(section, _)| *section);
        for (section, revision) in revisions {
            push_u32(&mut out, section.dim.0);
            push_i32(&mut out, section.cx);
            push_i32(&mut out, section.sy);
            push_i32(&mut out, section.cz);
            push_u64(&mut out, revision.current);
            push_u64(&mut out, revision.highwater);
        }
        push_u32(&mut out, self.quarantined_mods.len() as u32);
        for &mod_id in &self.quarantined_mods {
            push_u32(&mut out, u32::from(mod_id));
        }
        out
    }

    fn parse(bytes: &[u8], tick_for_error: u64) -> Result<Self, DstInvariantViolation> {
        let mut cursor = SnapshotCursor::new(bytes);
        if cursor.take(SNAPSHOT_MAGIC.len()).map_err(|reason| {
            DstInvariantViolation::SnapshotFormat {
                tick: tick_for_error,
                reason,
            }
        })? != SNAPSHOT_MAGIC
        {
            return Err(DstInvariantViolation::SnapshotFormat {
                tick: tick_for_error,
                reason: "bad magic",
            });
        }
        let tick = cursor
            .u64()
            .map_err(|reason| DstInvariantViolation::SnapshotFormat {
                tick: tick_for_error,
                reason,
            })?;
        let inventory_len =
            cursor
                .u32()
                .map_err(|reason| DstInvariantViolation::SnapshotFormat {
                    tick: tick_for_error,
                    reason,
                })? as usize;
        if inventory_len != INVENTORY_SLOTS {
            return Err(DstInvariantViolation::SnapshotFormat {
                tick: tick_for_error,
                reason: "inventory length mismatch",
            });
        }
        let mut inventory = [0u32; INVENTORY_SLOTS];
        for count in &mut inventory {
            *count = cursor
                .u32()
                .map_err(|reason| DstInvariantViolation::SnapshotFormat {
                    tick: tick_for_error,
                    reason,
                })?;
        }

        let player_count = cursor
            .u32()
            .map_err(|reason| DstInvariantViolation::SnapshotFormat {
                tick: tick_for_error,
                reason,
            })? as usize;
        if player_count > MAX_SNAPSHOT_PLAYERS {
            return Err(DstInvariantViolation::SnapshotFormat {
                tick: tick_for_error,
                reason: "player count too large",
            });
        }
        let mut players = BTreeMap::new();
        let mut dimension_members: BTreeMap<DimIndex, BTreeSet<u32>> = BTreeMap::new();
        for _ in 0..player_count {
            let player_id =
                cursor
                    .u32()
                    .map_err(|reason| DstInvariantViolation::SnapshotFormat {
                        tick: tick_for_error,
                        reason,
                    })?;
            let dim =
                DimIndex(
                    cursor
                        .u32()
                        .map_err(|reason| DstInvariantViolation::SnapshotFormat {
                            tick: tick_for_error,
                            reason,
                        })?,
                );
            let connected =
                match cursor
                    .u8()
                    .map_err(|reason| DstInvariantViolation::SnapshotFormat {
                        tick: tick_for_error,
                        reason,
                    })? {
                    0 => false,
                    1 => true,
                    _ => {
                        return Err(DstInvariantViolation::SnapshotFormat {
                            tick: tick_for_error,
                            reason: "invalid connected flag",
                        });
                    }
                };
            if players
                .insert(player_id, PlayerState { dim, connected })
                .is_some()
            {
                return Err(DstInvariantViolation::SnapshotFormat {
                    tick: tick_for_error,
                    reason: "duplicate player",
                });
            }
            if connected {
                dimension_members.entry(dim).or_default().insert(player_id);
            }
        }

        let chunk_count = cursor
            .u32()
            .map_err(|reason| DstInvariantViolation::SnapshotFormat {
                tick: tick_for_error,
                reason,
            })? as usize;
        if chunk_count > MAX_SNAPSHOT_CHUNKS {
            return Err(DstInvariantViolation::SnapshotFormat {
                tick: tick_for_error,
                reason: "chunk count too large",
            });
        }
        let mut chunks = BTreeMap::new();
        let mut total_blocks = 0usize;
        for _ in 0..chunk_count {
            let chunk = ChunkKey {
                dim: DimIndex(cursor.u32().map_err(|reason| {
                    DstInvariantViolation::SnapshotFormat {
                        tick: tick_for_error,
                        reason,
                    }
                })?),
                cx: cursor
                    .i32()
                    .map_err(|reason| DstInvariantViolation::SnapshotFormat {
                        tick: tick_for_error,
                        reason,
                    })?,
                cz: cursor
                    .i32()
                    .map_err(|reason| DstInvariantViolation::SnapshotFormat {
                        tick: tick_for_error,
                        reason,
                    })?,
            };
            let block_count =
                cursor
                    .u32()
                    .map_err(|reason| DstInvariantViolation::SnapshotFormat {
                        tick: tick_for_error,
                        reason,
                    })? as usize;
            total_blocks = total_blocks.saturating_add(block_count);
            if total_blocks > MAX_SNAPSHOT_BLOCKS {
                return Err(DstInvariantViolation::SnapshotFormat {
                    tick: tick_for_error,
                    reason: "block count too large",
                });
            }
            let mut blocks = BTreeMap::new();
            for _ in 0..block_count {
                let y = cursor
                    .i32()
                    .map_err(|reason| DstInvariantViolation::SnapshotFormat {
                        tick: tick_for_error,
                        reason,
                    })?;
                let lz = cursor
                    .u8()
                    .map_err(|reason| DstInvariantViolation::SnapshotFormat {
                        tick: tick_for_error,
                        reason,
                    })?;
                let lx = cursor
                    .u8()
                    .map_err(|reason| DstInvariantViolation::SnapshotFormat {
                        tick: tick_for_error,
                        reason,
                    })?;
                if lx >= 16 || lz >= 16 {
                    return Err(DstInvariantViolation::SnapshotFormat {
                        tick: tick_for_error,
                        reason: "local coordinate out of range",
                    });
                }
                let block_id =
                    cursor
                        .u32()
                        .map_err(|reason| DstInvariantViolation::SnapshotFormat {
                            tick: tick_for_error,
                            reason,
                        })?;
                if block_id == 0 {
                    return Err(DstInvariantViolation::SnapshotFormat {
                        tick: tick_for_error,
                        reason: "zero block id leaf",
                    });
                }
                if blocks
                    .insert(pack_local_block_key(y, lz, lx), block_id)
                    .is_some()
                {
                    return Err(DstInvariantViolation::SnapshotFormat {
                        tick: tick_for_error,
                        reason: "duplicate block coordinate",
                    });
                }
            }
            if blocks.is_empty() {
                return Err(DstInvariantViolation::SnapshotFormat {
                    tick: tick_for_error,
                    reason: "empty chunk leaf",
                });
            }
            if chunks.insert(chunk, blocks).is_some() {
                return Err(DstInvariantViolation::SnapshotFormat {
                    tick: tick_for_error,
                    reason: "duplicate chunk",
                });
            }
        }

        let revision_count =
            cursor
                .u32()
                .map_err(|reason| DstInvariantViolation::SnapshotFormat {
                    tick: tick_for_error,
                    reason,
                })? as usize;
        if revision_count > MAX_SNAPSHOT_REVISIONS {
            return Err(DstInvariantViolation::SnapshotFormat {
                tick: tick_for_error,
                reason: "revision count too large",
            });
        }
        let mut logical_revisions = CoordMap::default();
        for _ in 0..revision_count {
            let section = SectionKey {
                dim: DimIndex(cursor.u32().map_err(|reason| {
                    DstInvariantViolation::SnapshotFormat {
                        tick: tick_for_error,
                        reason,
                    }
                })?),
                cx: cursor
                    .i32()
                    .map_err(|reason| DstInvariantViolation::SnapshotFormat {
                        tick: tick_for_error,
                        reason,
                    })?,
                sy: cursor
                    .i32()
                    .map_err(|reason| DstInvariantViolation::SnapshotFormat {
                        tick: tick_for_error,
                        reason,
                    })?,
                cz: cursor
                    .i32()
                    .map_err(|reason| DstInvariantViolation::SnapshotFormat {
                        tick: tick_for_error,
                        reason,
                    })?,
            };
            let current = cursor
                .u64()
                .map_err(|reason| DstInvariantViolation::SnapshotFormat {
                    tick: tick_for_error,
                    reason,
                })?;
            let highwater =
                cursor
                    .u64()
                    .map_err(|reason| DstInvariantViolation::SnapshotFormat {
                        tick: tick_for_error,
                        reason,
                    })?;
            if current < highwater {
                return Err(DstInvariantViolation::SnapshotFormat {
                    tick: tick_for_error,
                    reason: "revision below highwater",
                });
            }
            logical_revisions.insert(section, RevisionTracker { current, highwater });
        }

        let mod_count = cursor
            .u32()
            .map_err(|reason| DstInvariantViolation::SnapshotFormat {
                tick: tick_for_error,
                reason,
            })? as usize;
        if mod_count > MAX_SNAPSHOT_MODS {
            return Err(DstInvariantViolation::SnapshotFormat {
                tick: tick_for_error,
                reason: "mod count too large",
            });
        }
        let mut quarantined_mods = BTreeSet::new();
        for _ in 0..mod_count {
            let mod_id = cursor
                .u32()
                .map_err(|reason| DstInvariantViolation::SnapshotFormat {
                    tick: tick_for_error,
                    reason,
                })?;
            if mod_id > u32::from(u16::MAX) {
                return Err(DstInvariantViolation::SnapshotFormat {
                    tick: tick_for_error,
                    reason: "mod id out of range",
                });
            }
            quarantined_mods.insert(mod_id as u16);
        }
        if !cursor.is_finished() {
            return Err(DstInvariantViolation::SnapshotFormat {
                tick: tick_for_error,
                reason: "trailing bytes",
            });
        }

        let dirty_chunks = chunks.keys().copied().collect();
        let canonical_revision_offsets = logical_revisions
            .iter()
            .map(|(&section, revision)| (section, revision.current))
            .collect();
        Ok(Self {
            chunks,
            chunk_digests: BTreeMap::new(),
            dirty_chunks,
            players,
            dimension_members,
            inventory,
            logical_revisions,
            canonical_revision_offsets,
            revision_table: SharedSectionRevTable::new(),
            quarantined_mods,
            tick,
        })
    }

    fn crash_and_restart(&mut self) -> Result<(), DstInvariantViolation> {
        let tick = self.tick;
        let pre_root = self.merkle_tree().root();
        let raw = self.serialize();
        let compressed = compress_block(&raw);
        let decoded = decompress_block(&compressed, raw.len())
            .map_err(|error| DstInvariantViolation::RestartLz4 { tick, error })?;
        let mut reloaded = Self::parse(&decoded, tick)?;
        let roundtrip = reloaded.serialize();
        if roundtrip != raw {
            return Err(DstInvariantViolation::RestartStateMismatch { tick });
        }
        let post_root = reloaded.merkle_tree().root();
        if post_root != pre_root {
            let mut pre_world = Self::parse(&raw, tick)?;
            let pre_tree = pre_world.merkle_tree();
            let post_tree = reloaded.merkle_tree();
            let mut divergent_keys = Vec::new();
            pre_tree.diff_keys(&post_tree, &mut divergent_keys);
            return Err(DstInvariantViolation::RestartMerkleMismatch {
                tick,
                pre_root,
                post_root,
                divergent_keys,
            });
        }
        *self = reloaded;
        Ok(())
    }
}

#[inline]
fn border_offsets(coord: i32) -> &'static [i32] {
    match coord & 15 {
        0 => &[-1, 0],
        15 => &[0, 1],
        _ => &[0],
    }
}

#[inline]
fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

#[inline]
fn push_i32(out: &mut Vec<u8>, value: i32) {
    out.extend_from_slice(&value.to_le_bytes());
}

#[inline]
fn push_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

struct SnapshotCursor<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> SnapshotCursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], &'static str> {
        let end = self.position.checked_add(len).ok_or("length overflow")?;
        if end > self.bytes.len() {
            return Err("truncated");
        }
        let slice = &self.bytes[self.position..end];
        self.position = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, &'static str> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, &'static str> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn i32(&mut self) -> Result<i32, &'static str> {
        let bytes = self.take(4)?;
        Ok(i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn u64(&mut self) -> Result<u64, &'static str> {
        let bytes = self.take(8)?;
        Ok(u64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    fn is_finished(&self) -> bool {
        self.position == self.bytes.len()
    }
}

#[inline]
fn digest_chunk_blocks(blocks: &BTreeMap<LocalBlockKey, u32>) -> Digest32 {
    let mut bytes = Vec::with_capacity(blocks.len() * 10);
    for (&packed_local, &block_id) in blocks {
        let (y, lz, lx) = split_local_block_key(packed_local);
        bytes.extend_from_slice(&y.to_le_bytes());
        bytes.push(lz);
        bytes.push(lx);
        bytes.extend_from_slice(&block_id.to_le_bytes());
    }
    sha256(&bytes)
}

/// Stable `u64` Merkle leaf key for one canonical chunk key.
#[must_use]
pub fn chunk_leaf_key(key: ChunkKey) -> u64 {
    let mut preimage = [0u8; 12];
    preimage[..4].copy_from_slice(&key.dim.0.to_le_bytes());
    preimage[4..8].copy_from_slice(&key.cx.to_le_bytes());
    preimage[8..].copy_from_slice(&key.cz.to_le_bytes());
    fnv1a64(&preimage)
}

/// Deterministic world simulator used by [`run_dst_campaign`]. The controlled
/// fault-injection methods are public because negative tests are part of the
/// oracle contract: they prove invariant checks can fail for the right reason.
pub struct DstSimulation {
    world: WorldState,
}

impl Default for DstSimulation {
    fn default() -> Self {
        Self::new()
    }
}

impl DstSimulation {
    /// Fresh deterministic world with the exact initial item mass.
    #[must_use]
    pub fn new() -> Self {
        Self {
            world: WorldState::new(),
        }
    }

    /// Restore a simulation from canonical uncompressed snapshot bytes.
    pub fn from_snapshot(bytes: &[u8]) -> Result<Self, DstInvariantViolation> {
        Ok(Self {
            world: WorldState::parse(bytes, 0)?,
        })
    }

    /// Canonical uncompressed snapshot bytes.
    #[must_use]
    pub fn snapshot_bytes(&self) -> Vec<u8> {
        self.world.serialize()
    }

    /// Operations completed by this simulation.
    #[must_use]
    pub const fn tick(&self) -> u64 {
        self.world.tick
    }

    /// Apply one operation. The boolean is `true` when a crash/restart cycle ran.
    pub fn apply_op(&mut self, op: DstOp) -> Result<bool, DstInvariantViolation> {
        let restarted = match op {
            DstOp::MutateBlock { dim, pos, block_id } => {
                self.world.mutate_block(dim, pos, block_id);
                false
            }
            DstOp::TransferItem {
                from_slot,
                to_slot,
                count,
            } => {
                self.world.transfer_items(from_slot, to_slot, count)?;
                false
            }
            DstOp::PlayerConnect { player_id } => {
                self.world.player_connect(player_id);
                false
            }
            DstOp::PlayerDisconnect { player_id } => {
                self.world.player_disconnect(player_id);
                false
            }
            DstOp::PortalTransit {
                player_id,
                target_dim,
            } => {
                self.world.portal_transit(player_id, target_dim);
                false
            }
            DstOp::InjectModFault { mod_id } => {
                if self.world.inject_mod_fault(mod_id) {
                    return Err(DstInvariantViolation::ModFaultEscaped {
                        tick: self.world.tick,
                        mod_id,
                    });
                }
                false
            }
            DstOp::CrashAndRestart => {
                self.world.crash_and_restart()?;
                true
            }
        };
        self.world.tick = self.world.tick.saturating_add(1);
        Ok(restarted)
    }

    /// Validate every cross-cutting invariant against current state.
    pub fn validate_invariants(&mut self) -> Result<(), DstInvariantViolation> {
        self.world.validate_invariants()
    }

    /// Current chunk Merkle root.
    pub fn merkle_root(&mut self) -> Digest32 {
        self.world.merkle_tree().root()
    }

    /// Dimension of a currently connected player, or `None` when absent/offline.
    #[must_use]
    pub fn connected_player_dimension(&self, player_id: u32) -> Option<DimIndex> {
        self.world
            .players
            .get(&player_id)
            .filter(|player| player.connected)
            .map(|player| player.dim)
    }

    /// Deliberately duplicate items without a source. This is an oracle fault
    /// injection and must be caught by [`Self::validate_invariants`].
    pub fn inject_item_dupe(&mut self, slot: u16, count: u32) -> bool {
        let Some(slot) = self.world.inventory.get_mut(usize::from(slot)) else {
            return false;
        };
        *slot = slot.saturating_add(count);
        true
    }

    /// Deliberately corrupt one reloaded block. Returns the Merkle leaf key of
    /// the affected chunk, or `None` when the world has no block to corrupt.
    pub fn inject_restart_block_corruption(&mut self) -> Option<u64> {
        for (&chunk, blocks) in &mut self.world.chunks {
            if let Some((_, block_id)) = blocks.iter_mut().next() {
                *block_id = block_id.wrapping_add(1).max(1);
                self.world.dirty_chunks.insert(chunk);
                self.world.chunk_digests.remove(&chunk);
                return Some(chunk_leaf_key(chunk));
            }
        }
        None
    }
}

/// Verify that two simulation instances represent the same authenticated chunk
/// state. On mismatch the exact divergent Merkle leaf keys are returned.
pub fn verify_merkle_restart(
    pre_restart: &mut DstSimulation,
    reloaded: &mut DstSimulation,
    tick: u64,
) -> Result<(), DstInvariantViolation> {
    let pre_tree = pre_restart.world.merkle_tree();
    let post_tree = reloaded.world.merkle_tree();
    let pre_root = pre_tree.root();
    let post_root = post_tree.root();
    if pre_root == post_root {
        return Ok(());
    }
    let mut divergent_keys = Vec::new();
    pre_tree.diff_keys(&post_tree, &mut divergent_keys);
    Err(DstInvariantViolation::RestartMerkleMismatch {
        tick,
        pre_root,
        post_root,
        divergent_keys,
    })
}

fn campaign_op(rng: &mut DstRng) -> DstOp {
    match rng.below(1000) {
        0..=649 | 990..=999 => DstOp::MutateBlock {
            dim: DimIndex(rng.below(4) as u32),
            pos: BlockPos::new(
                rng.below(8) as i32,
                rng.below(18) as i32,
                rng.below(8) as i32,
            ),
            block_id: rng.below(8) as u32,
        },
        650..=829 => DstOp::TransferItem {
            from_slot: rng.below(INVENTORY_SLOTS as u64) as u16,
            to_slot: rng.below(INVENTORY_SLOTS as u64) as u16,
            count: rng.below(65) as u32,
        },
        830..=909 => DstOp::PlayerConnect {
            player_id: rng.below(8) as u32,
        },
        910..=959 => DstOp::PlayerDisconnect {
            player_id: rng.below(8) as u32,
        },
        960..=988 => DstOp::PortalTransit {
            player_id: rng.below(8) as u32,
            target_dim: DimIndex(rng.below(4) as u32),
        },
        _ => DstOp::InjectModFault {
            mod_id: rng.below(5) as u16,
        },
    }
}

/// Run a deterministic TigerStyle simulation campaign.
///
/// Every tick validates exact item conservation, dimension membership, and
/// revision monotonicity. Every five-hundredth tick performs a compressed
/// crash/restart cycle and requires bit-exact Merkle root equality.
pub fn run_dst_campaign(
    seed: u64,
    ticks: usize,
) -> Result<DstCampaignReport, DstInvariantViolation> {
    let mut rng = DstRng::new(seed);
    let mut simulation = DstSimulation::new();
    let mut report = DstCampaignReport {
        seed,
        ..DstCampaignReport::default()
    };

    for tick_index in 0..ticks {
        let op = if (tick_index + 1) % 500 == 0 {
            DstOp::CrashAndRestart
        } else {
            campaign_op(&mut rng)
        };
        let crosses_dimension = match op {
            DstOp::PortalTransit {
                player_id,
                target_dim,
            } => simulation
                .connected_player_dimension(player_id)
                .is_some_and(|dimension| dimension != target_dim),
            _ => false,
        };
        let restarted = simulation.apply_op(op)?;
        simulation.validate_invariants()?;
        report.conservation_checks += 1;
        if restarted {
            report.restarts_verified += 1;
            report.merkle_roots_matched += 1;
        }
        if crosses_dimension {
            report.cross_dim_ops += 1;
        }
        if matches!(op, DstOp::InjectModFault { .. }) {
            report.mod_faults_isolated += 1;
        }
    }
    report.ticks_executed = ticks;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::{pack_local_block_key, split_local_block_key};

    #[test]
    fn local_block_key_preserves_full_signed_y_range() {
        for y in [i32::MIN, -16_777_216, -1, 0, 1, 16_777_215, i32::MAX] {
            for (lz, lx) in [(0, 0), (15, 15)] {
                let packed = pack_local_block_key(y, lz, lx);
                assert_eq!(split_local_block_key(packed), (y, lz, lx));
            }
        }
        assert_ne!(
            pack_local_block_key(0, 0, 0),
            pack_local_block_key(16_777_216, 0, 0),
            "widely separated modded heights must not collide"
        );
    }
}
