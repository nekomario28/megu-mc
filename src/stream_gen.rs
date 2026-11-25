//! Bounded asynchronous generation for streamed chunks.
//!
//! Why this exists: the delivery loop (`stream_chunks`) ran each chunk's full
//! generation — base terrain, the populate pass, the per-section fill — on the
//! connection thread under the shared generator lock, and any tick-side read of
//! a not-yet-generated chunk populated its demand box *on the tick*. Joins
//! measured ~6 TPS with a single tick at ~1.9 s. Vanilla splits the same work:
//! `fillFromNoise` on a background executor, FEATURES serial on the task's
//! thread. This queue mirrors that split and keeps the server's invariants:
//!
//! - **Ordered commits.** A request gets a ticket carrying a sequence number.
//!   Workers run the pure half (columns + base section rows, a function of the
//!   immutable `GenData` and seed) off-lock; a single committer thread then runs
//!   the generator-lock half — install, `ensure_populated`, the section arrays —
//!   strictly in request order. `pop_done`, `feat_c`, `shape_marks`, staged
//!   payloads and generated ticks therefore append in exactly the order the
//!   serial send loop would have produced, and `materialize_pending_generation`
//!   still applies them on the tick in that order.
//! - **Dedup under racing requests.** Tickets are keyed by `(dim, cx, cz)`:
//!   overlapping view windows (two players, a window move) share the ticket, so
//!   a chunk is generated once no matter who asked.
//! - **Bounded.** The ticket map is capped; requests beyond the cap are declined
//!   and the caller runs the original inline path, so a flood degrades to the
//!   old behaviour instead of growing memory.
//! - **Failure-safe.** A prepare or commit that fails (or panics — generation
//!   threads run under `catch_unwind` like the scoped prefetch workers) resolves
//!   its ticket as failed; the waiter then generates inline, the same path it
//!   always had.
//! - **Published results.** The committer publishes each packed section and its
//!   surface-band rows, the same stores the send loop fills today — so the
//!   encode's `take_published`, the sky-floor scan's `cell_source`, and
//!   `block_at_cached`/`stored_state` readers answer without the generator lock.

use crate::worldgen::{self, DimId, Gen, PrefetchedTerrain};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

/// A chunk column in a dimension: the ticket key.
pub(crate) type GenKey = (DimId, i32, i32);

/// What one queued request becomes on the generating side. `prepare` runs on a
/// worker and must not hold the generator lock; `commit` runs on the committer
/// in request order and is the only stage that may touch the shared generator.
trait StreamJob: Send + Sync + 'static {
    type Prepared: Send + 'static;
    fn prepare(&self, key: GenKey) -> Result<Self::Prepared, ()>;
    fn commit(&self, key: GenKey, prepared: Self::Prepared);
}

struct TicketInner {
    seq: u64,
    generation: u64,
    key: GenKey,
    outcome: Mutex<Option<bool>>,
    ready: Condvar,
}

/// One streamed chunk's generation, shared by every requester that asked for
/// the same `(dim, cx, cz)` before it finished.
pub(crate) struct Ticket(Arc<TicketInner>);

impl Ticket {
    /// Block until the queued generation resolves. `true` when the committer
    /// generated it; `false` on any failure — the caller then runs the same
    /// inline path it would have without a ticket.
    pub(crate) fn wait(&self) -> bool {
        let mut outcome = self.0.outcome.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(ok) = *outcome {
                return ok;
            }
            outcome = self.0.ready.wait(outcome).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// The resolved outcome without blocking, for tests and counters.
    #[cfg(test)]
    fn peek(&self) -> Option<bool> {
        *self.0.outcome.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Live queue state, all under one mutex. The two condvars split the waiters:
/// workers sleep on `work_cv` for `pending`, the committer sleeps on
/// `commit_cv` for the next-in-sequence entry of `prepared`.
struct QueueState<P> {
    tickets: HashMap<GenKey, Arc<TicketInner>>,
    /// Insertion order for eviction of *resolved* tickets under cap pressure;
    /// unresolved tickets are never evicted (their waiters must not hang).
    ticket_order: VecDeque<(u64, GenKey)>,
    pending: VecDeque<Arc<TicketInner>>,
    prepared: BTreeMap<u64, (Arc<TicketInner>, Result<P, ()>)>,
    next_seq: u64,
    /// The next sequence the committer may apply. Because only it advances, a
    /// ticket resolves exactly when `commit_next` passes its sequence — which is
    /// what makes commit order identical to request order.
    commit_next: u64,
    /// `current_gen_seq()` at request time: a worldgen re-init makes every
    /// outstanding ticket stale — prepared results built from the old generator
    /// snapshot must never land in the new one.
    generation: u64,
}

impl<P> QueueState<P> {
    fn clear(&mut self, generation: u64) {
        // Resolve every outstanding ticket as failed *before* dropping it: a
        // waiter parked on a ticket the reset erased would otherwise hang
        // forever — the failed outcome is what makes it take the inline path.
        for t in self.tickets.values() {
            *t.outcome.lock().unwrap_or_else(|e| e.into_inner()) = Some(false);
            t.ready.notify_all();
        }
        self.tickets.clear();
        self.ticket_order.clear();
        // `pending` and `prepared` are deliberately NOT cleared: every sequence
        // below `commit_next` must still reach the committer — dropping one
        // would stall the ordered lane behind a seq that never arrives. Their
        // generation tag no longer matches, so the committer resolves each as
        // failed without running its commit, which is the cheap drain the
        // sequence needs.
        self.generation = generation;
        // `next_seq`/`commit_next` stay monotone across the reset: a seq issued
        // for the old generator must still sort before every new one.
    }
}

/// The bounded queue itself. Construct once globally; tests build their own
/// with a stub job so the mechanics are checked without a real generator.
struct Queue<J: StreamJob> {
    job: J,
    inner: Mutex<QueueState<J::Prepared>>,
    work_cv: Condvar,
    commit_cv: Condvar,
    spawned: OnceLock<bool>,
    workers: usize,
    cap: usize,
}

impl<J: StreamJob> Queue<J> {
    fn new(job: J, workers: usize, cap: usize) -> Self {
        Queue {
            job,
            inner: Mutex::new(QueueState {
                tickets: HashMap::new(),
                ticket_order: VecDeque::new(),
                pending: VecDeque::new(),
                prepared: BTreeMap::new(),
                next_seq: 0,
                commit_next: 0,
                generation: 0,
            }),
            work_cv: Condvar::new(),
            commit_cv: Condvar::new(),
            spawned: OnceLock::new(),
            workers,
            cap,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, QueueState<J::Prepared>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Enqueue `(dim, cx, cz)` for generation, or return the live ticket a
    /// racing requester already holds. `None` when the queue is full of
    /// unresolved work — the caller falls back to generating inline.
    fn request(self: &Arc<Self>, key: GenKey, generation: u64) -> Option<Ticket> {
        // Spawn before enqueuing: a lane that could not start would strand every
        // parked waiter, so a spawn failure declines the request instead.
        if !self.ensure_spawned() {
            return None;
        }
        let mut st = self.lock();
        if st.generation != generation {
            st.clear(generation);
        }
        if let Some(t) = st.tickets.get(&key) {
            return Some(Ticket(t.clone()));
        }
        if st.tickets.len() >= self.cap {
            // Resolved tickets are dead weight a re-request would regenerate
            // anyway (the worker's `ensure_populated` is a no-op on `pop_done`),
            // so cap pressure evicts them oldest-first. Unresolved tickets stay
            // in `ticket_order`: evicting one would strand a parked waiter.
            let mut kept = VecDeque::with_capacity(st.ticket_order.len());
            while let Some((seq, k)) = st.ticket_order.pop_front() {
                let resolved = st
                    .tickets
                    .get(&k)
                    .filter(|t| t.seq == seq)
                    .is_some_and(|t| {
                        t.outcome
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .is_some()
                    });
                if resolved && st.tickets.len() >= self.cap {
                    st.tickets.remove(&k);
                } else {
                    kept.push_back((seq, k));
                }
            }
            st.ticket_order = kept;
            if st.tickets.len() >= self.cap {
                STREAMGEN_FULL.fetch_add(1, Ordering::Relaxed);
                return None;
            }
        }
        let seq = st.next_seq;
        st.next_seq += 1;
        let inner = Arc::new(TicketInner {
            seq,
            generation,
            key,
            outcome: Mutex::new(None),
            ready: Condvar::new(),
        });
        st.tickets.insert(key, inner.clone());
        st.ticket_order.push_back((seq, key));
        st.pending.push_back(inner.clone());
        drop(st);
        self.work_cv.notify_one();
        STREAMGEN_REQUESTED.fetch_add(1, Ordering::Relaxed);
        Some(Ticket(inner))
    }

    /// Start the lanes once. Returns whether the committer is running: without
    /// it no ticket can ever resolve, so a refusal declines the request rather
    /// than parking the caller forever.
    fn ensure_spawned(self: &Arc<Self>) -> bool {
        *self.spawned.get_or_init(|| {
            let mut ok = true;
            for i in 0..self.workers {
                let q = Arc::clone(self);
                ok &= spawn_named(&format!("streamgen-w{i}"), move || q.worker_loop());
            }
            let q = Arc::clone(self);
            ok &= spawn_named("streamgen-commit", move || q.commit_loop());
            ok
        })
    }

    /// Worker loop: claim the next pending ticket, build its pure half, hand it
    /// to the committer. Workers never touch the generator lock — `prepare` is
    /// scratch-generator work, which is also why the panic guard leaves nothing
    /// shared half-written (mirrors `prefetch_terrain`'s contract).
    fn worker_loop(self: &Arc<Self>) {
        // Same attribution the scoped prefetch workers get: off-owner terrain
        // work is billed to the worker bucket, not the owner's.
        let _m3w = worldgen::M3WorkerScope::enter();
        loop {
            let ticket = {
                let mut st = self.lock();
                loop {
                    if let Some(t) = st.pending.pop_front() {
                        break t;
                    }
                    st = self.work_cv.wait(st).unwrap_or_else(|e| e.into_inner());
                }
            };
            // `m3_flush` rides inside the guard too: the committer waits for
            // *this* seq below, so anything that can panic between the pop and
            // the `prepared` insert must degrade to a failed result rather than
            // a lost sequence number.
            let prepared = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let r = self.job.prepare(ticket.key);
                worldgen::m3_flush();
                r
            }));
            let prepared = prepared.unwrap_or(Err(()));
            {
                let mut st = self.lock();
                st.prepared.insert(ticket.seq, (ticket, prepared));
            }
            self.commit_cv.notify_one();
        }
    }

    /// Committer loop: the only thread that runs `commit`, and only ever the
    /// next sequence in line. A stale-generation or failed prepare still
    /// resolves its ticket (as failed) so waiters fall back instead of hanging.
    fn commit_loop(self: &Arc<Self>) {
        loop {
            let (ticket, prepared) = {
                let mut st = self.lock();
                loop {
                    let next = st.commit_next;
                    if let Some(v) = st.prepared.remove(&next) {
                        break v;
                    }
                    st = self.commit_cv.wait(st).unwrap_or_else(|e| e.into_inner());
                }
            };
            let current = self.lock().generation;
            let ok = ticket.generation == current
                && match prepared {
                    Ok(p) => std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        self.job.commit(ticket.key, p)
                    }))
                    .is_ok(),
                    Err(()) => false,
                };
            {
                let mut st = self.lock();
                st.commit_next += 1;
                *ticket.outcome.lock().unwrap_or_else(|e| e.into_inner()) = Some(ok);
            }
            ticket.ready.notify_all();
            if ok {
                STREAMGEN_COMMITTED.fetch_add(1, Ordering::Relaxed);
                prof_note();
            } else {
                STREAMGEN_FAILED.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

fn spawn_named(name: &str, f: impl FnOnce() + Send + 'static) -> bool {
    std::thread::Builder::new()
        .name(name.to_string())
        .spawn(f)
        .is_ok()
}

// ---------- the worldgen job ----------

/// The streamed-chunk job: terrain on the worker, ordered populate + section
/// publication on the committer.
struct WorldGenJob;

type PreparedChunk = Option<PrefetchedTerrain>;

impl StreamJob for WorldGenJob {
    type Prepared = PreparedChunk;

    /// The pure half, identical inputs to `prefetch_terrain_group_dim`'s worker
    /// arm but for one chunk and without a nested `bg_scope!` fanout: the queue
    /// itself is the parallelism, so each prepare is a single scratch generator.
    fn prepare(&self, key: GenKey) -> Result<PreparedChunk, ()> {
        let (dim, cx, cz) = key;
        let data = worldgen::data_snapshot_dim(dim).ok_or(())?;
        let resolver = worldgen::with_gen_dim(dim, |g| g.beard_resolver()).flatten();
        let (lo, hi) = (dim.min_y(), dim.min_y() + dim.height() - 16);
        let mut g = Gen::fresh_scratch(data);
        if let Some(resolver) = resolver {
            g.set_beard_resolver(resolver);
        }
        let cols = g.cols(cx, cz);
        Ok(g
            .build_terrain_rows(cx, cz, lo, hi, cols)
            .map(|(cols, rows)| PrefetchedTerrain {
                key: (cx, cz),
                cols,
                rows,
            }))
    }

    /// The lock half: install the worker's rows and columns, run the populate
    /// pass for this chunk's demand box (a no-op on `pop_done`), then build and
    /// publish every section. Sections are asked for one at a time through
    /// `terrain_section_dim` — a fresh short acquisition per section, exactly
    /// like the send loop's own calls, so tick-side reads can interleave
    /// instead of queuing behind one long hold.
    fn commit(&self, key: GenKey, prepared: PreparedChunk) {
        let (dim, cx, cz) = key;
        let _ = worldgen::with_gen_dim(dim, |g| {
            if let Some(chunk) = prepared {
                g.install_prefetched(vec![Some(chunk)]);
            }
            g.ensure_populated(cx, cz, crate::feature::populate);
        });
        let mut sy = dim.min_y();
        let top = dim.min_y() + dim.height();
        while sy < top {
            if let Some(rows) =
                worldgen::terrain_section_dim(dim, cx, sy, cz, crate::feature::populate)
            {
                let sec = crate::pack_section(&rows);
                // Band first: it borrows the packed section, publishing moves it
                // into the store's Arc.
                crate::install_band_rows(dim, cx, cz, sy, &sec);
                crate::publish_section(dim, cx, cz, sy, std::sync::Arc::new(sec));
            }
            sy += 16;
        }
    }
}

// ---------- the global queue + counters ----------

/// Outstanding-ticket cap. A window's `next_batch` is ack-paced well below
/// this; the cap exists for pathological request floods, where declining is
/// safer than queuing unbounded `PrefetchedTerrain` (~0.4 MiB per chunk).
const STREAM_GEN_CAP: usize = 512;

fn stream_gen_jobs() -> usize {
    if let Some(v) = std::env::var("MEGU_STREAM_GEN_JOBS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
    {
        return v.max(1);
    }
    // The committer serialises the lock half; workers only feed it, so the
    // useful range is small — a couple keeps a prepared buffer ahead of the
    // committer on a many-core host without crowding the connection threads.
    std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(2))
        .unwrap_or(1)
        .clamp(2, 4)
}

fn stream_gen_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("MEGU_STREAM_GEN")
            .map(|v| v != "0")
            .unwrap_or(true)
    })
}

fn queue() -> &'static Arc<Queue<WorldGenJob>> {
    static Q: OnceLock<Arc<Queue<WorldGenJob>>> = OnceLock::new();
    Q.get_or_init(|| Arc::new(Queue::new(WorldGenJob, stream_gen_jobs(), STREAM_GEN_CAP)))
}

/// Ask the pool to generate `(dim, cx, cz)` ahead of the send loop. `None` —
/// worldgen off, the queue switched off (`MEGU_STREAM_GEN=0`), or the ticket
/// map at its bound — means "generate inline exactly as before".
pub(crate) fn request(dim: DimId, cx: i32, cz: i32) -> Option<Ticket> {
    if !worldgen::enabled() || !stream_gen_enabled() {
        return None;
    }
    queue().request((dim, cx, cz), crate::current_gen_seq())
}

/// Tickets the pool accepted.
pub(crate) static STREAMGEN_REQUESTED: AtomicU64 = AtomicU64::new(0);
/// Ordered commits a worker completed on behalf of a streamed chunk — the
/// generation that used to run inline on the connection thread.
pub(crate) static STREAMGEN_COMMITTED: AtomicU64 = AtomicU64::new(0);
/// Tickets resolved as failed (panic or stale generation); each one made its
/// waiter take the inline path.
pub(crate) static STREAMGEN_FAILED: AtomicU64 = AtomicU64::new(0);
/// Requests declined because the ticket map was full of unresolved work.
pub(crate) static STREAMGEN_FULL: AtomicU64 = AtomicU64::new(0);
/// Chunks a `populate` pass decorated *on the tick thread* — the stall this
/// queue exists to remove. Owned by `worldgen` so the module graph stays acyclic.
pub(crate) use crate::worldgen::TICK_GENERATED_CHUNKS;

/// The profiling line, under `MEGU_STREAMGEN_PROF=1`, throttled to one per 64
/// commits so a join burst logs its own drain rate.
pub(crate) fn prof_note() {
    static ON: OnceLock<bool> = OnceLock::new();
    let on = *ON.get_or_init(|| std::env::var_os("MEGU_STREAMGEN_PROF").is_some());
    if !on {
        return;
    }
    let committed = STREAMGEN_COMMITTED.load(Ordering::Relaxed);
    if committed.is_multiple_of(64) {
        eprintln!(
            "[streamgen] requested={} committed={} failed={} full={} tick_generated={}",
            STREAMGEN_REQUESTED.load(Ordering::Relaxed),
            committed,
            STREAMGEN_FAILED.load(Ordering::Relaxed),
            STREAMGEN_FULL.load(Ordering::Relaxed),
            TICK_GENERATED_CHUNKS.load(Ordering::Relaxed),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// A job that records which thread ran which stage and in which order, so
    /// the queue's guarantees are checked without a real generator.
    struct TestJob {
        prepared_on: Mutex<Vec<(GenKey, std::thread::ThreadId)>>,
        committed: Mutex<Vec<(GenKey, std::thread::ThreadId)>>,
        fail_prepare: HashSet<GenKey>,
        gate: Option<Arc<(Mutex<bool>, Condvar)>>,
    }

    impl TestJob {
        fn new() -> Self {
            TestJob {
                prepared_on: Mutex::new(Vec::new()),
                committed: Mutex::new(Vec::new()),
                fail_prepare: HashSet::new(),
                gate: None,
            }
        }
        fn committed_order(&self) -> Vec<GenKey> {
            self.committed
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .map(|(k, _)| *k)
                .collect()
        }
    }

    impl StreamJob for TestJob {
        type Prepared = GenKey;
        fn prepare(&self, key: GenKey) -> Result<GenKey, ()> {
            if let Some(gate) = &self.gate {
                let mut open = gate.0.lock().unwrap_or_else(|e| e.into_inner());
                while !*open {
                    open = gate.1.wait(open).unwrap_or_else(|e| e.into_inner());
                }
            }
            self.prepared_on
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((key, std::thread::current().id()));
            if self.fail_prepare.contains(&key) {
                return Err(());
            }
            Ok(key)
        }
        fn commit(&self, key: GenKey, _prepared: GenKey) {
            self.committed
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((key, std::thread::current().id()));
        }
    }

    fn key(n: i32) -> GenKey {
        (DimId::Overworld, n, 0)
    }

    #[test]
    fn commits_run_off_the_requester_in_request_order() {
        let job = TestJob::new();
        let q = Arc::new(Queue::new(job, 2, 64));
        let caller = std::thread::current().id();
        let mut tickets = Vec::new();
        for n in 0..8 {
            tickets.push(q.request(key(n), 1).expect("request within cap"));
        }
        for t in &tickets {
            assert!(t.wait());
        }
        assert_eq!(
            q.job.committed_order(),
            (0..8).map(key).collect::<Vec<_>>(),
            "commit order must equal request order even when prepares race"
        );
        for (_, tid) in q.job.committed.lock().unwrap_or_else(|e| e.into_inner()).iter() {
            assert_ne!(*tid, caller, "commits must run on the committer thread");
        }
        for (_, tid) in q.job.prepared_on.lock().unwrap_or_else(|e| e.into_inner()).iter() {
            assert_ne!(*tid, caller, "prepares must run on worker threads");
        }
    }

    #[test]
    fn racing_requests_generate_each_chunk_once() {
        let job = TestJob::new();
        let q = Arc::new(Queue::new(job, 2, 64));
        let mut threads = Vec::new();
        for _ in 0..4 {
            let q = Arc::clone(&q);
            threads.push(std::thread::spawn(move || {
                for n in 0..16 {
                    if let Some(t) = q.request(key(n), 1) {
                        assert!(t.wait());
                    }
                }
            }));
        }
        for t in threads {
            t.join().unwrap();
        }
        let committed = q.job.committed_order();
        let mut sorted = committed.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(committed.len(), sorted.len(), "each key generated exactly once");
        assert_eq!(sorted.len(), 16);
    }

    #[test]
    fn failed_prepare_resolves_ticket_so_waiter_falls_back() {
        let mut job = TestJob::new();
        job.fail_prepare.insert(key(2));
        let q = Arc::new(Queue::new(job, 1, 64));
        let a = q.request(key(1), 1).unwrap();
        let b = q.request(key(2), 1).unwrap();
        let c = q.request(key(3), 1).unwrap();
        assert!(a.wait());
        assert!(!b.wait(), "a failed prepare must resolve the ticket false");
        assert!(c.wait(), "a failed seq must not block later commits");
        assert_eq!(b.peek(), Some(false));
    }

    #[test]
    fn unresolved_cap_declines_new_requests() {
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let mut job = TestJob::new();
        job.gate = Some(gate.clone());
        let q = Arc::new(Queue::new(job, 1, 2));
        let a = q.request(key(1), 1).unwrap();
        let b = q.request(key(2), 1).unwrap();
        // The worker is parked inside the gated prepare, so nothing resolves:
        // the third request must be declined, not queued past the bound.
        assert!(q.request(key(3), 1).is_none());
        *gate.0.lock().unwrap_or_else(|e| e.into_inner()) = true;
        gate.1.notify_all();
        assert!(a.wait());
        assert!(b.wait());
    }

    #[test]
    fn generation_reset_requeues_stale_keys() {
        let job = TestJob::new();
        let q = Arc::new(Queue::new(job, 1, 64));
        assert!(q.request(key(1), 1).unwrap().wait());
        // A worldgen re-init drops every ticket; asking again under the new
        // generation must produce a fresh ticket that resolves on its own.
        let t2 = q.request(key(1), 2).unwrap();
        assert!(t2.wait());
        assert_eq!(q.job.committed_order().len(), 2);
    }

    #[test]
    fn eight_clients_progress_past_the_initial_window_without_batch_starvation() {
        const CLIENTS: usize = 8;
        const CHUNKS_PER_CLIENT: usize = 45;
        const LOOKAHEAD: usize = 4;

        let q = Arc::new(Queue::new(TestJob::new(), 4, 512));
        let start = Arc::new(std::sync::Barrier::new(CLIENTS));
        let mut clients = Vec::new();
        for client in 0..CLIENTS {
            let q = Arc::clone(&q);
            let start = Arc::clone(&start);
            clients.push(std::thread::spawn(move || {
                let mut completed = 0;
                start.wait();
                while completed < CHUNKS_PER_CLIENT {
                    let end = (completed + LOOKAHEAD).min(CHUNKS_PER_CLIENT);
                    let tickets: Vec<_> = (completed..end)
                        .map(|index| {
                            let x = (client * 100 + index) as i32;
                            q.request((DimId::Overworld, x, 0), 1)
                                .expect("test queue has capacity")
                        })
                        .collect();
                    for ticket in tickets {
                        assert!(ticket.wait());
                    }
                    completed = end;
                }
                completed
            }));
        }
        for client in clients {
            assert_eq!(client.join().expect("client thread completes"), CHUNKS_PER_CLIENT);
        }
        let committed = q.job.committed_order();
        for client in 0..CLIENTS {
            for index in 0..CHUNKS_PER_CLIENT {
                assert!(committed.contains(&key((client * 100 + index) as i32)));
            }
        }
    }
}
