// SPDX-License-Identifier: LGPL-3.0-or-later
//! Crash containment & safe internal soft-restart, plus a zero-restart mod
//! reload planner (Lane UX-RESILIENCE-1).
//!
//! Two cooperating halves, both pure `std` and deterministic (every
//! time-aware decision takes an injected `now_ms`; this module never reads
//! a wall clock):
//!
//! * **Crash containment** — [`CrashGuard::run_contained`] and
//!   [`CrashGuard::run_session_contained`] wrap a frame/tick/session
//!   boundary in [`std::panic::catch_unwind`]. When a panic is caught, the
//!   polluted boundary state is dropped cleanly (session objects are moved
//!   *into* the containment boundary, so a caught panic can never leak a
//!   half-mutated session back to the caller), a redacted
//!   [`crate::crash_reporter::CrashReport`] is captured and persisted as
//!   `<crash_dir>/crash-<fingerprint>.md`, and a structured
//!   [`RecoveryAction`] is returned: either an internal soft-restart
//!   (window & UI stay open) or a transition to `Screen::CrashRecovery`.
//!   A [`CrashCircuitBreaker`] refuses restart loops: `>= 3` crashes in
//!   `10_000 ms`, or `>= 2` identical consecutive fingerprints, routes
//!   straight to the crash screen; `reset()` fires once a session runs
//!   healthy past `stable_window_ms` (default `30_000 ms`).
//! * **Zero-restart mod reload planning** — [`ModReloadPlanner::plan`]
//!   diffs the active mod snapshot against the desired one and emits a
//!   deterministic [`ModReloadPlan`]: declarative/data/shader changes
//!   reload in place, registry/Tier-2/3 changes trigger an internal soft
//!   restart, native/driver flags are the only thing allowed to demand an
//!   OS process restart, and any missing dependency stops the plan early
//!   with exact Modrinth lookup queries for a 1-click download UX.
//!   [`ModReloadTransaction`] provides stage → validate → commit-or-
//!   rollback so a failed or panicking apply never leaves the client with
//!   a broken mod set.
//!
//! For richer panic sites in guard-captured reports, install
//! [`install_panic_site_recorder`] once at startup — it records the panic
//! location on a thread-local before chaining to the previously installed
//! hook (e.g. [`crate::crash_reporter::install_panic_hook`]). Without it
//! the guard still captures a complete, redacted report; the `file:line`
//! suffix is simply absent.

use std::any::Any;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::panic::{self, AssertUnwindSafe, PanicHookInfo};
use std::path::{Path, PathBuf};

use crate::crash_reporter::{
    capture_panic_context, write_local_report, CrashReport, PanicSite, ReportEnv, ReportKind,
};

// ---------------------------------------------------------------------------
// Panic-site recorder (optional, installed once at startup)
// ---------------------------------------------------------------------------

thread_local! {
    /// The `file:line:column` of the most recent panic on this thread, as
    /// observed by [`install_panic_site_recorder`]. Consumed (taken) by the
    /// crash guard so a stale site is never attributed to a later panic.
    static LAST_PANIC_SITE: RefCell<Option<PanicSite>> = const { RefCell::new(None) };
}

fn recorded_panic_site() -> Option<PanicSite> {
    LAST_PANIC_SITE.with(|slot| slot.borrow_mut().take())
}

fn clear_recorded_panic_site() {
    LAST_PANIC_SITE.with(|slot| *slot.borrow_mut() = None);
}

/// Chain a panic-site recorder onto the current hook: on every panic it
/// stores `PanicHookInfo::location()` in a thread-local for the crash
/// guard to pick up, then invokes the previously installed hook. Install
/// once at startup — before or after
/// [`crate::crash_reporter::install_panic_hook`]; either order works
/// because the record happens on the panicking thread before
/// `catch_unwind` resumes.
///
/// Without this hook, [`CrashGuard`] reports carry no `PanicSite` (the
/// panic payload reachable through `catch_unwind` does not expose a
/// location). Everything else — capture, redaction, fingerprint, local
/// file — is identical.
pub fn install_panic_site_recorder() {
    let previous = panic::take_hook();
    panic::set_hook(Box::new(move |info: &PanicHookInfo<'_>| {
        LAST_PANIC_SITE.with(|slot| {
            *slot.borrow_mut() = info.location().map(|loc| PanicSite {
                file: loc.file().to_string(),
                line: loc.line(),
                column: loc.column(),
            });
        });
        previous(info);
    }));
}

/// Extract a displayable message from a `catch_unwind` panic payload.
fn panic_payload_message(payload: &dyn Any) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

// ---------------------------------------------------------------------------
// Crash circuit breaker
// ---------------------------------------------------------------------------

/// Default maximum crashes tolerated inside [`DEFAULT_CRASH_WINDOW_MS`].
pub const DEFAULT_MAX_CRASHES_IN_WINDOW: u32 = 3;
/// Default crash counting window (ms).
pub const DEFAULT_CRASH_WINDOW_MS: u64 = 10_000;
/// Default maximum identical consecutive fingerprints before tripping.
pub const DEFAULT_MAX_IDENTICAL_CONSECUTIVE: u32 = 2;
/// Default healthy-run duration (ms) that resets the breaker.
pub const DEFAULT_STABLE_WINDOW_MS: u64 = 30_000;

/// What [`CrashCircuitBreaker::decide`] concluded for one crash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BreakerDecision {
    /// Safe to perform an internal soft restart; `attempt` is the 1-based
    /// restart counter for the current epoch.
    Allow {
        /// 1-based count of crashes absorbed in the current epoch.
        attempt: u32,
    },
    /// The breaker is tripped: refuse further automatic restarts and route
    /// to the crash screen. `reason` names the rule that fired.
    Tripped {
        /// Human-readable explanation of the trip rule.
        reason: String,
    },
}

/// Deterministic crash-loop guard. All time is caller-injected
/// (`now_ms`), so behavior is fully reproducible and the module performs
/// zero wall-clock reads.
///
/// Trip rules (whichever fires first, checked per crash):
///
/// * `>= max_crashes_in_window` crashes inside `crash_window_ms`.
/// * `>= max_identical_consecutive` crashes with the same fingerprint in
///   a row.
///
/// Once tripped, every subsequent [`decide`](Self::decide) returns
/// [`BreakerDecision::Tripped`] until [`reset`](Self::reset). The guard
/// resets itself when [`note_healthy`](Self::note_healthy) observes a
/// boundary running clean for `stable_window_ms`.
#[derive(Clone, Debug)]
pub struct CrashCircuitBreaker {
    max_crashes_in_window: u32,
    crash_window_ms: u64,
    max_identical_consecutive: u32,
    stable_window_ms: u64,
    /// Crash timestamps (ms) inside the current window, oldest first.
    crashes: VecDeque<u64>,
    last_fingerprint: Option<String>,
    consecutive_identical: u32,
    /// Total crashes absorbed in the current epoch.
    attempts: u32,
    tripped: bool,
    /// Start of the current uninterrupted healthy streak, if any.
    healthy_since: Option<u64>,
}

impl Default for CrashCircuitBreaker {
    fn default() -> Self {
        Self::new()
    }
}

impl CrashCircuitBreaker {
    /// A breaker with the default thresholds (3 / 10 s window / 2
    /// identical / 30 s stable).
    pub fn new() -> Self {
        Self::with_thresholds(
            DEFAULT_MAX_CRASHES_IN_WINDOW,
            DEFAULT_CRASH_WINDOW_MS,
            DEFAULT_MAX_IDENTICAL_CONSECUTIVE,
            DEFAULT_STABLE_WINDOW_MS,
        )
    }

    /// A breaker with explicit thresholds (all values caller-tunable for
    /// tests and tuning).
    pub fn with_thresholds(
        max_crashes_in_window: u32,
        crash_window_ms: u64,
        max_identical_consecutive: u32,
        stable_window_ms: u64,
    ) -> Self {
        Self {
            max_crashes_in_window: max_crashes_in_window.max(1),
            crash_window_ms,
            max_identical_consecutive: max_identical_consecutive.max(1),
            stable_window_ms,
            crashes: VecDeque::new(),
            last_fingerprint: None,
            consecutive_identical: 0,
            attempts: 0,
            tripped: false,
            healthy_since: None,
        }
    }

    /// Whether the breaker is currently refusing automatic restarts.
    pub fn is_tripped(&self) -> bool {
        self.tripped
    }

    /// Crashes absorbed in the current epoch (1-based restart counter).
    pub fn attempts(&self) -> u32 {
        self.attempts
    }

    /// Record one crash at `now_ms` with `fingerprint` and decide whether
    /// an automatic restart is still safe.
    pub fn decide(&mut self, now_ms: u64, fingerprint: &str) -> BreakerDecision {
        // A crash interrupts any healthy streak in progress.
        self.healthy_since = None;
        self.crashes.push_back(now_ms);
        while self
            .crashes
            .front()
            .is_some_and(|&t| now_ms.saturating_sub(t) >= self.crash_window_ms)
        {
            self.crashes.pop_front();
        }
        if self.last_fingerprint.as_deref() == Some(fingerprint) {
            self.consecutive_identical = self.consecutive_identical.saturating_add(1);
        } else {
            self.consecutive_identical = 1;
            self.last_fingerprint = Some(fingerprint.to_string());
        }
        self.attempts = self.attempts.saturating_add(1);

        if self.tripped {
            return BreakerDecision::Tripped {
                reason: "circuit breaker already tripped; refusing further automatic restarts"
                    .to_string(),
            };
        }
        if self.consecutive_identical >= self.max_identical_consecutive {
            self.tripped = true;
            return BreakerDecision::Tripped {
                reason: format!(
                    "crash {fingerprint} repeated {} times consecutively",
                    self.consecutive_identical
                ),
            };
        }
        if self.crashes.len() as u32 >= self.max_crashes_in_window {
            self.tripped = true;
            return BreakerDecision::Tripped {
                reason: format!(
                    "{} crashes within {} ms (limit {})",
                    self.crashes.len(),
                    self.crash_window_ms,
                    self.max_crashes_in_window
                ),
            };
        }
        BreakerDecision::Allow {
            attempt: self.attempts,
        }
    }

    /// Observe a boundary that completed without panicking at `now_ms`.
    /// Returns `true` when the call reset the breaker, i.e. the session
    /// has now been healthy for `stable_window_ms` uninterrupted.
    pub fn note_healthy(&mut self, now_ms: u64) -> bool {
        if self.attempts == 0 {
            return false;
        }
        match self.healthy_since {
            None => {
                self.healthy_since = Some(now_ms);
                false
            }
            Some(since) => {
                if now_ms.saturating_sub(since) >= self.stable_window_ms {
                    self.reset();
                    true
                } else {
                    false
                }
            }
        }
    }

    /// Clear all crash history, the trip flag, and the healthy streak —
    /// the breaker behaves as newly constructed.
    pub fn reset(&mut self) {
        self.crashes.clear();
        self.last_fingerprint = None;
        self.consecutive_identical = 0;
        self.attempts = 0;
        self.tripped = false;
        self.healthy_since = None;
    }
}

// ---------------------------------------------------------------------------
// Crash containment
// ---------------------------------------------------------------------------

/// What the caller should do after [`CrashGuard`] caught a panic.
#[derive(Clone, Debug, PartialEq)]
pub enum RecoveryAction {
    /// The panic was absorbed; it is safe to re-initialize a fresh
    /// session/world while keeping the window & UI open. `attempt` is the
    /// 1-based restart counter, `crash_id` the `MEGU-E-XXXXXXXX`
    /// fingerprint of the persisted report, and `blame_hint` the redacted
    /// culprit label (mod id or module) for UI display.
    InternalSoftRestart {
        /// 1-based restart counter for the current epoch.
        attempt: u32,
        /// `MEGU-E-XXXXXXXX` fingerprint of the persisted report.
        crash_id: String,
        /// Redacted culprit label (`mod_or_module` from the report).
        blame_hint: String,
    },
    /// The circuit breaker refused an automatic restart: transition to
    /// `Screen::CrashRecovery` (window & process stay alive).
    /// `preview_markdown` is the exact redacted markdown file body.
    ReturnToCrashScreen {
        /// `MEGU-E-XXXXXXXX` fingerprint of the persisted report.
        crash_id: String,
        /// The exact redacted markdown the crash screen should preview.
        preview_markdown: String,
        /// Why automatic restart was refused (trip rule that fired).
        reason: String,
    },
}

/// Result of running one contained boundary.
#[derive(Debug)]
pub enum ContainmentOutcome<T> {
    /// The boundary completed without panicking.
    Completed(T),
    /// A panic was caught: the polluted boundary state was dropped, a
    /// redacted report was captured and persisted, and `action` says
    /// whether a soft restart or the crash screen is next.
    Recovered {
        /// Restart-vs-crash-screen routing decided by the breaker.
        action: RecoveryAction,
        /// The persisted, redacted crash report.
        report: Box<CrashReport>,
    },
}

/// Crash containment boundary for sessions, frames, and ticks.
///
/// The guard owns a [`ReportEnv`] (blame metadata for captured reports),
/// a `crash_dir` (where `crash-<fingerprint>.md` lands), and a
/// [`CrashCircuitBreaker`]. It never panics itself: report persistence is
/// best-effort and a failing write is swallowed — the structured
/// [`RecoveryAction`] is returned regardless.
pub struct CrashGuard {
    env: ReportEnv,
    crash_dir: PathBuf,
    breaker: CrashCircuitBreaker,
}

impl CrashGuard {
    /// A guard with the default circuit breaker.
    pub fn new(env: ReportEnv, crash_dir: PathBuf) -> Self {
        Self::with_breaker(env, crash_dir, CrashCircuitBreaker::new())
    }

    /// A guard with an explicitly configured breaker.
    pub fn with_breaker(env: ReportEnv, crash_dir: PathBuf, breaker: CrashCircuitBreaker) -> Self {
        Self {
            env,
            crash_dir,
            breaker,
        }
    }

    /// The circuit breaker (inspect trip state / attempts).
    pub fn breaker(&self) -> &CrashCircuitBreaker {
        &self.breaker
    }

    /// Mutable access for tests/tuning (`reset`, threshold changes).
    pub fn breaker_mut(&mut self) -> &mut CrashCircuitBreaker {
        &mut self.breaker
    }

    /// The directory crash reports are written into.
    pub fn crash_dir(&self) -> &Path {
        &self.crash_dir
    }

    /// Run one frame/tick boundary inside `catch_unwind`.
    ///
    /// On success the boundary's return value is handed back in
    /// [`ContainmentOutcome::Completed`] and the breaker records a healthy
    /// tick (resetting once `stable_window_ms` of clean execution
    /// elapsed). On panic the crash is captured, persisted, and routed
    /// per [`RecoveryAction`].
    ///
    /// **Safety invariant**: anything the closure *owns* is dropped during
    /// unwinding before `catch_unwind` returns, so a polluted object can
    /// never reach the caller. To make that airtight for session objects,
    /// prefer [`run_session_contained`](Self::run_session_contained),
    /// which moves the session into the boundary by value.
    pub fn run_contained<T, F>(&mut self, now_ms: u64, boundary: F) -> ContainmentOutcome<T>
    where
        F: FnOnce() -> T,
    {
        clear_recorded_panic_site();
        match panic::catch_unwind(AssertUnwindSafe(boundary)) {
            Ok(value) => {
                self.breaker.note_healthy(now_ms);
                ContainmentOutcome::Completed(value)
            }
            Err(payload) => self.recover(now_ms, &*payload),
        }
    }

    /// Run one session boundary inside `catch_unwind`, taking ownership of
    /// `session` so a caught panic drops the (possibly half-mutated)
    /// object during unwinding — it can never be reused in a polluted
    /// state. On success the session is returned inside
    /// [`ContainmentOutcome::Completed`] for the next boundary.
    pub fn run_session_contained<S, F>(
        &mut self,
        now_ms: u64,
        mut session: S,
        tick: F,
    ) -> ContainmentOutcome<S>
    where
        F: FnOnce(&mut S),
    {
        clear_recorded_panic_site();
        let result = panic::catch_unwind(AssertUnwindSafe(|| {
            tick(&mut session);
            session
        }));
        match result {
            Ok(session) => {
                self.breaker.note_healthy(now_ms);
                ContainmentOutcome::Completed(session)
            }
            Err(payload) => self.recover(now_ms, &*payload),
        }
    }

    /// Shared panic path: capture a redacted report, persist the markdown
    /// crash file, consult the breaker, and produce the outcome.
    fn recover<T>(&mut self, now_ms: u64, payload: &dyn Any) -> ContainmentOutcome<T> {
        let message = panic_payload_message(payload);
        let site = recorded_panic_site();
        let report = capture_panic_context(
            &self.env,
            ReportKind::Crash,
            "panic",
            &message,
            site.as_ref(),
        );
        // Best-effort: a failed write must never turn a contained crash
        // into a real one.
        let _ = write_local_report(&self.crash_dir, &report);
        let action = match self.breaker.decide(now_ms, &report.fingerprint) {
            BreakerDecision::Allow { attempt } => RecoveryAction::InternalSoftRestart {
                attempt,
                crash_id: report.fingerprint.clone(),
                blame_hint: report.context.culprit_mod_or_module.clone(),
            },
            BreakerDecision::Tripped { reason } => RecoveryAction::ReturnToCrashScreen {
                crash_id: report.fingerprint.clone(),
                preview_markdown: report.markdown.clone(),
                reason,
            },
        };
        ContainmentOutcome::Recovered {
            action,
            report: Box::new(report),
        }
    }
}

// ---------------------------------------------------------------------------
// Mod reload planning
// ---------------------------------------------------------------------------

/// How invasive applying a mod change is — the strictest tier present in
/// the diff decides the strategy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ReloadTier {
    /// Pure declarative/data content: datapacks, recipes, tags, spell/
    /// status/ritual documents, resource packs, shaderpacks. Reloads in
    /// place — no disconnect, no screen transition.
    DeclarativeHotReload,
    /// Registry IDs, worldgen, or Tier-2/3 runtime shims: needs a clean
    /// internal runtime teardown + re-init, but the client window and OS
    /// process stay alive.
    SessionSoftRestart,
    /// Incompatible native binary or GPU driver flags: the only tier
    /// allowed to demand a real OS process restart.
    ProcessRestart,
}

/// A dependency the target snapshot requires but does not contain —
/// mirrors `mod-adapter-core`'s `MissingDependency` shape so adapter
/// output maps field-for-field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MissingDep {
    /// Mod that declared the dependency.
    pub requiring_mod_id: String,
    /// The missing mod id — also the Modrinth project slug.
    pub required_mod_id: String,
    /// Declared version range, when the manifest carried one.
    pub version_range: Option<String>,
}

/// One mod in a snapshot: identity + content hash + reload tier.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModEntry {
    /// Primary mod id (`[a-z0-9_-]{1,64}`).
    pub mod_id: String,
    /// Mod version string.
    pub version: String,
    /// Lowercase hex sha256 of the artifact (64 chars).
    pub sha256: String,
    /// The strictest reload this mod can require.
    pub tier: ReloadTier,
    /// Dependencies this mod needs that the target snapshot lacks;
    /// already computed by the snapshot producer.
    pub missing_dependencies: Vec<MissingDep>,
}

impl ModEntry {
    /// Convenience constructor with no missing dependencies.
    pub fn new(
        mod_id: impl Into<String>,
        version: impl Into<String>,
        sha256: impl Into<String>,
        tier: ReloadTier,
    ) -> Self {
        Self {
            mod_id: mod_id.into(),
            version: version.into(),
            sha256: sha256.into(),
            tier,
            missing_dependencies: Vec::new(),
        }
    }
}

/// The strategy [`ModReloadPlan`] selected.
#[derive(Clone, Debug, PartialEq)]
pub enum ReloadStrategy {
    /// Active and target snapshots are identical — nothing to do.
    NoChange,
    /// The target snapshot has unsatisfied dependencies. The planner
    /// stops *before* anything is applied; `modrinth_queries` carries one
    /// ready-to-fetch Modrinth v2 URL per missing mod so the UI/CLI can
    /// offer a "Download Missing from Modrinth" action.
    MissingDependencies {
        /// Every missing dependency in the target snapshot (deduplicated,
        /// sorted by requiring mod then required mod).
        missing: Vec<MissingDep>,
        /// One Modrinth `/project/<slug>/version?...` URL per fetchable
        /// missing mod id (sorted).
        modrinth_queries: Vec<String>,
    },
    /// Every changed mod is [`ReloadTier::DeclarativeHotReload`]: reload
    /// in place without disconnecting or closing the screen.
    HotReloadInPlace {
        /// Mods present in the target but not the active snapshot.
        added: Vec<ModEntry>,
        /// Mods present in both but with a different version or sha256.
        updated: Vec<ModEntry>,
        /// Mods present in the active snapshot but not the target.
        removed: Vec<ModEntry>,
    },
    /// At least one changed mod needs [`ReloadTier::SessionSoftRestart`]:
    /// clean internal runtime teardown + re-init while the client window
    /// stays open — no OS process exit/relaunch.
    InternalSoftRestart {
        /// Mods present in the target but not the active snapshot.
        added: Vec<ModEntry>,
        /// Mods present in both but with a different version or sha256.
        updated: Vec<ModEntry>,
        /// Mods present in the active snapshot but not the target.
        removed: Vec<ModEntry>,
        /// Which changed mods forced the soft restart.
        reason: String,
    },
    /// Reserved for incompatible native binaries / GPU driver flags —
    /// the only case allowed to ask for an OS process restart.
    ProcessRestartRequired {
        /// Which changed mods forced the process restart.
        reason: String,
    },
}

/// The deterministic output of [`ModReloadPlanner::plan`].
#[derive(Clone, Debug, PartialEq)]
pub struct ModReloadPlan {
    /// The selected strategy.
    pub strategy: ReloadStrategy,
}

/// Modrinth API base used by [`ModReloadPlanner`] dependency lookups —
/// same value `mod-adapter-core`'s dep resolver emits.
pub const MODRINTH_API_BASE: &str = "https://api.modrinth.com/v2";

/// `[a-z0-9_-]{1,64}` — the same mod-id charset the ecosystem scanner and
/// Modrinth slugs use.
fn is_valid_mod_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'))
}

/// RFC 3986 unreserved characters pass through; every other byte is
/// percent-encoded uppercase.
fn percent_encode(input: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(input.len());
    for &b in input.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => {
                out.push('%');
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 0x0F) as usize] as char);
            }
        }
    }
    out
}

/// `["a","b"]` JSON array literal with minimal string escaping — the
/// shape Modrinth's query filters expect before percent-encoding.
fn json_string_array(items: &[&str]) -> String {
    let mut out = String::from("[");
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('"');
        for ch in item.chars() {
            match ch {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                c if (c as u32) < 0x20 => {
                    out.push_str(&format!("\\u{:04x}", c as u32));
                }
                c => out.push(c),
            }
        }
        out.push('"');
    }
    out.push(']');
    out
}

/// Diffs the active mod snapshot against the desired target and produces
/// a [`ModReloadPlan`]. Pure and deterministic: no I/O, no clock reads,
/// same inputs → same plan.
#[derive(Clone, Debug)]
pub struct ModReloadPlanner {
    /// Exact Modrinth `game_versions` tag for dependency queries
    /// (e.g. `"1.21.1"`).
    pub target_mc: String,
    /// Modrinth `loaders` filter values (e.g. `["fabric", "neoforge"]`).
    pub loaders: Vec<String>,
}

impl Default for ModReloadPlanner {
    fn default() -> Self {
        Self {
            target_mc: megu_kernel::TargetVersion::LATEST.id().to_string(),
            loaders: vec!["fabric".to_string(), "neoforge".to_string()],
        }
    }
}

/// Index a snapshot by mod id. A duplicate id keeps the first entry —
/// deterministic, and the snapshot producer is responsible for reporting
/// real duplicates upstream.
fn index_snapshot(snapshot: &[ModEntry]) -> BTreeMap<&str, &ModEntry> {
    let mut map = BTreeMap::new();
    for entry in snapshot {
        map.entry(entry.mod_id.as_str()).or_insert(entry);
    }
    map
}

fn sorted_entries<'a>(map: &BTreeMap<&'a str, &'a ModEntry>) -> Vec<ModEntry> {
    map.values().map(|e| (*e).clone()).collect()
}

/// The Modrinth v2 versions query for one missing mod id — e.g.
/// `https://api.modrinth.com/v2/project/sodium/version?game_versions=%5B%221.21.1%22%5D&loaders=%5B%22fabric%22%2C%22neoforge%22%5D`.
/// `None` when the id fails slug validation (fail closed — never emit a
/// malformed or unsafe URL; the dep is still reported in `missing`).
fn modrinth_versions_url(mod_id: &str, target_mc: &str, loaders: &[String]) -> Option<String> {
    if !is_valid_mod_id(mod_id) {
        return None;
    }
    let game_versions = json_string_array(&[target_mc]);
    let loader_names: Vec<&str> = loaders.iter().map(String::as_str).collect();
    let loaders_json = json_string_array(&loader_names);
    Some(format!(
        "{MODRINTH_API_BASE}/project/{}/version?game_versions={}&loaders={}",
        percent_encode(mod_id),
        percent_encode(&game_versions),
        percent_encode(&loaders_json),
    ))
}

/// Name the mods forcing `tier`, sorted, e.g.
/// `"create@2.0, sodium@1.1 require a session soft restart"`.
fn tier_reason(mods: &[&ModEntry], tier: ReloadTier, what: &str) -> String {
    let mut names: Vec<String> = mods
        .iter()
        .filter(|m| m.tier == tier)
        .map(|m| format!("{}@{}", m.mod_id, m.version))
        .collect();
    names.sort();
    format!("{} require {what}", names.join(", "))
}

impl ModReloadPlanner {
    /// Compute the deterministic plan turning `current` into `target`.
    ///
    /// Order of evaluation is fixed:
    /// 1. identical snapshots → [`ReloadStrategy::NoChange`];
    /// 2. any missing dependency in the target →
    ///    [`ReloadStrategy::MissingDependencies`] (nothing is applied);
    /// 3. any changed [`ReloadTier::ProcessRestart`] mod →
    ///    [`ReloadStrategy::ProcessRestartRequired`];
    /// 4. any changed [`ReloadTier::SessionSoftRestart`] mod →
    ///    [`ReloadStrategy::InternalSoftRestart`];
    /// 5. otherwise → [`ReloadStrategy::HotReloadInPlace`].
    pub fn plan(&self, current: &[ModEntry], target: &[ModEntry]) -> ModReloadPlan {
        let cur = index_snapshot(current);
        let tgt = index_snapshot(target);

        let added_map: BTreeMap<&str, &ModEntry> = tgt
            .iter()
            .filter(|(id, _)| !cur.contains_key(*id))
            .map(|(&id, &e)| (id, e))
            .collect();
        let removed_map: BTreeMap<&str, &ModEntry> = cur
            .iter()
            .filter(|(id, _)| !tgt.contains_key(*id))
            .map(|(&id, &e)| (id, e))
            .collect();
        let updated_map: BTreeMap<&str, &ModEntry> = tgt
            .iter()
            .filter(|(id, e)| {
                cur.get(*id)
                    .is_some_and(|c| c.version != e.version || c.sha256 != e.sha256)
            })
            .map(|(&id, &e)| (id, e))
            .collect();

        let added = sorted_entries(&added_map);
        let updated = sorted_entries(&updated_map);
        let removed = sorted_entries(&removed_map);

        if added.is_empty() && updated.is_empty() && removed.is_empty() {
            return ModReloadPlan {
                strategy: ReloadStrategy::NoChange,
            };
        }

        // Unsatisfied dependencies stop the plan before anything applies.
        let mut missing: Vec<MissingDep> = target
            .iter()
            .flat_map(|m| m.missing_dependencies.iter().cloned())
            .collect();
        missing.sort_by(|a, b| {
            (&a.requiring_mod_id, &a.required_mod_id)
                .cmp(&(&b.requiring_mod_id, &b.required_mod_id))
        });
        missing.dedup();
        if !missing.is_empty() {
            let mut query_ids = BTreeSet::new();
            for dep in &missing {
                query_ids.insert(dep.required_mod_id.clone());
            }
            let modrinth_queries = query_ids
                .iter()
                .filter_map(|id| modrinth_versions_url(id, &self.target_mc, &self.loaders))
                .collect();
            return ModReloadPlan {
                strategy: ReloadStrategy::MissingDependencies {
                    missing,
                    modrinth_queries,
                },
            };
        }

        let mut changed: Vec<&ModEntry> = Vec::new();
        changed.extend(added.iter());
        changed.extend(updated.iter());
        changed.extend(removed.iter());

        if changed.iter().any(|m| m.tier == ReloadTier::ProcessRestart) {
            return ModReloadPlan {
                strategy: ReloadStrategy::ProcessRestartRequired {
                    reason: tier_reason(&changed, ReloadTier::ProcessRestart, "a process restart"),
                },
            };
        }
        if changed
            .iter()
            .any(|m| m.tier == ReloadTier::SessionSoftRestart)
        {
            let reason = tier_reason(
                &changed,
                ReloadTier::SessionSoftRestart,
                "a session soft restart",
            );
            return ModReloadPlan {
                strategy: ReloadStrategy::InternalSoftRestart {
                    added,
                    updated,
                    removed,
                    reason,
                },
            };
        }
        ModReloadPlan {
            strategy: ReloadStrategy::HotReloadInPlace {
                added,
                updated,
                removed,
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Transactional reload
// ---------------------------------------------------------------------------

/// The result of committing a [`ModReloadTransaction`].
#[derive(Clone, Debug, PartialEq)]
pub enum ModReloadOutcome {
    /// Validation passed and the apply callback succeeded — `active` is
    /// the new mod set to install.
    Committed {
        /// The staged mod set, now committed as active.
        active: Vec<ModEntry>,
    },
    /// Validation failed, the apply callback returned an error, or it
    /// panicked — `active` is the exact untouched previous set; the
    /// caller simply keeps it.
    RolledBack {
        /// The previous active set, untouched.
        active: Vec<ModEntry>,
        /// Why the transaction was rolled back.
        reason: String,
    },
}

/// A stage → validate → commit-or-rollback transaction over the active
/// mod set. Staging never mutates the caller's set: commit hands back the
/// staged set only after validation and the apply callback succeed; any
/// error or panic inside `apply` rolls back to the exact original set.
/// A failed mod reload therefore can never leave the client broken.
#[derive(Clone, Debug)]
pub struct ModReloadTransaction {
    original: BTreeMap<String, ModEntry>,
    staged: BTreeMap<String, ModEntry>,
}

impl ModReloadTransaction {
    /// Start a transaction over `active`.
    pub fn begin(active: &[ModEntry]) -> Self {
        let original = index_snapshot(active);
        Self {
            original: original
                .iter()
                .map(|(&id, &e)| (id.to_string(), e.clone()))
                .collect(),
            staged: original
                .iter()
                .map(|(&id, &e)| (id.to_string(), e.clone()))
                .collect(),
        }
    }

    /// Stage a full diff (as emitted by [`ReloadStrategy::HotReloadInPlace`]
    /// or [`ReloadStrategy::InternalSoftRestart`]): add `added`, replace
    /// `updated`, drop `removed`. Removed entries absent from the staged
    /// set are ignored deterministically.
    pub fn stage(&mut self, added: &[ModEntry], updated: &[ModEntry], removed: &[ModEntry]) {
        for entry in removed {
            self.staged.remove(entry.mod_id.as_str());
        }
        for entry in added.iter().chain(updated.iter()) {
            self.staged.insert(entry.mod_id.clone(), entry.clone());
        }
    }

    /// The staged set as it would be committed (sorted by mod id).
    pub fn staged(&self) -> Vec<ModEntry> {
        self.staged.values().cloned().collect()
    }

    /// The untouched previous active set (sorted by mod id).
    pub fn original(&self) -> Vec<ModEntry> {
        self.original.values().cloned().collect()
    }

    /// Fail-closed staged-set validation: every entry needs a valid
    /// `[a-z0-9_-]{1,64}` mod id, a non-empty version, a 64-hex sha256,
    /// and zero missing dependencies — a dep-incomplete set is never
    /// committed. `Err` carries every problem found, in mod-id order.
    pub fn validate(&self) -> Result<(), String> {
        let mut problems = Vec::new();
        for entry in self.staged.values() {
            if !is_valid_mod_id(&entry.mod_id) {
                problems.push(format!(
                    "mod id {:?} is not a valid [a-z0-9_-]{{1,64}} id",
                    entry.mod_id
                ));
            }
            if entry.version.trim().is_empty() {
                problems.push(format!("mod {:?} has an empty version", entry.mod_id));
            }
            let hex_ok =
                entry.sha256.len() == 64 && entry.sha256.bytes().all(|b| b.is_ascii_hexdigit());
            if !hex_ok {
                problems.push(format!(
                    "mod {:?} has a malformed sha256 (expected 64 hex chars)",
                    entry.mod_id
                ));
            }
            for dep in &entry.missing_dependencies {
                problems.push(format!(
                    "mod {:?} still requires missing dependency {:?}",
                    entry.mod_id, dep.required_mod_id
                ));
            }
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems.join("; "))
        }
    }

    /// Validate the staged set, then run `apply` (the real install) inside
    /// `catch_unwind`. Returns [`ModReloadOutcome::Committed`] with the
    /// staged set only on full success; any validation failure, error, or
    /// panic inside `apply` yields [`ModReloadOutcome::RolledBack`]
    /// carrying the exact previous active set.
    pub fn commit<F>(self, apply: F) -> ModReloadOutcome
    where
        F: FnOnce(&[ModEntry]) -> Result<(), String>,
    {
        let original = self.original();
        if let Err(reason) = self.validate() {
            return ModReloadOutcome::RolledBack {
                active: original,
                reason: format!("validation failed: {reason}"),
            };
        }
        let staged = self.staged();
        match panic::catch_unwind(AssertUnwindSafe(|| apply(&staged))) {
            Ok(Ok(())) => ModReloadOutcome::Committed { active: staged },
            Ok(Err(reason)) => ModReloadOutcome::RolledBack {
                active: original,
                reason,
            },
            Err(payload) => ModReloadOutcome::RolledBack {
                active: original,
                reason: format!("apply panicked: {}", panic_payload_message(&*payload)),
            },
        }
    }
}
