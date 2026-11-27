// SPDX-License-Identifier: LGPL-3.0-or-later
#![forbid(unsafe_code)]
//! `megu_core::ext_gate` — the lock-free per-seam extension gate.
//!
//! Every gameplay seam in the spine is a dispatch point that asks one
//! question on the hot path: *does anything extend this?* An unmodded
//! server or client answers "no" at every one of those points, thousands of
//! times per tick, and the naive shape — walk a handler list, call each
//! entry, let it decline — pays `O(handlers)` for a call whose answer is
//! always the same. `O(0)` is the only acceptable cost for a seam nobody
//! extends.
//!
//! [`ExtGate`] is that answer: **one** `AtomicU32` word, laid out as
//! `bit 0 = armed` and `bits 1..=31 = generation counter`.
//!
//! * [`ExtGate::is_armed`] is a single relaxed load plus a mask test. No
//!   lock, no allocation, no atomic read-modify-write, no branch that
//!   depends on anything but the word.
//! * [`ExtGate::arm`] is a CAS that bumps the generation *and* sets the
//!   armed bit, so every arming is observable as a distinct version.
//! * [`ExtGate::park_if_stable`] is the complementary CAS: it clears the
//!   armed bit **only** when the word is still the exact version the
//!   caller saw. A concurrent [`arm`](ExtGate::arm) changed the word, so
//!   the CAS fails and the gate stays armed. This is what makes "last
//!   deregistration parks the gate" safe against a registration that
//!   lands in between: a lost wake-up would silently disable a mod's
//!   hooks forever, and the version check makes it impossible.
//!
//! The generation counter is 31 bits, so it wraps after `2^31` arming
//! operations. A wrap is only harmful if an `arm`/`park_if_stable` pair
//! straddles exactly `2^31` arms, which no realistic workload reaches;
//! the packed word stays well-formed across the wrap either way, so the
//! only consequence would be one ignored park (the gate stays armed — the
//! safe direction).
//!
//! # Seams
//!
//! [`ExtSeam`] names every seam the spine exposes to extensions:
//! interaction, damage, spawning, block use/break/place/neighbour
//! updates, fluids, redstone, pistons, item use, recipe matching, loot,
//! villager trades, commands, portals, custom payloads and saved data.
//! [`SeamGateTable`] holds one [`ExtGate`] per seam and is
//! [`const`-constructible](SeamGateTable::new), so the whole table is a
//! plain `static` — no `OnceLock`, no lazy initialisation, no first-call
//! cost.
//!
//! # Deterministic ordering
//!
//! [`OrderedHookList`] is the dispatch structure behind an armed gate. It
//! orders handlers by `(priority, registration_index)`:
//!
//! * lower `priority` runs first, so a mod registering at `500` runs
//!   *before* Vanilla's own handler and can override it;
//! * [`VANILLA_PRIORITY`] (`1000`) is where the spine registers its own
//!   behaviour;
//! * a mod registering at `2000` runs *after* Vanilla — the fallback slot
//!   for "only if nobody else handled this";
//! * equal priorities run in registration order, which makes load order
//!   the only tie-break and keeps dispatch reproducible across runs.
//!
//! The list arms the gate on the first registration and parks it when the
//! last hook is removed, so the zero-idle property survives reloads.

use std::fmt;
use std::sync::atomic::{AtomicU32, Ordering};

/// Priority the spine's own (Vanilla-equivalent) behaviour registers at.
///
/// A lower priority dispatches earlier, so an extension registering at
/// `< 1000` runs before Vanilla's handler and can override it, while one
/// registering at `> 1000` runs afterwards as a fallback.
pub const VANILLA_PRIORITY: i32 = 1000;

/// Mask of the armed flag: bit 0 of the packed version word.
pub const ARMED_BIT: u32 = 0b1;

/// Mask of the generation counter: bits 1..=31 of the packed version word.
pub const GENERATION_MASK: u32 = u32::MAX >> 1;

/// Pack a `(generation, armed)` pair into the single gate word.
///
/// The generation is truncated to its low 31 bits, exactly as
/// [`ExtGate::arm`] keeps it.
#[inline]
#[must_use]
pub const fn pack(generation: u32, armed: bool) -> u32 {
    // A plain `if` rather than `u32::from(bool)`: `From` is not a const
    // trait, and this has to stay usable from a `const` constructor.
    ((generation & GENERATION_MASK) << 1) | if armed { ARMED_BIT } else { 0 }
}

/// Split a gate word back into `(generation, armed)`.
#[inline]
#[must_use]
pub const fn unpack(word: u32) -> (u32, bool) {
    (word >> 1, word & ARMED_BIT != 0)
}

/// A lock-free, per-seam "does anything extend this?" flag.
///
/// The type is exactly one `u32` wide (4 bytes) with no lock inside, so a
/// table of gates is a single cache-line-friendly array and the disarmed
/// query never leaves the loaded word.
#[derive(Debug)]
pub struct ExtGate(AtomicU32);

impl ExtGate {
    /// A parked gate: disarmed, generation `0`.
    #[inline]
    #[must_use]
    pub const fn new() -> Self {
        Self(AtomicU32::new(0))
    }

    /// `true` when an extension is registered on this seam.
    ///
    /// One relaxed load and a mask test: no lock, no allocation, no
    /// read-modify-write. Callers use it as the outermost `if` of a
    /// dispatch site, so an unextended seam costs one load per call.
    ///
    /// Relaxed is the right ordering here because the flag is advisory:
    /// it only gates work that re-checks its own state. A caller that must
    /// observe a handler list published by [`arm`](Self::arm) should
    /// publish that list behind its own release/acquire pair (for example
    /// a `Mutex` or `RwLock` around the registration side), not rely on
    /// the gate word for payload visibility.
    #[inline(always)]
    #[must_use]
    pub fn is_armed(&self) -> bool {
        self.0.load(Ordering::Relaxed) & ARMED_BIT != 0
    }

    /// The raw packed word: `(generation << 1) | armed`.
    ///
    /// Feed the value to [`park_if_stable`](Self::park_if_stable) after
    /// the work it guards, so a concurrent arming is detected instead of
    /// being overwritten.
    #[inline]
    #[must_use]
    pub fn snapshot(&self) -> u32 {
        self.0.load(Ordering::Relaxed)
    }

    /// The current generation counter (bits 1..=31 of the word).
    #[inline]
    #[must_use]
    pub fn generation(&self) -> u32 {
        unpack(self.snapshot()).0
    }

    /// Arm the seam, bumping the generation counter.
    ///
    /// A single CAS loop; concurrent armings each get their own
    /// generation, so the counter equals the number of `arm` calls that
    /// completed. Returns the packed word the gate now holds — pass it to
    /// [`park_if_stable`](Self::park_if_stable) to undo exactly this
    /// arming.
    ///
    /// The generation is bumped even when the gate is already armed: that
    /// is what invalidates an in-flight park, and it is why the counter is
    /// monotonic rather than merely "toggled".
    pub fn arm(&self) -> u32 {
        let mut observed = self.0.load(Ordering::Relaxed);
        loop {
            let (generation, _) = unpack(observed);
            let next = pack(generation.wrapping_add(1), true);
            match self
                .0
                .compare_exchange_weak(observed, next, Ordering::AcqRel, Ordering::Relaxed)
            {
                Ok(_) => return next,
                Err(actual) => observed = actual,
            }
        }
    }

    /// Park the seam, but only if nothing armed it since `seen_version`.
    ///
    /// The CAS succeeds only while the word is *exactly* the version the
    /// caller snapshotted; any concurrent [`arm`](Self::arm) bumped the
    /// generation and makes it fail. Returns `true` when the gate is now
    /// disarmed by this call.
    ///
    /// A `seen_version` that is already disarmed returns `false` without
    /// touching memory: there is nothing to park, and reporting `true`
    /// would let a stale observation mask a later registration.
    pub fn park_if_stable(&self, seen_version: u32) -> bool {
        if seen_version & ARMED_BIT == 0 {
            return false;
        }
        self.0
            .compare_exchange(
                seen_version,
                seen_version & !ARMED_BIT,
                Ordering::AcqRel,
                Ordering::Relaxed,
            )
            .is_ok()
    }

    /// Unconditionally disarm, preserving the generation.
    ///
    /// For loader reload paths that tear down every registration at once;
    /// it deliberately skips the version check, so concurrent registrations
    /// must be quiesced by the caller. Returns `true` when the gate was
    /// armed.
    pub fn disarm(&self) -> bool {
        let previous = self.0.fetch_and(!ARMED_BIT, Ordering::AcqRel);
        previous & ARMED_BIT != 0
    }
}

impl Default for ExtGate {
    fn default() -> Self {
        Self::new()
    }
}

/// Every gameplay seam the spine exposes to extensions.
///
/// The set is deliberately closed: a seam is a dispatch point the engine
/// owns, so extensions register *into* a seam rather than inventing new
/// entry points the tick loop does not know about.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum ExtSeam {
    /// One entity interacting with another entity or the player.
    EntityInteract,
    /// Damage taken or dealt, before and after armour/effects.
    EntityDamage,
    /// Entity (mob, projectile, item) creation.
    EntitySpawn,
    /// A block being right-clicked with an item or empty hand.
    BlockUse,
    /// A block being broken, including the "can this be broken" query.
    BlockBreak,
    /// A block being placed.
    BlockPlace,
    /// A neighbour update reaching a block.
    BlockNeighbor,
    /// A fluid scheduled tick or spread step.
    FluidTick,
    /// A redstone component update.
    RedstoneTick,
    /// A piston extending or retracting a move.
    PistonPush,
    /// An item being used (food, tools, buckets, custom behaviour).
    ItemUse,
    /// Recipe matching for crafting, smelting and machine inputs.
    RecipeMatch,
    /// A loot-table roll.
    LootRoll,
    /// A villager trade being listed, selected or applied.
    VillagerTrade,
    /// A command being dispatched or tab-completed.
    CommandDispatch,
    /// A dimension portal being entered or created.
    DimensionPortal,
    /// A custom payload (plugin channel) frame being sent or received.
    CustomPayload,
    /// Saved data (`data/*.dat`) being read or written.
    SavedData,
}

impl ExtSeam {
    /// Every seam, in declaration order. Index `i` of this array is
    /// [`index`](Self::index) `i`.
    pub const ALL: [Self; 18] = [
        Self::EntityInteract,
        Self::EntityDamage,
        Self::EntitySpawn,
        Self::BlockUse,
        Self::BlockBreak,
        Self::BlockPlace,
        Self::BlockNeighbor,
        Self::FluidTick,
        Self::RedstoneTick,
        Self::PistonPush,
        Self::ItemUse,
        Self::RecipeMatch,
        Self::LootRoll,
        Self::VillagerTrade,
        Self::CommandDispatch,
        Self::DimensionPortal,
        Self::CustomPayload,
        Self::SavedData,
    ];

    /// Number of seams — always `ALL.len()`.
    pub const COUNT: usize = Self::ALL.len();

    /// Dense index in `0..COUNT`, usable as an array offset.
    #[inline]
    #[must_use]
    pub const fn index(self) -> usize {
        self as usize
    }

    /// Stable lowercase name for logs, ledgers and operator commands.
    #[inline]
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::EntityInteract => "entity_interact",
            Self::EntityDamage => "entity_damage",
            Self::EntitySpawn => "entity_spawn",
            Self::BlockUse => "block_use",
            Self::BlockBreak => "block_break",
            Self::BlockPlace => "block_place",
            Self::BlockNeighbor => "block_neighbor",
            Self::FluidTick => "fluid_tick",
            Self::RedstoneTick => "redstone_tick",
            Self::PistonPush => "piston_push",
            Self::ItemUse => "item_use",
            Self::RecipeMatch => "recipe_match",
            Self::LootRoll => "loot_roll",
            Self::VillagerTrade => "villager_trade",
            Self::CommandDispatch => "command_dispatch",
            Self::DimensionPortal => "dimension_portal",
            Self::CustomPayload => "custom_payload",
            Self::SavedData => "saved_data",
        }
    }

    /// The seam at `index`, or `None` when out of range.
    #[inline]
    #[must_use]
    pub const fn from_index(index: usize) -> Option<Self> {
        if index < Self::COUNT {
            Some(Self::ALL[index])
        } else {
            None
        }
    }
}

impl fmt::Display for ExtSeam {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One [`ExtGate`] per [`ExtSeam`], const-constructible for `static` use.
///
/// ```no_run
/// use megu_core::ext_gate::{ExtSeam, SeamGateTable};
///
/// static SEAMS: SeamGateTable = SeamGateTable::new();
///
/// fn any_extension_registered() -> bool {
///     SEAMS.any_armed()
/// }
/// # let _ = any_extension_registered();
/// # let _ = ExtSeam::BlockUse;
/// ```
#[derive(Debug)]
pub struct SeamGateTable {
    gates: [ExtGate; ExtSeam::COUNT],
}

impl SeamGateTable {
    /// A table with every seam parked. `const`, so it can initialise a
    /// `static` without any lazy-initialisation machinery.
    #[inline]
    #[must_use]
    pub const fn new() -> Self {
        Self {
            gates: [const { ExtGate::new() }; ExtSeam::COUNT],
        }
    }

    /// The gate for one seam.
    #[inline]
    #[must_use]
    pub fn gate(&self, seam: ExtSeam) -> &ExtGate {
        &self.gates[seam.index()]
    }

    /// Whether anything is registered on `seam` (one relaxed load).
    #[inline]
    #[must_use]
    pub fn is_armed(&self, seam: ExtSeam) -> bool {
        self.gates[seam.index()].is_armed()
    }

    /// Whether *any* seam has an extension — the single check a subsystem
    /// can do to skip a whole family of dispatch sites.
    #[inline]
    #[must_use]
    pub fn any_armed(&self) -> bool {
        self.gates.iter().any(ExtGate::is_armed)
    }

    /// How many seams are armed.
    #[inline]
    #[must_use]
    pub fn armed_count(&self) -> usize {
        self.gates.iter().filter(|gate| gate.is_armed()).count()
    }

    /// Arm one seam, returning the new packed version word.
    #[inline]
    pub fn arm(&self, seam: ExtSeam) -> u32 {
        self.gates[seam.index()].arm()
    }

    /// The packed version word of every seam, in [`ExtSeam::ALL`] order.
    #[must_use]
    pub fn snapshot(&self) -> [u32; ExtSeam::COUNT] {
        std::array::from_fn(|index| self.gates[index].snapshot())
    }

    /// Disarm every seam (loader reload). Returns how many were armed.
    pub fn disarm_all(&self) -> usize {
        self.gates.iter().filter(|gate| gate.disarm()).count()
    }
}

impl Default for SeamGateTable {
    fn default() -> Self {
        Self::new()
    }
}

/// One registered handler plus its deterministic dispatch key.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct HookSlot<H> {
    priority: i32,
    index: u64,
    hook: H,
}

impl<H> HookSlot<H> {
    /// Dispatch priority; lower runs earlier.
    #[inline]
    #[must_use]
    pub const fn priority(&self) -> i32 {
        self.priority
    }

    /// Registration index, unique per [`OrderedHookList`] and monotonic.
    #[inline]
    #[must_use]
    pub const fn index(&self) -> u64 {
        self.index
    }

    /// The handler itself.
    #[inline]
    #[must_use]
    pub const fn hook(&self) -> &H {
        &self.hook
    }

    /// Consume the slot, yielding the handler.
    #[inline]
    pub fn into_hook(self) -> H {
        self.hook
    }
}

/// Why a hook could not be registered.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HookListFull {
    max_hooks: usize,
}

impl HookListFull {
    /// The cap that refused the registration.
    #[inline]
    #[must_use]
    pub const fn new(max_hooks: usize) -> Self {
        Self { max_hooks }
    }

    /// The configured cap.
    #[inline]
    #[must_use]
    pub const fn max_hooks(&self) -> usize {
        self.max_hooks
    }
}

impl fmt::Display for HookListFull {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "hook list is full: {} handler(s) is the configured cap",
            self.max_hooks
        )
    }
}

impl std::error::Error for HookListFull {}

/// How a hook list's registrations split around [`VANILLA_PRIORITY`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PriorityBuckets {
    /// Handlers registered below [`VANILLA_PRIORITY`]: they run first and
    /// can pre-empt Vanilla.
    pub before_vanilla: usize,
    /// Handlers registered exactly at [`VANILLA_PRIORITY`].
    pub at_vanilla: usize,
    /// Handlers registered above [`VANILLA_PRIORITY`]: fallbacks.
    pub after_vanilla: usize,
}

impl PriorityBuckets {
    /// Total handlers across all three buckets.
    #[inline]
    #[must_use]
    pub const fn total(&self) -> usize {
        self.before_vanilla + self.at_vanilla + self.after_vanilla
    }
}

/// A dispatch list of handlers ordered by `(priority, registration_index)`,
/// bound to the [`ExtGate`] that says whether it must be consulted at all.
///
/// Registration is bounded by `max_hooks` and arms the gate; removing the
/// last handler parks it again, so a seam with no extensions returns to
/// the one-load fast path after a mod unload.
#[derive(Debug)]
pub struct OrderedHookList<'a, H> {
    gate: &'a ExtGate,
    hooks: Vec<HookSlot<H>>,
    next_index: u64,
    max_hooks: usize,
}

impl<'a, H> OrderedHookList<'a, H> {
    /// Empty list dispatching through `gate`, bounded at `max_hooks`.
    ///
    /// `max_hooks == 0` makes the list permanently unusable — every
    /// registration is refused and the gate is never armed — which is the
    /// right shape for a build that compiled the seam out.
    #[must_use]
    pub fn new(gate: &'a ExtGate, max_hooks: usize) -> Self {
        Self {
            gate,
            hooks: Vec::new(),
            next_index: 0,
            max_hooks,
        }
    }

    /// `true` when at least one handler is registered on the seam.
    ///
    /// One relaxed load; check this before doing any dispatch setup.
    #[inline(always)]
    #[must_use]
    pub fn is_armed(&self) -> bool {
        self.gate.is_armed()
    }

    /// The gate this list dispatches through.
    #[inline]
    #[must_use]
    pub fn gate(&self) -> &'a ExtGate {
        self.gate
    }

    /// The configured handler cap.
    #[inline]
    #[must_use]
    pub fn max_hooks(&self) -> usize {
        self.max_hooks
    }

    /// Number of live handlers.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.hooks.len()
    }

    /// `true` when no handler is registered.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    /// Handlers in dispatch order.
    #[inline]
    #[must_use]
    pub fn slots(&self) -> &[HookSlot<H>] {
        &self.hooks
    }

    /// Handlers in dispatch order, without their keys.
    pub fn hooks(&self) -> impl Iterator<Item = &H> {
        self.hooks.iter().map(HookSlot::hook)
    }

    /// Register `hook` at `priority`, returning its registration index.
    ///
    /// # Errors
    ///
    /// Fails closed at `max_hooks`; nothing is evicted and the gate is not
    /// armed by a refused registration.
    pub fn register(&mut self, priority: i32, hook: H) -> Result<u64, HookListFull> {
        if self.hooks.len() >= self.max_hooks {
            return Err(HookListFull::new(self.max_hooks));
        }
        let index = self.next_index;
        self.next_index = self.next_index.wrapping_add(1);
        let key = (priority, index);
        let position = self
            .hooks
            .partition_point(|slot| (slot.priority, slot.index) < key);
        self.hooks.insert(
            position,
            HookSlot {
                priority,
                index,
                hook,
            },
        );
        self.gate.arm();
        Ok(index)
    }

    /// Remove the handler registered as `handle` (the value
    /// [`register`](Self::register) returned), parking the gate when the
    /// list empties.
    pub fn remove(&mut self, handle: u64) -> Option<H> {
        let position = self.hooks.iter().position(|slot| slot.index == handle)?;
        let slot = self.hooks.remove(position);
        if self.hooks.is_empty() {
            self.park();
        }
        Some(slot.hook)
    }

    /// Drop every handler, parking the gate. Returns how many were
    /// removed.
    pub fn clear(&mut self) -> usize {
        let removed = self.hooks.len();
        self.hooks.clear();
        self.park();
        removed
    }

    /// Park the gate if no registration raced with this call.
    ///
    /// Returns `true` when the gate is now disarmed. A registration that
    /// lands between the list emptying and this CAS keeps the gate armed,
    /// which is the safe direction: the list is consulted again and finds
    /// the new handler.
    pub fn park(&self) -> bool {
        self.gate.park_if_stable(self.gate.snapshot())
    }

    /// Count handlers below, at, and above [`VANILLA_PRIORITY`].
    #[must_use]
    pub fn priority_buckets(&self) -> PriorityBuckets {
        let mut buckets = PriorityBuckets::default();
        for slot in &self.hooks {
            match slot.priority.cmp(&VANILLA_PRIORITY) {
                std::cmp::Ordering::Less => buckets.before_vanilla += 1,
                std::cmp::Ordering::Equal => buckets.at_vanilla += 1,
                std::cmp::Ordering::Greater => buckets.after_vanilla += 1,
            }
        }
        buckets
    }

    /// Run `visit` over the handlers in dispatch order until one returns
    /// `Some`, yielding that value.
    ///
    /// A disarmed gate returns `None` without touching the list, which is
    /// the whole point of the type: a seam nobody extends costs one
    /// relaxed load.
    pub fn dispatch<R>(&self, mut visit: impl FnMut(&H) -> Option<R>) -> Option<R> {
        if !self.gate.is_armed() {
            return None;
        }
        self.hooks.iter().find_map(|slot| visit(&slot.hook))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::size_of;
    use std::sync::Arc;

    #[test]
    fn a_new_gate_is_parked_with_generation_zero() {
        let gate = ExtGate::new();
        assert!(!gate.is_armed());
        assert_eq!(gate.snapshot(), 0);
        assert_eq!(gate.generation(), 0);
        assert_eq!(unpack(0), (0, false));
        assert_eq!(ExtGate::default().snapshot(), 0);
    }

    #[test]
    fn the_gate_is_one_word_with_no_lock_inside() {
        // The zero-idle claim rests on the gate being a single atomic word:
        // a lock (or any second field) would show up here.
        assert_eq!(size_of::<ExtGate>(), 4);
        assert_eq!(size_of::<ExtGate>(), size_of::<u32>());
    }

    #[test]
    fn is_armed_is_a_pure_read_that_never_mutates() {
        let gate = ExtGate::new();
        for _ in 0..100_000 {
            assert!(!gate.is_armed());
        }
        // A read-only query leaves the word untouched, so a parked gate
        // stays parked no matter how often the fast path is probed.
        assert_eq!(gate.snapshot(), 0);
        gate.arm();
        for _ in 0..100_000 {
            assert!(gate.is_armed());
        }
        assert_eq!(gate.snapshot(), pack(1, true));
    }

    #[test]
    fn arming_sets_the_bit_and_advances_the_generation() {
        let gate = ExtGate::new();
        assert_eq!(gate.arm(), pack(1, true));
        assert!(gate.is_armed());
        assert_eq!(gate.generation(), 1);
        assert_eq!(gate.arm(), pack(2, true));
        assert_eq!(gate.arm(), pack(3, true));
        assert_eq!(gate.generation(), 3);
        assert_eq!(gate.snapshot(), 0b111);
    }

    #[test]
    fn arming_an_armed_gate_still_advances_the_generation() {
        // The generation must move on every arming, otherwise an in-flight
        // park could mistake a re-arm for the state it snapshotted.
        let gate = ExtGate::new();
        let first = gate.arm();
        let second = gate.arm();
        assert_ne!(first, second);
        assert_eq!(unpack(second).0, unpack(first).0 + 1);
    }

    #[test]
    fn park_if_stable_round_trips_its_own_arming() {
        let gate = ExtGate::new();
        let version = gate.arm();
        assert!(gate.park_if_stable(version));
        assert!(!gate.is_armed());
        // Same generation, armed bit cleared: a later arm continues the
        // sequence instead of restarting it.
        assert_eq!(gate.snapshot(), pack(1, false));
        assert_eq!(gate.generation(), 1);
        assert_eq!(gate.arm(), pack(2, true));
    }

    #[test]
    fn a_stale_park_never_clears_a_newer_arm() {
        let gate = ExtGate::new();
        let stale = gate.arm();
        gate.arm();
        assert!(!gate.park_if_stable(stale));
        assert!(
            gate.is_armed(),
            "a concurrent arm must survive a stale park"
        );
        assert_eq!(gate.generation(), 2);
    }

    #[test]
    fn parking_a_disarmed_gate_reports_false() {
        let gate = ExtGate::new();
        // Nothing was armed, so there is nothing to park and the CAS must
        // not be reached at all: reporting `true` would let a stale
        // observation hide the next registration.
        assert!(!gate.park_if_stable(0));
        assert!(!gate.park_if_stable(pack(7, false)));
        assert_eq!(gate.snapshot(), 0);
    }

    #[test]
    fn disarm_is_unconditional_and_preserves_the_generation() {
        let gate = ExtGate::new();
        assert!(!gate.disarm());
        gate.arm();
        gate.arm();
        assert!(gate.disarm());
        assert!(!gate.is_armed());
        assert_eq!(gate.generation(), 2);
        assert!(!gate.disarm());
    }

    #[test]
    fn the_packed_word_layout_matches_the_documented_bit_placement() {
        assert_eq!(ARMED_BIT, 0b1);
        assert_eq!(GENERATION_MASK, 0x7fff_ffff);
        assert_eq!(pack(0, false), 0);
        assert_eq!(pack(0, true), 0b1);
        assert_eq!(pack(1, false), 0b10);
        assert_eq!(pack(1, true), 0b11);
        assert_eq!(pack(GENERATION_MASK, true), u32::MAX);
        assert_eq!(unpack(u32::MAX), (GENERATION_MASK, true));
        // Everything above bit 0 is generation, so no generation can spill
        // into the armed flag.
        assert_eq!(pack(u32::MAX, true), u32::MAX);
        assert!(unpack(pack(u32::MAX, true)).1);
    }

    #[test]
    fn the_generation_wraps_without_disturbing_the_armed_flag() {
        // Reaching the wrap in a test would take 2^31 armings; the packing
        // is what decides it, so pin the arithmetic at the boundary.
        let top = pack(GENERATION_MASK, true);
        let (generation, armed) = unpack(top);
        assert!(armed);
        assert_eq!(generation, GENERATION_MASK);
        let wrapped = pack(generation.wrapping_add(1), true);
        assert_eq!(unpack(wrapped), (0, true));
        assert!(unpack(wrapped).1, "the wrap must never clear the armed bit");
    }

    #[test]
    fn concurrent_armings_never_lose_a_generation() {
        const THREADS: u32 = 4;
        const PER_THREAD: u32 = 2_000;
        let gate = Arc::new(ExtGate::new());
        let mut handles = Vec::new();
        for _ in 0..THREADS {
            let gate = Arc::clone(&gate);
            handles.push(std::thread::spawn(move || {
                for _ in 0..PER_THREAD {
                    gate.arm();
                }
            }));
        }
        for handle in handles {
            handle.join().expect("armer thread panicked");
        }
        // Joining synchronises, so the final word is fully visible here.
        let (generation, armed) = unpack(gate.snapshot());
        assert!(armed);
        assert_eq!(generation, THREADS * PER_THREAD);
    }

    #[test]
    fn a_park_racing_an_arm_can_never_lose_the_registration() {
        const PER_THREAD: u32 = 1_000;
        const THREADS: u32 = 4;
        let gate = Arc::new(ExtGate::new());
        let mut handles = Vec::new();
        for _ in 0..THREADS {
            let gate = Arc::clone(&gate);
            handles.push(std::thread::spawn(move || {
                for _ in 0..PER_THREAD {
                    gate.arm();
                }
            }));
        }
        // Park from this thread while the armers run: every attempt either
        // wins against an unchanged word or is rejected by the version
        // check. What it must never do is erase an arming.
        for _ in 0..PER_THREAD {
            let seen = gate.snapshot();
            gate.park_if_stable(seen);
        }
        for handle in handles {
            handle.join().expect("armer thread panicked");
        }
        let final_word = gate.arm();
        assert!(gate.is_armed());
        // Every arming counted exactly once; the final arm is the +1.
        assert_eq!(unpack(final_word).0, THREADS * PER_THREAD + 1);
    }

    #[test]
    fn every_seam_has_a_unique_dense_index_and_name() {
        assert_eq!(ExtSeam::COUNT, 18);
        assert_eq!(ExtSeam::ALL.len(), ExtSeam::COUNT);
        let mut names = std::collections::BTreeSet::new();
        for (index, seam) in ExtSeam::ALL.iter().enumerate() {
            assert_eq!(seam.index(), index, "{seam:?}");
            assert_eq!(ExtSeam::from_index(index), Some(*seam));
            assert!(names.insert(seam.as_str()), "duplicate name {seam}");
            assert_eq!(seam.to_string(), seam.as_str());
            assert_eq!(seam.as_str(), seam.as_str().to_lowercase());
        }
        assert_eq!(ExtSeam::from_index(ExtSeam::COUNT), None);
        // Declaration order is the index order, so a table offset and an
        // `as_str` lookup can never disagree.
        assert_eq!(ExtSeam::ALL[0], ExtSeam::EntityInteract);
        assert_eq!(ExtSeam::ALL[ExtSeam::COUNT - 1], ExtSeam::SavedData);
    }

    #[test]
    fn the_name_is_stable_for_every_seam() {
        let expected = [
            (ExtSeam::EntityInteract, "entity_interact"),
            (ExtSeam::EntityDamage, "entity_damage"),
            (ExtSeam::EntitySpawn, "entity_spawn"),
            (ExtSeam::BlockUse, "block_use"),
            (ExtSeam::BlockBreak, "block_break"),
            (ExtSeam::BlockPlace, "block_place"),
            (ExtSeam::BlockNeighbor, "block_neighbor"),
            (ExtSeam::FluidTick, "fluid_tick"),
            (ExtSeam::RedstoneTick, "redstone_tick"),
            (ExtSeam::PistonPush, "piston_push"),
            (ExtSeam::ItemUse, "item_use"),
            (ExtSeam::RecipeMatch, "recipe_match"),
            (ExtSeam::LootRoll, "loot_roll"),
            (ExtSeam::VillagerTrade, "villager_trade"),
            (ExtSeam::CommandDispatch, "command_dispatch"),
            (ExtSeam::DimensionPortal, "dimension_portal"),
            (ExtSeam::CustomPayload, "custom_payload"),
            (ExtSeam::SavedData, "saved_data"),
        ];
        for (seam, name) in expected {
            assert_eq!(seam.as_str(), name);
        }
    }

    #[test]
    fn a_seam_gate_table_is_const_constructible_for_static_use() {
        // A `static` table is the deployment shape: no lazy init, no
        // first-call cost.
        static SEAMS: SeamGateTable = SeamGateTable::new();
        assert_eq!(SEAMS.armed_count(), 0);
        assert!(!SEAMS.any_armed());
        assert!(SEAMS.snapshot().iter().all(|word| *word == 0));
        assert_eq!(SEAMS.arm(ExtSeam::BlockUse), pack(1, true));
        assert!(SEAMS.is_armed(ExtSeam::BlockUse));
        assert!(!SEAMS.is_armed(ExtSeam::BlockBreak));
        assert!(SEAMS.any_armed());
        assert_eq!(SEAMS.armed_count(), 1);
        assert_eq!(SEAMS.disarm_all(), 1);
        assert_eq!(SEAMS.armed_count(), 0);
    }

    #[test]
    fn arming_one_seam_leaves_every_other_seam_parked() {
        let table = SeamGateTable::new();
        for seam in ExtSeam::ALL {
            table.arm(seam);
        }
        assert_eq!(table.armed_count(), ExtSeam::COUNT);
        for seam in ExtSeam::ALL {
            assert!(table.is_armed(seam), "{seam}");
        }
        table.gate(ExtSeam::LootRoll).disarm();
        assert!(!table.is_armed(ExtSeam::LootRoll));
        assert_eq!(table.armed_count(), ExtSeam::COUNT - 1);
        let snapshot = table.snapshot();
        assert_eq!(snapshot[ExtSeam::LootRoll.index()], pack(1, false));
        assert_eq!(snapshot[ExtSeam::BlockUse.index()], pack(1, true));
    }

    #[test]
    fn a_hook_list_starts_parked_and_arms_on_first_registration() {
        let gate = ExtGate::new();
        let mut list: OrderedHookList<&'static str> = OrderedHookList::new(&gate, 8);
        assert!(!list.is_armed());
        assert!(list.is_empty());
        assert_eq!(list.len(), 0);
        assert_eq!(list.max_hooks(), 8);
        assert!(!gate.is_armed());

        let handle = list.register(VANILLA_PRIORITY, "vanilla").unwrap();
        assert!(list.is_armed());
        assert!(gate.is_armed());
        assert_eq!(list.len(), 1);
        assert_eq!(handle, 0);

        assert_eq!(list.remove(handle), Some("vanilla"));
        assert!(list.is_empty());
        assert!(!list.is_armed(), "the last removal must park the gate");
        assert!(!gate.is_armed());
        assert_eq!(list.remove(handle), None);
    }

    #[test]
    fn hooks_dispatch_before_vanilla_then_vanilla_then_fallbacks() {
        let gate = ExtGate::new();
        let mut list: OrderedHookList<&'static str> = OrderedHookList::new(&gate, 16);
        // Registration order deliberately disagrees with dispatch order.
        list.register(VANILLA_PRIORITY + 500, "fallback").unwrap();
        list.register(VANILLA_PRIORITY, "vanilla").unwrap();
        list.register(VANILLA_PRIORITY - 500, "override").unwrap();
        let order: Vec<&str> = list.hooks().copied().collect();
        assert_eq!(order, ["override", "vanilla", "fallback"]);
        assert_eq!(VANILLA_PRIORITY, 1000);
        let buckets = list.priority_buckets();
        assert_eq!(buckets.before_vanilla, 1);
        assert_eq!(buckets.at_vanilla, 1);
        assert_eq!(buckets.after_vanilla, 1);
        assert_eq!(buckets.total(), 3);
    }

    #[test]
    fn equal_priorities_dispatch_in_registration_order() {
        let gate = ExtGate::new();
        let mut list: OrderedHookList<u32> = OrderedHookList::new(&gate, 8);
        for tag in [7u32, 3, 9] {
            list.register(VANILLA_PRIORITY, tag).unwrap();
        }
        let order: Vec<u32> = list.hooks().copied().collect();
        // The `(priority, registration_index)` key makes this total order
        // reproducible instead of dependent on the sort's tie-break.
        assert_eq!(order, [7, 3, 9]);
        let indices: Vec<u64> = list.slots().iter().map(HookSlot::index).collect();
        assert_eq!(indices, [0, 1, 2]);
        assert_eq!(list.slots()[0].priority(), VANILLA_PRIORITY);
    }

    #[test]
    fn dispatch_returns_the_first_handler_that_answers() {
        let gate = ExtGate::new();
        let mut list: OrderedHookList<u8> = OrderedHookList::new(&gate, 8);
        list.register(VANILLA_PRIORITY + 100, 40).unwrap();
        list.register(VANILLA_PRIORITY - 100, 10).unwrap();
        list.register(VANILLA_PRIORITY, 20).unwrap();
        let mut seen = Vec::new();
        let first = list.dispatch(|value| {
            seen.push(*value);
            (*value >= 20).then_some(*value)
        });
        assert_eq!(first, Some(20));
        assert_eq!(seen, [10, 20], "dispatch stops at the first answer");
        assert_eq!(list.dispatch(|_| None::<u8>), None);
    }

    #[test]
    fn a_parked_list_dispatch_is_a_single_load_with_no_work() {
        let gate = ExtGate::new();
        let list: OrderedHookList<&'static str> = OrderedHookList::new(&gate, 8);
        let answer = list.dispatch(|_| -> Option<u32> {
            panic!("a parked seam must never consult its handlers")
        });
        assert_eq!(answer, None);
        assert!(!list.is_armed());
    }

    #[test]
    fn registration_fails_closed_at_the_cap() {
        let gate = ExtGate::new();
        let mut list: OrderedHookList<u8> = OrderedHookList::new(&gate, 2);
        assert!(list.register(VANILLA_PRIORITY, 1).is_ok());
        assert!(list.register(VANILLA_PRIORITY, 2).is_ok());
        let error = list.register(VANILLA_PRIORITY, 3).unwrap_err();
        assert_eq!(error.max_hooks(), 2);
        assert!(error.to_string().contains('2'));
        assert_eq!(list.len(), 2);
        assert_eq!(list.hooks().copied().collect::<Vec<u8>>(), [1, 2]);
    }

    #[test]
    fn a_zero_cap_list_never_arms_the_gate() {
        let gate = ExtGate::new();
        let mut list: OrderedHookList<u8> = OrderedHookList::new(&gate, 0);
        assert_eq!(
            list.register(VANILLA_PRIORITY, 1).unwrap_err(),
            HookListFull::new(0)
        );
        assert!(list.is_empty());
        assert!(!gate.is_armed());
        assert_eq!(list.dispatch(|_| Some(1u8)), None);
    }

    #[test]
    fn clear_parks_the_gate_and_reports_the_removed_count() {
        let gate = ExtGate::new();
        let mut list: OrderedHookList<u8> = OrderedHookList::new(&gate, 8);
        for tag in 0..4u8 {
            list.register(VANILLA_PRIORITY, tag).unwrap();
        }
        assert_eq!(list.clear(), 4);
        assert!(list.is_empty());
        assert!(!gate.is_armed());
        // Registering again after a clear continues the index sequence, so
        // a handle from the previous epoch can never be confused with a
        // live one.
        assert_eq!(list.register(VANILLA_PRIORITY, 9).unwrap(), 4);
        assert!(gate.is_armed());
    }

    #[test]
    fn a_park_at_a_stale_snapshot_never_hides_a_racing_registration() {
        let gate = ExtGate::new();
        let mut list: OrderedHookList<u8> = OrderedHookList::new(&gate, 8);
        let handle = list.register(VANILLA_PRIORITY, 1).unwrap();
        // The interleaving that would lose a hook: the emptying list
        // captures the version, a registration arms the gate, and only
        // then does the park CAS run.
        let seen = gate.snapshot();
        gate.arm();
        assert!(!gate.park_if_stable(seen));
        assert!(
            list.is_armed(),
            "a parked gate must never hide a racing registration"
        );
        assert_eq!(list.dispatch(|value| Some(*value)), Some(1));
        // With no race, the list parks itself once it empties again: the
        // snapshot is taken immediately before the CAS, so the window is
        // only the concurrent window that exists in reality.
        list.remove(handle).unwrap();
        assert!(!list.is_armed());
    }
}
