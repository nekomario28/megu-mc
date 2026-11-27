// SPDX-License-Identifier: LGPL-3.0-or-later
#![forbid(unsafe_code)]
//! `megu_core::data_map` — the loader-agnostic `DataMap<V>` registry.
//!
//! Data-driven values attached to registry keys — how many ticks a fuel
//! burns, a compost chance, a stonecutter target, a crafting remainder —
//! arrive through three different mechanisms depending on where the data
//! came from: a platform data map shipped by a mod, a game registry entry,
//! or a JSON file in a datapack. All three describe the same thing:
//! `namespace:path -> V`, with a tag fallback `#namespace:tag -> V` and a
//! priority that says who wins.
//!
//! [`DataMap`] is that single shape, so the spine parses each loader's
//! spelling into one registry and every consumer asks the same question:
//!
//! ```text
//! resolve("minecraft:oak_planks", ["minecraft:planks"]) -> Option<&V>
//! ```
//!
//! # Resolution order
//!
//! A candidate set is built from every direct registration of the id plus
//! every registration of each tag the caller passed, and the candidate with
//! the smallest `(priority, registration_index)` wins:
//!
//! 1. **lower priority wins** — [`VANILLA_PRIORITY`] (`1000`) is where the
//!    baseline data registers, a mod or datapack overriding it registers at
//!    a *lower* number and takes precedence regardless of which map the
//!    value lives in. That is what makes a tag override able to beat a
//!    direct entry, and vice versa;
//! 2. **equal priorities break by registration index** — the earliest
//!    registration wins, so the outcome depends on load order only, never
//!    on hash order, insertion into a `Vec`, or thread scheduling.
//!
//! # Reload
//!
//! Every registration optionally carries a source id (the mod or datapack
//! that produced it). [`DataMap::remove_by_source`] drops exactly that
//! source's registrations and leaves every other one in place, so a
//! datapack reload or mod unload is a precise operation instead of "clear
//! everything and hope load order reproduces it".
//!
//! # Bounds
//!
//! The map is capped at a configurable number of live registrations and
//! fails closed when it is reached: nothing is evicted and no write is
//! silently dropped, so an over-budget pack is a visible error at load
//! time rather than a value that quietly disappears from the running
//! server.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use crate::id::{InvalidResourceLocation, ResourceLocation};

/// Priority baseline data registers at; see the [module docs](self).
///
/// Re-exported from [`crate::ext_gate`] so the dispatch order of hooks and
/// the precedence of data values are literally the same contract.
pub use crate::ext_gate::VANILLA_PRIORITY;

/// Default cap on live registrations in one [`DataMap`].
pub const DEFAULT_MAX_ENTRIES: usize = 65_536;

/// Why a registration was rejected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DataMapError {
    /// The `namespace:path` key is not a valid resource location.
    InvalidId(InvalidResourceLocation),
    /// The `#namespace:tag` tag is not a valid resource location.
    InvalidTag(InvalidResourceLocation),
    /// The map is at its registration cap.
    CapacityExceeded {
        /// The cap that refused the registration.
        max_entries: usize,
    },
}

impl fmt::Display for DataMapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidId(error) => write!(f, "invalid data-map id: {error}"),
            Self::InvalidTag(error) => write!(f, "invalid data-map tag: {error}"),
            Self::CapacityExceeded { max_entries } => write!(
                f,
                "data map is full: {max_entries} live registration(s) is the configured cap"
            ),
        }
    }
}

impl Error for DataMapError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidId(error) | Self::InvalidTag(error) => Some(error),
            Self::CapacityExceeded { .. } => None,
        }
    }
}

/// One live registration: the value plus everything resolution orders on.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Registration<V> {
    value: V,
    priority: i32,
    reg_index: u64,
    /// `None` for baseline data that has no owning pack.
    source: Option<String>,
}

/// Total order on registrations: `(priority, reg_index)`.
fn order_key<V>(a: &Registration<V>, b: &Registration<V>) -> Ordering {
    (a.priority, a.reg_index).cmp(&(b.priority, b.reg_index))
}

/// The winning registration of one key's candidate list.
fn winner<V>(registrations: &[Registration<V>]) -> Option<&Registration<V>> {
    registrations.iter().min_by(|a, b| order_key(a, b))
}

/// Parse a tag spelling with or without its leading `#`.
fn parse_tag(text: &str) -> Result<ResourceLocation, InvalidResourceLocation> {
    ResourceLocation::parse(text.strip_prefix('#').unwrap_or(text))
}

/// Drop every registration whose source is `source_id`, returning how many
/// went away. Keys left with no registration lose their entry entirely, so
/// a removed source cannot keep a dead key alive.
fn remove_source_from<V>(
    map: &mut BTreeMap<ResourceLocation, Vec<Registration<V>>>,
    source_id: &str,
) -> usize {
    let mut removed = 0;
    map.retain(|_, registrations| {
        let before = registrations.len();
        registrations.retain(|registration| registration.source.as_deref() != Some(source_id));
        removed += before - registrations.len();
        !registrations.is_empty()
    });
    removed
}

/// A `namespace:path -> V` table with tag fallback and deterministic
/// precedence; see the [module docs](self).
#[derive(Clone, Debug)]
pub struct DataMap<V> {
    entries: BTreeMap<ResourceLocation, Vec<Registration<V>>>,
    tags: BTreeMap<ResourceLocation, Vec<Registration<V>>>,
    registrations: usize,
    next_index: u64,
    max_entries: usize,
}

impl<V> DataMap<V> {
    /// An empty map capped at [`DEFAULT_MAX_ENTRIES`].
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_MAX_ENTRIES)
    }

    /// An empty map capped at `max_entries` live registrations.
    #[must_use]
    pub fn with_capacity(max_entries: usize) -> Self {
        Self {
            entries: BTreeMap::new(),
            tags: BTreeMap::new(),
            registrations: 0,
            next_index: 0,
            max_entries,
        }
    }

    /// The registration cap.
    #[inline]
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.max_entries
    }

    /// Live registrations across entries and tags.
    #[inline]
    #[must_use]
    pub fn registration_count(&self) -> usize {
        self.registrations
    }

    /// Distinct direct keys with at least one live registration.
    #[inline]
    #[must_use]
    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    /// Distinct tags with at least one live registration.
    #[inline]
    #[must_use]
    pub fn tag_count(&self) -> usize {
        self.tags.len()
    }

    /// `true` when nothing is registered.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.registrations == 0
    }

    /// Drop every registration and restart the registration counter.
    ///
    /// The counter reset is what makes a full reload reproduce the previous
    /// order deterministically instead of continuing an old sequence.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.tags.clear();
        self.registrations = 0;
        self.next_index = 0;
    }

    /// Register `value` for the direct key `id` at `priority`.
    ///
    /// `id` may be bare (`"stone"`), in which case it takes the default
    /// namespace, exactly like every other resource-location field.
    ///
    /// # Errors
    ///
    /// Rejects a malformed id ([`DataMapError::InvalidId`]) and fails
    /// closed at the registration cap ([`DataMapError::CapacityExceeded`]).
    pub fn register_entry(
        &mut self,
        id: &str,
        value: V,
        priority: i32,
    ) -> Result<u64, DataMapError> {
        self.insert_entry(None, id, value, priority)
    }

    /// Register `value` for `id` at `priority`, tagged with `source` so
    /// [`remove_by_source`](Self::remove_by_source) can undo exactly this
    /// write on reload.
    ///
    /// # Errors
    ///
    /// Same rejections as [`register_entry`](Self::register_entry).
    pub fn register_entry_from(
        &mut self,
        source: &str,
        id: &str,
        value: V,
        priority: i32,
    ) -> Result<u64, DataMapError> {
        self.insert_entry(Some(source), id, value, priority)
    }

    /// Register `value` for the tag `tag` at `priority`. The tag may be
    /// spelled `"#namespace:tag"` or `"namespace:tag"`.
    ///
    /// # Errors
    ///
    /// Rejects a malformed tag ([`DataMapError::InvalidTag`]) and fails
    /// closed at the registration cap.
    pub fn register_tag(
        &mut self,
        tag: &str,
        value: V,
        priority: i32,
    ) -> Result<u64, DataMapError> {
        self.insert_tag(None, tag, value, priority)
    }

    /// Register `value` for `tag` at `priority`, tagged with `source`.
    ///
    /// # Errors
    ///
    /// Same rejections as [`register_tag`](Self::register_tag).
    pub fn register_tag_from(
        &mut self,
        source: &str,
        tag: &str,
        value: V,
        priority: i32,
    ) -> Result<u64, DataMapError> {
        self.insert_tag(Some(source), tag, value, priority)
    }

    fn insert_entry(
        &mut self,
        source: Option<&str>,
        id: &str,
        value: V,
        priority: i32,
    ) -> Result<u64, DataMapError> {
        let key = ResourceLocation::parse(id).map_err(DataMapError::InvalidId)?;
        self.insert(source, key, value, priority, true)
    }

    fn insert_tag(
        &mut self,
        source: Option<&str>,
        tag: &str,
        value: V,
        priority: i32,
    ) -> Result<u64, DataMapError> {
        let key = parse_tag(tag).map_err(DataMapError::InvalidTag)?;
        self.insert(source, key, value, priority, false)
    }

    fn insert(
        &mut self,
        source: Option<&str>,
        key: ResourceLocation,
        value: V,
        priority: i32,
        direct: bool,
    ) -> Result<u64, DataMapError> {
        if self.registrations >= self.max_entries {
            return Err(DataMapError::CapacityExceeded {
                max_entries: self.max_entries,
            });
        }
        let reg_index = self.next_index;
        self.next_index = self.next_index.wrapping_add(1);
        let registration = Registration {
            value,
            priority,
            reg_index,
            source: source.map(str::to_owned),
        };
        let bucket = if direct {
            self.entries.entry(key)
        } else {
            self.tags.entry(key)
        };
        bucket.or_default().push(registration);
        self.registrations += 1;
        Ok(reg_index)
    }

    /// Resolve `id` with its tags, returning the winning value.
    ///
    /// `item_tags` may spell each tag with or without a leading `#`; an
    /// unspellable tag is skipped rather than failing the whole lookup,
    /// because a real item list can carry tags from a pack that failed to
    /// load. An unspellable `id` yields `None` (fail closed).
    #[must_use]
    pub fn resolve(&self, id: &str, item_tags: &[&str]) -> Option<&V> {
        let key = ResourceLocation::parse(id).ok()?;
        self.resolve_key(&key, item_tags)
    }

    /// [`resolve`](Self::resolve) for a caller that already holds the
    /// parsed key, so a hot path can skip re-parsing.
    #[must_use]
    pub fn resolve_key(&self, id: &ResourceLocation, item_tags: &[&str]) -> Option<&V> {
        let mut best = self.entries.get(id).and_then(|regs| winner(regs));
        for tag in item_tags {
            let Ok(tag_key) = parse_tag(tag) else {
                continue;
            };
            let Some(candidate) = self.tags.get(&tag_key).and_then(|regs| winner(regs)) else {
                continue;
            };
            let better = match best {
                None => true,
                Some(current) => order_key(candidate, current) == Ordering::Less,
            };
            if better {
                best = Some(candidate);
            }
        }
        best.map(|registration| &registration.value)
    }

    /// Every direct key with its winning value, sorted by
    /// [`ResourceLocation`]'s path-first order.
    pub fn iter(&self) -> impl Iterator<Item = (&ResourceLocation, &V)> {
        self.entries
            .iter()
            .filter_map(|(key, regs)| winner(regs).map(|registration| (key, &registration.value)))
    }

    /// Every tag with its winning value, sorted by path-first order.
    pub fn tag_iter(&self) -> impl Iterator<Item = (&ResourceLocation, &V)> {
        self.tags
            .iter()
            .filter_map(|(key, regs)| winner(regs).map(|registration| (key, &registration.value)))
    }

    /// Every registration of `id` in precedence order (first wins).
    ///
    /// This is what a conflict report needs: every source that claimed the
    /// key, with the winner first.
    #[must_use]
    pub fn candidates(&self, id: &str) -> Vec<(i32, u64, &V)> {
        let Ok(key) = ResourceLocation::parse(id) else {
            return Vec::new();
        };
        let mut candidates: Vec<&Registration<V>> = self
            .entries
            .get(&key)
            .map(|regs| regs.iter().collect())
            .unwrap_or_default();
        candidates.sort_by(|a, b| order_key(a, b));
        candidates
            .into_iter()
            .map(|registration| {
                (
                    registration.priority,
                    registration.reg_index,
                    &registration.value,
                )
            })
            .collect()
    }

    /// The source that produced the winning registration for `id`, when it
    /// was registered from one.
    #[must_use]
    pub fn source_of(&self, id: &str) -> Option<&str> {
        let key = ResourceLocation::parse(id).ok()?;
        let winner = winner(self.entries.get(&key)?)?;
        winner.source.as_deref()
    }

    /// Drop every registration whose source is `source_id`.
    ///
    /// Returns how many registrations were removed. This is the reload
    /// primitive: a datapack or mod unloading removes only its own writes,
    /// and keys it owned fall back to whatever remains — or disappear.
    pub fn remove_by_source(&mut self, source_id: &str) -> usize {
        let removed = remove_source_from(&mut self.entries, source_id)
            + remove_source_from(&mut self.tags, source_id);
        self.registrations -= removed;
        removed
    }
}

impl<V> Default for DataMap<V> {
    fn default() -> Self {
        Self::new()
    }
}

/// A fuel's burn duration in ticks.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct FuelEntry {
    /// Ticks this fuel keeps a burner lit.
    pub burn_ticks: u32,
}

impl FuelEntry {
    /// Wrap a burn duration.
    #[inline]
    #[must_use]
    pub const fn burn_ticks(burn_ticks: u32) -> Self {
        Self { burn_ticks }
    }

    /// Seconds of burn at the canonical 20 ticks per second.
    #[inline]
    #[must_use]
    pub const fn burn_seconds(&self) -> u32 {
        self.burn_ticks / 20
    }
}

/// A compostable item's chance to raise the compost level.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CompostEntry {
    /// Chance in `0.0..=1.0` that one insertion advances the level.
    pub chance: f32,
}

impl CompostEntry {
    /// Wrap a chance verbatim; use [`clamped`](Self::clamped) to bound it.
    #[inline]
    #[must_use]
    pub const fn new(chance: f32) -> Self {
        Self { chance }
    }

    /// Wrap a chance clamped into `0.0..=1.0`, so a malformed pack value
    /// cannot turn into a guaranteed or impossible compost.
    #[inline]
    #[must_use]
    pub fn clamped(chance: f32) -> Self {
        Self {
            chance: chance.clamp(0.0, 1.0),
        }
    }

    /// `true` when the stored chance is inside `0.0..=1.0`.
    #[inline]
    #[must_use]
    pub fn is_valid(&self) -> bool {
        (0.0..=1.0).contains(&self.chance)
    }
}

/// A one-way transformation target (the `target` half of a pair map).
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct TransformPair {
    /// `namespace:path` of the produced key.
    pub target: String,
}

impl TransformPair {
    /// Wrap a target key.
    #[must_use]
    pub fn new(target: impl Into<String>) -> Self {
        Self {
            target: target.into(),
        }
    }
}

/// The item a crafting operation leaves behind in the grid.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct CraftRemainderEntry {
    /// `namespace:path` of the remainder item.
    pub remainder: String,
}

impl CraftRemainderEntry {
    /// Wrap a remainder key.
    #[must_use]
    pub fn new(remainder: impl Into<String>) -> Self {
        Self {
            remainder: remainder.into(),
        }
    }
}

/// Burn durations keyed by fuel item.
pub type FuelMap = DataMap<FuelEntry>;
/// Compost chances keyed by item or tag.
pub type CompostMap = DataMap<CompostEntry>;
/// Transformation targets keyed by input item or tag.
pub type TransformMap = DataMap<TransformPair>;
/// Crafting remainders keyed by recipe output or tag.
pub type CraftRemainderMap = DataMap<CraftRemainderEntry>;

#[cfg(test)]
mod tests {
    use super::*;

    fn map() -> DataMap<u32> {
        DataMap::new()
    }

    #[test]
    fn a_new_map_is_empty_and_uses_the_default_cap() {
        let map: DataMap<u32> = DataMap::new();
        assert!(map.is_empty());
        assert_eq!(map.registration_count(), 0);
        assert_eq!(map.entry_count(), 0);
        assert_eq!(map.tag_count(), 0);
        assert_eq!(map.capacity(), DEFAULT_MAX_ENTRIES);
        assert_eq!(map.resolve("stone", &[]), None);
        assert_eq!(DataMap::<u32>::default().capacity(), DEFAULT_MAX_ENTRIES);
    }

    #[test]
    fn a_direct_entry_resolves_by_id() {
        let mut map = map();
        map.register_entry("minecraft:oak_planks", 300, VANILLA_PRIORITY)
            .unwrap();
        // An unqualified lookup takes the default namespace, so both
        // spellings reach the same value.
        assert_eq!(map.resolve("minecraft:oak_planks", &[]), Some(&300));
        assert_eq!(map.resolve("oak_planks", &[]), Some(&300));
        assert_eq!(map.resolve("minecraft:birch_planks", &[]), None);
        assert_eq!(map.entry_count(), 1);
    }

    #[test]
    fn a_tag_fills_in_when_no_direct_entry_exists() {
        let mut map = map();
        map.register_tag("#minecraft:planks", 200, VANILLA_PRIORITY)
            .unwrap();
        assert_eq!(
            map.resolve("minecraft:oak_planks", &["minecraft:planks"]),
            Some(&200)
        );
        // The tag is also reachable under its `#` spelling, and a missing
        // tag list simply misses.
        assert_eq!(
            map.resolve("minecraft:oak_planks", &["#minecraft:planks"]),
            Some(&200)
        );
        assert_eq!(map.resolve("minecraft:oak_planks", &[]), None);
        assert_eq!(map.tag_count(), 1);
    }

    #[test]
    fn a_direct_entry_beats_a_tag_at_the_same_priority() {
        let mut map = map();
        map.register_entry("minecraft:stick", 1, VANILLA_PRIORITY)
            .unwrap();
        map.register_tag("#minecraft:planks", 2, VANILLA_PRIORITY)
            .unwrap();
        assert_eq!(
            map.resolve("minecraft:stick", &["minecraft:planks"]),
            Some(&1)
        );
    }

    #[test]
    fn an_explicit_higher_priority_tag_override_beats_the_direct_entry() {
        let mut map = map();
        map.register_entry("minecraft:stick", 1, VANILLA_PRIORITY)
            .unwrap();
        // A pack registering the tag at a *lower* priority number wins,
        // even though the direct entry is the more specific match.
        map.register_tag("#minecraft:planks", 2, VANILLA_PRIORITY - 1)
            .unwrap();
        assert_eq!(
            map.resolve("minecraft:stick", &["minecraft:planks"]),
            Some(&2)
        );
    }

    #[test]
    fn a_mod_override_before_vanilla_wins_and_the_vanilla_value_stays() {
        let mut map = map();
        map.register_entry("minecraft:lava_bucket", 20_000, VANILLA_PRIORITY)
            .unwrap();
        map.register_entry("minecraft:lava_bucket", 40_000, VANILLA_PRIORITY - 100)
            .unwrap();
        assert_eq!(map.resolve("minecraft:lava_bucket", &[]), Some(&40_000));
        // Both registrations are still there: the override is precedence,
        // not replacement, so removing the mod restores the baseline.
        let candidates = map.candidates("minecraft:lava_bucket");
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].0, VANILLA_PRIORITY - 100);
        assert_eq!(candidates[1].0, VANILLA_PRIORITY);
        assert_eq!(map.registration_count(), 2);
    }

    #[test]
    fn equal_priorities_break_by_registration_index() {
        let mut map = map();
        map.register_entry("minecraft:dirt", 7, VANILLA_PRIORITY)
            .unwrap();
        map.register_entry("minecraft:dirt", 9, VANILLA_PRIORITY)
            .unwrap();
        // First registration wins the tie, so load order is the only
        // variable and repeated loads reproduce the same answer.
        assert_eq!(map.resolve("minecraft:dirt", &[]), Some(&7));
        let candidates = map.candidates("minecraft:dirt");
        assert_eq!(candidates[0].1, 0);
        assert_eq!(candidates[1].1, 1);
    }

    #[test]
    fn a_lower_priority_mod_entry_replaces_the_vanilla_answer() {
        let mut map = map();
        map.register_entry_from("datapack:vanilla", "minecraft:coal", 1600, VANILLA_PRIORITY)
            .unwrap();
        map.register_entry_from(
            "mod:fast_fuel",
            "minecraft:coal",
            3200,
            VANILLA_PRIORITY - 1,
        )
        .unwrap();
        assert_eq!(map.resolve("minecraft:coal", &[]), Some(&3200));
        assert_eq!(map.source_of("minecraft:coal"), Some("mod:fast_fuel"));
    }

    #[test]
    fn removing_a_source_restores_the_previous_answer() {
        let mut map = map();
        map.register_entry_from("datapack:vanilla", "minecraft:coal", 1600, VANILLA_PRIORITY)
            .unwrap();
        map.register_tag_from(
            "datapack:vanilla",
            "#minecraft:coals",
            1600,
            VANILLA_PRIORITY,
        )
        .unwrap();
        map.register_entry_from(
            "mod:fast_fuel",
            "minecraft:coal",
            3200,
            VANILLA_PRIORITY - 1,
        )
        .unwrap();
        map.register_tag_from(
            "mod:fast_fuel",
            "#minecraft:coals",
            3200,
            VANILLA_PRIORITY - 1,
        )
        .unwrap();
        assert_eq!(map.resolve("minecraft:coal", &[]), Some(&3200));

        // Unload the mod: its registrations go, the baseline stays, so the
        // answer falls back without a full reload.
        assert_eq!(map.remove_by_source("mod:fast_fuel"), 2);
        assert_eq!(map.resolve("minecraft:coal", &[]), Some(&1600));
        assert_eq!(
            map.resolve("minecraft:charcoal", &["minecraft:coals"]),
            Some(&1600)
        );
        assert_eq!(map.registration_count(), 2);
        assert_eq!(map.remove_by_source("mod:fast_fuel"), 0);
        // A source that owned a key outright takes the key with it.
        assert_eq!(map.remove_by_source("datapack:vanilla"), 2);
        assert!(map.is_empty());
        assert_eq!(map.entry_count(), 0);
        assert_eq!(map.resolve("minecraft:coal", &[]), None);
    }

    #[test]
    fn malformed_keys_fail_closed() {
        let mut map = map();
        assert!(matches!(
            map.register_entry("Bad:Id", 1, VANILLA_PRIORITY),
            Err(DataMapError::InvalidId(_))
        ));
        assert!(matches!(
            map.register_entry("ns:..", 1, VANILLA_PRIORITY),
            Err(DataMapError::InvalidId(_))
        ));
        assert!(matches!(
            map.register_tag("#", 1, VANILLA_PRIORITY),
            Err(DataMapError::InvalidTag(_))
        ));
        assert!(matches!(
            map.register_tag("#a:b:c", 1, VANILLA_PRIORITY),
            Err(DataMapError::InvalidTag(_))
        ));
        assert!(map.is_empty(), "a rejected registration writes nothing");
        // Lookups never panic on a hostile id; they simply miss.
        assert_eq!(map.resolve("BAD:ID", &["#"]), None);
        assert!(map.candidates("BAD:ID").is_empty());
        assert_eq!(map.source_of("BAD:ID"), None);
        // The errors describe themselves and keep their cause.
        let error = DataMapError::InvalidId(ResourceLocation::parse("BAD:ID").unwrap_err());
        assert!(error.to_string().contains("invalid data-map id"));
        assert!(error.source().is_some());
        let full = DataMapError::CapacityExceeded { max_entries: 4 };
        assert!(full.to_string().contains('4'));
        assert!(full.source().is_none());
    }

    #[test]
    fn the_cap_fails_closed_without_evicting_anything() {
        let mut map: DataMap<u32> = DataMap::with_capacity(2);
        map.register_entry("minecraft:a", 1, VANILLA_PRIORITY)
            .unwrap();
        map.register_tag("#minecraft:b", 2, VANILLA_PRIORITY)
            .unwrap();
        assert_eq!(map.registration_count(), 2);
        assert!(matches!(
            map.register_entry("minecraft:c", 3, VANILLA_PRIORITY),
            Err(DataMapError::CapacityExceeded { max_entries: 2 })
        ));
        assert!(matches!(
            map.register_tag("#minecraft:d", 4, VANILLA_PRIORITY),
            Err(DataMapError::CapacityExceeded { .. })
        ));
        // Nothing was evicted or overwritten by the refused writes.
        assert_eq!(map.resolve("minecraft:a", &[]), Some(&1));
        assert_eq!(map.resolve("x", &["minecraft:b"]), Some(&2));
        assert_eq!(map.resolve("minecraft:c", &[]), None);
        assert_eq!(map.registration_count(), 2);
    }

    #[test]
    fn a_zero_capacity_map_refuses_every_registration() {
        let mut map: DataMap<u32> = DataMap::with_capacity(0);
        assert!(matches!(
            map.register_entry("minecraft:a", 1, VANILLA_PRIORITY),
            Err(DataMapError::CapacityExceeded { max_entries: 0 })
        ));
        assert!(map.is_empty());
    }

    #[test]
    fn iteration_is_sorted_and_clear_restarts_the_sequence() {
        let mut map = map();
        for (id, value) in [
            ("minecraft:zinc", 3u32),
            ("minecraft:apple", 1),
            ("mod:brass", 2),
        ] {
            map.register_entry(id, value, VANILLA_PRIORITY).unwrap();
        }
        map.register_tag("#minecraft:ores", 9, VANILLA_PRIORITY)
            .unwrap();
        let keys: Vec<String> = map.iter().map(|(key, _)| key.to_string()).collect();
        // Resource-location order is path-first, then namespace.
        assert_eq!(keys, ["minecraft:apple", "mod:brass", "minecraft:zinc"]);
        assert_eq!(map.tag_iter().count(), 1);
        assert_eq!(map.registration_count(), 4);

        map.clear();
        assert!(map.is_empty());
        assert_eq!(map.entry_count(), 0);
        assert_eq!(map.tag_count(), 0);
        // The registration counter restarts, so a reload reproduces the
        // same tie-break instead of continuing an old epoch.
        assert_eq!(
            map.register_entry("minecraft:a", 1, VANILLA_PRIORITY),
            Ok(0)
        );
    }

    #[test]
    fn the_typed_payloads_cover_the_common_data_maps() {
        let mut fuel: FuelMap = DataMap::new();
        fuel.register_entry(
            "minecraft:coal",
            FuelEntry::burn_ticks(1600),
            VANILLA_PRIORITY,
        )
        .unwrap();
        fuel.register_tag(
            "#minecraft:coals",
            FuelEntry::burn_ticks(1600),
            VANILLA_PRIORITY,
        )
        .unwrap();
        let entry = fuel
            .resolve("minecraft:charcoal", &["minecraft:coals"])
            .unwrap();
        assert_eq!(entry.burn_ticks, 1600);
        assert_eq!(entry.burn_seconds(), 80);

        let mut compost: CompostMap = DataMap::new();
        compost
            .register_entry(
                "minecraft:wheat_seeds",
                CompostEntry::new(0.3),
                VANILLA_PRIORITY,
            )
            .unwrap();
        let entry = compost.resolve("minecraft:wheat_seeds", &[]).unwrap();
        assert!((entry.chance - 0.3).abs() < f32::EPSILON);
        assert!(entry.is_valid());
        // Out-of-range pack data is clamped into the meaningful window
        // instead of turning into a guaranteed or impossible roll.
        assert_eq!(CompostEntry::clamped(2.0).chance, 1.0);
        assert_eq!(CompostEntry::clamped(-1.0).chance, 0.0);
        assert!(!CompostEntry::new(1.5).is_valid());

        let mut transform: TransformMap = DataMap::new();
        transform
            .register_entry(
                "minecraft:stone",
                TransformPair::new("minecraft:stone_bricks"),
                VANILLA_PRIORITY,
            )
            .unwrap();
        assert_eq!(
            transform.resolve("minecraft:stone", &[]).unwrap().target,
            "minecraft:stone_bricks"
        );

        let mut remainder: CraftRemainderMap = DataMap::new();
        remainder
            .register_entry(
                "minecraft:cake",
                CraftRemainderEntry::new("minecraft:bucket"),
                VANILLA_PRIORITY,
            )
            .unwrap();
        assert_eq!(
            remainder.resolve("minecraft:cake", &[]).unwrap().remainder,
            "minecraft:bucket"
        );
    }

    #[test]
    fn unspellable_tags_are_skipped_instead_of_failing_the_lookup() {
        let mut map = map();
        map.register_entry("minecraft:a", 1, VANILLA_PRIORITY)
            .unwrap();
        map.register_tag("#minecraft:b", 2, VANILLA_PRIORITY)
            .unwrap();
        // A pack that failed to load can leave a broken tag in an item's
        // tag list; the direct entry must still resolve.
        assert_eq!(
            map.resolve("minecraft:a", &["BAD:TAG", "#a:b:c", "minecraft:b"]),
            Some(&1)
        );
        assert_eq!(
            map.resolve("minecraft:z", &["BAD:TAG", "minecraft:b"]),
            Some(&2)
        );
    }
}
