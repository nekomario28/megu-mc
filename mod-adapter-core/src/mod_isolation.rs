// SPDX-License-Identifier: LGPL-3.0-or-later
#![forbid(unsafe_code)]
//! `megu_core::mod_isolation` — zero-crash WASM/native mod quarantine plus a
//! local-only, content-addressed world time-machine.
//!
//! Two cooperating primitives, both pure `std` and clock-free (callers supply
//! the monotonic `tick` / `created_unix`):
//!
//! * [`ModQuarantineSupervisor`] watches the sandboxed-mod fleet. A mod that
//!   traps, exhausts its WASM fuel meter, exceeds its memory cap, attempts a
//!   disallowed host call, or repeatedly misses its tick deadline is
//!   quarantined — *only that mod* — while the live world and every healthy
//!   mod keep running uninterrupted. Fault classes split into transient
//!   strikes ([`ModFaultKind::FuelExhausted`], [`ModFaultKind::RepeatedTimeout`]
//!   — three by default, decayed by healthy ticks) and hard faults
//!   ([`ModFaultKind::TrapOrPanic`], [`ModFaultKind::MemoryLimitExceeded`],
//!   [`ModFaultKind::DisallowedHostCall`] — quarantined on the first strike).
//!   Every quarantine emits a bilingual EN/JA [`QuarantineToast`] with a
//!   deterministic `MEGU-E-XXXXXXXX` blame fingerprint, a redacted one-line
//!   detail hint, and the three player-facing [`ToastChoice`] actions
//!   ([`ToastChoice::ViewLog`], [`ToastChoice::KeepDisabled`],
//!   [`ToastChoice::RollbackPreModSnapshot`]). Raw stack traces never reach
//!   the toast surface.
//! * [`WorldTimeMachine`] keeps local rollback snapshots so a bad mod
//!   install/update/removal is a one-click undo instead of a lost world.
//!   Before any [`SnapshotTrigger`] event it walks the world directory into a
//!   content-addressed manifest: every file is SHA-256'd
//!   ([`megu_core::crypto`]) and each unique blob is stored exactly once under
//!   `objects/<hex>`, so unchanged files cost zero extra disk. A strict
//!   purity & secret denylist refuses `*.jar`, `launcher_accounts.json`,
//!   `accounts.json`, `*.token`, symlinks, and any path-escaping entry —
//!   secrets and binaries are never copied into the store. Retention is
//!   bounded by `max_snapshots` and `max_total_bytes`, and unreferenced blobs
//!   are reclaimed by a mark-and-sweep GC. [`WorldTimeMachine::restore_snapshot`]
//!   is atomic: every blob's SHA-256 is re-verified *before* a single byte of
//!   the target is touched, the restored tree is staged in a sibling
//!   directory, and the swap is a pair of directory renames — a corrupt blob
//!   fails closed and never partially overwrites a world.
//!
//! Snapshots stay on the local disk only; external modpack/world sharing
//! remains the standard `.mrpack` pipeline — this module deliberately defines
//! no distribution format.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use megu_core::crash_report::{redact_sensitive, CrashContext};
use megu_core::crypto::{hex_encode, Sha256};
use megu_core::json::{parse_json_with_limits, JsonLimits, JsonValue};
use megu_core::version::TargetVersion;

// ============================================================================
//  Part 1 — Zero-crash mod isolation & auto-quarantine
// ============================================================================

/// The sandbox failure classes the supervisor acts on. Each kind maps to a
/// strike budget in [`ModQuarantineConfig`]: transient kinds are forgiven a
/// few times (they are usually unlucky scheduling, not malice) while hard
/// kinds quarantine on first contact.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ModFaultKind {
    /// The WASM fuel meter ran dry mid-execution — usually an unbounded
    /// loop; occasionally just an unlucky tick. Transient.
    FuelExhausted,
    /// A WASM trap or native panic was caught inside the sandbox boundary.
    /// Hard fault: quarantine immediately.
    TrapOrPanic,
    /// The mod exceeded its linear-memory / allocation cap. Hard fault.
    MemoryLimitExceeded,
    /// The mod attempted a host call outside its granted capability set.
    /// Hard fault — this is the sandbox perimeter itself.
    DisallowedHostCall,
    /// The mod missed its per-tick deadline repeatedly. Transient.
    RepeatedTimeout,
}

impl ModFaultKind {
    /// Stable lowercase token used in manifests, logs, and fingerprints.
    pub fn slug(self) -> &'static str {
        match self {
            Self::FuelExhausted => "fuel_exhausted",
            Self::TrapOrPanic => "trap_or_panic",
            Self::MemoryLimitExceeded => "memory_limit_exceeded",
            Self::DisallowedHostCall => "disallowed_host_call",
            Self::RepeatedTimeout => "repeated_timeout",
        }
    }

    /// Whether the fault is transient (forgiven up to its strike budget and
    /// decayed by healthy ticks) rather than an immediate-quarantine hard
    /// fault.
    pub fn is_transient(self) -> bool {
        matches!(self, Self::FuelExhausted | Self::RepeatedTimeout)
    }
}

/// The lifecycle position of one registered sandboxed mod.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SandboxModState {
    /// Running normally inside its sandbox.
    Active,
    /// Still running, but carrying `strikes` recorded transient faults.
    /// Reaching the kind's strike budget quarantines the mod; sustained
    /// healthy ticks decay the strikes back toward `Active`.
    Degraded { strikes: u32 },
    /// Removed from the live world after a fault. `reason` is the fault
    /// class that tripped the budget, `tick` the world tick it happened on,
    /// and `detail` a redacted one-line hint for logs — never a stack trace.
    Quarantined {
        reason: ModFaultKind,
        tick: u64,
        detail: String,
    },
    /// The player chose to keep the mod unloaded; identical containment to
    /// `Quarantined` but only [`ModQuarantineSupervisor::re_enable_mod`]
    /// brings it back.
    DisabledByUser,
}

impl SandboxModState {
    /// Whether the mod still executes in the live world (`Active` or
    /// `Degraded`). Quarantined and user-disabled mods are contained.
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Active | Self::Degraded { .. })
    }
}

/// One action button on a [`QuarantineToast`]. The UI renders these in
/// order; every choice is safe for a non-technical player.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToastChoice {
    /// Open the mod's redacted sandbox log for the curious.
    ViewLog,
    /// Accept the quarantine and leave the mod unloaded.
    KeepDisabled,
    /// Roll the world back to the pre-mod snapshot taken by
    /// [`WorldTimeMachine`].
    RollbackPreModSnapshot,
}

/// The player-facing notification emitted when a mod is quarantined. Both
/// fields of each text pair are fully rendered (`{mod}` already substituted)
/// so the UI is a pure presenter, and `detail` is a redacted single line —
/// raw stack traces are deliberately not part of this struct.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuarantineToast {
    /// The quarantined mod id.
    pub mod_id: String,
    /// Deterministic `MEGU-E-XXXXXXXX` blame fingerprint, so duplicate
    /// incidents stack onto one operator-facing ID.
    pub fingerprint: String,
    /// The fault class that quarantined the mod.
    pub reason: ModFaultKind,
    /// The world tick the quarantine happened on.
    pub tick: u64,
    /// Short English headline, e.g. `Mod "create" was quarantined`.
    pub title_en: String,
    /// Short Japanese headline, e.g. `Mod「create」を隔離しました`.
    pub title_ja: String,
    /// One-sentence English summary of what happened and that the world is
    /// still running.
    pub summary_en: String,
    /// One-sentence Japanese summary mirroring `summary_en`.
    pub summary_ja: String,
    /// Redacted, single-line detail hint for the log viewer — never a stack
    /// trace.
    pub detail: String,
    /// The rendered action buttons, always
    /// `[ViewLog, KeepDisabled, RollbackPreModSnapshot]`.
    pub choices: Vec<ToastChoice>,
}

/// What [`ModQuarantineSupervisor::record_fault`] decided for one report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FaultOutcome {
    /// A strike was recorded; the mod keeps running under
    /// `SandboxModState::Degraded`. `strikes`/`max_strikes` let the HUD show
    /// a "warning: N of M" meter.
    Degraded { strikes: u32, max_strikes: u32 },
    /// The mod crossed its strike budget and was quarantined; the attached
    /// toast is also queued for [`ModQuarantineSupervisor::drain_toasts`].
    Quarantined(QuarantineToast),
    /// The mod was already quarantined or disabled — the fault is noted but
    /// no new toast is emitted and no counter moves.
    AlreadyContained,
}

/// Errors from the supervisor's explicit state-changing calls.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QuarantineError {
    /// The mod id was never registered and carries no record.
    UnknownMod(String),
}

impl fmt::Display for QuarantineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownMod(id) => write!(f, "unknown sandboxed mod: {id:?}"),
        }
    }
}
impl std::error::Error for QuarantineError {}

/// Tuning knobs for [`ModQuarantineSupervisor`]. All values are sanitized on
/// construction so a degenerate config fails closed (every bound at least
/// `1`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModQuarantineConfig {
    /// Fuel-exhaustion strikes before quarantine (default 3) — a dry fuel
    /// meter is usually an unbounded loop but occasionally just unlucky.
    pub fuel_exhaustion_max_strikes: u32,
    /// Repeated-timeout strikes before quarantine (default 3).
    pub timeout_max_strikes: u32,
    /// Strikes tolerated for hard faults — trap/panic, memory-cap breach,
    /// disallowed host call — before quarantine (default 1: immediately).
    pub hard_fault_max_strikes: u32,
    /// Consecutive healthy ticks that decay one transient strike
    /// (default 100 = 5 s at 20 TPS).
    pub healthy_ticks_per_decay: u32,
    /// Hard bound on simultaneously tracked mods (default 1_024). Eviction
    /// only ever drops `Active` (then `Degraded`) records — quarantined and
    /// user-disabled mods are never evicted, so a flood of fresh mod ids
    /// cannot push a live quarantine out of the table.
    pub max_tracked_mods: usize,
}

impl Default for ModQuarantineConfig {
    fn default() -> Self {
        Self {
            fuel_exhaustion_max_strikes: 3,
            timeout_max_strikes: 3,
            hard_fault_max_strikes: 1,
            healthy_ticks_per_decay: 100,
            max_tracked_mods: 1_024,
        }
    }
}

impl ModQuarantineConfig {
    fn sanitized(mut self) -> Self {
        self.fuel_exhaustion_max_strikes = self.fuel_exhaustion_max_strikes.max(1);
        self.timeout_max_strikes = self.timeout_max_strikes.max(1);
        self.hard_fault_max_strikes = self.hard_fault_max_strikes.max(1);
        self.healthy_ticks_per_decay = self.healthy_ticks_per_decay.max(1);
        self.max_tracked_mods = self.max_tracked_mods.max(1);
        self
    }

    fn budget_for(&self, kind: ModFaultKind) -> u32 {
        match kind {
            ModFaultKind::FuelExhausted => self.fuel_exhaustion_max_strikes,
            ModFaultKind::RepeatedTimeout => self.timeout_max_strikes,
            ModFaultKind::TrapOrPanic
            | ModFaultKind::MemoryLimitExceeded
            | ModFaultKind::DisallowedHostCall => self.hard_fault_max_strikes,
        }
    }
}

/// Per-mod bookkeeping. Created lazily on registration or first fault.
#[derive(Clone, Debug)]
struct ModRecord {
    state: SandboxModState,
    /// Consecutive healthy ticks since the last fault; every
    /// `healthy_ticks_per_decay` of them erases one transient strike.
    healthy_ticks: u32,
    /// Last tick any call touched this record — the eviction key.
    last_touch_tick: u64,
}

/// Cap on the toast `detail` line so a hostile multi-megabyte panic payload
/// cannot bloat the queue.
const TOAST_DETAIL_MAX_CHARS: usize = 240;

/// Collapse `detail` to a single redacted line of at most
/// [`TOAST_DETAIL_MAX_CHARS`] chars — what a toast may safely show.
fn sanitize_detail(detail: &str) -> String {
    let one_line: String = detail
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    let redacted = redact_sensitive(&one_line);
    let trimmed = redacted.trim();
    if trimmed.chars().count() <= TOAST_DETAIL_MAX_CHARS {
        trimmed.to_string()
    } else {
        let mut s: String = trimmed.chars().take(TOAST_DETAIL_MAX_CHARS).collect();
        s.push('…');
        s
    }
}

fn quarantine_toast(
    mod_id: &str,
    reason: ModFaultKind,
    tick: u64,
    detail: String,
) -> QuarantineToast {
    let fingerprint = CrashContext::new(
        reason.slug(),
        format!("mod:{mod_id}"),
        TargetVersion::LATEST.id(),
        detail.clone(),
    )
    .fingerprint();
    let (summary_en, summary_ja) = match reason {
        ModFaultKind::FuelExhausted => (
            "It stopped making progress inside its sandbox and was paused; your world and every other mod keep running.",
            "サンドボックス内で処理が止まったため一時停止しました。ワールドと他のModは動き続けています。",
        ),
        ModFaultKind::TrapOrPanic => (
            "It crashed inside its sandbox and was quarantined; your world and every other mod keep running.",
            "サンドボックス内でクラッシュしたため隔離しました。ワールドと他のModは動き続けています。",
        ),
        ModFaultKind::MemoryLimitExceeded => (
            "It used more memory than its sandbox allows and was quarantined; your world and every other mod keep running.",
            "サンドボックスの上限を超えるメモリを使ったため隔離しました。ワールドと他のModは動き続けています。",
        ),
        ModFaultKind::DisallowedHostCall => (
            "It tried to do something outside its permissions and was quarantined; your world and every other mod keep running.",
            "許可されていない操作を試みたため隔離しました。ワールドと他のModは動き続けています。",
        ),
        ModFaultKind::RepeatedTimeout => (
            "It repeatedly missed its tick deadline and was quarantined; your world and every other mod keep running.",
            "繰り返し処理期限を超過したため隔離しました。ワールドと他のModは動き続けています。",
        ),
    };
    QuarantineToast {
        mod_id: mod_id.to_string(),
        fingerprint,
        reason,
        tick,
        title_en: format!("Mod \"{mod_id}\" was quarantined"),
        title_ja: format!("Mod「{mod_id}」を隔離しました"),
        summary_en: summary_en.to_string(),
        summary_ja: summary_ja.to_string(),
        detail,
        choices: vec![
            ToastChoice::ViewLog,
            ToastChoice::KeepDisabled,
            ToastChoice::RollbackPreModSnapshot,
        ],
    }
}

/// The quarantine supervisor. One instance watches the whole sandboxed-mod
/// fleet; the runtime loop calls:
///
/// 1. [`register_mod`](Self::register_mod) when a mod's sandbox spins up.
/// 2. [`record_fault`](Self::record_fault) whenever the sandbox boundary
///    catches a [`ModFaultKind`] — the returned [`FaultOutcome`] says whether
///    the mod keeps running.
/// 3. [`record_healthy_tick`](Self::record_healthy_tick) once per clean tick
///    so transient strikes decay instead of accumulating forever.
///
/// [`drain_toasts`](Self::drain_toasts) hands the UI every pending
/// [`QuarantineToast`]; [`re_enable_mod`](Self::re_enable_mod) and
/// [`disable_mod`](Self::disable_mod) implement the toast choices.
#[derive(Clone, Debug)]
pub struct ModQuarantineSupervisor {
    config: ModQuarantineConfig,
    mods: HashMap<String, ModRecord>,
    pending_toasts: Vec<QuarantineToast>,
}

impl Default for ModQuarantineSupervisor {
    fn default() -> Self {
        Self::new(ModQuarantineConfig::default())
    }
}

impl ModQuarantineSupervisor {
    /// A supervisor with `config` (sanitized into the fail-closed contract).
    pub fn new(config: ModQuarantineConfig) -> Self {
        Self {
            config: config.sanitized(),
            mods: HashMap::new(),
            pending_toasts: Vec::new(),
        }
    }

    /// The sanitized configuration in force.
    pub fn config(&self) -> &ModQuarantineConfig {
        &self.config
    }

    /// Number of mods currently tracked — always `<= max_tracked_mods`.
    pub fn tracked_mod_count(&self) -> usize {
        self.mods.len()
    }

    /// Register a mod as `Active` when its sandbox spins up. Idempotent:
    /// re-registering an already-tracked mod leaves its state untouched (a
    /// quarantined mod stays quarantined until re-enabled).
    pub fn register_mod(&mut self, mod_id: &str, tick: u64) {
        if self.mods.contains_key(mod_id) {
            if let Some(r) = self.mods.get_mut(mod_id) {
                r.last_touch_tick = tick;
            }
            return;
        }
        self.make_room();
        self.mods.insert(
            mod_id.to_string(),
            ModRecord {
                state: SandboxModState::Active,
                healthy_ticks: 0,
                last_touch_tick: tick,
            },
        );
    }

    /// Drop a mod's record entirely (its sandbox was torn down). Removing a
    /// quarantined mod is allowed — the toast was already emitted.
    pub fn unregister_mod(&mut self, mod_id: &str) {
        self.mods.remove(mod_id);
    }

    /// The current state for `mod_id`, or `None` if never registered and
    /// never faulted.
    pub fn mod_state(&self, mod_id: &str) -> Option<SandboxModState> {
        self.mods.get(mod_id).map(|r| r.state.clone())
    }

    /// Convenience check for the tick loop: `true` while the mod may execute
    /// (unknown mods count as running — they have no strikes).
    pub fn is_running(&self, mod_id: &str) -> bool {
        self.mods
            .get(mod_id)
            .map(|r| r.state.is_running())
            .unwrap_or(true)
    }

    /// Report one sandbox-caught fault for `mod_id` at `tick`. `detail` is
    /// the raw boundary detail — it is redacted and collapsed to a single
    /// line before it can reach any player-facing surface.
    ///
    /// The outcome is computed even when the record table is full and the
    /// record could not be stored, so a scope flood can never make a
    /// quarantine decision silently disappear.
    pub fn record_fault(
        &mut self,
        mod_id: &str,
        kind: ModFaultKind,
        tick: u64,
        detail: &str,
    ) -> FaultOutcome {
        let budget = self.config.budget_for(kind);
        if self
            .mods
            .get(mod_id)
            .map(|r| !r.state.is_running())
            .unwrap_or(false)
        {
            return FaultOutcome::AlreadyContained;
        }

        let strikes = match self.mods.get(mod_id).map(|r| &r.state) {
            Some(SandboxModState::Degraded { strikes }) => *strikes,
            _ => 0,
        }
        .saturating_add(1);

        if strikes < budget {
            if let Some(r) = self.mods.get_mut(mod_id) {
                r.state = SandboxModState::Degraded { strikes };
                r.healthy_ticks = 0;
                r.last_touch_tick = tick;
            } else {
                self.make_room();
                if self.mods.len() < self.config.max_tracked_mods {
                    self.mods.insert(
                        mod_id.to_string(),
                        ModRecord {
                            state: SandboxModState::Degraded { strikes },
                            healthy_ticks: 0,
                            last_touch_tick: tick,
                        },
                    );
                }
            }
            return FaultOutcome::Degraded {
                strikes,
                max_strikes: budget,
            };
        }

        // Quarantine — the decision is made before (and independently of)
        // whether the record table had room to store it.
        let clean_detail = sanitize_detail(detail);
        let toast = quarantine_toast(mod_id, kind, tick, clean_detail.clone());
        if let Some(r) = self.mods.get_mut(mod_id) {
            r.state = SandboxModState::Quarantined {
                reason: kind,
                tick,
                detail: clean_detail,
            };
            r.healthy_ticks = 0;
            r.last_touch_tick = tick;
        } else {
            self.make_room();
            if self.mods.len() < self.config.max_tracked_mods {
                self.mods.insert(
                    mod_id.to_string(),
                    ModRecord {
                        state: SandboxModState::Quarantined {
                            reason: kind,
                            tick,
                            detail: clean_detail,
                        },
                        healthy_ticks: 0,
                        last_touch_tick: tick,
                    },
                );
            }
        }
        self.pending_toasts.push(toast.clone());
        FaultOutcome::Quarantined(toast)
    }

    /// Report one clean tick for `mod_id`. Every `healthy_ticks_per_decay`
    /// consecutive clean ticks erase one transient strike; a `Degraded` mod
    /// that decays to zero returns to `Active`. No-op for unknown,
    /// quarantined, or disabled mods.
    pub fn record_healthy_tick(&mut self, mod_id: &str, tick: u64) {
        let Some(r) = self.mods.get_mut(mod_id) else {
            return;
        };
        r.last_touch_tick = tick;
        let SandboxModState::Degraded { strikes } = r.state else {
            return;
        };
        r.healthy_ticks = r.healthy_ticks.saturating_add(1);
        if r.healthy_ticks >= self.config.healthy_ticks_per_decay {
            r.healthy_ticks = 0;
            let strikes = strikes.saturating_sub(1);
            r.state = if strikes == 0 {
                SandboxModState::Active
            } else {
                SandboxModState::Degraded { strikes }
            };
        }
    }

    /// Execute one mod step under quarantine supervision: a clean step
    /// calls [`Self::record_healthy_tick`] to decay transient strikes, while
    /// a fault routes through [`Self::record_fault`].
    pub fn run_mod_guarded<T>(
        &mut self,
        mod_id: &str,
        tick: u64,
        f: impl FnOnce() -> Result<T, (ModFaultKind, String)>,
    ) -> Result<T, Box<FaultOutcome>> {
        match f() {
            Ok(value) => {
                self.record_healthy_tick(mod_id, tick);
                Ok(value)
            }
            Err((kind, detail)) => Err(Box::new(self.record_fault(mod_id, kind, tick, &detail))),
        }
    }

    /// Bring a quarantined or user-disabled mod back to `Active` (toast
    /// choice handling, or an operator's explicit re-enable). Re-enabling a
    /// `Degraded` mod clears its strikes; re-enabling `Active` is a no-op.
    pub fn re_enable_mod(
        &mut self,
        mod_id: &str,
        tick: u64,
    ) -> Result<SandboxModState, QuarantineError> {
        let Some(r) = self.mods.get_mut(mod_id) else {
            return Err(QuarantineError::UnknownMod(mod_id.to_string()));
        };
        r.state = SandboxModState::Active;
        r.healthy_ticks = 0;
        r.last_touch_tick = tick;
        Ok(SandboxModState::Active)
    }

    /// The `KeepDisabled` toast choice / explicit operator disable: the mod
    /// is contained until [`re_enable_mod`](Self::re_enable_mod).
    pub fn disable_mod(&mut self, mod_id: &str, tick: u64) -> Result<(), QuarantineError> {
        let Some(r) = self.mods.get_mut(mod_id) else {
            return Err(QuarantineError::UnknownMod(mod_id.to_string()));
        };
        r.state = SandboxModState::DisabledByUser;
        r.healthy_ticks = 0;
        r.last_touch_tick = tick;
        Ok(())
    }

    /// Take every queued [`QuarantineToast`] for the UI, oldest first.
    pub fn drain_toasts(&mut self) -> Vec<QuarantineToast> {
        std::mem::take(&mut self.pending_toasts)
    }

    /// Ids of every mod currently contained (quarantined or user-disabled),
    /// sorted for deterministic UI lists.
    pub fn contained_mods(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .mods
            .iter()
            .filter(|(_, r)| !r.state.is_running())
            .map(|(id, _)| id.clone())
            .collect();
        ids.sort();
        ids
    }

    /// Evict the least-recently-touched evictable record when the table is
    /// at capacity. `Quarantined` and `DisabledByUser` records are never
    /// evicted; `Active` goes first, then `Degraded`.
    fn make_room(&mut self) {
        if self.mods.len() < self.config.max_tracked_mods {
            return;
        }
        let evict = self
            .mods
            .iter()
            .filter(|(_, r)| matches!(r.state, SandboxModState::Active))
            .min_by_key(|(_, r)| r.last_touch_tick)
            .map(|(id, _)| id.clone())
            .or_else(|| {
                self.mods
                    .iter()
                    .filter(|(_, r)| matches!(r.state, SandboxModState::Degraded { .. }))
                    .min_by_key(|(_, r)| r.last_touch_tick)
                    .map(|(id, _)| id.clone())
            });
        if let Some(id) = evict {
            self.mods.remove(&id);
        }
    }
}

// ============================================================================
//  Part 2 — Local-only automatic world time-machine
// ============================================================================

/// Why a snapshot was taken. Stored in the manifest so the rollback UI can
/// group "before mod X was added" points.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotTrigger {
    /// A mod was added to the world/profile.
    ModAdded,
    /// A mod was removed.
    ModRemoved,
    /// A mod's code/version changed.
    ModUpdated,
    /// The player pressed "snapshot now".
    Manual,
    /// The launcher itself is about to upgrade.
    PreUpgrade,
}

impl SnapshotTrigger {
    /// Stable lowercase token stored in manifests.
    pub fn slug(self) -> &'static str {
        match self {
            Self::ModAdded => "mod_added",
            Self::ModRemoved => "mod_removed",
            Self::ModUpdated => "mod_updated",
            Self::Manual => "manual",
            Self::PreUpgrade => "pre_upgrade",
        }
    }

    /// Inverse of [`slug`](Self::slug); unknown tokens are `None`.
    pub fn from_slug(raw: &str) -> Option<Self> {
        match raw {
            "mod_added" => Some(Self::ModAdded),
            "mod_removed" => Some(Self::ModRemoved),
            "mod_updated" => Some(Self::ModUpdated),
            "manual" => Some(Self::Manual),
            "pre_upgrade" => Some(Self::PreUpgrade),
            _ => None,
        }
    }
}

/// Why [`WorldTimeMachine::create_snapshot`] declined to store a path.
/// Skipped paths are listed in the manifest so a rollback can report
/// "N files were intentionally not part of this snapshot".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkipReason {
    /// Basename matched the purity & secret denylist (`*.jar`,
    /// `launcher_accounts.json`, `accounts.json`, `*.token`).
    DenylistedName,
    /// A symlink — the time-machine never follows or stores links.
    Symlink,
    /// Neither a regular file nor a directory (fifo, socket, device).
    NonRegularFile,
    /// The path is not valid UTF-8 and cannot be represented in a manifest.
    NonUtf8Path,
}

impl SkipReason {
    fn slug(self) -> &'static str {
        match self {
            Self::DenylistedName => "denylisted",
            Self::Symlink => "symlink",
            Self::NonRegularFile => "non_regular",
            Self::NonUtf8Path => "non_utf8",
        }
    }

    fn from_slug(raw: &str) -> Option<Self> {
        match raw {
            "denylisted" => Some(Self::DenylistedName),
            "symlink" => Some(Self::Symlink),
            "non_regular" => Some(Self::NonRegularFile),
            "non_utf8" => Some(Self::NonUtf8Path),
            _ => None,
        }
    }
}

/// One world-relative path the snapshot walker declined to store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkippedPath {
    /// World-relative `/`-joined path that was skipped.
    pub path: String,
    /// Why it was skipped.
    pub reason: SkipReason,
}

/// One file captured by a snapshot manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotFileEntry {
    /// World-relative `/`-joined path.
    pub path: String,
    /// Lowercase hex SHA-256 of the file's contents — also the blob's name
    /// under `objects/`.
    pub sha256: String,
    /// File length in bytes.
    pub len: u64,
}

/// A stored snapshot: identity, provenance, and the file manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotRecord {
    /// `snap-<16 hex>` — the content-derived snapshot id.
    pub id: String,
    /// Monotonic store-local sequence number used for retention ordering.
    pub seq: u64,
    /// Why the snapshot was taken.
    pub trigger: SnapshotTrigger,
    /// Caller-supplied UNIX timestamp.
    pub created_unix: u64,
    /// The captured files, sorted by path for deterministic manifests.
    pub files: Vec<SnapshotFileEntry>,
    /// Paths the walker declined to store (denylist, symlink, ...).
    pub skipped: Vec<SkippedPath>,
    /// Sum of `len` over `files` (logical bytes, before blob dedup).
    pub total_bytes: u64,
}

/// What [`WorldTimeMachine::restore_snapshot`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoreReport {
    /// The snapshot that was restored.
    pub snapshot_id: String,
    /// Files written into the target.
    pub restored_files: usize,
    /// Bytes written into the target.
    pub restored_bytes: u64,
    /// Where the previous contents of `target_dir` were moved — kept, never
    /// deleted, so a mistaken rollback is itself undoable. `None` when the
    /// target did not exist.
    pub backup_dir: Option<PathBuf>,
}

/// Everything that can fail in the time-machine. Variants carry enough
/// context to log without re-walking the store.
#[derive(Debug)]
pub enum TimeMachineError {
    /// A filesystem operation failed.
    Io {
        /// What was being attempted (`"scan"`, `"read"`, `"write"`, ...).
        op: &'static str,
        /// The path involved.
        path: PathBuf,
        /// The underlying error.
        source: io::Error,
    },
    /// No manifest exists for the requested snapshot id.
    SnapshotNotFound {
        /// The requested id.
        id: String,
    },
    /// A manifest on disk could not be parsed or is missing required fields.
    CorruptManifest {
        /// The manifest's snapshot id (or filename when unparsable).
        id: String,
        /// What was wrong.
        detail: String,
    },
    /// A manifest contained a path that is absolute, contains `..` or
    /// drive/separator tricks, or otherwise escapes the target.
    UnsafeManifestPath {
        /// The manifest's snapshot id.
        id: String,
        /// The offending stored path.
        path: String,
    },
    /// A manifest references a blob that is not in `objects/`.
    MissingBlob {
        /// The expected blob digest (hex).
        sha256: String,
    },
    /// A stored blob's actual SHA-256 does not match its name — the store
    /// is corrupted. Restore fails closed before touching the target.
    BlobChecksumMismatch {
        /// The blob name (expected digest, hex).
        expected: String,
        /// The digest the blob actually hashes to.
        actual: String,
    },
    /// One file exceeded `max_file_bytes`.
    FileTooLarge {
        /// World-relative path of the oversized file.
        path: String,
        /// Its length in bytes.
        len: u64,
        /// The configured cap.
        cap: u64,
    },
    /// The world held more files than `max_files_per_snapshot`.
    TooManyFiles {
        /// The configured cap.
        cap: usize,
    },
}

impl fmt::Display for TimeMachineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { op, path, source } => {
                write!(
                    f,
                    "time-machine {op} failed on {}: {source}",
                    path.display()
                )
            }
            Self::SnapshotNotFound { id } => write!(f, "snapshot {id:?} not found"),
            Self::CorruptManifest { id, detail } => {
                write!(f, "snapshot manifest {id:?} is corrupt: {detail}")
            }
            Self::UnsafeManifestPath { id, path } => {
                write!(f, "snapshot {id:?} contains unsafe path {path:?}")
            }
            Self::MissingBlob { sha256 } => {
                write!(f, "snapshot blob objects/{sha256} is missing")
            }
            Self::BlobChecksumMismatch { expected, actual } => write!(
                f,
                "blob integrity failure: objects/{expected} hashes to {actual}"
            ),
            Self::FileTooLarge { path, len, cap } => {
                write!(f, "file {path:?} is {len} bytes, over the {cap}-byte cap")
            }
            Self::TooManyFiles { cap } => {
                write!(f, "world has more than {cap} files")
            }
        }
    }
}
impl std::error::Error for TimeMachineError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Retention and bound knobs for [`WorldTimeMachine`]. Sanitized on
/// construction so zero-configs fail closed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimeMachineConfig {
    /// Newest snapshots kept (default 10). Older manifests are pruned after
    /// each snapshot.
    pub max_snapshots: usize,
    /// Ceiling on the sum of *unique referenced blob bytes* across retained
    /// snapshots (default 8 GiB). When exceeded, oldest snapshots are
    /// pruned until the live set fits, then the mark-and-sweep GC frees the
    /// orphaned blobs.
    pub max_total_bytes: u64,
    /// Largest single file admitted to the store (default 512 MiB); larger
    /// files fail the snapshot closed rather than blowing the budget.
    pub max_file_bytes: u64,
    /// Largest file count a manifest may hold (default 65_536) so a hostile
    /// directory flood cannot produce an unbounded manifest.
    pub max_files_per_snapshot: usize,
}

impl Default for TimeMachineConfig {
    fn default() -> Self {
        Self {
            max_snapshots: 10,
            max_total_bytes: 8 * 1024 * 1024 * 1024,
            max_file_bytes: 512 * 1024 * 1024,
            max_files_per_snapshot: 65_536,
        }
    }
}

impl TimeMachineConfig {
    fn sanitized(mut self) -> Self {
        self.max_snapshots = self.max_snapshots.max(1);
        self.max_total_bytes = self.max_total_bytes.max(1);
        self.max_file_bytes = self.max_file_bytes.max(1);
        self.max_files_per_snapshot = self.max_files_per_snapshot.max(1);
        self
    }
}

/// `true` when a file/dir *basename* is on the purity & secret denylist:
/// `*.jar` (mod binaries belong to the `.mrpack` lane, not world blobs),
/// `launcher_accounts.json` / `accounts.json` (credentials), and `*.token`
/// (session tokens). Matching is case-insensitive and applies to directory
/// names too — a denylisted directory skips its whole subtree.
fn denylisted_basename(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower == "launcher_accounts.json"
        || lower == "accounts.json"
        || lower.ends_with(".jar")
        || lower.ends_with(".token")
}

/// Render a relative `Path` as a portable `/`-joined UTF-8 string, or
/// `None` when it has non-normal components or non-UTF-8 parts.
fn rel_to_manifest(rel: &Path) -> Option<String> {
    let mut parts = Vec::new();
    for c in rel.components() {
        match c {
            std::path::Component::Normal(os) => parts.push(os.to_str()?.to_string()),
            _ => return None,
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("/"))
}

/// Fail-closed check for a manifest-supplied relative path before it is
/// joined onto a target directory at restore time. Rejects empty segments,
/// `.`/`..`, drive letters, UNC/separator tricks and leading `/`.
fn is_safe_manifest_path(p: &str) -> bool {
    !p.is_empty()
        && !p.starts_with('/')
        && p.split('/').all(|seg| {
            !seg.is_empty()
                && seg != "."
                && seg != ".."
                && !seg.contains('\\')
                && !seg.contains(':')
        })
}

/// The content-addressed snapshot store. Layout under `store_dir`:
///
/// ```text
/// objects/<64-hex-sha256>          one file per unique blob, named by digest
/// snapshots/<id>.json              canonical-JSON manifests
/// ```
///
/// Both writes (blob ingest, manifest publish) go through a same-directory
/// tmp file + rename so a crash mid-write can never leave a half-file.
#[derive(Clone, Debug)]
pub struct WorldTimeMachine {
    store_dir: PathBuf,
    config: TimeMachineConfig,
}

impl WorldTimeMachine {
    /// A time-machine rooted at `store_dir` (created on first snapshot).
    pub fn new(store_dir: impl Into<PathBuf>, config: TimeMachineConfig) -> Self {
        Self {
            store_dir: store_dir.into(),
            config: config.sanitized(),
        }
    }

    /// The sanitized configuration in force.
    pub fn config(&self) -> &TimeMachineConfig {
        &self.config
    }

    /// The store root.
    pub fn store_dir(&self) -> &Path {
        &self.store_dir
    }

    fn objects_dir(&self) -> PathBuf {
        self.store_dir.join("objects")
    }

    fn snapshots_dir(&self) -> PathBuf {
        self.store_dir.join("snapshots")
    }

    fn manifest_path(&self, id: &str) -> PathBuf {
        self.snapshots_dir().join(format!("{id}.json"))
    }

    fn io_err(op: &'static str, path: PathBuf) -> impl FnOnce(io::Error) -> TimeMachineError {
        move |source| TimeMachineError::Io { op, path, source }
    }

    /// Capture `world_dir` as a new snapshot attributed to `trigger`.
    /// `created_unix` is caller-supplied so the module stays clock-free.
    ///
    /// The snapshot id is derived from the manifest content (trigger,
    /// created timestamp, file list, skip list): re-capturing the identical
    /// state returns the existing record instead of writing a duplicate.
    /// Retention (`max_snapshots`, `max_total_bytes`) and the mark-and-sweep
    /// GC run after every successful capture.
    pub fn create_snapshot(
        &mut self,
        world_dir: &Path,
        trigger: SnapshotTrigger,
        created_unix: u64,
    ) -> Result<SnapshotRecord, TimeMachineError> {
        fs::create_dir_all(self.objects_dir())
            .map_err(Self::io_err("mkdir", self.objects_dir()))?;
        fs::create_dir_all(self.snapshots_dir())
            .map_err(Self::io_err("mkdir", self.snapshots_dir()))?;

        let mut files = Vec::new();
        let mut skipped = Vec::new();
        let mut cursor = 0u64;
        self.walk(world_dir, world_dir, &mut files, &mut skipped, &mut cursor)?;
        files.sort_by(|a, b| a.path.cmp(&b.path));
        skipped.sort_by(|a, b| a.path.cmp(&b.path));
        let total_bytes = files.iter().map(|f| f.len).fold(0u64, u64::saturating_add);

        let seq = self
            .list_snapshots()?
            .iter()
            .map(|s| s.seq)
            .max()
            .unwrap_or(0)
            .saturating_add(1);

        // Digest the content identity — trigger, timestamp, files, skips —
        // but not the seq/id fields themselves (seq is pinned to 0 in the
        // preimage so the same world state always derives the same id).
        let preimage =
            manifest_json(None, 0, trigger, created_unix, &files, &skipped).to_json_string();
        let id = format!(
            "snap-{}",
            &megu_core::crypto::sha256_hex(preimage.as_bytes())[..16]
        );

        let path = self.manifest_path(&id);
        if path.exists() {
            // Identical content already captured — idempotent.
            if let Ok(existing) = self.load_manifest(&path) {
                return Ok(existing);
            }
        }
        let record = SnapshotRecord {
            id: id.clone(),
            seq,
            trigger,
            created_unix,
            files,
            skipped,
            total_bytes,
        };
        let body = manifest_json(
            Some(&record.id),
            record.seq,
            record.trigger,
            record.created_unix,
            &record.files,
            &record.skipped,
        )
        .to_json_string();
        write_atomic(&path, body.as_bytes()).map_err(Self::io_err("write", path.clone()))?;
        self.enforce_retention()?;
        Ok(record)
    }

    /// Recursive walker. Denylisted basenames and symlinks are skipped
    /// (recorded, never followed); regular files are hashed once and their
    /// blob stored under `objects/<hex>` only when that digest is new —
    /// unchanged content is never rewritten.
    fn walk(
        &self,
        root: &Path,
        dir: &Path,
        files: &mut Vec<SnapshotFileEntry>,
        skipped: &mut Vec<SkippedPath>,
        cursor: &mut u64,
    ) -> Result<(), TimeMachineError> {
        let mut entries: Vec<fs::DirEntry> = fs::read_dir(dir)
            .map_err(Self::io_err("scan", dir.to_path_buf()))?
            .collect::<Result<_, _>>()
            .map_err(Self::io_err("scan", dir.to_path_buf()))?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let name_os = entry.file_name();
            let path = entry.path();
            let ft = entry
                .file_type()
                .map_err(Self::io_err("stat", path.clone()))?;
            let rel = match rel_to_manifest(match path.strip_prefix(root) {
                Ok(r) => r,
                Err(_) => continue,
            }) {
                Some(r) => r,
                None => {
                    skipped.push(SkippedPath {
                        path: name_os.to_string_lossy().into_owned(),
                        reason: SkipReason::NonUtf8Path,
                    });
                    continue;
                }
            };
            let name = name_os.to_string_lossy();
            if denylisted_basename(&name) {
                skipped.push(SkippedPath {
                    path: rel,
                    reason: SkipReason::DenylistedName,
                });
                continue;
            }
            if ft.is_symlink() {
                skipped.push(SkippedPath {
                    path: rel,
                    reason: SkipReason::Symlink,
                });
                continue;
            }
            if ft.is_dir() {
                self.walk(root, &path, files, skipped, cursor)?;
            } else if ft.is_file() {
                if files.len() >= self.config.max_files_per_snapshot {
                    return Err(TimeMachineError::TooManyFiles {
                        cap: self.config.max_files_per_snapshot,
                    });
                }
                files.push(self.ingest(&path, &rel, cursor)?);
            } else {
                skipped.push(SkippedPath {
                    path: rel,
                    reason: SkipReason::NonRegularFile,
                });
            }
        }
        Ok(())
    }

    /// Stream one file: hash it and write it to a tmp object in one pass,
    /// then rename into `objects/<hex>`. Returns the manifest entry.
    fn ingest(
        &self,
        src: &Path,
        rel: &str,
        cursor: &mut u64,
    ) -> Result<SnapshotFileEntry, TimeMachineError> {
        let meta = fs::metadata(src).map_err(Self::io_err("stat", src.to_path_buf()))?;
        if meta.len() > self.config.max_file_bytes {
            return Err(TimeMachineError::FileTooLarge {
                path: rel.to_string(),
                len: meta.len(),
                cap: self.config.max_file_bytes,
            });
        }
        *cursor = cursor.saturating_add(1);
        let tmp = self
            .objects_dir()
            .join(format!(".ingest-{}-{}", std::process::id(), *cursor));
        let mut input = fs::File::open(src).map_err(Self::io_err("read", src.to_path_buf()))?;
        let mut out = fs::File::create(&tmp).map_err(Self::io_err("write", tmp.clone()))?;
        let mut hasher = Sha256::new();
        let mut total = 0u64;
        let mut chunk = [0u8; 64 * 1024];
        let result = (|input: &mut fs::File,
                       out: &mut fs::File,
                       hasher: &mut Sha256,
                       total: &mut u64|
         -> Result<(), TimeMachineError> {
            loop {
                let n = input
                    .read(&mut chunk)
                    .map_err(Self::io_err("read", src.to_path_buf()))?;
                if n == 0 {
                    break;
                }
                *total = total.saturating_add(n as u64);
                if *total > self.config.max_file_bytes {
                    return Err(TimeMachineError::FileTooLarge {
                        path: rel.to_string(),
                        len: *total,
                        cap: self.config.max_file_bytes,
                    });
                }
                hasher.update(&chunk[..n]);
                out.write_all(&chunk[..n])
                    .map_err(Self::io_err("write", tmp.clone()))?;
            }
            Ok(())
        })(&mut input, &mut out, &mut hasher, &mut total);
        drop(out);
        if let Err(e) = result {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
        let sha256 = hex_encode(&hasher.finalize());
        let dest = self.objects_dir().join(&sha256);
        if dest.exists() {
            // Blob already stored (dedup hit) — drop the tmp copy.
            let _ = fs::remove_file(&tmp);
        } else if let Err(e) = fs::rename(&tmp, &dest) {
            let _ = fs::remove_file(&tmp);
            return Err(Self::io_err("write", dest)(e));
        }
        Ok(SnapshotFileEntry {
            path: rel.to_string(),
            sha256,
            len: total,
        })
    }

    /// Read and validate one manifest file.
    fn load_manifest(&self, path: &Path) -> Result<SnapshotRecord, TimeMachineError> {
        let id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        let text = fs::read_to_string(path).map_err(Self::io_err("read", path.to_path_buf()))?;
        parse_manifest(&id, &text)
    }

    /// Every snapshot in the store, ordered by `seq` (oldest first).
    /// Unparsable manifests are reported as `CorruptManifest` rather than
    /// silently skipped — a dropped manifest would look identical to a
    /// pruned one in retention accounting.
    pub fn list_snapshots(&self) -> Result<Vec<SnapshotRecord>, TimeMachineError> {
        let dir = self.snapshots_dir();
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in fs::read_dir(&dir).map_err(Self::io_err("scan", dir.clone()))? {
            let entry = entry.map_err(Self::io_err("scan", dir.clone()))?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            out.push(self.load_manifest(&path)?);
        }
        out.sort_by_key(|s| s.seq);
        Ok(out)
    }

    /// Fetch one snapshot by id.
    pub fn get_snapshot(&self, id: &str) -> Result<SnapshotRecord, TimeMachineError> {
        let path = self.manifest_path(id);
        if !path.exists() {
            return Err(TimeMachineError::SnapshotNotFound { id: id.to_string() });
        }
        self.load_manifest(&path)
    }

    /// Prune to `max_snapshots` and `max_total_bytes`, then mark-and-sweep
    /// `objects/`: every blob still referenced by a retained manifest is
    /// marked live; everything else on disk is reclaimed.
    fn enforce_retention(&self) -> Result<(), TimeMachineError> {
        let mut snaps = self.list_snapshots()?;
        while snaps.len() > self.config.max_snapshots {
            let victim = snaps.remove(0);
            let _ = fs::remove_file(self.manifest_path(&victim.id));
        }
        loop {
            let live_bytes = unique_blob_bytes(&snaps);
            if live_bytes <= self.config.max_total_bytes || snaps.is_empty() {
                break;
            }
            let victim = snaps.remove(0);
            let _ = fs::remove_file(self.manifest_path(&victim.id));
        }
        // Mark: every sha referenced by a retained manifest stays.
        let live: HashSet<&str> = snaps
            .iter()
            .flat_map(|s| s.files.iter().map(|f| f.sha256.as_str()))
            .collect();
        // Sweep: unreferenced objects are reclaimed; non-blob names (tmp
        // files, junk) are removed too so the store stays exactly the
        // marked set.
        let objects = self.objects_dir();
        if objects.exists() {
            for entry in fs::read_dir(&objects).map_err(Self::io_err("scan", objects.clone()))? {
                let entry = entry.map_err(Self::io_err("scan", objects.clone()))?;
                let name = entry.file_name();
                let Some(name) = name.to_str() else { continue };
                if live.contains(name) {
                    continue;
                }
                let _ = fs::remove_file(entry.path());
            }
        }
        Ok(())
    }

    /// Verify one blob's content against its digest name.
    fn verify_blob(&self, sha256: &str) -> Result<(), TimeMachineError> {
        let blob = self.objects_dir().join(sha256);
        if !blob.exists() {
            return Err(TimeMachineError::MissingBlob {
                sha256: sha256.to_string(),
            });
        }
        let actual = megu_core::crypto::sha256_file(&blob).map_err(Self::io_err("read", blob))?;
        if actual != sha256 {
            return Err(TimeMachineError::BlobChecksumMismatch {
                expected: sha256.to_string(),
                actual,
            });
        }
        Ok(())
    }

    /// Atomically roll `target_dir` back to `snapshot_id`.
    ///
    /// Fail-closed ordering:
    ///
    /// 1. Load and validate the manifest — every stored path is re-checked
    ///    against [`is_safe_manifest_path`] so a hand-edited manifest cannot
    ///    write outside `target_dir`.
    /// 2. Re-verify **every** blob's SHA-256 against live bytes. A missing
    ///    or corrupt blob aborts here, before the target is touched.
    /// 3. Stage the complete tree in a sibling `*.mengu-restore-staging`
    ///    directory.
    /// 4. Swap with two directory renames: `target` → `*.mengu-pre-restore`
    ///    backup (kept for the player), `staging` → `target`. The target is
    ///    never partially overwritten; if the second rename fails the
    ///    original is moved back.
    pub fn restore_snapshot(
        &self,
        snapshot_id: &str,
        target_dir: &Path,
    ) -> Result<RestoreReport, TimeMachineError> {
        let snap = self.get_snapshot(snapshot_id)?;
        for f in &snap.files {
            if !is_safe_manifest_path(&f.path) {
                return Err(TimeMachineError::UnsafeManifestPath {
                    id: snap.id.clone(),
                    path: f.path.clone(),
                });
            }
        }
        // Pass 1: verify every blob before writing a single byte.
        for f in &snap.files {
            self.verify_blob(&f.sha256)?;
        }
        let restored_bytes = snap
            .files
            .iter()
            .map(|f| f.len)
            .fold(0u64, u64::saturating_add);

        // Pass 2: stage the full restored tree in a sibling directory.
        let parent = target_dir.parent().unwrap_or_else(|| Path::new("."));
        let base = target_dir
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("world");
        let staging = parent.join(format!(".{base}.mengu-restore-staging"));
        if staging.exists() {
            // Leftover from an interrupted restore — owned by us, safe to
            // clear before re-staging.
            fs::remove_dir_all(&staging).map_err(Self::io_err("write", staging.clone()))?;
        }
        for f in &snap.files {
            let mut out_path = staging.clone();
            for seg in f.path.split('/') {
                out_path.push(seg);
            }
            if let Some(p) = out_path.parent() {
                fs::create_dir_all(p).map_err(Self::io_err("mkdir", p.to_path_buf()))?;
            }
            fs::copy(self.objects_dir().join(&f.sha256), &out_path)
                .map_err(Self::io_err("write", out_path.clone()))?;
        }

        // Pass 3: atomic-ish swap — move the live target aside, then move
        // the staged tree into place.
        let mut backup_dir = None;
        if target_dir.exists() {
            let mut n = 0u32;
            let backup = loop {
                let cand = if n == 0 {
                    parent.join(format!("{base}.mengu-pre-restore"))
                } else {
                    parent.join(format!("{base}.mengu-pre-restore-{n}"))
                };
                if !cand.exists() {
                    break cand;
                }
                n = n.saturating_add(1);
            };
            fs::rename(target_dir, &backup)
                .map_err(Self::io_err("rename", target_dir.to_path_buf()))?;
            if let Err(e) = fs::rename(&staging, target_dir) {
                // Best effort: put the original world back where it was.
                let _ = fs::rename(&backup, target_dir);
                return Err(Self::io_err("rename", staging.clone())(e));
            }
            backup_dir = Some(backup);
        } else {
            fs::create_dir_all(parent).map_err(Self::io_err("mkdir", parent.to_path_buf()))?;
            fs::rename(&staging, target_dir).map_err(Self::io_err("rename", staging.clone()))?;
        }
        Ok(RestoreReport {
            snapshot_id: snap.id,
            restored_files: snap.files.len(),
            restored_bytes,
            backup_dir,
        })
    }
}

/// Sum of byte lengths over the deduplicated blob set referenced by
/// `snaps` — what `max_total_bytes` bounds.
fn unique_blob_bytes(snaps: &[SnapshotRecord]) -> u64 {
    let mut seen: HashMap<&str, u64> = HashMap::new();
    for s in snaps {
        for f in &s.files {
            seen.entry(f.sha256.as_str()).or_insert(f.len);
        }
    }
    seen.values().fold(0u64, |acc, &v| acc.saturating_add(v))
}

/// Serialize `record` (or its content preimage when `id` is `None`) into a
/// canonical [`JsonValue`]. Field order is fixed so serialization is
/// byte-identical across lanes.
fn manifest_json(
    id: Option<&str>,
    seq: u64,
    trigger: SnapshotTrigger,
    created_unix: u64,
    files: &[SnapshotFileEntry],
    skipped: &[SkippedPath],
) -> JsonValue {
    let mut fields = vec![
        (
            "format".to_string(),
            JsonValue::String("mengu-timemachine/1".to_string()),
        ),
        (
            "id".to_string(),
            id.map_or(JsonValue::Null, |i| JsonValue::String(i.to_string())),
        ),
        ("seq".to_string(), JsonValue::Number(seq as f64)),
        (
            "trigger".to_string(),
            JsonValue::String(trigger.slug().to_string()),
        ),
        (
            "created_unix".to_string(),
            JsonValue::Number(created_unix as f64),
        ),
        (
            "total_bytes".to_string(),
            JsonValue::Number(files.iter().map(|f| f.len).fold(0u64, u64::saturating_add) as f64),
        ),
    ];
    let file_arr: Vec<JsonValue> = files
        .iter()
        .map(|f| {
            JsonValue::Object(vec![
                ("path".to_string(), JsonValue::String(f.path.clone())),
                ("sha256".to_string(), JsonValue::String(f.sha256.clone())),
                ("len".to_string(), JsonValue::Number(f.len as f64)),
            ])
        })
        .collect();
    fields.push(("files".to_string(), JsonValue::Array(file_arr)));
    let skip_arr: Vec<JsonValue> = skipped
        .iter()
        .map(|s| {
            JsonValue::Object(vec![
                ("path".to_string(), JsonValue::String(s.path.clone())),
                (
                    "reason".to_string(),
                    JsonValue::String(s.reason.slug().to_string()),
                ),
            ])
        })
        .collect();
    fields.push(("skipped".to_string(), JsonValue::Array(skip_arr)));
    JsonValue::Object(fields)
}

/// Manifest parse limits: manifests are machine-written and bounded by
/// `max_files_per_snapshot`, but a hand-edited or hostile file must not be
/// able to exhaust memory — 8-deep nesting and 1M nodes are far above any
/// legitimate manifest, 128 MiB caps the raw input.
const MANIFEST_LIMITS: JsonLimits = JsonLimits {
    max_depth: 8,
    max_nodes: 1_000_000,
    max_bytes: 128 * 1024 * 1024,
};

/// Parse + validate one manifest document. Fail-closed on any missing or
/// mistyped field and on duplicate object keys.
fn parse_manifest(id_fallback: &str, text: &str) -> Result<SnapshotRecord, TimeMachineError> {
    let corrupt = |detail: &str| TimeMachineError::CorruptManifest {
        id: id_fallback.to_string(),
        detail: detail.to_string(),
    };
    let doc = parse_json_with_limits(text, MANIFEST_LIMITS, true)
        .map_err(|e| corrupt(&format!("parse: {e}")))?;
    if doc.get_str("format") != Some("mengu-timemachine/1") {
        return Err(corrupt("missing or wrong \"format\" tag"));
    }
    let id = doc
        .get_str("id")
        .map(str::to_string)
        .unwrap_or_else(|| id_fallback.to_string());
    let corrupt = |detail: &str| TimeMachineError::CorruptManifest {
        id: id.clone(),
        detail: detail.to_string(),
    };
    let seq = doc
        .get("seq")
        .and_then(JsonValue::as_u64)
        .ok_or_else(|| corrupt("missing \"seq\""))?;
    let trigger = doc
        .get_str("trigger")
        .and_then(SnapshotTrigger::from_slug)
        .ok_or_else(|| corrupt("unknown \"trigger\""))?;
    let created_unix = doc
        .get("created_unix")
        .and_then(JsonValue::as_u64)
        .ok_or_else(|| corrupt("missing \"created_unix\""))?;
    let total_bytes = doc
        .get("total_bytes")
        .and_then(JsonValue::as_u64)
        .ok_or_else(|| corrupt("missing \"total_bytes\""))?;
    let mut files = Vec::new();
    for item in doc
        .get("files")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| corrupt("missing \"files\" array"))?
    {
        let path = item
            .get_str("path")
            .ok_or_else(|| corrupt("file entry missing \"path\""))?
            .to_string();
        let sha256 = item
            .get_str("sha256")
            .ok_or_else(|| corrupt("file entry missing \"sha256\""))?
            .to_string();
        let ok_digest = sha256.len() == 64
            && sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if !ok_digest {
            return Err(corrupt("file entry has malformed \"sha256\""));
        }
        let len = item
            .get("len")
            .and_then(JsonValue::as_u64)
            .ok_or_else(|| corrupt("file entry missing \"len\""))?;
        files.push(SnapshotFileEntry { path, sha256, len });
    }
    let mut skipped = Vec::new();
    if let Some(arr) = doc.get("skipped").and_then(JsonValue::as_array) {
        for item in arr {
            let path = item
                .get_str("path")
                .ok_or_else(|| corrupt("skip entry missing \"path\""))?
                .to_string();
            let reason = item
                .get_str("reason")
                .and_then(SkipReason::from_slug)
                .ok_or_else(|| corrupt("skip entry has unknown \"reason\""))?;
            skipped.push(SkippedPath { path, reason });
        }
    }
    Ok(SnapshotRecord {
        id,
        seq,
        trigger,
        created_unix,
        files,
        skipped,
        total_bytes,
    })
}

/// Write `bytes` to `path` via a same-directory tmp file + rename so a
/// crash can never leave a half-written manifest.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    fs::File::create(&tmp)?.write_all(bytes)?;
    fs::rename(&tmp, path)
}
