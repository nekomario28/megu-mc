//! Bounded asynchronous frame-build pool for chunk delivery.
//!
//! Why this exists: `stream_chunks` built every missing `0x27` frame on the
//! connection thread that was about to write it — populate under the generator
//! lock, the per-section encode, the light cull, then (for the first reader)
//! the zlib pass. A moving player entering ungenerated ground put a near-second
//! of that work into one serial grind, and twenty players walking toward the
//! same spawn built the identical chunk twenty times. This pool lifts the
//! build off the streaming thread the same way `stream_gen` lifted generation:
//!
//! - **Dedup under racing requests.** Jobs are keyed (the chunk-frame cache
//!   key: dimension, column, occlusion lane). Two connections asking for the
//!   same key while it is being built share one ticket, so the chunk is built
//!   once no matter how many players walk toward it.
//! - **Bounded.** The in-flight map is capped; a request past the cap is
//!   declined (`None`) and the caller runs the same inline build it always
//!   had, so a flood degrades to the old behaviour instead of growing memory.
//! - **Failure-safe.** A job that returns `None`, or a worker that panics
//!   (jobs run under `catch_unwind`, like the `stream_gen` lanes), resolves
//!   its ticket as failed; the waiter then builds inline.
//! - **No tick-side locks.** Workers own a per-job emitter snapshot and only
//!   ever take the shared worldgen/cache locks the inline path already takes;
//!   they never touch per-client locks, so a stalled socket or a dropped
//!   connection cannot wedge a build, and a finished build simply lands in the
//!   shared `ChunkPacketCache` for whoever still wants it.
//!
//! The module is deliberately generic — it knows nothing about chunks, so the
//! mechanics can be tested with a stub job exactly like `stream_gen`'s queue.
//! `FrameJob`/`FrameOutcome` and the `0x27` build live with the send loop.

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

/// Shared resolution slot for one in-flight build. `Some(Ok)` is a finished
/// job, `Some(Err)` a failed one (build declined or the worker panicked); both
/// wake every waiter — a waiter never hangs on a ticket.
struct TicketInner<R> {
    outcome: Mutex<Option<Result<Arc<R>, ()>>>,
    ready: Condvar,
}

/// One in-flight build, shared by every requester that asked for the same key
/// before it finished.
pub(crate) struct Ticket<R>(Arc<TicketInner<R>>);

impl<R> Clone for Ticket<R> {
    fn clone(&self) -> Self {
        Ticket(self.0.clone())
    }
}

impl<R> Ticket<R> {
    /// Block until the job resolves. `Some` is the built outcome shared by
    /// every waiter on the ticket; `None` is a failed build — the caller runs
    /// the same inline path it would have without a ticket.
    pub(crate) fn wait(&self) -> Option<Arc<R>> {
        let mut outcome = self.0.outcome.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(done) = &*outcome {
                return done.clone().ok();
            }
            outcome = self.0.ready.wait(outcome).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// The resolved outcome without blocking, for tests and counters.
    /// `None` while the job is still running.
    #[cfg(test)]
    fn peek(&self) -> Option<Result<Arc<R>, ()>> {
        self.0
            .outcome
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// Live pool state, all under one mutex.
struct PoolState<K, J, R> {
    /// Key -> ticket for every job still pending or running. Resolved jobs are
    /// removed as they complete — repeat requesters re-check the shared chunk
    /// cache first, so a resolved ticket has no dedup value left.
    in_flight: HashMap<K, Arc<TicketInner<R>>>,
    /// FIFO of accepted jobs waiting for a worker.
    pending: VecDeque<Pending<K, J, R>>,
}

struct Pending<K, J, R> {
    key: K,
    ticket: Arc<TicketInner<R>>,
    job: J,
}

/// Everything a worker thread shares with the pool owner.
struct Shared<K, J, R> {
    state: Mutex<PoolState<K, J, R>>,
    work_cv: Condvar,
    run: fn(J) -> Option<R>,
    stop: AtomicBool,
}

impl<K: Eq + Hash, J, R> Shared<K, J, R> {
    fn lock(&self) -> std::sync::MutexGuard<'_, PoolState<K, J, R>> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Resolve a ticket so waiters fall back instead of hanging.
    fn resolve(ticket: &TicketInner<R>, outcome: Result<Arc<R>, ()>) {
        *ticket.outcome.lock().unwrap_or_else(|e| e.into_inner()) = Some(outcome);
        ticket.ready.notify_all();
    }

    /// Worker loop: claim the next pending job, run it off-lock, publish the
    /// outcome and free the key. Workers hold no lock across `run`, so a slow
    /// build never blocks enqueue or a sibling resolution.
    fn worker_loop(self: &Arc<Self>) {
        loop {
            let pending = {
                let mut st = self.lock();
                loop {
                    if let Some(p) = st.pending.pop_front() {
                        break p;
                    }
                    if self.stop.load(Ordering::Relaxed) {
                        return;
                    }
                    st = self.work_cv.wait(st).unwrap_or_else(|e| e.into_inner());
                }
            };
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                (self.run)(pending.job)
            }))
            .ok()
            .flatten();
            {
                let mut st = self.lock();
                // The in-flight entry is necessarily this ticket: a key maps to
                // exactly one job at a time and only completion removes it.
                st.in_flight.remove(&pending.key);
            }
            ASYNC_FRAMES_COMPLETED.fetch_add(1, Ordering::Relaxed);
            prof_note();
            Self::resolve(
                &pending.ticket,
                outcome.map(Arc::new).ok_or(()),
            );
        }
    }
}

/// The bounded deduplicating build pool itself. Construct once globally; tests
/// build their own with a stub `run` so the mechanics are checked without a
/// real world.
pub(crate) struct Pool<
    K: Eq + Hash + Copy + Send + 'static,
    J: Send + 'static,
    R: Send + Sync + 'static,
> {
    shared: Arc<Shared<K, J, R>>,
    spawned: OnceLock<()>,
    workers: usize,
    cap: usize,
    handles: Mutex<Vec<std::thread::JoinHandle<()>>>,
}

impl<K: Eq + Hash + Copy + Send + 'static, J: Send + 'static, R: Send + Sync + 'static>
    Pool<K, J, R>
{
    pub(crate) fn new(workers: usize, cap: usize, run: fn(J) -> Option<R>) -> Self {
        Pool {
            shared: Arc::new(Shared {
                state: Mutex::new(PoolState {
                    in_flight: HashMap::new(),
                    pending: VecDeque::new(),
                }),
                work_cv: Condvar::new(),
                run,
                stop: AtomicBool::new(false),
            }),
            spawned: OnceLock::new(),
            workers: workers.max(1),
            cap,
            handles: Mutex::new(Vec::new()),
        }
    }

    /// Start the workers once. With zero live workers no job could ever run,
    /// so a spawn failure declines the request rather than stranding a waiter.
    fn ensure_spawned(&self) -> bool {
        let mut spawned_any = false;
        self.spawned.get_or_init(|| {
            let mut handles = self.handles.lock().unwrap_or_else(|e| e.into_inner());
            for i in 0..self.workers {
                let shared = Arc::clone(&self.shared);
                match std::thread::Builder::new()
                    .name(format!("chunk-delivery-w{i}"))
                    .spawn(move || shared.worker_loop())
                {
                    Ok(h) => {
                        spawned_any = true;
                        handles.push(h);
                    }
                    Err(_) => break,
                }
            }
        });
        // OnceLock caches the attempt, not the outcome: if every spawn failed
        // `handles` is empty and requests must degrade to inline builds.
        spawned_any || !self.handles.lock().unwrap_or_else(|e| e.into_inner()).is_empty()
    }

    /// Enqueue `job` for `key`, or return the live ticket a racing requester
    /// already holds. `None` when the in-flight map is full — the caller falls
    /// back to building inline exactly as before.
    pub(crate) fn request(&self, key: K, job: J) -> Option<Ticket<R>> {
        if self.shared.stop.load(Ordering::Relaxed) || !self.ensure_spawned() {
            return None;
        }
        let mut st = self.shared.lock();
        if let Some(t) = st.in_flight.get(&key) {
            ASYNC_FRAMES_DEDUPED.fetch_add(1, Ordering::Relaxed);
            return Some(Ticket(t.clone()));
        }
        if st.in_flight.len() >= self.cap {
            ASYNC_FRAMES_FULL.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let ticket = Arc::new(TicketInner {
            outcome: Mutex::new(None),
            ready: Condvar::new(),
        });
        st.in_flight.insert(key, ticket.clone());
        st.pending.push_back(Pending {
            key,
            ticket: ticket.clone(),
            job,
        });
        drop(st);
        self.shared.work_cv.notify_one();
        ASYNC_FRAMES_REQUESTED.fetch_add(1, Ordering::Relaxed);
        Some(Ticket(ticket))
    }

    /// Stop accepting work, resolve every queued job as failed so waiters take
    /// their inline path, and join the workers. Running jobs finish first —
    /// their outcome still publishes, only the queue behind them is drained.
    pub(crate) fn shutdown(&self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        {
            let mut st = self.shared.lock();
            while let Some(p) = st.pending.pop_front() {
                st.in_flight.remove(&p.key);
                Shared::<K, J, R>::resolve(&p.ticket, Err(()));
            }
        }
        self.shared.work_cv.notify_all();
        let mut handles = self.handles.lock().unwrap_or_else(|e| e.into_inner());
        for h in handles.drain(..) {
            let _ = h.join();
        }
    }

    /// Live in-flight count, for tests.
    #[cfg(test)]
    fn in_flight_len(&self) -> usize {
        self.shared.lock().in_flight.len()
    }
}

impl<K: Eq + Hash + Copy + Send + 'static, J: Send + 'static, R: Send + Sync + 'static> Drop
    for Pool<K, J, R>
{
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Outstanding-job cap. A window's `next_batch` is ack-paced far below this;
/// the cap exists for pathological request floods where declining is safer
/// than queuing unbounded frame builds (~tens of KiB of state per job).
pub(crate) const ASYNC_FRAME_CAP: usize = 512;

/// Whether the asynchronous delivery lane is live. `MEGU_ASYNC_CHUNK_DELIVERY=0`
/// restores the fully synchronous send loop for deterministic tests; async is
/// the live-runtime default. Read per call so a test can flip the switch.
pub(crate) fn enabled() -> bool {
    std::env::var("MEGU_ASYNC_CHUNK_DELIVERY")
        .map(|v| v != "0")
        .unwrap_or(true)
}

/// Worker count for the delivery pool. `MEGU_ASYNC_CHUNK_JOBS` wins, then
/// `MEGU_GEN_JOBS` (the existing delivery-lane worker knob, so a serial
/// comparison stays serial); otherwise half the host's parallelism, clamped to
/// a small range — frame builds are mostly lock-held work, so fanning out past
/// a few workers buys contention, not throughput.
pub(crate) fn worker_count() -> usize {
    for var in ["MEGU_ASYNC_CHUNK_JOBS", "MEGU_GEN_JOBS"] {
        if let Some(v) = std::env::var(var)
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
        {
            return v.max(1);
        }
    }
    std::thread::available_parallelism()
        .map(|n| n.get() / 2)
        .unwrap_or(1)
        .clamp(1, 4)
}

/// Jobs the pool accepted.
pub(crate) static ASYNC_FRAMES_REQUESTED: AtomicU64 = AtomicU64::new(0);
/// Worker builds that ran to completion (success or failure).
pub(crate) static ASYNC_FRAMES_COMPLETED: AtomicU64 = AtomicU64::new(0);
/// Requests that joined an in-flight job instead of building twice — the
/// cross-player dedup this pool exists for.
pub(crate) static ASYNC_FRAMES_DEDUPED: AtomicU64 = AtomicU64::new(0);
/// Requests declined because the in-flight map was full.
pub(crate) static ASYNC_FRAMES_FULL: AtomicU64 = AtomicU64::new(0);

/// The profiling line, under `MEGU_ASYNC_CHUNK_PROF=1`, throttled to one per
/// 64 completed builds so a join burst logs its own drain rate — the proof the
/// async lane carried the build rather than the inline fallback.
pub(crate) fn prof_note() {
    static ON: OnceLock<bool> = OnceLock::new();
    let on = *ON.get_or_init(|| std::env::var_os("MEGU_ASYNC_CHUNK_PROF").is_some());
    if !on {
        return;
    }
    let completed = ASYNC_FRAMES_COMPLETED.load(Ordering::Relaxed);
    if completed.is_multiple_of(64) {
        eprintln!(
            "[async-chunk] requested={} completed={} deduped={} full={}",
            ASYNC_FRAMES_REQUESTED.load(Ordering::Relaxed),
            completed,
            ASYNC_FRAMES_DEDUPED.load(Ordering::Relaxed),
            ASYNC_FRAMES_FULL.load(Ordering::Relaxed),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::{Duration, Instant};

    /// A job carrying a build counter and an optional gate so tests control
    /// when builds finish.
    struct Job {
        key: u64,
        builds: Arc<AtomicUsize>,
        started: Arc<AtomicUsize>,
        gate: Option<Arc<(Mutex<bool>, Condvar)>>,
        fail: bool,
    }

    fn run(job: Job) -> Option<u64> {
        job.started.fetch_add(1, Ordering::Relaxed);
        if let Some(gate) = &job.gate {
            let mut open = gate.0.lock().unwrap_or_else(|e| e.into_inner());
            while !*open {
                open = gate.1.wait(open).unwrap_or_else(|e| e.into_inner());
            }
        }
        job.builds.fetch_add(1, Ordering::Relaxed);
        if job.fail {
            return None;
        }
        Some(job.key * 10)
    }

    fn job(key: u64) -> Job {
        Job {
            key,
            builds: Arc::new(AtomicUsize::new(0)),
            started: Arc::new(AtomicUsize::new(0)),
            gate: None,
            fail: false,
        }
    }

    /// Spin until `n` jobs have started, so a test can rely on a worker being
    /// parked inside a gate instead of racing the claim.
    fn wait_started(started: &AtomicUsize, n: usize) {
        for _ in 0..10_000 {
            if started.load(Ordering::Relaxed) >= n {
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!("timed out waiting for {n} jobs to start");
    }

    #[test]
    fn racing_requests_build_the_key_once_and_share_the_outcome() {
        let builds = Arc::new(AtomicUsize::new(0));
        let pool = Pool::<u64, Job, u64>::new(2, 64, run);
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let mut tickets = Vec::new();
        for _ in 0..4 {
            tickets.push(
                pool.request(
                    7,
                    Job {
                        builds: builds.clone(),
                        gate: Some(gate.clone()),
                        ..job(7)
                    },
                )
                .expect("within cap"),
            );
        }
        // The build is still parked: every waiter must see the same ticket
        // resolving once, not four builds.
        assert_eq!(pool.in_flight_len(), 1);
        *gate.0.lock().unwrap_or_else(|e| e.into_inner()) = true;
        gate.1.notify_all();
        for t in tickets {
            assert_eq!(t.wait().as_deref(), Some(&70));
        }
        assert_eq!(builds.load(Ordering::Relaxed), 1, "dedup must build once");
        pool.shutdown();
    }

    #[test]
    fn request_never_blocks_while_a_build_is_parked() {
        // Two workers: one stays parked inside the gate while the other proves
        // a queued request is claimed and resolved independently. A single
        // worker could never run the second job until the gate opened.
        let pool = Pool::<u64, Job, u64>::new(2, 64, run);
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let parked = pool
            .request(
                1,
                Job {
                    builds: Arc::new(AtomicUsize::new(0)),
                    gate: Some(gate.clone()),
                    ..job(1)
                },
            )
            .unwrap();
        // A second request lands while the worker is parked inside the gate:
        // enqueueing must be immediate and the pending ticket unresolved.
        let t0 = Instant::now();
        let second = pool.request(2, job(2)).expect("within cap");
        assert!(t0.elapsed() < Duration::from_secs(1));
        assert!(parked.peek().is_none());
        assert_eq!(second.wait().as_deref(), Some(&20));
        assert!(parked.peek().is_none(), "parked job still unresolved");
        *gate.0.lock().unwrap_or_else(|e| e.into_inner()) = true;
        gate.1.notify_all();
        assert_eq!(parked.wait().as_deref(), Some(&10));
        pool.shutdown();
    }

    #[test]
    fn dropped_waiter_does_not_leak_the_in_flight_entry() {
        let pool = Pool::<u64, Job, u64>::new(1, 64, run);
        let ticket = pool.request(9, job(9)).unwrap();
        drop(ticket);
        // The "disconnected" waiter is gone; the worker still completes the
        // build and frees the key for the next requester.
        for _ in 0..100 {
            if pool.in_flight_len() == 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(pool.in_flight_len(), 0);
        let again = pool.request(9, job(9)).unwrap();
        assert_eq!(again.wait().as_deref(), Some(&90));
        pool.shutdown();
    }

    #[test]
    fn different_keys_build_independently() {
        // The dimension-switch case: same column under another dimension key
        // is a separate job, never deduped against the old dimension's.
        let builds = Arc::new(AtomicUsize::new(0));
        let pool = Pool::<(u8, u64), Job, u64>::new(2, 64, run);
        let a = pool
            .request(
                (0, 5),
                Job {
                    builds: builds.clone(),
                    ..job(5)
                },
            )
            .unwrap();
        let b = pool
            .request(
                (1, 5),
                Job {
                    builds: builds.clone(),
                    ..job(5)
                },
            )
            .unwrap();
        assert_eq!(a.wait().as_deref(), Some(&50));
        assert_eq!(b.wait().as_deref(), Some(&50));
        assert_eq!(builds.load(Ordering::Relaxed), 2);
        pool.shutdown();
    }

    #[test]
    fn cap_declines_and_failed_jobs_fall_back() {
        let pool = Pool::<u64, Job, u64>::new(1, 2, run);
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let a = pool
            .request(
                1,
                Job {
                    builds: Arc::new(AtomicUsize::new(0)),
                    gate: Some(gate.clone()),
                    ..job(1)
                },
            )
            .unwrap();
        let b = pool
            .request(
                2,
                Job {
                    builds: Arc::new(AtomicUsize::new(0)),
                    fail: true,
                    ..job(2)
                },
            )
            .unwrap();
        // One running, one pending — the bound is reached, so the next request
        // degrades to the caller's inline path instead of queueing past it.
        assert!(pool.request(3, job(3)).is_none());
        *gate.0.lock().unwrap_or_else(|e| e.into_inner()) = true;
        gate.1.notify_all();
        assert_eq!(a.wait().as_deref(), Some(&10));
        assert!(b.wait().is_none(), "a failed job resolves as a fallback");
        pool.shutdown();
    }

    #[test]
    fn shutdown_resolves_queued_waiters_and_joins_workers() {
        let pool = Pool::<u64, Job, u64>::new(1, 64, run);
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let started = Arc::new(AtomicUsize::new(0));
        let running = pool
            .request(
                1,
                Job {
                    builds: Arc::new(AtomicUsize::new(0)),
                    started: started.clone(),
                    gate: Some(gate.clone()),
                    ..job(1)
                },
            )
            .unwrap();
        // Job 2 is only guaranteed still queued once the single worker is
        // parked inside job 1's gate.
        wait_started(&started, 1);
        let queued = pool.request(2, job(2)).unwrap();
        // shutdown() joins the workers, and the worker is parked — so it must
        // run on another thread while this one asserts the queue drain.
        std::thread::scope(|s| {
            s.spawn(|| pool.shutdown());
            assert!(queued.wait().is_none(), "shutdown resolves the queue as failed");
            *gate.0.lock().unwrap_or_else(|e| e.into_inner()) = true;
            gate.1.notify_all();
            assert_eq!(running.wait().as_deref(), Some(&10));
        });
        assert!(pool.request(3, job(3)).is_none(), "a stopped pool declines");
    }
}
