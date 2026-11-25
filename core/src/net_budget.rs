// SPDX-License-Identifier: LGPL-3.0-or-later
#![forbid(unsafe_code)]
//! `megu_core::net_budget` — symmetric encode-once frame caching and a
//! perceptual priority scheduler for chunk/entity streaming.
//!
//! Two cheap primitives recover most of the bandwidth a naive streamer
//! burns:
//!
//! * [`EncodeOnceFrameCache`] — chunk payloads are encoded and compressed
//!   **once**, then the same `Arc<[u8]>` is handed to every watching
//!   player. Freshness rides the canonical
//!   [`crate::section_rev::SharedSectionRevTable`]: a hit requires the
//!   cached `rev` to equal `chunk_rev(key)`, so the first block edit
//!   invalidates the frame for all players at once with no per-player
//!   bookkeeping. Capacity is bounded at compile time by
//!   [`crate::bounded::BoundedMap`].
//! * [`PerceptualBand`] + [`classify_perceptual_band`] +
//!   [`should_emit_tick`] — entities and chunks behind the viewer or far
//!   away don't need 20 Hz updates. A pure geometric classifier sorts
//!   targets into near/mid/rear/quiescent bands, and the emit test
//!   staggers each band by `entity_id` so the surviving emissions spread
//!   evenly across ticks instead of bursting.
//!
//! Both halves are symmetric: a multiplayer server uses them to cut wire
//! traffic, and a client GPU/LOD lane can classify and stagger draw work
//! with the same code and the same semantics.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::bounded::BoundedMap;
use crate::coords::ChunkKey;
use crate::hash::crc32_ieee;
use crate::section_rev::SharedSectionRevTable;

/// One encoded, checksummed, shareable wire frame.
///
/// `bytes` is reference-counted, so handing the same encoded payload to
/// `N` players costs `N` pointer clones and zero re-encoding or copies.
/// `crc32` rides along so transports and tests can validate the bytes
/// without trusting the cache.
#[derive(Clone, Debug)]
pub struct SharedEncodedFrame {
    /// Chunk column this frame encodes.
    pub key: ChunkKey,
    /// [`SharedSectionRevTable::chunk_rev`] value the encoding was made
    /// against; a hit requires the table to still read this revision.
    pub rev: u64,
    /// IEEE-802.3 CRC-32 of `bytes` ([`crc32_ieee`]).
    pub crc32: u32,
    /// The encoded payload, shared across every recipient.
    pub bytes: Arc<[u8]>,
}

impl SharedEncodedFrame {
    /// Recompute the CRC and compare — `true` when the stored checksum
    /// matches the bytes (encoding integrity check for tests and
    /// transports).
    #[must_use]
    pub fn verify(&self) -> bool {
        crc32_ieee(&self.bytes) == self.crc32
    }
}

/// Bounded revision-keyed cache of encode-once wire frames.
///
/// `MAX` is a compile-time cap: the cache can never grow past it — the
/// least-recently-stored frame is evicted instead (see
/// [`crate::bounded::BoundedMap`]). Hit statistics are kept on atomic
/// counters so `get` stays a shared `&self` lookup across the per-player
/// send loop.
#[derive(Debug)]
pub struct EncodeOnceFrameCache<const MAX: usize> {
    frames: BoundedMap<ChunkKey, SharedEncodedFrame, MAX>,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl<const MAX: usize> EncodeOnceFrameCache<MAX> {
    /// Empty cache bounded at `MAX` frames.
    #[must_use]
    pub fn new() -> Self {
        Self {
            frames: BoundedMap::new(),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    /// `O(1)` shared lookup: returns the cached frame — one `Arc` clone,
    /// no re-encoding — when `key` is cached *and* its stored `rev` still
    /// equals `rev_table.chunk_rev(key)`. Any block edit in the column
    /// bumps the table rev and turns the next lookup into a miss.
    ///
    /// Counts a hit or a miss on the internal counters.
    #[must_use]
    pub fn get(
        &self,
        key: ChunkKey,
        rev_table: &SharedSectionRevTable,
    ) -> Option<SharedEncodedFrame> {
        let hit = self
            .frames
            .get(&key)
            .filter(|frame| frame.rev == rev_table.chunk_rev(key))
            .cloned();
        if hit.is_some() {
            self.hits.fetch_add(1, Ordering::Relaxed);
        } else {
            self.misses.fetch_add(1, Ordering::Relaxed);
        }
        hit
    }

    /// Same freshness rule as [`get`](Self::get) but against an explicit
    /// revision, for callers that already hold `chunk_rev(key)`.
    /// Does not touch the hit/miss counters.
    #[must_use]
    pub fn get_rev(&self, key: ChunkKey, current_rev: u64) -> Option<SharedEncodedFrame> {
        self.frames
            .get(&key)
            .filter(|frame| frame.rev == current_rev)
            .cloned()
    }

    /// Store `bytes` as `key`'s frame at revision `rev`, computing its
    /// CRC once. Evicts the least-recently-stored frame when the cache
    /// is at `MAX`, and returns the stored frame (the same `Arc` every
    /// later `get` hands out).
    pub fn store(&mut self, key: ChunkKey, rev: u64, bytes: Arc<[u8]>) -> SharedEncodedFrame {
        let frame = SharedEncodedFrame {
            key,
            rev,
            crc32: crc32_ieee(&bytes),
            bytes,
        };
        // If the insert evicted a stale frame it is simply dropped; the
        // BoundedMap guarantees len <= MAX either way.
        let _evicted = self.frames.insert(key, frame.clone());
        frame
    }

    /// [`store`](Self::store) with the revision read straight from the
    /// canonical table — the common "just re-encoded after a miss" path.
    pub fn store_fresh(
        &mut self,
        key: ChunkKey,
        rev_table: &SharedSectionRevTable,
        bytes: Arc<[u8]>,
    ) -> SharedEncodedFrame {
        self.store(key, rev_table.chunk_rev(key), bytes)
    }

    /// Drop `key`'s cached frame, returning it when present. For explicit
    /// eviction (chunk unload); revision bumps make it unnecessary for
    /// ordinary edits.
    pub fn invalidate(&mut self, key: ChunkKey) -> Option<SharedEncodedFrame> {
        self.frames.remove(&key)
    }

    /// Revision of `key`'s cached frame, if any (regardless of whether it
    /// is still fresh).
    #[inline]
    #[must_use]
    pub fn cached_rev(&self, key: ChunkKey) -> Option<u64> {
        self.frames.get(&key).map(|frame| frame.rev)
    }

    /// Fresh-cache hits so far.
    #[inline]
    #[must_use]
    pub fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    /// Stale-or-absent lookups so far.
    #[inline]
    #[must_use]
    pub fn misses(&self) -> u64 {
        self.misses.load(Ordering::Relaxed)
    }

    /// Frames currently held (`<= MAX` by construction).
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    /// `true` when nothing is cached.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// Compile-time capacity — always exactly `MAX`.
    #[inline]
    #[must_use]
    pub const fn capacity(&self) -> usize {
        MAX
    }
}

impl<const MAX: usize> Default for EncodeOnceFrameCache<MAX> {
    fn default() -> Self {
        Self::new()
    }
}

/// How much an update stream can afford to send for one target per tick.
///
/// The discriminant *is* the tick stride — the number of ticks between
/// emissions for this band at the canonical 20 ticks/second:
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
#[repr(u8)]
pub enum PerceptualBand {
    /// No emissions at all — the target is out of range or provably
    /// static, so skipping it is free bandwidth.
    Quiescent = 0,
    /// In front of the viewer and close: full-rate 20 Hz (every tick).
    InFrustumNear = 1,
    /// In front of the viewer but mid-range: 10 Hz (every 2 ticks).
    InFrustumMid = 2,
    /// Behind the viewer or far-but-visible: 4 Hz (every 5 ticks).
    RearOrFar = 5,
}

/// Cosine of the forward half-angle that counts as "in frustum".
/// `0.5` ≈ a 120° full cone — a deliberately generous superset of typical
/// camera FOV plus peripheral meshing, so borderline targets are
/// over-served rather than starved.
pub const FRUSTUM_HALF_ANGLE_COS: f32 = 0.5;

impl PerceptualBand {
    /// Ticks between emissions for this band; `0` means never emit.
    #[inline]
    #[must_use]
    pub const fn tick_stride(self) -> u64 {
        self as u64
    }

    /// Emissions-per-second at the canonical 20 ticks/second.
    #[inline]
    #[must_use]
    pub const fn hz_at_20tps(self) -> u64 {
        match self {
            Self::Quiescent => 0,
            Self::InFrustumNear => 20,
            Self::InFrustumMid => 10,
            Self::RearOrFar => 4,
        }
    }
}

/// Classify one target's update urgency from pure geometry.
///
/// * Distance past `far_dist` → [`PerceptualBand::Quiescent`].
/// * A target sitting exactly on the viewer → [`PerceptualBand::InFrustumNear`].
/// * Inside `near_dist` and inside the forward cone → `InFrustumNear`.
/// * A **stationary** target outside `near_dist` → `Quiescent` (nothing
///   new can happen between full resyncs).
/// * Inside `far_dist` and inside the forward cone → `InFrustumMid`.
/// * Anything else in range → `RearOrFar`.
///
/// `viewer_fwd` need not be normalized; a zero-length forward vector
/// counts as "facing nowhere" (everything rear).
#[must_use]
pub fn classify_perceptual_band(
    viewer_pos: [f32; 3],
    viewer_fwd: [f32; 3],
    target_pos: [f32; 3],
    moving: bool,
    near_dist: f32,
    far_dist: f32,
) -> PerceptualBand {
    let dx = target_pos[0] - viewer_pos[0];
    let dy = target_pos[1] - viewer_pos[1];
    let dz = target_pos[2] - viewer_pos[2];
    let dist_sq = dx * dx + dy * dy + dz * dz;
    if dist_sq > far_dist * far_dist {
        return PerceptualBand::Quiescent;
    }
    let dist = dist_sq.sqrt();
    if dist == 0.0 {
        return PerceptualBand::InFrustumNear;
    }
    let fwd_len = (viewer_fwd[0] * viewer_fwd[0]
        + viewer_fwd[1] * viewer_fwd[1]
        + viewer_fwd[2] * viewer_fwd[2])
        .sqrt();
    let in_frustum = fwd_len != 0.0
        && (dx * viewer_fwd[0] + dy * viewer_fwd[1] + dz * viewer_fwd[2])
            / (dist * fwd_len)
            >= FRUSTUM_HALF_ANGLE_COS;
    if in_frustum && dist <= near_dist {
        return PerceptualBand::InFrustumNear;
    }
    if !moving && dist > near_dist {
        return PerceptualBand::Quiescent;
    }
    if in_frustum {
        return PerceptualBand::InFrustumMid;
    }
    PerceptualBand::RearOrFar
}

/// Should this band emit an update on `tick` for `entity_id`?
///
/// `Quiescent` never emits. Otherwise emission fires once per
/// `tick_stride()` ticks, staggered by `entity_id` so a fleet of entities
/// in the same band spreads across the stride window instead of bursting
/// on one tick: `entity 0` emits on tick `0`, `entity 1` on tick `1`, and
/// so on modulo the stride.
#[inline]
#[must_use]
pub fn should_emit_tick(band: PerceptualBand, tick: u64, entity_id: u32) -> bool {
    let stride = band.tick_stride();
    stride != 0 && tick.wrapping_add(u64::from(entity_id)) % stride == 0
}
