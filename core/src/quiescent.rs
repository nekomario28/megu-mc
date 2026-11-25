// SPDX-License-Identifier: LGPL-3.0-or-later
#![forbid(unsafe_code)]
//! `megu_core::quiescent` — a zero-work quiescent chunk scheduler.
//!
//! Real Minecraft-like worlds are overwhelmingly idle: of the thousands of
//! chunk columns a server keeps loaded, only the handful containing fluids,
//! block entities, mobs, redstone or modded machines actually need tick
//! work. The naive design — iterating every loaded chunk each tick and
//! asking each "do you need work?" — is `O(loaded)` even when the answer is
//! almost always "no".
//!
//! [`QuiescentChunkScheduler`] flips the structure: it stores *only* the
//! chunks whose [`ActivityMask`] is non-zero inside the canonical
//! [`crate::sparse_set::SparseSet`], so the per-tick loop touches a dense,
//! cache-contiguous slice of exactly the chunks that have work and
//! physically never visits an idle one (`O(0)` per quiescent chunk, not
//! `O(1)`-check). Delayed reactivation rides the canonical
//! [`crate::timing_wheel::TimingWheel`], and block mutations fan out
//! through the canonical [`crate::section_rev::SharedSectionRevTable`] so
//! every derived cache (lighting, meshing, wire caches) and the activity
//! mask itself are invalidated by the same edit.
//!
//! [`ActivityMask`] is a plain `u32` bitflag word: six bits are reserved
//! for vanilla categories and [`ActivityMask::custom`] leaves the rest open
//! so any mod can register its own wake sources without a new scheduler.

use crate::coords::{ChunkKey, CoordMap, DimIndex};
use crate::section_rev::SharedSectionRevTable;
use crate::sparse_set::SparseSet;
use crate::timing_wheel::{TimerHandle, TimingWheel};

/// Bitflag word describing which ticking systems have work inside a chunk.
///
/// A chunk with `mask == NONE` is *quiescent*: it is not stored in the
/// scheduler's active set at all, so the tick loop never visits it. Bits
/// `0..=5` name the vanilla wake sources; bits `6..=31` are open to mods
/// via [`ActivityMask::custom`].
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct ActivityMask(pub u32);

impl ActivityMask {
    /// No pending activity: the chunk is quiescent.
    pub const NONE: Self = Self(0);
    /// Fluid scheduled ticks / spread (water, lava, modded fluids).
    pub const FLUID: Self = Self(1 << 0);
    /// Redstone wire / component updates.
    pub const REDSTONE: Self = Self(1 << 1);
    /// Block entity ticking (furnaces, chests, modded machines).
    pub const BLOCK_ENTITY: Self = Self(1 << 2);
    /// Mob AI / spawning activity.
    pub const MOB: Self = Self(1 << 3);
    /// Random ticks (crop growth, fire spread, ice melt).
    pub const RANDOM_TICK: Self = Self(1 << 4);
    /// Generic modded machine wake source.
    pub const MOD_MACHINE: Self = Self(1 << 5);
    /// Union of every vanilla-reserved bit (`0..=5`).
    pub const ALL_VANILLA: Self = Self(0b11_1111);

    /// Arbitrary mod-owned wake bit. Bits `0..=5` collide with the vanilla
    /// constants above; mods should allocate from `6..=31`. The bit index
    /// wraps at 32 so this stays `const`.
    #[inline]
    #[must_use]
    pub const fn custom(bit: u8) -> Self {
        Self(1u32 << (bit & 31))
    }

    /// Raw bit word.
    #[inline]
    #[must_use]
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// `true` when no activity bits are set (chunk may go quiescent).
    #[inline]
    #[must_use]
    pub const fn is_none(self) -> bool {
        self.0 == 0
    }

    /// `true` when any bit in `other` is set in `self`.
    #[inline]
    #[must_use]
    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    /// `true` when every bit in `other` is set in `self`.
    #[inline]
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Set all bits present in `other`.
    #[inline]
    pub fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }

    /// Clear all bits present in `other`.
    #[inline]
    pub fn remove(&mut self, other: Self) {
        self.0 &= !other.0;
    }
}

impl std::ops::BitOr for ActivityMask {
    type Output = Self;

    #[inline]
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for ActivityMask {
    #[inline]
    fn bitor_assign(&mut self, rhs: Self) {
        self.insert(rhs);
    }
}

impl std::ops::BitAnd for ActivityMask {
    type Output = Self;

    #[inline]
    fn bitand(self, rhs: Self) -> Self {
        Self(self.0 & rhs.0)
    }
}

/// One live chunk inside [`QuiescentChunkScheduler::active_chunks`].
///
/// Entries sit in the scheduler's dense `SparseSet` values array, so the
/// whole active working set is one contiguous scan — no pointer chasing
/// across idle chunks.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ActiveChunkEntry {
    /// Chunk column that has pending work.
    pub key: ChunkKey,
    /// Which wake sources currently apply.
    pub mask: ActivityMask,
    /// Scheduler tick at which this chunk last (re-)activated. `u64::MAX`
    /// before the first [`QuiescentChunkScheduler::advance_tick`].
    pub last_woken_tick: u64,
}

/// Neighbour offsets along one horizontal axis for a border block:
/// `-1` when the block is on the low edge, `+1` on the high edge.
#[inline]
fn border_offsets(coord: i32) -> &'static [i32] {
    match coord & 15 {
        0 => &[-1, 0],
        15 => &[0, 1],
        _ => &[0],
    }
}

/// Scheduler that keeps only non-quiescent chunks in the tick path.
///
/// `ChunkKey`s map to dense `u32` slot ids (assigned monotonically and
/// recycled through a free list), and only those slots live in the
/// [`SparseSet`]. When a chunk's mask drains to [`ActivityMask::NONE`]
/// the entry is swap-removed and its slot recycled, so an idle chunk
/// occupies zero bytes of the hot path and is never visited by
/// [`active_chunks`](Self::active_chunks) again.
#[derive(Debug)]
pub struct QuiescentChunkScheduler {
    /// Dense storage of only the non-quiescent chunks, keyed by slot id.
    active: SparseSet<ActiveChunkEntry>,
    /// `ChunkKey` -> dense slot id, only for chunks that have ever been
    /// registered (and not yet [`forget_chunk`](Self::forget_chunk)ed).
    slots: CoordMap<ChunkKey, u32>,
    /// Recycled dense slot ids.
    free_slots: Vec<u32>,
    /// Next never-before-used dense slot id.
    next_slot: u32,
    /// Delayed wakeups, in canonical timing-wheel form.
    wheel: TimingWheel<ChunkKey>,
    /// Activity bits awaiting a delayed wake, OR-accumulated per key until
    /// its timer fires.
    pending_wake: CoordMap<ChunkKey, ActivityMask>,
    /// Reusable wheel-drain scratch so `advance_tick` stays alloc-free.
    scratch: Vec<ChunkKey>,
}

impl QuiescentChunkScheduler {
    /// Empty scheduler at tick zero.
    #[must_use]
    pub fn new() -> Self {
        Self {
            active: SparseSet::new(),
            slots: CoordMap::default(),
            free_slots: Vec::new(),
            next_slot: 0,
            wheel: TimingWheel::new(),
            pending_wake: CoordMap::default(),
            scratch: Vec::new(),
        }
    }

    /// Empty scheduler with dense capacity for `capacity` simultaneously
    /// active chunks.
    #[must_use]
    pub fn with_active_capacity(capacity: usize) -> Self {
        Self {
            active: SparseSet::with_capacity(capacity),
            ..Self::new()
        }
    }

    /// Current scheduler tick (completed [`advance_tick`](Self::advance_tick)
    /// calls, shared with the internal timing wheel).
    #[inline]
    #[must_use]
    pub fn current_tick(&self) -> u64 {
        self.wheel.current_tick()
    }

    /// Number of currently non-quiescent chunks.
    #[inline]
    #[must_use]
    pub fn active_len(&self) -> usize {
        self.active.len()
    }

    /// `true` when `key` is in the active set.
    #[inline]
    #[must_use]
    pub fn is_active(&self, key: ChunkKey) -> bool {
        self.slots
            .get(&key)
            .is_some_and(|&slot| self.active.contains(slot))
    }

    /// `key`'s current activity mask, [`ActivityMask::NONE`] when
    /// quiescent.
    #[inline]
    #[must_use]
    pub fn activity_of(&self, key: ChunkKey) -> ActivityMask {
        self.slots
            .get(&key)
            .and_then(|&slot| self.active.get(slot))
            .map_or(ActivityMask::NONE, |entry| entry.mask)
    }

    /// Number of delayed wakeups still pending in the wheel.
    #[inline]
    #[must_use]
    pub fn pending_delayed_wakes(&self) -> usize {
        self.wheel.len()
    }

    /// Cache-contiguous slice over *only* the active chunks — the whole
    /// point of the scheduler: quiescent chunks cost zero iteration and
    /// zero cache lines.
    #[inline]
    #[must_use]
    pub fn active_chunks(&self) -> &[ActiveChunkEntry] {
        self.active.as_slice()
    }

    /// Fraction of `total_loaded` chunks that are quiescent (`0.0` when
    /// everything is awake, `1.0` when nothing is — including the
    /// degenerate `total_loaded == 0` case where nothing can have work).
    /// Clamped so an over-reported `total_loaded` cannot go negative.
    #[inline]
    #[must_use]
    pub fn quiescent_ratio(&self, total_loaded: usize) -> f64 {
        if total_loaded == 0 {
            return 1.0;
        }
        let active = self.active.len().min(total_loaded);
        1.0 - (active as f64 / total_loaded as f64)
    }

    /// O(1) wake: mark `key` active with `bits` OR-ed into its mask.
    /// A `NONE` mask is a no-op.
    pub fn wake(&mut self, key: ChunkKey, bits: ActivityMask) {
        if bits.is_none() {
            return;
        }
        let tick = self.wheel.current_tick();
        let slot = self.slot_for(key);
        match self.active.get_mut(slot) {
            Some(entry) => {
                entry.mask.insert(bits);
                entry.last_woken_tick = tick;
            }
            None => {
                self.active.insert(
                    slot,
                    ActiveChunkEntry {
                        key,
                        mask: bits,
                        last_woken_tick: tick,
                    },
                );
            }
        }
    }

    /// O(1) delayed wake: `bits` is OR-accumulated and applied when the
    /// timer fires `delay_ticks` [`advance_tick`](Self::advance_tick)s
    /// later (`0`/`1` fire on the next advance, matching vanilla
    /// scheduled-tick semantics).
    pub fn wake_after(
        &mut self,
        key: ChunkKey,
        bits: ActivityMask,
        delay_ticks: u64,
    ) -> TimerHandle {
        self.pending_wake
            .entry(key)
            .or_insert(ActivityMask::NONE)
            .insert(bits);
        self.wheel.schedule_after(delay_ticks, key)
    }

    /// Clear `bits` from `key`'s mask. When the mask reaches
    /// [`ActivityMask::NONE`] the chunk is physically swap-removed from
    /// the dense set: idle chunks are not "skipped cheaply", they are
    /// *gone* (`O(0)` per tick). Returns the mask after removal.
    pub fn clear_bits(&mut self, key: ChunkKey, bits: ActivityMask) -> ActivityMask {
        let Some(&slot) = self.slots.get(&key) else {
            return ActivityMask::NONE;
        };
        let Some(entry) = self.active.get_mut(slot) else {
            return ActivityMask::NONE;
        };
        entry.mask.remove(bits);
        let mask = entry.mask;
        if mask.is_none() {
            self.active.remove(slot);
            self.slots.remove(&key);
            self.free_slots.push(slot);
        }
        mask
    }

    /// The single entry point every block mutation should take: bump the
    /// canonical [`SharedSectionRevTable`] (which already propagates to
    /// border-adjacent sections and their chunks), wake the owning chunk
    /// with `bits`, and wake every horizontally adjacent chunk column the
    /// block touches (`x & 15` or `z & 15` in `{0, 15}` — corner edits wake
    /// all four columns, exactly matching how light and fluid state leak).
    ///
    /// Returns the `(section_rev, chunk_rev)` pair from the rev-table bump
    /// so callers can key their own derived caches without a second lookup.
    pub fn on_block_mutated(
        &mut self,
        dim: DimIndex,
        x: i32,
        y: i32,
        z: i32,
        bits: ActivityMask,
        rev_table: &mut SharedSectionRevTable,
    ) -> (u64, u64) {
        let revs = rev_table.bump_block(dim, x, y, z);
        let cx = x >> 4;
        let cz = z >> 4;
        self.wake(
            ChunkKey {
                dim,
                cx,
                cz,
            },
            bits,
        );
        for &dx in border_offsets(x) {
            for &dz in border_offsets(z) {
                if dx == 0 && dz == 0 {
                    continue;
                }
                let (Some(ncx), Some(ncz)) = (cx.checked_add(dx), cz.checked_add(dz))
                else {
                    continue;
                };
                self.wake(
                    ChunkKey {
                        dim,
                        cx: ncx,
                        cz: ncz,
                    },
                    bits,
                );
            }
        }
        revs
    }

    /// Advance the scheduler one tick: every delayed wake whose timer
    /// expires now applies its accumulated bits and is appended to
    /// `out_woken` (timers whose pending bits were dropped by
    /// [`forget_chunk`](Self::forget_chunk) are silently retired).
    ///
    /// Returns the number of chunks woken this tick.
    pub fn advance_tick(&mut self, out_woken: &mut Vec<ChunkKey>) -> usize {
        let mut fired = std::mem::take(&mut self.scratch);
        self.wheel.advance_into(&mut fired);
        let mut woken = 0usize;
        for key in fired.drain(..) {
            if let Some(bits) = self.pending_wake.remove(&key) {
                self.wake(key, bits);
                out_woken.push(key);
                woken += 1;
            }
        }
        self.scratch = fired;
        woken
    }

    /// Drop every record of `key`: its active entry (if any), its slot
    /// mapping, and its pending delayed-wake bits. In-flight wheel timers
    /// for the key retire as no-ops when they fire. For chunk unload.
    pub fn forget_chunk(&mut self, key: ChunkKey) {
        self.pending_wake.remove(&key);
        if let Some(slot) = self.slots.remove(&key) {
            if self.active.remove(slot).is_some() {
                self.free_slots.push(slot);
            }
        }
    }

    /// Drop every active entry, pending wake and timer; the tick counter
    /// is preserved (clearing work is not time travel).
    pub fn clear(&mut self) {
        self.active.clear();
        self.slots.clear();
        self.free_slots.clear();
        self.wheel.clear();
        self.pending_wake.clear();
    }

    fn slot_for(&mut self, key: ChunkKey) -> u32 {
        if let Some(&slot) = self.slots.get(&key) {
            return slot;
        }
        let slot = self.free_slots.pop().unwrap_or_else(|| {
            let slot = self.next_slot;
            self.next_slot = self
                .next_slot
                .checked_add(1)
                .expect("dense chunk slot ids exhausted u32 space");
            slot
        });
        self.slots.insert(key, slot);
        slot
    }
}

impl Default for QuiescentChunkScheduler {
    fn default() -> Self {
        Self::new()
    }
}
