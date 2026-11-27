// SPDX-License-Identifier: LGPL-3.0-or-later

//! Region-local simulation and little-endian, direct-buffer-friendly delta batches.
//! Fluid edits and transport/recipe wakes are scheduled work. Kinetic topology
//! and contraption edits are synchronous.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use crate::contraption::Contraption;
use crate::fluid_network::{FluidError, FluidNetwork, TankState};
use crate::kinetic_graph::KineticGraph;
use crate::recipe_processor::{Drive, Recipe, RecipeError, RecipeProcessor, Stack};
use crate::sleeping_transport::{SleepingTransport, TransportDelta, TransportError};

pub const REGION_SIDE_BLOCKS: i32 = 32;
pub const MAX_BATCH_BYTES: usize = 256 * 1024;
pub const MAX_BATCH_RECORDS: usize = 4096;
pub const BATCH_HEADER_BYTES: usize = 32;
pub const RECORD_HEADER_BYTES: usize = 16;
pub const TRANSPORT_TRANSFER: u16 = 1;
pub const FLUID_TANK_UPDATE: u16 = 2;
pub const RECIPE_EVENT: u16 = 3;
pub const FIRST_ADAPTER_KIND: u16 = 0x100;

const FRAME_MAGIC: [u8; 4] = [b'S', b'5', 1, 1];
const TRANSFER_PAYLOAD_BYTES: usize = 9;
const TANK_PAYLOAD_BYTES: usize = 9;
const RECIPE_PAYLOAD_BYTES: usize = 5;

/// A region address: dimension, horizontal 32-block cell, and vertical band.
/// `y` is a block-y band index, not a block coordinate — vanilla-height
/// dimensions keep `y == 0` (one band spanning the column), while custom
/// "tall" dimensions exceeding the vanilla 384-block span use
/// [`RegionKey::for_block_tall`] so vertically distant work schedules
/// independently.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct RegionKey {
    pub dimension: i32,
    pub x: i32,
    /// Vertical band: block y `div_euclid` `REGION_SIDE_BLOCKS`. `0` for
    /// column regions built by [`RegionKey::for_block`].
    pub y: i32,
    pub z: i32,
}

impl RegionKey {
    /// Columnar region key for vanilla-height dimensions: `y` band `0`.
    pub fn for_block(dimension: i32, x: i32, z: i32) -> Self {
        Self {
            dimension,
            x: x.div_euclid(REGION_SIDE_BLOCKS),
            y: 0,
            z: z.div_euclid(REGION_SIDE_BLOCKS),
        }
    }

    /// Banded region key for tall dimensions (heights past the vanilla
    /// `[-64, 320)` span — e.g. mod dimensions taller than ~1,008 blocks):
    /// the block `y` maps to a 32-block vertical band exactly like `x`/`z`
    /// map to horizontal cells.
    pub fn for_block_tall(dimension: i32, x: i32, y: i32, z: i32) -> Self {
        Self {
            dimension,
            x: x.div_euclid(REGION_SIDE_BLOCKS),
            y: y.div_euclid(REGION_SIDE_BLOCKS),
            z: z.div_euclid(REGION_SIDE_BLOCKS),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegionBudget {
    pub max_steps: usize,
    /// Checked between atomic units of kernel work.
    pub max_time: Option<Duration>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RegionReport {
    pub processed: usize,
    pub backlog: Option<u64>,
    pub output_blocked: bool,
    pub time_exhausted: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BatchError {
    ReservedKind,
    OversizedRecord,
    QueueFull,
    BufferTooSmall { needed: usize },
    SequenceExhausted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BatchReport {
    pub bytes: usize,
    pub records: usize,
    pub sequence: Option<u64>,
}

#[derive(Default)]
pub struct RegionScheduler {
    regions: BTreeMap<RegionKey, Region>,
    pool: Option<WorkerPool>,
    sleeping: BTreeMap<RegionKey, Option<u64>>,
    awake: BTreeSet<RegionKey>,
    deadlines: BTreeSet<(u64, RegionKey)>,
    active_regions_last_tick: usize,
    last_tick: Option<u64>,
}

impl RegionScheduler {
    pub fn region(&self, key: RegionKey) -> Option<&Region> {
        self.regions.get(&key)
    }

    /// Obtain a region's independent kernels, clock, work budget and output queue.
    pub fn region_mut(&mut self, key: RegionKey) -> &mut Region {
        self.wake_region(key);
        self.regions.entry(key).or_insert_with(|| Region::new(key));
        self.awake.insert(key);
        let region = self.regions.get_mut(&key).expect("region was inserted");
        if let Some(tick) = self.last_tick {
            region.advance_idle_clock_to(tick);
        }
        region
    }

    /// Wake a region after an external event. Calling [`region_mut`](Self::region_mut)
    /// does this automatically; this method is useful when an owner queues an
    /// event through a retained region handle.
    pub fn wake_region(&mut self, key: RegionKey) {
        if let Some(due) = self.sleeping.remove(&key).flatten() {
            self.deadlines.remove(&(due, key));
        }
        if self.regions.contains_key(&key) {
            self.awake.insert(key);
        }
    }

    /// Number of regions currently parked until an external mutation or a
    /// scheduled deadline.
    pub fn sleeping_regions(&self) -> usize {
        self.sleeping.len()
    }

    pub fn is_sleeping(&self, key: RegionKey) -> bool {
        self.sleeping.contains_key(&key)
    }

    /// Number of regions actually advanced by the most recent active round.
    pub fn active_regions_last_tick(&self) -> usize {
        self.active_regions_last_tick
    }

    /// Number of awake regions plus sleeping regions whose deadline is due.
    pub fn runnable_region_count(&self, tick: u64) -> usize {
        self.awake.len()
            + self
                .deadlines
                .range(
                    ..=(
                        tick,
                        RegionKey {
                            dimension: i32::MAX,
                            x: i32::MAX,
                            y: i32::MAX,
                            z: i32::MAX,
                        },
                    ),
                )
                .count()
    }

    pub fn runnable_region_count_for(&self, keys: &BTreeSet<RegionKey>, tick: u64) -> usize {
        let awake = self.awake.iter().filter(|key| keys.contains(key)).count();
        let due = self
            .deadlines
            .range(
                ..=(
                    tick,
                    RegionKey {
                        dimension: i32::MAX,
                        x: i32::MAX,
                        y: i32::MAX,
                        z: i32::MAX,
                    },
                ),
            )
            .filter(|(_, key)| keys.contains(key))
            .count();
        awake + due
    }

    /// Refresh a region after the caller has drained its output queue. Idle
    /// regions park immediately; scheduled work parks with its next deadline.
    pub fn refresh_region_sleep(&mut self, key: RegionKey) {
        self.awake.remove(&key);
        if let Some(due) = self.sleeping.remove(&key).flatten() {
            self.deadlines.remove(&(due, key));
        }
        let Some(region) = self.regions.get(&key) else {
            return;
        };
        if region.is_idle() {
            self.sleeping.insert(key, None);
        } else if let Some(due) = region.next_due_tick() {
            self.sleeping.insert(key, Some(due));
            self.deadlines.insert((due, key));
        } else {
            self.awake.insert(key);
        }
    }

    /// Region keys present, in deterministic sorted order.
    pub fn region_keys(&self) -> Vec<RegionKey> {
        self.regions.keys().copied().collect()
    }

    /// Advance every region to `tick` under `budget`, spread across `threads`
    /// workers. Regions are independent by construction, so worker count
    /// changes only wall-clock parallelism — never the result: the returned
    /// `(key, outcome)` pairs come back in sorted key order regardless of how
    /// the work interleaved, making `threads = 1` and `threads = N` runs
    /// bit-identical. One region's error never stops its neighbours.
    ///
    /// Parallel rounds hand whole owned regions to a resident [`WorkerPool`]
    /// instead of spawning scoped threads per tick; workers park on a condvar
    /// while idle, so an unused pool costs no CPU and `Drop` joins it cleanly.
    pub fn advance_all(
        &mut self,
        tick: u64,
        budget: RegionBudget,
        threads: usize,
    ) -> Vec<(RegionKey, Result<RegionReport, TransportError>)> {
        let keys = self.regions.keys().copied().collect();
        let results = self.advance_keys(tick, budget, threads, keys);
        self.active_regions_last_tick = results.len();
        self.record_tick(tick);
        for (key, result) in &results {
            if result.is_ok() {
                self.refresh_region_sleep(*key);
            }
        }
        results
    }

    /// Advance awake regions and sleeping regions with due deadlines. Region
    /// results are returned in `(dimension, rx, rz)` key order independent of
    /// worker scheduling. A region with a future deadline remains parked until
    /// that deadline; `region_mut` wakes it immediately on mutation.
    pub fn advance_active_parallel(
        &mut self,
        tick: u64,
        budget: RegionBudget,
        threads: usize,
    ) -> Vec<(RegionKey, Result<RegionReport, TransportError>)> {
        if self.last_tick.is_some_and(|current| tick < current) {
            let keys: BTreeSet<_> = self.regions.keys().copied().collect();
            return self.advance_active_parallel_for(&keys, tick, budget, threads);
        }
        let mut keys = self.awake.clone();
        keys.extend(
            self.deadlines
                .range(
                    ..=(
                        tick,
                        RegionKey {
                            dimension: i32::MAX,
                            x: i32::MAX,
                            y: i32::MAX,
                            z: i32::MAX,
                        },
                    ),
                )
                .map(|(_, key)| *key),
        );
        self.advance_active_parallel_for(&keys, tick, budget, threads)
    }

    /// The keyed form of [`advance_active_parallel`](Self::advance_active_parallel),
    /// for owners that expose only a subset of scheduler regions as active.
    pub fn advance_active_parallel_for(
        &mut self,
        keys: &BTreeSet<RegionKey>,
        tick: u64,
        budget: RegionBudget,
        threads: usize,
    ) -> Vec<(RegionKey, Result<RegionReport, TransportError>)> {
        if let Some(current) = self.last_tick.filter(|&current| tick < current) {
            self.active_regions_last_tick = 0;
            return keys
                .iter()
                .filter(|key| self.regions.contains_key(key))
                .map(|&key| {
                    (
                        key,
                        Err(TransportError::TimeWentBackwards {
                            current,
                            requested: tick,
                        }),
                    )
                })
                .collect();
        }
        let mut selected: BTreeSet<_> = self
            .awake
            .iter()
            .filter(|key| keys.contains(key))
            .copied()
            .collect();
        selected.extend(
            self.deadlines
                .range(
                    ..=(
                        tick,
                        RegionKey {
                            dimension: i32::MAX,
                            x: i32::MAX,
                            y: i32::MAX,
                            z: i32::MAX,
                        },
                    ),
                )
                .map(|(_, key)| *key)
                .filter(|key| keys.contains(key)),
        );
        let keys: Vec<_> = selected.into_iter().collect();
        let results = self.advance_keys(tick, budget, threads, keys);
        self.active_regions_last_tick = results.len();
        self.record_tick(tick);
        for (key, result) in &results {
            if result.is_ok() {
                self.refresh_region_sleep(*key);
            }
        }
        results
    }

    fn record_tick(&mut self, tick: u64) {
        if self.last_tick.is_none_or(|previous| tick >= previous) {
            self.last_tick = Some(tick);
        }
    }

    fn advance_keys(
        &mut self,
        tick: u64,
        budget: RegionBudget,
        threads: usize,
        keys: Vec<RegionKey>,
    ) -> Vec<(RegionKey, Result<RegionReport, TransportError>)> {
        if keys.is_empty() {
            return Vec::new();
        }
        let workers = threads.max(1).min(self.regions.len().max(1));
        let workers = workers.min(keys.len());
        if workers == 1 {
            let mut results = Vec::with_capacity(keys.len());
            for key in keys {
                let result = self
                    .regions
                    .get_mut(&key)
                    .expect("scheduled region exists")
                    .advance_to(tick, budget);
                results.push((key, result));
            }
            return results;
        }
        let pool = self.pool.get_or_insert_with(WorkerPool::new);
        pool.ensure_workers(workers);
        let job_count = keys.len();
        let (reply_tx, reply_rx) = mpsc::channel();
        {
            let mut jobs = mutex_lock(&pool.shared.jobs);
            for key in keys {
                let region = self.regions.remove(&key).expect("scheduled region exists");
                jobs.push_back(RegionJob {
                    key,
                    region,
                    tick,
                    budget,
                    reply: reply_tx.clone(),
                });
            }
        }
        pool.shared.job_ready.notify_all();
        drop(reply_tx);

        let mut results = BTreeMap::new();
        let mut restored = BTreeMap::new();
        let mut first_panic = None;
        let mut received = 0_usize;
        while received < job_count {
            let Ok(outcome) = reply_rx.recv() else {
                break;
            };
            received += 1;
            match outcome {
                RegionOutcome::Done {
                    key,
                    result,
                    region,
                } => {
                    results.insert(key, result);
                    restored.insert(key, region);
                }
                RegionOutcome::Panicked {
                    key,
                    region,
                    payload,
                } => {
                    restored.insert(key, region);
                    if first_panic.is_none() {
                        first_panic = Some(payload);
                    }
                }
            }
        }
        if received < job_count {
            // Workers only exit on pool shutdown, so this cannot happen while
            // `advance_all` holds `&mut self`; still return every queued
            // region to the map before failing.
            let mut jobs = mutex_lock(&pool.shared.jobs);
            for job in jobs.drain(..) {
                restored.insert(job.key, job.region);
            }
            self.regions.extend(restored);
            panic!(
                "mod-offload worker pool lost {} region(s)",
                job_count - received
            );
        }
        self.regions.extend(restored);
        if let Some(payload) = first_panic {
            panic::resume_unwind(payload);
        }
        results.into_iter().collect()
    }
}

/// One region's slice of an `advance_all` round. The region travels as an
/// owned value so no borrowed `&mut` ever crosses a thread boundary; the
/// worker reports back over `reply`.
struct RegionJob {
    key: RegionKey,
    region: Region,
    tick: u64,
    budget: RegionBudget,
    reply: mpsc::Sender<RegionOutcome>,
}

enum RegionOutcome {
    Done {
        key: RegionKey,
        result: Result<RegionReport, TransportError>,
        region: Region,
    },
    Panicked {
        key: RegionKey,
        region: Region,
        payload: Box<dyn std::any::Any + Send>,
    },
}

struct PoolShared {
    jobs: Mutex<VecDeque<RegionJob>>,
    job_ready: Condvar,
    shutdown: AtomicBool,
}

/// Resident worker pool shared by every parallel `advance_all` call. Workers
/// park on `job_ready` between rounds, so an idle pool costs no CPU; `Drop`
/// flips `shutdown`, wakes every worker and joins them.
struct WorkerPool {
    shared: Arc<PoolShared>,
    handles: Vec<thread::JoinHandle<()>>,
}

impl WorkerPool {
    fn new() -> Self {
        Self {
            shared: Arc::new(PoolShared {
                jobs: Mutex::new(VecDeque::new()),
                job_ready: Condvar::new(),
                shutdown: AtomicBool::new(false),
            }),
            handles: Vec::new(),
        }
    }

    /// Grow the pool to `count` workers; already-running workers are kept.
    fn ensure_workers(&mut self, count: usize) {
        while self.handles.len() < count {
            let shared = Arc::clone(&self.shared);
            self.handles
                .push(thread::spawn(move || worker_loop(shared)));
        }
    }
}

impl Drop for WorkerPool {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::Release);
        self.shared.job_ready.notify_all();
        for handle in self.handles.drain(..) {
            let _ = handle.join();
        }
    }
}

fn worker_loop(shared: Arc<PoolShared>) {
    loop {
        let job = {
            let mut jobs = mutex_lock(&shared.jobs);
            loop {
                if shared.shutdown.load(Ordering::Acquire) {
                    return;
                }
                if let Some(job) = jobs.pop_front() {
                    break job;
                }
                jobs = match shared.job_ready.wait(jobs) {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
            }
        };
        let RegionJob {
            key,
            mut region,
            tick,
            budget,
            reply,
        } = job;
        let outcome = match panic::catch_unwind(AssertUnwindSafe(|| {
            region.advance_to(tick, budget)
        })) {
            Ok(result) => RegionOutcome::Done {
                key,
                result,
                region,
            },
            Err(payload) => RegionOutcome::Panicked {
                key,
                region,
                payload,
            },
        };
        let _ = reply.send(outcome);
    }
}

fn mutex_lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub struct Region {
    pub kinetic: KineticGraph,
    pub contraptions: BTreeMap<u32, Contraption>,
    transport: SleepingTransport,
    fluid: FluidNetwork,
    recipes: RecipeProcessor,
    fluid_updates: Vec<(u32, TankState)>,
    fluid_cursor: usize,
    pub(crate) batch: BatchQueue,
}

pub(crate) struct BatchQueue {
    key: RegionKey,
    pending: VecDeque<Delta>,
    pending_bytes: usize,
    sequence: u64,
}

struct Delta {
    tick: u64,
    source: u32,
    kind: u16,
    payload: Payload,
}

/// Built-in records fit inline; only adapter snapshots larger than this spill
/// to the heap.
const INLINE_PAYLOAD_BYTES: usize = 16;

enum Payload {
    Inline {
        len: u16,
        bytes: [u8; INLINE_PAYLOAD_BYTES],
    },
    Heap(Box<[u8]>),
}

impl Payload {
    fn new(bytes: &[u8]) -> Self {
        if bytes.len() <= INLINE_PAYLOAD_BYTES {
            let mut inline = [0; INLINE_PAYLOAD_BYTES];
            inline[..bytes.len()].copy_from_slice(bytes);
            Self::Inline {
                len: bytes.len() as u16,
                bytes: inline,
            }
        } else {
            Self::Heap(bytes.into())
        }
    }

    fn as_slice(&self) -> &[u8] {
        match self {
            Self::Inline { len, bytes } => &bytes[..*len as usize],
            Self::Heap(bytes) => bytes,
        }
    }
}

impl Delta {
    fn bytes(&self) -> usize {
        RECORD_HEADER_BYTES + self.payload.as_slice().len()
    }
}

impl Region {
    pub fn new(key: RegionKey) -> Self {
        Self {
            kinetic: KineticGraph::default(),
            contraptions: BTreeMap::new(),
            transport: SleepingTransport::default(),
            fluid: FluidNetwork::default(),
            recipes: RecipeProcessor::default(),
            fluid_updates: Vec::new(),
            fluid_cursor: 0,
            batch: BatchQueue::new(key),
        }
    }

    pub fn transport(&self) -> &SleepingTransport {
        &self.transport
    }

    pub fn fluid(&self) -> &FluidNetwork {
        &self.fluid
    }

    /// Earliest scheduled transport, recipe, or fluid work for this region.
    /// Fluid dirtiness is due at the region's current logical tick.
    pub fn next_due_tick(&self) -> Option<u64> {
        self.next_work().map(|(tick, _)| tick)
    }

    /// Whether this region has no scheduled work, fluid dirtiness, generated
    /// fluid deltas, or unexported output. Mutating through a scheduler's
    /// `region_mut` wakes its region before the mutation is applied.
    pub fn is_idle(&self) -> bool {
        self.next_work().is_none()
            && self.fluid_cursor == self.fluid_updates.len()
            && self.batch.is_empty()
    }

    fn advance_idle_clock_to(&mut self, tick: u64) {
        if self.next_due_tick().is_none_or(|due| due >= tick)
            && self.transport.check_time(tick).is_ok()
        {
            self.transport.advance_clock_to(tick);
            self.recipes.advance_idle_to(tick);
        }
    }

    pub fn add_tank(&mut self, id: u32, capacity: u32, elevation: i32) -> Result<(), FluidError> {
        self.fluid.add_tank(id, capacity, elevation)
    }

    pub fn set_tank(&mut self, id: u32, capacity: u32, elevation: i32) -> Result<(), FluidError> {
        self.fluid.set_tank(id, capacity, elevation)
    }

    pub fn set_fluid_link(&mut self, a: u32, b: u32, head: Option<i32>) -> Result<(), FluidError> {
        self.fluid.set_link(a, b, head)
    }

    pub fn insert_fluid(&mut self, id: u32, fluid: u32, amount: u32) -> Result<u32, FluidError> {
        self.fluid.insert(id, fluid, amount)
    }

    pub fn extract_fluid(&mut self, id: u32, amount: u32) -> Result<u32, FluidError> {
        self.fluid.extract(id, amount)
    }

    pub fn add_processor(
        &mut self,
        id: u32,
        input_capacity: u32,
        output_capacity: u32,
        recipes: Vec<Recipe>,
        drive: Drive,
    ) -> Result<(), RecipeError> {
        self.recipes
            .add_machine(id, input_capacity, output_capacity, recipes, drive)
    }

    pub fn set_processor_drive(&mut self, id: u32, drive: Drive) -> Result<(), RecipeError> {
        self.recipes.set_drive(id, drive)
    }

    pub fn insert_ingredient(&mut self, id: u32, stack: Stack) -> Result<u32, RecipeError> {
        self.recipes.insert(id, stack)
    }

    pub fn extract_product(&mut self, id: u32, stack: Stack) -> Result<u32, RecipeError> {
        self.recipes.extract(id, stack)
    }

    pub fn distribute_product(
        &mut self,
        id: u32,
        stack: Stack,
        destinations: &[u32],
    ) -> Result<u32, RecipeError> {
        self.recipes.distribute(id, stack, destinations)
    }

    pub fn add_machine(
        &mut self,
        id: u32,
        capacity: u32,
        delay: u64,
    ) -> Result<(), TransportError> {
        self.transport.add_machine(id, capacity, delay)
    }

    pub fn set_downstream(&mut self, id: u32, to: Option<u32>) -> Result<(), TransportError> {
        self.transport.set_downstream(id, to)
    }

    pub fn insert_item(&mut self, id: u32, item: u32) -> Result<(), TransportError> {
        self.transport.insert(id, item)
    }

    pub fn extract_output(&mut self, id: u32) -> Result<Option<u32>, TransportError> {
        self.transport.extract_output(id)
    }

    /// Advance this region's ready work in timestamp order. Fluid edits run
    /// at the current clock and transport precedes recipes on equal deadlines.
    pub fn advance_to(
        &mut self,
        tick: u64,
        budget: RegionBudget,
    ) -> Result<RegionReport, TransportError> {
        self.transport.check_time(tick)?;
        let time_limit = budget.max_time.map(|limit| (Instant::now(), limit));
        let mut report = RegionReport::default();
        while let Some((due, work)) = self.next_work().filter(|(due, _)| *due <= tick) {
            if report.processed == budget.max_steps {
                break;
            }
            if !self.batch.has_room(work.payload_bytes()) {
                report.output_blocked = true;
                break;
            }
            if time_limit.is_some_and(|(start, limit)| start.elapsed() >= limit) {
                report.time_exhausted = true;
                break;
            }
            self.transport.advance_clock_to(due);
            self.recipes.advance_idle_to(due);
            match work {
                Work::Fluid => {
                    if self.fluid_cursor == self.fluid_updates.len() {
                        self.fluid_updates.clear();
                        self.fluid_cursor = 0;
                        self.fluid.step_into(&mut self.fluid_updates);
                    }
                    if self.fluid_cursor < self.fluid_updates.len() {
                        let (id, state) = self.fluid_updates[self.fluid_cursor];
                        self.fluid_cursor += 1;
                        self.queue_tank(due, id, state);
                    }
                }
                Work::Transport => {
                    let Some(delta) = self.transport.step(due) else {
                        break;
                    };
                    self.queue_transfer(delta);
                }
                Work::Recipe => {
                    let Some(event) = self.recipes.step() else {
                        break;
                    };
                    let mut payload = [0; RECIPE_PAYLOAD_BYTES];
                    payload[0] = u8::from(event.recipe.is_some());
                    payload[1..].copy_from_slice(&event.recipe.unwrap_or(0).to_le_bytes());
                    self.batch
                        .push(event.tick, event.machine, RECIPE_EVENT, &payload);
                }
            }
            report.processed += 1;
        }
        report.backlog = self
            .next_work()
            .map(|(due, _)| due)
            .filter(|&due| due <= tick);
        if report.backlog.is_none() {
            self.transport.advance_clock_to(tick);
            self.recipes.advance_idle_to(tick);
        }
        Ok(report)
    }

    fn next_work(&self) -> Option<(u64, Work)> {
        if self.fluid_cursor < self.fluid_updates.len() || self.fluid.has_pending() {
            return Some((self.transport.now(), Work::Fluid));
        }
        match (self.transport.next_wake_tick(), self.recipes.next_due()) {
            (Some(transport), Some(recipe)) if transport <= recipe => {
                Some((transport, Work::Transport))
            }
            (_, Some(recipe)) => Some((recipe, Work::Recipe)),
            (Some(transport), None) => Some((transport, Work::Transport)),
            (None, None) => None,
        }
    }

    /// Queue an adapter-defined snapshot delta at the region's logical tick.
    /// The caller retains its payload on failure so it can retry after export.
    pub fn queue_delta(
        &mut self,
        kind: u16,
        source: u32,
        payload: &[u8],
    ) -> Result<(), BatchError> {
        if kind < FIRST_ADAPTER_KIND {
            return Err(BatchError::ReservedKind);
        }
        self.batch.check_room(payload.len())?;
        self.batch.push(self.transport.now(), source, kind, payload);
        Ok(())
    }

    /// Export the longest FIFO prefix that fits the caller's byte and record
    /// limits. An empty batch writes nothing and does not consume a sequence.
    pub fn export_into(
        &mut self,
        buffer: &mut [u8],
        max_records: usize,
    ) -> Result<BatchReport, BatchError> {
        self.batch
            .export_with(buffer.len(), max_records, |offset, bytes| {
                buffer[offset..offset + bytes.len()].copy_from_slice(bytes);
            })
    }

    fn queue_transfer(&mut self, delta: TransportDelta) {
        let mut payload = [0; TRANSFER_PAYLOAD_BYTES];
        payload[0] = u8::from(delta.destination.is_some());
        payload[1..5].copy_from_slice(&delta.destination.unwrap_or(0).to_le_bytes());
        payload[5..9].copy_from_slice(&delta.item.to_le_bytes());
        self.batch
            .push(delta.tick, delta.source, TRANSPORT_TRANSFER, &payload);
    }

    fn queue_tank(&mut self, tick: u64, id: u32, state: TankState) {
        let mut payload = [0; TANK_PAYLOAD_BYTES];
        payload[0] = u8::from(state.fluid.is_some());
        payload[1..5].copy_from_slice(&state.fluid.unwrap_or(0).to_le_bytes());
        payload[5..9].copy_from_slice(&state.amount.to_le_bytes());
        self.batch.push(tick, id, FLUID_TANK_UPDATE, &payload);
    }
}

impl BatchQueue {
    pub(crate) fn new(key: RegionKey) -> Self {
        Self {
            key,
            pending: VecDeque::new(),
            pending_bytes: 0,
            sequence: 0,
        }
    }

    fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    pub(crate) fn check_room(&self, payload_len: usize) -> Result<(), BatchError> {
        if payload_len > u16::MAX as usize {
            return Err(BatchError::OversizedRecord);
        }
        if !self.has_room(payload_len) {
            return Err(BatchError::QueueFull);
        }
        Ok(())
    }

    /// Writes wire bytes directly to a caller-provided sink. Offsets and
    /// lengths always fall within the supplied capacity.
    pub(crate) fn export_with(
        &mut self,
        capacity: usize,
        max_records: usize,
        mut write: impl FnMut(usize, &[u8]),
    ) -> Result<BatchReport, BatchError> {
        let empty = BatchReport {
            bytes: 0,
            records: 0,
            sequence: None,
        };
        if max_records == 0 {
            return Ok(empty);
        }
        let Some(first) = self.pending.front() else {
            return Ok(empty);
        };
        let limit = capacity.min(MAX_BATCH_BYTES);
        let needed = BATCH_HEADER_BYTES + first.bytes();
        if needed > limit {
            return Err(BatchError::BufferTooSmall { needed });
        }
        let sequence = self
            .sequence
            .checked_add(1)
            .ok_or(BatchError::SequenceExhausted)?;
        let mut offset = BATCH_HEADER_BYTES;
        let mut count = 0_u32;
        while (count as usize) < max_records.min(MAX_BATCH_RECORDS) {
            let Some(delta) = self.pending.front() else {
                break;
            };
            let bytes = delta.bytes();
            if offset + bytes > limit {
                break;
            }
            let Some(delta) = self.pending.pop_front() else {
                break;
            };
            let mut record = [0; RECORD_HEADER_BYTES];
            record[..8].copy_from_slice(&delta.tick.to_le_bytes());
            record[8..12].copy_from_slice(&delta.source.to_le_bytes());
            record[12..14].copy_from_slice(&delta.kind.to_le_bytes());
            record[14..16].copy_from_slice(&(delta.payload.as_slice().len() as u16).to_le_bytes());
            write(offset, &record);
            write(offset + RECORD_HEADER_BYTES, delta.payload.as_slice());
            offset += bytes;
            count += 1;
            self.pending_bytes -= bytes;
        }
        let mut header = [0; BATCH_HEADER_BYTES];
        header[..4].copy_from_slice(&FRAME_MAGIC);
        header[4..8].copy_from_slice(&count.to_le_bytes());
        header[8..16].copy_from_slice(&sequence.to_le_bytes());
        header[16..20].copy_from_slice(&self.key.dimension.to_le_bytes());
        header[20..24].copy_from_slice(&self.key.x.to_le_bytes());
        header[24..28].copy_from_slice(&self.key.z.to_le_bytes());
        header[28..32].copy_from_slice(&self.key.y.to_le_bytes());
        write(0, &header);
        self.sequence = sequence;
        Ok(BatchReport {
            bytes: offset,
            records: count as usize,
            sequence: Some(sequence),
        })
    }

    fn has_room(&self, payload_len: usize) -> bool {
        self.pending.len() < MAX_BATCH_RECORDS
            && BATCH_HEADER_BYTES + self.pending_bytes + RECORD_HEADER_BYTES + payload_len
                <= MAX_BATCH_BYTES
    }

    pub(crate) fn push(&mut self, tick: u64, source: u32, kind: u16, payload: &[u8]) {
        self.pending_bytes += RECORD_HEADER_BYTES + payload.len();
        self.pending.push_back(Delta {
            tick,
            source,
            kind,
            payload: Payload::new(payload),
        });
    }
}

#[derive(Clone, Copy)]
enum Work {
    Fluid,
    Transport,
    Recipe,
}

impl Work {
    fn payload_bytes(self) -> usize {
        match self {
            Self::Fluid => TANK_PAYLOAD_BYTES,
            Self::Transport => TRANSFER_PAYLOAD_BYTES,
            Self::Recipe => RECIPE_PAYLOAD_BYTES,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUDGET: RegionBudget = RegionBudget {
        max_steps: 1,
        max_time: None,
    };

    #[test]
    fn block_coordinates_use_euclidean_regions_and_include_dimension() {
        assert_eq!(
            RegionKey::for_block(-1, -1, -32),
            RegionKey {
                dimension: -1,
                x: -1,
                y: 0,
                z: -1
            }
        );
        assert_eq!(
            RegionKey::for_block(1, -33, 32),
            RegionKey {
                dimension: 1,
                x: -2,
                y: 0,
                z: 1
            }
        );
        assert_ne!(RegionKey::for_block(1, 0, 0), RegionKey::for_block(2, 0, 0));
    }

    #[test]
    fn deadlines_and_full_queue_are_local_to_a_region() {
        let mut scheduler = RegionScheduler::default();
        let busy = RegionKey::for_block(0, 0, 0);
        let quiet = RegionKey::for_block(0, 32, 0);
        for key in [busy, quiet] {
            let region = scheduler.region_mut(key);
            region.add_machine(0, 2, 1).unwrap();
            region.insert_item(0, 3).unwrap();
            region.insert_item(0, 5).unwrap();
        }
        let region = scheduler.region_mut(busy);
        assert_eq!(
            region.advance_to(
                10,
                RegionBudget {
                    max_time: Some(Duration::ZERO),
                    ..BUDGET
                }
            ),
            Ok(RegionReport {
                processed: 0,
                backlog: Some(1),
                time_exhausted: true,
                output_blocked: false
            })
        );
        for _ in 0..MAX_BATCH_RECORDS {
            region.queue_delta(FIRST_ADAPTER_KIND, 0, b"").unwrap();
        }
        assert_eq!(
            region.queue_delta(FIRST_ADAPTER_KIND, 0, b""),
            Err(BatchError::QueueFull)
        );
        assert_eq!(
            region.advance_to(10, BUDGET),
            Ok(RegionReport {
                processed: 0,
                backlog: Some(1),
                time_exhausted: false,
                output_blocked: true
            })
        );
        assert_eq!(region.transport().now(), 0);

        let region = scheduler.region_mut(quiet);
        assert_eq!(region.advance_to(10, BUDGET).unwrap().backlog, Some(2));
        assert_eq!(region.advance_to(10, BUDGET).unwrap().backlog, None);
        assert_eq!(region.extract_output(0), Ok(Some(3)));
        assert_eq!(region.extract_output(0), Ok(Some(5)));
        assert_eq!(region.transport().now(), 10);

        let mut buffer = [0; 128];
        let region = scheduler.region_mut(busy);
        assert_eq!(region.export_into(&mut buffer, 1).unwrap().records, 1);
        assert_eq!(region.advance_to(10, BUDGET).unwrap().processed, 1);
        assert_eq!(region.extract_output(0), Ok(Some(3)));
    }

    #[test]
    fn batch_has_exact_wire_bytes_and_preserves_remainder_on_small_buffers() {
        let mut scheduler = RegionScheduler::default();
        let region = scheduler.region_mut(RegionKey {
            dimension: -2,
            x: -1,
            y: 0,
            z: 3,
        });
        region.add_machine(7, 1, 2).unwrap();
        region.add_machine(0, 1, 1).unwrap();
        region.set_downstream(7, Some(0)).unwrap();
        region.insert_item(7, 0x1234_abcd).unwrap();
        region.advance_to(2, BUDGET).unwrap();
        region
            .queue_delta(FIRST_ADAPTER_KIND, 0x102, b"pose")
            .unwrap();

        let mut short = [0xee; 56];
        assert_eq!(
            region.export_into(&mut short, 2),
            Err(BatchError::BufferTooSmall { needed: 57 })
        );
        assert_eq!(short, [0xee; 56]);
        let mut buffer = [0xee; 64];
        let first = region.export_into(&mut buffer, 2).unwrap();
        assert_eq!(
            first,
            BatchReport {
                bytes: 57,
                records: 1,
                sequence: Some(1)
            }
        );
        assert_eq!(
            &buffer[..32],
            &[
                b'S', b'5', 1, 1, 1, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 254, 255, 255, 255, 255, 255,
                255, 255, 3, 0, 0, 0, 0, 0, 0, 0,
            ]
        );
        assert_eq!(
            &buffer[32..57],
            &[
                2, 0, 0, 0, 0, 0, 0, 0, 7, 0, 0, 0, 1, 0, 9, 0, 1, 0, 0, 0, 0, 0xcd, 0xab, 0x34,
                0x12,
            ]
        );
        assert_eq!(&buffer[57..], &[0xee; 7]);
        let second = region.export_into(&mut buffer, 1).unwrap();
        assert_eq!(
            second,
            BatchReport {
                bytes: 52,
                records: 1,
                sequence: Some(2)
            }
        );
        assert_eq!(
            &buffer[32..52],
            &[2, 0, 0, 0, 0, 0, 0, 0, 2, 1, 0, 0, 0, 1, 4, 0, b'p', b'o', b's', b'e']
        );
        assert_eq!(region.export_into(&mut buffer, 1).unwrap().sequence, None);
    }

    #[test]
    fn batch_validates_record_kind_length_and_queue_byte_budget() {
        let mut scheduler = RegionScheduler::default();
        let region = scheduler.region_mut(RegionKey::for_block(0, 0, 0));
        let payload = vec![0; u16::MAX as usize];
        assert_eq!(
            region.queue_delta(TRANSPORT_TRANSFER, 0, b""),
            Err(BatchError::ReservedKind)
        );
        assert_eq!(
            region.queue_delta(FIRST_ADAPTER_KIND, 0, &[0; 65536]),
            Err(BatchError::OversizedRecord)
        );
        for _ in 0..3 {
            region.queue_delta(FIRST_ADAPTER_KIND, 0, &payload).unwrap();
        }
        assert_eq!(
            region.queue_delta(FIRST_ADAPTER_KIND, 0, &payload),
            Err(BatchError::QueueFull)
        );
        let mut buffer = vec![0; MAX_BATCH_BYTES];
        assert_eq!(region.export_into(&mut buffer, 2).unwrap().records, 2);
        region.queue_delta(FIRST_ADAPTER_KIND, 0, &payload).unwrap();
        assert_eq!(
            region
                .export_into(&mut buffer, MAX_BATCH_RECORDS)
                .unwrap()
                .records,
            2
        );
    }

    /// A custom tall dimension twice the vanilla depth: block y spans
    /// -2032..=2031, well past 1,008 blocks.
    const TALL_DIM: i32 = 7;

    fn tall_keys() -> Vec<RegionKey> {
        vec![
            RegionKey::for_block(0, 0, 0),                      // overworld column
            RegionKey::for_block(-1, 0, 0),                     // nether
            RegionKey::for_block(1, 0, 0),                      // end
            RegionKey::for_block_tall(TALL_DIM, 0, -2048, 0),   // bedrock band
            RegionKey::for_block_tall(TALL_DIM, 0, -1024, 0),   // deep band
            RegionKey::for_block_tall(TALL_DIM, 0, 0, 0),       // surface band
            RegionKey::for_block_tall(TALL_DIM, 0, 1024, 0),    // sky band
            RegionKey::for_block_tall(TALL_DIM, 0, 2031, 0),    // roof band
            RegionKey::for_block_tall(TALL_DIM, -64, 512, -64), // offset cell
        ]
    }

    fn seed_region(region: &mut Region, machine: u32, items: &[u32]) {
        region.add_machine(machine, 8, 1).unwrap();
        for &item in items {
            region.insert_item(machine, item).unwrap();
        }
    }

    #[test]
    fn tall_dimension_bands_are_euclidean_and_carry_y_on_the_wire() {
        assert_eq!(
            RegionKey::for_block_tall(TALL_DIM, 33, -1025, -1),
            RegionKey {
                dimension: TALL_DIM,
                x: 1,
                y: -33,
                z: -1
            }
        );
        // The vanilla column helper keeps band 0 even for tall heights' x/z.
        assert_eq!(RegionKey::for_block(TALL_DIM, 0, 0).y, 0);
        // Distinct bands are distinct regions; the same band maps together.
        assert_ne!(
            RegionKey::for_block_tall(TALL_DIM, 0, -1, 0),
            RegionKey::for_block_tall(TALL_DIM, 0, 0, 0)
        );
        assert_eq!(
            RegionKey::for_block_tall(TALL_DIM, 0, 100, 0),
            RegionKey::for_block_tall(TALL_DIM, 0, 101, 0)
        );

        // The exported S5 header carries the y band in its last word.
        let key = RegionKey::for_block_tall(TALL_DIM, 0, -1024, 0); // y band -32
        let mut scheduler = RegionScheduler::default();
        let region = scheduler.region_mut(key);
        region.queue_delta(FIRST_ADAPTER_KIND, 0, b"b").unwrap();
        let mut frame = [0; 64];
        let report = region.export_into(&mut frame, 1).unwrap();
        assert_eq!(report.bytes, 32 + 16 + 1);
        assert_eq!(&frame[16..20], &TALL_DIM.to_le_bytes());
        assert_eq!(&frame[28..32], &(-32_i32).to_le_bytes());
    }

    #[test]
    fn multi_dimension_and_tall_regions_progress_independently() {
        let mut scheduler = RegionScheduler::default();
        let keys = tall_keys();
        for (i, &key) in keys.iter().enumerate() {
            seed_region(
                scheduler.region_mut(key),
                i as u32,
                &[100 + i as u32, 200 + i as u32],
            );
        }
        // Advance only the deep band; every other region must stay at tick 0.
        let deep = RegionKey::for_block_tall(TALL_DIM, 0, -1024, 0);
        let report = scheduler.region_mut(deep).advance_to(4, BUDGET).unwrap();
        assert_eq!(report.processed, 1);
        for &key in &keys {
            let now = scheduler.region(key).unwrap().transport().now();
            assert_eq!(now, if key == deep { 1 } else { 0 }, "{key:?}");
        }
        // Region ordering is a deterministic sorted sequence: dimension
        // first, then x, band and z.
        assert_eq!(
            scheduler.region_keys(),
            {
                let mut sorted = keys.clone();
                sorted.sort();
                sorted
            }
        );
    }

    /// One fully deterministic run of every region, returning each exported
    /// frame in sorted-key order. `budget.max_time` stays `None` — wall time
    /// is never part of the result.
    type RunAllExport = (
        Vec<(RegionKey, Result<RegionReport, TransportError>)>,
        Vec<(RegionKey, Vec<u8>)>,
    );

    fn run_all_and_export(
        scheduler: &mut RegionScheduler,
        tick: u64,
        threads: usize,
    ) -> RunAllExport {
        let reports = scheduler.advance_all(
            tick,
            RegionBudget {
                max_steps: 64,
                max_time: None,
            },
            threads,
        );
        let mut frames = Vec::new();
        let mut buffer = vec![0; MAX_BATCH_BYTES];
        for key in scheduler.region_keys() {
            let report = scheduler
                .region_mut(key)
                .export_into(&mut buffer, MAX_BATCH_RECORDS)
                .unwrap();
            frames.push((key, buffer[..report.bytes].to_vec()));
        }
        (reports, frames)
    }

    #[test]
    fn advance_all_is_bit_identical_between_serial_and_parallel() {
        let keys = tall_keys();
        let build = || {
            let mut scheduler = RegionScheduler::default();
            for (i, &key) in keys.iter().enumerate() {
                let region = scheduler.region_mut(key);
                seed_region(region, i as u32, &[100 + i as u32, 200 + i as u32]);
                region.queue_delta(FIRST_ADAPTER_KIND, i as u32, b"mark").unwrap();
            }
            scheduler
        };
        let mut serial = build();
        let mut parallel = build();
        let mut octa = build();

        let serial_out = run_all_and_export(&mut serial, 40, 1);
        let parallel_out = run_all_and_export(&mut parallel, 40, 4);
        let octa_out = run_all_and_export(&mut octa, 40, 8);

        assert_eq!(serial_out, parallel_out);
        assert_eq!(serial_out, octa_out);
        // Keys are reported in sorted order, one outcome each.
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(
            serial_out.0.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
            sorted
        );
        // Every region processed its seeded work: 2 items each plus the
        // queued delta already exported.
        for (_, report) in &serial_out.0 {
            assert_eq!(report.unwrap().processed, 2);
        }
        // A second identical round keeps parity (regions are deterministic).
        let serial_out2 = run_all_and_export(&mut serial, 80, 1);
        let parallel_out2 = run_all_and_export(&mut parallel, 80, 4);
        assert_eq!(serial_out2, parallel_out2);
    }

    #[test]
    fn advance_all_keeps_sorted_order_and_survives_a_region_error() {
        let mut scheduler = RegionScheduler::default();
        let keys = tall_keys();
        for (i, &key) in keys.iter().enumerate() {
            seed_region(scheduler.region_mut(key), i as u32, &[i as u32]);
        }
        // Push one region's clock ahead, then ask all regions for an earlier
        // tick: that region reports its own error; neighbours still advance.
        let ahead = keys[3];
        scheduler.region_mut(ahead).advance_to(50, BUDGET).unwrap();
        let reports = scheduler.advance_all(40, BUDGET, 4);
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(
            reports.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
            sorted
        );
        for (key, outcome) in &reports {
            if *key == ahead {
                assert_eq!(
                    *outcome,
                    Err(TransportError::TimeWentBackwards {
                        current: 50,
                        requested: 40
                    })
                );
            } else {
                assert!(outcome.is_ok(), "{key:?}: {outcome:?}");
            }
        }
        // Empty scheduler: no work, no panic, whatever the thread count.
        let mut empty = RegionScheduler::default();
        assert!(empty.advance_all(10, BUDGET, 4).is_empty());
    }

    #[test]
    fn idle_single_region_stays_empty_and_active_work_obeys_budget() {
        let key = RegionKey::for_block(0, 0, 0);
        let mut scheduler = RegionScheduler::default();
        scheduler.region_mut(key);

        for tick in 1..=32 {
            let reports = scheduler.advance_all(tick, BUDGET, 1);
            assert_eq!(reports.len(), 1);
            assert_eq!(reports[0].0, key);
            assert_eq!(reports[0].1.unwrap().processed, 0);
            assert_eq!(reports[0].1.unwrap().backlog, None);
        }

        seed_region(scheduler.region_mut(key), 42, &[7, 8, 9]);
        let first = scheduler.advance_all(40, BUDGET, 1);
        assert_eq!(first[0].1.unwrap().processed, 1);
        assert!(first[0].1.unwrap().backlog.is_some());

        let second = scheduler.advance_all(40, BUDGET, 1);
        assert_eq!(second[0].1.unwrap().processed, 1);
        let third = scheduler.advance_all(40, BUDGET, 1);
        assert_eq!(third[0].1.unwrap().processed, 1);
        assert_eq!(third[0].1.unwrap().backlog, None);
        assert_eq!(scheduler.advance_all(40, BUDGET, 1)[0].1.unwrap().processed, 0);
    }
}
