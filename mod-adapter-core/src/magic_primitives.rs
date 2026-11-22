// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Clean-room magic-mod gameplay primitives — pure `std`, zero new
//! dependencies, 100% declarative data, and entirely deterministic tick
//! evaluation (no wall-clock `SystemTime` anywhere).
//!
//! Six families cover the core contract shared by generic spell/magic mods:
//!
//! 1. [`ManaPool`]/[`MagicAttributes`]: a finite-float, NaN-safe, saturating
//!    mana resource pool with per-school power scaling and clamped
//!    cooldown/cast-time reduction attributes.
//! 2. [`SpellCastState`]/[`CooldownTracker`]/[`SpellCaster`]: instant,
//!    cast-time, and channeled casting with damage/move interrupts,
//!    per-spell cooldowns, and a global cooldown — all tick-driven.
//! 3. [`SpellDefinition`]/[`SpellForm`]/[`SpellEffect`]: a composable spell
//!    delivery graph (self, touch, projectile, beam, AoE zone, chain, orbit,
//!    summon) with fail-closed safety caps on every unbounded dimension.
//! 4. [`CustomStatusRegistry`]/[`ActiveStatusEffects`]: data-driven custom
//!    status effects — DoT/HoT pulses, absorption shields, crowd-control
//!    flags, attribute modifiers, and VFX aura tags — with a deterministic
//!    [`ActiveStatusEffects::tick`] event report.
//! 5. [`RitualRecipe`]/[`RitualRuntime`]/[`ManaNetwork`]: multiblock altar
//!    pattern verification, atomic reagent consumption, mana-drained ritual
//!    progress, and a bounded cycle-safe mana transfer network.
//! 6. `MAG1` wire codec on channel [`MAGIC_CHANNEL`]: deterministic binary
//!    sync of mana, cast progress, cooldowns, and active custom statuses
//!    with strict length caps, canonical-order enforcement, NaN rejection,
//!    and trailing-bytes rejection.
//!
//! Clean-room note: every rule, cap, and wire layout here is an original
//! declarative design — no code, constants, or data tables are ported from
//! any third-party magic mod.

use std::collections::BTreeMap;

use crate::neoforge_wire::{read_mc_string, read_varint, write_mc_string, write_varint};

// ===========================================================================
// Shared bounds
// ===========================================================================

/// Maximum length of any identifier string (spell id, status id, block id,
/// entity type, VFX id) accepted anywhere in this module.
pub const MAX_ID_LEN: usize = 128;

/// Largest `cooldown_reduction` / `cast_time_reduction` allowed — an 80%
/// reduction keeps every cooldown and cast time strictly positive.
pub const MAX_REDUCTION: f32 = 0.8;

fn finite(v: f32) -> bool {
    v.is_finite()
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_ID_LEN
}

/// Clamp `v` into `0.0..=MAX_REDUCTION`; non-finite input clamps to `0.0`.
fn clamp_reduction(v: f32) -> f32 {
    if finite(v) {
        v.clamp(0.0, MAX_REDUCTION)
    } else {
        0.0
    }
}

/// Sanitize a mutable float in place: non-finite becomes `0.0`, negatives
/// become `0.0` where `allow_negative` is false.
fn sanitize_non_negative(v: &mut f32) {
    if !v.is_finite() || *v < 0.0 {
        *v = 0.0;
    }
}

// ===========================================================================
// 1. Mana / resource pool
// ===========================================================================

/// Declarative caster attributes. Floats are sanitized by
/// [`MagicAttributes::sanitize`]: every stored value is finite, reductions
/// are clamped into `0.0..=0.8`, and non-finite school powers are dropped.
#[derive(Clone, Debug, PartialEq)]
pub struct MagicAttributes {
    /// Upper bound of the mana pool. `0.0` disables mana use entirely.
    pub max_mana: f32,
    /// Mana restored per server tick by [`ManaPool::tick_regen`].
    pub regen_per_tick: f32,
    /// Global power multiplier for scalable effects (`1.0` = baseline).
    pub spell_power: f32,
    /// Fractional cooldown reduction, clamped to `0.0..=0.8`.
    pub cooldown_reduction: f32,
    /// Fractional cast-time reduction, clamped to `0.0..=0.8`.
    pub cast_time_reduction: f32,
    /// Additive per-school power bonuses on top of [`Self::spell_power`].
    pub school_power: BTreeMap<String, f32>,
}

impl Default for MagicAttributes {
    fn default() -> Self {
        Self {
            max_mana: 100.0,
            regen_per_tick: 0.0,
            spell_power: 1.0,
            cooldown_reduction: 0.0,
            cast_time_reduction: 0.0,
            school_power: BTreeMap::new(),
        }
    }
}

impl MagicAttributes {
    /// Clamp reductions into `0.0..=0.8`, force every scalar finite and
    /// non-negative (except `spell_power`/school powers, which may not go
    /// below `0.0` either), and drop non-finite school-power entries.
    pub fn sanitize(&mut self) {
        sanitize_non_negative(&mut self.max_mana);
        sanitize_non_negative(&mut self.regen_per_tick);
        if !self.spell_power.is_finite() {
            self.spell_power = 1.0;
        }
        self.spell_power = self.spell_power.max(0.0);
        self.cooldown_reduction = clamp_reduction(self.cooldown_reduction);
        self.cast_time_reduction = clamp_reduction(self.cast_time_reduction);
        self.school_power.retain(|_, v| v.is_finite() && *v >= 0.0);
    }

    /// Sanitized copy of `self`.
    pub fn sanitized(mut self) -> Self {
        self.sanitize();
        self
    }

    /// Combined power multiplier for `school`: `spell_power` plus the
    /// per-school bonus, floored at `0.0`. Always finite post-`sanitize`.
    pub fn effective_school_power(&self, school: &str) -> f32 {
        let bonus = self.school_power.get(school).copied().unwrap_or(0.0);
        (self.spell_power + bonus).max(0.0)
    }

    /// Scale `base_ticks` by `cast_time_reduction` (round to nearest).
    pub fn reduced_cast_ticks(&self, base_ticks: u32) -> u32 {
        let scale = 1.0 - clamp_reduction(self.cast_time_reduction);
        (base_ticks as f32 * scale).round().max(0.0) as u32
    }
}

/// A live mana pool backed by [`MagicAttributes`]. All mutators are
/// saturating and NaN-safe: a non-finite amount is always a no-op (or a
/// failed spend), never a panic or an infinite resource.
#[derive(Clone, Debug, PartialEq)]
pub struct ManaPool {
    /// Caster attributes; `max_mana`/`regen_per_tick` drive this pool.
    pub attributes: MagicAttributes,
    /// Current stored mana, always within `0.0..=attributes.max_mana`.
    pub current_mana: f32,
}

impl ManaPool {
    /// Pool filled to `max_mana` after attribute sanitization.
    pub fn new(attributes: MagicAttributes) -> Self {
        let attributes = attributes.sanitized();
        let current_mana = attributes.max_mana;
        Self {
            attributes,
            current_mana,
        }
    }

    /// Pool starting at `current` (clamped into `0.0..=max_mana`).
    pub fn with_mana(attributes: MagicAttributes, current: f32) -> Self {
        let attributes = attributes.sanitized();
        let current_mana = if current.is_finite() {
            current.clamp(0.0, attributes.max_mana)
        } else {
            0.0
        };
        Self {
            attributes,
            current_mana,
        }
    }

    /// `true` when `cost` is finite, non-negative, and affordable.
    pub fn has(&self, cost: f32) -> bool {
        cost.is_finite() && cost >= 0.0 && self.current_mana >= cost
    }

    /// Deterministic per-tick regeneration; saturates at `max_mana`.
    pub fn tick_regen(&mut self) {
        if self.attributes.regen_per_tick > 0.0 {
            self.current_mana =
                (self.current_mana + self.attributes.regen_per_tick).min(self.attributes.max_mana);
        }
    }

    /// Spend `cost` mana; returns `false` (and spends nothing) when `cost`
    /// is non-finite, negative, or exceeds `current_mana`.
    pub fn try_spend(&mut self, cost: f32) -> bool {
        if !self.has(cost) {
            return false;
        }
        self.current_mana -= cost;
        // Floating-point guard: never let a spend leave a negative residue.
        if self.current_mana < 0.0 {
            self.current_mana = 0.0;
        }
        true
    }

    /// Restore up to `amount` mana; returns the amount actually gained.
    /// Non-finite or negative input is a no-op returning `0.0`.
    pub fn restore(&mut self, amount: f32) -> f32 {
        if !amount.is_finite() || amount <= 0.0 {
            return 0.0;
        }
        let before = self.current_mana;
        self.current_mana = (self.current_mana + amount).min(self.attributes.max_mana);
        self.current_mana - before
    }

    /// Overwrite `current_mana` (e.g. from a wire sync), clamped and
    /// NaN-safe.
    pub fn set_current(&mut self, current: f32) {
        self.current_mana = if current.is_finite() {
            current.clamp(0.0, self.attributes.max_mana)
        } else {
            0.0
        };
    }
}

// ===========================================================================
// 2. Casting & cooldowns
// ===========================================================================

/// Progress model for one in-flight cast. The `elapsed_ticks` fields are
/// the runtime counter; in a [`SpellDefinition`] template they must be `0`
/// (the constructors guarantee this).
#[derive(Clone, Debug, PartialEq)]
pub enum SpellCastState {
    /// Resolves immediately on `begin_cast`; occupies no cast slot.
    Instant,
    /// Wind-up: resolves after `total_ticks` elapsed ticks.
    CastTime {
        total_ticks: u32,
        elapsed_ticks: u32,
    },
    /// Hold-to-channel: emits a pulse every `interval_ticks`, drains
    /// `mana_per_pulse` per pulse, and resolves at `max_ticks`.
    Channeled {
        interval_ticks: u32,
        max_ticks: u32,
        elapsed_ticks: u32,
        mana_per_pulse: f32,
    },
}

impl SpellCastState {
    /// Cast-time template with `total_ticks` wind-up.
    pub fn cast_time(total_ticks: u32) -> Self {
        Self::CastTime {
            total_ticks,
            elapsed_ticks: 0,
        }
    }

    /// Channeled template: pulse every `interval_ticks`, stop at
    /// `max_ticks`, draining `mana_per_pulse` on each pulse tick.
    pub fn channeled(interval_ticks: u32, max_ticks: u32, mana_per_pulse: f32) -> Self {
        Self::Channeled {
            interval_ticks,
            max_ticks,
            elapsed_ticks: 0,
            mana_per_pulse,
        }
    }

    /// Elapsed ticks of the runtime state (always `0` for `Instant`).
    pub fn elapsed(&self) -> u32 {
        match self {
            Self::Instant => 0,
            Self::CastTime { elapsed_ticks, .. } | Self::Channeled { elapsed_ticks, .. } => {
                *elapsed_ticks
            }
        }
    }

    /// Total scheduled ticks (`0` for `Instant`).
    pub fn total_ticks(&self) -> u32 {
        match self {
            Self::Instant => 0,
            Self::CastTime { total_ticks, .. } => *total_ticks,
            Self::Channeled { max_ticks, .. } => *max_ticks,
        }
    }
}

/// Why an in-flight cast terminated early.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InterruptReason {
    /// Damage taken while `interrupt_on_damage` was set.
    Damaged,
    /// Movement while `interrupt_on_move` was set.
    Moved,
    /// A channel pulse could not afford `mana_per_pulse`.
    OutOfMana,
}

/// Per-tick event emitted by [`SpellCaster::tick`], `on_damage`, `on_move`,
/// and `begin_cast`.
#[derive(Clone, Debug, PartialEq)]
pub enum CastEvent {
    /// Cast resolved; per-spell cooldown and GCD are now running.
    Completed { spell_id: String },
    /// A channeled pulse fired (mana already drained).
    Pulse { spell_id: String },
    /// Cast terminated early; no cooldown is applied.
    Interrupted {
        spell_id: String,
        reason: InterruptReason,
    },
}

/// Why [`SpellCaster::begin_cast`] refused a spell.
#[derive(Clone, Debug, PartialEq)]
pub enum CastError {
    /// A cast is already in flight.
    AlreadyCasting,
    /// Per-spell cooldown still running.
    OnCooldown { remaining_ticks: u32 },
    /// Global cooldown still running.
    OnGlobalCooldown { remaining_ticks: u32 },
    /// `mana_cost` exceeds the current pool.
    InsufficientMana { needed: f32, available: f32 },
}

/// An in-flight cast plus its interruptibility flags.
#[derive(Clone, Debug, PartialEq)]
pub struct ActiveCast {
    pub spell_id: String,
    pub state: SpellCastState,
    /// Damage interrupts the cast (silences `CastTime`/`Channeled`).
    pub interrupt_on_damage: bool,
    /// Movement interrupts the cast.
    pub interrupt_on_move: bool,
}

/// Per-spell cooldowns plus a global cooldown (GCD), all in integer ticks.
/// Durations are pre-reduced by `cooldown_reduction` at trigger time;
/// [`CooldownTracker::tick`] decrements everything once per server tick.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CooldownTracker {
    spells: BTreeMap<String, u32>,
    gcd_remaining: u32,
    gcd_duration: u32,
    reduction: f32,
}

impl CooldownTracker {
    /// Tracker whose GCD lasts `gcd_duration` ticks when triggered.
    pub fn new(gcd_duration: u32) -> Self {
        Self {
            gcd_duration,
            ..Self::default()
        }
    }

    /// Update the fractional cooldown reduction (clamped `0.0..=0.8`);
    /// applies to the *next* [`Self::trigger`], not running cooldowns.
    pub fn set_reduction(&mut self, reduction: f32) {
        self.reduction = clamp_reduction(reduction);
    }

    /// `true` when neither the per-spell cooldown nor the GCD is running.
    pub fn can_cast(&self, spell_id: &str) -> bool {
        self.gcd_remaining == 0 && self.remaining(spell_id) == 0
    }

    /// Ticks left on `spell_id`'s own cooldown (`0` = ready).
    pub fn remaining(&self, spell_id: &str) -> u32 {
        self.spells.get(spell_id).copied().unwrap_or(0)
    }

    /// Ticks left on the global cooldown (`0` = ready).
    pub fn gcd_remaining(&self) -> u32 {
        self.gcd_remaining
    }

    /// Start `spell_id`'s cooldown (`base_ticks` reduced by the current
    /// reduction, rounded to nearest) and arm the GCD.
    pub fn trigger(&mut self, spell_id: &str, base_ticks: u32) {
        let scale = 1.0 - self.reduction;
        let reduced = (base_ticks as f32 * scale).round().max(0.0) as u32;
        self.spells.insert(spell_id.to_string(), reduced);
        self.gcd_remaining = self.gcd_duration;
    }

    /// Clear `spell_id`'s cooldown without touching the GCD.
    pub fn clear(&mut self, spell_id: &str) {
        self.spells.remove(spell_id);
    }

    /// Advance every cooldown by one tick; expired entries are dropped.
    pub fn tick(&mut self) {
        self.spells.retain(|_, t| {
            *t = t.saturating_sub(1);
            *t > 0
        });
        self.gcd_remaining = self.gcd_remaining.saturating_sub(1);
    }

    /// Read-only view of running per-spell cooldowns (id -> ticks left),
    /// in deterministic id order.
    pub fn entries(&self) -> &BTreeMap<String, u32> {
        &self.spells
    }
}

/// Stateful caster: mana pool + cooldown tracker + at most one in-flight
/// cast. Mana for `Instant`/`CastTime` spells is committed at `begin_cast`;
/// channeled spells drain `mana_per_pulse` per pulse instead.
#[derive(Clone, Debug, PartialEq)]
pub struct SpellCaster {
    pub mana: ManaPool,
    pub cooldowns: CooldownTracker,
    /// The single in-flight cast, if any.
    pub active: Option<ActiveCast>,
}

impl SpellCaster {
    pub fn new(mana: ManaPool, gcd_duration: u32) -> Self {
        Self {
            mana,
            cooldowns: CooldownTracker::new(gcd_duration),
            active: None,
        }
    }

    /// Try to begin `spell`. Instant casts resolve and return
    /// `CastEvent::Completed` immediately; `CastTime`/`Channeled` casts are
    /// parked in [`Self::active`]. The tracker's reduction is synced from
    /// `mana.attributes.cooldown_reduction` on every call.
    pub fn begin_cast(&mut self, spell: &SpellDefinition) -> Result<Vec<CastEvent>, CastError> {
        if self.active.is_some() {
            return Err(CastError::AlreadyCasting);
        }
        if self.cooldowns.remaining(&spell.id) > 0 {
            return Err(CastError::OnCooldown {
                remaining_ticks: self.cooldowns.remaining(&spell.id),
            });
        }
        if self.cooldowns.gcd_remaining() > 0 {
            return Err(CastError::OnGlobalCooldown {
                remaining_ticks: self.cooldowns.gcd_remaining(),
            });
        }
        self.cooldowns
            .set_reduction(self.mana.attributes.cooldown_reduction);

        match &spell.cast {
            SpellCastState::Channeled { mana_per_pulse, .. } => {
                // Channeled spells pay per pulse; the upfront `mana_cost`
                // still applies as an activation fee when nonzero.
                if spell.mana_cost > 0.0 && !self.mana.try_spend(spell.mana_cost) {
                    return Err(CastError::InsufficientMana {
                        needed: spell.mana_cost,
                        available: self.mana.current_mana,
                    });
                }
                if !self.mana.has(*mana_per_pulse) && *mana_per_pulse > 0.0 {
                    return Err(CastError::InsufficientMana {
                        needed: *mana_per_pulse,
                        available: self.mana.current_mana,
                    });
                }
            }
            _ => {
                if !self.mana.try_spend(spell.mana_cost) {
                    return Err(CastError::InsufficientMana {
                        needed: spell.mana_cost,
                        available: self.mana.current_mana,
                    });
                }
            }
        }

        match &spell.cast {
            SpellCastState::Instant => {
                self.cooldowns.trigger(&spell.id, spell.base_cooldown_ticks);
                Ok(vec![CastEvent::Completed {
                    spell_id: spell.id.clone(),
                }])
            }
            SpellCastState::CastTime { total_ticks, .. } => {
                let reduced = self.mana.attributes.reduced_cast_ticks(*total_ticks);
                if reduced == 0 {
                    self.cooldowns.trigger(&spell.id, spell.base_cooldown_ticks);
                    return Ok(vec![CastEvent::Completed {
                        spell_id: spell.id.clone(),
                    }]);
                }
                self.active = Some(ActiveCast {
                    spell_id: spell.id.clone(),
                    state: SpellCastState::CastTime {
                        total_ticks: reduced,
                        elapsed_ticks: 0,
                    },
                    interrupt_on_damage: spell.interrupt_on_damage,
                    interrupt_on_move: spell.interrupt_on_move,
                });
                Ok(Vec::new())
            }
            SpellCastState::Channeled { .. } => {
                self.active = Some(ActiveCast {
                    spell_id: spell.id.clone(),
                    state: spell.cast.clone(),
                    interrupt_on_damage: spell.interrupt_on_damage,
                    interrupt_on_move: spell.interrupt_on_move,
                });
                Ok(Vec::new())
            }
        }
    }

    /// Interrupt the active cast, if any. Returns the event when a cast was
    /// actually interrupted (flag checked), `None` otherwise.
    fn interrupt(&mut self, reason: InterruptReason, flag: bool) -> Option<CastEvent> {
        if !flag {
            return None;
        }
        self.active.take().map(|cast| CastEvent::Interrupted {
            spell_id: cast.spell_id,
            reason,
        })
    }

    /// Damage hook: interrupts when the active cast sets
    /// `interrupt_on_damage`.
    pub fn on_damage(&mut self) -> Option<CastEvent> {
        let flag = self.active.as_ref().is_some_and(|c| c.interrupt_on_damage);
        self.interrupt(InterruptReason::Damaged, flag)
    }

    /// Movement hook: interrupts when the active cast sets
    /// `interrupt_on_move`.
    pub fn on_move(&mut self) -> Option<CastEvent> {
        let flag = self.active.as_ref().is_some_and(|c| c.interrupt_on_move);
        self.interrupt(InterruptReason::Moved, flag)
    }

    /// One server tick: regen mana, run cooldowns, advance the active cast.
    /// Channeled pulses fire on `elapsed % interval == 0` (an unaffordable
    /// pulse interrupts with `OutOfMana`); a channeled cast completes when
    /// `elapsed` reaches `max_ticks` (a final-tick pulse fires first).
    pub fn tick(&mut self) -> Vec<CastEvent> {
        let mut events = Vec::new();
        self.mana.tick_regen();
        self.cooldowns.tick();

        let Some(cast) = self.active.as_mut() else {
            return events;
        };

        match &mut cast.state {
            SpellCastState::Instant => {
                // Defensive: Instant never parks in `active`.
                self.cooldowns.trigger(&cast.spell_id, 0);
                events.push(CastEvent::Completed {
                    spell_id: cast.spell_id.clone(),
                });
                self.active = None;
            }
            SpellCastState::CastTime {
                total_ticks,
                elapsed_ticks,
            } => {
                *elapsed_ticks += 1;
                if *elapsed_ticks >= *total_ticks {
                    let spell_id = cast.spell_id.clone();
                    self.active = None;
                    // The tracker doesn't know the spell's configured base
                    // cooldown here; callers re-arm via `complete_cooldown`.
                    events.push(CastEvent::Completed { spell_id });
                }
            }
            SpellCastState::Channeled {
                interval_ticks,
                max_ticks,
                elapsed_ticks,
                mana_per_pulse,
            } => {
                *elapsed_ticks += 1;
                let elapsed = *elapsed_ticks;
                let max = *max_ticks;
                let pulse = *interval_ticks > 0 && elapsed % *interval_ticks == 0;
                let pulse_cost = *mana_per_pulse;
                let spell_id = cast.spell_id.clone();

                if pulse && pulse_cost > 0.0 && !self.mana.try_spend(pulse_cost) {
                    self.active = None;
                    events.push(CastEvent::Interrupted {
                        spell_id,
                        reason: InterruptReason::OutOfMana,
                    });
                    return events;
                }
                if pulse {
                    events.push(CastEvent::Pulse {
                        spell_id: spell_id.clone(),
                    });
                }
                if elapsed >= max {
                    self.active = None;
                    events.push(CastEvent::Completed { spell_id });
                }
            }
        }
        events
    }

    /// Arm `spell`'s configured cooldown + GCD after a `Completed` event.
    /// Instant casts do this inside `begin_cast`; `CastTime`/`Channeled`
    /// callers run it when they observe `CastEvent::Completed`.
    pub fn complete_cooldown(&mut self, spell: &SpellDefinition) {
        self.cooldowns
            .set_reduction(self.mana.attributes.cooldown_reduction);
        self.cooldowns.trigger(&spell.id, spell.base_cooldown_ticks);
    }
}

// ===========================================================================
// 3. Composable spell delivery graph
// ===========================================================================

/// Hard cap on delivery steps (graph nodes) per spell.
pub const MAX_SPELL_STEPS: usize = 16;
/// Hard cap on effects carried by a single step.
pub const MAX_EFFECTS_PER_STEP: usize = 8;
/// Hard cap on chain hops; `Chain::max_hops` above this fails validation.
pub const MAX_CHAIN_HOPS: u32 = 8;
/// Hard cap on AoE zone radius in blocks.
pub const MAX_AOE_RADIUS: f32 = 32.0;
/// Hard cap on simultaneously active summons for `Summon::max_active`.
pub const MAX_ACTIVE_SUMMONS: u32 = 16;
/// Hard cap on any `duration_ticks`/`max_ticks` field (one hour).
pub const MAX_DURATION_TICKS: u32 = 72_000;
/// Hard cap on beam pierce depth.
pub const MAX_PIERCE_CAP: u32 = 64;
/// Hard cap on orbit projectile count.
pub const MAX_ORBIT_COUNT: u32 = 32;
/// Deterministic upper bound on AoE zone targets per pulse.
pub const MAX_AOE_TARGETS: u32 = 128;

/// How one spell step delivers its effects.
#[derive(Clone, Debug, PartialEq)]
pub enum SpellForm {
    /// Applies directly to the caster.
    SelfCast,
    /// Melee-range application; `range` in blocks.
    Touch { range: f32 },
    /// Travels `speed` blocks/tick under `gravity` (blocks/tick²) for at
    /// most `max_ticks`.
    Projectile {
        speed: f32,
        gravity: f32,
        max_ticks: u32,
    },
    /// Instant beam reaching `range` blocks, piercing up to `pierce_cap`
    /// targets.
    RaycastBeam { range: f32, pierce_cap: u32 },
    /// Ground zone of `radius` blocks pulsing every `pulse_interval` ticks
    /// for `duration_ticks`.
    AoeZone {
        radius: f32,
        duration_ticks: u32,
        pulse_interval: u32,
    },
    /// Jumps to a new target within `hop_radius` up to `max_hops` times.
    Chain { max_hops: u32, hop_radius: f32 },
    /// `count` orbiting projectiles at `radius` for `duration_ticks`.
    Orbit {
        count: u32,
        radius: f32,
        duration_ticks: u32,
    },
    /// Summons `entity_type` for `duration_ticks`, capped at `max_active`
    /// concurrent summons.
    Summon {
        entity_type: String,
        duration_ticks: u32,
        max_active: u32,
    },
}

/// What a spell step does to each delivered target.
#[derive(Clone, Debug, PartialEq)]
pub enum SpellEffect {
    /// `base_amount` damage of `school`, scaled by
    /// [`MagicAttributes::effective_school_power`].
    Damage { school: String, base_amount: f32 },
    /// `base_amount` healing, scaled by `spell_power`.
    Heal { base_amount: f32 },
    /// Absorption shield of `amount` for `duration_ticks`, scaled by
    /// `spell_power`.
    Shield { amount: f32, duration_ticks: u32 },
    /// Apply custom status `status_id` at `amplifier` for `duration_ticks`.
    ApplyStatus {
        status_id: String,
        amplifier: u8,
        duration_ticks: u32,
    },
    /// Knockback impulse of `strength` blocks.
    Knockback { strength: f32 },
    /// Teleport the target up to `max_distance` blocks.
    Blink { max_distance: f32 },
    /// Place a light source of `level` (`0..=15`) for `duration_ticks`.
    ConjureLight { level: u8, duration_ticks: u32 },
    /// Break a block whose harvest tier is at most `max_tier`.
    BreakBlock { max_tier: u8 },
    /// Emit client VFX `vfx_id` at the delivery point.
    EmitVfx { vfx_id: String },
}

/// One delivery step: a form plus the effects it applies per target.
#[derive(Clone, Debug, PartialEq)]
pub struct SpellNode {
    pub form: SpellForm,
    pub effects: Vec<SpellEffect>,
}

/// A complete, composable spell. `steps` execute in order; each step is a
/// [`SpellForm`] delivering [`SpellEffect`]s to its targets.
#[derive(Clone, Debug, PartialEq)]
pub struct SpellDefinition {
    pub id: String,
    /// Upfront mana cost (committed at `begin_cast`; `0.0` for pure
    /// pulse-budget channeled spells).
    pub mana_cost: f32,
    /// Cooldown armed when the cast completes, before reduction.
    pub base_cooldown_ticks: u32,
    /// Cast progress template (`elapsed_ticks` fields must be `0`).
    pub cast: SpellCastState,
    /// Damage interrupts a parked cast.
    pub interrupt_on_damage: bool,
    /// Movement interrupts a parked cast.
    pub interrupt_on_move: bool,
    /// Ordered delivery steps (`1..=MAX_SPELL_STEPS`).
    pub steps: Vec<SpellNode>,
}

/// Why [`SpellDefinition::validate`] rejected a spell.
#[derive(Clone, Debug, PartialEq)]
pub enum SpellValidationError {
    /// `id` empty or longer than [`MAX_ID_LEN`].
    BadSpellId,
    /// `mana_cost` non-finite or negative.
    BadManaCost,
    /// Cast template carries nonzero `elapsed_ticks`, a zero
    /// `total_ticks`/`interval_ticks`/`max_ticks`, non-finite
    /// `mana_per_pulse`, or a duration over [`MAX_DURATION_TICKS`].
    BadCastTemplate,
    /// No delivery steps.
    EmptySteps,
    /// More than [`MAX_SPELL_STEPS`] steps.
    TooManySteps,
    /// A step carries more than [`MAX_EFFECTS_PER_STEP`] effects.
    TooManyEffects { step: usize },
    /// A form parameter is non-finite, non-positive, or above its cap.
    BadForm { step: usize },
    /// An effect parameter is non-finite, negative, oversized, or an id
    /// exceeds [`MAX_ID_LEN`].
    BadEffect { step: usize },
}

impl SpellForm {
    fn validate(&self) -> bool {
        match self {
            Self::SelfCast => true,
            Self::Touch { range } => finite(*range) && *range > 0.0,
            Self::Projectile {
                speed,
                gravity,
                max_ticks,
            } => {
                finite(*speed)
                    && *speed > 0.0
                    && finite(*gravity)
                    && *max_ticks > 0
                    && *max_ticks <= MAX_DURATION_TICKS
            }
            Self::RaycastBeam { range, pierce_cap } => {
                finite(*range) && *range > 0.0 && *pierce_cap > 0 && *pierce_cap <= MAX_PIERCE_CAP
            }
            Self::AoeZone {
                radius,
                duration_ticks,
                pulse_interval,
            } => {
                finite(*radius)
                    && *radius > 0.0
                    && *radius <= MAX_AOE_RADIUS
                    && *duration_ticks > 0
                    && *duration_ticks <= MAX_DURATION_TICKS
                    && *pulse_interval > 0
            }
            Self::Chain {
                max_hops,
                hop_radius,
            } => {
                *max_hops > 0
                    && *max_hops <= MAX_CHAIN_HOPS
                    && finite(*hop_radius)
                    && *hop_radius > 0.0
            }
            Self::Orbit {
                count,
                radius,
                duration_ticks,
            } => {
                *count > 0
                    && *count <= MAX_ORBIT_COUNT
                    && finite(*radius)
                    && *radius > 0.0
                    && *duration_ticks > 0
                    && *duration_ticks <= MAX_DURATION_TICKS
            }
            Self::Summon {
                entity_type,
                duration_ticks,
                max_active,
            } => {
                valid_id(entity_type)
                    && *duration_ticks > 0
                    && *duration_ticks <= MAX_DURATION_TICKS
                    && *max_active > 0
                    && *max_active <= MAX_ACTIVE_SUMMONS
            }
        }
    }

    /// Deterministic upper bound on targets for one delivery.
    pub fn max_targets(&self) -> u32 {
        match self {
            Self::SelfCast | Self::Touch { .. } | Self::Projectile { .. } => 1,
            Self::RaycastBeam { pierce_cap, .. } => *pierce_cap,
            Self::AoeZone { .. } => MAX_AOE_TARGETS,
            Self::Chain { max_hops, .. } => max_hops.saturating_add(1),
            Self::Orbit { count, .. } => *count,
            Self::Summon { max_active, .. } => *max_active,
        }
    }

    /// Number of effect pulses this form produces over its lifetime.
    pub fn pulse_count(&self) -> u32 {
        match self {
            Self::AoeZone {
                duration_ticks,
                pulse_interval,
                ..
            } => duration_ticks / pulse_interval,
            _ => 1,
        }
    }
}

impl SpellEffect {
    fn validate(&self) -> bool {
        match self {
            Self::Damage {
                school,
                base_amount,
            } => valid_id(school) && finite(*base_amount) && *base_amount >= 0.0,
            Self::Heal { base_amount } => finite(*base_amount) && *base_amount >= 0.0,
            Self::Shield {
                amount,
                duration_ticks,
            } => {
                finite(*amount)
                    && *amount >= 0.0
                    && *duration_ticks > 0
                    && *duration_ticks <= MAX_DURATION_TICKS
            }
            Self::ApplyStatus {
                status_id,
                duration_ticks,
                ..
            } => {
                valid_id(status_id) && *duration_ticks > 0 && *duration_ticks <= MAX_DURATION_TICKS
            }
            Self::Knockback { strength } => finite(*strength) && *strength >= 0.0,
            Self::Blink { max_distance } => finite(*max_distance) && *max_distance >= 0.0,
            Self::ConjureLight {
                level,
                duration_ticks,
            } => *level <= 15 && *duration_ticks > 0 && *duration_ticks <= MAX_DURATION_TICKS,
            Self::BreakBlock { max_tier } => *max_tier <= 4,
            Self::EmitVfx { vfx_id } => valid_id(vfx_id),
        }
    }
}

impl SpellDefinition {
    /// Fail-closed structural validation of every unbounded dimension.
    pub fn validate(&self) -> Result<(), SpellValidationError> {
        if !valid_id(&self.id) {
            return Err(SpellValidationError::BadSpellId);
        }
        if !finite(self.mana_cost) || self.mana_cost < 0.0 {
            return Err(SpellValidationError::BadManaCost);
        }
        let cast_ok = match &self.cast {
            SpellCastState::Instant => true,
            SpellCastState::CastTime {
                total_ticks,
                elapsed_ticks,
            } => *elapsed_ticks == 0 && *total_ticks > 0 && *total_ticks <= MAX_DURATION_TICKS,
            SpellCastState::Channeled {
                interval_ticks,
                max_ticks,
                elapsed_ticks,
                mana_per_pulse,
            } => {
                *elapsed_ticks == 0
                    && *interval_ticks > 0
                    && *max_ticks > 0
                    && *max_ticks <= MAX_DURATION_TICKS
                    && finite(*mana_per_pulse)
                    && *mana_per_pulse >= 0.0
            }
        };
        if !cast_ok {
            return Err(SpellValidationError::BadCastTemplate);
        }
        if self.steps.is_empty() {
            return Err(SpellValidationError::EmptySteps);
        }
        if self.steps.len() > MAX_SPELL_STEPS {
            return Err(SpellValidationError::TooManySteps);
        }
        for (i, node) in self.steps.iter().enumerate() {
            if node.effects.len() > MAX_EFFECTS_PER_STEP {
                return Err(SpellValidationError::TooManyEffects { step: i });
            }
            if !node.form.validate() {
                return Err(SpellValidationError::BadForm { step: i });
            }
            if node.effects.iter().any(|e| !e.validate()) {
                return Err(SpellValidationError::BadEffect { step: i });
            }
        }
        Ok(())
    }

    /// Resolve the deterministic delivery plan: each step's form, target
    /// bound, pulse count, and effects with `spell_power`/school scaling
    /// applied to `Damage`/`Heal`/`Shield`. Validation runs first — an
    /// invalid spell can never produce a plan. `Summon` steps fail closed
    /// when `ctx.active_summons` already meets `max_active`.
    pub fn execute(
        &self,
        attributes: &MagicAttributes,
        ctx: &CastContext,
    ) -> Result<SpellExecutionResult, SpellExecutionError> {
        self.validate().map_err(SpellExecutionError::Invalid)?;

        let mut plans = Vec::with_capacity(self.steps.len());
        for (node_index, node) in self.steps.iter().enumerate() {
            if let SpellForm::Summon { max_active, .. } = &node.form {
                if ctx.active_summons >= *max_active {
                    return Err(SpellExecutionError::SummonCapReached {
                        active: ctx.active_summons,
                        max: *max_active,
                    });
                }
            }
            let effects = node
                .effects
                .iter()
                .map(|e| ResolvedEffect {
                    scaled_amount: scale_effect(e, attributes),
                    effect: e.clone(),
                })
                .collect();
            plans.push(NodePlan {
                node_index,
                form: node.form.clone(),
                max_targets: node.form.max_targets(),
                pulse_count: node.form.pulse_count(),
                effects,
            });
        }
        Ok(SpellExecutionResult {
            spell_id: self.id.clone(),
            mana_cost: self.mana_cost,
            plans,
        })
    }
}

/// Scaled magnitude of an effect: `Damage` uses the school multiplier,
/// `Heal`/`Shield` use `spell_power`, everything else passes its primary
/// magnitude through unscaled (`0.0` for purely declarative effects).
fn scale_effect(effect: &SpellEffect, attributes: &MagicAttributes) -> f32 {
    match effect {
        SpellEffect::Damage {
            school,
            base_amount,
        } => base_amount * attributes.effective_school_power(school),
        SpellEffect::Heal { base_amount } => base_amount * attributes.spell_power,
        SpellEffect::Shield { amount, .. } => amount * attributes.spell_power,
        SpellEffect::ApplyStatus { amplifier, .. } => f32::from(*amplifier),
        SpellEffect::Knockback { strength } => *strength,
        SpellEffect::Blink { max_distance } => *max_distance,
        SpellEffect::ConjureLight { level, .. } => f32::from(*level),
        SpellEffect::BreakBlock { max_tier } => f32::from(*max_tier),
        SpellEffect::EmitVfx { .. } => 0.0,
    }
}

/// Execution-time world facts the spell planner needs.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CastContext {
    /// Summons already active for this caster.
    pub active_summons: u32,
}

/// One resolved effect with its power-scaled magnitude.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedEffect {
    pub effect: SpellEffect,
    /// Scaled magnitude (see [`scale_effect`]).
    pub scaled_amount: f32,
}

/// Resolved plan for one delivery step.
#[derive(Clone, Debug, PartialEq)]
pub struct NodePlan {
    pub node_index: usize,
    pub form: SpellForm,
    /// Deterministic upper bound on delivered targets.
    pub max_targets: u32,
    /// Number of effect pulses over the step's lifetime.
    pub pulse_count: u32,
    pub effects: Vec<ResolvedEffect>,
}

/// Deterministic output of [`SpellDefinition::execute`].
#[derive(Clone, Debug, PartialEq)]
pub struct SpellExecutionResult {
    pub spell_id: String,
    pub mana_cost: f32,
    pub plans: Vec<NodePlan>,
}

/// Why [`SpellDefinition::execute`] failed.
#[derive(Clone, Debug, PartialEq)]
pub enum SpellExecutionError {
    /// Structural validation failed.
    Invalid(SpellValidationError),
    /// The caster already has `max_active` summons running.
    SummonCapReached { active: u32, max: u32 },
}

// ===========================================================================
// 4. Custom status effects
// ===========================================================================

/// Hard cap on registered custom status definitions.
pub const MAX_STATUS_DEFS: usize = 1024;
/// Hard cap on simultaneously active statuses per entity.
pub const MAX_ACTIVE_STATUSES: usize = 256;
/// Hard cap on attribute modifiers per status definition.
pub const MAX_ATTRIBUTE_MODIFIERS: usize = 16;
/// Hard cap on status duration.
pub const MAX_STATUS_TICKS: u32 = 1_728_000;

/// Data-driven definition of one custom status effect. All numeric fields
/// scale by `(amplifier + 1)` at application/pulse time.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StatusEffectDefinition {
    pub id: String,
    /// Highest legal amplifier.
    pub max_amplifier: u8,
    /// Ticks between DoT/HoT pulses; `0` = never pulses.
    pub pulse_interval_ticks: u32,
    /// Damage per pulse per amplifier level (DoT).
    pub damage_per_pulse: f32,
    /// Healing per pulse per amplifier level (HoT).
    pub heal_per_pulse: f32,
    /// Absorption granted on apply per amplifier level.
    pub absorption_per_level: f32,
    /// Target cannot move (harsher `rooted`: also no turning).
    pub frozen: bool,
    /// Target cannot move.
    pub rooted: bool,
    /// Target cannot cast spells.
    pub silenced: bool,
    /// Target cannot use items/attack.
    pub disarmed: bool,
    /// Additive attribute modifiers applied per amplifier level.
    pub attribute_modifiers: BTreeMap<String, f32>,
    /// Optional client aura VFX tag.
    pub vfx_aura: Option<String>,
}

/// Why [`CustomStatusRegistry::register`] rejected a definition.
#[derive(Clone, Debug, PartialEq)]
pub enum StatusDefError {
    /// `id` empty or longer than [`MAX_ID_LEN`].
    BadId,
    /// Registry already holds [`MAX_STATUS_DEFS`] entries.
    RegistryFull,
    /// `id` is already registered.
    Duplicate,
    /// A float field is non-finite or negative.
    BadValue,
    /// `damage_per_pulse`/`heal_per_pulse` nonzero with a zero
    /// `pulse_interval_ticks`.
    PulseWithoutInterval,
    /// More than [`MAX_ATTRIBUTE_MODIFIERS`] modifiers, a non-finite
    /// modifier value, an overlong attribute name, or overlong
    /// `vfx_aura`.
    BadModifiers,
}

/// Read-only registry of custom status definitions.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CustomStatusRegistry {
    defs: BTreeMap<String, StatusEffectDefinition>,
}

impl CustomStatusRegistry {
    /// Register `def` after fail-closed validation.
    pub fn register(&mut self, def: StatusEffectDefinition) -> Result<(), StatusDefError> {
        if !valid_id(&def.id) {
            return Err(StatusDefError::BadId);
        }
        if self.defs.len() >= MAX_STATUS_DEFS {
            return Err(StatusDefError::RegistryFull);
        }
        if self.defs.contains_key(&def.id) {
            return Err(StatusDefError::Duplicate);
        }
        for v in [
            def.damage_per_pulse,
            def.heal_per_pulse,
            def.absorption_per_level,
        ] {
            if !finite(v) || v < 0.0 {
                return Err(StatusDefError::BadValue);
            }
        }
        if (def.damage_per_pulse > 0.0 || def.heal_per_pulse > 0.0) && def.pulse_interval_ticks == 0
        {
            return Err(StatusDefError::PulseWithoutInterval);
        }
        if def.attribute_modifiers.len() > MAX_ATTRIBUTE_MODIFIERS
            || def
                .attribute_modifiers
                .iter()
                .any(|(k, v)| k.is_empty() || k.len() > MAX_ID_LEN || !v.is_finite())
            || def
                .vfx_aura
                .as_ref()
                .is_some_and(|a| a.is_empty() || a.len() > MAX_ID_LEN)
        {
            return Err(StatusDefError::BadModifiers);
        }
        self.defs.insert(def.id.clone(), def);
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<&StatusEffectDefinition> {
        self.defs.get(id)
    }

    pub fn len(&self) -> usize {
        self.defs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.defs.is_empty()
    }
}

/// One live status instance on an entity.
#[derive(Clone, Debug, PartialEq)]
pub struct ActiveStatus {
    pub def_id: String,
    /// Effective amplifier (clamped to `def.max_amplifier` at apply).
    pub amplifier: u8,
    pub remaining_ticks: u32,
    /// Countdown to the next DoT/HoT pulse.
    pub ticks_until_pulse: u32,
    /// Remaining absorption shield pool.
    pub absorption_remaining: f32,
}

/// Why [`ActiveStatusEffects::apply`] failed.
#[derive(Clone, Debug, PartialEq)]
pub enum ApplyStatusError {
    /// [`MAX_ACTIVE_STATUSES`] already active and `def.id` is new.
    TooManyActive,
    /// `duration_ticks` is `0` or above [`MAX_STATUS_TICKS`].
    BadDuration,
}

/// Aggregated crowd-control flags across all active statuses.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CcFlags {
    pub frozen: bool,
    pub rooted: bool,
    pub silenced: bool,
    pub disarmed: bool,
}

/// Deterministic per-tick outcome of [`ActiveStatusEffects::tick`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StatusTickReport {
    /// Total DoT damage dealt this tick (id order).
    pub damage: f32,
    /// Total HoT healing done this tick.
    pub healing: f32,
    /// Status ids that pulsed this tick.
    pub pulsed: Vec<String>,
    /// Status ids that expired this tick.
    pub expired: Vec<String>,
}

/// Live custom-status table for one entity, keyed by definition id so a
/// status is either absent or present exactly once.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ActiveStatusEffects {
    active: BTreeMap<String, ActiveStatus>,
}

impl ActiveStatusEffects {
    /// Apply `def` at `amplifier` (clamped to `max_amplifier`) for
    /// `duration_ticks`. Replaces an existing instance when the new
    /// amplifier is at least the old one; otherwise the stronger existing
    /// instance is kept.
    pub fn apply(
        &mut self,
        def: &StatusEffectDefinition,
        amplifier: u8,
        duration_ticks: u32,
    ) -> Result<(), ApplyStatusError> {
        if duration_ticks == 0 || duration_ticks > MAX_STATUS_TICKS {
            return Err(ApplyStatusError::BadDuration);
        }
        let amplifier = amplifier.min(def.max_amplifier);
        if let Some(existing) = self.active.get(&def.id) {
            if amplifier < existing.amplifier {
                return Ok(());
            }
        } else if self.active.len() >= MAX_ACTIVE_STATUSES {
            return Err(ApplyStatusError::TooManyActive);
        }
        let level = f32::from(amplifier) + 1.0;
        self.active.insert(
            def.id.clone(),
            ActiveStatus {
                def_id: def.id.clone(),
                amplifier,
                remaining_ticks: duration_ticks,
                ticks_until_pulse: def.pulse_interval_ticks,
                absorption_remaining: def.absorption_per_level * level,
            },
        );
        Ok(())
    }

    /// Remove `id`; returns whether it was active.
    pub fn remove(&mut self, id: &str) -> bool {
        self.active.remove(id).is_some()
    }

    pub fn is_active(&self, id: &str) -> bool {
        self.active.contains_key(id)
    }

    pub fn get(&self, id: &str) -> Option<&ActiveStatus> {
        self.active.get(id)
    }

    pub fn len(&self) -> usize {
        self.active.len()
    }

    pub fn is_empty(&self) -> bool {
        self.active.is_empty()
    }

    /// Snapshot of all live instances in id order.
    pub fn entries(&self) -> Vec<ActiveStatus> {
        self.active.values().cloned().collect()
    }

    /// OR of every active status's crowd-control flags (defs missing from
    /// `registry` contribute nothing).
    pub fn crowd_control(&self, registry: &CustomStatusRegistry) -> CcFlags {
        let mut flags = CcFlags::default();
        for status in self.active.values() {
            if let Some(def) = registry.get(&status.def_id) {
                flags.frozen |= def.frozen;
                flags.rooted |= def.rooted;
                flags.silenced |= def.silenced;
                flags.disarmed |= def.disarmed;
            }
        }
        flags
    }

    /// Sum of `attr` modifiers over all active statuses, each scaled by
    /// `(amplifier + 1)`.
    pub fn attribute_modifier(&self, registry: &CustomStatusRegistry, attr: &str) -> f32 {
        let mut total = 0.0;
        for status in self.active.values() {
            if let Some(def) = registry.get(&status.def_id) {
                if let Some(m) = def.attribute_modifiers.get(attr) {
                    total += m * (f32::from(status.amplifier) + 1.0);
                }
            }
        }
        total
    }

    /// Absorb up to `damage` from active absorption pools (in id order);
    /// returns the unabsorbed remainder. Non-positive or non-finite input
    /// absorbs nothing and returns `0.0`.
    pub fn absorb_damage(&mut self, damage: f32) -> f32 {
        if !damage.is_finite() || damage <= 0.0 {
            return 0.0;
        }
        let mut remaining = damage;
        for status in self.active.values_mut() {
            if status.absorption_remaining <= 0.0 {
                continue;
            }
            let absorbed = status.absorption_remaining.min(remaining);
            status.absorption_remaining -= absorbed;
            remaining -= absorbed;
            if remaining <= 0.0 {
                return 0.0;
            }
        }
        remaining
    }

    /// Total live absorption across all statuses.
    pub fn total_absorption(&self) -> f32 {
        self.active.values().map(|s| s.absorption_remaining).sum()
    }

    /// One deterministic tick: pulse every status whose pulse countdown
    /// reaches zero (damage/heal scale by `amplifier + 1`), decrement all
    /// remaining times, and expire anything hitting `0`. Iteration is in
    /// `BTreeMap` id order.
    pub fn tick(&mut self, registry: &CustomStatusRegistry) -> StatusTickReport {
        let mut report = StatusTickReport::default();
        let mut expired = Vec::new();

        for (id, status) in self.active.iter_mut() {
            if let Some(def) = registry.get(id) {
                if def.pulse_interval_ticks > 0 {
                    status.ticks_until_pulse = status.ticks_until_pulse.saturating_sub(1);
                    if status.ticks_until_pulse == 0 {
                        status.ticks_until_pulse = def.pulse_interval_ticks;
                        let level = f32::from(status.amplifier) + 1.0;
                        report.damage += def.damage_per_pulse * level;
                        report.healing += def.heal_per_pulse * level;
                        report.pulsed.push(id.clone());
                    }
                }
            }
            status.remaining_ticks = status.remaining_ticks.saturating_sub(1);
            if status.remaining_ticks == 0 {
                expired.push(id.clone());
            }
        }

        for id in expired {
            self.active.remove(&id);
            report.expired.push(id);
        }
        report
    }
}

// ===========================================================================
// 5. Ritual altars & mana network
// ===========================================================================

/// Hard cap on blocks in a ritual multiblock pattern.
pub const MAX_RITUAL_PATTERN_BLOCKS: usize = 256;
/// Largest absolute pattern offset on any axis.
pub const MAX_RITUAL_OFFSET: i32 = 16;
/// Hard cap on distinct reagent item types.
pub const MAX_REAGENTS: usize = 64;
/// Hard cap on ritual duration.
pub const MAX_RITUAL_TICKS: u32 = 72_000;
/// Hard cap on mana network nodes.
pub const MAX_NETWORK_NODES: usize = 512;
/// Hard cap on mana network edges.
pub const MAX_NETWORK_EDGES: usize = 4096;

/// Declarative ritual altar recipe: a relative multiblock pattern, reagent
/// cost, duration, and per-tick mana drain.
#[derive(Clone, Debug, PartialEq)]
pub struct RitualRecipe {
    pub id: String,
    /// `(dx, dy, dz)` offset from the altar origin -> required block id.
    /// Only required blocks are checked; extra world blocks are ignored.
    pub pattern: BTreeMap<(i32, i32, i32), String>,
    /// `item_id -> count` consumed atomically at ritual start.
    pub reagents: BTreeMap<String, u32>,
    pub duration_ticks: u32,
    /// Mana drained per active ritual tick.
    pub mana_per_tick: f32,
}

/// Why a ritual recipe/pattern/start failed.
#[derive(Clone, Debug, PartialEq)]
pub enum RitualError {
    /// Recipe failed structural validation (detail is stable prose).
    InvalidRecipe(&'static str),
    /// A required pattern block is absent or wrong at `offset`.
    PatternMismatch {
        offset: (i32, i32, i32),
        expected: String,
        found: Option<String>,
    },
    /// A reagent stack is short: `needed` vs `available`.
    MissingReagents {
        item: String,
        needed: u32,
        available: u32,
    },
}

impl RitualRecipe {
    /// Fail-closed structural validation.
    pub fn validate(&self) -> Result<(), RitualError> {
        if !valid_id(&self.id) {
            return Err(RitualError::InvalidRecipe("bad ritual id"));
        }
        if self.pattern.len() > MAX_RITUAL_PATTERN_BLOCKS {
            return Err(RitualError::InvalidRecipe("pattern too large"));
        }
        for ((dx, dy, dz), block) in &self.pattern {
            if dx.abs() > MAX_RITUAL_OFFSET
                || dy.abs() > MAX_RITUAL_OFFSET
                || dz.abs() > MAX_RITUAL_OFFSET
            {
                return Err(RitualError::InvalidRecipe("pattern offset out of range"));
            }
            if block.is_empty() || block.len() > MAX_ID_LEN {
                return Err(RitualError::InvalidRecipe("bad block id"));
            }
        }
        if self.reagents.len() > MAX_REAGENTS
            || self
                .reagents
                .iter()
                .any(|(item, n)| item.is_empty() || item.len() > MAX_ID_LEN || *n == 0)
        {
            return Err(RitualError::InvalidRecipe("bad reagents"));
        }
        if self.duration_ticks == 0 || self.duration_ticks > MAX_RITUAL_TICKS {
            return Err(RitualError::InvalidRecipe("bad duration"));
        }
        if !finite(self.mana_per_tick) || self.mana_per_tick < 0.0 {
            return Err(RitualError::InvalidRecipe("bad mana drain"));
        }
        Ok(())
    }

    /// Verify every required pattern block against `lookup(x, y, z)` at
    /// `origin + offset`. Extra blocks near the altar are ignored.
    pub fn verify_pattern(
        &self,
        origin: (i32, i32, i32),
        lookup: &dyn Fn(i32, i32, i32) -> Option<String>,
    ) -> Result<(), RitualError> {
        self.validate()?;
        for ((dx, dy, dz), expected) in &self.pattern {
            let found = lookup(origin.0 + dx, origin.1 + dy, origin.2 + dz);
            if found.as_deref() != Some(expected.as_str()) {
                return Err(RitualError::PatternMismatch {
                    offset: (*dx, *dy, *dz),
                    expected: expected.clone(),
                    found,
                });
            }
        }
        Ok(())
    }
}

/// Ritual runtime phase.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RitualPhase {
    Active,
    Completed,
    /// Terminal failure — reserved for hard aborts; mana starvation is
    /// reported as a stall, not a failure.
    Failed,
}

/// Per-tick ritual outcome.
#[derive(Clone, Debug, PartialEq)]
pub enum RitualEvent {
    /// Mana drained and progress advanced.
    Progressed { elapsed: u32, total: u32 },
    /// Pool could not pay `mana_per_tick`; no progress made.
    Stalled { needed_mana: f32 },
    /// Final tick completed.
    Completed,
    /// `tick` on an already terminal ritual.
    AlreadyFinished,
}

/// A running ritual: reagents are already consumed; progress advances one
/// tick per successful `mana_per_tick` drain and *stalls* (never fails)
/// when mana starves.
#[derive(Clone, Debug, PartialEq)]
pub struct RitualRuntime {
    pub recipe_id: String,
    pub elapsed_ticks: u32,
    pub total_ticks: u32,
    pub mana_per_tick: f32,
    pub phase: RitualPhase,
}

impl RitualRuntime {
    /// Validate `recipe`, atomically consume its reagents from `inventory`
    /// (`item_id -> count`), and start the ritual. Nothing is consumed
    /// unless every reagent stack covers its cost.
    pub fn start(
        recipe: &RitualRecipe,
        inventory: &mut BTreeMap<String, u32>,
    ) -> Result<Self, RitualError> {
        recipe.validate()?;
        for (item, needed) in &recipe.reagents {
            let available = inventory.get(item).copied().unwrap_or(0);
            if available < *needed {
                return Err(RitualError::MissingReagents {
                    item: item.clone(),
                    needed: *needed,
                    available,
                });
            }
        }
        for (item, needed) in &recipe.reagents {
            if let Some(slot) = inventory.get_mut(item) {
                *slot -= needed;
            }
        }
        Ok(Self {
            recipe_id: recipe.id.clone(),
            elapsed_ticks: 0,
            total_ticks: recipe.duration_ticks,
            mana_per_tick: recipe.mana_per_tick,
            phase: RitualPhase::Active,
        })
    }

    /// One server tick: drain `mana_per_tick` and advance, stall on
    /// starvation, complete at `total_ticks`.
    pub fn tick(&mut self, pool: &mut ManaPool) -> RitualEvent {
        if self.phase != RitualPhase::Active {
            return RitualEvent::AlreadyFinished;
        }
        if self.mana_per_tick > 0.0 && !pool.try_spend(self.mana_per_tick) {
            return RitualEvent::Stalled {
                needed_mana: self.mana_per_tick,
            };
        }
        self.elapsed_ticks += 1;
        if self.elapsed_ticks >= self.total_ticks {
            self.phase = RitualPhase::Completed;
            return RitualEvent::Completed;
        }
        RitualEvent::Progressed {
            elapsed: self.elapsed_ticks,
            total: self.total_ticks,
        }
    }

    /// Fraction complete in `0.0..=1.0`.
    pub fn progress(&self) -> f32 {
        if self.total_ticks == 0 {
            return 1.0;
        }
        self.elapsed_ticks as f32 / self.total_ticks as f32
    }
}

/// Role of a [`ManaNode`] in a [`ManaNetwork`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManaNodeRole {
    /// Generates `generation_per_tick` and pushes outward.
    Source,
    /// Buffer that pushes stored mana outward every tick.
    Spreader,
    /// Forwarding buffer — mechanically identical to `Spreader`; the
    /// distinct role exists for network topology declaration.
    Relay,
    /// Storage sink; never pushes.
    Pool,
}

impl ManaNodeRole {
    /// Whether nodes of this role push mana along outbound edges.
    pub fn can_push(self) -> bool {
        matches!(self, Self::Source | Self::Spreader | Self::Relay)
    }
}

/// One bounded node in a mana transfer network.
#[derive(Clone, Debug, PartialEq)]
pub struct ManaNode {
    pub id: String,
    pub role: ManaNodeRole,
    /// Currently stored mana, clamped to `0.0..=capacity`.
    pub stored: f32,
    /// Storage bound.
    pub capacity: f32,
    /// Per-tick generation (`Source` only; forced `0.0` otherwise).
    pub generation_per_tick: f32,
    /// Per-tick outbound transfer cap, total across all edges.
    pub transfer_per_tick: f32,
}

/// Why [`ManaNetwork::add_node`]/[`ManaNetwork::link`] failed.
#[derive(Clone, Debug, PartialEq)]
pub enum NetworkError {
    /// Bad id, non-finite/negative values, or zero/negative capacity.
    InvalidNode(&'static str),
    /// [`MAX_NETWORK_NODES`] reached or id already present.
    TooManyNodes,
    /// Node id already registered.
    Duplicate,
    /// An edge endpoint is not a registered node.
    UnknownNode(String),
    /// [`MAX_NETWORK_EDGES`] reached.
    TooManyEdges,
}

/// Deterministic per-tick network outcome.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NetworkTickReport {
    /// Mana generated by `Source` nodes this tick.
    pub generated: f32,
    /// Mana moved along edges this tick.
    pub moved: f32,
}

/// Bounded directed mana transfer network. One [`ManaNetwork::tick`] is
/// snapshot-based: generation applies first, then every pushing node
/// distributes `min(stored, transfer_per_tick)` evenly across outbound
/// edges whose receivers have snapshot space, and all deltas apply at
/// once. A unit of mana therefore moves at most one hop per tick, which
/// makes cycles safe by construction — a `Source -> A -> B -> A` ring can
/// never spin mana more than `transfer_per_tick` per node per tick.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ManaNetwork {
    nodes: BTreeMap<String, ManaNode>,
    /// `from -> [to]` adjacency; each `to` list is sorted and deduped.
    edges: BTreeMap<String, Vec<String>>,
}

impl ManaNetwork {
    /// Register `node` after fail-closed validation.
    pub fn add_node(&mut self, mut node: ManaNode) -> Result<(), NetworkError> {
        if !valid_id(&node.id) {
            return Err(NetworkError::InvalidNode("bad node id"));
        }
        if self.nodes.len() >= MAX_NETWORK_NODES {
            return Err(NetworkError::TooManyNodes);
        }
        if self.nodes.contains_key(&node.id) {
            return Err(NetworkError::Duplicate);
        }
        for v in [
            node.stored,
            node.capacity,
            node.generation_per_tick,
            node.transfer_per_tick,
        ] {
            if !finite(v) || v < 0.0 {
                return Err(NetworkError::InvalidNode("non-finite or negative value"));
            }
        }
        if node.capacity <= 0.0 {
            return Err(NetworkError::InvalidNode("capacity must be positive"));
        }
        if node.role != ManaNodeRole::Source {
            node.generation_per_tick = 0.0;
        }
        node.stored = node.stored.min(node.capacity);
        self.nodes.insert(node.id.clone(), node);
        Ok(())
    }

    /// Directed edge `from -> to`; duplicate edges are ignored.
    pub fn link(&mut self, from: &str, to: &str) -> Result<(), NetworkError> {
        if !self.nodes.contains_key(from) {
            return Err(NetworkError::UnknownNode(from.to_string()));
        }
        if !self.nodes.contains_key(to) {
            return Err(NetworkError::UnknownNode(to.to_string()));
        }
        let total_edges: usize = self.edges.values().map(Vec::len).sum();
        let list = self.edges.entry(from.to_string()).or_default();
        if !list.iter().any(|e| e == to) && total_edges >= MAX_NETWORK_EDGES {
            return Err(NetworkError::TooManyEdges);
        }
        if !list.iter().any(|e| e == to) {
            list.push(to.to_string());
            list.sort();
        }
        Ok(())
    }

    pub fn node(&self, id: &str) -> Option<&ManaNode> {
        self.nodes.get(id)
    }

    pub fn node_mut(&mut self, id: &str) -> Option<&mut ManaNode> {
        self.nodes.get_mut(id)
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Total stored mana across the network.
    pub fn total_stored(&self) -> f32 {
        self.nodes.values().map(|n| n.stored).sum()
    }

    /// One snapshot-based transfer tick (see type docs). Fully
    /// deterministic: node and edge iteration follow `BTreeMap` order.
    pub fn tick(&mut self) -> NetworkTickReport {
        let mut report = NetworkTickReport::default();

        // Phase 1: generation.
        for node in self.nodes.values_mut() {
            if node.role == ManaNodeRole::Source && node.generation_per_tick > 0.0 {
                let before = node.stored;
                node.stored = (node.stored + node.generation_per_tick).min(node.capacity);
                report.generated += node.stored - before;
            }
        }

        // Phase 2: snapshot stored amounts and receiver space.
        let stored: BTreeMap<String, f32> = self
            .nodes
            .iter()
            .map(|(id, n)| (id.clone(), n.stored))
            .collect();
        let space: BTreeMap<String, f32> = self
            .nodes
            .iter()
            .map(|(id, n)| (id.clone(), (n.capacity - n.stored).max(0.0)))
            .collect();

        // Phase 3: compute deltas from the snapshot.
        let mut deltas: BTreeMap<String, f32> = BTreeMap::new();
        for (id, node) in &self.nodes {
            if !node.role.can_push() {
                continue;
            }
            let Some(targets) = self.edges.get(id) else {
                continue;
            };
            let budget = node.transfer_per_tick.min(stored[id]);
            if budget <= 0.0 {
                continue;
            }
            let open: Vec<&String> = targets
                .iter()
                .filter(|t| space.get(*t).copied().unwrap_or(0.0) > 0.0)
                .collect();
            if open.is_empty() {
                continue;
            }
            let share = budget / open.len() as f32;
            let mut sent = 0.0;
            for target in open {
                let amount = share.min(space.get(target.as_str()).copied().unwrap_or(0.0));
                if amount <= 0.0 {
                    continue;
                }
                *deltas.entry(target.clone()).or_insert(0.0) += amount;
                sent += amount;
            }
            if sent > 0.0 {
                *deltas.entry(id.clone()).or_insert(0.0) -= sent;
                report.moved += sent;
            }
        }

        // Phase 4: apply deltas, clamped into `0.0..=capacity`.
        for (id, delta) in deltas {
            if let Some(node) = self.nodes.get_mut(&id) {
                node.stored = (node.stored + delta).clamp(0.0, node.capacity);
            }
        }
        report
    }
}

// ===========================================================================
// 6. MAG1 wire codec (channel `megu:magic_v1`)
// ===========================================================================

/// Custom-payload channel for `MAG1` magic sync frames.
pub const MAGIC_CHANNEL: &str = "megu:magic_v1";
/// 4-byte frame magic.
pub const MAGIC_SYNC_MAGIC: &[u8; 4] = b"MAG1";
/// Hard cap on wire cooldown entries.
pub const MAX_WIRE_COOLDOWNS: usize = 512;
/// Hard cap on wire status entries.
pub const MAX_WIRE_STATUSES: usize = 256;
/// Hard cap on wire identifier strings.
pub const MAX_WIRE_ID_LEN: usize = MAX_ID_LEN;

/// In-flight cast progress carried on the wire (Instant casts have no
/// progress and encode as `cast: None`).
#[derive(Clone, Debug, PartialEq)]
pub struct WireCastProgress {
    pub spell_id: String,
    pub state: SpellCastState,
}

/// Active status entry carried on the wire.
#[derive(Clone, Debug, PartialEq)]
pub struct WireStatus {
    pub id: String,
    pub amplifier: u8,
    pub remaining_ticks: u32,
    pub absorption_remaining: f32,
}

/// Full deterministic magic sync payload for one player.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MagicSyncState {
    pub mana: f32,
    pub max_mana: f32,
    /// Parked `CastTime`/`Channeled` cast, if any.
    pub cast: Option<WireCastProgress>,
    /// `spell_id -> remaining ticks`; encoded in sorted key order.
    pub cooldowns: BTreeMap<String, u32>,
    /// Active custom statuses.
    pub statuses: Vec<WireStatus>,
}

fn write_f32(out: &mut Vec<u8>, v: f32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn read_f32(bytes: &[u8], cursor: &mut usize) -> Option<f32> {
    let slice = bytes.get(*cursor..*cursor + 4)?;
    *cursor += 4;
    let v = f32::from_le_bytes(slice.try_into().ok()?);
    v.is_finite().then_some(v)
}

const CAST_TAG_NONE: u8 = 0;
const CAST_TAG_CAST_TIME: u8 = 1;
const CAST_TAG_CHANNELED: u8 = 2;

/// Encode `state` as a `MAG1` frame: `MAG1 | f32 mana | f32 max_mana |
/// u8 cast_tag + cast fields | VarInt n_cooldowns (id, VarInt ticks)* |
/// VarInt n_statuses (id, u8 amp, VarInt ticks, f32 absorption)*`.
/// Returns `Err` only when an id exceeds [`MAX_WIRE_ID_LEN`].
pub fn encode_magic_sync(state: &MagicSyncState) -> Result<Vec<u8>, &'static str> {
    let ids_ok = state.cast.as_ref().is_none_or(|c| valid_id(&c.spell_id))
        && state.cooldowns.keys().all(|k| valid_id(k))
        && state.statuses.iter().all(|s| valid_id(&s.id));
    if !ids_ok {
        return Err("oversized identifier in magic sync state");
    }

    let mut out = Vec::with_capacity(32);
    out.extend_from_slice(MAGIC_SYNC_MAGIC);
    write_f32(
        &mut out,
        if state.mana.is_finite() {
            state.mana
        } else {
            0.0
        },
    );
    write_f32(
        &mut out,
        if state.max_mana.is_finite() {
            state.max_mana
        } else {
            0.0
        },
    );

    match &state.cast {
        None => out.push(CAST_TAG_NONE),
        Some(WireCastProgress {
            spell_id,
            state:
                SpellCastState::CastTime {
                    total_ticks,
                    elapsed_ticks,
                },
        }) => {
            out.push(CAST_TAG_CAST_TIME);
            write_mc_string(&mut out, spell_id);
            write_varint(&mut out, *total_ticks);
            write_varint(&mut out, *elapsed_ticks);
        }
        Some(WireCastProgress {
            spell_id,
            state:
                SpellCastState::Channeled {
                    interval_ticks,
                    max_ticks,
                    elapsed_ticks,
                    mana_per_pulse,
                },
        }) => {
            out.push(CAST_TAG_CHANNELED);
            write_mc_string(&mut out, spell_id);
            write_varint(&mut out, *interval_ticks);
            write_varint(&mut out, *max_ticks);
            write_varint(&mut out, *elapsed_ticks);
            write_f32(&mut out, *mana_per_pulse);
        }
        Some(WireCastProgress {
            state: SpellCastState::Instant,
            ..
        }) => {
            // Instant casts carry no progress; encode as absent.
            out.push(CAST_TAG_NONE);
        }
    }

    write_varint(&mut out, state.cooldowns.len() as u32);
    for (id, ticks) in &state.cooldowns {
        write_mc_string(&mut out, id);
        write_varint(&mut out, *ticks);
    }

    write_varint(&mut out, state.statuses.len() as u32);
    for status in &state.statuses {
        write_mc_string(&mut out, &status.id);
        out.push(status.amplifier);
        write_varint(&mut out, status.remaining_ticks);
        write_f32(&mut out, status.absorption_remaining);
    }
    Ok(out)
}

/// Decode a `MAG1` frame, fail-closed: `None` on bad magic, truncation,
/// unknown cast tag, non-finite float, oversized count or string,
/// non-canonical (unsorted/duplicate) cooldown or status ids, or any
/// trailing byte.
pub fn decode_magic_sync(bytes: &[u8]) -> Option<MagicSyncState> {
    if bytes.get(..4)? != MAGIC_SYNC_MAGIC {
        return None;
    }
    let mut cursor = 4usize;

    let mana = read_f32(bytes, &mut cursor)?;
    let max_mana = read_f32(bytes, &mut cursor)?;

    let cast_tag = *bytes.get(cursor)?;
    cursor += 1;
    let cast = match cast_tag {
        CAST_TAG_NONE => None,
        CAST_TAG_CAST_TIME => {
            let spell_id = read_mc_string(bytes, &mut cursor, MAX_WIRE_ID_LEN)?;
            let total_ticks = read_varint(bytes, &mut cursor)?;
            let elapsed_ticks = read_varint(bytes, &mut cursor)?;
            if total_ticks == 0 {
                return None;
            }
            Some(WireCastProgress {
                spell_id,
                state: SpellCastState::CastTime {
                    total_ticks,
                    elapsed_ticks,
                },
            })
        }
        CAST_TAG_CHANNELED => {
            let spell_id = read_mc_string(bytes, &mut cursor, MAX_WIRE_ID_LEN)?;
            let interval_ticks = read_varint(bytes, &mut cursor)?;
            let max_ticks = read_varint(bytes, &mut cursor)?;
            let elapsed_ticks = read_varint(bytes, &mut cursor)?;
            let mana_per_pulse = read_f32(bytes, &mut cursor)?;
            if interval_ticks == 0 || max_ticks == 0 {
                return None;
            }
            Some(WireCastProgress {
                spell_id,
                state: SpellCastState::Channeled {
                    interval_ticks,
                    max_ticks,
                    elapsed_ticks,
                    mana_per_pulse,
                },
            })
        }
        _ => return None,
    };

    let n_cooldowns = read_varint(bytes, &mut cursor)? as usize;
    if n_cooldowns > MAX_WIRE_COOLDOWNS {
        return None;
    }
    let mut cooldowns = BTreeMap::new();
    let mut last_id: Option<String> = None;
    for _ in 0..n_cooldowns {
        let id = read_mc_string(bytes, &mut cursor, MAX_WIRE_ID_LEN)?;
        let ticks = read_varint(bytes, &mut cursor)?;
        // Canonical form: strictly ascending ids — rejects duplicates and
        // out-of-order encodings in one check.
        if last_id.as_ref().is_some_and(|p| id <= *p) {
            return None;
        }
        last_id = Some(id.clone());
        cooldowns.insert(id, ticks);
    }

    let n_statuses = read_varint(bytes, &mut cursor)? as usize;
    if n_statuses > MAX_WIRE_STATUSES {
        return None;
    }
    let mut statuses = Vec::with_capacity(n_statuses);
    let mut last_status: Option<String> = None;
    for _ in 0..n_statuses {
        let id = read_mc_string(bytes, &mut cursor, MAX_WIRE_ID_LEN)?;
        if last_status.as_ref().is_some_and(|p| id <= *p) {
            return None;
        }
        last_status = Some(id.clone());
        let amplifier = *bytes.get(cursor)?;
        cursor += 1;
        let remaining_ticks = read_varint(bytes, &mut cursor)?;
        let absorption_remaining = read_f32(bytes, &mut cursor)?;
        statuses.push(WireStatus {
            id,
            amplifier,
            remaining_ticks,
            absorption_remaining,
        });
    }

    if cursor != bytes.len() {
        return None;
    }
    Some(MagicSyncState {
        mana,
        max_mana,
        cast,
        cooldowns,
        statuses,
    })
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn attrs() -> MagicAttributes {
        MagicAttributes {
            max_mana: 100.0,
            regen_per_tick: 2.0,
            spell_power: 1.0,
            cooldown_reduction: 0.0,
            cast_time_reduction: 0.0,
            school_power: BTreeMap::new(),
        }
    }

    fn simple_spell(id: &str, cast: SpellCastState) -> SpellDefinition {
        SpellDefinition {
            id: id.to_string(),
            mana_cost: 10.0,
            base_cooldown_ticks: 20,
            cast,
            interrupt_on_damage: false,
            interrupt_on_move: false,
            steps: vec![SpellNode {
                form: SpellForm::SelfCast,
                effects: vec![SpellEffect::Heal { base_amount: 5.0 }],
            }],
        }
    }

    // -- 1. mana / attributes ------------------------------------------------

    #[test]
    fn mana_pool_spend_regen_restore_saturates() {
        let mut pool = ManaPool::new(attrs());
        assert_eq!(pool.current_mana, 100.0);

        assert!(pool.try_spend(30.0));
        assert_eq!(pool.current_mana, 70.0);
        assert!(!pool.try_spend(80.0));
        assert_eq!(pool.current_mana, 70.0);

        pool.tick_regen();
        assert_eq!(pool.current_mana, 72.0);
        assert_eq!(pool.restore(50.0), 28.0);
        assert_eq!(pool.current_mana, 100.0);
    }

    #[test]
    fn mana_pool_nan_and_infinite_inputs_are_safe() {
        let mut pool = ManaPool::new(attrs());
        assert!(!pool.try_spend(f32::NAN));
        assert!(!pool.try_spend(f32::INFINITY));
        assert!(!pool.try_spend(-1.0));
        assert_eq!(pool.current_mana, 100.0);
        assert_eq!(pool.restore(f32::NAN), 0.0);
        assert_eq!(pool.restore(f32::INFINITY), 0.0);

        let mut attrs_nan = attrs();
        attrs_nan.regen_per_tick = f32::NAN;
        attrs_nan.max_mana = f32::NAN;
        let mut pool = ManaPool::with_mana(attrs_nan, f32::NAN);
        assert_eq!(pool.attributes.max_mana, 0.0);
        assert_eq!(pool.current_mana, 0.0);
        pool.tick_regen();
        assert_eq!(pool.current_mana, 0.0);
    }

    #[test]
    fn attributes_clamp_reductions_and_school_power() {
        let mut a = attrs();
        a.cooldown_reduction = 4.0;
        a.cast_time_reduction = f32::NAN;
        a.school_power.insert("fire".into(), 0.5);
        a.school_power.insert("bad".into(), f32::NAN);
        a.sanitize();
        assert_eq!(a.cooldown_reduction, 0.8);
        assert_eq!(a.cast_time_reduction, 0.0);
        assert_eq!(a.school_power.len(), 1);
        assert_eq!(a.effective_school_power("fire"), 1.5);
        assert_eq!(a.effective_school_power("water"), 1.0);
        assert_eq!(a.reduced_cast_ticks(100), 100);
        a.cast_time_reduction = 0.5;
        assert_eq!(a.reduced_cast_ticks(100), 50);
    }

    // -- 2. casting & cooldowns ----------------------------------------------

    #[test]
    fn cooldown_tracker_tick_gcd_and_reduction() {
        let mut cd = CooldownTracker::new(5);
        cd.set_reduction(0.5);
        cd.trigger("a", 10);
        assert_eq!(cd.remaining("a"), 5);
        assert_eq!(cd.gcd_remaining(), 5);
        assert!(!cd.can_cast("b"));
        for _ in 0..5 {
            cd.tick();
        }
        assert_eq!(cd.remaining("a"), 0);
        assert!(cd.can_cast("a"));

        cd.set_reduction(f32::NAN);
        cd.trigger("a", 10);
        assert_eq!(cd.remaining("a"), 10);
    }

    #[test]
    fn cast_time_completes_after_ticks() {
        let mut caster = SpellCaster::new(ManaPool::new(attrs()), 2);
        let spell = simple_spell("wind", SpellCastState::cast_time(3));
        assert_eq!(caster.begin_cast(&spell), Ok(Vec::new()));
        assert_eq!(caster.mana.current_mana, 90.0);

        assert!(caster.tick().is_empty());
        assert!(caster.tick().is_empty());
        let events = caster.tick();
        assert_eq!(
            events,
            vec![CastEvent::Completed {
                spell_id: "wind".into()
            }]
        );
        assert!(caster.active.is_none());
    }

    #[test]
    fn channeled_pulses_drain_and_outofmana_interrupts() {
        let mut caster = SpellCaster::new(ManaPool::new(attrs()), 0);
        let spell = simple_spell("beam", SpellCastState::channeled(2, 10, 25.0));
        spell.validate().unwrap();
        caster.begin_cast(&spell).unwrap();
        // tick 1: no pulse (elapsed 1 % 2 != 0)
        assert!(caster.tick().is_empty());
        // tick 2: pulse drains 25
        assert_eq!(
            caster.tick(),
            vec![CastEvent::Pulse {
                spell_id: "beam".into()
            }]
        );
        // regen 2/tick: spent 10 upfront, -25 pulse +2 regen *2 => 69
        assert_eq!(caster.mana.current_mana, 69.0);

        // Drain the pool so the next pulse cannot pay.
        caster.mana.try_spend(69.0);
        caster.mana.attributes.regen_per_tick = 0.0;
        let events = caster.tick();
        assert!(events.is_empty()); // tick 3: regen was 0
        let events = caster.tick(); // tick 4: pulse tick, no mana
        assert_eq!(
            events,
            vec![CastEvent::Interrupted {
                spell_id: "beam".into(),
                reason: InterruptReason::OutOfMana,
            }]
        );
        assert!(caster.active.is_none());
    }

    #[test]
    fn interrupts_on_damage_and_move_only_when_flagged() {
        let mut caster = SpellCaster::new(ManaPool::new(attrs()), 0);
        let mut spell = simple_spell("ward", SpellCastState::cast_time(10));
        caster.begin_cast(&spell).unwrap();
        assert!(caster.on_damage().is_none());
        assert!(caster.active.is_some());
        caster.active = None;

        spell.interrupt_on_damage = true;
        spell.interrupt_on_move = true;
        caster.begin_cast(&spell).unwrap();
        assert_eq!(
            caster.on_move(),
            Some(CastEvent::Interrupted {
                spell_id: "ward".into(),
                reason: InterruptReason::Moved,
            })
        );

        caster.begin_cast(&spell).unwrap();
        assert_eq!(
            caster.on_damage(),
            Some(CastEvent::Interrupted {
                spell_id: "ward".into(),
                reason: InterruptReason::Damaged,
            })
        );
    }

    #[test]
    fn begin_cast_failures_are_reported() {
        let mut caster = SpellCaster::new(ManaPool::new(attrs()), 4);
        let spell = simple_spell("x", SpellCastState::Instant);
        caster.begin_cast(&spell).unwrap();
        assert_eq!(
            caster.begin_cast(&spell),
            Err(CastError::OnCooldown {
                remaining_ticks: 20
            })
        );
        let other = simple_spell("y", SpellCastState::Instant);
        assert_eq!(
            caster.begin_cast(&other),
            Err(CastError::OnGlobalCooldown { remaining_ticks: 4 })
        );

        caster.cooldowns = CooldownTracker::new(0);
        caster.mana.set_current(5.0);
        assert_eq!(
            caster.begin_cast(&spell),
            Err(CastError::InsufficientMana {
                needed: 10.0,
                available: 5.0
            })
        );
    }

    // -- 3. spell delivery graph ---------------------------------------------

    #[test]
    fn spell_validation_fails_closed_on_every_cap() {
        let node = |form: SpellForm| SpellNode {
            form,
            effects: vec![],
        };
        let base = simple_spell("v", SpellCastState::Instant);

        let mut s = base.clone();
        s.steps = vec![node(SpellForm::SelfCast); MAX_SPELL_STEPS + 1];
        assert_eq!(s.validate(), Err(SpellValidationError::TooManySteps));

        s = base.clone();
        s.steps = vec![node(SpellForm::Chain {
            max_hops: MAX_CHAIN_HOPS + 1,
            hop_radius: 4.0,
        })];
        assert_eq!(s.validate(), Err(SpellValidationError::BadForm { step: 0 }));

        s = base.clone();
        s.steps = vec![node(SpellForm::AoeZone {
            radius: MAX_AOE_RADIUS + 1.0,
            duration_ticks: 20,
            pulse_interval: 5,
        })];
        assert_eq!(s.validate(), Err(SpellValidationError::BadForm { step: 0 }));

        s = base.clone();
        s.steps = vec![node(SpellForm::Summon {
            entity_type: "wisp".into(),
            duration_ticks: 100,
            max_active: MAX_ACTIVE_SUMMONS + 1,
        })];
        assert_eq!(s.validate(), Err(SpellValidationError::BadForm { step: 0 }));

        s = base.clone();
        s.steps = vec![SpellNode {
            form: SpellForm::SelfCast,
            effects: vec![SpellEffect::Damage {
                school: "f".into(),
                base_amount: f32::NAN,
            }],
        }];
        assert_eq!(
            s.validate(),
            Err(SpellValidationError::BadEffect { step: 0 })
        );

        s = base.clone();
        s.mana_cost = f32::INFINITY;
        assert_eq!(s.validate(), Err(SpellValidationError::BadManaCost));

        s = base.clone();
        s.steps.clear();
        assert_eq!(s.validate(), Err(SpellValidationError::EmptySteps));
    }

    #[test]
    fn execute_scales_and_plans_delivery() {
        let mut a = attrs();
        a.spell_power = 2.0;
        a.school_power.insert("fire".into(), 0.5);
        let spell = SpellDefinition {
            id: "blast".into(),
            mana_cost: 40.0,
            base_cooldown_ticks: 60,
            cast: SpellCastState::Instant,
            interrupt_on_damage: false,
            interrupt_on_move: false,
            steps: vec![
                SpellNode {
                    form: SpellForm::Projectile {
                        speed: 1.5,
                        gravity: 0.05,
                        max_ticks: 40,
                    },
                    effects: vec![SpellEffect::Damage {
                        school: "fire".into(),
                        base_amount: 10.0,
                    }],
                },
                SpellNode {
                    form: SpellForm::AoeZone {
                        radius: 4.0,
                        duration_ticks: 20,
                        pulse_interval: 5,
                    },
                    effects: vec![SpellEffect::Heal { base_amount: 3.0 }],
                },
            ],
        };
        let result = spell.execute(&a, &CastContext::default()).unwrap();
        assert_eq!(result.plans.len(), 2);
        assert_eq!(result.plans[0].effects[0].scaled_amount, 25.0); // (2.0+0.5)*10
        assert_eq!(result.plans[0].max_targets, 1);
        assert_eq!(result.plans[1].effects[0].scaled_amount, 6.0); // 2.0*3
        assert_eq!(result.plans[1].pulse_count, 4);
        assert_eq!(result.plans[1].max_targets, MAX_AOE_TARGETS);
    }

    #[test]
    fn execute_blocks_summon_at_cap() {
        let spell = SpellDefinition {
            id: "call".into(),
            mana_cost: 0.0,
            base_cooldown_ticks: 0,
            cast: SpellCastState::Instant,
            interrupt_on_damage: false,
            interrupt_on_move: false,
            steps: vec![SpellNode {
                form: SpellForm::Summon {
                    entity_type: "wisp".into(),
                    duration_ticks: 100,
                    max_active: 2,
                },
                effects: vec![],
            }],
        };
        let ctx = CastContext { active_summons: 2 };
        assert_eq!(
            spell.execute(&attrs(), &ctx),
            Err(SpellExecutionError::SummonCapReached { active: 2, max: 2 })
        );
        let ctx = CastContext { active_summons: 1 };
        assert!(spell.execute(&attrs(), &ctx).is_ok());
    }

    // -- 4. custom status effects --------------------------------------------

    fn registry() -> CustomStatusRegistry {
        let mut r = CustomStatusRegistry::default();
        r.register(StatusEffectDefinition {
            id: "burn".into(),
            max_amplifier: 4,
            pulse_interval_ticks: 2,
            damage_per_pulse: 2.0,
            ..StatusEffectDefinition::default()
        })
        .unwrap();
        r.register(StatusEffectDefinition {
            id: "mend".into(),
            max_amplifier: 2,
            pulse_interval_ticks: 1,
            heal_per_pulse: 1.5,
            ..StatusEffectDefinition::default()
        })
        .unwrap();
        r.register(StatusEffectDefinition {
            id: "ward".into(),
            max_amplifier: 1,
            absorption_per_level: 4.0,
            ..StatusEffectDefinition::default()
        })
        .unwrap();
        r.register(StatusEffectDefinition {
            id: "bind".into(),
            max_amplifier: 0,
            rooted: true,
            silenced: true,
            ..StatusEffectDefinition::default()
        })
        .unwrap();
        r
    }

    #[test]
    fn status_dot_hot_pulse_and_expire() {
        let reg = registry();
        let mut fx = ActiveStatusEffects::default();
        fx.apply(reg.get("burn").unwrap(), 1, 4).unwrap(); // level 2 -> 4 dmg/pulse
        fx.apply(reg.get("mend").unwrap(), 0, 3).unwrap(); // 1.5 heal/tick

        let r1 = fx.tick(&reg); // tick 1: mend pulses, burn countdown->1
        assert_eq!(r1.healing, 1.5);
        assert_eq!(r1.damage, 0.0);

        let r2 = fx.tick(&reg); // tick 2: burn pulses for 4
        assert_eq!(r2.damage, 4.0);
        assert_eq!(r2.healing, 1.5);
        assert!(r2.pulsed.contains(&"burn".to_string()));

        fx.tick(&reg);
        let r4 = fx.tick(&reg); // burn (4 ticks) & mend (3 ticks) expired
        assert!(r4.expired.contains(&"burn".to_string()));
        assert!(!fx.is_active("burn"));
        assert!(!fx.is_active("mend"));
    }

    #[test]
    fn status_absorption_shield_absorbs_in_id_order() {
        let reg = registry();
        let mut fx = ActiveStatusEffects::default();
        fx.apply(reg.get("ward").unwrap(), 0, 100).unwrap(); // 4.0 pool
        assert_eq!(fx.total_absorption(), 4.0);
        assert_eq!(fx.absorb_damage(3.0), 0.0);
        assert_eq!(fx.total_absorption(), 1.0);
        assert_eq!(fx.absorb_damage(5.0), 4.0);
        assert_eq!(fx.total_absorption(), 0.0);
    }

    #[test]
    fn status_cc_flags_and_attribute_modifiers() {
        let mut reg = registry();
        reg.register(StatusEffectDefinition {
            id: "haste".into(),
            max_amplifier: 1,
            attribute_modifiers: BTreeMap::from([("move_speed".into(), 0.2)]),
            ..StatusEffectDefinition::default()
        })
        .unwrap();
        let mut fx = ActiveStatusEffects::default();
        fx.apply(reg.get("bind").unwrap(), 5, 10).unwrap(); // clamped to amp 0
        fx.apply(reg.get("haste").unwrap(), 1, 10).unwrap();

        let cc = fx.crowd_control(&reg);
        assert!(cc.rooted && cc.silenced);
        assert!(!cc.frozen && !cc.disarmed);
        // amp 1 -> level 2 -> 0.4
        assert_eq!(fx.attribute_modifier(&reg, "move_speed"), 0.4);
    }

    #[test]
    fn status_registry_rejects_bad_defs() {
        let mut r = CustomStatusRegistry::default();
        assert_eq!(
            r.register(StatusEffectDefinition {
                id: "x".into(),
                damage_per_pulse: 1.0,
                pulse_interval_ticks: 0,
                ..StatusEffectDefinition::default()
            }),
            Err(StatusDefError::PulseWithoutInterval)
        );
        assert_eq!(
            r.register(StatusEffectDefinition {
                id: "y".into(),
                damage_per_pulse: f32::NAN,
                ..StatusEffectDefinition::default()
            }),
            Err(StatusDefError::BadValue)
        );
        r.register(StatusEffectDefinition {
            id: "z".into(),
            ..StatusEffectDefinition::default()
        })
        .unwrap();
        assert_eq!(
            r.register(StatusEffectDefinition {
                id: "z".into(),
                ..StatusEffectDefinition::default()
            }),
            Err(StatusDefError::Duplicate)
        );
    }

    // -- 5. rituals & mana network --------------------------------------------

    fn recipe() -> RitualRecipe {
        RitualRecipe {
            id: "ascend".into(),
            pattern: BTreeMap::from([
                ((0, 0, 0), "mc:altar".to_string()),
                ((1, 0, 0), "mc:candle".to_string()),
                ((-1, 0, 0), "mc:candle".to_string()),
            ]),
            reagents: BTreeMap::from([("mc:dust".to_string(), 3)]),
            duration_ticks: 4,
            mana_per_tick: 10.0,
        }
    }

    #[test]
    fn ritual_verify_start_consume_and_complete() {
        let r = recipe();
        let lookup = |x: i32, _y: i32, z: i32| -> Option<String> {
            match (x, z) {
                (5, 5) => Some("mc:altar".into()),
                (6, 5) | (4, 5) => Some("mc:candle".into()),
                _ => None,
            }
        };
        r.verify_pattern((5, 0, 5), &lookup).unwrap();

        // Wrong block at +1 fails closed with the offending offset.
        let bad = |x: i32, y: i32, z: i32| -> Option<String> {
            if x == 6 {
                Some("mc:dirt".into())
            } else {
                lookup(x, y, z)
            }
        };
        assert!(matches!(
            r.verify_pattern((5, 0, 5), &bad),
            Err(RitualError::PatternMismatch {
                offset: (1, 0, 0),
                ..
            })
        ));

        // Reagents consume atomically.
        let mut inv = BTreeMap::from([("mc:dust".to_string(), 2)]);
        assert!(matches!(
            RitualRuntime::start(&r, &mut inv),
            Err(RitualError::MissingReagents { .. })
        ));
        assert_eq!(inv["mc:dust"], 2); // nothing consumed on failure
        inv.insert("mc:dust".into(), 5);
        let mut rt = RitualRuntime::start(&r, &mut inv).unwrap();
        assert_eq!(inv["mc:dust"], 2);

        let mut pool = ManaPool::with_mana(attrs(), 25.0);
        assert_eq!(
            rt.tick(&mut pool),
            RitualEvent::Progressed {
                elapsed: 1,
                total: 4
            }
        );
        assert_eq!(
            rt.tick(&mut pool),
            RitualEvent::Progressed {
                elapsed: 2,
                total: 4
            }
        );
        assert_eq!(
            rt.tick(&mut pool),
            RitualEvent::Stalled { needed_mana: 10.0 }
        ); // 25-20=5 <10
        pool.restore(15.0);
        assert_eq!(
            rt.tick(&mut pool),
            RitualEvent::Progressed {
                elapsed: 3,
                total: 4
            }
        );
        assert_eq!(rt.tick(&mut pool), RitualEvent::Completed);
        assert_eq!(rt.tick(&mut pool), RitualEvent::AlreadyFinished);
    }

    #[test]
    fn mana_network_transfers_caps_and_is_cycle_safe() {
        let mut net = ManaNetwork::default();
        let node = |id: &str, role: ManaNodeRole, cap: f32, xfer: f32| ManaNode {
            id: id.into(),
            role,
            stored: 0.0,
            capacity: cap,
            generation_per_tick: 0.0,
            transfer_per_tick: xfer,
        };
        let mut src = node("src", ManaNodeRole::Source, 100.0, 10.0);
        src.generation_per_tick = 5.0;
        net.add_node(src).unwrap();
        net.add_node(node("relay", ManaNodeRole::Relay, 50.0, 4.0))
            .unwrap();
        net.add_node(node("pool", ManaNodeRole::Pool, 100.0, 0.0))
            .unwrap();
        net.link("src", "relay").unwrap();
        net.link("relay", "pool").unwrap();

        let r = net.tick();
        assert_eq!(r.generated, 5.0);
        assert_eq!(r.moved, 5.0); // src pushes its 5 to relay
        assert_eq!(net.node("relay").unwrap().stored, 5.0);

        let r = net.tick();
        // relay pushes min(5 stored, 4 xfer) = 4 to pool; src pushes 5 more.
        assert_eq!(net.node("pool").unwrap().stored, 4.0);
        assert_eq!(net.node("relay").unwrap().stored, 6.0);
        assert_eq!(r.moved, 9.0);
    }

    #[test]
    fn mana_network_cycle_moves_one_hop_per_tick() {
        let mut net = ManaNetwork::default();
        net.add_node(ManaNode {
            id: "a".into(),
            role: ManaNodeRole::Spreader,
            stored: 10.0,
            capacity: 10.0,
            generation_per_tick: 0.0,
            transfer_per_tick: 4.0,
        })
        .unwrap();
        net.add_node(ManaNode {
            id: "b".into(),
            role: ManaNodeRole::Spreader,
            stored: 0.0,
            capacity: 10.0,
            generation_per_tick: 0.0,
            transfer_per_tick: 4.0,
        })
        .unwrap();
        net.link("a", "b").unwrap();
        net.link("b", "a").unwrap(); // cycle

        let r = net.tick();
        // Snapshot semantics: a->b 4.0, b has 0 stored so sends 0.
        assert_eq!(r.moved, 4.0);
        assert_eq!(net.node("b").unwrap().stored, 4.0);
        assert_eq!(net.node("a").unwrap().stored, 6.0);
        // Total mana is conserved and bounded — no runaway duplication.
        assert_eq!(net.total_stored(), 10.0);
    }

    // -- 6. MAG1 wire codec ----------------------------------------------------

    fn sync_state() -> MagicSyncState {
        MagicSyncState {
            mana: 42.5,
            max_mana: 100.0,
            cast: Some(WireCastProgress {
                spell_id: "beam".into(),
                state: SpellCastState::Channeled {
                    interval_ticks: 2,
                    max_ticks: 10,
                    elapsed_ticks: 3,
                    mana_per_pulse: 1.5,
                },
            }),
            cooldowns: BTreeMap::from([("alpha".to_string(), 12), ("beta".to_string(), 30)]),
            statuses: vec![
                WireStatus {
                    id: "burn".into(),
                    amplifier: 2,
                    remaining_ticks: 40,
                    absorption_remaining: 0.0,
                },
                WireStatus {
                    id: "ward".into(),
                    amplifier: 0,
                    remaining_ticks: 90,
                    absorption_remaining: 4.0,
                },
            ],
        }
    }

    #[test]
    fn mag1_roundtrip_preserves_state() {
        let state = sync_state();
        let bytes = encode_magic_sync(&state).unwrap();
        assert_eq!(&bytes[..4], b"MAG1");
        assert_eq!(decode_magic_sync(&bytes), Some(state));

        let mut no_cast = sync_state();
        no_cast.cast = None;
        let bytes = encode_magic_sync(&no_cast).unwrap();
        assert_eq!(decode_magic_sync(&bytes), Some(no_cast));
    }

    #[test]
    fn mag1_rejects_hostile_inputs() {
        let good = encode_magic_sync(&sync_state()).unwrap();

        assert_eq!(decode_magic_sync(&[]), None);
        assert_eq!(decode_magic_sync(&good[..3]), None);
        assert_eq!(decode_magic_sync(&good[..good.len() - 1]), None);

        let mut trailing = good.clone();
        trailing.push(0x00);
        assert_eq!(decode_magic_sync(&trailing), None);

        let mut bad_magic = good.clone();
        bad_magic[0] = b'X';
        assert_eq!(decode_magic_sync(&bad_magic), None);

        // NaN mana payload.
        let mut nan = encode_magic_sync(&MagicSyncState::default()).unwrap();
        nan[4..8].copy_from_slice(&f32::NAN.to_le_bytes());
        assert_eq!(decode_magic_sync(&nan), None);

        // Unknown cast tag.
        let mut tag = encode_magic_sync(&MagicSyncState::default()).unwrap();
        tag[12] = 9;
        assert_eq!(decode_magic_sync(&tag), None);

        // Cooldown count above the wire cap.
        let mut count = encode_magic_sync(&MagicSyncState::default()).unwrap();
        // varint for 513 = 0x81 0x04; splice after tag byte (offset 13).
        count.splice(13..14, [0x81, 0x04]);
        assert_eq!(decode_magic_sync(&count), None);

        // Duplicate / out-of-order cooldown ids: the encoder always writes
        // sorted order, so craft a non-canonical frame by hand.
        let mut frame = Vec::new();
        frame.extend_from_slice(MAGIC_SYNC_MAGIC);
        write_f32(&mut frame, 0.0);
        write_f32(&mut frame, 0.0);
        frame.push(CAST_TAG_NONE);
        write_varint(&mut frame, 2);
        write_mc_string(&mut frame, "b");
        write_varint(&mut frame, 1);
        write_mc_string(&mut frame, "a");
        write_varint(&mut frame, 2);
        write_varint(&mut frame, 0); // statuses
        assert_eq!(decode_magic_sync(&frame), None);
    }
}
