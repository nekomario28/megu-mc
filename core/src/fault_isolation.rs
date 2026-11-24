// SPDX-License-Identifier: LGPL-3.0-or-later
//! `megu_core::fault_isolation` — a deterministic, zero-dependency
//! per-subsystem runtime fault-isolation circuit breaker.
//!
//! One runaway mod, lag machine, corrupt chunk region, or misbehaving
//! contraption must never take the whole process down with it. The
//! [`FaultIsolationRouter`] keeps an independent circuit per
//! [`SubsystemScope`] and walks it through four states:
//!
//! * [`CircuitState::Closed`] — healthy: every tick executes normally.
//! * [`CircuitState::Throttled`] — soft degradation: after
//!   `overrun_throttle_strikes` consecutive per-tick budget overruns the
//!   subsystem runs only every `stride` ticks (phase-offset by scope so
//!   throttled scopes do not lockstep), giving a lag machine a bounded
//!   slice of the tick instead of an unbounded one.
//! * [`CircuitState::OpenIsolated`] — hard isolation: the subsystem is
//!   skipped outright for `until_tick` while the rest of the server keeps
//!   running smoothly. Every isolation carries a deterministic
//!   `MEGU-E-XXXXXXXX` blame fingerprint minted through
//!   [`crate::crash_report::CrashContext`], so operator HUDs and crash
//!   reports stack duplicate incidents onto the same ID.
//! * [`CircuitState::HalfOpenProbe`] — recovery: once the cooldown
//!   expires the router permits exactly one guarded probe execution. A
//!   clean probe closes the circuit and resets the backoff; a faulting
//!   probe re-isolates immediately with the cooldown doubled, capped at
//!   `max_cooldown_ticks`.
//!
//! Isolation triggers are fail-closed and deterministic:
//!
//! * `overrun_isolate_strikes` consecutive budget overruns, or
//!   [`HARD_STALL_ISOLATE_LIMIT`] ticks above `hard_overrun_us` since the
//!   last clean tick (a single-tick stall so severe the subsystem is
//!   presumed wedged),
//! * `error_isolate_strikes` consecutive reported runtime errors,
//! * any panic report — a panicking subsystem is isolated *immediately*,
//! * an explicit operator [`manual_quarantine`].
//!
//! Memory is strictly bounded by `max_tracked_scopes`: when the table is
//! full the router evicts the least-recently-touched *healthy* circuit
//! (then, only if no healthy circuit exists, the oldest throttled one).
//! [`CircuitState::OpenIsolated`] and [`CircuitState::HalfOpenProbe`]
//! records are never evicted — an attacker flooding the router with fresh
//! scope names cannot push a live isolation out of the table. In the
//! pathological case where every slot holds a non-evictable isolated
//! circuit, new scopes simply run untracked and fault reports still
//! return the state they would have held, so `record_panic` stays
//! fail-closed even when it cannot store the record.
//!
//! Everything is pure `std`, clock-free (callers supply the monotonic
//! `tick` and measured `elapsed_us`), panic-free on hostile input, and
//! free of interior mutability — the router is `&mut`-driven like every
//! other `core` primitive.
//!
//! [`manual_quarantine`]: FaultIsolationRouter::manual_quarantine

use std::collections::HashMap;

use crate::crash_report::CrashContext;
use crate::perf_kit::fingerprint_bytes;
use crate::version::TargetVersion;

/// Ticks measured above `hard_overrun_us` since the last clean tick that
/// justify immediate isolation. One 40 ms+ stall is a bad tick; two mean
/// the subsystem is wedged, not unlucky.
pub const HARD_STALL_ISOLATE_LIMIT: u32 = 2;

/// Ceiling for the throttle stride so a deeply degraded subsystem still
/// gets a bounded, non-zero slice of the tick.
pub const MAX_THROTTLE_STRIDE: u32 = 16;

/// Stride assigned at the first throttle strike; each further recorded
/// overrun widens the gap by one tick up to [`MAX_THROTTLE_STRIDE`].
const BASE_THROTTLE_STRIDE: u32 = 2;

/// The subsystem a circuit protects. Scopes are intentionally coarse —
/// the granularity an operator would quarantine at — so a single bad mod
/// or chunk does not splash blame onto neighbors.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SubsystemScope {
    /// A mod namespace, e.g. `"create"`, `"mystic_addon"`.
    ModNamespace(String),
    /// A chunk region keyed by dimension and chunk coordinates, e.g. the
    /// chunk holding a corrupt block entity or a lag machine.
    ChunkRegion { dimension: String, cx: i32, cz: i32 },
    /// A physics contraption (Create-style carriage, airship, train) by id.
    Contraption(u64),
    /// A first-party subsystem, e.g. `"redstone"`, `"entity_ai"`,
    /// `"lighting"`.
    BuiltinSubsystem(String),
}

impl SubsystemScope {
    /// Stable human-readable label used as the crash-report culprit and
    /// for throttle phase hashing, e.g. `mod:create`, `chunk:overworld:4,-9`.
    pub fn label(&self) -> String {
        match self {
            Self::ModNamespace(ns) => format!("mod:{ns}"),
            Self::ChunkRegion { dimension, cx, cz } => {
                format!("chunk:{dimension}:{cx},{cz}")
            }
            Self::Contraption(id) => format!("contraption:{id}"),
            Self::BuiltinSubsystem(name) => format!("builtin:{name}"),
        }
    }
}

/// Why a circuit was forced open. Carried inside
/// [`CircuitState::OpenIsolated`] so operators can see not just *that* a
/// subsystem is isolated but *why*.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IsolationReason {
    /// Sustained per-tick budget overruns escalated past the isolate
    /// strike limit or the hard-stall limit.
    SustainedTickBudgetOverrun {
        consecutive_overruns: u32,
        last_elapsed_us: u64,
        budget_us: u64,
    },
    /// Repeated reported runtime errors crossed the error strike limit.
    RepeatedRuntimeErrors {
        error_count: u32,
        last_error: String,
    },
    /// A panic was caught inside the subsystem; isolated immediately.
    PanicCaught { summary: String },
    /// An operator quarantined the scope by hand.
    OperatorManualQuarantine,
}

/// The circuit position of one scope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CircuitState {
    /// Healthy: normal execution.
    Closed,
    /// Soft degradation: executes only on ticks where
    /// `tick % stride == phase(scope, stride)` until `until_tick`, when
    /// the throttle decision is re-evaluated from the recorded counters.
    Throttled { stride: u32, until_tick: u64 },
    /// Hard isolation: skipped outright until `until_tick`, then moved to
    /// [`CircuitState::HalfOpenProbe`]. `fingerprint` is the deterministic
    /// `MEGU-E-XXXXXXXX` blame ID from [`crate::crash_report`].
    OpenIsolated {
        until_tick: u64,
        reason: IsolationReason,
        fingerprint: String,
    },
    /// Recovery probe: one guarded execution was permitted at
    /// `probe_tick`; the circuit stays here until the runner reports the
    /// probe result. If no result arrives within `cooldown_ticks` the
    /// next `should_execute` re-arms a single probe instead of leaving
    /// the scope in limbo.
    HalfOpenProbe { probe_tick: u64 },
}

/// The execution decision [`FaultIsolationRouter::should_execute`]
/// returns for one scope on one tick.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecutionPermit {
    /// Run the subsystem this tick.
    Allow,
    /// Run exactly one guarded probe; report the outcome through
    /// `record_success`/`record_error`/`record_panic`.
    AllowHalfOpenProbe,
    /// Throttled scope — this tick is not one of its stride ticks.
    SkipThrottled,
    /// Isolated scope — skipped; `fingerprint` is the `MEGU-E-XXXXXXXX`
    /// blame ID for operator surfaces.
    SkipIsolated { fingerprint: String },
}

/// Tuning knobs for [`FaultIsolationRouter`]. All thresholds are
/// sanitized on construction (strike orderings are enforced, zero-length
/// cooldowns become non-zero) so a degenerate config fails closed rather
/// than producing a circuit that never isolates or always isolates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FaultIsolationConfig {
    /// Per-subsystem per-tick soft budget in microseconds; executions
    /// taking longer count as overruns (default 8 ms).
    pub tick_budget_us: u64,
    /// Single-tick elapsed time in microseconds that counts as a hard
    /// stall (default 40 ms). [`HARD_STALL_ISOLATE_LIMIT`] hard stalls
    /// since the last clean tick isolate regardless of strike count.
    pub hard_overrun_us: u64,
    /// Consecutive overruns that move `Closed` to `Throttled`
    /// (default 3).
    pub overrun_throttle_strikes: u32,
    /// Consecutive overruns that move any state to `OpenIsolated`
    /// (default 8).
    pub overrun_isolate_strikes: u32,
    /// Consecutive reported runtime errors that move any state to
    /// `OpenIsolated` (default 3).
    pub error_isolate_strikes: u32,
    /// Cooldown before the first recovery probe, in ticks
    /// (default 200 = 10 s at 20 TPS). Also the throttle re-assessment
    /// window.
    pub initial_cooldown_ticks: u64,
    /// Maximum cooldown after repeated probe failures, in ticks
    /// (default 6_000 = 5 min). The cooldown doubles on each failed probe
    /// and is clamped here.
    pub max_cooldown_ticks: u64,
    /// Hard bound on simultaneously tracked scopes (default 4_096).
    /// Eviction only ever removes `Closed` (then `Throttled`) circuits —
    /// live isolations and in-flight probes are never dropped.
    pub max_tracked_scopes: usize,
}

impl Default for FaultIsolationConfig {
    fn default() -> Self {
        Self {
            tick_budget_us: 8_000,
            hard_overrun_us: 40_000,
            overrun_throttle_strikes: 3,
            overrun_isolate_strikes: 8,
            error_isolate_strikes: 3,
            initial_cooldown_ticks: 200,
            max_cooldown_ticks: 6_000,
            max_tracked_scopes: 4_096,
        }
    }
}

impl FaultIsolationConfig {
    /// Clamp a user-supplied config into the fail-closed contract:
    /// non-zero strikes and cooldowns, `hard_overrun_us` no smaller than
    /// the soft budget, `isolate` strikes strictly above `throttle`
    /// strikes so the throttled state is reachable, and room for at least
    /// one tracked scope.
    fn sanitized(mut self) -> Self {
        self.hard_overrun_us = self.hard_overrun_us.max(self.tick_budget_us);
        self.overrun_throttle_strikes = self.overrun_throttle_strikes.max(1);
        self.overrun_isolate_strikes = self
            .overrun_isolate_strikes
            .max(self.overrun_throttle_strikes.saturating_add(1));
        self.error_isolate_strikes = self.error_isolate_strikes.max(1);
        self.initial_cooldown_ticks = self.initial_cooldown_ticks.max(1);
        self.max_cooldown_ticks = self.max_cooldown_ticks.max(self.initial_cooldown_ticks);
        self.max_tracked_scopes = self.max_tracked_scopes.max(1);
        self
    }
}

/// Per-scope bookkeeping. Created lazily — a scope only occupies a slot
/// once it does something worth remembering (an overrun, error, panic,
/// or quarantine), so a fleet of always-healthy scopes costs zero memory.
#[derive(Clone, Debug)]
struct ScopeRecord {
    state: CircuitState,
    /// Overruns since the last clean tick.
    consecutive_overruns: u32,
    /// Hard stalls since the last clean tick.
    consecutive_hard_stalls: u32,
    /// Reported errors since the last clean tick.
    consecutive_errors: u32,
    /// Current isolation cooldown; doubles on each failed probe up to
    /// `max_cooldown_ticks`, reset by a clean probe or manual release.
    cooldown_ticks: u64,
    /// Last tick any call touched this record; the eviction key.
    last_touch_tick: u64,
    /// The most recently minted `MEGU-E-XXXXXXXX` blame fingerprint.
    /// Kept outside `state` so a `HalfOpenProbe` skip can still surface it.
    fingerprint: String,
}

impl ScopeRecord {
    fn new(tick: u64, cooldown_ticks: u64) -> Self {
        Self {
            state: CircuitState::Closed,
            consecutive_overruns: 0,
            consecutive_hard_stalls: 0,
            consecutive_errors: 0,
            cooldown_ticks,
            last_touch_tick: tick,
            fingerprint: String::new(),
        }
    }
}

/// Double the cooldown after a failed probe, clamped into
/// `[initial_cooldown_ticks, max_cooldown_ticks]`.
fn next_backoff(current: u64, config: &FaultIsolationConfig) -> u64 {
    current
        .saturating_mul(2)
        .clamp(config.initial_cooldown_ticks, config.max_cooldown_ticks)
}

/// Mint the deterministic `MEGU-E-XXXXXXXX` blame fingerprint for an
/// isolation and move the record to `OpenIsolated`. The fingerprint is a
/// pure function of scope + reason, so repeat incidents of the same
/// fault stack onto the same ID.
fn isolate(
    record: &mut ScopeRecord,
    scope: &SubsystemScope,
    tick: u64,
    error_kind: &str,
    detail: String,
    reason: IsolationReason,
) {
    let fingerprint = CrashContext::new(
        error_kind,
        scope.label(),
        TargetVersion::LATEST.id(),
        detail,
    )
    .fingerprint();
    record.fingerprint = fingerprint.clone();
    record.state = CircuitState::OpenIsolated {
        until_tick: tick.saturating_add(record.cooldown_ticks.max(1)),
        reason,
        fingerprint,
    };
}

/// The throttle stride for a given consecutive-overrun count: starts at
/// [`BASE_THROTTLE_STRIDE`] on the first throttle strike and widens by
/// one per further recorded overrun, capped at [`MAX_THROTTLE_STRIDE`].
fn throttle_stride(overruns: u32, config: &FaultIsolationConfig) -> u32 {
    overruns
        .saturating_sub(config.overrun_throttle_strikes)
        .saturating_add(BASE_THROTTLE_STRIDE)
        .clamp(1, MAX_THROTTLE_STRIDE)
}

/// Deterministic throttle phase in `[0, stride)`: derived from the scope
/// label so different throttled scopes land on different ticks of the
/// stride cycle instead of stampeding together.
fn throttle_phase(scope: &SubsystemScope, stride: u32) -> u64 {
    if stride == 0 {
        return 0;
    }
    fingerprint_bytes(scope.label().as_bytes()) % u64::from(stride)
}

/// The circuit-breaker router. One instance protects every subsystem
/// scope on a node; callers run the same loop per scope per tick:
///
/// 1. [`should_execute`](Self::should_execute) — run, probe, or skip.
/// 2. If it ran, report the outcome via
///    [`record_success`](Self::record_success) (with the measured
///    `elapsed_us`), [`record_error`](Self::record_error), or
///    [`record_panic`](Self::record_panic).
#[derive(Clone, Debug)]
pub struct FaultIsolationRouter {
    config: FaultIsolationConfig,
    scopes: HashMap<SubsystemScope, ScopeRecord>,
}

impl Default for FaultIsolationRouter {
    fn default() -> Self {
        Self::new(FaultIsolationConfig::default())
    }
}

impl FaultIsolationRouter {
    /// A router with `config` (sanitized into the fail-closed contract).
    pub fn new(config: FaultIsolationConfig) -> Self {
        Self {
            config: config.sanitized(),
            scopes: HashMap::new(),
        }
    }

    /// The sanitized configuration in force.
    pub fn config(&self) -> &FaultIsolationConfig {
        &self.config
    }

    /// Number of scopes currently tracked — always `<=
    /// max_tracked_scopes`.
    pub fn tracked_scope_count(&self) -> usize {
        self.scopes.len()
    }

    /// The current circuit state for `scope`, or `Closed` if the scope
    /// has never done anything worth tracking.
    pub fn circuit_state(&self, scope: &SubsystemScope) -> CircuitState {
        self.scopes
            .get(scope)
            .map(|r| r.state.clone())
            .unwrap_or(CircuitState::Closed)
    }

    /// Decide whether `scope` may execute on `tick`. Never allocates a
    /// record for an unknown scope: healthy scopes are free.
    pub fn should_execute(&mut self, scope: &SubsystemScope, tick: u64) -> ExecutionPermit {
        let Some(rec) = self.scopes.get_mut(scope) else {
            return ExecutionPermit::Allow;
        };
        rec.last_touch_tick = tick;
        match rec.state {
            CircuitState::Closed => ExecutionPermit::Allow,
            CircuitState::Throttled { stride, until_tick } => {
                let mut until_tick = until_tick;
                if tick >= until_tick {
                    // Re-assessment window: sustained pressure renews the
                    // throttle; counters a clean run already zeroed relax
                    // the circuit back to Closed.
                    if rec.consecutive_overruns >= self.config.overrun_throttle_strikes {
                        until_tick = tick.saturating_add(self.config.initial_cooldown_ticks);
                        rec.state = CircuitState::Throttled { stride, until_tick };
                    } else {
                        rec.state = CircuitState::Closed;
                        return ExecutionPermit::Allow;
                    }
                }
                if tick % u64::from(stride) == throttle_phase(scope, stride) {
                    ExecutionPermit::Allow
                } else {
                    ExecutionPermit::SkipThrottled
                }
            }
            CircuitState::OpenIsolated {
                until_tick,
                ref fingerprint,
                ..
            } => {
                if tick < until_tick {
                    ExecutionPermit::SkipIsolated {
                        fingerprint: fingerprint.clone(),
                    }
                } else {
                    rec.state = CircuitState::HalfOpenProbe { probe_tick: tick };
                    ExecutionPermit::AllowHalfOpenProbe
                }
            }
            CircuitState::HalfOpenProbe { probe_tick } => {
                if tick.saturating_sub(probe_tick) >= rec.cooldown_ticks {
                    // The previous probe's result was never reported
                    // (runner died between permit and report): re-arm one
                    // probe rather than strand the scope forever.
                    rec.state = CircuitState::HalfOpenProbe { probe_tick: tick };
                    ExecutionPermit::AllowHalfOpenProbe
                } else {
                    ExecutionPermit::SkipIsolated {
                        fingerprint: rec.fingerprint.clone(),
                    }
                }
            }
        }
    }

    /// Report a completed execution for `scope` on `tick` that took
    /// `elapsed_us`. A clean execution (`elapsed_us <= tick_budget_us`)
    /// resets all strike counters, heals `Throttled` back to `Closed`,
    /// and closes a probed circuit; an overrun advances the
    /// `Closed -> Throttled -> OpenIsolated` escalation. Returns the
    /// resulting circuit state.
    pub fn record_success(
        &mut self,
        scope: &SubsystemScope,
        tick: u64,
        elapsed_us: u64,
    ) -> CircuitState {
        let clean = elapsed_us <= self.config.tick_budget_us;
        if clean && !self.scopes.contains_key(scope) {
            return CircuitState::Closed;
        }
        let Some(rec) = Self::tracked_mut(&mut self.scopes, &self.config, scope, tick) else {
            return CircuitState::Closed;
        };
        match rec.state {
            CircuitState::OpenIsolated { .. } => {
                // Executed despite a skip permit, or a stale report; the
                // isolation stands.
            }
            CircuitState::HalfOpenProbe { .. } => {
                if clean {
                    rec.state = CircuitState::Closed;
                    rec.consecutive_overruns = 0;
                    rec.consecutive_hard_stalls = 0;
                    rec.consecutive_errors = 0;
                    rec.cooldown_ticks = self.config.initial_cooldown_ticks;
                } else {
                    rec.consecutive_overruns = rec.consecutive_overruns.saturating_add(1);
                    if elapsed_us > self.config.hard_overrun_us {
                        rec.consecutive_hard_stalls = rec.consecutive_hard_stalls.saturating_add(1);
                    }
                    rec.cooldown_ticks = next_backoff(rec.cooldown_ticks, &self.config);
                    let reason = IsolationReason::SustainedTickBudgetOverrun {
                        consecutive_overruns: rec.consecutive_overruns,
                        last_elapsed_us: elapsed_us,
                        budget_us: self.config.tick_budget_us,
                    };
                    isolate(
                        rec,
                        scope,
                        tick,
                        "tick_budget_overrun",
                        format!(
                            "probe overrun: consecutive_overruns={} \
                             last_elapsed_us={} budget_us={}",
                            rec.consecutive_overruns, elapsed_us, self.config.tick_budget_us
                        ),
                        reason,
                    );
                }
            }
            CircuitState::Closed | CircuitState::Throttled { .. } => {
                if clean {
                    rec.consecutive_overruns = 0;
                    rec.consecutive_hard_stalls = 0;
                    rec.consecutive_errors = 0;
                    rec.state = CircuitState::Closed;
                } else {
                    rec.consecutive_overruns = rec.consecutive_overruns.saturating_add(1);
                    if elapsed_us > self.config.hard_overrun_us {
                        rec.consecutive_hard_stalls = rec.consecutive_hard_stalls.saturating_add(1);
                    }
                    if rec.consecutive_hard_stalls >= HARD_STALL_ISOLATE_LIMIT
                        || rec.consecutive_overruns >= self.config.overrun_isolate_strikes
                    {
                        isolate(
                            rec,
                            scope,
                            tick,
                            "tick_budget_overrun",
                            format!(
                                "consecutive_overruns={} last_elapsed_us={} \
                                 budget_us={}",
                                rec.consecutive_overruns, elapsed_us, self.config.tick_budget_us
                            ),
                            IsolationReason::SustainedTickBudgetOverrun {
                                consecutive_overruns: rec.consecutive_overruns,
                                last_elapsed_us: elapsed_us,
                                budget_us: self.config.tick_budget_us,
                            },
                        );
                    } else if rec.consecutive_overruns >= self.config.overrun_throttle_strikes {
                        rec.state = CircuitState::Throttled {
                            stride: throttle_stride(rec.consecutive_overruns, &self.config),
                            until_tick: tick.saturating_add(self.config.initial_cooldown_ticks),
                        };
                    }
                }
            }
        }
        rec.state.clone()
    }

    /// Report a caught runtime error (non-fatal: the subsystem survived
    /// but misbehaved). `error_isolate_strikes` consecutive errors since
    /// the last clean tick isolate the scope; an error during a
    /// half-open probe re-isolates immediately with doubled cooldown.
    /// Returns the resulting circuit state.
    pub fn record_error(&mut self, scope: &SubsystemScope, tick: u64, error: &str) -> CircuitState {
        let Some(rec) = Self::tracked_mut(&mut self.scopes, &self.config, scope, tick) else {
            return CircuitState::Closed;
        };
        rec.consecutive_errors = rec.consecutive_errors.saturating_add(1);
        match rec.state {
            CircuitState::OpenIsolated { .. } => {}
            CircuitState::HalfOpenProbe { .. } => {
                rec.cooldown_ticks = next_backoff(rec.cooldown_ticks, &self.config);
                isolate(
                    rec,
                    scope,
                    tick,
                    "repeated_runtime_error",
                    format!(
                        "probe error: error_count={} last_error={}",
                        rec.consecutive_errors, error
                    ),
                    IsolationReason::RepeatedRuntimeErrors {
                        error_count: rec.consecutive_errors,
                        last_error: error.to_string(),
                    },
                );
            }
            CircuitState::Closed | CircuitState::Throttled { .. } => {
                if rec.consecutive_errors >= self.config.error_isolate_strikes {
                    isolate(
                        rec,
                        scope,
                        tick,
                        "repeated_runtime_error",
                        format!(
                            "error_count={} last_error={}",
                            rec.consecutive_errors, error
                        ),
                        IsolationReason::RepeatedRuntimeErrors {
                            error_count: rec.consecutive_errors,
                            last_error: error.to_string(),
                        },
                    );
                }
            }
        }
        rec.state.clone()
    }

    /// Report that a panic was caught inside `scope`'s execution. The
    /// circuit opens *immediately* — a subsystem that panics once is not
    /// given a chance to corrupt shared state a second time — and a
    /// `MEGU-E-XXXXXXXX` blame fingerprint is minted for the operator
    /// report. During a half-open probe the cooldown doubles. Returns
    /// the resulting circuit state; even when the scope table is
    /// saturated by non-evictable isolations the panic reports
    /// `OpenIsolated` (fail closed), the record simply cannot be stored.
    pub fn record_panic(
        &mut self,
        scope: &SubsystemScope,
        tick: u64,
        summary: &str,
    ) -> CircuitState {
        let Some(rec) = Self::tracked_mut(&mut self.scopes, &self.config, scope, tick) else {
            let mut tmp = ScopeRecord::new(tick, self.config.initial_cooldown_ticks);
            isolate(
                &mut tmp,
                scope,
                tick,
                "panic",
                summary.to_string(),
                IsolationReason::PanicCaught {
                    summary: summary.to_string(),
                },
            );
            return tmp.state;
        };
        rec.consecutive_errors = rec.consecutive_errors.saturating_add(1);
        match rec.state {
            CircuitState::OpenIsolated { .. } => {}
            _ => {
                if matches!(rec.state, CircuitState::HalfOpenProbe { .. }) {
                    rec.cooldown_ticks = next_backoff(rec.cooldown_ticks, &self.config);
                }
                isolate(
                    rec,
                    scope,
                    tick,
                    "panic",
                    summary.to_string(),
                    IsolationReason::PanicCaught {
                        summary: summary.to_string(),
                    },
                );
            }
        }
        rec.state.clone()
    }

    /// Operator-forced isolation: the scope is skipped for the current
    /// cooldown window with [`IsolationReason::OperatorManualQuarantine`]
    /// and carries a blame fingerprint like any other isolation.
    pub fn manual_quarantine(&mut self, scope: &SubsystemScope, tick: u64) -> CircuitState {
        let Some(rec) = Self::tracked_mut(&mut self.scopes, &self.config, scope, tick) else {
            let mut tmp = ScopeRecord::new(tick, self.config.initial_cooldown_ticks);
            isolate(
                &mut tmp,
                scope,
                tick,
                "operator_quarantine",
                format!("operator quarantine of {}", scope.label()),
                IsolationReason::OperatorManualQuarantine,
            );
            return tmp.state;
        };
        isolate(
            rec,
            scope,
            tick,
            "operator_quarantine",
            format!("operator quarantine of {}", scope.label()),
            IsolationReason::OperatorManualQuarantine,
        );
        rec.state.clone()
    }

    /// Operator release: drop every strike counter, reset the backoff,
    /// and return the scope to `Closed`. Unknown scopes are a no-op.
    pub fn manual_release(&mut self, scope: &SubsystemScope) {
        if let Some(rec) = self.scopes.get_mut(scope) {
            let touch = rec.last_touch_tick;
            *rec = ScopeRecord::new(touch, self.config.initial_cooldown_ticks);
        }
    }

    /// Every scope currently kept out of normal execution — `OpenIsolated`
    /// plus mid-probe `HalfOpenProbe` circuits — sorted by scope for a
    /// stable operator HUD/console listing.
    pub fn isolated_scopes(&self) -> Vec<(SubsystemScope, CircuitState)> {
        let mut out: Vec<(SubsystemScope, CircuitState)> = self
            .scopes
            .iter()
            .filter(|(_, r)| {
                matches!(
                    r.state,
                    CircuitState::OpenIsolated { .. } | CircuitState::HalfOpenProbe { .. }
                )
            })
            .map(|(s, r)| (s.clone(), r.state.clone()))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Fetch or create the record for `scope`, evicting the oldest
    /// evictable circuit first when the table is full. Returns `None`
    /// only when every slot holds a non-evictable isolated/probing
    /// circuit — the memory bound is honored even then.
    fn tracked_mut<'a>(
        scopes: &'a mut HashMap<SubsystemScope, ScopeRecord>,
        config: &FaultIsolationConfig,
        scope: &SubsystemScope,
        tick: u64,
    ) -> Option<&'a mut ScopeRecord> {
        if !scopes.contains_key(scope) {
            if scopes.len() >= config.max_tracked_scopes {
                Self::evict_one(scopes);
            }
            if scopes.len() >= config.max_tracked_scopes {
                return None;
            }
            scopes.insert(
                scope.clone(),
                ScopeRecord::new(tick, config.initial_cooldown_ticks),
            );
        }
        let rec = scopes.get_mut(scope)?;
        rec.last_touch_tick = tick;
        Some(rec)
    }

    /// Evict the single least-recently-touched `Closed` circuit; if none
    /// exists, the least-recently-touched `Throttled` one. `OpenIsolated`
    /// and `HalfOpenProbe` records are never candidates. Ties break on
    /// scope order so eviction is fully deterministic.
    fn evict_one(scopes: &mut HashMap<SubsystemScope, ScopeRecord>) {
        let mut best: Option<(u8, u64, SubsystemScope)> = None;
        for (scope, rec) in scopes.iter() {
            let tier = match rec.state {
                CircuitState::Closed => 0,
                CircuitState::Throttled { .. } => 1,
                CircuitState::OpenIsolated { .. } | CircuitState::HalfOpenProbe { .. } => continue,
            };
            let candidate = (tier, rec.last_touch_tick, scope.clone());
            if best.as_ref().is_none_or(|b| candidate < *b) {
                best = Some(candidate);
            }
        }
        if let Some((_, _, scope)) = best {
            scopes.remove(&scope);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mod_scope(name: &str) -> SubsystemScope {
        SubsystemScope::ModNamespace(name.to_string())
    }

    #[test]
    fn config_sanitization_enforces_fail_closed_ordering() {
        let cfg = FaultIsolationConfig {
            tick_budget_us: 10_000,
            hard_overrun_us: 1, // below the soft budget
            overrun_throttle_strikes: 0,
            overrun_isolate_strikes: 0,
            error_isolate_strikes: 0,
            initial_cooldown_ticks: 0,
            max_cooldown_ticks: 0,
            max_tracked_scopes: 0,
        }
        .sanitized();
        assert_eq!(cfg.hard_overrun_us, 10_000);
        assert_eq!(cfg.overrun_throttle_strikes, 1);
        assert!(cfg.overrun_isolate_strikes > cfg.overrun_throttle_strikes);
        assert_eq!(cfg.error_isolate_strikes, 1);
        assert_eq!(cfg.initial_cooldown_ticks, 1);
        assert_eq!(cfg.max_cooldown_ticks, 1);
        assert_eq!(cfg.max_tracked_scopes, 1);
    }

    #[test]
    fn saturated_router_still_reports_panic_as_isolated() {
        let mut router = FaultIsolationRouter::new(FaultIsolationConfig {
            max_tracked_scopes: 1,
            ..FaultIsolationConfig::default()
        });
        router.record_panic(&mod_scope("held"), 0, "boom");
        // The single slot is a non-evictable isolation; a fresh panic is
        // reported isolated even though it cannot be stored.
        let state = router.record_panic(&mod_scope("new"), 1, "boom");
        assert!(matches!(state, CircuitState::OpenIsolated { .. }));
        assert_eq!(router.tracked_scope_count(), 1);
        // Untracked scope still runs — the bound never wedges new work.
        assert_eq!(
            router.should_execute(&mod_scope("new"), 2),
            ExecutionPermit::Allow
        );
    }
}
