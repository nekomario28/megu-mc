// SPDX-License-Identifier: LGPL-3.0-or-later
#![forbid(unsafe_code)]
//! `megu_core::anticheat_v2` — next-gen competitive-survival anti-cheat
//! primitives, built on the crate's bounded CS containers.
//!
//! Five components, all allocation-bounded and timestamp-driven (the caller
//! supplies tick counters / millisecond clocks, so the engine is
//! deterministic and testable):
//!
//! * [`TransactionEchoQueue`] — in-order server-transaction tracking
//!   (velocity grants, teleports, effect changes). Only a client *echo* of
//!   the matching wire id confirms the transaction and folds the true RTT
//!   into an EWMA+jitter estimate, so physics state transitions can wait
//!   for proof the client saw the packet instead of rubber-banding lag.
//! * [`HitboxRewindBuffer`] — a bounded ring of `(tick, Aabb3)` snapshots
//!   with [`HitboxRewindBuffer::sample_at_tick`] interpolation and a
//!   NaN-safe slab ray test for lag-compensated Reach/Hitbox/KillAura
//!   validation.
//! * [`PhysicsPredictionStep`] + [`AcModGrantToken`] — the deterministic
//!   one-tick Vanilla 1.21.1 motion envelope (gravity 0.08, drag 0.98,
//!   slipperiness 0.6/0.98/0.989, fluids, climbables, elytra, effects)
//!   plus a token mods use to declare legitimate external velocity
//!   (Create contraptions, airships) instead of disabling checks.
//! * [`BehavioralCusumDetector`] — Page's CUSUM over Welford EWMA
//!   statistics: machine-regular click cadence (AutoClicker), angular
//!   snap/GCD patterns (KillAura), yaw-locked placement timing (Scaffold)
//!   and metronomic digging (FastBreak), all O(1) memory.
//! * [`ProtocolInvariantFsm`] — O(1) packet-order and action invariants
//!   the vanilla client can never produce (dig FINISH without START when
//!   the block could not insta-mine, sprinting while using an item,
//!   negative/regressing sequence ids, out-of-range face enums).

use crate::bvh::Aabb3;
use crate::ring_spsc::BoundedRing;

// ---------------------------------------------------------------------------
// TransactionEchoQueue
// ---------------------------------------------------------------------------

/// What a pending server transaction carries once the client confirms it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxKind {
    /// `SetEntityMotion` velocity grant (knockback, launches, mod pushes).
    VelocityGrant,
    /// Position/rotation teleport the client must confirm.
    Teleport,
    /// Entity-effect apply/remove that changes movement-relevant state.
    EffectChange,
}

#[derive(Clone, Copy, Debug)]
struct PendingTx {
    /// Monotonic internal sequence; an echo drains everything at or below it.
    seq: u64,
    /// The wire id the client echoes (teleport confirm id, sequence ack).
    wire: u64,
    kind: TxKind,
    issued_ms: u64,
}

/// EWMA round-trip estimate fed exclusively by client-confirmed echoes.
/// `samples == 0` means "no measurement yet" — callers must treat that as
/// quiet, never as zero latency.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RttEstimate {
    /// Smoothed round-trip time, milliseconds.
    pub rtt_ms: f64,
    /// Smoothed mean deviation (RFC 6298 `RTTVAR`), milliseconds.
    pub jitter_ms: f64,
    /// Echo samples folded into the estimate.
    pub samples: u64,
}

/// Bounded in-order transaction queue. TCP semantics: when the client
/// echoes wire id `w`, every pending transaction issued at or before it has
/// provably been seen — the queue drains them and the matched entry's age
/// becomes one confirmed RTT sample.
pub struct TransactionEchoQueue<const N: usize> {
    pending: BoundedRing<PendingTx>,
    next_seq: u64,
    next_wire: u64,
    rtt: RttEstimate,
}

impl<const N: usize> Default for TransactionEchoQueue<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> std::fmt::Debug for TransactionEchoQueue<N> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransactionEchoQueue")
            .field("pending", &self.pending.len())
            .field("rtt", &self.rtt)
            .finish()
    }
}

impl<const N: usize> TransactionEchoQueue<N> {
    /// Empty queue with capacity `N`.
    pub fn new() -> Self {
        Self {
            pending: BoundedRing::with_capacity(N.max(1)),
            next_seq: 0,
            next_wire: 0,
            rtt: RttEstimate::default(),
        }
    }

    /// Issue a transaction and get its wire id. Internally-issued ids carry
    /// the high bit so they can never collide with a real protocol wire id
    /// (teleport confirms and sequence ids are non-negative `i32`s) — an
    /// echo only ever confirms what actually went on the wire, while the
    /// in-order drain still ages out pending grants.
    pub fn issue(&mut self, kind: TxKind, now_ms: u64) -> u64 {
        self.next_wire = self.next_wire.wrapping_add(1);
        let wire = self.next_wire | (1 << 63);
        self.push(PendingTx {
            seq: self.next_seq,
            wire,
            kind,
            issued_ms: now_ms,
        });
        self.next_seq += 1;
        wire
    }

    /// Record a transaction whose wire id was assigned elsewhere
    /// (the teleport-confirm counter, a sequence number).
    pub fn note_grant(&mut self, wire_id: u64, kind: TxKind, now_ms: u64) {
        self.push(PendingTx {
            seq: self.next_seq,
            wire: wire_id,
            kind,
            issued_ms: now_ms,
        });
        self.next_seq += 1;
    }

    fn push(&mut self, tx: PendingTx) {
        self.pending.push_overwrite_oldest(tx);
    }

    /// The client echoed `wire_id`. In-order transport means everything
    /// issued at or before the matched entry is confirmed; returns the
    /// confirmed round-trip in milliseconds when an echo matched.
    pub fn note_echo(&mut self, wire_id: u64, now_ms: u64) -> Option<u64> {
        let matched_seq = self
            .pending
            .iter()
            .find(|p| p.wire == wire_id)
            .map(|p| p.seq)?;
        let mut age = None;
        while let Some(front) = self.pending.front() {
            if front.seq > matched_seq {
                break;
            }
            if front.seq == matched_seq {
                age = Some(now_ms.saturating_sub(front.issued_ms));
            }
            self.pending.pop_front();
        }
        let sample = age?;
        // RFC 6298: RTTVAR <- (1-beta)|SRTT - R'| , SRTT <- (1-alpha)SRTT + alpha R'.
        const ALPHA: f64 = 0.125;
        let dev = if self.rtt.samples == 0 {
            sample as f64
        } else {
            (sample as f64 - self.rtt.rtt_ms).abs()
        };
        if self.rtt.samples == 0 {
            self.rtt.rtt_ms = sample as f64;
            self.rtt.jitter_ms = dev;
        } else {
            self.rtt.jitter_ms += ALPHA * (dev - self.rtt.jitter_ms);
            self.rtt.rtt_ms += ALPHA * (sample as f64 - self.rtt.rtt_ms);
        }
        self.rtt.samples += 1;
        Some(sample)
    }

    /// Current confirmed-RTT estimate (empty until the first echo).
    pub fn rtt(&self) -> RttEstimate {
        self.rtt
    }

    /// Pending transactions the client has not yet confirmed.
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// Whether a transaction of `kind` is still unconfirmed — e.g. a
    /// velocity grant whose effect the client could not have seen yet.
    pub fn has_unconfirmed(&self, kind: TxKind) -> bool {
        self.pending.iter().any(|p| p.kind == kind)
    }

    /// Evict transactions older than `ttl_ms` without an echo: an
    /// unanswerable grant must not pin physics leniency forever.
    pub fn expire(&mut self, now_ms: u64, ttl_ms: u64) {
        while self
            .pending
            .front()
            .is_some_and(|p| now_ms.saturating_sub(p.issued_ms) > ttl_ms)
        {
            self.pending.pop_front();
        }
    }
}

// ---------------------------------------------------------------------------
// HitboxRewindBuffer
// ---------------------------------------------------------------------------

/// One recorded `(tick, aabb)` snapshot.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HitboxSample {
    /// Server tick the box is authoritative for.
    pub tick: u64,
    /// World-space bounding box at that tick.
    pub aabb: Aabb3,
}

/// NaN-safe slab ray test against `aabb`: `ray = eye + dir * t` with `dir`
/// arbitrary length (it is normalized internally); `t` measures world
/// distance. Returns the hit distance when the ray lands inside `max_reach`.
pub fn ray_aabb_hit(eye: [f64; 3], dir: [f64; 3], aabb: &Aabb3, max_reach: f64) -> Option<f32> {
    if !eye.iter().all(|v| v.is_finite())
        || !dir.iter().all(|v| v.is_finite())
        || !max_reach.is_finite()
        || max_reach < 0.0
    {
        return None;
    }
    let len = (dir[0] * dir[0] + dir[1] * dir[1] + dir[2] * dir[2]).sqrt();
    if !len.is_finite() || len <= f64::EPSILON {
        return None;
    }
    // inv_dir components for the slab test; `len / d` maps t onto blocks.
    let inv_dir = [
        (len / dir[0]) as f32,
        (len / dir[1]) as f32,
        (len / dir[2]) as f32,
    ];
    let origin = [eye[0] as f32, eye[1] as f32, eye[2] as f32];
    aabb.ray_intersects(origin, inv_dir, max_reach as f32)
}

/// Timestamped hitbox history for lag-compensated combat validation.
/// Recording is `O(1)` with zero steady-state allocation; sampling
/// interpolates between the bracketing snapshots.
pub struct HitboxRewindBuffer<const N: usize> {
    samples: BoundedRing<HitboxSample>,
}

impl<const N: usize> Default for HitboxRewindBuffer<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> std::fmt::Debug for HitboxRewindBuffer<N> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HitboxRewindBuffer")
            .field("samples", &self.samples.len())
            .finish()
    }
}

impl<const N: usize> HitboxRewindBuffer<N> {
    /// Empty history holding the newest `N` ticks.
    pub fn new() -> Self {
        Self {
            samples: BoundedRing::with_capacity(N.max(1)),
        }
    }

    /// Record the box for `tick`. Same-tick re-records replace; out-of-order
    /// ticks are dropped so iteration stays monotonic.
    pub fn record(&mut self, tick: u64, aabb: Aabb3) {
        if let Some(back) = self.samples.back_mut() {
            if back.tick == tick {
                back.aabb = aabb;
                return;
            }
            if back.tick > tick {
                return;
            }
        }
        self.samples.push_overwrite_oldest(HitboxSample { tick, aabb });
    }

    /// Newest recorded snapshot.
    pub fn latest(&self) -> Option<HitboxSample> {
        self.samples.back().copied()
    }

    /// Number of stored snapshots.
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// Whether the history is empty.
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// The box at `target_tick`, linearly interpolated between the
    /// bracketing snapshots; outside the window the nearest recorded box is
    /// returned clamped (no extrapolation).
    pub fn sample_at_tick(&self, target_tick: u64) -> Option<Aabb3> {
        let mut prev: Option<HitboxSample> = None;
        for &s in self.samples.iter() {
            if s.tick >= target_tick {
                return Some(match prev {
                    None => s.aabb,
                    Some(p) => {
                        let span = s.tick - p.tick;
                        let t = if span == 0 {
                            0.0
                        } else {
                            (target_tick - p.tick) as f32 / span as f32
                        };
                        lerp_aabb(&p.aabb, &s.aabb, t)
                    }
                });
            }
            prev = Some(s);
        }
        prev.map(|p| p.aabb)
    }

    /// Lag-compensated reach validation: interpolate the box at
    /// `target_tick`, fatten it by `latency_margin`, and NaN-safely test the
    /// ray for a hit inside `max_reach`. `Some(distance)` is a confirmed
    /// reach; `None` means the hit could not have landed — including when
    /// the history is empty or the input is degenerate.
    pub fn verify_ray_reach(
        &self,
        eye: [f64; 3],
        dir: [f64; 3],
        max_reach: f64,
        target_tick: u64,
        latency_margin: f32,
    ) -> Option<f32> {
        if !latency_margin.is_finite() {
            return None;
        }
        let aabb = self.sample_at_tick(target_tick)?.expanded(latency_margin);
        ray_aabb_hit(eye, dir, &aabb, max_reach)
    }
}

fn lerp_aabb(a: &Aabb3, b: &Aabb3, t: f32) -> Aabb3 {
    let lerp = |x: f32, y: f32| x + (y - x) * t;
    Aabb3 {
        min: [
            lerp(a.min[0], b.min[0]),
            lerp(a.min[1], b.min[1]),
            lerp(a.min[2], b.min[2]),
        ],
        max: [
            lerp(a.max[0], b.max[0]),
            lerp(a.max[1], b.max[1]),
            lerp(a.max[2], b.max[2]),
        ],
    }
}

// ---------------------------------------------------------------------------
// PhysicsPredictionStep & AcModGrantToken
// ---------------------------------------------------------------------------

/// Vanilla gravity, blocks/tick² (`generic.gravity` default).
pub const VANILLA_GRAVITY: f64 = 0.08;
/// Vanilla air drag retained per tick.
pub const VANILLA_DRAG: f64 = 0.98;
/// Ground friction of ordinary blocks (`blockFriction`).
pub const SLIP_GROUND: f64 = 0.6;
/// Ice / packed ice / frosted ice slipperiness.
pub const SLIP_ICE: f64 = 0.98;
/// Blue ice slipperiness.
pub const SLIP_BLUE_ICE: f64 = 0.989;
/// `generic.jump_strength` default.
pub const VANILLA_JUMP: f64 = 0.42;
/// `generic.movement_speed` default.
pub const VANILLA_WALK: f64 = 0.1;
/// Sprint-jumping on flat ground at default speed, blocks/tick.
const SPRINT_JUMP: f64 = 0.357;
/// Ice-family sprint multiplier under a ceiling.
const ICE_SPRINT_MUL: f64 = 3.5;
/// Envelope slack: float noise, stairs, and attribute rounding.
const ENV_SLACK_MUL: f64 = 1.3;
const ENV_SLACK_ADD: f64 = 0.1;
/// Fastest sustainable swim before Dolphin's Grace/Depth Strider extremes.
const SWIM_CAP: f64 = 1.6;
/// Elytra / riptide travel bound.
const LAUNCHED: f64 = 4.0;
/// Creative-fly and spectator-fly per-tick bounds.
const FLY_CAP: f64 = 6.0;

/// Every input the one-tick envelope needs, gathered once per packet.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PhysicsEnv {
    /// `generic.gravity` (0.08 vanilla).
    pub gravity: f64,
    /// `generic.jump_strength + 0.1 * jump_boost_level`.
    pub jump_velocity: f64,
    /// `generic.movement_speed` (0.1 vanilla).
    pub movement_speed: f64,
    /// Speed effect level (0 = none).
    pub speed_level: i32,
    /// Slipperiness of the supporting block: `SLIP_GROUND`/`SLIP_ICE`/`SLIP_BLUE_ICE`.
    pub friction: f64,
    /// Claimed or server-derived on-ground.
    pub on_ground: bool,
    /// Feet/eye in water.
    pub in_water: bool,
    /// In lava (same push family as water, different drag).
    pub in_lava: bool,
    /// On a ladder/vine/scaffolding column.
    pub on_climbable: bool,
    /// Elytra glide.
    pub fall_flying: bool,
    /// Levitation effect level.
    pub levitation_level: i32,
    /// Creative or spectator flight.
    pub flying: bool,
}

impl Default for PhysicsEnv {
    /// Vanilla standing on ordinary ground: the strictest honest envelope.
    fn default() -> Self {
        Self {
            gravity: VANILLA_GRAVITY,
            jump_velocity: VANILLA_JUMP,
            movement_speed: VANILLA_WALK,
            speed_level: 0,
            friction: SLIP_GROUND,
            on_ground: true,
            in_water: false,
            in_lava: false,
            on_climbable: false,
            fall_flying: false,
            levitation_level: 0,
            flying: false,
        }
    }
}

/// A mod's declaration that it applied external motion this tick: Create
/// contraption carry, an airship hull, an elevator platform. The token
/// widens the envelope by exactly the declared velocity for `ttl_ticks`
/// instead of blanket-disabling the check.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AcModGrantToken {
    /// Blocks/tick the mod asserts it applied: `[dx, dy, dz]`.
    pub velocity: [f64; 3],
    /// Ticks the declaration stays valid.
    pub ttl_ticks: u32,
}

impl AcModGrantToken {
    /// Grant `velocity` for `ttl_ticks` server ticks.
    pub fn new(velocity: [f64; 3], ttl_ticks: u32) -> Self {
        Self {
            velocity,
            ttl_ticks,
        }
    }

    /// Whether the grant has run out.
    pub fn expired(&self) -> bool {
        self.ttl_ticks == 0
    }

    /// Advance one tick.
    pub fn tick_down(&mut self) {
        self.ttl_ticks = self.ttl_ticks.saturating_sub(1);
    }
}

/// The judgement a [`PhysicsPredictionStep::evaluate`] produces.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PredictionVerdict {
    /// The legal one-tick envelope `[horizontal, up, down]` in blocks/tick
    /// the environment (plus any grant) permits.
    pub envelope: [f64; 3],
    /// `observed - envelope` clamped at zero, one slot per axis.
    pub excess_residual: [f64; 3],
    /// Whether the observed delta fit the envelope.
    pub within: bool,
}

/// Deterministic Vanilla 1.21.1 one-tick motion predictor: given the
/// environment, compute the largest legal `[horizontal, up, down]`
/// displacement a single tick can produce.
#[derive(Clone, Copy, Debug, Default)]
pub struct PhysicsPredictionStep;

impl PhysicsPredictionStep {
    /// The one-tick motion envelope for `env`. Never below the vanilla
    /// default: weakened attributes must not convict an honest player.
    pub fn envelope(env: &PhysicsEnv) -> [f64; 3] {
        let speed_factor = (env.movement_speed / VANILLA_WALK).max(1.0)
            * (1.0 + 0.2 * f64::from(env.speed_level.max(0)));
        let mut h = SPRINT_JUMP * speed_factor * ENV_SLACK_MUL + ENV_SLACK_ADD;
        if env.friction >= SLIP_ICE {
            h *= ICE_SPRINT_MUL;
        }
        if env.in_water || env.in_lava {
            h = h.max(SWIM_CAP * speed_factor);
        }
        let mut up = (env.jump_velocity.max(VANILLA_JUMP) + ENV_SLACK_ADD) * ENV_SLACK_MUL;
        if env.levitation_level > 0 {
            up = up.max(0.1 * f64::from(env.levitation_level) + 0.5);
        }
        if env.in_water || env.in_lava {
            up = up.max(1.0);
        }
        let mut down = (env.gravity.max(VANILLA_GRAVITY) / (1.0 - VANILLA_DRAG)) * 1.05;
        if env.on_climbable {
            up = up.max(0.3);
            down = down.max(0.25);
        }
        if env.fall_flying {
            h = h.max(LAUNCHED);
            up = up.max(LAUNCHED);
            down = down.max(LAUNCHED);
        }
        if env.flying {
            h = h.max(FLY_CAP);
            up = up.max(FLY_CAP);
            down = down.max(FLY_CAP);
        }
        [h, up, down]
    }

    /// Compare `observed = [horizontal, up, down]` against the envelope,
    /// honoring a live [`AcModGrantToken`] (expired tokens widen nothing).
    pub fn evaluate(
        env: &PhysicsEnv,
        observed: [f64; 3],
        grant: Option<&AcModGrantToken>,
    ) -> PredictionVerdict {
        let mut envelope = Self::envelope(env);
        if let Some(g) = grant.filter(|g| !g.expired()) {
            envelope[0] += g.velocity[0].hypot(g.velocity[2]);
            envelope[1] += g.velocity[1].max(0.0);
            envelope[2] += (-g.velocity[1]).max(0.0);
        }
        let residual = [
            (observed[0] - envelope[0]).max(0.0),
            (observed[1] - envelope[1]).max(0.0),
            (observed[2] - envelope[2]).max(0.0),
        ];
        PredictionVerdict {
            envelope,
            excess_residual: residual,
            within: residual.iter().all(|r| *r <= f64::EPSILON),
        }
    }
}

// ---------------------------------------------------------------------------
// BehavioralCusumDetector
// ---------------------------------------------------------------------------

/// Which behavioural channel produced an alarm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BehavioralFlag {
    /// Click cadence too machine-regular (AutoClicker).
    AutoClicker,
    /// Angular snap / mouse-GCD pattern (KillAura).
    KillAura,
    /// Placement cadence + yaw lock (Scaffold).
    Scaffold,
    /// Dig cadence too metronomic (FastBreak).
    FastBreak,
}

/// Welford mean/variance plus Page's one-sided CUSUM on the *regularity
/// deficit*: an inhumanly stable stream (low coefficient of variation)
/// accumulates evidence, ordinary human jitter bleeds the score back down.
/// O(1) memory — no sample history is kept.
#[derive(Clone, Copy, Debug)]
struct RegularityCusum {
    n: u32,
    mean: f64,
    m2: f64,
    score: f64,
    warmup: u32,
    human_cv: f64,
    relax: f64,
    alarm: f64,
}

impl RegularityCusum {
    const fn new(warmup: u32, human_cv: f64, relax: f64, alarm: f64) -> Self {
        Self {
            n: 0,
            mean: 0.0,
            m2: 0.0,
            score: 0.0,
            warmup,
            human_cv,
            relax,
            alarm,
        }
    }

    fn observe(&mut self, x: f64) {
        if !x.is_finite() || x <= 0.0 {
            return;
        }
        self.n += 1;
        let d = x - self.mean;
        self.mean += d / f64::from(self.n);
        self.m2 += d * (x - self.mean);
        if self.n < self.warmup {
            return;
        }
        let var = self.m2 / f64::from(self.n - 1);
        let cv = var.sqrt() / self.mean.abs().max(1e-9);
        let deficit = self.human_cv - cv;
        if deficit > 0.0 {
            self.score += deficit;
        } else {
            self.score = (self.score - self.relax).max(0.0);
        }
    }

    fn alarmed(&self) -> bool {
        self.score >= self.alarm
    }

    fn score(&self) -> f64 {
        self.score
    }
}

/// Sample counts before a stream may be judged — a detector must see a
/// human variance baseline before it can call one machine-regular.
const WARMUP: u32 = 12;
/// Coefficient-of-variation floor for human click/dig intervals (~8 %).
const HUMAN_CV: f64 = 0.08;
/// Score bleed per clean sample.
const CUSUM_RELAX: f64 = 0.02;
/// Score that reports an alarm (~10 fully machine-regular samples).
const CUSUM_ALARM: f64 = 0.6;
/// Yaw deltas below this between consecutive placements count as "locked".
const YAW_LOCK_DEG: f32 = 0.5;
/// Locked-yaw placements in a row that scream Scaffold.
const YAW_LOCK_RUN: u32 = 6;
/// A yaw snap above this between attacks is KillAura-scale.
const KA_SNAP_DEG: f64 = 120.0;
/// Snaps above `KA_SNAP_DEG` in a row that report.
const KA_SNAP_RUN: u32 = 4;

/// Multi-channel behavioural detector: one [`RegularityCusum`] per timed
/// stream plus the yaw-lock / snap-run counters, all O(1) memory. Each
/// `note_*` returns `Some(flag)` the moment its channel crosses the alarm
/// threshold; human variance alone can never get there.
#[derive(Clone, Debug)]
pub struct BehavioralCusumDetector {
    click: RegularityCusum,
    yaw: RegularityCusum,
    place: RegularityCusum,
    dig: RegularityCusum,
    last_place_yaw: Option<f32>,
    yaw_lock_run: u32,
    snap_run: u32,
}

impl Default for BehavioralCusumDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl BehavioralCusumDetector {
    /// Fresh detector.
    pub fn new() -> Self {
        Self {
            click: RegularityCusum::new(WARMUP, HUMAN_CV, CUSUM_RELAX, CUSUM_ALARM),
            yaw: RegularityCusum::new(WARMUP, 0.06, CUSUM_RELAX, CUSUM_ALARM),
            place: RegularityCusum::new(WARMUP, HUMAN_CV, CUSUM_RELAX, CUSUM_ALARM),
            dig: RegularityCusum::new(WARMUP, HUMAN_CV, CUSUM_RELAX, CUSUM_ALARM),
            last_place_yaw: None,
            yaw_lock_run: 0,
            snap_run: 0,
        }
    }

    /// One attack/click interval in seconds.
    pub fn note_click_interval(&mut self, dt_secs: f64) -> Option<BehavioralFlag> {
        self.click.observe(dt_secs);
        self.click.alarmed().then_some(BehavioralFlag::AutoClicker)
    }

    /// One yaw change between consecutive attacks, in degrees.
    pub fn note_yaw_delta(&mut self, delta_deg: f64) -> Option<BehavioralFlag> {
        if !delta_deg.is_finite() || delta_deg < 0.0 {
            return None;
        }
        self.yaw.observe(delta_deg.max(1e-3));
        self.snap_run = if delta_deg > KA_SNAP_DEG {
            self.snap_run + 1
        } else {
            0
        };
        if self.snap_run >= KA_SNAP_RUN || self.yaw.alarmed() {
            return Some(BehavioralFlag::KillAura);
        }
        None
    }

    /// One block placement: interval seconds plus the view yaw it was made
    /// with. The yaw-lock run catches scaffold towers placed without ever
    /// re-aiming.
    pub fn note_place(&mut self, dt_secs: f64, yaw_deg: f32) -> Option<BehavioralFlag> {
        self.place.observe(dt_secs);
        let locked = self
            .last_place_yaw
            .is_some_and(|prev| (yaw_deg - prev).abs() < YAW_LOCK_DEG);
        self.yaw_lock_run = if locked { self.yaw_lock_run + 1 } else { 0 };
        self.last_place_yaw = Some(yaw_deg);
        if self.yaw_lock_run >= YAW_LOCK_RUN || self.place.alarmed() {
            return Some(BehavioralFlag::Scaffold);
        }
        None
    }

    /// One dig-start interval in seconds (FastBreak metronome detection).
    pub fn note_dig_interval(&mut self, dt_secs: f64) -> Option<BehavioralFlag> {
        self.dig.observe(dt_secs);
        self.dig.alarmed().then_some(BehavioralFlag::FastBreak)
    }

    /// The highest channel score normalized to its alarm threshold
    /// (1.0 = alarmed); telemetry only, never a verdict on its own.
    pub fn suspicion(&self) -> f64 {
        [self.click, self.yaw, self.place, self.dig]
            .iter()
            .map(|c| c.score() / CUSUM_ALARM)
            .fold(0.0, f64::max)
    }
}

// ---------------------------------------------------------------------------
// ProtocolInvariantFsm
// ---------------------------------------------------------------------------

/// An impossible-to-legitimately-produce packet ordering or action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProtocolViolation {
    /// `STOP_DESTROY_BLOCK`/`FINISH` with no pending `START` while the
    /// block could not have insta-mined.
    DigFinishWithoutStart,
    /// A second `START` while a dig is already pending (vanilla aborts
    /// first).
    DigStartWhileDigging,
    /// Sprint is impossible while the use-item flag is up — the vanilla
    /// client drops sprint before eating/blocking/drawing.
    SprintWhileUsingItem,
    /// Block-change sequence ids must be non-negative and non-decreasing.
    SequenceRegression,
    /// `use_on` face varint outside the legal `0..=5` (and the `-1`
    /// air-use sentinel).
    InvalidFace,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum DigState {
    #[default]
    Idle,
    Started,
}

/// O(1) protocol-invariant state machine: tracks only the handful of bits
/// the vanilla client can never contradict.
#[derive(Clone, Debug, Default)]
pub struct ProtocolInvariantFsm {
    dig: DigState,
    using_item: bool,
    sprinting: bool,
    last_seq: Option<i64>,
    violations: u64,
}

impl ProtocolInvariantFsm {
    /// Fresh FSM.
    pub fn new() -> Self {
        Self::default()
    }

    /// One `block_dig` status. `instant_mine_possible` tells the FSM a
    /// FINISH can legally arrive with no START on file — the insta-mining
    /// client never sends FINISH, and a server-rejected START can still be
    /// followed by the client's finish.
    pub fn note_dig_status(
        &mut self,
        status: i64,
        instant_mine_possible: bool,
    ) -> Result<(), ProtocolViolation> {
        match status {
            0 => {
                // START: a second start without an abort is protocol noise.
                if self.dig == DigState::Started {
                    self.violations += 1;
                    self.dig = DigState::Started;
                    return Err(ProtocolViolation::DigStartWhileDigging);
                }
                self.dig = DigState::Started;
                Ok(())
            }
            1 => {
                // ABORT clears the dig; aborting an idle dig is tolerated.
                self.dig = DigState::Idle;
                Ok(())
            }
            2 => {
                // FINISH: legal only after a START, or when the block could
                // insta-mine (rejected START still leaves a client finish).
                let ok = self.dig == DigState::Started || instant_mine_possible;
                self.dig = DigState::Idle;
                if ok {
                    Ok(())
                } else {
                    self.violations += 1;
                    Err(ProtocolViolation::DigFinishWithoutStart)
                }
            }
            _ => Ok(()),
        }
    }

    /// Item use begins (shield raise, eat, bow draw): illegal while the
    /// sprint flag is still up.
    pub fn note_use_item(&mut self) -> Result<(), ProtocolViolation> {
        self.using_item = true;
        if self.sprinting {
            self.violations += 1;
            return Err(ProtocolViolation::SprintWhileUsingItem);
        }
        Ok(())
    }

    /// Item use released (`block_dig` status 5).
    pub fn note_use_item_stop(&mut self) {
        self.using_item = false;
    }

    /// Sprint state change (`entity_action` 3/4).
    pub fn note_sprint(&mut self, sprinting: bool) -> Result<(), ProtocolViolation> {
        let was = self.sprinting;
        self.sprinting = sprinting;
        if sprinting && !was && self.using_item {
            self.violations += 1;
            return Err(ProtocolViolation::SprintWhileUsingItem);
        }
        Ok(())
    }

    /// A block-change sequence id carried by a dig/place/ack packet.
    pub fn note_sequence(&mut self, seq: i64) -> Result<(), ProtocolViolation> {
        if seq < 0 || self.last_seq.is_some_and(|prev| seq < prev) {
            self.violations += 1;
            return Err(ProtocolViolation::SequenceRegression);
        }
        self.last_seq = Some(seq);
        Ok(())
    }

    /// A `use_on` face varint. `-1` is the air-use sentinel; everything
    /// else must be `0..=5`.
    pub fn note_face(&mut self, face: i64) -> Result<(), ProtocolViolation> {
        if !(-1..=5).contains(&face) {
            self.violations += 1;
            return Err(ProtocolViolation::InvalidFace);
        }
        Ok(())
    }

    /// Total invariant violations observed.
    pub fn violations(&self) -> u64 {
        self.violations
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn box_at(x: f64, y: f64, z: f64) -> Aabb3 {
        Aabb3::new(
            [x as f32 - 0.3, y as f32, z as f32 - 0.3],
            [x as f32 + 0.3, y as f32 + 1.8, z as f32 + 0.3],
        )
    }

    // ---------- TransactionEchoQueue ----------

    #[test]
    fn tx_echo_confirms_in_order_and_measures_rtt() {
        let mut q = TransactionEchoQueue::<8>::new();
        let a = q.issue(TxKind::VelocityGrant, 1_000);
        let b = q.issue(TxKind::Teleport, 1_010);
        let _c = q.issue(TxKind::EffectChange, 1_020);
        // Internally-issued ids carry the high bit — disjoint from real
        // wire ids — but still echo verbatim.
        assert!(a & (1 << 63) != 0 && b & (1 << 63) != 0);
        assert_eq!(q.pending(), 3);
        // Echo of the second drains the first two and returns its own age.
        let rtt = q.note_echo(b, 1_130);
        assert_eq!(rtt, Some(120));
        assert_eq!(q.pending(), 1);
        assert!(q.has_unconfirmed(TxKind::EffectChange));
        assert!(!q.has_unconfirmed(TxKind::VelocityGrant));
        // Unknown / already-confirmed ids yield nothing.
        assert_eq!(q.note_echo(999, 1_500), None);
        assert_eq!(q.note_echo(a, 1_500), None);
        let est = q.rtt();
        assert_eq!(est.samples, 1);
        assert!((est.rtt_ms - 120.0).abs() < 1e-9);
        // A second echo folds into the EWMA with jitter.
        let c = q.issue(TxKind::Teleport, 1_600);
        assert_eq!(q.note_echo(c, 1_700), Some(100));
        let est = q.rtt();
        assert_eq!(est.samples, 2);
        assert!((est.rtt_ms - 117.5).abs() < 1e-9);
        assert!(est.jitter_ms > 0.0);
    }

    #[test]
    fn tx_overflow_and_expiry_stay_bounded() {
        let mut q = TransactionEchoQueue::<4>::new();
        for i in 0..10 {
            q.note_grant(100 + i, TxKind::Teleport, i);
        }
        assert_eq!(q.pending(), 4);
        q.expire(1_000, 10);
        assert_eq!(q.pending(), 0);
    }

    // ---------- HitboxRewindBuffer ----------

    #[test]
    fn rewind_interpolates_between_bracketing_ticks() {
        let mut r = HitboxRewindBuffer::<64>::new();
        assert!(r.sample_at_tick(10).is_none());
        r.record(10, box_at(0.0, 64.0, 0.0));
        r.record(20, box_at(2.0, 64.0, 0.0));
        let mid = r.sample_at_tick(15).unwrap();
        assert!((mid.min[0] - (0.0 - 0.3 + 1.0)).abs() < 1e-5);
        // Outside the window: clamps, never extrapolates.
        assert_eq!(r.sample_at_tick(0), Some(box_at(0.0, 64.0, 0.0)));
        assert_eq!(r.sample_at_tick(99), Some(box_at(2.0, 64.0, 0.0)));
        // Same-tick replace and out-of-order drop keep monotonicity.
        r.record(20, box_at(4.0, 64.0, 0.0));
        r.record(5, box_at(9.0, 9.0, 9.0));
        assert_eq!(r.latest().unwrap().aabb, box_at(4.0, 64.0, 0.0));
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn rewind_verify_ray_reach_is_nan_safe() {
        let mut r = HitboxRewindBuffer::<64>::new();
        r.record(10, box_at(0.0, 64.0, 3.0));
        let eye = [0.0, 65.5, 0.0];
        // Straight at the box: inside 4.0 reach, outside 2.0.
        assert!(r.verify_ray_reach(eye, [0.0, -0.2, 1.0], 4.0, 10, 0.1).is_some());
        assert_eq!(r.verify_ray_reach(eye, [0.0, -0.2, 1.0], 2.0, 10, 0.1), None);
        // Wrong direction, NaN eye, NaN dir, empty history: no hit.
        assert_eq!(r.verify_ray_reach(eye, [1.0, 0.0, 0.0], 4.0, 10, 0.1), None);
        assert_eq!(r.verify_ray_reach([f64::NAN; 3], [0.0, 0.0, 1.0], 4.0, 10, 0.1), None);
        assert_eq!(r.verify_ray_reach(eye, [f64::NAN; 3], 4.0, 10, 0.1), None);
        let empty = HitboxRewindBuffer::<4>::new();
        assert_eq!(empty.verify_ray_reach(eye, [0.0, 0.0, 1.0], 9.0, 10, 0.1), None);
    }

    // ---------- PhysicsPredictionStep ----------

    #[test]
    fn physics_envelope_accepts_vanilla_and_rejects_impossible() {
        let env = PhysicsEnv::default();
        // Sprint-jump burst and a jump are comfortably inside.
        let ok = PhysicsPredictionStep::evaluate(&env, [0.36, 0.42, 0.0], None);
        assert!(ok.within);
        // A 30-block blink is not.
        let bad = PhysicsPredictionStep::evaluate(&env, [30.0, 0.0, 0.0], None);
        assert!(!bad.within && bad.excess_residual[0] > 29.0);
        // Terminal fall is inside; faster is not.
        let fall = PhysicsPredictionStep::evaluate(&env, [0.0, 0.0, 3.9], None);
        assert!(fall.within);
        let fast_fall = PhysicsPredictionStep::evaluate(&env, [0.0, 0.0, 9.0], None);
        assert!(!fast_fall.within);
        // Ice multiplies the horizontal envelope.
        let mut icy = env;
        icy.friction = SLIP_BLUE_ICE;
        let slide = PhysicsPredictionStep::evaluate(&icy, [1.4, 0.0, 0.0], None);
        assert!(slide.within);
        // Elytra widens everything.
        let mut glide = env;
        glide.fall_flying = true;
        assert!(PhysicsPredictionStep::evaluate(&glide, [3.5, 3.0, 3.5], None).within);
        // A mod grant covers exactly the declared carry, once.
        let grant = AcModGrantToken::new([1.2, 0.0, 0.0], 5);
        let carried = PhysicsPredictionStep::evaluate(&env, [1.5, 0.0, 0.0], Some(&grant));
        assert!(carried.within);
        let mut dead = grant;
        for _ in 0..5 {
            dead.tick_down();
        }
        assert!(dead.expired());
        assert!(!PhysicsPredictionStep::evaluate(&env, [1.5, 0.0, 0.0], Some(&dead)).within);
    }

    // ---------- BehavioralCusumDetector ----------

    #[test]
    fn cusum_flags_machine_cadence_and_ignores_human_jitter() {
        let mut bot = BehavioralCusumDetector::new();
        let mut flagged = false;
        // 200 ms metronome clicks — variance ~0.
        for i in 0..40 {
            if bot.note_click_interval(0.2 + (i % 2) as f64 * 0.000001).is_some() {
                flagged = true;
            }
        }
        assert!(flagged, "metronome clicks must alarm");
        let mut human = BehavioralCusumDetector::new();
        let mut flagged = false;
        // Deterministic-but-human spread of intervals (8-40 % swing).
        let mut x = 0.21f64;
        for _ in 0..200 {
            x = (x * 1.37).rem_euclid(0.4) + 0.08;
            if human.note_click_interval(x).is_some() {
                flagged = true;
            }
        }
        assert!(!flagged, "human jitter must never alarm");
    }

    #[test]
    fn cusum_yaw_snap_and_scaffold_channels() {
        let mut d = BehavioralCusumDetector::new();
        let mut flagged = false;
        for i in 0..KA_SNAP_RUN {
            flagged |= d.note_yaw_delta(130.0).is_some();
            let _ = i;
        }
        assert!(flagged, "sustained 130-degree snaps must flag KillAura");
        let mut d = BehavioralCusumDetector::new();
        let mut flagged = false;
        // Six placements, same yaw, metronome interval.
        for _ in 0..YAW_LOCK_RUN + 2 {
            flagged |= d.note_place(0.25, 42.0).is_some();
        }
        assert!(flagged, "yaw-locked metronome placement must flag Scaffold");
        let mut d = BehavioralCusumDetector::new();
        let mut flagged = false;
        for _ in 0..40 {
            flagged |= d.note_dig_interval(0.05).is_some();
        }
        assert!(flagged, "metronome digging must flag FastBreak");
    }

    // ---------- ProtocolInvariantFsm ----------

    #[test]
    fn fsm_catches_impossible_orderings_and_passes_vanilla() {
        let mut f = ProtocolInvariantFsm::new();
        // Insta-mine: FINISH without START is legal.
        assert!(f.note_dig_status(2, true).is_ok());
        // Without insta-mine it is a violation.
        assert_eq!(
            f.note_dig_status(2, false),
            Err(ProtocolViolation::DigFinishWithoutStart)
        );
        // Vanilla START -> FINISH sequence passes.
        assert!(f.note_dig_status(0, false).is_ok());
        assert!(f.note_dig_status(2, false).is_ok());
        // START while digging, negative/regressing seq, bad face, sprint-use.
        assert_eq!(
            f.note_dig_status(0, false).and_then(|_| f.note_dig_status(0, false)),
            Err(ProtocolViolation::DigStartWhileDigging)
        );
        assert_eq!(
            f.note_sequence(-1),
            Err(ProtocolViolation::SequenceRegression)
        );
        assert!(f.note_sequence(4).is_ok());
        assert_eq!(
            f.note_sequence(3),
            Err(ProtocolViolation::SequenceRegression)
        );
        assert!(f.note_face(-1).is_ok());
        assert!(f.note_face(5).is_ok());
        assert_eq!(f.note_face(6), Err(ProtocolViolation::InvalidFace));
        assert!(f.note_sprint(true).is_ok());
        assert_eq!(
            f.note_use_item(),
            Err(ProtocolViolation::SprintWhileUsingItem)
        );
        assert!(f.violations() >= 5);
    }
}
