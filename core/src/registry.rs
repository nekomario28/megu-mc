// SPDX-License-Identifier: LGPL-3.0-or-later
#![forbid(unsafe_code)]
//! `megu_core::registry` — the universal namespaced registry spine that
//! vanilla *and* mods share.
//!
//! One registry shape enumerates everything the spine hands numeric
//! handles to: vanilla blocks, items and dimensions, menus/containers,
//! custom-payload channels, recipes, loot tables — and through the same
//! spine, the mod UI screens ([`crate::ui_contract`]) contributed by
//! Fabric, NeoForge, Create or WASM mods and datapacks.
//!
//! Three pieces keep the contract tight:
//!
//! * [`NamespacedId`] — the canonical `"namespace:path"` identity. The
//!   grammar *is* [`ResourceLocation`]'s (`minecraft` default namespace,
//!   `[a-z0-9_.-]` namespaces, `[a-z0-9_./-]` paths, fail-closed on any
//!   deviation), validated once through `id` and never duplicated here.
//! * [`RegistryId`] — a dense `u32` handle assigned in deterministic
//!   insertion order. Tick and render hot paths compare and index these
//!   integers; no `String` hashing survives registration time.
//! * [`UniversalRegistry<T>`] — the ordered table itself: duplicate
//!   rejection, `O(1)` lookup in both directions, a [`freeze`] barrier
//!   after world/mod load so no late registration can shift the dense
//!   ID space under a running server, and a 4-byte [`digest_crc32`]
//!   fingerprint that lets server ↔ client sync detect *any*
//!   vanilla-or-mod registry mismatch before a single block packet is
//!   exchanged.
//!
//! [`freeze`]: UniversalRegistry::freeze
//! [`digest_crc32`]: UniversalRegistry::digest_crc32

use std::error::Error;
use std::fmt;
use std::str::FromStr;

use crate::coords::CoordMap;
use crate::hash::crc32_ieee_update;
use crate::id::{InvalidResourceLocation, ResourceLocation};

/// Re-exported under the registry's own name: the validation grammar is
/// [`ResourceLocation`]'s, so the failure type is too.
pub type InvalidNamespacedId = InvalidResourceLocation;

/// Canonical `"namespace:path"` identity of a registry entry.
///
/// Validation delegates to [`ResourceLocation`], so every rule —
/// `minecraft` default namespace, the `[a-z0-9_.-]` / `[a-z0-9_./-]`
/// character sets, rejection of empty halves, stray `:` separators and
/// `..` traversal — is enforced in exactly one place.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NamespacedId {
    inner: ResourceLocation,
}

impl NamespacedId {
    /// Parse a `"namespace:path"` (or bare `"path"`, defaulting to the
    /// `minecraft` namespace) under the canonical resource-location
    /// grammar.
    ///
    /// # Errors
    ///
    /// Fails closed on every malformed input — see
    /// [`ResourceLocation::parse`] for the full rule set.
    pub fn parse(text: &str) -> Result<Self, InvalidNamespacedId> {
        ResourceLocation::parse(text).map(|inner| Self { inner })
    }

    /// Build from an explicit namespace and path under the same rules
    /// as [`parse`](Self::parse).
    ///
    /// # Errors
    ///
    /// Fails closed on every malformed half.
    pub fn new(namespace: &str, path: &str) -> Result<Self, InvalidNamespacedId> {
        ResourceLocation::new(namespace, path).map(|inner| Self { inner })
    }

    /// The `namespace` half, e.g. `"minecraft"` or `"create"`.
    #[inline]
    #[must_use]
    pub fn namespace(&self) -> &str {
        self.inner.namespace()
    }

    /// The `path` half, e.g. `"stone"` or `"item/mechanical_belt"`.
    #[inline]
    #[must_use]
    pub fn path(&self) -> &str {
        self.inner.path()
    }

    /// The combined `"namespace:path"` spelling.
    #[inline]
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.inner.as_str()
    }

    /// Borrow the validated [`ResourceLocation`] backing this id.
    #[inline]
    #[must_use]
    pub fn as_resource_location(&self) -> &ResourceLocation {
        &self.inner
    }

    /// Consume into the backing [`ResourceLocation`].
    #[inline]
    #[must_use]
    pub fn into_resource_location(self) -> ResourceLocation {
        self.inner
    }
}

impl fmt::Display for NamespacedId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for NamespacedId {
    type Err = InvalidNamespacedId;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl TryFrom<&str> for NamespacedId {
    type Error = InvalidNamespacedId;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<ResourceLocation> for NamespacedId {
    fn from(inner: ResourceLocation) -> Self {
        Self { inner }
    }
}

/// Dense, zero-copy numeric handle into a [`UniversalRegistry`].
///
/// Vanilla (`minecraft:*`) and modded (`create:*`, `ae2:*`, WASM mod,
/// datapack) entries draw from the same `u32` space assigned in
/// insertion order, so tick and render hot paths compare plain
/// integers — never `String` hashing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RegistryId(pub u32);

impl RegistryId {
    /// The dense array index (`0..registry.len()`) this handle
    /// addresses.
    #[inline]
    #[must_use]
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

impl fmt::Display for RegistryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// Why a [`UniversalRegistry::register`] call was rejected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegistryError {
    /// The registry is already [`frozen`](UniversalRegistry::freeze);
    /// `id` arrived too late.
    Frozen { id: NamespacedId },
    /// `id` already names an entry — the canonical spelling is stored
    /// once, so `"stone"` and `"minecraft:stone"` collide as intended.
    Duplicate { id: NamespacedId },
    /// The dense `u32` id space is exhausted
    /// (`u32::MAX` entries registered).
    Full,
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Frozen { id } => {
                write!(f, "registry frozen: cannot register {id}")
            }
            Self::Duplicate { id } => {
                write!(f, "duplicate registry entry: {id}")
            }
            Self::Full => f.write_str("registry full: u32 id space exhausted"),
        }
    }
}

impl Error for RegistryError {}

/// Ordered, dual-indexed registry: `Vec` for `O(1)` id → entry lookup
/// and a deterministic-hash map for `O(1)` name → id lookup.
///
/// The name index rides the same zero-seed hasher as
/// [`CoordMap`](crate::coords::CoordMap), so iteration order and
/// collision behaviour never depend on a per-process seed — two
/// processes loading identical entry sequences produce identical dense
/// id assignments and identical [`digest_crc32`](Self::digest_crc32)s.
#[derive(Clone, Debug)]
pub struct UniversalRegistry<T> {
    entries: Vec<(NamespacedId, T)>,
    by_name: CoordMap<String, u32>,
    frozen: bool,
}

impl<T> Default for UniversalRegistry<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> UniversalRegistry<T> {
    /// Empty, unfrozen registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            by_name: CoordMap::default(),
            frozen: false,
        }
    }

    /// Empty registry with `capacity` entry slots pre-reserved — for
    /// loaders that know the vanilla count up front and want zero
    /// reallocation mid-load.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: Vec::with_capacity(capacity),
            by_name: CoordMap::with_capacity_and_hasher(capacity, Default::default()),
            frozen: false,
        }
    }

    /// Insert `id` → `value`, returning the dense [`RegistryId`]
    /// assigned (`0`, `1`, `2`, … in registration order).
    ///
    /// Deterministic: the same sequence of registrations always yields
    /// the same id assignment, so replicas and replayed logs agree
    /// without exchanging the table.
    ///
    /// # Errors
    ///
    /// * [`RegistryError::Frozen`] after [`freeze`](Self::freeze),
    /// * [`RegistryError::Duplicate`] when `id`'s canonical spelling is
    ///   already registered,
    /// * [`RegistryError::Full`] when `u32::MAX` entries exist.
    pub fn register(&mut self, id: NamespacedId, value: T) -> Result<RegistryId, RegistryError> {
        if self.frozen {
            return Err(RegistryError::Frozen { id });
        }
        if self.by_name.contains_key(id.as_str()) {
            return Err(RegistryError::Duplicate { id });
        }
        let raw = u32::try_from(self.entries.len()).map_err(|_| RegistryError::Full)?;
        self.by_name.insert(id.as_str().to_owned(), raw);
        self.entries.push((id, value));
        Ok(RegistryId(raw))
    }

    /// Value for `id`, `None` for any handle out of range — including
    /// ids a *different* registry instance happened to hand out.
    #[inline]
    #[must_use]
    pub fn get(&self, id: RegistryId) -> Option<&T> {
        self.entries.get(id.index()).map(|(_, value)| value)
    }

    /// Mutable value for `id`. Freezing gates *registration*, not
    /// mutation of existing entries.
    #[inline]
    pub fn get_mut(&mut self, id: RegistryId) -> Option<&mut T> {
        self.entries.get_mut(id.index()).map(|(_, value)| value)
    }

    /// `(name, value)` pair for `id`.
    #[inline]
    #[must_use]
    pub fn entry(&self, id: RegistryId) -> Option<(&NamespacedId, &T)> {
        self.entries.get(id.index()).map(|(name, value)| (name, value))
    }

    /// `O(1)` name → id lookup.
    ///
    /// `text` may be the canonical `"namespace:path"` spelling or a bare
    /// `"path"`, which resolves through the `minecraft` default
    /// namespace exactly like [`NamespacedId::parse`]; malformed inputs
    /// and unknown names both return `None`.
    #[inline]
    #[must_use]
    pub fn id_of(&self, text: &str) -> Option<RegistryId> {
        if let Some(&raw) = self.by_name.get(text) {
            return Some(RegistryId(raw));
        }
        // Unqualified or non-canonical spellings: normalize through the
        // shared grammar, then look up the canonical key.
        let parsed = NamespacedId::parse(text).ok()?;
        self.by_name.get(parsed.as_str()).map(|&raw| RegistryId(raw))
    }

    /// Canonical name for `id`.
    #[inline]
    #[must_use]
    pub fn name_of(&self, id: RegistryId) -> Option<&NamespacedId> {
        self.entries.get(id.index()).map(|(name, _)| name)
    }

    /// Insertion-ordered `(RegistryId, name, value)` triples — the same
    /// order [`digest_crc32`](Self::digest_crc32) folds.
    pub fn iter(&self) -> impl Iterator<Item = (RegistryId, &NamespacedId, &T)> + '_ {
        self.entries
            .iter()
            .enumerate()
            .map(|(index, (name, value))| (RegistryId(index as u32), name, value))
    }

    /// Number of registered entries.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` when nothing is registered.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// `true` after [`freeze`](Self::freeze).
    #[inline]
    #[must_use]
    pub const fn is_frozen(&self) -> bool {
        self.frozen
    }

    /// Seal the registry: the dense id space becomes immutable so every
    /// handle a peer already exchanged stays valid. World/mod loaders
    /// freeze after the final registration phase; any later `register`
    /// — a stray datapack re-scan, a misbehaving mod — fails closed
    /// instead of silently shifting the id space under a running
    /// server.
    pub fn freeze(&mut self) {
        self.frozen = true;
    }

    /// Deterministic 32-bit fingerprint of the registry's *identity*
    /// contents: the insertion-ordered `(RegistryId, namespace, path)`
    /// triples, length-framed and folded through the canonical
    /// [`crc32_ieee_update`].
    ///
    /// Two registries carrying identical entries in identical order
    /// digest to the same value on any platform, so a server ↔ client
    /// sync check detects any vanilla/mod registry mismatch in 4 bytes.
    /// Values (`T`) are deliberately *not* fingerprinted — identity,
    /// not payload, is what must agree.
    #[must_use]
    pub fn digest_crc32(&self) -> u32 {
        let mut crc = 0u32;
        for (raw, (id, _)) in (0u32..).zip(self.entries.iter()) {
            crc = crc32_ieee_update(crc, &raw.to_le_bytes());
            crc = crc32_ieee_update(crc, &(id.namespace().len() as u32).to_le_bytes());
            crc = crc32_ieee_update(crc, id.namespace().as_bytes());
            crc = crc32_ieee_update(crc, &(id.path().len() as u32).to_le_bytes());
            crc = crc32_ieee_update(crc, id.path().as_bytes());
        }
        crc
    }
}
