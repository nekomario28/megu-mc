// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! `mc_spine_core::server_perf_governors` — deterministic, zero-dependency
//! single-node server optimization governors that aim past what Storia (a
//! Folia fork) and Pumpkin (a Rust reimplementation) do today, while
//! preserving 100% vanilla gameplay parity.
//!
//! Three primitives, each attacking a different hot path of a busy server:
//!
//! * [`VelocityViewGovernor`] — anisotropic velocity-adaptive view-cone
//!   planning. Storia shrinks the view distance symmetrically under load;
//!   this governor keeps **full forward reach** for fast players (Elytra,
//!   rocket, Create train, airship) so terrain never pops in late, while
//!   pruning the receding rear half of the send set. Non-finite velocity
//!   input fails closed to the symmetric stationary plan.
//! * [`CrowdTickGuard`] — parity-safe prime-stride crowd tick guard. Storia's
//!   tick guard throttles mobs in dense chunks; this guard **never** skips
//!   movement, gravity, damage, cramming, or combat for any entity, so mob
//!   farms, water pushers, fall traps, and entity cramming remain
//!   vanilla-exact. Only redundant idle AI (pathfinding and ambient look) is
//!   strided, deterministically, by a prime period keyed on entity id plus
//!   tick — zero lockstep spikes, fair spread.
//! * [`CompressedChunkPacketCache`] — epoch-tagged zero-copy shared chunk
//!   packet cache. Where Pumpkin serializes and compresses the `0x27` chunk
//!   packet per recipient, this cache encodes once per
//!   `(dim, chunk, dirty_epoch, antixray_profile)` key and hands out
//!   [`Arc`]-shared immutable slices to every viewer: O(1) fan-out with zero
//!   re-serialization, zero re-compression, zero heap copy.
//!
//! Everything here is pure `std`, allocation-bounded, clock-free
//! (`debt_ratchet` contract), panic-free on hostile input, and fully
//! deterministic: eviction and striding order are driven by monotonic
//! sequence numbers and integer arithmetic, never by wall-clock time or
//! thread scheduling.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

// ===========================================================================
// 1. VelocityViewGovernor — anisotropic velocity-adaptive view-cone
// ===========================================================================

/// Hard lower bound on `view_distance` (mirrors the vanilla floor).
pub const MIN_VIEW_DISTANCE: u8 = 2;
/// Hard upper bound on `view_distance` (mirrors the vanilla ceiling).
pub const MAX_VIEW_DISTANCE: u8 = 32;

/// Horizontal speed, in blocks/tick, at or above which a player is treated
/// as fast-moving and the view switches to the forward-biased cone. 0.75
/// covers Elytra flight, rocket boosts, Create trains, and airships while
/// leaving sprint-jumping (~0.28) and swimming in the stationary plan.
pub const HIGH_SPEED_THRESHOLD: f32 = 0.75;

/// Direction alignment (`dot(offset_dir, heading)`) at or above which a
/// chunk counts as inside the forward hemisphere and keeps full
/// `view_distance` reach.
pub const FORWARD_MIN_ALIGNMENT: f32 = 0.15;

/// Direction alignment below which a chunk is receding behind the player
/// and its reach is pruned to the rear radius.
pub const REAR_MAX_ALIGNMENT: f32 = -0.20;

/// Floor for the rear radius: `max(view_distance / 2, MIN_REAR_RADIUS)`.
pub const MIN_REAR_RADIUS: i32 = 3;

/// Anisotropic velocity-adaptive view-cone governor.
///
/// Replaces Storia-style symmetric view-distance shrinking with a
/// direction-aware send plan:
///
/// * **Stationary or walking** (`speed < `[`HIGH_SPEED_THRESHOLD`], or any
///   non-finite velocity): emits the full symmetric Chebyshev square whose
///   radius is the load-scaled [`effective_radius`] — identical coverage to
///   vanilla's view distance, modulated by the adaptive load scale.
/// * **High speed** (`speed >= `[`HIGH_SPEED_THRESHOLD`]): the send set
///   becomes a forward-biased cone. Chunks inside the forward hemisphere
///   keep the **unscaled** `view_distance` reach (forward terrain never
///   pops in late); receding chunks behind the player are pruned to
///   `max(view_distance / 2, `[`MIN_REAR_RADIUS`]`)`; side chunks
///   interpolate smoothly between the two. The load governor still gets
///   its say through `max_chunks_per_tick`, which the caller scales.
///
/// [`plan_chunk_sends`] returns the surviving chunk coordinates ordered by
/// forward alignment then proximity — nearest and dead-ahead first — and
/// capped at `max_chunks_per_tick`, so a bandwidth budget slices the cone
/// from the least important chunk inward.
///
/// [`effective_radius`]: Self::effective_radius
/// [`plan_chunk_sends`]: Self::plan_chunk_sends
#[derive(Clone, Debug)]
pub struct VelocityViewGovernor {
    /// Base view distance, sanitized into [`MIN_VIEW_DISTANCE`..=`MAX_VIEW_DISTANCE`].
    view_distance: u8,
}

impl VelocityViewGovernor {
    /// A governor at `view_distance` (clamped into `2..=32`).
    pub fn new(view_distance: u8) -> Self {
        Self {
            view_distance: view_distance.clamp(MIN_VIEW_DISTANCE, MAX_VIEW_DISTANCE),
        }
    }

    /// The configured base view distance after clamping.
    pub fn view_distance(&self) -> u8 {
        self.view_distance
    }

    /// Update the base view distance (clamped into `2..=32`).
    pub fn set_view_distance(&mut self, view_distance: u8) {
        self.view_distance = view_distance.clamp(MIN_VIEW_DISTANCE, MAX_VIEW_DISTANCE);
    }

    /// Whether `(vx, vz)` describes a finite velocity at or above the
    /// high-speed threshold. Non-finite input is *not* high speed: the
    /// governor fails closed to the symmetric stationary plan.
    pub fn is_high_speed(vx: f32, vz: f32) -> bool {
        vx.is_finite() && vz.is_finite() && vx.hypot(vz) >= HIGH_SPEED_THRESHOLD
    }

    /// Sanitized load scale: finite values clamp into the governor contract
    /// `[0.25, 1.0]`; non-finite input falls back to `1.0` so a broken
    /// load sample never under-sends chunks.
    fn sanitize_load_scale(load_scale: f32) -> f32 {
        if load_scale.is_finite() {
            load_scale.clamp(0.25, 1.0)
        } else {
            1.0
        }
    }

    /// Symmetric stationary view radius:
    /// `round(view_distance * load_scale)` clamped into `2..=view_distance`.
    pub fn effective_radius(&self, load_scale: f32) -> i32 {
        let vd = f32::from(self.view_distance);
        (vd * Self::sanitize_load_scale(load_scale))
            .round()
            .clamp(2.0, vd) as i32
    }

    /// Directional reach for a moving player: the radius a chunk at
    /// `alignment` = `dot(offset_dir, heading)` may extend to.
    ///
    /// * `alignment >= `[`FORWARD_MIN_ALIGNMENT`]: full `view_distance`.
    /// * `alignment < `[`REAR_MAX_ALIGNMENT`]: `max(view_distance / 2, 3)`.
    /// * In between: a hermite smoothstep between the rear radius and full
    ///   reach, so the cone envelope has no radius cliff a player could
    ///   see chunks popping across.
    ///
    /// `alignment` outside `[-1, 1]` (or non-finite) is clamped, keeping
    /// the envelope defined for any input.
    pub fn directional_radius(&self, alignment: f32) -> i32 {
        let vd = i32::from(self.view_distance);
        let alignment = if alignment.is_finite() {
            alignment.clamp(-1.0, 1.0)
        } else {
            -1.0 // Hostile alignment: assume fully receding (minimum reach).
        };
        if alignment >= FORWARD_MIN_ALIGNMENT {
            return vd;
        }
        let rear = (vd / 2).max(MIN_REAR_RADIUS);
        if alignment < REAR_MAX_ALIGNMENT {
            return rear;
        }
        let t = (alignment - REAR_MAX_ALIGNMENT) / (FORWARD_MIN_ALIGNMENT - REAR_MAX_ALIGNMENT);
        let s = t * t * (3.0 - 2.0 * t);
        (rear as f32 + s * (vd - rear) as f32).round() as i32
    }

    /// Plan this tick's chunk sends for a player.
    ///
    /// `pcx`/`pcz` are the player's current chunk coordinates, `(vx, vz)`
    /// the horizontal velocity in blocks/tick, `load_scale` the
    /// [`crate::perf_kit::AdaptiveLoadGovernor`] output (`0.25..=1.0`,
    /// sanitized fail-closed), and `max_chunks_per_tick` the per-tick send
    /// budget (`0` = send nothing).
    ///
    /// Returns absolute chunk coordinates, sorted by forward alignment
    /// then proximity (nearest and dead-ahead first), at most
    /// `max_chunks_per_tick` entries. The player's own chunk — which has
    /// no direction — is always treated as dead-ahead.
    pub fn plan_chunk_sends(
        &self,
        pcx: i32,
        pcz: i32,
        vx: f32,
        vz: f32,
        load_scale: f32,
        max_chunks_per_tick: usize,
    ) -> Vec<(i32, i32)> {
        if max_chunks_per_tick == 0 {
            return Vec::new();
        }
        if Self::is_high_speed(vx, vz) {
            self.plan_cone(pcx, pcz, vx, vz, max_chunks_per_tick)
        } else {
            self.plan_symmetric(pcx, pcz, load_scale, max_chunks_per_tick)
        }
    }

    /// Symmetric plan: the full Chebyshev square at [`effective_radius`],
    /// nearest chunks first (ties broken by coordinate for determinism).
    ///
    /// [`effective_radius`]: Self::effective_radius
    fn plan_symmetric(
        &self,
        pcx: i32,
        pcz: i32,
        load_scale: f32,
        max_chunks_per_tick: usize,
    ) -> Vec<(i32, i32)> {
        let r = self.effective_radius(load_scale);
        let mut offsets = Vec::with_capacity(((2 * r + 1) * (2 * r + 1)) as usize);
        for dx in -r..=r {
            for dz in -r..=r {
                offsets.push((dx, dz));
            }
        }
        offsets.sort_by_key(|&(dx, dz)| (dx * dx + dz * dz, dx, dz));
        offsets
            .iter()
            .take(max_chunks_per_tick)
            .map(|&(dx, dz)| (pcx.saturating_add(dx), pcz.saturating_add(dz)))
            .collect()
    }

    /// High-speed cone plan: keep every offset whose distance fits inside
    /// the [`directional_radius`] envelope for its alignment with the
    /// heading, sorted by alignment (dead-ahead first) then proximity.
    ///
    /// [`directional_radius`]: Self::directional_radius
    fn plan_cone(
        &self,
        pcx: i32,
        pcz: i32,
        vx: f32,
        vz: f32,
        max_chunks_per_tick: usize,
    ) -> Vec<(i32, i32)> {
        let speed = vx.hypot(vz);
        let hx = vx / speed;
        let hz = vz / speed;
        let vd = i32::from(self.view_distance);

        let mut candidates: Vec<(i32, i32, f32, i32)> = Vec::new();
        for dx in -vd..=vd {
            for dz in -vd..=vd {
                let d2 = dx * dx + dz * dz;
                let (alignment, dist) = if d2 == 0 {
                    // The player's own chunk has no direction; it is always
                    // resident and always first.
                    (1.0_f32, 0.0_f32)
                } else {
                    let dist = (d2 as f32).sqrt();
                    ((dx as f32 * hx + dz as f32 * hz) / dist, dist)
                };
                if dist > self.directional_radius(alignment) as f32 {
                    continue;
                }
                candidates.push((dx, dz, alignment, d2));
            }
        }
        candidates.sort_by(|a, b| {
            b.2.partial_cmp(&a.2)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.3.cmp(&b.3))
                .then_with(|| a.0.cmp(&b.0))
                .then_with(|| a.1.cmp(&b.1))
        });
        candidates
            .iter()
            .take(max_chunks_per_tick)
            .map(|&(dx, dz, _, _)| (pcx.saturating_add(dx), pcz.saturating_add(dz)))
            .collect()
    }
}

// ===========================================================================
// 2. CrowdTickGuard — parity-safe prime-stride crowd tick guard
// ===========================================================================

/// Default per-chunk entity count above which crowd mobs start striding
/// their idle AI. Vanilla mob farms routinely exceed this inside a single
/// chunk; ordinary base or wilderness density stays below it.
pub const DEFAULT_CROWD_THRESHOLD: u32 = 16;

/// Tick classification for an entity: how much of its per-tick work is
/// allowed to be strided. The classifier (entity type, combat state,
/// redstone coupling) lives in the caller; the guard only consumes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EntityTickClass {
    /// Players, projectiles, falling blocks, primed TNT, minecarts, bosses,
    /// active combat targets, and redstone-coupled entities. **Always**
    /// runs at full 20 TPS — throttling any of these would break vanilla
    /// parity or be observable to players.
    Critical,
    /// An ordinary crowd mob (farm animal, idle hostile, villager) whose
    /// idle AI may be strided when its chunk is crowded.
    CrowdMob,
}

/// Per-entity, per-tick work decision produced by [`CrowdTickGuard`].
///
/// There is deliberately **no** "skip everything" variant: movement,
/// gravity, damage, entity cramming, and combat always run, so mob farms,
/// water pushers, fall traps, and cramming mechanics stay vanilla-exact.
/// The only thing a guard may defer is redundant idle AI.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TickDecision {
    /// Everything runs this tick: physics, damage, combat, idle AI
    /// pathfinding, and ambient look. Vanilla behavior.
    RunFull,
    /// Physics, damage, cramming, and combat still run; idle AI
    /// pathfinding and ambient look are deferred to the mob's next stride
    /// phase. Undetectable to players in aggregate: a strided mob simply
    /// re-evaluates its idle wander target slightly less often.
    PhysicsOnly,
}

impl TickDecision {
    /// `true` when every subsystem runs this tick.
    pub fn is_full(self) -> bool {
        matches!(self, Self::RunFull)
    }

    /// Movement, gravity, damage, cramming, and combat. Always `true` for
    /// every decision the guard can emit — this is the parity guarantee.
    pub fn run_physics_and_damage(self) -> bool {
        true
    }

    /// Idle AI pathfinding (wander target selection, goal re-evaluation).
    /// Runs on [`RunFull`] ticks only.
    ///
    /// [`RunFull`]: Self::RunFull
    pub fn run_ai_pathfinding(self) -> bool {
        matches!(self, Self::RunFull)
    }

    /// Ambient look (head tracking, idle sound/look animations). Runs on
    /// [`RunFull`] ticks only.
    ///
    /// [`RunFull`]: Self::RunFull
    pub fn run_ambient_look(self) -> bool {
        matches!(self, Self::RunFull)
    }

    /// Convenience: both idle-AI subsystems together.
    pub fn run_idle_ai(self) -> bool {
        matches!(self, Self::RunFull)
    }
}

/// Prime-stride crowd tick guard.
///
/// Tracks per-chunk entity density and decides, per entity per tick,
/// whether idle AI runs or is deferred:
///
/// * [`EntityTickClass::Critical`] → always [`TickDecision::RunFull`], at
///   any density. Never throttled.
/// * [`EntityTickClass::CrowdMob`] at `density <= crowd_threshold` →
///   [`TickDecision::RunFull`].
/// * [`EntityTickClass::CrowdMob`] at `density > crowd_threshold` →
///   [`TickDecision::PhysicsOnly`] on off-phase ticks, with physics always
///   running. The stride is prime — `2` for `17..=32`, `3` for `33..=64`,
///   `5` above — and the phase is `(eid.wrapping_abs() as u64 + tick)
///   % stride == 0`, so load spreads evenly across ticks with no lockstep
///   spike and no wall-clock involvement.
#[derive(Clone, Debug)]
pub struct CrowdTickGuard {
    /// Density strictly above which crowd mobs start striding.
    crowd_threshold: u32,
    /// Per-chunk live entity counts, keyed `(cx, cz)`.
    chunk_entity_counts: HashMap<(i32, i32), u32>,
}

impl Default for CrowdTickGuard {
    fn default() -> Self {
        Self::new(DEFAULT_CROWD_THRESHOLD)
    }
}

impl CrowdTickGuard {
    /// A guard that starts striding crowd mobs above `crowd_threshold`
    /// entities per chunk.
    pub fn new(crowd_threshold: u32) -> Self {
        Self {
            crowd_threshold,
            chunk_entity_counts: HashMap::new(),
        }
    }

    /// The configured crowding threshold.
    pub fn crowd_threshold(&self) -> u32 {
        self.crowd_threshold
    }

    /// Update the crowding threshold.
    pub fn set_crowd_threshold(&mut self, crowd_threshold: u32) {
        self.crowd_threshold = crowd_threshold;
    }

    /// Record the live entity count for chunk `(cx, cz)`.
    pub fn set_chunk_entity_count(&mut self, cx: i32, cz: i32, count: u32) {
        self.chunk_entity_counts.insert((cx, cz), count);
    }

    /// Live entity count last recorded for `(cx, cz)`; `0` if untracked.
    pub fn chunk_entity_count(&self, cx: i32, cz: i32) -> u32 {
        self.chunk_entity_counts
            .get(&(cx, cz))
            .copied()
            .unwrap_or(0)
    }

    /// Stop tracking `(cx, cz)` (chunk unloaded). Returns whether the
    /// chunk had a recorded count.
    pub fn remove_chunk(&mut self, cx: i32, cz: i32) -> bool {
        self.chunk_entity_counts.remove(&(cx, cz)).is_some()
    }

    /// Drop every recorded density (e.g., on full recount ticks).
    pub fn clear(&mut self) {
        self.chunk_entity_counts.clear();
    }

    /// Number of chunks currently tracked.
    pub fn tracked_chunk_count(&self) -> usize {
        self.chunk_entity_counts.len()
    }

    /// The prime idle-AI stride for a crowded density: `2` for
    /// `17..=32`, `3` for `33..=64`, `5` above `64`. Densities at or below
    /// the threshold never reach this — they run full — so the `_` arm
    /// covers every input the crowded branch can produce.
    pub fn idle_ai_stride(density: u32) -> u64 {
        match density {
            17..=32 => 2,
            33..=64 => 3,
            _ => 5,
        }
    }

    /// Decide this entity's work for `tick`.
    ///
    /// `eid` is the entity id; its absolute value is the entity's phase
    /// offset so consecutive ids land on different ticks of the stride.
    /// `i64::MIN` fails closed deterministically via `wrapping_abs`.
    ///
    /// Always returns a decision whose [`run_physics_and_damage`] is
    /// `true`; the only question ever decided here is idle AI.
    ///
    /// [`run_physics_and_damage`]: TickDecision::run_physics_and_damage
    pub fn decide(
        &self,
        class: EntityTickClass,
        eid: i64,
        cx: i32,
        cz: i32,
        tick: u64,
    ) -> TickDecision {
        match class {
            EntityTickClass::Critical => TickDecision::RunFull,
            EntityTickClass::CrowdMob => {
                let density = self.chunk_entity_count(cx, cz);
                if density <= self.crowd_threshold {
                    return TickDecision::RunFull;
                }
                let stride = Self::idle_ai_stride(density);
                if (eid.wrapping_abs() as u64)
                    .wrapping_add(tick)
                    .is_multiple_of(stride)
                {
                    TickDecision::RunFull
                } else {
                    TickDecision::PhysicsOnly
                }
            }
        }
    }
}

// ===========================================================================
// 3. CompressedChunkPacketCache — epoch-tagged zero-copy shared packet cache
// ===========================================================================

/// Cache key for one shareable compressed chunk packet.
///
/// Two viewers receive the same `Arc` slice only when every field matches:
/// the dimension, the chunk, the chunk's mutation epoch (any block change
/// bumps `dirty_epoch`, making every older encoding unmatchable), and the
/// anti-xray obfuscation profile (different profiles obfuscate differently
/// and must never share bytes).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ChunkPacketCacheKey {
    /// Dimension id.
    pub dim: i32,
    /// Chunk X coordinate.
    pub cx: i32,
    /// Chunk Z coordinate.
    pub cz: i32,
    /// Chunk mutation epoch; bumped on any block/heightmap change.
    pub dirty_epoch: u64,
    /// Anti-xray obfuscation profile selector.
    pub antixray_profile: u8,
}

/// One cached packet: the immutable, already-serialized and
/// already-compressed `0x27` chunk-data payload, plus its position in the
/// LRU order.
#[derive(Clone, Debug)]
struct CacheEntry {
    packet: Arc<[u8]>,
    /// Last-touch sequence number; smaller = colder.
    last_used: u64,
}

/// Bounded LRU cache of shared compressed chunk packets.
///
/// A chunk packet is serialized and compressed **once** per
/// [`ChunkPacketCacheKey`], then handed out to every viewer as an
/// [`Arc`]-cloned immutable slice — per-player fan-out costs a refcount
/// bump instead of a full encode+compress+copy. Eviction order is a
/// monotonic sequence number (no wall clock): hits refresh the entry's
/// sequence, and overflow evicts the smallest sequence first, which is
/// exactly least-recently-used and fully deterministic.
///
/// Statistics: [`hits`], [`misses`], and [`bytes_saved_by_sharing`], which
/// accumulates the payload size of every avoided re-encode.
///
/// [`hits`]: Self::hits
/// [`misses`]: Self::misses
/// [`bytes_saved_by_sharing`]: Self::bytes_saved_by_sharing
#[derive(Debug)]
pub struct CompressedChunkPacketCache {
    /// Maximum resident entries; `0` disables retention entirely.
    max_entries: usize,
    entries: HashMap<ChunkPacketCacheKey, CacheEntry>,
    /// LRU order: `last_used` sequence → key. Sequences are unique, so
    /// the smallest entry is unambiguous.
    lru: BTreeMap<u64, ChunkPacketCacheKey>,
    /// Monotonic touch counter; only ever increases.
    next_seq: u64,
    hits: u64,
    misses: u64,
    bytes_saved_by_sharing: u64,
}

impl CompressedChunkPacketCache {
    /// An empty cache retaining at most `max_entries` packets.
    pub fn new(max_entries: usize) -> Self {
        Self {
            max_entries,
            entries: HashMap::new(),
            lru: BTreeMap::new(),
            next_seq: 0,
            hits: 0,
            misses: 0,
            bytes_saved_by_sharing: 0,
        }
    }

    /// Configured capacity.
    pub fn max_entries(&self) -> usize {
        self.max_entries
    }

    /// Resident entries (never exceeds [`max_entries`]).
    ///
    /// [`max_entries`]: Self::max_entries
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is retained.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Number of `get_or_encode`/`get` calls served from the cache.
    pub fn hits(&self) -> u64 {
        self.hits
    }

    /// Number of `get_or_encode` calls that had to invoke `encode_fn`.
    pub fn misses(&self) -> u64 {
        self.misses
    }

    /// Total payload bytes whose re-serialization, re-compression, and
    /// re-copy were avoided by sharing an existing `Arc`.
    pub fn bytes_saved_by_sharing(&self) -> u64 {
        self.bytes_saved_by_sharing
    }

    /// Drop every entry and reset the LRU order (statistics are kept).
    pub fn clear(&mut self) {
        self.entries.clear();
        self.lru.clear();
    }

    /// Record a cache hit under `key`: refresh its LRU position, bump the
    /// counters, and return the shared payload.
    fn hit(&mut self, key: ChunkPacketCacheKey) -> Arc<[u8]> {
        let entry = self
            .entries
            .get_mut(&key)
            .expect("hit() called for a missing key");
        self.hits += 1;
        self.bytes_saved_by_sharing = self
            .bytes_saved_by_sharing
            .saturating_add(entry.packet.len() as u64);
        self.lru.remove(&entry.last_used);
        self.next_seq += 1;
        entry.last_used = self.next_seq;
        self.lru.insert(self.next_seq, key);
        Arc::clone(&entry.packet)
    }

    /// Return the shared packet for `key` if resident, refreshing its LRU
    /// position. `None` on a miss (does not touch `misses` — encode
    /// accounting belongs to [`get_or_encode`]).
    ///
    /// [`get_or_encode`]: Self::get_or_encode
    pub fn get(&mut self, key: ChunkPacketCacheKey) -> Option<Arc<[u8]>> {
        self.entries.contains_key(&key).then(|| self.hit(key))
    }

    /// Return the shared packet for `key`, encoding on a miss.
    ///
    /// On a hit this is an O(log n) LRU refresh plus an [`Arc`] refcount
    /// bump — no serialization, no compression, no heap copy. On a miss
    /// `encode_fn` runs exactly once, the bytes are wrapped in an
    /// `Arc<[u8]>`, and the entry is inserted; if the cache is at
    /// capacity the coldest entry is evicted first.
    ///
    /// With `max_entries == 0` the packet is still encoded and returned,
    /// just never retained.
    pub fn get_or_encode<F>(&mut self, key: ChunkPacketCacheKey, encode_fn: F) -> Arc<[u8]>
    where
        F: FnOnce() -> Vec<u8>,
    {
        if self.entries.contains_key(&key) {
            return self.hit(key);
        }
        self.misses += 1;
        let packet: Arc<[u8]> = Arc::from(encode_fn());
        if self.max_entries == 0 {
            return packet;
        }
        self.next_seq += 1;
        self.lru.insert(self.next_seq, key);
        self.entries.insert(
            key,
            CacheEntry {
                packet: Arc::clone(&packet),
                last_used: self.next_seq,
            },
        );
        while self.entries.len() > self.max_entries {
            let Some((_, victim)) = self.lru.pop_first() else {
                break;
            };
            self.entries.remove(&victim);
        }
        packet
    }

    /// Drop every cached packet for chunk `(dim, cx, cz)` regardless of
    /// epoch or anti-xray profile — used when the chunk unloads or is
    /// mutated. Returns how many entries were removed.
    pub fn invalidate_chunk(&mut self, dim: i32, cx: i32, cz: i32) -> usize {
        let stale: Vec<ChunkPacketCacheKey> = self
            .entries
            .keys()
            .filter(|k| k.dim == dim && k.cx == cx && k.cz == cz)
            .copied()
            .collect();
        let removed = stale.len();
        for key in stale {
            if let Some(entry) = self.entries.remove(&key) {
                self.lru.remove(&entry.last_used);
            }
        }
        removed
    }
}
