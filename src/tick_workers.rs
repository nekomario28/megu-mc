//! Bounded off-tick compute for `mob_tick`'s pure per-entity reads (H9/H10/H11).
//!
//! `live_mob_tick_dimension` walks `mobs_snap` in order and, before any mutation,
//! does a handful of read-only probes per mob: feet/above block states, water and
//! lava classification, the drowning "eye submerged" scan, the suffocating-cell
//! scan, sky visibility for sun burning, the block under a sheep, the pet
//! teleport candidate search, and the nearest/attackable/stare/look player
//! targeting scan (including the enderman's raycast). All of those only need the
//! entity snapshot plus read-only world/player access, so this module computes
//! them on a small fixed pool while the tick thread is still upstream in the
//! loop, then the loop commits the answers in the exact same serial order —
//! every mutation, broadcast, and RNG draw still happens on the tick thread.
//!
//! Safety rules mirror `stream_gen.rs`:
//!
//! * Fixed worker count (default `max(1, threads-2)` clamped to `1..=4`,
//!   `MEGU_TICK_WORKERS` to override), one bounded job list per dimension tick —
//!   never an unbounded queue: at most one `MobJobInput` per snapped mob exists.
//! * Every worker job runs under `catch_unwind`; a panic or a pool failure
//!   resolves that mob's slot to `Err`, and the tick thread recomputes the same
//!   values inline exactly where the serial code computed them.
//! * A plan is only applied while it is still true: the mob must not have moved
//!   since the probe position was captured, the recorded world epoch
//!   (`CHUNK_VERSION_SEQ`) must still match so no `set_block` landed mid-batch,
//!   the generation seq must match, the recorded spectator flags and enderman
//!   stare inputs (live player pose + helmet slot) must still match, and the
//!   recorded provoked flag must match the live `hurt_at` value. Anything stale
//!   falls back to the identical inline computation — observable behavior is
//!   bit-identical to serial.
//! * Kill switch `MEGU_TICK_PARALLEL=0` (checked once at startup) makes every
//!   batch resolve as "no plan", so the loop executes the untouched serial
//!   expressions inline.
//!
//! Counters: `TICKPAR_REQUESTED` jobs enqueued, `TICKPAR_COMPUTED` plans a
//! worker finished (the "off-tick compute ran" proof), `TICKPAR_FRESH` plans
//! the commit consumed, `TICKPAR_STALE` plans discarded for changed inputs,
//! `TICKPAR_FAILED` pool/panic/timeout losses. `MEGU_TICKPAR_PROF=1` prints a
//! summary line every 64 resolved batches.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicI64, AtomicI8, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::LocalKey;
use std::time::{Duration, Instant};

use crate::block_registry::{breg, BReg};
use crate::worldgen::DimId;
use crate::{
    block_at, block_at_cached, block_at_cached_dim, drowned_should_hide_in_water,
    enderman_stared_at, entity_distance_sq, entity_spec_by_id, height_at, item_to_name,
    live_mob_block_hit, live_mob_states, mob_cell_solid, mob_drowning_submerged, mob_in_lava,
    mob_in_water, pet_follow_teleport_step, shrink_if_sparse, spider_is_hostile,
    AiEntityPerception, Client, LiveMobPlayer, Mob, MobBehaviorState, CHUNK_VERSION_SEQ,
};

// ---------------------------------------------------------------------------
// Counters / env knobs
// ---------------------------------------------------------------------------

/// Jobs handed to the worker pool.
pub(crate) static TICKPAR_REQUESTED: AtomicU64 = AtomicU64::new(0);
/// Plans a worker actually finished computing (off-tick compute proof).
pub(crate) static TICKPAR_COMPUTED: AtomicU64 = AtomicU64::new(0);
/// Plans the commit consumed (position + world epoch + live inputs still true).
pub(crate) static TICKPAR_FRESH: AtomicU64 = AtomicU64::new(0);
/// Plans discarded because a recorded input changed before the mob was reached.
pub(crate) static TICKPAR_STALE: AtomicU64 = AtomicU64::new(0);
/// Jobs lost to panic, pool failure, or a commit-side wait timeout.
pub(crate) static TICKPAR_FAILED: AtomicU64 = AtomicU64::new(0);

static PROF_COUNT: AtomicU64 = AtomicU64::new(0);

fn env_flag(name: &str, default: bool) -> bool {
    match std::env::var(name).as_deref() {
        Ok("1") | Ok("true") | Ok("TRUE") => true,
        Ok("0") | Ok("false") | Ok("FALSE") => false,
        Ok(_) => default,
        Err(_) => default,
    }
}

fn parallel_env_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| env_flag("MEGU_TICK_PARALLEL", true))
}

/// -1 = env decides, 0 = force serial, 1 = force parallel (test hook).
static PARALLEL_OVERRIDE: AtomicI8 = AtomicI8::new(-1);

fn parallel_enabled() -> bool {
    match PARALLEL_OVERRIDE.load(Ordering::Relaxed) {
        0 => false,
        1 => true,
        _ => parallel_env_enabled(),
    }
}

#[cfg(test)]
pub(crate) fn set_parallel_override(v: Option<bool>) {
    PARALLEL_OVERRIDE.store(v.map(i8::from).unwrap_or(-1), Ordering::Relaxed);
}

/// Mob jobs one worker task prepares and commits as a single batch. A range
/// worker still computes each mob independently — identical inputs, identical
/// plan output — but lands up to this many results under one `results` lock
/// and one condvar wake instead of one task/lock/notify per mob.
const MOB_BATCH_SIZE: usize = 32;

/// -1 = `MOB_BATCH_SIZE`, otherwise the fixed range size (test hook).
static MOB_BATCH_OVERRIDE: AtomicI64 = AtomicI64::new(-1);

fn mob_batch_size() -> usize {
    match MOB_BATCH_OVERRIDE.load(Ordering::Relaxed) {
        n if n > 0 => n as usize,
        _ => MOB_BATCH_SIZE,
    }
}

#[cfg(test)]
fn set_mob_batch_override(v: Option<usize>) {
    MOB_BATCH_OVERRIDE.store(v.map(|n| n as i64).unwrap_or(-1), Ordering::Relaxed);
}

fn prof_enabled() -> bool {
    static PROF: OnceLock<bool> = OnceLock::new();
    *PROF.get_or_init(|| env_flag("MEGU_TICKPAR_PROF", false))
}

fn prof_note() {
    if !prof_enabled() {
        return;
    }
    if PROF_COUNT.fetch_add(1, Ordering::Relaxed) % 64 != 0 {
        return;
    }
    eprintln!(
        "TICKPAR requested={} computed={} fresh={} stale={} failed={}",
        TICKPAR_REQUESTED.load(Ordering::Relaxed),
        TICKPAR_COMPUTED.load(Ordering::Relaxed),
        TICKPAR_FRESH.load(Ordering::Relaxed),
        TICKPAR_STALE.load(Ordering::Relaxed),
        TICKPAR_FAILED.load(Ordering::Relaxed),
    );
}

// ---------------------------------------------------------------------------
// Snapshot types
// ---------------------------------------------------------------------------

/// A `LiveMobPlayer` made `Send` for workers: the snapshotted scalar fields plus
/// the client handle for the few live reads the targeting rules make
/// (`is_spectator`, the enderman's live stare pose/helmet). Every live read a
/// worker performs is recorded in the plan and re-verified on the tick thread
/// before the plan is consumed.
pub(crate) struct TargetPlayer {
    pub eid: i32,
    pub pos: (f64, f64, f64),
    pub dim: DimId,
    pub held_item: Option<u32>,
    pub attackable: bool,
    pub client: Arc<Client>,
}

impl TargetPlayer {
    /// `LiveMobPlayer::perceive`, field for field.
    fn perceive(&self) -> AiEntityPerception {
        let mut p = AiEntityPerception::new(self.eid, self.pos);
        p.held_item = self.held_item;
        p
    }
}

/// One live read the enderman-stare scan performed on a player: the live
/// position/look/on-ground tuple, and the helmet name iff the scan reached the
/// helmet check (i.e. the player passed the snapshot filters and distance gate).
#[derive(Debug, PartialEq)]
struct StareRead {
    player: usize,
    pos5: (f64, f64, f64, f32, f32, bool),
    helmet: Option<String>,
}

/// The pure read-only probe bundle for one mob at one position. `probe_pos` is
/// the position every read was made at; a post-travel field (`submerged`,
/// `cell_solid`) is only reused when the mob's current position still equals it.
#[derive(Clone, Copy)]
pub(crate) struct MobEnv {
    pub probe_pos: (f64, f64, f64),
    /// `block_at_cached_dim(dim, bx, by, bz)` — feet block state.
    pub feet_state: u32,
    pub in_water: bool,
    pub in_lava: bool,
    /// `mob_drowning_submerged(etype, x, y, z)` at `probe_pos` (OW only).
    pub submerged: bool,
    /// `mob_cell_solid(x, y+1, z)` at `probe_pos` (OW only).
    pub cell_solid: bool,
    /// `block_at_cached_dim(dim, bx, by+1, bz)` for non-OW dims.
    pub above_state: u32,
    /// `height_at(bx, bz)`.
    pub sky_height: i32,
    /// `block_at_cached(bx, by+2, bz)`.
    pub sky_air2: u32,
    /// `block_at(bx, by-1, bz)` for the sheep graze check (OW only).
    pub below_block: u32,
}

impl MobEnv {
    /// The worker computed this at `probe_pos`; if the mob's position changed
    /// (travel) the caller must recompute inline at the live position. Every
    /// use is also gated on `fresh` (`MobPlanSet::epoch_fresh` at that point):
    /// a `set_block` landing between dispatch and this use changes the live
    /// answer, so the stale probe must not stand in for it.
    pub(crate) fn submerged_or(
        &self,
        fresh: bool,
        pos: (f64, f64, f64),
        compute: impl FnOnce() -> bool,
    ) -> bool {
        if fresh && self.probe_pos == pos {
            self.submerged
        } else {
            compute()
        }
    }

    pub(crate) fn cell_solid_or(
        &self,
        fresh: bool,
        pos: (f64, f64, f64),
        compute: impl FnOnce() -> bool,
    ) -> bool {
        if fresh && self.probe_pos == pos {
            self.cell_solid
        } else {
            compute()
        }
    }

    /// `by + 1 >= height_at(bx, bz) && block_at_cached(bx, by + 2, bz) == 0`,
    /// probed or recomputed — same epoch rule as `submerged_or`.
    pub(crate) fn sky_visible_or(&self, fresh: bool, by: i32, compute: impl FnOnce() -> bool) -> bool {
        if fresh {
            by + 1 >= self.sky_height && self.sky_air2 == 0
        } else {
            compute()
        }
    }

    /// `height_at(bx, bz)` for the rain check — same epoch rule.
    pub(crate) fn sky_height_or(&self, fresh: bool, compute: impl FnOnce() -> i32) -> i32 {
        if fresh {
            self.sky_height
        } else {
            compute()
        }
    }

    /// `block_at(bx, by - 1, bz)` for the sheep graze check — same epoch rule.
    pub(crate) fn below_or(&self, fresh: bool, compute: impl FnOnce() -> u32) -> u32 {
        if fresh {
            self.below_block
        } else {
            compute()
        }
    }

    /// `block_at_cached_dim(dim, bx, by + 1, bz)` for non-OW suffocation —
    /// same epoch rule.
    pub(crate) fn above_or(&self, fresh: bool, compute: impl FnOnce() -> u32) -> u32 {
        if fresh {
            self.above_state
        } else {
            compute()
        }
    }
}

/// Pet-teleport plan for etype 15|122. `state` is the recorded
/// `(owner, sitting)` pair read from `live_mob_states`; the plan is only reused
/// when the tick-thread re-read of that state still equals it. `teleport` is
/// `pet_follow_teleport_step`'s answer for the recorded inputs.
#[derive(Clone, Copy)]
pub(crate) struct PetPlan {
    pub state: Option<(Option<[u8; 16]>, bool)>,
    pub teleport: Option<(f64, f64, f64)>,
}

/// Player targeting outcome for one mob. Indices refer to the shared
/// `players` snapshot (same order as the tick's `LiveMobPlayer` slice), so the
/// commit phase resolves them back to `&LiveMobPlayer` without copying.
pub(crate) struct MobTargeting {
    /// Position the scan ran at — must equal the mob's post-travel position.
    pub pos: (f64, f64, f64),
    /// `hurt_at.elapsed() < 10s` as the worker saw it; re-verified live.
    pub provoked: bool,
    pub nearest: Option<usize>,
    pub target: Option<usize>,
    pub perception: Option<AiEntityPerception>,
    pub look: Option<usize>,
    /// `(players index, is_spectator)` for every live spectator read, in order.
    spectators: Vec<(usize, bool)>,
    /// Every live read the enderman stare scan performed, in order.
    stare: Vec<StareRead>,
}

impl MobTargeting {
    /// Re-check every live input the plan recorded. Snapshot-derived fields
    /// (dim, attackable, held item, the snapshot `pos`/`hurt_at`) are fixed for
    /// the tick and need no re-check; the world-block inputs are covered by the
    /// batch epoch check.
    fn inputs_live(&self, players: &[TargetPlayer]) -> bool {
        for &(i, recorded) in &self.spectators {
            if players[i].client.is_spectator() != recorded {
                return false;
            }
        }
        for r in &self.stare {
            let client = &players[r.player].client;
            let live5 = *client.pos.lock().unwrap_or_else(|e| e.into_inner());
            if live5 != r.pos5 {
                return false;
            }
            if let Some(recorded_head) = &r.helmet {
                let live_head = client
                    .inv
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&5)
                    .and_then(|stack| item_to_name(stack.0).map(str::to_owned))
                    .unwrap_or_default();
                if &live_head != recorded_head {
                    return false;
                }
            }
        }
        true
    }
}

/// Everything a worker computed for one mob. Consumed field-by-field; the
/// commit only trusts it while `pos` still equals the mob's position and the
/// batch epoch is fresh.
pub(crate) struct MobPlan {
    pub pos: (f64, f64, f64),
    pub env: MobEnv,
    pub pet: Option<PetPlan>,
    pub targeting: Option<MobTargeting>,
}

// ---------------------------------------------------------------------------
// Pure probes (shared by worker and inline fallback — one definition, so the
// serial semantics stay verbatim in exactly one place)
// ---------------------------------------------------------------------------

/// The env probe block serial `live_mob_tick_dimension` ran inline. Reads are
/// the same expressions, same order, same early-outs.
pub(crate) fn env_probe(dim: DimId, reg: &BReg, etype: i64, pos: (f64, f64, f64)) -> MobEnv {
    let [bx, by, bz] = [
        pos.0.floor() as i32,
        pos.1.floor() as i32,
        pos.2.floor() as i32,
    ];
    let feet_state = block_at_cached_dim(dim, bx, by, bz);
    let ow = dim == DimId::Overworld;
    let (in_water, in_lava) = if ow {
        (
            mob_in_water(etype, pos.0, pos.1, pos.2),
            mob_in_lava(etype, pos.0, pos.1, pos.2),
        )
    } else {
        (
            reg.has_water(feet_state),
            reg.fluid_name(feet_state) == "minecraft:lava",
        )
    };
    MobEnv {
        probe_pos: pos,
        feet_state,
        in_water,
        in_lava,
        submerged: ow && mob_drowning_submerged(etype, pos.0, pos.1, pos.2),
        cell_solid: ow && mob_cell_solid(pos.0, pos.1 + 1.0, pos.2),
        above_state: if ow {
            0
        } else {
            block_at_cached_dim(dim, bx, by + 1, bz)
        },
        sky_height: height_at(bx, bz),
        sky_air2: block_at_cached(bx, by + 2, bz),
        below_block: if ow { block_at(bx, by - 1, bz) } else { 0 },
    }
}

/// Inline fallback when no valid plan exists — identical to what the worker
/// computed.
pub(crate) fn env_or(
    plan: Option<&MobPlan>,
    dim: DimId,
    reg: &BReg,
    etype: i64,
    pos: (f64, f64, f64),
) -> MobEnv {
    match plan {
        Some(p) => p.env,
        None => env_probe(dim, reg, etype, pos),
    }
}

/// Pet-teleport probe: reads `live_mob_states` and (when tamed) the owner
/// position from the shared player snapshot, then runs the pure step search.
fn pet_probe(
    dim: DimId,
    reg: &BReg,
    eid: i32,
    pos: (f64, f64, f64),
    players: &[TargetPlayer],
) -> PetPlan {
    let state = live_mob_states()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&eid)
        .and_then(|state| match &state.behavior {
            Some(MobBehaviorState::Cat(pet) | MobBehaviorState::Wolf(pet)) => {
                Some((pet.owner, pet.sitting))
            }
            _ => None,
        });
    let teleport = if let Some((Some(owner), sitting)) = state {
        players
            .iter()
            .find(|p| p.dim == dim && p.client.uuid == owner)
            .and_then(|p| {
                pet_follow_teleport_step(dim, p.pos, pos, sitting, |d, x, y, z| {
                    reg.blocks_motion(block_at_cached_dim(d, x, y, z))
                })
            })
    } else {
        None
    };
    PetPlan { state, teleport }
}

/// The whole player-targeting block, verbatim from the serial loop but writing
/// indices instead of `&LiveMobPlayer`, plus recording every live client read
/// so the commit can prove the inputs are unchanged before reusing it.
#[allow(clippy::too_many_arguments)]
fn compute_targeting(
    etype: i64,
    eye_height: f32,
    position: (f64, f64, f64),
    cell: (i32, i32, i32),
    players: &[TargetPlayer],
    dim: DimId,
    reg: &BReg,
    day: bool,
    hostile: bool,
    provoked: bool,
    in_water: bool,
    sky_height: i32,
) -> MobTargeting {
    let pos = [position.0, position.1, position.2];
    let (_bx, by, _bz) = cell;
    let distance = |p: &TargetPlayer| entity_distance_sq(position, p.pos);
    let mut spectators: Vec<(usize, bool)> = Vec::new();
    let nearest = players
        .iter()
        .enumerate()
        .filter(|(i, p)| {
            if p.dim != dim {
                return false;
            }
            let s = p.client.is_spectator();
            spectators.push((*i, s));
            !s
        })
        .min_by(|(_, a), (_, b)| distance(a).total_cmp(&distance(b)))
        .map(|(i, _)| i);
    let target = players
        .iter()
        .enumerate()
        .filter(|(_, p)| {
            p.dim == dim && p.attackable && hostile && distance(p) <= 64.0f64.powi(2)
        })
        .min_by(|(_, a), (_, b)| distance(a).total_cmp(&distance(b)))
        .map(|(i, _)| i);
    let target = if matches!(
        etype,
        8 | 23 | 29 | 31 | 35 | 45 | 50 | 76 | 80 | 91 | 116 | 118 | 119 | 124 | 100 | 16 | 27 | 33
            | 54 | 88 | 102
    ) {
        target
    } else {
        None
    };
    let mut stare = Vec::new();
    let target = if provoked {
        target
    } else {
        match etype {
            100 if !spider_is_hostile(day, if day && by + 1 >= sky_height { 15 } else { 0 }, false) => {
                None
            }
            27 if drowned_should_hide_in_water(day, in_water) => None,
            33 => {
                let enderman_eye = [pos[0], pos[1] + f64::from(eye_height), pos[2]];
                let mut stared = false;
                for (i, p) in players.iter().enumerate() {
                    if p.dim != dim || !p.attackable {
                        continue;
                    }
                    let pos5 = *p.client.pos.lock().unwrap_or_else(|e| e.into_inner());
                    let (px, py, pz, pyaw, ppitch, ..) = pos5;
                    if entity_distance_sq(position, (px, py, pz)) > 64.0f64.powi(2) {
                        stare.push(StareRead {
                            player: i,
                            pos5,
                            helmet: None,
                        });
                        continue;
                    }
                    let yaw = f64::from(pyaw).to_radians();
                    let pitch = f64::from(ppitch).to_radians();
                    let player_eye = [px, py + 1.62, pz];
                    let player_look = [
                        -yaw.sin() * pitch.cos(),
                        -pitch.sin(),
                        yaw.cos() * pitch.cos(),
                    ];
                    let head = p
                        .client
                        .inv
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get(&5)
                        .and_then(|stack| item_to_name(stack.0).map(str::to_owned))
                        .unwrap_or_default();
                    let block_hit = live_mob_block_hit(dim, reg, player_eye, enderman_eye);
                    stared = enderman_stared_at(
                        player_eye,
                        player_look,
                        enderman_eye,
                        &head,
                        block_hit.is_none(),
                    );
                    stare.push(StareRead {
                        player: i,
                        pos5,
                        helmet: Some(head),
                    });
                    if stared {
                        break;
                    }
                }
                if stared {
                    target
                } else {
                    None
                }
            }
            _ => target,
        }
    };
    let perception = target.map(|t| {
        let mut p = players[t].perceive();
        p.line_of_sight = live_mob_block_hit(
            dim,
            reg,
            [pos[0], pos[1] + f64::from(eye_height), pos[2]],
            [p.position.0, p.position.1 + 1.62, p.position.2],
        )
        .is_none();
        p
    });
    let tempting = match etype {
        19 => Some(853),
        22 | 87 => Some(854),
        77 => Some(1097),
        _ => None,
    };
    let look = tempting
        .and_then(|item| {
            players
                .iter()
                .enumerate()
                .filter(|(i, p)| {
                    if !(p.dim == dim && p.held_item == Some(item)) {
                        return false;
                    }
                    let s = p.client.is_spectator();
                    spectators.push((*i, s));
                    !s && distance(p) < 100.0
                })
                .min_by(|(_, a), (_, b)| distance(a).total_cmp(&distance(b)))
                .map(|(i, _)| i)
        })
        .or(nearest);
    MobTargeting {
        pos: position,
        provoked,
        nearest,
        target,
        perception,
        look,
        spectators,
        stare,
    }
}

// ---------------------------------------------------------------------------
// Worker pool + ordered batch
// ---------------------------------------------------------------------------

struct MobJobInput {
    mob: Arc<Mob>,
    pos: (f64, f64, f64),
    hurt_at: Instant,
    eye_height: f32,
}

/// Outcome for one plan slot. `Skipped` jobs were gated out before dispatch
/// (dead, cold, out of activation range — the loop resolves them as ordinary
/// inline fallbacks without a failure count); `Failed` means a dispatched job
/// was lost to panic or pool failure and is counted in `TICKPAR_FAILED`.
enum Slot {
    Skipped,
    Failed,
    Done(MobPlan),
}

/// Shared state for one dimension's worth of plan jobs. `results[i]` is
/// `Some(slot)` once resolved (by a worker or by pre-dispatch gating), `None`
/// while a dispatched job is still pending.
struct BatchShared {
    jobs: Vec<Option<MobJobInput>>,
    results: Mutex<Vec<Option<Slot>>>,
    done_cv: Condvar,
    players: Arc<Vec<TargetPlayer>>,
    dim: DimId,
    day: bool,
    hostile: bool,
}

thread_local! {
    /// The per-tick `jobs`/`results` buffers, recycled through
    /// [`BatchShared::drop`] so a steady mob count stops paying two fresh
    /// `Vec`s per dimension tick. Each is `mem::take`n out for the batch and
    /// handed back (cleared, sparsity-bounded) when the last `Arc` drops —
    /// normally on the tick thread, which is also where `mob_plans` runs.
    static JOB_VECS: RefCell<Vec<Option<MobJobInput>>> = const { RefCell::new(Vec::new()) };
    static RESULT_VECS: RefCell<Vec<Option<Slot>>> = const { RefCell::new(Vec::new()) };
    /// Commit scratch on worker threads: `(job index, resolved slot)` pairs
    /// gathered for one `MOB_BATCH_SIZE` range so the whole batch lands under
    /// a single `results` lock + one condvar wake. `try_borrow_mut` keeps a
    /// nested use on a local `Vec` — no panics — and the buffer shrinks when
    /// a spike leaves it far above steady use.
    static RANGE_SCRATCH: RefCell<Vec<(usize, Slot)>> = const { RefCell::new(Vec::new()) };
}

fn take_vec_scratch<T>(cell: &'static LocalKey<RefCell<Vec<T>>>) -> Vec<T> {
    cell.try_with(|c| match c.try_borrow_mut() {
        Ok(mut v) => std::mem::take(&mut *v),
        Err(_) => Vec::new(),
    })
    .unwrap_or_default()
}

fn return_vec_scratch<T>(cell: &'static LocalKey<RefCell<Vec<T>>>, mut v: Vec<T>) {
    if v.is_empty() {
        return;
    }
    // Bound while `len` still describes this tick's mob count, then clear —
    // a one-time spike must not pin its peak capacity forever.
    shrink_if_sparse(&mut v);
    v.clear();
    let _ = cell.try_with(move |c| {
        if let Ok(mut slot) = c.try_borrow_mut() {
            *slot = v;
        }
    });
}

impl Drop for BatchShared {
    fn drop(&mut self) {
        let jobs = std::mem::take(&mut self.jobs);
        let results = std::mem::take(self.results.get_mut().unwrap_or_else(|e| e.into_inner()));
        return_vec_scratch(&JOB_VECS, jobs);
        return_vec_scratch(&RESULT_VECS, results);
    }
}

enum Work {
    /// Prepare the dispatched jobs in `start..end` of one dimension batch
    /// and commit them under a single results lock.
    Plan {
        batch: Arc<BatchShared>,
        start: usize,
        end: usize,
    },
    /// Bare job used by the pool's own tests (panic survival, job accounting).
    #[cfg(test)]
    Test(Box<dyn FnOnce() + Send>),
}

struct Pool {
    pending: Mutex<VecDeque<Work>>,
    work_cv: Condvar,
    spawned: OnceLock<bool>,
    workers: usize,
}

fn worker_count() -> usize {
    static COUNT: OnceLock<usize> = OnceLock::new();
    *COUNT.get_or_init(|| {
        std::env::var("MEGU_TICK_WORKERS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or_else(|| {
                std::thread::available_parallelism()
                    .map(|n| n.get().saturating_sub(2))
                    .unwrap_or(1)
                    .clamp(1, 4)
            })
            .clamp(1, 16)
    })
}

fn pool() -> &'static Pool {
    static POOL: OnceLock<Pool> = OnceLock::new();
    POOL.get_or_init(|| Pool {
        pending: Mutex::new(VecDeque::new()),
        work_cv: Condvar::new(),
        spawned: OnceLock::new(),
        workers: worker_count(),
    })
}

impl Pool {
    fn ensure_spawned(&'static self) -> bool {
        *self.spawned.get_or_init(|| {
            (0..self.workers).all(|i| {
                std::thread::Builder::new()
                    .name(format!("tickpar-{i}"))
                    .spawn(move || worker_loop(pool()))
                    .is_ok()
            })
        })
    }

    fn submit(&self, w: Work) {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(w);
        self.work_cv.notify_one();
    }
}

fn worker_loop(pool: &'static Pool) {
    loop {
        let work = {
            let mut pending = pool.pending.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if let Some(w) = pending.pop_front() {
                    break w;
                }
                pending = pool.work_cv.wait(pending).unwrap_or_else(|e| e.into_inner());
            }
        };
        match work {
            Work::Plan { batch, start, end } => {
                // Per-job catch inside `run_plan_range`; this outer catch
                // only guards the commit path — a panic there fails every
                // still-pending slot in the range so the tick falls back
                // inline instead of stalling on the condvar timeout.
                if catch_unwind(AssertUnwindSafe(|| run_plan_range(&batch, start, end)))
                    .is_err()
                {
                    let mut results = batch.results.lock().unwrap_or_else(|e| e.into_inner());
                    let mut failed = false;
                    for idx in start..end {
                        if batch.jobs[idx].is_some() && results[idx].is_none() {
                            results[idx] = Some(Slot::Failed);
                            failed = true;
                        }
                    }
                    drop(results);
                    if failed {
                        batch.done_cv.notify_all();
                    }
                }
            }
            #[cfg(test)]
            Work::Test(job) => {
                let _ = catch_unwind(AssertUnwindSafe(job));
            }
        }
    }
}

/// Prepare every dispatched job in `start..end` and land all of them under
/// one `results` lock + one condvar wake. Jobs run in index order on this
/// worker with the same inputs the per-mob dispatch used, so each plan is
/// bit-identical — only the synchronization granularity changed.
fn run_plan_range(batch: &BatchShared, start: usize, end: usize) {
    RANGE_SCRATCH.with(|cell| match cell.try_borrow_mut() {
        Ok(mut v) => {
            v.clear();
            run_plan_range_into(batch, start, end, &mut v);
            // `len` is 0 after the drain, so bound on capacity directly: a
            // spike (huge override in tests) must not pin memory forever.
            if v.capacity() > 4096 {
                v.shrink_to(2048);
            }
        }
        Err(_) => {
            let mut v = Vec::new();
            run_plan_range_into(batch, start, end, &mut v);
        }
    });
}

fn run_plan_range_into(
    batch: &BatchShared,
    start: usize,
    end: usize,
    buf: &mut Vec<(usize, Slot)>,
) {
    for idx in start..end {
        if batch.jobs[idx].is_none() {
            continue;
        }
        let slot = match catch_unwind(AssertUnwindSafe(|| prepare(batch, idx))) {
            Ok(Ok(plan)) => Slot::Done(plan),
            _ => Slot::Failed,
        };
        buf.push((idx, slot));
    }
    if buf.is_empty() {
        return;
    }
    {
        let mut results = batch.results.lock().unwrap_or_else(|e| e.into_inner());
        for (idx, slot) in buf.drain(..) {
            results[idx] = Some(slot);
        }
    }
    batch.done_cv.notify_all();
}

fn prepare(batch: &BatchShared, idx: usize) -> Result<MobPlan, ()> {
    let job = batch.jobs[idx].as_ref().ok_or(())?;
    let reg = breg().ok_or(())?;
    let env = env_probe(batch.dim, reg, job.mob.etype, job.pos);
    let pet = matches!(job.mob.etype, 15 | 122).then(|| {
        pet_probe(batch.dim, reg, job.mob.eid, job.pos, &batch.players)
    });
    let provoked = job.hurt_at.elapsed() < Duration::from_secs(10);
    let targeting = compute_targeting(
        job.mob.etype,
        job.eye_height,
        job.pos,
        (
            job.pos.0.floor() as i32,
            job.pos.1.floor() as i32,
            job.pos.2.floor() as i32,
        ),
        &batch.players,
        batch.dim,
        reg,
        batch.day,
        batch.hostile,
        provoked,
        env.in_water,
        env.sky_height,
    );
    TICKPAR_COMPUTED.fetch_add(1, Ordering::Relaxed);
    Ok(MobPlan {
        pos: job.pos,
        env,
        pet,
        targeting: Some(targeting),
    })
}

/// Per-dimension plan set. `take_valid` resolves slot `i` (waiting on the
/// worker if needed) and returns the plan only while it still describes the
/// world the commit is about to act on.
pub(crate) struct MobPlanSet {
    batch: Option<Arc<BatchShared>>,
    len: usize,
    epoch: u64,
    gen_seq: u64,
    players: Arc<Vec<TargetPlayer>>,
    stalled: bool,
}

impl MobPlanSet {
    /// `true` while no block write or generation reset has landed since the
    /// batch was dispatched — the plans' world reads are still the world the
    /// serial code would have seen.
    pub(crate) fn epoch_fresh(&self) -> bool {
        CHUNK_VERSION_SEQ.load(Ordering::Relaxed) == self.epoch
            && crate::current_gen_seq() == self.gen_seq
    }

    /// Slot `i`'s plan, iff a worker produced one and it was captured at this
    /// exact mob position with a still-fresh epoch.
    pub(crate) fn take_valid(&mut self, i: usize, pos: (f64, f64, f64)) -> Option<MobPlan> {
        let plan = self.resolve(i)?;
        if plan.pos == pos && self.epoch_fresh() {
            TICKPAR_FRESH.fetch_add(1, Ordering::Relaxed);
            Some(plan)
        } else {
            TICKPAR_STALE.fetch_add(1, Ordering::Relaxed);
            None
        }
    }

    fn resolve(&mut self, i: usize) -> Option<MobPlan> {
        if self.stalled {
            return None;
        }
        let batch = self.batch.as_ref()?;
        if i >= batch.jobs.len() {
            return None;
        }
        let mut results = batch.results.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(r) = results[i].take() {
                return match r {
                    Slot::Done(plan) => Some(plan),
                    Slot::Failed => {
                        TICKPAR_FAILED.fetch_add(1, Ordering::Relaxed);
                        None
                    }
                    Slot::Skipped => None,
                };
            }
            let (g, timeout) = batch
                .done_cv
                .wait_timeout(results, Duration::from_millis(250))
                .unwrap_or_else(|e| e.into_inner());
            results = g;
            if timeout.timed_out() {
                // A lost worker must never hang the tick: give up on the whole
                // batch — later slots resolve instantly as inline fallbacks.
                self.stalled = true;
                TICKPAR_FAILED.fetch_add(1, Ordering::Relaxed);
                return None;
            }
        }
    }

    /// The targeting plan for this mob iff every recorded input (position,
    /// provoked flag, spectator flags, stare pose/helmets) and the world epoch
    /// still match — else recomputes inline, exactly as serial did.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn targeting(
        &self,
        plan: Option<MobTargeting>,
        provoked: bool,
        position: (f64, f64, f64),
        cell: (i32, i32, i32),
        env: &MobEnv,
        etype: i64,
        eye_height: f32,
        dim: DimId,
        reg: &BReg,
        day: bool,
        hostile: bool,
    ) -> MobTargeting {
        if let Some(t) = plan {
            if t.pos == position
                && t.provoked == provoked
                && self.epoch_fresh()
                && t.inputs_live(&self.players)
            {
                return t;
            }
            TICKPAR_STALE.fetch_add(1, Ordering::Relaxed);
        }
        compute_targeting(
            etype,
            eye_height,
            position,
            cell,
            &self.players,
            dim,
            reg,
            day,
            hostile,
            provoked,
            env.in_water,
            env.sky_height,
        )
    }
}

impl Drop for MobPlanSet {
    fn drop(&mut self) {
        prof_note();
    }
}

/// Entry point for `live_mob_tick_dimension`: snapshot the planning inputs and
/// hand the pure probes to the pool. Every mutation still happens on the tick
/// thread; this only prepares answers.
pub(crate) fn mob_plans(
    dim: DimId,
    mobs_snap: &[Arc<Mob>],
    players: &[LiveMobPlayer<'_>],
    dead: &[bool],
    hot: &[bool],
    day: bool,
    hostile: bool,
    activation: i32,
) -> MobPlanSet {
    let players: Arc<Vec<TargetPlayer>> = Arc::new(
        players
            .iter()
            .map(|p| TargetPlayer {
                eid: p.client.eid,
                pos: p.pos,
                dim: p.dim,
                held_item: p.held_item,
                attackable: p.attackable,
                client: p.client.clone(),
            })
            .collect(),
    );
    let mut set = MobPlanSet {
        batch: None,
        len: mobs_snap.len(),
        epoch: CHUNK_VERSION_SEQ.load(Ordering::Relaxed),
        gen_seq: crate::current_gen_seq(),
        players: players.clone(),
        stalled: false,
    };
    if !parallel_enabled() || set.len == 0 {
        return set;
    }
    let pool = pool();
    if !pool.ensure_spawned() {
        TICKPAR_FAILED.fetch_add(1, Ordering::Relaxed);
        return set;
    }

    // Job selection mirrors the loop's own early-out gates (dead, cold, dead-
    // soon, spec-less, out of activation range) so workers don't spend time on
    // plans the loop can never consume.
    let activation_full = activation >= crate::LoadBudget::FULL.entity_activation_range;
    let activation_sq = (activation as f64).powi(2);
    // The two per-mob buffers ride the tick thread's recycling pool: taken
    // out with their steady-state capacity and handed back by
    // `BatchShared::drop`, so a stable mob count allocates nothing here.
    let mut jobs = take_vec_scratch(&JOB_VECS);
    let mut results = take_vec_scratch(&RESULT_VECS);
    jobs.clear();
    results.clear();
    for (i, m) in mobs_snap.iter().enumerate() {
        let mut job = None;
        let mut result = None;
        if !dead[i] && hot[i] && *m.hp.lock().unwrap_or_else(|e| e.into_inner()) > 0.0 {
            let pos_guard = m.pos.lock().unwrap_or_else(|e| e.into_inner());
            let pos = (pos_guard.0, pos_guard.1, pos_guard.2);
            let cell = [
                pos.0.floor() as i32,
                pos.1.floor() as i32,
                pos.2.floor() as i32,
            ];
            let activation_ok = activation_full
                || players.iter().any(|p| {
                    p.dim == dim && entity_distance_sq(pos, p.pos) <= activation_sq
                });
            if let (true, Some(spec)) = (activation_ok, entity_spec_by_id(m.etype)) {
                if spec.max_health > 0.0 && cell[1] >= -64 && cell[1] <= 320 {
                    job = Some(MobJobInput {
                        mob: m.clone(),
                        pos,
                        hurt_at: *m.hurt_at.lock().unwrap_or_else(|e| e.into_inner()),
                        eye_height: spec.eye_height,
                    });
                }
            }
        }
        if job.is_none() {
            result = Some(Slot::Skipped);
        }
        jobs.push(job);
        results.push(result);
    }

    let batch = Arc::new(BatchShared {
        jobs,
        results: Mutex::new(results),
        done_cv: Condvar::new(),
        players,
        dim,
        day,
        hostile,
    });
    // Chunk the mob list into fixed-size ranges: each pool task prepares a
    // contiguous slice and commits it under one lock + one wake, so 500 mobs
    // cost ~16 tasks and ~16 synchronizations instead of 1,500 of each.
    // Slots are still indexed by mob position, so resolution order — and
    // therefore every observable output — is unchanged.
    let batch_size = mob_batch_size();
    let mut n = 0u64;
    let mut start = 0usize;
    while start < batch.jobs.len() {
        let end = (start + batch_size).min(batch.jobs.len());
        let pending = batch.jobs[start..end].iter().filter(|j| j.is_some()).count();
        if pending > 0 {
            n += pending as u64;
            pool.submit(Work::Plan {
                batch: batch.clone(),
                start,
                end,
            });
        }
        start = end;
    }
    TICKPAR_REQUESTED.fetch_add(n, Ordering::Relaxed);
    set.batch = Some(batch);
    set
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn stub_env() -> MobEnv {
        MobEnv {
            probe_pos: (1.5, 65.0, 1.5),
            feet_state: 0,
            in_water: false,
            in_lava: false,
            submerged: true,
            cell_solid: true,
            above_state: 0,
            sky_height: 64,
            sky_air2: 0,
            below_block: 0,
        }
    }

    fn stub_plan(pos: (f64, f64, f64)) -> MobPlan {
        MobPlan {
            pos,
            env: stub_env(),
            pet: None,
            targeting: None,
        }
    }

    fn make_set(
        jobs: Vec<Option<MobJobInput>>,
        results: Vec<Option<Slot>>,
    ) -> (Arc<BatchShared>, MobPlanSet) {
        let players = Arc::new(Vec::new());
        let batch = Arc::new(BatchShared {
            jobs,
            results: Mutex::new(results),
            done_cv: Condvar::new(),
            players: players.clone(),
            dim: DimId::Overworld,
            day: true,
            hostile: true,
        });
        let set = MobPlanSet {
            batch: Some(batch.clone()),
            len: batch.jobs.len(),
            epoch: CHUNK_VERSION_SEQ.load(Ordering::Relaxed),
            gen_seq: crate::current_gen_seq(),
            players,
            stalled: false,
        };
        (batch, set)
    }

    #[test]
    fn env_probe_reuses_only_at_probe_pos_and_fresh_epoch() {
        let env = stub_env();
        let pos = env.probe_pos;
        assert!(env.submerged_or(true, pos, || panic!("must reuse")));
        assert!(env.cell_solid_or(true, pos, || panic!("must reuse")));
        // A moved mob (travel) must recompute inline, not trust the probe.
        let other = (pos.0 + 0.5, pos.1, pos.2);
        assert!(!env.submerged_or(true, other, || false));
        assert!(!env.cell_solid_or(true, other, || false));
        // A stale epoch must recompute too — the probed value no longer
        // describes the block a live read would see.
        assert!(!env.submerged_or(false, pos, || false));
        assert!(!env.cell_solid_or(false, pos, || false));
        let mut recomputed = false;
        let _ = env.submerged_or(false, pos, || {
            recomputed = true;
            false
        });
        assert!(recomputed);
        assert!(env.sky_visible_or(true, 63, || panic!("must reuse")));
        // Stale epoch: the probed `true` must not stand in — compute decides.
        assert!(!env.sky_visible_or(false, 63, || false));
    }

    #[test]
    fn results_resolve_in_slot_order_regardless_of_fill_order() {
        let pos_a = (1.0, 65.0, 1.0);
        let pos_b = (2.0, 65.0, 2.0);
        let pos_c = (3.0, 65.0, 3.0);
        let (batch, mut set) = make_set(
            vec![None, None, None],
            vec![None, None, None],
        );
        // A writer fills the slots out of order, like racing workers would;
        // `resolve` still returns each slot in order once filled. (Going
        // through `resolve`, not `take_valid`: the epoch gate is covered by
        // its own tests and a concurrent test's world writes must not make
        // this ordering property flaky.)
        let writer = batch.clone();
        std::thread::spawn(move || {
            for (idx, pos) in [(2usize, pos_c), (0, pos_a), (1, pos_b)] {
                std::thread::sleep(Duration::from_millis(10));
                writer.results.lock().unwrap_or_else(|e| e.into_inner())[idx] =
                    Some(Slot::Done(stub_plan(pos)));
                writer.done_cv.notify_all();
            }
        });
        assert_eq!(set.resolve(0).map(|p| p.pos), Some(pos_a));
        assert_eq!(set.resolve(1).map(|p| p.pos), Some(pos_b));
        assert_eq!(set.resolve(2).map(|p| p.pos), Some(pos_c));
    }

    #[test]
    fn err_slot_falls_back_and_counts_failure() {
        let before = TICKPAR_FAILED.load(Ordering::Relaxed);
        let (_batch, mut set) = make_set(vec![None], vec![Some(Slot::Failed)]);
        assert!(set.take_valid(0, (0.0, 0.0, 0.0)).is_none());
        assert_eq!(TICKPAR_FAILED.load(Ordering::Relaxed), before + 1);
    }

    #[test]
    fn skipped_slot_falls_back_without_counting_failure() {
        let before = TICKPAR_FAILED.load(Ordering::Relaxed);
        let (_batch, mut set) = make_set(vec![None], vec![Some(Slot::Skipped)]);
        assert!(set.take_valid(0, (0.0, 0.0, 0.0)).is_none());
        assert_eq!(TICKPAR_FAILED.load(Ordering::Relaxed), before);
    }

    #[test]
    fn moved_mob_stales_the_plan() {
        let before = TICKPAR_STALE.load(Ordering::Relaxed);
        let plan_pos = (1.0, 65.0, 1.0);
        let (_batch, mut set) =
            make_set(vec![None], vec![Some(Slot::Done(stub_plan(plan_pos)))]);
        // The mob moved between dispatch and commit (pet teleport, knockback):
        // the plan no longer describes it and must not be consumed.
        assert!(set.take_valid(0, (9.0, 65.0, 9.0)).is_none());
        assert_eq!(TICKPAR_STALE.load(Ordering::Relaxed), before + 1);
    }

    #[test]
    fn block_write_stales_every_plan_in_the_batch() {
        let plan_pos = (1.0, 65.0, 1.0);
        let (_batch, mut set) =
            make_set(vec![None], vec![Some(Slot::Done(stub_plan(plan_pos)))]);
        // A `set_block` (or generation write) after dispatch bumps the epoch;
        // the plan's world reads no longer describe the world serial would see.
        // Simulated by rewinding the batch's recorded epoch — touching the
        // real global would race other tests' in-flight batches.
        set.epoch = set.epoch.wrapping_sub(1);
        assert!(set.take_valid(0, plan_pos).is_none());
    }

    #[test]
    fn lost_slot_stalls_the_batch_not_the_tick() {
        // A slot that never resolves must not hang the commit: after the
        // timeout the whole batch is marked dead and later slots resolve
        // instantly as fallbacks.
        let (_batch, mut set) = make_set(vec![None, None], vec![None, None]);
        let started = Instant::now();
        assert!(set.take_valid(0, (0.0, 0.0, 0.0)).is_none());
        assert!(started.elapsed() >= Duration::from_millis(200));
        assert!(set.stalled);
        let again = Instant::now();
        assert!(set.take_valid(1, (0.0, 0.0, 0.0)).is_none());
        assert!(again.elapsed() < Duration::from_millis(50));
    }

    #[test]
    fn serial_mode_resolves_nothing() {
        let mut set = MobPlanSet {
            batch: None,
            len: 4,
            epoch: CHUNK_VERSION_SEQ.load(Ordering::Relaxed),
            gen_seq: crate::current_gen_seq(),
            players: Arc::new(Vec::new()),
            stalled: false,
        };
        for i in 0..4 {
            assert!(set.take_valid(i, (0.0, 0.0, 0.0)).is_none());
        }
    }

    #[test]
    fn worker_survives_a_panicking_job() {
        assert!(pool().ensure_spawned());
        let ran = Arc::new(AtomicUsize::new(0));
        for _ in 0..4 {
            pool().submit(Work::Test(Box::new(|| panic!("boom"))));
        }
        for _ in 0..4 {
            let ran = ran.clone();
            pool().submit(Work::Test(Box::new(move || {
                ran.fetch_add(1, Ordering::Relaxed);
            })));
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while ran.load(Ordering::Relaxed) < 4 {
            assert!(Instant::now() < deadline, "workers did not survive panic");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn every_submitted_job_runs_exactly_once() {
        assert!(pool().ensure_spawned());
        let ran = Arc::new(AtomicUsize::new(0));
        for _ in 0..32 {
            let ran = ran.clone();
            pool().submit(Work::Test(Box::new(move || {
                ran.fetch_add(1, Ordering::Relaxed);
            })));
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while ran.load(Ordering::Relaxed) < 32 {
            assert!(Instant::now() < deadline, "jobs lost or duplicated");
            std::thread::sleep(Duration::from_millis(5));
        }
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(ran.load(Ordering::Relaxed), 32);
    }

    #[test]
    fn kill_switch_makes_every_slot_inline() {
        set_parallel_override(Some(false));
        let mobs: Vec<Arc<Mob>> = Vec::new();
        let players: Vec<LiveMobPlayer<'_>> = Vec::new();
        let mut set = mob_plans(
            DimId::Overworld,
            &mobs,
            &players,
            &[],
            &[],
            true,
            true,
            32,
        );
        assert!(set.batch.is_none());
        assert!(set.take_valid(0, (0.0, 0.0, 0.0)).is_none());
        set_parallel_override(None);
    }

    fn plan_mob(eid: i32, x: f64) -> Arc<Mob> {
        Arc::new(Mob {
            dim: DimId::Overworld,
            eid,
            uuid: [eid as u8; 16],
            // Zombie: a spec'd hostile with no pet, tempting, or spider branches,
            // so every plan field is a pure function of the shared inputs.
            etype: 124,
            pos: Mutex::new((x, 65.0, x, 0.0, 0.0)),
            hp: Mutex::new(20.0),
            last_hit: Mutex::new(Instant::now() - Duration::from_secs(60)),
            hurt_at: Mutex::new(Instant::now() - Duration::from_secs(60)),
            last_hurt_tick: Mutex::new(None),
            vel: Mutex::new([0.0; 3]),
            fuse: Mutex::new(0),
            fire_ticks: Mutex::new(0),
            love: Mutex::new(0),
            air: Mutex::new(300),
        })
    }

    fn perception_eq(a: &AiEntityPerception, b: &AiEntityPerception) -> bool {
        a.entity_id == b.entity_id
            && a.position == b.position
            && a.width == b.width
            && a.line_of_sight == b.line_of_sight
            && a.held_item == b.held_item
            && a.in_love == b.in_love
            && a.compatible == b.compatible
    }

    fn targeting_eq(a: &MobTargeting, b: &MobTargeting) -> bool {
        a.pos == b.pos
            && a.provoked == b.provoked
            && a.nearest == b.nearest
            && a.target == b.target
            && a.look == b.look
            && a.spectators == b.spectators
            && a.stare == b.stare
            && match (&a.perception, &b.perception) {
                (Some(x), Some(y)) => perception_eq(x, y),
                (None, None) => true,
                _ => false,
            }
    }

    fn plans_equal(a: &MobPlan, b: &MobPlan) -> bool {
        a.pos == b.pos
            && a.env.probe_pos == b.env.probe_pos
            && a.env.feet_state == b.env.feet_state
            && a.env.in_water == b.env.in_water
            && a.env.in_lava == b.env.in_lava
            && a.env.submerged == b.env.submerged
            && a.env.cell_solid == b.env.cell_solid
            && a.env.above_state == b.env.above_state
            && a.env.sky_height == b.env.sky_height
            && a.env.sky_air2 == b.env.sky_air2
            && a.env.below_block == b.env.below_block
            && match (&a.pet, &b.pet) {
                (Some(x), Some(y)) => {
                    x.state == y.state && x.teleport == y.teleport
                }
                (None, None) => true,
                _ => false,
            }
            && match (&a.targeting, &b.targeting) {
                (Some(x), Some(y)) => targeting_eq(x, y),
                (None, None) => true,
                _ => false,
            }
    }

    /// Chunked range dispatch must produce bit-identical plans to the old
    /// one-task-per-mob dispatch: same mobs, same gates, every chunk boundary
    /// (1, 31, 32, 33) and a 200-mob run.
    #[test]
    fn batched_ranges_preserve_bit_identical_plans() {
        crate::test_support::require_vanilla_data();
        // The world read by `env_probe` must not move mid-comparison; the
        // guard also serializes against the serial/parallel fixture's
        // `set_parallel_override` window in tick_tests.
        let _guard = crate::world_guard();
        set_parallel_override(Some(true));
        let run = |n: usize, batch: Option<usize>| -> Vec<Option<MobPlan>> {
            set_mob_batch_override(batch);
            let mobs: Vec<Arc<Mob>> = (0..n)
                .map(|i| plan_mob(10_000 + i as i32, i as f64 * 0.5))
                .collect();
            let dead = vec![false; n];
            let hot = vec![true; n];
            let mut set = mob_plans(
                DimId::Overworld,
                &mobs,
                &[],
                &dead,
                &hot,
                true,
                true,
                i32::MAX,
            );
            let mut out = Vec::with_capacity(n);
            for i in 0..n {
                out.push(set.resolve(i));
            }
            out
        };
        // A serial-mode interloper (another test's override) resolves every
        // slot to None; retry until the pool actually produced plans.
        let attempt = |n: usize, batch: Option<usize>| -> Vec<Option<MobPlan>> {
            for _ in 0..8 {
                let out = run(n, batch);
                if out.iter().all(Option::is_some) {
                    return out;
                }
            }
            panic!("worker pool produced no plans for {n} mobs");
        };
        for n in [1usize, 31, 32, 33, 200] {
            let reference = attempt(n, Some(1));
            for batch in [Some(31), Some(32), Some(33), None] {
                let got = attempt(n, batch);
                for (i, (want, have)) in reference.iter().zip(got.iter()).enumerate() {
                    let (Some(want), Some(have)) = (want, have) else {
                        panic!("mob {i} of {n} (batch {batch:?}) lost its plan");
                    };
                    assert!(
                        plans_equal(want, have),
                        "mob {i} of {n} (batch {batch:?}) plan diverged"
                    );
                }
            }
        }
        set_mob_batch_override(None);
        set_parallel_override(None);
    }

    #[test]
    fn targeting_inputs_live_replays_recorded_reads() {
        let (client, _peer) = crate::effect_tests::client_pair();
        let client = Arc::new(client);
        let players = vec![TargetPlayer {
            eid: client.eid,
            pos: (0.0, 65.0, 0.0),
            dim: DimId::Overworld,
            held_item: None,
            attackable: true,
            client: client.clone(),
        }];
        let mut t = MobTargeting {
            pos: (1.0, 65.0, 1.0),
            provoked: false,
            nearest: None,
            target: None,
            perception: None,
            look: None,
            spectators: vec![(0, client.is_spectator())],
            stare: vec![StareRead {
                player: 0,
                pos5: *client.pos.lock().unwrap_or_else(|e| e.into_inner()),
                helmet: None,
            }],
        };
        assert!(t.inputs_live(&players));
        // A spectator-flag flip invalidates the recorded reads.
        *client.gm.lock().unwrap_or_else(|e| e.into_inner()) = 3;
        assert!(!t.inputs_live(&players));
        *client.gm.lock().unwrap_or_else(|e| e.into_inner()) = 0;
        assert!(t.inputs_live(&players));
        // A moved player (stare pose read) invalidates too.
        let mut pos = *client.pos.lock().unwrap_or_else(|e| e.into_inner());
        pos.0 += 8.0;
        *client.pos.lock().unwrap_or_else(|e| e.into_inner()) = pos;
        assert!(!t.inputs_live(&players));
        pos.0 -= 8.0;
        *client.pos.lock().unwrap_or_else(|e| e.into_inner()) = pos;
        assert!(t.inputs_live(&players));
        // A helmet-slot change invalidates only when it was recorded.
        t.stare[0].helmet = Some(String::new());
        assert!(t.inputs_live(&players));
        client
            .inv
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(5, (crate::name_to_item("minecraft:carved_pumpkin").unwrap(), 1, Vec::new()));
        assert!(!t.inputs_live(&players));
    }
}
