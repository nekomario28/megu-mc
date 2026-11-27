// SPDX-License-Identifier: LGPL-3.0-or-later
#![forbid(unsafe_code)]
//! `megu_core::nbt_sidecar` — lossless unknown-NBT preservation and the
//! loud unresolved-extension ledger.
//!
//! Two silent-loss bugs share one root cause: an extension point that
//! declines a value nobody understands and forgets it happened.
//!
//! # 1. Unknown NBT must survive a load/save cycle
//!
//! A world written by a modded server carries keys the vanilla parser has
//! no field for: extra entity attributes, per-block-entity machine state,
//! chunk attachment blobs, `level.dat` settings, per-player data, and
//! `data/*.dat` saved data. Decoding into a typed struct and re-encoding
//! drops every key the struct does not name, so loading a world with a mod
//! and saving it without that mod silently destroys the mod's data — the
//! classic reason a datapack world cannot be reopened.
//!
//! [`UnknownNbtSidecar`] keeps those keys *verbatim*: the raw encoded
//! payload is stored under its original key and written back unchanged on
//! save, so the bytes make the round trip even though nothing in megu
//! interprets them. It is bounded in both key count
//! ([`MAX_SIDECAR_KEYS`]) and total bytes ([`MAX_SIDECAR_BYTES`]) and fails
//! closed past either cap, because a sidecar that grows without limit is a
//! memory-exhaustion surface that any mod (or a corrupted save) can drive.
//!
//! # 2. A declined extension must be visible
//!
//! The other silent loss is behavioural: a dispatcher that reaches a
//! `_ => continue`, a lookup that returns `false` for an unknown id, a
//! packet whose unknown field is skipped. Each one is defensible in
//! isolation and invisible in aggregate, so a pack "mostly works" with no
//! way to ask what was dropped.
//!
//! [`UnresolvedLedger`] is the recorder: `(domain, id)` pairs with a
//! count, deduplicated, bounded, and rendered as a deterministic summary a
//! boot log or an operator command can print. The overflow row keeps the
//! counts honest when the site table is full — a dropped diagnostic is
//! still counted, just aggregated.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

/// Default cap on distinct unknown keys preserved for one holder.
pub const MAX_SIDECAR_KEYS: usize = 1024;

/// Default cap on total preserved payload bytes for one holder (4 MiB).
pub const MAX_SIDECAR_BYTES: usize = 4 * 1024 * 1024;

/// Cap on the byte length of one preserved key.
pub const MAX_SIDECAR_KEY_BYTES: usize = 256;

/// Default cap on distinct `(domain, id)` rows in one
/// [`UnresolvedLedger`], excluding the reserved overflow row.
pub const MAX_LEDGER_ENTRIES: usize = 256;

/// Cap on the character count of one ledger `domain`/`id` field.
pub const MAX_LEDGER_FIELD_CHARS: usize = 96;

/// Domain of the reserved overflow row.
pub const OVERFLOW_DOMAIN: &str = "overflow";

/// Id of the reserved overflow row.
pub const OVERFLOW_ID: &str = "other";

/// Which persisted object a [`UnknownNbtSidecar`] belongs to.
///
/// The domain never changes the bytes; it exists so a diagnostic can say
/// *where* an unknown key was found and so per-holder budgets stay
/// comparable.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum SidecarDomain {
    /// An entity's `data` compound.
    Entity,
    /// A block entity's `data` compound.
    BlockEntity,
    /// A chunk column's serialised sections and attachments.
    Chunk,
    /// The world's `level.dat`.
    LevelData,
    /// One player's `playerdata/*.dat`.
    PlayerData,
    /// A `data/*.dat` saved-data file.
    SavedData,
}

impl SidecarDomain {
    /// Every domain, in declaration order.
    pub const ALL: [Self; 6] = [
        Self::Entity,
        Self::BlockEntity,
        Self::Chunk,
        Self::LevelData,
        Self::PlayerData,
        Self::SavedData,
    ];

    /// Stable lowercase name for logs.
    #[inline]
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Entity => "entity",
            Self::BlockEntity => "block_entity",
            Self::Chunk => "chunk",
            Self::LevelData => "level_data",
            Self::PlayerData => "player_data",
            Self::SavedData => "saved_data",
        }
    }

    /// An empty sidecar for this domain at the module default caps.
    #[inline]
    #[must_use]
    pub fn sidecar(self) -> UnknownNbtSidecar {
        UnknownNbtSidecar::new(self)
    }
}

impl fmt::Display for SidecarDomain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a key could not be preserved.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SidecarError {
    /// The key is empty, which no NBT compound can spell.
    EmptyKey,
    /// The key is longer than [`MAX_SIDECAR_KEY_BYTES`].
    KeyTooLong {
        /// The configured cap.
        max_bytes: usize,
    },
    /// The sidecar already holds its maximum number of distinct keys.
    TooManyKeys {
        /// The configured cap.
        max_keys: usize,
    },
    /// Preserving the payload would exceed the byte budget.
    TooManyBytes {
        /// The configured cap.
        max_bytes: usize,
        /// The total the write would have reached.
        requested: usize,
    },
}

impl fmt::Display for SidecarError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyKey => f.write_str("sidecar key must not be empty"),
            Self::KeyTooLong { max_bytes } => {
                write!(f, "sidecar key exceeds {max_bytes} byte(s)")
            }
            Self::TooManyKeys { max_keys } => {
                write!(f, "sidecar holds its maximum of {max_keys} key(s)")
            }
            Self::TooManyBytes {
                max_bytes,
                requested,
            } => write!(
                f,
                "sidecar byte budget is {max_bytes}; this write would reach {requested}"
            ),
        }
    }
}

impl Error for SidecarError {}

/// Verbatim storage for NBT keys nothing in megu interprets.
///
/// Keys are ordered (`BTreeMap`), so the save path re-emits them in a
/// stable order instead of the arbitrary order a hash map would produce —
/// which keeps a chunk's saved bytes comparable across runs.
#[derive(Clone, Debug)]
pub struct UnknownNbtSidecar {
    domain: SidecarDomain,
    entries: BTreeMap<String, Vec<u8>>,
    bytes: usize,
    max_keys: usize,
    max_bytes: usize,
}

impl UnknownNbtSidecar {
    /// An empty sidecar for `domain`, capped at the module defaults.
    #[must_use]
    pub fn new(domain: SidecarDomain) -> Self {
        Self::with_limits(domain, MAX_SIDECAR_KEYS, MAX_SIDECAR_BYTES)
    }

    /// An empty sidecar with explicit caps.
    ///
    /// Different holders need different budgets — a chunk column is far
    /// larger than one player's data — so the caps are parameters rather
    /// than only constants.
    #[must_use]
    pub fn with_limits(domain: SidecarDomain, max_keys: usize, max_bytes: usize) -> Self {
        Self {
            domain,
            entries: BTreeMap::new(),
            bytes: 0,
            max_keys,
            max_bytes,
        }
    }

    /// The holder domain this sidecar belongs to.
    #[inline]
    #[must_use]
    pub fn domain(&self) -> SidecarDomain {
        self.domain
    }

    /// The key cap.
    #[inline]
    #[must_use]
    pub fn max_keys(&self) -> usize {
        self.max_keys
    }

    /// The byte cap.
    #[inline]
    #[must_use]
    pub fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    /// Distinct preserved keys.
    #[inline]
    #[must_use]
    pub fn key_count(&self) -> usize {
        self.entries.len()
    }

    /// Total preserved payload bytes.
    #[inline]
    #[must_use]
    pub fn byte_count(&self) -> usize {
        self.bytes
    }

    /// `true` when nothing is preserved.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// `true` when no further *new* key can be preserved.
    #[inline]
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.entries.len() >= self.max_keys
    }

    /// `true` when `key` is preserved.
    #[inline]
    #[must_use]
    pub fn contains_key(&self, key: &str) -> bool {
        self.entries.contains_key(key)
    }

    /// The preserved payload for `key`.
    #[inline]
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&[u8]> {
        self.entries.get(key).map(Vec::as_slice)
    }

    /// Preserve `payload` under `key`, overwriting any previous bytes.
    ///
    /// # Errors
    ///
    /// Fails closed — the sidecar is left unchanged — on an empty or
    /// oversized key, on a new key past [`MAX_SIDECAR_KEYS`], and on a
    /// write past [`MAX_SIDECAR_BYTES`]. Overwriting an existing key does
    /// not consume a new key slot, so a save/load cycle is idempotent.
    pub fn preserve(&mut self, key: &str, payload: &[u8]) -> Result<(), SidecarError> {
        self.preserve_owned(key, payload.to_vec())
    }

    /// [`preserve`](Self::preserve) for a caller that already owns the
    /// buffer, avoiding one copy on the decode path.
    ///
    /// # Errors
    ///
    /// Same rejections as [`preserve`](Self::preserve).
    pub fn preserve_owned(&mut self, key: &str, payload: Vec<u8>) -> Result<(), SidecarError> {
        if key.is_empty() {
            return Err(SidecarError::EmptyKey);
        }
        if key.len() > MAX_SIDECAR_KEY_BYTES {
            return Err(SidecarError::KeyTooLong {
                max_bytes: MAX_SIDECAR_KEY_BYTES,
            });
        }
        let previous = self.entries.get(key).map_or(0, Vec::len);
        let is_new = !self.entries.contains_key(key);
        if is_new && self.entries.len() >= self.max_keys {
            return Err(SidecarError::TooManyKeys {
                max_keys: self.max_keys,
            });
        }
        let requested = self
            .bytes
            .saturating_sub(previous)
            .saturating_add(payload.len());
        if requested > self.max_bytes {
            return Err(SidecarError::TooManyBytes {
                max_bytes: self.max_bytes,
                requested,
            });
        }
        self.bytes = requested;
        self.entries.insert(key.to_owned(), payload);
        Ok(())
    }

    /// Drop one key, returning its bytes.
    pub fn remove(&mut self, key: &str) -> Option<Vec<u8>> {
        let payload = self.entries.remove(key)?;
        self.bytes = self.bytes.saturating_sub(payload.len());
        Some(payload)
    }

    /// Drop everything.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
    }

    /// Every `(key, payload)` in key order — the save path.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &[u8])> {
        self.entries
            .iter()
            .map(|(key, payload)| (key.as_str(), payload.as_slice()))
    }

    /// Take every preserved pair in key order and reset the sidecar.
    ///
    /// This is the re-emit primitive: the save encoder writes the pairs
    /// back under their original keys and the sidecar starts empty for the
    /// next cycle.
    pub fn take_all(&mut self) -> Vec<(String, Vec<u8>)> {
        self.bytes = 0;
        std::mem::take(&mut self.entries).into_iter().collect()
    }
}

/// Truncate a diagnostic field to [`MAX_LEDGER_FIELD_CHARS`] characters,
/// on a character boundary, marking the cut.
fn clamp_field(text: &str) -> String {
    match text.char_indices().nth(MAX_LEDGER_FIELD_CHARS) {
        None => text.to_owned(),
        Some((cut, _)) => {
            let mut clamped = String::with_capacity(cut + 3);
            clamped.push_str(&text[..cut]);
            clamped.push_str("...");
            clamped
        }
    }
}

/// A bounded recorder of extension points that declined a value.
///
/// Rows are `(domain, id) -> count`, deduplicated and rendered in
/// `(domain, id)` order, so the same run always prints the same summary.
/// The ledger holds at most `max_entries` distinct sites plus one reserved
/// overflow row (`"overflow"/"other"`) that aggregates every site beyond
/// the cap: memory stays bounded and no declined call goes uncounted.
#[derive(Clone, Debug)]
pub struct UnresolvedLedger {
    entries: BTreeMap<(String, String), u64>,
    total: u64,
    overflowed: u64,
    max_entries: usize,
}

impl UnresolvedLedger {
    /// An empty ledger capped at [`MAX_LEDGER_ENTRIES`].
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(MAX_LEDGER_ENTRIES)
    }

    /// An empty ledger with an explicit site cap.
    #[must_use]
    pub fn with_capacity(max_entries: usize) -> Self {
        Self {
            entries: BTreeMap::new(),
            total: 0,
            overflowed: 0,
            max_entries,
        }
    }

    /// The site cap, excluding the reserved overflow row.
    #[inline]
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.max_entries
    }

    /// Distinct recorded rows, including the overflow row when it exists.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` when nothing has been recorded.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Total declined calls recorded.
    #[inline]
    #[must_use]
    pub fn total(&self) -> u64 {
        self.total
    }

    /// Calls that landed in the overflow row because the site table was
    /// full. Only shrinks by [`clear`](Self::clear).
    #[inline]
    #[must_use]
    pub fn overflowed(&self) -> u64 {
        self.overflowed
    }

    /// Distinct domains with at least one row.
    #[must_use]
    pub fn domain_count(&self) -> usize {
        self.entries
            .keys()
            .map(|(domain, _)| domain)
            .collect::<BTreeSet<&String>>()
            .len()
    }

    /// Record one declined call for `(domain, id)`, returning that row's
    /// running count.
    ///
    /// This is the replacement for `_ => continue`: the caller names what
    /// it could not handle and the ledger makes it countable.
    pub fn record(&mut self, domain: &str, id: &str) -> u64 {
        self.total = self.total.saturating_add(1);
        let key = (clamp_field(domain), clamp_field(id));
        if let Some(count) = self.entries.get_mut(&key) {
            *count = count.saturating_add(1);
            return *count;
        }
        if self.entries.len() < self.max_entries {
            self.entries.insert(key, 1);
            return 1;
        }
        self.overflowed = self.overflowed.saturating_add(1);
        let bucket = (OVERFLOW_DOMAIN.to_owned(), OVERFLOW_ID.to_owned());
        let count = self.entries.entry(bucket).or_insert(0);
        *count = count.saturating_add(1);
        *count
    }

    /// Record one declined call, reporting whether this `(domain, id)` was
    /// unseen before — the "log the first occurrence only" shape.
    pub fn record_once(&mut self, domain: &str, id: &str) -> bool {
        let first = !self
            .entries
            .contains_key(&(clamp_field(domain), clamp_field(id)));
        self.record(domain, id);
        first
    }

    /// Every row as `(domain, id, count)` in `(domain, id)` order.
    pub fn entries(&self) -> impl Iterator<Item = (&str, &str, u64)> {
        self.entries
            .iter()
            .map(|((domain, id), count)| (domain.as_str(), id.as_str(), *count))
    }

    /// The count recorded for one site, if any.
    #[must_use]
    pub fn count_of(&self, domain: &str, id: &str) -> Option<u64> {
        self.entries
            .get(&(clamp_field(domain), clamp_field(id)))
            .copied()
    }

    /// A deterministic multi-line summary for boot logs and operator
    /// commands.
    ///
    /// The header states the site and call totals; each following line
    /// names one row, sorted by `(domain, id)`. An empty ledger renders a
    /// single line, so the caller can print it unconditionally.
    #[must_use]
    pub fn summary(&self) -> String {
        if self.entries.is_empty() {
            return "unresolved extensions: none".to_owned();
        }
        let mut out = format!(
            "unresolved extensions: {} site(s) across {} domain(s), {} call(s)",
            self.entries.len(),
            self.domain_count(),
            self.total
        );
        for (domain, id, count) in self.entries() {
            out.push('\n');
            out.push_str("  ");
            out.push_str(domain);
            out.push('/');
            out.push_str(id);
            out.push_str(" x");
            out.push_str(&count.to_string());
        }
        out
    }

    /// Drop every row and reset the counters.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.total = 0;
        self.overflowed = 0;
    }
}

impl Default for UnresolvedLedger {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_module_defaults_are_the_documented_ones() {
        let sidecar = UnknownNbtSidecar::new(SidecarDomain::Chunk);
        assert_eq!(sidecar.max_keys(), MAX_SIDECAR_KEYS);
        assert_eq!(sidecar.max_bytes(), MAX_SIDECAR_BYTES);
        assert_eq!(sidecar.domain(), SidecarDomain::Chunk);
        assert_eq!(
            SidecarDomain::SavedData.sidecar().domain(),
            SidecarDomain::SavedData
        );
        // The documented defaults, pinned: a silent budget change is
        // exactly the kind of drift a sidecar bound must not have.
        assert_eq!(MAX_SIDECAR_KEYS, 1024);
        assert_eq!(MAX_SIDECAR_BYTES, 4 * 1024 * 1024);
        assert_eq!(MAX_SIDECAR_KEY_BYTES, 256);
        assert_eq!(MAX_LEDGER_ENTRIES, 256);
        assert_eq!(MAX_LEDGER_FIELD_CHARS, 96);
        assert_eq!(UnresolvedLedger::new().capacity(), MAX_LEDGER_ENTRIES);
        assert_eq!(UnresolvedLedger::default().capacity(), MAX_LEDGER_ENTRIES);
    }

    #[test]
    fn every_domain_has_a_stable_name() {
        let names: Vec<&str> = SidecarDomain::ALL.iter().map(|d| d.as_str()).collect();
        assert_eq!(
            names,
            [
                "entity",
                "block_entity",
                "chunk",
                "level_data",
                "player_data",
                "saved_data"
            ]
        );
        for domain in SidecarDomain::ALL {
            assert_eq!(domain.to_string(), domain.as_str());
        }
    }

    #[test]
    fn unknown_keys_survive_a_load_save_round_trip_verbatim() {
        let mut load = UnknownNbtSidecar::new(SidecarDomain::Entity);
        // Payloads are opaque: they may be NBT, a compressed blob, or
        // bytes that are not valid text at all.
        let opaque: [Vec<u8>; 3] = [
            vec![0x0a, 0x00, 0x00, 0xff, 0xfe],
            Vec::new(),
            vec![b'x'; 128],
        ];
        assert_eq!(load.preserve("machine/state", &opaque[0]), Ok(()));
        assert_eq!(load.preserve("empty", &opaque[1]), Ok(()));
        assert_eq!(load.preserve("zzz/last", &opaque[2]), Ok(()));
        assert_eq!(load.key_count(), 3);
        assert_eq!(load.byte_count(), 5 + 128);
        assert!(!load.is_empty());

        // Save path: emit every pair under its original key, in key order.
        let emitted = load.take_all();
        assert!(load.is_empty());
        assert_eq!(load.byte_count(), 0);
        let keys: Vec<&str> = emitted.iter().map(|(key, _)| key.as_str()).collect();
        assert_eq!(keys, ["empty", "machine/state", "zzz/last"]);

        // Reload path: the same bytes come back, unmodified.
        let mut reload = UnknownNbtSidecar::new(SidecarDomain::Entity);
        for (key, payload) in &emitted {
            reload.preserve(key, payload).unwrap();
        }
        assert_eq!(reload.byte_count(), 5 + 128);
        for (key, payload) in &emitted {
            assert_eq!(reload.get(key), Some(payload.as_slice()), "{key}");
        }
        assert_eq!(reload.iter().count(), 3);
        assert_eq!(
            reload.iter().map(|(k, _)| k).collect::<Vec<&str>>(),
            ["empty", "machine/state", "zzz/last"]
        );
        assert_eq!(emitted[0].1, opaque[1]);
        assert_eq!(emitted[1].1, opaque[0]);
        assert_eq!(emitted[2].1, opaque[2]);
    }

    #[test]
    fn overwriting_a_key_costs_no_new_slot() {
        let mut sidecar = UnknownNbtSidecar::with_limits(SidecarDomain::Chunk, 2, 1024);
        sidecar.preserve("a", &[1, 2, 3]).unwrap();
        sidecar.preserve("b", &[4]).unwrap();
        assert!(sidecar.is_full());
        // Overwriting is how a save/load cycle repeats: same keys, no new
        // slot consumed, and the byte accounting follows the new size.
        sidecar.preserve("a", &[9]).unwrap();
        assert_eq!(sidecar.key_count(), 2);
        assert_eq!(sidecar.byte_count(), 2);
        assert_eq!(sidecar.get("a"), Some([9u8].as_slice()));
        assert_eq!(
            sidecar.preserve("c", &[0]).unwrap_err(),
            SidecarError::TooManyKeys { max_keys: 2 }
        );
        assert_eq!(sidecar.key_count(), 2, "a refused key writes nothing");
        assert!(!sidecar.contains_key("c"));
    }

    #[test]
    fn the_byte_budget_fails_closed() {
        let mut sidecar = UnknownNbtSidecar::with_limits(SidecarDomain::LevelData, 8, 10);
        sidecar.preserve("a", &[0; 6]).unwrap();
        let error = sidecar.preserve("b", &[0; 5]).unwrap_err();
        assert_eq!(
            error,
            SidecarError::TooManyBytes {
                max_bytes: 10,
                requested: 11
            }
        );
        assert!(error.to_string().contains("10"));
        // The refused write changed nothing at all.
        assert_eq!(sidecar.key_count(), 1);
        assert_eq!(sidecar.byte_count(), 6);
        // A write that fits is accepted, and a shrink credits bytes back.
        sidecar.preserve("b", &[0; 4]).unwrap();
        assert_eq!(sidecar.byte_count(), 10);
        sidecar.preserve("a", &[]).unwrap();
        assert_eq!(sidecar.byte_count(), 4);
        assert_eq!(
            sidecar.preserve("c", &[0; 7]).unwrap_err(),
            SidecarError::TooManyBytes {
                max_bytes: 10,
                requested: 11
            }
        );
    }

    #[test]
    fn keys_are_validated() {
        let mut sidecar = UnknownNbtSidecar::new(SidecarDomain::SavedData);
        assert_eq!(sidecar.preserve("", &[1]), Err(SidecarError::EmptyKey));
        let huge = "k".repeat(MAX_SIDECAR_KEY_BYTES + 1);
        assert_eq!(
            sidecar.preserve(&huge, &[1]),
            Err(SidecarError::KeyTooLong {
                max_bytes: MAX_SIDECAR_KEY_BYTES
            })
        );
        assert!(sidecar.is_empty());
        // Exactly at the cap is accepted.
        let exact = "k".repeat(MAX_SIDECAR_KEY_BYTES);
        sidecar.preserve(&exact, &[1]).unwrap();
        assert_eq!(sidecar.key_count(), 1);
        assert_eq!(
            SidecarError::EmptyKey.to_string(),
            "sidecar key must not be empty"
        );
    }

    #[test]
    fn removal_and_clear_keep_the_byte_accounting_exact() {
        let mut sidecar = UnknownNbtSidecar::new(SidecarDomain::PlayerData);
        sidecar.preserve("a", &[1, 2]).unwrap();
        sidecar.preserve("b", &[3, 4, 5]).unwrap();
        assert_eq!(sidecar.byte_count(), 5);
        assert_eq!(sidecar.remove("a"), Some(vec![1, 2]));
        assert_eq!(sidecar.byte_count(), 3);
        assert_eq!(sidecar.remove("a"), None);
        assert_eq!(sidecar.byte_count(), 3);
        sidecar.clear();
        assert!(sidecar.is_empty());
        assert_eq!(sidecar.byte_count(), 0);
        // A failed preserve after a clear still fails for the right reason:
        // no stale accounting left a phantom byte budget behind.
        sidecar.preserve("c", &[0; 4]).unwrap();
        assert_eq!(sidecar.byte_count(), 4);
    }

    #[test]
    fn a_zero_budget_sidecar_preserves_nothing() {
        let mut sidecar = UnknownNbtSidecar::with_limits(SidecarDomain::Chunk, 0, 0);
        assert!(sidecar.is_full());
        assert_eq!(
            sidecar.preserve("a", &[]),
            Err(SidecarError::TooManyKeys { max_keys: 0 })
        );
        assert!(sidecar.is_empty());
    }

    #[test]
    fn the_ledger_deduplicates_and_counts() {
        let mut ledger = UnresolvedLedger::new();
        assert!(ledger.is_empty());
        assert_eq!(ledger.len(), 0);
        assert_eq!(ledger.total(), 0);
        assert_eq!(ledger.summary(), "unresolved extensions: none");

        assert_eq!(ledger.record("abilities", "unknown_flag"), 1);
        assert_eq!(ledger.record("abilities", "unknown_flag"), 2);
        assert_eq!(ledger.record("recipes", "unknown_type"), 1);
        assert_eq!(ledger.len(), 2, "repeats share one row");
        assert_eq!(ledger.total(), 3);
        assert_eq!(ledger.domain_count(), 2);
        assert_eq!(ledger.count_of("abilities", "unknown_flag"), Some(2));
        assert_eq!(ledger.count_of("abilities", "nope"), None);
        assert!(!ledger.is_empty());
    }

    #[test]
    fn record_once_reports_only_the_first_occurrence() {
        let mut ledger = UnresolvedLedger::new();
        assert!(ledger.record_once("seam", "block_use"));
        assert!(!ledger.record_once("seam", "block_use"));
        assert!(ledger.record_once("seam", "item_use"));
        assert_eq!(ledger.total(), 3);
        assert_eq!(ledger.len(), 2);
    }

    #[test]
    fn the_summary_is_deterministic_and_sorted() {
        let mut ledger = UnresolvedLedger::new();
        ledger.record("seam", "item_use");
        ledger.record("abilities", "unknown_flag");
        ledger.record("abilities", "unknown_flag");
        ledger.record("seam", "block_use");
        let summary = ledger.summary();
        let lines: Vec<&str> = summary.lines().collect();
        assert_eq!(
            lines[0],
            "unresolved extensions: 3 site(s) across 2 domain(s), 4 call(s)"
        );
        // `(domain, id)` order: abilities before seam, and within a domain
        // the id decides.
        assert_eq!(lines[1], "  abilities/unknown_flag x2");
        assert_eq!(lines[2], "  seam/block_use x1");
        assert_eq!(lines[3], "  seam/item_use x1");
        assert_eq!(summary, ledger.summary(), "the summary is a pure function");
        assert!(!summary.ends_with('\n'));
    }

    #[test]
    fn the_site_cap_aggregates_into_the_reserved_overflow_row() {
        let mut ledger = UnresolvedLedger::with_capacity(2);
        assert_eq!(ledger.capacity(), 2);
        ledger.record("d", "a");
        ledger.record("d", "b");
        // Past the cap: the count is kept, the row is not created.
        ledger.record("d", "c");
        ledger.record("d", "d");
        ledger.record("d", "d");
        assert_eq!(ledger.len(), 3, "cap plus one reserved overflow row");
        assert_eq!(ledger.overflowed(), 3);
        assert_eq!(ledger.total(), 5);
        assert_eq!(ledger.count_of(OVERFLOW_DOMAIN, OVERFLOW_ID), Some(3));
        assert_eq!(ledger.count_of("d", "c"), None);
        // A row seen before the cap stays accurate afterwards.
        ledger.record("d", "a");
        assert_eq!(ledger.count_of("d", "a"), Some(2));
        assert_eq!(ledger.overflowed(), 3);
        let summary = ledger.summary();
        assert!(summary.contains("overflow/other x3"), "{summary}");
        assert!(summary.contains("6 call(s)"), "{summary}");
    }

    #[test]
    fn a_zero_capacity_ledger_records_only_overflow() {
        let mut ledger = UnresolvedLedger::with_capacity(0);
        ledger.record("d", "a");
        ledger.record("d", "b");
        assert_eq!(ledger.len(), 1);
        assert_eq!(ledger.overflowed(), 2);
        assert_eq!(ledger.total(), 2);
        assert_eq!(ledger.count_of(OVERFLOW_DOMAIN, OVERFLOW_ID), Some(2));
    }

    #[test]
    fn long_fields_are_clamped_on_a_character_boundary() {
        let mut ledger = UnresolvedLedger::new();
        // Multi-byte characters are where a naive byte slice panics.
        let long = "domain".repeat(MAX_LEDGER_FIELD_CHARS);
        let long_id = "識別子".repeat(64);
        ledger.record(&long, &long_id);
        assert_eq!(ledger.len(), 1);
        let (domain, id, _) = ledger.entries().next().unwrap();
        assert_eq!(domain.chars().count(), MAX_LEDGER_FIELD_CHARS + 3);
        assert!(domain.ends_with("..."));
        assert_eq!(id.chars().count(), MAX_LEDGER_FIELD_CHARS + 3);
        assert!(id.ends_with("..."));
        // Distinct long ids that clamp to the same prefix share one row,
        // which is the intended aggregate instead of unbounded growth.
        ledger.record(&long, &format!("{long_id}tail"));
        assert_eq!(ledger.len(), 1);
        assert_eq!(ledger.total(), 2);
        ledger.record("short", "short");
        assert_eq!(ledger.len(), 2);
    }

    #[test]
    fn clear_resets_the_rows_and_every_counter() {
        let mut ledger = UnresolvedLedger::with_capacity(1);
        ledger.record("d", "a");
        ledger.record("d", "b");
        assert_eq!(ledger.overflowed(), 1);
        ledger.clear();
        assert!(ledger.is_empty());
        assert_eq!(ledger.total(), 0);
        assert_eq!(ledger.overflowed(), 0);
        assert_eq!(ledger.domain_count(), 0);
        assert_eq!(ledger.summary(), "unresolved extensions: none");
        // The capacity survives a clear, so a reload keeps its bound.
        ledger.record("d", "a");
        ledger.record("d", "b");
        assert_eq!(ledger.overflowed(), 1);
    }
}
