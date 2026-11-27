// SPDX-License-Identifier: LGPL-3.0-or-later

//! Clean-room, zero-trademark collector/companion primitives.
//!
//! A generic "capture-and-store companions" kernel for collector-style
//! mods: a six-slot active party backed by a multi-box PC archive, a
//! deterministic seeded capture roll, a data-driven experience and
//! evolution-trigger engine, and a fail-closed `PRTY` binary codec with an
//! IEEE CRC32 trailer. Nothing here names a third-party franchise or
//! character — species, items, biomes and moves are caller-supplied
//! namespaced identifiers (`ns:path`), so the same code drives any mod's
//! roster.
//!
//! Determinism is a contract: the capture roll derives every bit of
//! randomness from the caller's `rng_seed` through a SplitMix64 stream, the
//! experience tables are pure integer polynomials, and [`encode_storage`]
//! serializes slots in a fixed order so identical storage produces
//! byte-identical output. [`decode_storage`] fails closed — bad magic, a
//! CRC mismatch, truncation, an unknown schema version, a count past the
//! hard caps, a field violating a record invariant or trailing garbage are
//! all errors, never a partially restored storage or a panic.
//!
//! Wire layout — every integer is big-endian, every count and string
//! length is a `u32`:
//!
//! ```text
//!   magic      : 4 bytes "PRTY"
//!   schema     : u32        (currently 1)
//!   box_count  : u32        (0..=64)
//!   party      : 6 slots    (presence u8 + record when present)
//!   boxes      : box_count × 30 slots, same slot encoding
//!   record     : uid 16B, owner_uuid 16B, species_id str, nickname opt-str,
//!                level u8, experience u32, current_hp u16, max_hp u16,
//!                friendship u8, held_item opt-str, status_condition opt-str,
//!                nature_or_trait str, shiny_or_rare u8, move_count u32 +
//!                move strings, custom_stats 6 × u16
//!   trailer    : u32 CRC32 (IEEE 802.3, reflected) over every byte above
//! ```

use std::error::Error;
use std::fmt;

/// Active party size: a trainer carries at most six companions.
pub const PARTY_SLOTS: usize = 6;
/// Companions per PC box: a fixed thirty-slot grid.
pub const BOX_SLOTS: usize = 30;
/// Number of PC boxes a fresh [`PlayerCompanionStorage`] starts with.
pub const DEFAULT_BOX_COUNT: usize = 8;
/// Hard cap on PC boxes; construction and decode both refuse more.
pub const MAX_BOX_COUNT: usize = 64;
/// Highest attainable level on any [`GrowthCurve`].
pub const MAX_LEVEL: u8 = 100;
/// A companion's move list never exceeds four entries.
pub const MAX_MOVES: usize = 4;

/// A capture device at or above this multiplier always succeeds; the roll
/// is short-circuited before a single shake check runs.
pub const GUARANTEED_CAPTURE_MULTIPLIER: f32 = 255.0;
/// Number of shake checks a capture attempt runs before it locks.
pub const SHAKE_CHECKS: u8 = 4;

/// Fail-closed caps shared by record validation and the `PRTY` decoder.
/// They sit far past any real field so they reject garbage, not legitimate
/// saves.
const MAX_STRING_LEN: usize = 1_024;

/// `ns:path` identifier shape: one colon, a lowercase namespace of
/// `[a-z0-9_.-]` and a non-empty lowercase path of `[a-z0-9_./-]` with no
/// second colon.
fn is_namespaced_id(s: &str) -> bool {
    let Some((ns, path)) = s.split_once(':') else {
        return false;
    };
    !ns.is_empty()
        && !path.is_empty()
        && ns.bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-'))
        && path
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'/'))
}

fn capped(field: &'static str, s: &str) -> Result<(), RecordError> {
    if s.len() > MAX_STRING_LEN {
        return Err(RecordError::FieldTooLong {
            field,
            len: s.len(),
            cap: MAX_STRING_LEN,
        });
    }
    Ok(())
}

// ------------------------------------------------------------------ record

/// One stored companion. Every identity is caller-supplied and namespaced;
/// the struct is deliberately plain data so a mod's roster maps onto it
/// without adapter types.
#[derive(Clone, Debug, PartialEq)]
pub struct CompanionRecord {
    /// Unique 128-bit identifier; conservation tests treat it as the
    /// companion's identity across every storage operation.
    pub uid: [u8; 16],
    /// Namespaced species identifier (`ns:path`), e.g. `mymod:ember_cub`.
    pub species_id: String,
    /// Optional free-text display name.
    pub nickname: Option<String>,
    /// Current level, `1..=100`.
    pub level: u8,
    /// Accumulated experience points toward the current growth curve.
    pub experience: u32,
    /// Remaining hit points; never exceeds `max_hp`.
    pub current_hp: u16,
    /// Hit-point ceiling; at least 1.
    pub max_hp: u16,
    /// Friendship/affection score `0..=255`, read by `MinFriendship`
    /// evolution conditions.
    pub friendship: u8,
    /// Namespaced id of the held item, if any.
    pub held_item: Option<String>,
    /// Free-text status condition (`"poisoned"`, `"asleep"`, …), if any.
    pub status_condition: Option<String>,
    /// Free-text nature/trait descriptor.
    pub nature_or_trait: String,
    /// Rare-variant flag.
    pub shiny_or_rare: bool,
    /// Namespaced move ids, at most [`MAX_MOVES`].
    pub moves: Vec<String>,
    /// Six caller-defined stat values (attack/defence/speed/…), order fixed
    /// by the owning mod.
    pub custom_stats: [u16; 6],
    /// Owning player's 128-bit UUID.
    pub owner_uuid: [u8; 16],
}

/// Why a [`CompanionRecord`] violates a storage invariant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordError {
    /// `level` is outside `1..=100`.
    LevelOutOfRange { level: u8 },
    /// `species_id` is not a well-formed `ns:path` identifier.
    InvalidSpeciesId { species_id: String },
    /// More than [`MAX_MOVES`] moves were attached.
    TooManyMoves { count: usize },
    /// `max_hp` is zero or `current_hp` exceeds it.
    InvalidHitPoints { current: u16, max: u16 },
    /// A string field exceeds the codec's byte-length cap.
    FieldTooLong {
        field: &'static str,
        len: usize,
        cap: usize,
    },
    /// A move slot held an empty string.
    EmptyMove { index: usize },
}

impl fmt::Display for RecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RecordError::LevelOutOfRange { level } => {
                write!(f, "companion level {level} is outside 1..={MAX_LEVEL}")
            }
            RecordError::InvalidSpeciesId { species_id } => {
                write!(f, "species id {species_id:?} is not a namespaced ns:path id")
            }
            RecordError::TooManyMoves { count } => {
                write!(f, "companion has {count} moves, cap is {MAX_MOVES}")
            }
            RecordError::InvalidHitPoints { current, max } => {
                write!(f, "invalid hit points: current {current}, max {max}")
            }
            RecordError::FieldTooLong { field, len, cap } => {
                write!(f, "{field} is {len} bytes, cap is {cap}")
            }
            RecordError::EmptyMove { index } => write!(f, "move slot {index} is empty"),
        }
    }
}

impl Error for RecordError {}

impl CompanionRecord {
    /// A record with sane defaults: level 5, full health, empty move list.
    /// Callers override fields directly; [`validate`](Self::validate)
    /// catches the result when it no longer satisfies the invariants.
    pub fn new(uid: [u8; 16], owner_uuid: [u8; 16], species_id: impl Into<String>) -> Self {
        CompanionRecord {
            uid,
            species_id: species_id.into(),
            nickname: None,
            level: 5,
            experience: 0,
            current_hp: 20,
            max_hp: 20,
            friendship: 0,
            held_item: None,
            status_condition: None,
            nature_or_trait: String::new(),
            shiny_or_rare: false,
            moves: Vec::new(),
            custom_stats: [0; 6],
            owner_uuid,
        }
    }

    /// Check every invariant the storage and the `PRTY` codec rely on.
    /// [`PlayerCompanionStorage::add_captured`] runs this before inserting,
    /// and [`decode_storage`] runs it on every decoded record.
    pub fn validate(&self) -> Result<(), RecordError> {
        if !(1..=MAX_LEVEL).contains(&self.level) {
            return Err(RecordError::LevelOutOfRange { level: self.level });
        }
        if !is_namespaced_id(&self.species_id) {
            return Err(RecordError::InvalidSpeciesId {
                species_id: self.species_id.clone(),
            });
        }
        if self.moves.len() > MAX_MOVES {
            return Err(RecordError::TooManyMoves {
                count: self.moves.len(),
            });
        }
        if self.max_hp == 0 || self.current_hp > self.max_hp {
            return Err(RecordError::InvalidHitPoints {
                current: self.current_hp,
                max: self.max_hp,
            });
        }
        capped("species_id", &self.species_id)?;
        if let Some(s) = &self.nickname {
            capped("nickname", s)?;
        }
        if let Some(s) = &self.held_item {
            capped("held_item", s)?;
        }
        if let Some(s) = &self.status_condition {
            capped("status_condition", s)?;
        }
        capped("nature_or_trait", &self.nature_or_trait)?;
        for (index, mv) in self.moves.iter().enumerate() {
            if mv.is_empty() {
                return Err(RecordError::EmptyMove { index });
            }
            capped("moves", mv)?;
        }
        Ok(())
    }
}

// ----------------------------------------------------------------- storage

/// A slot address inside [`PlayerCompanionStorage`]: either one of the six
/// active-party slots or one grid position inside one PC box.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StorageSlotRef {
    /// Active-party slot `0..6`.
    Party(usize),
    /// PC box `box_index`, grid slot `0..30`.
    PcBox { box_index: usize, slot_index: usize },
}

impl fmt::Display for StorageSlotRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StorageSlotRef::Party(i) => write!(f, "party[{i}]"),
            StorageSlotRef::PcBox {
                box_index,
                slot_index,
            } => write!(f, "box[{box_index}][{slot_index}]"),
        }
    }
}

/// Why a storage mutation failed.
#[derive(Clone, Debug, PartialEq)]
pub enum StorageError {
    /// Party and every PC box are occupied; nothing was inserted.
    Full,
    /// The referenced slot lies outside the storage's geometry.
    InvalidSlot(StorageSlotRef),
    /// Requested box count exceeds [`MAX_BOX_COUNT`].
    BoxCountExceeded { requested: usize, cap: usize },
    /// The record violates a [`CompanionRecord`] invariant; it was not
    /// inserted.
    InvalidRecord(RecordError),
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StorageError::Full => write!(f, "party and all PC boxes are full"),
            StorageError::InvalidSlot(slot) => write!(f, "invalid slot reference {slot}"),
            StorageError::BoxCountExceeded { requested, cap } => {
                write!(f, "box count {requested} exceeds cap {cap}")
            }
            StorageError::InvalidRecord(e) => write!(f, "invalid companion record: {e}"),
        }
    }
}

impl Error for StorageError {}

/// One player's companion storage: a six-slot active party plus a
/// configurable number of thirty-slot PC boxes. Every mutating operation
/// conserves records — no duplication, no silent loss.
#[derive(Debug, PartialEq)]
pub struct PlayerCompanionStorage {
    party: [Option<CompanionRecord>; PARTY_SLOTS],
    boxes: Vec<[Option<CompanionRecord>; BOX_SLOTS]>,
}

impl PlayerCompanionStorage {
    /// A storage with [`DEFAULT_BOX_COUNT`] empty PC boxes.
    pub fn new() -> Self {
        Self::with_box_count(DEFAULT_BOX_COUNT)
            .expect("DEFAULT_BOX_COUNT is inside the allowed range")
    }

    /// A storage with `box_count` empty PC boxes. `0` is a legal
    /// party-only configuration; anything past [`MAX_BOX_COUNT`] is
    /// refused.
    pub fn with_box_count(box_count: usize) -> Result<Self, StorageError> {
        if box_count > MAX_BOX_COUNT {
            return Err(StorageError::BoxCountExceeded {
                requested: box_count,
                cap: MAX_BOX_COUNT,
            });
        }
        Ok(PlayerCompanionStorage {
            party: std::array::from_fn(|_| None),
            boxes: (0..box_count)
                .map(|_| std::array::from_fn(|_| None))
                .collect(),
        })
    }

    /// Read-only view of the six active-party slots.
    pub fn party(&self) -> &[Option<CompanionRecord>; PARTY_SLOTS] {
        &self.party
    }

    /// Number of PC boxes this storage was built with.
    pub fn box_count(&self) -> usize {
        self.boxes.len()
    }

    /// Read-only view of one PC box's thirty slots.
    pub fn box_slots(&self, box_index: usize) -> Option<&[Option<CompanionRecord>; BOX_SLOTS]> {
        self.boxes.get(box_index)
    }

    /// Total slot capacity: party plus every PC box.
    pub fn capacity(&self) -> usize {
        PARTY_SLOTS + self.boxes.len() * BOX_SLOTS
    }

    /// Whether `slot` addresses a real slot in this storage's geometry
    /// (regardless of occupancy).
    pub fn is_valid_slot(&self, slot: StorageSlotRef) -> bool {
        self.slot_ref(slot).is_some()
    }

    /// The companion in `slot`, or `None` when the slot is empty or the
    /// reference is outside the geometry.
    pub fn get(&self, slot: StorageSlotRef) -> Option<&CompanionRecord> {
        self.slot_ref(slot)?.as_ref()
    }

    /// Every occupied slot in canonical order: party `0..6`, then each box
    /// in ascending box-then-slot order.
    pub fn occupied_slots(&self) -> Vec<StorageSlotRef> {
        let mut out = Vec::new();
        for (i, slot) in self.party.iter().enumerate() {
            if slot.is_some() {
                out.push(StorageSlotRef::Party(i));
            }
        }
        for (box_index, bx) in self.boxes.iter().enumerate() {
            for (slot_index, slot) in bx.iter().enumerate() {
                if slot.is_some() {
                    out.push(StorageSlotRef::PcBox {
                        box_index,
                        slot_index,
                    });
                }
            }
        }
        out
    }

    /// Count of occupied slots across party and all boxes.
    pub fn total_occupied(&self) -> usize {
        self.party.iter().flatten().count()
            + self
                .boxes
                .iter()
                .flat_map(|bx| bx.iter())
                .flatten()
                .count()
    }

    /// Insert a freshly captured companion: first empty party slot, then
    /// the first empty PC-box slot in ascending box/slot order.
    /// `Err(StorageError::Full)` means nothing was inserted.
    pub fn add_captured(
        &mut self,
        record: CompanionRecord,
    ) -> Result<StorageSlotRef, StorageError> {
        record.validate().map_err(StorageError::InvalidRecord)?;
        for (i, slot) in self.party.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = Some(record);
                return Ok(StorageSlotRef::Party(i));
            }
        }
        for (box_index, bx) in self.boxes.iter_mut().enumerate() {
            for (slot_index, slot) in bx.iter_mut().enumerate() {
                if slot.is_none() {
                    *slot = Some(record);
                    return Ok(StorageSlotRef::PcBox {
                        box_index,
                        slot_index,
                    });
                }
            }
        }
        Err(StorageError::Full)
    }

    /// Atomically exchange the contents of any two slots — party↔party,
    /// party↔box or box↔box. Either slot may be empty; both references are
    /// validated before anything moves, so an invalid pair changes nothing.
    pub fn swap_slots(&mut self, a: StorageSlotRef, b: StorageSlotRef) -> Result<(), StorageError> {
        if !self.is_valid_slot(a) {
            return Err(StorageError::InvalidSlot(a));
        }
        if !self.is_valid_slot(b) {
            return Err(StorageError::InvalidSlot(b));
        }
        if a == b {
            return Ok(());
        }
        let va = self.slot_mut(a).expect("validated slot").take();
        let vb = self.slot_mut(b).expect("validated slot").take();
        *self.slot_mut(a).expect("validated slot") = vb;
        *self.slot_mut(b).expect("validated slot") = va;
        Ok(())
    }

    /// Restore every party member to full hit points and clear its status
    /// condition. Returns how many members actually needed healing.
    pub fn heal_all_party(&mut self) -> usize {
        let mut healed = 0;
        for record in self.party.iter_mut().flatten() {
            if record.current_hp < record.max_hp || record.status_condition.is_some() {
                healed += 1;
            }
            record.current_hp = record.max_hp;
            record.status_condition = None;
        }
        healed
    }

    /// Remove and return the companion in `slot`, leaving it empty.
    /// `None` when the slot was already empty or out of range.
    pub fn release(&mut self, slot: StorageSlotRef) -> Option<CompanionRecord> {
        self.slot_mut(slot)?.take()
    }

    fn slot_ref(&self, slot: StorageSlotRef) -> Option<&Option<CompanionRecord>> {
        match slot {
            StorageSlotRef::Party(i) => self.party.get(i),
            StorageSlotRef::PcBox {
                box_index,
                slot_index,
            } => self.boxes.get(box_index)?.get(slot_index),
        }
    }

    fn slot_mut(&mut self, slot: StorageSlotRef) -> Option<&mut Option<CompanionRecord>> {
        match slot {
            StorageSlotRef::Party(i) => self.party.get_mut(i),
            StorageSlotRef::PcBox {
                box_index,
                slot_index,
            } => self.boxes.get_mut(box_index)?.get_mut(slot_index),
        }
    }
}

impl Default for PlayerCompanionStorage {
    fn default() -> Self {
        Self::new()
    }
}

// ------------------------------------------------------------ capture roll

/// A capture device's data-driven specification. `device_id` is a
/// namespaced identifier; `device_multiplier` scales the catch value (a
/// plain device is `1.0`, and `>= `[`GUARANTEED_CAPTURE_MULTIPLIER`]
/// captures unconditionally).
#[derive(Clone, Debug, PartialEq)]
pub struct CaptureDeviceSpec {
    /// Namespaced device identifier (`ns:path`).
    pub device_id: String,
    /// Catch-value multiplier; non-finite or non-positive values are
    /// treated as zero.
    pub device_multiplier: f32,
}

/// Result of [`evaluate_capture_attempt`]: the device either locked
/// (`Captured`) or broke open after `shakes` (`0..=3`) successful shake
/// checks (`Escaped`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureOutcome {
    /// All [`SHAKE_CHECKS`] shake checks passed; the target is caught.
    Captured,
    /// Check `shakes` failed after `shakes` successful shakes
    /// (`0` = broke out immediately, `3` = failed the final check).
    Escaped { shakes: u8 },
}

impl CaptureOutcome {
    /// Whether the attempt ended in a capture.
    pub fn is_captured(&self) -> bool {
        matches!(self, CaptureOutcome::Captured)
    }
}

/// Deterministic SplitMix64 stream; every roll in one attempt derives from
/// the caller's `rng_seed`, so a seed reproduces an identical attempt.
fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// A non-finite or non-positive multiplier contributes zero catch value.
fn finite_or_zero(v: f32) -> f64 {
    if v.is_finite() && v > 0.0 {
        f64::from(v)
    } else {
        0.0
    }
}

/// Evaluate one capture attempt deterministically.
///
/// The catch value `a = base_catch_rate × device_multiplier × hp_factor ×
/// status_bonus × env_bonus`, where `hp_factor = (3·max_hp − 2·hp) /
/// (3·max_hp)` runs from `1/3` at full health toward `1` near zero health.
/// Each of the [`SHAKE_CHECKS`] shake checks succeeds with probability
/// `(a / 255)^(1/4)` — the classic fourth-root shape that makes the
/// overall capture chance equal `a / 255`. Rolls come from a SplitMix64
/// stream seeded with `rng_seed`, so the same inputs always reproduce the
/// same shake-for-shake outcome, and a larger catch value is monotone per
/// seed: it can only turn failed checks into passes.
///
/// `target_hp` is clamped to `target_max_hp` and `base_catch_rate` to `255`
/// so out-of-range callers cannot manufacture a guaranteed catch; a
/// multiplier `>= `[`GUARANTEED_CAPTURE_MULTIPLIER`] is the deliberate
/// escape hatch and returns [`CaptureOutcome::Captured`] without rolling.
pub fn evaluate_capture_attempt(
    spec: &CaptureDeviceSpec,
    target_hp: u16,
    target_max_hp: u16,
    base_catch_rate: u16,
    status_bonus: f32,
    env_bonus: f32,
    rng_seed: u64,
) -> CaptureOutcome {
    if spec.device_multiplier >= GUARANTEED_CAPTURE_MULTIPLIER {
        return CaptureOutcome::Captured;
    }
    let max_hp = f64::from(target_max_hp.max(1));
    let hp = f64::from(target_hp.min(target_max_hp));
    let hp_factor = (3.0 * max_hp - 2.0 * hp) / (3.0 * max_hp);
    let catch_value = f64::from(base_catch_rate.min(255))
        * finite_or_zero(spec.device_multiplier)
        * hp_factor
        * finite_or_zero(status_bonus)
        * finite_or_zero(env_bonus);
    if catch_value <= 0.0 {
        return CaptureOutcome::Escaped { shakes: 0 };
    }
    let p_check = (catch_value / 255.0).min(1.0).powf(0.25);
    // 53-bit rolls against a scaled threshold: p_check == 1.0 yields a
    // threshold of 2^53, which every 53-bit roll is below.
    let threshold = (p_check * 9_007_199_254_740_992.0) as u64;
    let mut state = rng_seed;
    for shakes in 0..SHAKE_CHECKS {
        let roll = splitmix64(&mut state) >> 11;
        if roll >= threshold {
            return CaptureOutcome::Escaped { shakes };
        }
    }
    CaptureOutcome::Captured
}

// -------------------------------------------------------- levels & growth

/// Experience-rate curve: how many total XP a companion needs to stand at
/// a given level. The four classic polynomial families, expressed as
/// integer math — `n` is the level.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrowthCurve {
    /// `4n³/5` — 800 000 XP at level 100.
    Fast,
    /// `n³` — 1 000 000 XP at level 100.
    MediumFast,
    /// `6n³/5 − 15n² + 100n − 140` — 1 059 860 XP at level 100; the
    /// polynomial's derivative is positive for every `n ≥ 1`, so the table
    /// is strictly increasing.
    MediumSlow,
    /// `5n³/4` — 1 250 000 XP at level 100.
    Slow,
}

/// Total experience required to stand at `level` on `curve`. Level 1
/// always costs zero; out-of-range levels clamp to `1..=100`.
pub fn experience_for_level(curve: GrowthCurve, level: u8) -> u32 {
    let n = u64::from(level.clamp(1, MAX_LEVEL));
    if n <= 1 {
        return 0;
    }
    let cubed = n * n * n;
    let xp = match curve {
        GrowthCurve::Fast => cubed * 4 / 5,
        GrowthCurve::MediumFast => cubed,
        GrowthCurve::MediumSlow => {
            let v = (cubed * 6 / 5) as i64 - 15 * (n * n) as i64 + 100 * n as i64 - 140;
            if v <= 0 {
                return 0;
            }
            v as u64
        }
        GrowthCurve::Slow => cubed * 5 / 4,
    };
    u32::try_from(xp).unwrap_or(u32::MAX)
}

/// Highest level reachable with `experience` on `curve`, `1..=100`.
pub fn level_for_experience(curve: GrowthCurve, experience: u32) -> u8 {
    let mut level = 1u8;
    for candidate in 2..=MAX_LEVEL {
        if experience_for_level(curve, candidate) <= experience {
            level = candidate;
        } else {
            break;
        }
    }
    level
}

/// What [`award_experience`] did to a record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LevelUpReport {
    /// Level before the award.
    pub previous_level: u8,
    /// Level after the award (never lower than `previous_level`).
    pub new_level: u8,
    /// `new_level − previous_level`; can span many levels on a large award.
    pub levels_gained: u8,
    /// Total experience after the award, capped at the level-100 entry.
    pub experience: u32,
}

/// Grant `amount` XP to `record` along `curve`, re-deriving its level from
/// the table. Experience saturates at the level-100 requirement and levels
/// never decrease, so the operation handles multi-level awards and
/// post-cap awards deterministically.
pub fn award_experience(
    record: &mut CompanionRecord,
    curve: GrowthCurve,
    amount: u32,
) -> LevelUpReport {
    let cap = experience_for_level(curve, MAX_LEVEL);
    let previous_level = record.level;
    record.experience = record.experience.saturating_add(amount).min(cap);
    let new_level = level_for_experience(curve, record.experience).max(previous_level);
    record.level = new_level;
    LevelUpReport {
        previous_level,
        new_level,
        levels_gained: new_level - previous_level,
        experience: record.experience,
    }
}

// ----------------------------------------------------------- evolution

/// A quarter of the in-world day cycle, matched by
/// [`EvolutionCondition::TimeOfDay`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DayPhase {
    /// Morning transition window.
    Dawn,
    /// Full daylight.
    #[default]
    Day,
    /// Evening transition window.
    Dusk,
    /// Full darkness.
    Night,
}

/// One declarative requirement inside an [`EvolutionRule`]. All data is
/// namespaced strings or scalars — no species names or hardcoded triggers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EvolutionCondition {
    /// The companion's level is at least the given value.
    MinLevel(u8),
    /// The companion's held item is exactly this namespaced id.
    HeldItem(String),
    /// This namespaced item was applied to the companion this check.
    UseItem(String),
    /// The companion's friendship is at least the given value.
    MinFriendship(u8),
    /// The world is currently in this day phase.
    TimeOfDay(DayPhase),
    /// The companion currently stands in this namespaced biome.
    Biome(String),
}

/// A data-driven evolution edge: companions of `source_species_id` whose
/// record and context satisfy every `conditions` entry may evolve into
/// `target_species_id`. An empty `conditions` list is unconditional.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EvolutionRule {
    /// Namespaced species this rule applies to.
    pub source_species_id: String,
    /// Namespaced species the companion evolves into.
    pub target_species_id: String,
    /// Every condition that must hold simultaneously.
    pub conditions: Vec<EvolutionCondition>,
}

/// World-side inputs for [`eligible_evolutions`]: the item applied this
/// check, the current day phase and the companion's biome. Record-side
/// inputs (level, friendship, held item) come from the record itself.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EvolutionContext {
    /// Namespaced id of an item used on the companion this check, if any.
    pub used_item: Option<String>,
    /// Current world day phase (`Day` when unspecified).
    pub time_of_day: DayPhase,
    /// Namespaced id of the biome the companion stands in, if known.
    pub biome_id: Option<String>,
}

/// One rule whose conditions a record fully satisfies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EvolutionCandidate {
    /// Index of the satisfied rule inside the caller's `rules` slice.
    pub rule_index: usize,
    /// UID of the companion that may evolve.
    pub source_uid: [u8; 16],
    /// Namespaced species it may evolve into.
    pub target_species_id: String,
}

fn condition_met(
    condition: &EvolutionCondition,
    record: &CompanionRecord,
    ctx: &EvolutionContext,
) -> bool {
    match condition {
        EvolutionCondition::MinLevel(min) => record.level >= *min,
        EvolutionCondition::HeldItem(item) => record.held_item.as_deref() == Some(item.as_str()),
        EvolutionCondition::UseItem(item) => ctx.used_item.as_deref() == Some(item.as_str()),
        EvolutionCondition::MinFriendship(min) => record.friendship >= *min,
        EvolutionCondition::TimeOfDay(phase) => ctx.time_of_day == *phase,
        EvolutionCondition::Biome(biome) => ctx.biome_id.as_deref() == Some(biome.as_str()),
    }
}

/// Every rule in `rules` whose source species matches `record` and whose
/// conditions all hold — in the caller's rule order, so a mod's own table
/// ordering decides priority when several rules qualify.
pub fn eligible_evolutions(
    record: &CompanionRecord,
    rules: &[EvolutionRule],
    ctx: &EvolutionContext,
) -> Vec<EvolutionCandidate> {
    rules
        .iter()
        .enumerate()
        .filter(|(_, rule)| rule.source_species_id == record.species_id)
        .filter(|(_, rule)| {
            rule.conditions
                .iter()
                .all(|c| condition_met(c, record, ctx))
        })
        .map(|(rule_index, rule)| EvolutionCandidate {
            rule_index,
            source_uid: record.uid,
            target_species_id: rule.target_species_id.clone(),
        })
        .collect()
}

// ------------------------------------------------------------------ CRC32

/// IEEE 802.3 reflected CRC32 (the Ethernet/`zlib` polynomial), built at
/// compile time — pure `std`, no table crate.
const fn crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

static CRC32_TABLE: [u32; 256] = crc32_table();

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in bytes {
        crc = CRC32_TABLE[((crc ^ u32::from(b)) & 0xff) as usize] ^ (crc >> 8);
    }
    !crc
}

// --------------------------------------------------------- PRTY encoding

/// File magic: `P`a`RT`Y storage, schema version follows in the header.
const PRTY_MAGIC: &[u8; 4] = b"PRTY";
/// Current `PRTY` schema version. Decode refuses any other value.
pub const PRTY_SCHEMA_VERSION: u32 = 1;

/// Why a byte string is not a valid `PRTY` storage image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StorageDecodeError {
    /// The first four bytes are not `b"PRTY"`.
    BadMagic,
    /// The stream ends before the record being read completes.
    Truncated,
    /// The trailing CRC32 does not match the header and body bytes.
    CrcMismatch { expected: u32, actual: u32 },
    /// The schema version field is not [`PRTY_SCHEMA_VERSION`].
    UnsupportedSchemaVersion(u32),
    /// A count field exceeds the codec's safe cap.
    OversizedCount {
        field: &'static str,
        count: u32,
        cap: u32,
    },
    /// A length-prefixed byte field exceeds its cap.
    OversizedLength {
        field: &'static str,
        len: u32,
        cap: u32,
    },
    /// A string field is not valid UTF-8.
    InvalidUtf8(&'static str),
    /// A single-byte flag carries a value outside its `0/1` contract.
    InvalidFlag { field: &'static str, value: u8 },
    /// A decoded record violates a [`CompanionRecord`] invariant.
    InvalidRecord(RecordError),
    /// Bytes remain after the last declared record: the stream carries
    /// trailing garbage.
    TrailingBytes { trailing: usize },
}

impl fmt::Display for StorageDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StorageDecodeError::BadMagic => write!(f, "not a PRTY storage image (bad magic)"),
            StorageDecodeError::Truncated => write!(f, "storage image ends mid-record"),
            StorageDecodeError::CrcMismatch { expected, actual } => write!(
                f,
                "storage CRC32 mismatch: stored {expected:#010x}, computed {actual:#010x}"
            ),
            StorageDecodeError::UnsupportedSchemaVersion(v) => {
                write!(f, "unsupported PRTY schema version {v}")
            }
            StorageDecodeError::OversizedCount { field, count, cap } => {
                write!(f, "storage {field} count {count} exceeds cap {cap}")
            }
            StorageDecodeError::OversizedLength { field, len, cap } => {
                write!(f, "storage {field} length {len} exceeds cap {cap}")
            }
            StorageDecodeError::InvalidUtf8(field) => {
                write!(f, "storage {field} is not valid UTF-8")
            }
            StorageDecodeError::InvalidFlag { field, value } => {
                write!(f, "storage {field} flag {value} is not 0 or 1")
            }
            StorageDecodeError::InvalidRecord(e) => {
                write!(f, "stored companion record is invalid: {e}")
            }
            StorageDecodeError::TrailingBytes { trailing } => {
                write!(f, "{trailing} trailing bytes after storage image")
            }
        }
    }
}

impl Error for StorageDecodeError {}

fn put_u8(out: &mut Vec<u8>, v: u8) {
    out.push(v);
}

fn put_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_be_bytes());
}

/// `u32` length prefix + raw bytes.
fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    put_u32(
        out,
        u32::try_from(bytes.len()).expect("storage field longer than u32::MAX"),
    );
    out.extend_from_slice(bytes);
}

/// `u8` presence flag + bytes when present.
fn put_opt_string(out: &mut Vec<u8>, value: &Option<String>) {
    match value {
        Some(s) => {
            put_u8(out, 1);
            put_bytes(out, s.as_bytes());
        }
        None => put_u8(out, 0),
    }
}

fn put_record(out: &mut Vec<u8>, record: &CompanionRecord) {
    out.extend_from_slice(&record.uid);
    out.extend_from_slice(&record.owner_uuid);
    put_bytes(out, record.species_id.as_bytes());
    put_opt_string(out, &record.nickname);
    put_u8(out, record.level);
    put_u32(out, record.experience);
    put_u16(out, record.current_hp);
    put_u16(out, record.max_hp);
    put_u8(out, record.friendship);
    put_opt_string(out, &record.held_item);
    put_opt_string(out, &record.status_condition);
    put_bytes(out, record.nature_or_trait.as_bytes());
    put_u8(out, u8::from(record.shiny_or_rare));
    put_u32(out, record.moves.len() as u32);
    for mv in &record.moves {
        put_bytes(out, mv.as_bytes());
    }
    for stat in record.custom_stats {
        put_u16(out, stat);
    }
}

fn put_slot(out: &mut Vec<u8>, slot: &Option<CompanionRecord>) {
    match slot {
        Some(record) => {
            put_u8(out, 1);
            put_record(out, record);
        }
        None => put_u8(out, 0),
    }
}

/// Serialize `storage` into the `PRTY` format. Deterministic: the same
/// storage always produces byte-identical output because slots encode in a
/// fixed order and no hash-map iteration reaches the wire.
pub fn encode_storage(storage: &PlayerCompanionStorage) -> Vec<u8> {
    // Reserve the fixed envelope and one presence marker per slot. Records
    // grow the buffer only when their variable-length contents require it.
    let mut out = Vec::with_capacity(
        16 + PARTY_SLOTS + storage.boxes.len() * BOX_SLOTS,
    );
    out.extend_from_slice(PRTY_MAGIC);
    put_u32(&mut out, PRTY_SCHEMA_VERSION);
    put_u32(&mut out, storage.boxes.len() as u32);
    for slot in &storage.party {
        put_slot(&mut out, slot);
    }
    for bx in &storage.boxes {
        for slot in bx {
            put_slot(&mut out, slot);
        }
    }
    let crc = crc32(&out);
    put_u32(&mut out, crc);
    out
}

// --------------------------------------------------------- PRTY decoding

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], StorageDecodeError> {
        if self.bytes.len() - self.pos < n {
            return Err(StorageDecodeError::Truncated);
        }
        let slice = &self.bytes[self.pos..self.pos + n];
        self.pos += n;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, StorageDecodeError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, StorageDecodeError> {
        Ok(u16::from_be_bytes(
            self.take(2)?.try_into().map_err(|_| StorageDecodeError::Truncated)?,
        ))
    }

    fn u32(&mut self) -> Result<u32, StorageDecodeError> {
        Ok(u32::from_be_bytes(
            self.take(4)?.try_into().map_err(|_| StorageDecodeError::Truncated)?,
        ))
    }

    /// A `u32` count refused when it exceeds `cap` — checked before a
    /// single element is read or allocated.
    fn count(&mut self, field: &'static str, cap: u32) -> Result<u32, StorageDecodeError> {
        let count = self.u32()?;
        if count > cap {
            return Err(StorageDecodeError::OversizedCount { field, count, cap });
        }
        Ok(count)
    }

    /// A `u32` length + that many bytes, refused when the length exceeds
    /// `cap` before any bytes are taken.
    fn bytes_capped(
        &mut self,
        field: &'static str,
        cap: u32,
    ) -> Result<&'a [u8], StorageDecodeError> {
        let len = self.u32()?;
        if len > cap {
            return Err(StorageDecodeError::OversizedLength { field, len, cap });
        }
        self.take(len as usize)
    }

    fn string(&mut self, field: &'static str) -> Result<String, StorageDecodeError> {
        let bytes = self.bytes_capped(field, MAX_STRING_LEN as u32)?;
        std::str::from_utf8(bytes)
            .map(str::to_owned)
            .map_err(|_| StorageDecodeError::InvalidUtf8(field))
    }

    /// A `0/1` presence flag + a string when `1`; any other flag value is
    /// a hard error, not a silent `None`.
    fn opt_string(&mut self, field: &'static str) -> Result<Option<String>, StorageDecodeError> {
        match self.u8()? {
            0 => Ok(None),
            1 => Ok(Some(self.string(field)?)),
            value => Err(StorageDecodeError::InvalidFlag { field, value }),
        }
    }

    fn fixed<const N: usize>(&mut self) -> Result<[u8; N], StorageDecodeError> {
        self.take(N)?
            .try_into()
            .map_err(|_| StorageDecodeError::Truncated)
    }
}

fn read_record(r: &mut Reader<'_>) -> Result<CompanionRecord, StorageDecodeError> {
    let uid: [u8; 16] = r.fixed()?;
    let owner_uuid: [u8; 16] = r.fixed()?;
    let species_id = r.string("species_id")?;
    let nickname = r.opt_string("nickname")?;
    let level = r.u8()?;
    let experience = r.u32()?;
    let current_hp = r.u16()?;
    let max_hp = r.u16()?;
    let friendship = r.u8()?;
    let held_item = r.opt_string("held_item")?;
    let status_condition = r.opt_string("status_condition")?;
    let nature_or_trait = r.string("nature_or_trait")?;
    let shiny_or_rare = match r.u8()? {
        0 => false,
        1 => true,
        value => {
            return Err(StorageDecodeError::InvalidFlag {
                field: "shiny_or_rare",
                value,
            });
        }
    };
    let move_count = r.count("move_count", MAX_MOVES as u32)?;
    let mut moves = Vec::with_capacity(move_count as usize);
    for _ in 0..move_count {
        moves.push(r.string("moves")?);
    }
    let mut custom_stats = [0u16; 6];
    for stat in &mut custom_stats {
        *stat = r.u16()?;
    }
    let record = CompanionRecord {
        uid,
        species_id,
        nickname,
        level,
        experience,
        current_hp,
        max_hp,
        friendship,
        held_item,
        status_condition,
        nature_or_trait,
        shiny_or_rare,
        moves,
        custom_stats,
        owner_uuid,
    };
    record.validate().map_err(StorageDecodeError::InvalidRecord)?;
    Ok(record)
}

fn read_slot(r: &mut Reader<'_>) -> Result<Option<CompanionRecord>, StorageDecodeError> {
    match r.u8()? {
        0 => Ok(None),
        1 => read_record(r).map(Some),
        value => Err(StorageDecodeError::InvalidFlag {
            field: "slot_presence",
            value,
        }),
    }
}

/// Restore a storage produced by [`encode_storage`].
///
/// Magic is checked first, then the trailer CRC over every body byte —
/// a flipped bit anywhere is refused before any field is trusted. Every
/// count is capped before its loop or allocation runs, every flag is
/// validated, every record is re-checked against the storage invariants,
/// and the reader must land exactly on the end of the body: trailing
/// garbage is an error, never ignored.
pub fn decode_storage(bytes: &[u8]) -> Result<PlayerCompanionStorage, StorageDecodeError> {
    let body_len = bytes
        .len()
        .checked_sub(4)
        .ok_or(StorageDecodeError::Truncated)?;
    if body_len < PRTY_MAGIC.len() {
        return Err(StorageDecodeError::Truncated);
    }
    let (body, trailer) = bytes.split_at(body_len);
    if body[..PRTY_MAGIC.len()] != *PRTY_MAGIC {
        return Err(StorageDecodeError::BadMagic);
    }
    let expected = u32::from_be_bytes(
        trailer
            .try_into()
            .map_err(|_| StorageDecodeError::Truncated)?,
    );
    let actual = crc32(body);
    if actual != expected {
        return Err(StorageDecodeError::CrcMismatch { expected, actual });
    }

    let mut r = Reader {
        bytes: body,
        pos: PRTY_MAGIC.len(),
    };
    let schema = r.u32()?;
    if schema != PRTY_SCHEMA_VERSION {
        return Err(StorageDecodeError::UnsupportedSchemaVersion(schema));
    }
    let box_count = r.count("box_count", MAX_BOX_COUNT as u32)? as usize;

    let mut party: [Option<CompanionRecord>; PARTY_SLOTS] = std::array::from_fn(|_| None);
    for slot in party.iter_mut() {
        *slot = read_slot(&mut r)?;
    }
    let mut boxes = Vec::with_capacity(box_count);
    for _ in 0..box_count {
        let mut bx: [Option<CompanionRecord>; BOX_SLOTS] = std::array::from_fn(|_| None);
        for slot in bx.iter_mut() {
            *slot = read_slot(&mut r)?;
        }
        boxes.push(bx);
    }
    if r.pos != body.len() {
        return Err(StorageDecodeError::TrailingBytes {
            trailing: body.len() - r.pos,
        });
    }
    Ok(PlayerCompanionStorage { party, boxes })
}
