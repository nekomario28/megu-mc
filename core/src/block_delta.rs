// SPDX-License-Identifier: LGPL-3.0-or-later
//! Reversible section edits, commit receipts, and conservation-checked transfers.

use std::cmp::Ordering;
use std::collections::{btree_map::Entry, BTreeMap};
use std::fmt;

use crate::bits::section_index;
use crate::crypto::Sha256;
use crate::merkle_cas::Digest32;

const SECTION_EDGE: u8 = 16;
const SECTION_VOLUME: usize = (SECTION_EDGE as usize).pow(3);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeltaCoreError {
    IndexOutOfRange {
        raw: u16,
    },
    CoordinatesOutOfRange {
        x: u8,
        y: u8,
        z: u8,
    },
    StateMismatch {
        idx: SectionBlockIndex,
        expected: u32,
        actual: u32,
    },
}

impl fmt::Display for DeltaCoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IndexOutOfRange { raw } => write!(f, "section index {raw} out of range"),
            Self::CoordinatesOutOfRange { x, y, z } => {
                write!(f, "section coordinates ({x}, {y}, {z}) out of range")
            }
            Self::StateMismatch {
                idx,
                expected,
                actual,
            } => write!(
                f,
                "section index {} has state {actual}, expected {expected}",
                idx.as_usize()
            ),
        }
    }
}

impl std::error::Error for DeltaCoreError {}

/// A validated index into a 16³ section in x-fastest, y-major order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SectionBlockIndex(u16);

impl SectionBlockIndex {
    pub const fn from_xyz(x: u8, y: u8, z: u8) -> Result<Self, DeltaCoreError> {
        if x >= SECTION_EDGE || y >= SECTION_EDGE || z >= SECTION_EDGE {
            return Err(DeltaCoreError::CoordinatesOutOfRange { x, y, z });
        }
        Ok(Self(
            section_index(x as usize, y as usize, z as usize) as u16
        ))
    }

    pub const fn from_raw(raw: u16) -> Result<Self, DeltaCoreError> {
        if raw as usize >= SECTION_VOLUME {
            return Err(DeltaCoreError::IndexOutOfRange { raw });
        }
        Ok(Self(raw))
    }

    pub const fn xyz(self) -> (u8, u8, u8) {
        let mask = (SECTION_EDGE - 1) as u16;
        (
            (self.0 & mask) as u8,
            (self.0 >> 8) as u8,
            ((self.0 >> 4) & mask) as u8,
        )
    }

    pub const fn as_usize(self) -> usize {
        self.0 as usize
    }
}

/// One checked transition between block states.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockEdit {
    pub idx: SectionBlockIndex,
    pub prev_state: u32,
    pub next_state: u32,
}

impl BlockEdit {
    fn verify_previous(self, actual: u32) -> Result<(), DeltaCoreError> {
        if actual != self.prev_state {
            return Err(DeltaCoreError::StateMismatch {
                idx: self.idx,
                expected: self.prev_state,
                actual,
            });
        }
        Ok(())
    }

    fn changes_state(self) -> bool {
        self.prev_state != self.next_state
    }
}

/// A section's net edits, sorted by index with one entry per changed cell.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SectionDelta {
    edits: Vec<BlockEdit>,
}

impl SectionDelta {
    /// Collapse an edit stream into a delta. Repeated edits at a cell must
    /// form a continuous chain in input order.
    pub fn from_edits(edits: impl IntoIterator<Item = BlockEdit>) -> Result<Self, DeltaCoreError> {
        let mut by_index = BTreeMap::<_, BlockEdit>::new();
        for edit in edits {
            match by_index.entry(edit.idx) {
                Entry::Vacant(entry) => {
                    entry.insert(edit);
                }
                Entry::Occupied(mut entry) => {
                    let prior = entry.get_mut();
                    edit.verify_previous(prior.next_state)?;
                    prior.next_state = edit.next_state;
                }
            }
        }
        Ok(Self {
            edits: by_index
                .into_values()
                .filter(|edit| edit.changes_state())
                .collect(),
        })
    }

    pub fn edits(&self) -> &[BlockEdit] {
        &self.edits
    }

    pub fn is_empty(&self) -> bool {
        self.edits.is_empty()
    }

    /// Verify every precondition before writing any edits.
    pub fn apply_to_section(
        &self,
        section: &mut [u32; SECTION_VOLUME],
    ) -> Result<(), DeltaCoreError> {
        for edit in &self.edits {
            edit.verify_previous(section[edit.idx.as_usize()])?;
        }
        for edit in &self.edits {
            section[edit.idx.as_usize()] = edit.next_state;
        }
        Ok(())
    }

    pub fn invert(&self) -> Self {
        Self {
            edits: self
                .edits
                .iter()
                .map(|edit| BlockEdit {
                    idx: edit.idx,
                    prev_state: edit.next_state,
                    next_state: edit.prev_state,
                })
                .collect(),
        }
    }

    /// Compose a compatible sequence, checking continuity at shared indices.
    /// Net-zero transitions are omitted from the result.
    pub fn compose(first: &Self, second: &Self) -> Result<Self, DeltaCoreError> {
        let mut first = first.edits.iter().peekable();
        let mut second = second.edits.iter().peekable();
        let mut edits = Vec::new();
        while let (Some(&&a), Some(&&b)) = (first.peek(), second.peek()) {
            match a.idx.cmp(&b.idx) {
                Ordering::Less => {
                    edits.push(a);
                    first.next();
                }
                Ordering::Greater => {
                    edits.push(b);
                    second.next();
                }
                Ordering::Equal => {
                    b.verify_previous(a.next_state)?;
                    let edit = BlockEdit {
                        next_state: b.next_state,
                        ..a
                    };
                    if edit.changes_state() {
                        edits.push(edit);
                    }
                    first.next();
                    second.next();
                }
            }
        }
        edits.extend(first.copied());
        edits.extend(second.copied());
        Ok(Self { edits })
    }
}

/// Compute the canonical delta between two section snapshots.
pub fn diff_sections(
    before: &[u32; SECTION_VOLUME],
    after: &[u32; SECTION_VOLUME],
) -> SectionDelta {
    let mut edits = Vec::new();
    for (index, (&prev_state, &next_state)) in before.iter().zip(after).enumerate() {
        if prev_state != next_state {
            edits.push(BlockEdit {
                idx: SectionBlockIndex(index as u16),
                prev_state,
                next_state,
            });
        }
    }
    SectionDelta { edits }
}

/// Hash a section's u32 block states in index order and big-endian byte order.
/// The preimage starts with the fixed tag `megu:section:v1\0`.
pub fn section_merkle_leaf(section: &[u32; SECTION_VOLUME]) -> Digest32 {
    let mut hash = Sha256::new();
    hash.update(b"megu:section:v1\0");
    for state in section {
        hash.update(&state.to_be_bytes());
    }
    hash.finalize()
}

/// Hash an ordered row of child digests. Use distinct domains for different
/// levels, such as section leaves within a chunk and chunk roots within a region.
/// The preimage is `megu:region:v1\0`, the domain's big-endian u64 byte length,
/// the domain itself, and the concatenated 32-byte child digests.
pub fn region_merkle_root(domain: &[u8], child_hashes: &[Digest32]) -> Digest32 {
    let mut hash = Sha256::new();
    hash.update(b"megu:region:v1\0");
    hash.update(&(domain.len() as u64).to_be_bytes());
    hash.update(domain);
    for child in child_hashes {
        hash.update(child);
    }
    hash.finalize()
}

/// Exclusively edit a section, restoring the original cells on drop unless
/// the transaction is committed.
#[must_use = "dropping the transaction rolls back its staged edits"]
pub struct SectionDeltaTxn<'a> {
    section: &'a mut [u32; SECTION_VOLUME],
    originals: BTreeMap<SectionBlockIndex, u32>,
}

impl<'a> SectionDeltaTxn<'a> {
    pub fn new(section: &'a mut [u32; SECTION_VOLUME]) -> Self {
        Self {
            section,
            originals: BTreeMap::new(),
        }
    }

    /// Stage a transition against the cell's current value. A failed edit
    /// leaves the cell and any earlier staged edits unchanged.
    pub fn stage_edit(&mut self, edit: BlockEdit) -> Result<(), DeltaCoreError> {
        let cell = &mut self.section[edit.idx.as_usize()];
        edit.verify_previous(*cell)?;
        if edit.changes_state() {
            self.originals.entry(edit.idx).or_insert(*cell);
            *cell = edit.next_state;
        }
        Ok(())
    }

    /// Keep the staged changes and issue a receipt for the resulting section.
    pub fn commit(mut self) -> CommittedDelta {
        let edits = self
            .originals
            .iter()
            .map(|(&idx, &prev_state)| BlockEdit {
                idx,
                prev_state,
                next_state: self.section[idx.as_usize()],
            })
            .filter(|edit| edit.changes_state())
            .collect();
        let leaf_hash = section_merkle_leaf(self.section);
        self.originals.clear();
        CommittedDelta {
            delta: SectionDelta { edits },
            leaf_hash,
        }
    }
}

impl Drop for SectionDeltaTxn<'_> {
    fn drop(&mut self) {
        for (&idx, &state) in &self.originals {
            self.section[idx.as_usize()] = state;
        }
    }
}

/// An opaque receipt that can only be issued by committing a section transaction.
#[derive(Debug)]
pub struct CommittedDelta {
    delta: SectionDelta,
    leaf_hash: Digest32,
}

impl CommittedDelta {
    pub fn delta(&self) -> &SectionDelta {
        &self.delta
    }

    pub fn leaf_hash(&self) -> Digest32 {
        self.leaf_hash
    }
}

/// One endpoint for moving a resource identified by `K`.
///
/// Simulation is read-only and returns at most the supplied amount. A commit
/// applies exactly its amount, which may be smaller than the simulated amount.
/// The ports' state must stay stable between simulation and commit.
pub trait ConservationPort<K: Eq + Clone> {
    fn simulate_extract(&self, key: &K, requested: u64) -> u64;
    fn commit_extract(&mut self, key: &K, amount: u64);
    fn simulate_insert(&self, key: &K, offered: u64) -> u64;
    fn commit_insert(&mut self, key: &K, amount: u64);
    fn total_amount(&self, key: &K) -> u64;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConservationError {
    ConservationViolation,
}

impl fmt::Display for ConservationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("resource totals violate the exact transfer contract")
    }
}

impl std::error::Error for ConservationError {}

/// Simulate both endpoints, then commit only their mutually transferable amount.
///
/// Success requires exact movement and conservation of the resource total.
/// A violation detected after commit is reported to the caller. Port mutations
/// already made by that point cannot be rolled back by this interface.
pub fn transfer_conserved<K: Eq + Clone>(
    from: &mut (impl ConservationPort<K> + ?Sized),
    to: &mut (impl ConservationPort<K> + ?Sized),
    key: &K,
    requested: u64,
) -> Result<u64, ConservationError> {
    if requested == 0 {
        return Ok(0);
    }
    let before_from = from.total_amount(key);
    let before_to = to.total_amount(key);
    let extractable = from.simulate_extract(key, requested).min(requested);
    let amount = to.simulate_insert(key, extractable).min(extractable);
    if amount == 0 {
        return Ok(0);
    }

    let violation = ConservationError::ConservationViolation;
    let expected_from = before_from.checked_sub(amount).ok_or(violation)?;
    before_to.checked_add(amount).ok_or(violation)?;
    from.commit_extract(key, amount);
    to.commit_insert(key, amount);

    let after_from = from.total_amount(key);
    let after_to = to.total_amount(key);
    let combined_before = u128::from(before_from) + u128::from(before_to);
    let combined_after = u128::from(after_from) + u128::from(after_to);
    if after_from != expected_from || combined_before != combined_after {
        return Err(violation);
    }
    Ok(amount)
}
