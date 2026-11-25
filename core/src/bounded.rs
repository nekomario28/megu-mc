// SPDX-License-Identifier: LGPL-3.0-or-later
#![forbid(unsafe_code)]
//! `megu_core::bounded` — compile-time bug prevention primitives.
//!
//! Three independent tools move whole classes of defect from "runtime
//! incident" to "does not compile":
//!
//! * [`BoundedMap`] — a hash map whose capacity is a `const` generic
//!   parameter. Insertion past the cap evicts the
//!   least-recently-written entry instead of growing, so unbounded global
//!   registries (the classic memory-leak bug class in long-lived servers
//!   and clients) are *structurally impossible*: there is no code path
//!   that exceeds `MAX` entries.
//! * [`LootSeedToken`] / [`SpawnerDrawToken`] / [`MustConsume`] —
//!   `#[must_use]` consumption tokens. A critical RNG draw or side-effect
//!   result wrapped in one of these cannot be silently dropped: under
//!   `-D warnings` the build fails, turning "forgot to stage the draw"
//!   from a silent parity bug into a compile error.
//! * [`DimBound`] — a value paired with the [`crate::coords::DimIndex`]
//!   it was computed in, so an Overworld position can never be
//!   interpreted as a Nether position (or any modded dimension's)
//!   without an explicit, checked unwrap.

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;

use crate::coords::{CoordBuildHasher, DimIndex};

/// A `K -> V` map that can never hold more than `MAX` entries.
///
/// Lookup is deterministic `O(1)` (the canonical zero-seed
/// [`CoordBuildHasher`] mixer — no per-process seed, no SipHash cost, no
/// hash-DoS surface), and insertion of a *new* key while the map is full
/// evicts the least-recently-written entry in `O(1)`, returning it to the
/// caller. Re-inserting an existing key refreshes its recency: eviction
/// order is therefore least-recently-written, a deterministic write-LRU
/// that does not depend on read patterns.
///
/// `MAX` is a compile-time constant, so "the registry grew without bound"
/// cannot be expressed: the only way past the cap is eviction, which is
/// observable (the evicted pair is returned) rather than silent.
#[derive(Debug)]
pub struct BoundedMap<K, V, const MAX: usize> {
    entries: HashMap<K, V, CoordBuildHasher>,
    /// Recency order: front is the least-recently-written key, back the
    /// most recent. Bounded by `MAX` like `entries`.
    recency: VecDeque<K>,
}

impl<K, V, const MAX: usize> BoundedMap<K, V, MAX>
where
    K: Eq + Hash + Clone,
{
    /// Empty map bounded at `MAX` entries.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: HashMap::with_hasher(CoordBuildHasher::default()),
            recency: VecDeque::new(),
        }
    }

    /// Compile-time capacity — always exactly `MAX`.
    #[inline]
    #[must_use]
    pub const fn capacity(&self) -> usize {
        MAX
    }

    /// Number of live entries (`<= MAX` by construction).
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` when no entries are stored.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// `true` when the next insertion of a *new* key must evict.
    #[inline]
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.entries.len() >= MAX
    }

    /// `true` when `key` is present.
    #[inline]
    #[must_use]
    pub fn contains_key(&self, key: &K) -> bool {
        self.entries.contains_key(key)
    }

    /// `O(1)` lookup.
    #[inline]
    #[must_use]
    pub fn get(&self, key: &K) -> Option<&V> {
        self.entries.get(key)
    }

    /// `O(1)` mutable lookup.
    #[inline]
    #[must_use]
    pub fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        self.entries.get_mut(key)
    }

    /// Insert `key -> value`.
    ///
    /// * Existing key: value is replaced and the key becomes most-recently
    ///   written; `None` is returned (nothing was evicted — `get` on the
    ///   old value is gone but the pair stays).
    /// * New key at capacity: the least-recently-written pair is evicted
    ///   and returned as `Some((K, V))`, so eviction is always observable.
    /// * `MAX == 0`: the inserted pair itself is returned immediately.
    ///
    /// In every branch `len() <= MAX` holds afterwards.
    pub fn insert(&mut self, key: K, value: V) -> Option<(K, V)> {
        if MAX == 0 {
            return Some((key, value));
        }
        if self.entries.contains_key(&key) {
            self.entries.insert(key.clone(), value);
            self.refresh_recency(&key);
            return None;
        }
        let evicted = if self.is_full() {
            let oldest = self
                .recency
                .pop_front()
                .expect("a full bounded map always has a recency entry");
            self.entries.remove(&oldest).map(|v| (oldest, v))
        } else {
            None
        };
        self.recency.push_back(key.clone());
        self.entries.insert(key, value);
        evicted
    }

    /// Remove `key`, returning its value when present. `O(MAX)` worst
    /// case for the recency scan — bounded by the same constant that
    /// bounds memory.
    pub fn remove(&mut self, key: &K) -> Option<V> {
        let value = self.entries.remove(key)?;
        if let Some(pos) = self.recency.iter().position(|k| k == key) {
            self.recency.remove(pos);
        }
        Some(value)
    }

    /// Keep only entries the predicate accepts; evicted recency entries
    /// are dropped in the same pass.
    pub fn retain(&mut self, mut f: impl FnMut(&K, &mut V) -> bool) {
        self.entries.retain(|k, v| f(k, v));
        let entries = &self.entries;
        self.recency.retain(|k| entries.contains_key(k));
    }

    /// Iterate every live `(key, value)` pair (unordered).
    pub fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        self.entries.iter()
    }

    /// The key that the next capacity-forced eviction would remove
    /// (least-recently-written), `None` when empty.
    #[inline]
    #[must_use]
    pub fn oldest_key(&self) -> Option<&K> {
        self.recency.front()
    }

    /// Drop all entries and recency bookkeeping.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.recency.clear();
    }

    fn refresh_recency(&mut self, key: &K) {
        if let Some(pos) = self.recency.iter().position(|k| k == key) {
            self.recency.remove(pos);
        }
        self.recency.push_back(key.clone());
    }
}

impl<K, V, const MAX: usize> Default for BoundedMap<K, V, MAX>
where
    K: Eq + Hash + Clone,
{
    fn default() -> Self {
        Self::new()
    }
}

/// A loot-table RNG draw that has been produced but not yet staged into a
/// container or packet payload.
///
/// `#[must_use]` on the type makes `let _ = draw(...)` — or simply
/// ignoring the return — a warning (an error under `-D warnings`), so a
/// seed draw can never be rolled and silently discarded: the only way out
/// is [`LootSeedToken::stage`], which consumes the token.
#[must_use = "loot seed draw must be staged into a container/payload"]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct LootSeedToken(i64);

impl LootSeedToken {
    /// Wrap a freshly drawn loot seed.
    #[inline]
    pub const fn from_rng_draw(draw: i64) -> Self {
        Self(draw)
    }

    /// Consume the token and yield the seed for staging.
    #[inline]
    pub fn stage(self) -> i64 {
        self.0
    }
}

/// A spawner RNG draw that has been produced but not yet staged into a
/// spawner payload.
#[must_use = "spawner draw must be staged into a spawner payload"]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct SpawnerDrawToken(u32);

impl SpawnerDrawToken {
    /// Wrap a freshly drawn spawner value.
    #[inline]
    pub const fn from_rng_draw(draw: u32) -> Self {
        Self(draw)
    }

    /// Consume the token and yield the value for staging.
    #[inline]
    pub fn stage(self) -> u32 {
        self.0
    }
}

/// Generic consumption token for any critical value that must not be
/// silently dropped: seeds, mutation receipts, capability grants, drain
/// results — anything a vanilla or modded subsystem wants to make
/// impossible to discard.
#[must_use = "token must be consumed"]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct MustConsume<T>(T);

impl<T> MustConsume<T> {
    /// Wrap `value`; the wrapper must be [`consume`](Self::consume)d.
    #[inline]
    pub const fn new(value: T) -> Self {
        Self(value)
    }

    /// Consume the token and yield the value.
    #[inline]
    pub fn consume(self) -> T {
        self.0
    }
}

/// A value that is only meaningful in one dimension.
///
/// World coordinates, block states, chunk columns and biome lookups are
/// dimension-relative; carrying the [`DimIndex`] in the type makes the
/// classic Overworld/Nether (or modded-dimension) mix-up a checked
/// operation instead of a silent corruption.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct DimBound<T> {
    /// Dimension this value belongs to.
    pub dim: DimIndex,
    /// The dimension-bound value.
    pub value: T,
}

impl<T> DimBound<T> {
    /// Bind `value` to `dim`.
    #[inline]
    pub const fn new(dim: DimIndex, value: T) -> Self {
        Self { dim, value }
    }

    /// Transform the value, preserving the dimension binding.
    #[inline]
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> DimBound<U> {
        DimBound {
            dim: self.dim,
            value: f(self.value),
        }
    }

    /// Yield the value only when `expected` matches the bound dimension —
    /// `None` on a cross-dimension use, which is exactly the bug this type
    /// exists to reject.
    #[inline]
    pub fn into_inner_checked(self, expected: DimIndex) -> Option<T> {
        (self.dim == expected).then_some(self.value)
    }

    /// Yield the value unconditionally. Prefer
    /// [`into_inner_checked`](Self::into_inner_checked) at dimension
    /// boundaries; this is for code that already proved the dimension.
    #[inline]
    pub fn into_inner(self) -> T {
        self.value
    }
}
