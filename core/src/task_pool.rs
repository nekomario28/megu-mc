// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! `mc_spine_core::task_pool` — a zero-dependency, dual-lane, work-stealing
//! parallel runtime for the voxel engine hot paths.
//!
//! * [`TaskLane`] — two scheduling lanes mirroring heterogeneous hardware:
//!   [`TaskLane::Interactive`] (Lane A, P-core / V-Cache-CCD low-latency work
//!   such as player-facing tick bursts) and [`TaskLane::Background`] (Lane B,
//!   E-core throughput work such as chunk serialization). Lane A work can
//!   never queue behind a saturated Lane B.
//! * [`PoolTopologyConfig::from_hardware_profile`] — derives exact lane
//!   counts from a [`UniversalHardwareProfile`]: `logical_cores <= 2`
//!   yields `(1, 1)` so a Tier-0 two-core host never thrashes, `<= 4`
//!   yields `(2, 1)`, [`CoreHeterogeneity::Hybrid`] maps Lane A to
//!   `p_cores - 1` and Lane B to `e_cores`, and uniform high-core desktops
//!   scale linearly with no arbitrary worker cap.
//! * [`MeguTaskPool`] — cache-line-padded per-worker LIFO deques with FIFO
//!   cross-worker stealing plus a global injection queue, so there is no
//!   single-mutex bottleneck. Workers spin at most 64 `spin_loop`s then
//!   park on a `Condvar` (0% idle CPU) and are always joined on drop.
//! * [`TaskPoolHandle::scope_blocking`] and
//!   [`TaskPoolHandle::par_for_each`] — scoped, stack-borrowing parallel
//!   execution on the resident workers with caller help-stealing: even a
//!   one-thread pool or a `(1, 1)` two-core topology cannot deadlock, and
//!   dispatch avoids `std::thread::scope`-style OS thread spawn/join cost.
//!
//! Everything is pure `std`, clock-free and panic-safe. The only `unsafe`
//! in the module is the scoped-job lifetime erasure in [`Scope::spawn`]
//! and [`TaskPoolHandle::scope_blocking`], justified by the latch contract
//! documented at each site: the pool never lets `scope_blocking` return —
//! normally or via panic — while a scoped task could still touch the
//! borrowed stack frame.

#![allow(unsafe_code)]

use std::any::Any;
use std::cell::Cell;
use std::collections::VecDeque;
use std::mem;
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};

use crate::hw_adaptive_engine::{
    detect_universal_hardware_profile, CoreHeterogeneity, UniversalHardwareProfile,
};

/// Maximum `spin_loop` iterations a worker performs between a failed work
/// scan and parking on the lane `Condvar`.
const MAX_SPIN_ROUNDS: u32 = 64;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Scheduling lane a task is routed to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TaskLane {
    /// Lane A — interactive, latency-critical work. Maps to P-cores /
    /// V-Cache CCDs on hybrid hosts.
    Interactive,
    /// Lane B — background throughput work. Maps to E-cores on hybrid
    /// hosts; saturating it never delays Lane A.
    Background,
}

impl TaskLane {
    const fn index(self) -> usize {
        match self {
            Self::Interactive => 0,
            Self::Background => 1,
        }
    }

    const fn other(self) -> Self {
        match self {
            Self::Interactive => Self::Background,
            Self::Background => Self::Interactive,
        }
    }
}

/// Exact per-lane worker counts for a [`MeguTaskPool`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PoolTopologyConfig {
    /// Lane A (interactive) resident worker threads.
    pub interactive_threads: usize,
    /// Lane B (background) resident worker threads.
    pub background_threads: usize,
}

impl PoolTopologyConfig {
    pub const fn new(interactive_threads: usize, background_threads: usize) -> Self {
        Self {
            interactive_threads,
            background_threads,
        }
    }

    /// Derive lane counts from a probed hardware profile.
    ///
    /// * `logical_cores <= 2` → `(1, 1)` — a 2-core low-spec host gets one
    ///   worker per lane so parallel scheduling never thrashes it.
    /// * `logical_cores <= 4` → `(2, 1)`.
    /// * [`CoreHeterogeneity::Hybrid`] → Lane A gets `p_cores - 1`
    ///   (one P-core stays free for the caller/UI thread) and Lane B gets
    ///   `e_cores`, each clamped to at least one worker.
    /// * Uniform high-core desktops scale linearly: Lane B gets
    ///   `logical_cores / 4`, Lane A the rest — no `MAX_WORKERS` cap.
    pub fn from_hardware_profile(profile: &UniversalHardwareProfile) -> Self {
        let logical = profile.base.logical_cores.max(1) as usize;
        let (interactive, background) = if logical <= 2 {
            (1, 1)
        } else if logical <= 4 {
            (2, 1)
        } else {
            match profile.core_layout {
                CoreHeterogeneity::Hybrid { p_cores, e_cores } => (
                    (p_cores as usize).saturating_sub(1).max(1),
                    (e_cores as usize).max(1),
                ),
                CoreHeterogeneity::Uniform { .. } => {
                    let background = (logical / 4).max(1);
                    (logical - background, background)
                }
            }
        };
        Self::new(interactive, background)
    }
}

/// Work handed to the pool. `Send + 'static` by construction; scoped jobs
/// widen a shorter lifetime only behind the [`ScopeLatch`] completion
/// guarantee (see [`Scope::spawn`]).
type Job = Box<dyn FnOnce() + Send + 'static>;

/// A worker-private task deque on its own cache line so the owner's LIFO
/// pushes/pops never false-share with a neighbouring worker's traffic.
/// Owners pop from the back; peers steal from the front.
#[repr(align(64))]
struct WorkerDeque {
    queue: Mutex<VecDeque<Job>>,
}

impl WorkerDeque {
    fn new() -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
        }
    }
}

/// Sequence counter workers sleep on. Every queue mutation that could
/// produce work (submit, scope-task completion that a waiter needs) bumps
/// the sequence under this mutex before notifying, so a worker that reads
/// the sequence, re-scans, and only then waits cannot miss a wakeup.
struct ParkState {
    sequence: u64,
}

/// All scheduling state for one [`TaskLane`].
struct LanePool {
    /// Per-worker private deques (LIFO owner / FIFO stealer).
    workers: Vec<WorkerDeque>,
    /// Global injection queue: external `spawn` submissions and scoped
    /// spawns that did not originate on a same-lane worker land here.
    injector: Mutex<VecDeque<Job>>,
    park: Mutex<ParkState>,
    wake: Condvar,
}

impl LanePool {
    fn with_workers(count: usize) -> Self {
        Self {
            workers: (0..count).map(|_| WorkerDeque::new()).collect(),
            injector: Mutex::new(VecDeque::new()),
            park: Mutex::new(ParkState { sequence: 0 }),
            wake: Condvar::new(),
        }
    }
}

/// Countdown + first-panic record for one `scope_blocking` call. Lives on
/// the caller's stack frame; the caller provably outlives it because
/// [`PoolCore::wait_latch`] cannot return until `remaining` hits zero and
/// every scoped job performs its latch access *before* decrementing.
struct ScopeLatch {
    remaining: AtomicUsize,
    first_panic: Mutex<Option<Box<dyn Any + Send + 'static>>>,
}

impl ScopeLatch {
    fn new() -> Self {
        Self {
            remaining: AtomicUsize::new(0),
            first_panic: Mutex::new(None),
        }
    }

    fn record_panic(&self, payload: Box<dyn Any + Send + 'static>) {
        let mut slot = lock(&self.first_panic);
        if slot.is_none() {
            *slot = Some(payload);
        }
    }

    /// Called by a scoped job exactly once, after its final latch touch.
    /// Wakes a parked scope owner so the wait exits promptly.
    fn task_done(&self, pool: &PoolCore, lane: TaskLane) {
        if self.remaining.fetch_sub(1, Ordering::AcqRel) == 1 {
            pool.notify(lane);
        }
    }
}

/// Shared state behind [`MeguTaskPool`] / [`TaskPoolHandle`].
struct PoolCore {
    lanes: [LanePool; 2],
    shutdown: AtomicBool,
    tasks_executed: AtomicUsize,
    tasks_stolen: AtomicUsize,
    live_workers: Arc<AtomicUsize>,
}

impl PoolCore {
    /// Identity token distinguishing pools sharing this process's TLS.
    fn id(&self) -> usize {
        self as *const Self as usize
    }

    fn lane(&self, lane: TaskLane) -> &LanePool {
        &self.lanes[lane.index()]
    }

    /// Bump the lane sequence under the park mutex, then wake every parked
    /// worker. The lock ordering is what makes the workers' check-then-wait
    /// free of missed wakeups.
    fn notify(&self, lane: TaskLane) {
        let pool = self.lane(lane);
        {
            let mut state = lock(&pool.park);
            state.sequence = state.sequence.wrapping_add(1);
        }
        pool.wake.notify_all();
    }

    fn notify_all_lanes(&self) {
        for lane in [TaskLane::Interactive, TaskLane::Background] {
            self.notify(lane);
        }
    }

    /// External fire-and-forget submission: lands on the global injector.
    fn submit_external(&self, lane: TaskLane, job: Job) {
        lock(&self.lane(lane).injector).push_back(job);
        self.notify(lane);
    }

    /// Scoped submission: when the caller is one of this pool's workers on
    /// the same lane the job goes to that worker's private deque (locality
    /// for nested fan-out, stealable by peers); otherwise to the injector.
    /// A lane configured with zero workers always falls back to the
    /// injector so a blocked scope owner can still help-execute it.
    fn submit_scoped(&self, lane: TaskLane, job: Job) {
        let pool = self.lane(lane);
        let target =
            WORKER_HOME.with(|home| home.get().filter(|h| h.pool == self.id() && h.lane == lane));
        match target {
            Some(home) if home.index < pool.workers.len() => {
                lock(&pool.workers[home.index].queue).push_back(job);
            }
            _ => lock(&pool.injector).push_back(job),
        }
        self.notify(lane);
    }

    /// Index of the calling thread's deque when it is a worker of this
    /// pool on `lane`.
    fn home_index(&self, lane: TaskLane) -> Option<usize> {
        WORKER_HOME.with(|home| {
            home.get()
                .filter(|h| h.pool == self.id() && h.lane == lane)
                .map(|h| h.index)
        })
    }

    /// Next job for worker `index` of `lane`: own deque tail, then the
    /// injector head, then FIFO steals from peers' deques.
    fn next_job(&self, lane: TaskLane, index: usize) -> Option<(Job, bool)> {
        let pool = self.lane(lane);
        if let Some(job) = lock(&pool.workers[index].queue).pop_back() {
            return Some((job, false));
        }
        if let Some(job) = lock(&pool.injector).pop_front() {
            return Some((job, true));
        }
        let count = pool.workers.len();
        for offset in 1..count {
            let victim = (index + offset) % count;
            if let Some(job) = lock(&pool.workers[victim].queue).pop_front() {
                return Some((job, true));
            }
        }
        None
    }

    /// Execute `job` with panic containment and telemetry. `shared` marks
    /// jobs pulled from the injector or a peer deque rather than the
    /// executing worker's private tail.
    fn execute(&self, job: Job, shared: bool) {
        let _ = catch_unwind(AssertUnwindSafe(job));
        self.tasks_executed.fetch_add(1, Ordering::Relaxed);
        if shared {
            self.tasks_stolen.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Run one queued job on the calling thread, preferring `preferred`'s
    /// lane. Used by scope owners (and workers blocked in a nested scope)
    /// to make progress instead of sleeping: this is what makes a `(1, 1)`
    /// or single-worker topology deadlock-free, since the waiting thread
    /// can execute — including nested jobs of other lanes — rather than
    /// rely on a busy peer.
    fn help_one(&self, preferred: TaskLane) -> bool {
        // Every pop is bound to a local so the queue guard drops *before*
        // `execute` runs: a job may submit nested work that locks these
        // same queues, so executing under a held lock self-deadlocks.
        if let Some(index) = self.home_index(preferred) {
            let job = lock(&self.lane(preferred).workers[index].queue).pop_back();
            if let Some(job) = job {
                self.execute(job, false);
                return true;
            }
        }
        for lane in [preferred, preferred.other()] {
            let pool = self.lane(lane);
            let job = lock(&pool.injector).pop_back();
            if let Some(job) = job {
                self.execute(job, true);
                return true;
            }
            for worker in &pool.workers {
                let job = lock(&worker.queue).pop_front();
                if let Some(job) = job {
                    self.execute(job, true);
                    return true;
                }
            }
        }
        false
    }

    /// Block until `latch.remaining` reaches zero, help-stealing whenever
    /// there is queued work and parking on the lane condvar otherwise.
    ///
    /// This function cannot unwind: job panics are contained by
    /// [`PoolCore::execute`]/the scoped wrapper itself, and every mutex
    /// acquisition is poison-tolerant. That property is load-bearing for
    /// the scoped-lifetime safety argument — a return or unwind happens
    /// only after every scoped job finished touching the latch.
    fn wait_latch(&self, lane: TaskLane, latch: &ScopeLatch) {
        let pool = self.lane(lane);
        while latch.remaining.load(Ordering::Acquire) != 0 {
            if self.help_one(lane) {
                continue;
            }
            let observed = lock(&pool.park).sequence;
            if latch.remaining.load(Ordering::Acquire) == 0 || self.help_one(lane) {
                continue;
            }
            let mut state = lock(&pool.park);
            while state.sequence == observed && latch.remaining.load(Ordering::Acquire) != 0 {
                state = pool
                    .wake
                    .wait(state)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
        }
    }
}

/// Worker-thread registration in TLS so scoped spawns can route to the
/// private deque of the worker they are running on.
#[derive(Clone, Copy)]
struct WorkerHome {
    pool: usize,
    lane: TaskLane,
    index: usize,
}

thread_local! {
    static WORKER_HOME: Cell<Option<WorkerHome>> = const { Cell::new(None) };
}

fn worker_main(core: Arc<PoolCore>, lane: TaskLane, index: usize) {
    core.live_workers.fetch_add(1, Ordering::SeqCst);
    WORKER_HOME.with(|home| {
        home.set(Some(WorkerHome {
            pool: core.id(),
            lane,
            index,
        }));
    });
    let pool = core.lane(lane);
    'outer: loop {
        if let Some((job, shared)) = core.next_job(lane, index) {
            core.execute(job, shared);
            continue;
        }
        // Shutdown is drain-then-exit: a worker only leaves once nothing
        // reachable remains in its lane.
        if core.shutdown.load(Ordering::Acquire) {
            break 'outer;
        }
        for _ in 0..MAX_SPIN_ROUNDS {
            std::hint::spin_loop();
        }
        if let Some((job, shared)) = core.next_job(lane, index) {
            core.execute(job, shared);
            continue;
        }
        if core.shutdown.load(Ordering::Acquire) {
            break 'outer;
        }
        // Park phase: read the sequence, re-scan, then sleep only if the
        // sequence is unchanged — submissions bump it under the same lock
        // before notifying, so a wakeup cannot be lost.
        let observed = lock(&pool.park).sequence;
        if let Some((job, shared)) = core.next_job(lane, index) {
            core.execute(job, shared);
            continue;
        }
        if core.shutdown.load(Ordering::Acquire) {
            break 'outer;
        }
        let mut state = lock(&pool.park);
        while state.sequence == observed && !core.shutdown.load(Ordering::Acquire) {
            state = pool
                .wake
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }
    WORKER_HOME.with(|home| home.set(None));
    core.live_workers.fetch_sub(1, Ordering::SeqCst);
}

/// Scoped-spawn facade handed to the closure passed to
/// [`TaskPoolHandle::scope_blocking`]. Mirrors `std::thread::scope`'s
/// `Scope` API but runs on the pool's resident workers.
pub struct Scope<'scope> {
    pool: &'scope PoolCore,
    lane: TaskLane,
    latch: &'scope ScopeLatch,
}

impl<'scope> Scope<'scope> {
    /// The lane this scope's jobs are routed to.
    pub fn lane(&self) -> TaskLane {
        self.lane
    }

    /// Queue a scoped job that may borrow from the enclosing stack frame.
    ///
    /// # Safety mechanism (the only unsafe in this module)
    ///
    /// `f` is `FnOnce() + Send + 'scope`; the job queue stores
    /// `'static` jobs, so the boxed closure's lifetime is widened by
    /// `mem::transmute`. This is sound because `scope_blocking` guarantees
    /// — for normal returns *and* panic unwinds — that `latch.remaining`
    /// reaches zero before it returns, and `remaining` is decremented only
    /// after the job (and therefore `f`, `latch` and `pool`) made its last
    /// access to the caller's frame. The latch itself lives on the
    /// `scope_blocking` stack, strictly inside `'scope`.
    pub fn spawn<F>(&self, f: F)
    where
        F: FnOnce() + Send + 'scope,
    {
        self.latch.remaining.fetch_add(1, Ordering::AcqRel);
        // SAFETY: see the doc comment above — the latch outlives every use
        // of these widened references by construction of `wait_latch`.
        let latch: &'static ScopeLatch = unsafe { mem::transmute(self.latch) };
        let pool: &'static PoolCore = unsafe { mem::transmute(self.pool) };
        let lane = self.lane;
        let wrapped = move || {
            if let Err(payload) = catch_unwind(AssertUnwindSafe(f)) {
                latch.record_panic(payload);
            }
            latch.task_done(pool, lane);
        };
        let boxed: Box<dyn FnOnce() + Send + 'scope> = Box::new(wrapped);
        // SAFETY: same contract as above — the erased job is only ever
        // executed while `scope_blocking` keeps the borrowed frame alive.
        let job: Job = unsafe { mem::transmute::<Box<dyn FnOnce() + Send + 'scope>, Job>(boxed) };
        pool.submit_scoped(lane, job);
    }
}

/// Cloneable, `Send + 'static` handle to a running pool's shared state.
/// Obtained from [`MeguTaskPool::handle`]; lets `spawn`ed jobs (which are
/// `'static` and cannot borrow the pool) submit nested work, including
/// nested `scope_blocking` calls.
#[derive(Clone)]
pub struct TaskPoolHandle {
    core: Arc<PoolCore>,
}

impl TaskPoolHandle {
    /// Queue a fire-and-forget job on `lane`'s global injection queue.
    pub fn spawn<F>(&self, lane: TaskLane, f: F)
    where
        F: FnOnce() + Send + 'static,
    {
        self.core.submit_external(lane, Box::new(f));
    }

    /// Run `f` with a [`Scope`] that fans borrowed closures out across the
    /// resident workers of `lane`. Blocks — help-stealing, never idling —
    /// until every scoped job completes, then returns `f`'s result. If a
    /// scoped job panicked, its first panic is rethrown here after all
    /// sibling jobs joined, matching `std::thread::scope` semantics.
    ///
    /// This is panic-safe for borrowed lifetimes: even if `f` itself
    /// unwinds, [`PoolCore::wait_latch`] still drains every spawned job
    /// before the panic resumes, so `'scope` borrows can never dangle.
    pub fn scope_blocking<'scope, F, R>(&'scope self, lane: TaskLane, f: F) -> R
    where
        F: for<'s> FnOnce(&'s Scope<'scope>) -> R,
    {
        let latch = ScopeLatch::new();
        // SAFETY: `latch` is declared before `scope` and dropped after
        // `wait_latch` returns; `wait_latch` returns only when
        // `remaining == 0`, i.e. after every job's last latch access. The
        // widened reference therefore never outlives the latch.
        let latch_ref: &'scope ScopeLatch = unsafe { &*(&latch as *const ScopeLatch) };
        let scope = Scope {
            pool: &self.core,
            lane,
            latch: latch_ref,
        };
        let result = catch_unwind(AssertUnwindSafe(|| f(&scope)));
        self.core.wait_latch(lane, &latch);
        match result {
            Ok(value) => {
                if let Some(payload) = lock(&latch.first_panic).take() {
                    resume_unwind(payload);
                }
                value
            }
            Err(payload) => resume_unwind(payload),
        }
    }

    /// Deterministic-index parallel iteration: splits `slice` into at most
    /// `lane_workers * 4` contiguous chunks and applies `f(index, item)`
    /// to every element on `lane`'s resident pool (plus caller
    /// help-stealing), preserving each element's global index. With a pure
    /// `f`, output is bit-identical to serial iteration.
    pub fn par_for_each<'scope, T, F>(&'scope self, lane: TaskLane, slice: &'scope [T], f: F)
    where
        T: Sync + 'scope,
        F: Fn(usize, &T) + Sync + 'scope,
    {
        let len = slice.len();
        if len == 0 {
            return;
        }
        let workers = self.core.lane(lane).workers.len().max(1);
        let chunks = workers.saturating_mul(4).min(len);
        let chunk_len = len.div_ceil(chunks);
        let f = &f;
        self.scope_blocking(lane, |s| {
            for start in (0..len).step_by(chunk_len) {
                let end = (start + chunk_len).min(len);
                s.spawn(move || {
                    for (index, item) in slice.iter().enumerate().take(end).skip(start) {
                        f(index, item);
                    }
                });
            }
        });
    }

    /// Total jobs executed (worker threads and help-stealing callers).
    pub fn tasks_executed(&self) -> usize {
        self.core.tasks_executed.load(Ordering::Relaxed)
    }

    /// Jobs executed after being pulled from the shared injector or a peer
    /// worker's deque — i.e. every job that did not come off the executing
    /// worker's private LIFO tail.
    pub fn tasks_stolen(&self) -> usize {
        self.core.tasks_stolen.load(Ordering::Relaxed)
    }

    /// Lane A worker count.
    pub fn interactive_threads(&self) -> usize {
        self.core.lane(TaskLane::Interactive).workers.len()
    }

    /// Lane B worker count.
    pub fn background_threads(&self) -> usize {
        self.core.lane(TaskLane::Background).workers.len()
    }

    /// Workers currently inside `worker_main` for this pool.
    pub fn live_workers(&self) -> usize {
        self.core.live_workers.load(Ordering::SeqCst)
    }

    /// Shared live-worker counter, exposed so tests can prove a dropped
    /// pool joined every worker.
    #[doc(hidden)]
    pub fn live_worker_counter(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.core.live_workers)
    }
}

/// Resident dual-lane work-stealing thread pool. Dropping it sets the
/// shutdown flag, wakes every parked worker, lets each drain its lane, and
/// joins every handle — zero leaked threads.
pub struct MeguTaskPool {
    handle: TaskPoolHandle,
    handles: Vec<JoinHandle<()>>,
}

impl MeguTaskPool {
    /// Spawn the resident workers described by `cfg`. A lane may be
    /// configured with zero workers (its jobs then run via scope
    /// help-stealing only); a `(0, 0)` config is promoted to `(1, 1)`.
    pub fn new(cfg: PoolTopologyConfig) -> Self {
        let (interactive, background) = if cfg.interactive_threads + cfg.background_threads == 0 {
            (1, 1)
        } else {
            (cfg.interactive_threads, cfg.background_threads)
        };
        let core = Arc::new(PoolCore {
            lanes: [
                LanePool::with_workers(interactive),
                LanePool::with_workers(background),
            ],
            shutdown: AtomicBool::new(false),
            tasks_executed: AtomicUsize::new(0),
            tasks_stolen: AtomicUsize::new(0),
            live_workers: Arc::new(AtomicUsize::new(0)),
        });
        let mut handles = Vec::with_capacity(interactive + background);
        let mut spawn_workers = |lane: TaskLane, count: usize| {
            for index in 0..count {
                let core = Arc::clone(&core);
                let name = match lane {
                    TaskLane::Interactive => format!("megu-tp-i{index}"),
                    TaskLane::Background => format!("megu-tp-b{index}"),
                };
                let handle = thread::Builder::new()
                    .name(name)
                    .spawn(move || worker_main(core, lane, index))
                    .expect("failed to spawn task pool worker");
                handles.push(handle);
            }
        };
        spawn_workers(TaskLane::Interactive, interactive);
        spawn_workers(TaskLane::Background, background);
        Self {
            handle: TaskPoolHandle { core },
            handles,
        }
    }

    /// Build a pool sized by probing this host's hardware profile.
    pub fn for_host() -> Self {
        Self::new(PoolTopologyConfig::from_hardware_profile(
            &detect_universal_hardware_profile(),
        ))
    }

    /// Cheap cloneable handle for submitting nested work from inside jobs.
    pub fn handle(&self) -> TaskPoolHandle {
        self.handle.clone()
    }

    /// Queue a fire-and-forget job on `lane`'s global injection queue.
    pub fn spawn<F>(&self, lane: TaskLane, f: F)
    where
        F: FnOnce() + Send + 'static,
    {
        self.handle.spawn(lane, f);
    }

    /// See [`TaskPoolHandle::scope_blocking`].
    pub fn scope_blocking<'scope, F, R>(&'scope self, lane: TaskLane, f: F) -> R
    where
        F: for<'s> FnOnce(&'s Scope<'scope>) -> R,
    {
        self.handle.scope_blocking(lane, f)
    }

    /// See [`TaskPoolHandle::par_for_each`].
    pub fn par_for_each<'scope, T, F>(&'scope self, lane: TaskLane, slice: &'scope [T], f: F)
    where
        T: Sync + 'scope,
        F: Fn(usize, &T) + Sync + 'scope,
    {
        self.handle.par_for_each(lane, slice, f);
    }

    /// Total jobs executed (worker threads and help-stealing callers).
    pub fn tasks_executed(&self) -> usize {
        self.handle.tasks_executed()
    }

    /// Jobs pulled from the shared injector or a peer deque — see
    /// [`TaskPoolHandle::tasks_stolen`].
    pub fn tasks_stolen(&self) -> usize {
        self.handle.tasks_stolen()
    }

    /// Lane A worker count.
    pub fn interactive_threads(&self) -> usize {
        self.handle.interactive_threads()
    }

    /// Lane B worker count.
    pub fn background_threads(&self) -> usize {
        self.handle.background_threads()
    }

    /// Workers currently running for this pool.
    pub fn live_workers(&self) -> usize {
        self.handle.live_workers()
    }

    /// Shared live-worker counter for leak assertions in tests.
    #[doc(hidden)]
    pub fn live_worker_counter(&self) -> Arc<AtomicUsize> {
        self.handle.live_worker_counter()
    }
}

impl Drop for MeguTaskPool {
    fn drop(&mut self) {
        self.handle.core.shutdown.store(true, Ordering::Release);
        self.handle.core.notify_all_lanes();
        for handle in self.handles.drain(..) {
            let _ = handle.join();
        }
    }
}
