// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Server-owned, tick-driven action timelines and hit validation.
//!
//! Timings and windows are half-open tick intervals. The caller supplies the
//! authoritative server tick and server-observed positions and facing. MOT1
//! packets describe snapshots for clients; decoding one never advances an action.

use std::collections::BTreeSet;

pub const MOTION_CHANNEL: &str = "megu:motion_v1";
pub const MOT1_MAGIC: [u8; 4] = *b"MOT1";
pub const MOT1_PACKET_LEN: usize = 28;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ActionPhase {
    Idle = 0,
    Anticipation = 1,
    ActiveHitWindow = 2,
    Recovery = 3,
}

impl ActionPhase {
    fn from_byte(value: u8) -> Result<Self, MotionError> {
        match value {
            0 => Ok(Self::Idle),
            1 => Ok(Self::Anticipation),
            2 => Ok(Self::ActiveHitWindow),
            3 => Ok(Self::Recovery),
            _ => Err(MotionError::InvalidPacket),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Hand {
    MainHand = 0,
    OffHand = 1,
}

impl Hand {
    fn from_byte(value: u8) -> Result<Self, MotionError> {
        match value {
            0 => Ok(Self::MainHand),
            1 => Ok(Self::OffHand),
            _ => Err(MotionError::InvalidPacket),
        }
    }

    fn opposite(self) -> Self {
        match self {
            Self::MainHand => Self::OffHand,
            Self::OffHand => Self::MainHand,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MotionError {
    InvalidSpec,
    InvalidCombo,
    NonFinite,
    TickOverflow,
    TickRewind,
    ActionLocked,
    InvalidPacket,
}

/// Inclusive start, exclusive end, relative to the action's start tick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TickWindow {
    pub start_tick: u32,
    pub end_tick: u32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum HitShape {
    SweepArc {
        reach: f64,
        half_angle_deg: f64,
        vertical_tolerance: f64,
    },
    ThrustCapsule {
        reach: f64,
        radius: f64,
    },
    RadialAoe {
        radius: f64,
    },
    ForwardCone {
        reach: f64,
        half_angle_deg: f64,
    },
}

impl HitShape {
    fn validate(self) -> Result<(), MotionError> {
        let positive = |value: f64| value.is_finite() && value > 0.0;
        let nonnegative = |value: f64| value.is_finite() && value >= 0.0;
        let angle = |value: f64| nonnegative(value) && value <= 180.0;
        let valid = match self {
            Self::SweepArc {
                reach,
                half_angle_deg,
                vertical_tolerance,
            } => positive(reach) && angle(half_angle_deg) && nonnegative(vertical_tolerance),
            Self::ThrustCapsule { reach, radius } => positive(reach) && nonnegative(radius),
            Self::RadialAoe { radius } => positive(radius),
            Self::ForwardCone {
                reach,
                half_angle_deg,
            } => positive(reach) && angle(half_angle_deg),
        };
        if valid {
            Ok(())
        } else {
            Err(MotionError::InvalidSpec)
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ActionSpec {
    pub action_id: u32,
    pub anticipation_ticks: u32,
    pub active_ticks: u32,
    pub recovery_ticks: u32,
    /// Earliest nominal elapsed tick at which an action can be canceled.
    pub cancel_after_tick: Option<u32>,
    /// Nominal elapsed ticks during which the actor is invulnerable.
    pub invulnerable_window: Option<TickWindow>,
    pub movement_speed_factor: f32,
    pub hit_shape: HitShape,
}

impl ActionSpec {
    pub fn validate(&self) -> Result<(), MotionError> {
        let total = self
            .anticipation_ticks
            .checked_add(self.active_ticks)
            .and_then(|n| n.checked_add(self.recovery_ticks))
            .ok_or(MotionError::TickOverflow)?;
        if self.active_ticks == 0
            || !self.movement_speed_factor.is_finite()
            || self.movement_speed_factor < 0.0
            || self.cancel_after_tick.is_some_and(|start| start > total)
            || self.invulnerable_window.is_some_and(|window| {
                window.start_tick >= window.end_tick || window.end_tick > total
            })
        {
            return Err(MotionError::InvalidSpec);
        }
        self.hit_shape.validate()
    }
}

fn normalized_speed(speed: f32) -> Result<f32, MotionError> {
    if !speed.is_finite() {
        return Err(MotionError::NonFinite);
    }
    Ok(speed.clamp(0.25, 4.0))
}

fn scaled_ticks(nominal: u32, speed: f32) -> Result<u32, MotionError> {
    let scaled = (f64::from(nominal) / f64::from(speed)).ceil();
    if scaled > f64::from(u32::MAX) {
        return Err(MotionError::TickOverflow);
    }
    Ok(scaled as u32)
}

#[derive(Clone, Copy, Debug)]
struct ScaledTimeline {
    anticipation_end: u32,
    active_end: u32,
    total: u32,
    cancel_from: Option<u32>,
    invulnerable: Option<TickWindow>,
}

impl ScaledTimeline {
    fn new(spec: &ActionSpec, speed: f32) -> Result<Self, MotionError> {
        let anticipation_end = scaled_ticks(spec.anticipation_ticks, speed)?;
        let active_end = anticipation_end
            .checked_add(scaled_ticks(spec.active_ticks, speed)?)
            .ok_or(MotionError::TickOverflow)?;
        let total = active_end
            .checked_add(scaled_ticks(spec.recovery_ticks, speed)?)
            .ok_or(MotionError::TickOverflow)?;

        // Scale offsets within their own phase so boundaries agree exactly
        // with the separately rounded phase durations.
        let offset = |tick: u32| -> Result<u32, MotionError> {
            if tick <= spec.anticipation_ticks {
                scaled_ticks(tick, speed)
            } else if tick <= spec.anticipation_ticks + spec.active_ticks {
                anticipation_end
                    .checked_add(scaled_ticks(tick - spec.anticipation_ticks, speed)?)
                    .ok_or(MotionError::TickOverflow)
            } else {
                active_end
                    .checked_add(scaled_ticks(
                        tick - spec.anticipation_ticks - spec.active_ticks,
                        speed,
                    )?)
                    .ok_or(MotionError::TickOverflow)
            }
        };

        let cancel_from = spec.cancel_after_tick.map(&offset).transpose()?;
        let invulnerable = spec
            .invulnerable_window
            .map(|window| {
                let start_tick = offset(window.start_tick)?;
                let end_tick = offset(window.end_tick)?
                    .max(start_tick.saturating_add(1))
                    .min(total);
                // A non-empty nominal window retains at least one server tick.
                Ok::<_, MotionError>(TickWindow {
                    start_tick: start_tick.min(total - 1),
                    end_tick,
                })
            })
            .transpose()?;
        Ok(Self {
            anticipation_end,
            active_end,
            total,
            cancel_from,
            invulnerable,
        })
    }
}

#[derive(Clone, Debug)]
pub struct ActiveAction {
    spec: ActionSpec,
    start_tick: u64,
    elapsed_ticks: u32,
    speed_multiplier: f32,
    hand: Hand,
    combo_index: u16,
    already_hit: BTreeSet<u64>,
    timeline: ScaledTimeline,
    last_update_tick: u64,
}

impl ActiveAction {
    pub fn start(
        spec: ActionSpec,
        speed: f32,
        now_tick: u64,
        hand: Hand,
    ) -> Result<Self, MotionError> {
        spec.validate()?;
        let speed_multiplier = normalized_speed(speed)?;
        let timeline = ScaledTimeline::new(&spec, speed_multiplier)?;
        now_tick
            .checked_add(u64::from(timeline.total))
            .ok_or(MotionError::TickOverflow)?;
        Ok(Self {
            spec,
            start_tick: now_tick,
            elapsed_ticks: 0,
            speed_multiplier,
            hand,
            combo_index: 0,
            already_hit: BTreeSet::new(),
            timeline,
            last_update_tick: now_tick,
        })
    }

    pub fn total_ticks(&self) -> u32 {
        self.timeline.total
    }

    pub fn spec(&self) -> &ActionSpec {
        &self.spec
    }

    pub fn start_tick(&self) -> u64 {
        self.start_tick
    }

    pub fn elapsed_ticks(&self) -> u32 {
        self.elapsed_ticks
    }

    pub fn speed_multiplier(&self) -> f32 {
        self.speed_multiplier
    }

    pub fn hand(&self) -> Hand {
        self.hand
    }

    pub fn combo_index(&self) -> u16 {
        self.combo_index
    }

    pub fn already_hit(&self) -> &BTreeSet<u64> {
        &self.already_hit
    }

    pub fn finished_at(&self) -> u64 {
        self.start_tick + u64::from(self.timeline.total)
    }

    pub fn phase(&self) -> ActionPhase {
        if self.elapsed_ticks >= self.timeline.total {
            ActionPhase::Idle
        } else if self.elapsed_ticks < self.timeline.anticipation_end {
            ActionPhase::Anticipation
        } else if self.elapsed_ticks < self.timeline.active_end {
            ActionPhase::ActiveHitWindow
        } else {
            ActionPhase::Recovery
        }
    }

    /// Advance using the server tick. Duplicate ticks are idempotent; rewinds fail.
    pub fn advance_to(&mut self, now_tick: u64) -> Result<ActionPhase, MotionError> {
        if now_tick < self.last_update_tick {
            return Err(MotionError::TickRewind);
        }
        self.last_update_tick = now_tick;
        self.elapsed_ticks = now_tick
            .saturating_sub(self.start_tick)
            .min(u64::from(self.timeline.total)) as u32;
        Ok(self.phase())
    }

    pub fn can_cancel(&self) -> bool {
        self.phase() != ActionPhase::Idle
            && self
                .timeline
                .cancel_from
                .is_some_and(|start| self.elapsed_ticks >= start)
    }

    pub fn is_invulnerable(&self) -> bool {
        self.phase() != ActionPhase::Idle
            && self.timeline.invulnerable.is_some_and(|window| {
                self.elapsed_ticks >= window.start_tick && self.elapsed_ticks < window.end_tick
            })
    }

    pub fn movement_speed_factor(&self) -> f32 {
        if self.phase() == ActionPhase::Idle {
            1.0
        } else {
            self.spec.movement_speed_factor
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ComboChain {
    pub steps: Vec<ActionSpec>,
    /// Maximum ticks after the previous action has finished to continue.
    pub combo_window_ticks: u32,
    pub alternate_hands: bool,
}

impl ComboChain {
    pub fn new(
        steps: Vec<ActionSpec>,
        combo_window_ticks: u32,
        alternate_hands: bool,
    ) -> Result<Self, MotionError> {
        let chain = Self {
            steps,
            combo_window_ticks,
            alternate_hands,
        };
        chain.validate()?;
        Ok(chain)
    }

    fn validate(&self) -> Result<(), MotionError> {
        if self.steps.is_empty() || self.steps.len() > usize::from(u16::MAX) + 1 {
            return Err(MotionError::InvalidCombo);
        }
        for spec in &self.steps {
            spec.validate()?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default)]
pub struct ComboState {
    next_index: usize,
    last_hand: Option<Hand>,
    current: Option<ActiveAction>,
    last_update_tick: Option<u64>,
}

impl ComboState {
    pub fn current(&self) -> Option<&ActiveAction> {
        self.current.as_ref()
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn advance_to(&mut self, now_tick: u64) -> Result<ActionPhase, MotionError> {
        if self.last_update_tick.is_some_and(|last| now_tick < last) {
            return Err(MotionError::TickRewind);
        }
        let phase = match &mut self.current {
            Some(action) => action.advance_to(now_tick)?,
            None => ActionPhase::Idle,
        };
        self.last_update_tick = Some(now_tick);
        Ok(phase)
    }

    /// Trigger the next step using only authoritative server time. A running
    /// step must reach its cancel marker before it can transition to the next.
    pub fn trigger(
        &mut self,
        chain: &ComboChain,
        now_tick: u64,
        speed: f32,
    ) -> Result<&ActiveAction, MotionError> {
        chain.validate()?;
        let speed = normalized_speed(speed)?;
        self.advance_to(now_tick)?;
        // An input retransmission must not enqueue another combo step in the
        // same tick, even when the first action permits immediate cancellation.
        if self
            .current
            .as_ref()
            .is_some_and(|previous| previous.start_tick == now_tick)
        {
            return Err(MotionError::ActionLocked);
        }

        let expired = if let Some(previous) = &self.current {
            if previous.phase() != ActionPhase::Idle {
                if !previous.can_cancel() {
                    return Err(MotionError::ActionLocked);
                }
                false
            } else {
                now_tick - previous.finished_at() > u64::from(chain.combo_window_ticks)
            }
        } else {
            true
        };

        let fresh = expired || self.next_index >= chain.steps.len();
        let index = if fresh { 0 } else { self.next_index };
        let hand = if chain.alternate_hands && !fresh {
            self.last_hand.unwrap_or(Hand::MainHand).opposite()
        } else {
            Hand::MainHand
        };
        let mut next = ActiveAction::start(chain.steps[index].clone(), speed, now_tick, hand)?;
        next.combo_index = index as u16;
        self.next_index = (index + 1) % chain.steps.len();
        self.last_hand = Some(hand);
        self.current = Some(next);
        Ok(self.current.as_ref().expect("just inserted active action"))
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Vec3 {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

impl Vec3 {
    pub const fn new(x: f64, y: f64, z: f64) -> Self {
        Self { x, y, z }
    }

    fn finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite() && self.z.is_finite()
    }

    fn sub(self, other: Self) -> Self {
        Self::new(self.x - other.x, self.y - other.y, self.z - other.z)
    }

    fn dot(self, other: Self) -> f64 {
        self.x * other.x + self.y * other.y + self.z * other.z
    }

    fn length_sq(self) -> f64 {
        self.dot(self)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HitCandidate {
    pub target_id: u64,
    pub position: Vec3,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HitValidationVerdict {
    Accepted,
    OutsideActiveWindow,
    AlreadyHit,
    OutOfRange,
    OutsideArc,
    Invulnerable,
    InvalidGeometry,
}

/// `source`, `facing`, and `candidate` must come from server state.
/// `target_action` is the target's server-owned dodge action, if any.
pub fn validate_hit(
    attacker_action: &mut ActiveAction,
    source: Vec3,
    facing: Vec3,
    candidate: HitCandidate,
    target_action: Option<&ActiveAction>,
) -> HitValidationVerdict {
    use HitValidationVerdict as Verdict;
    if attacker_action.phase() != ActionPhase::ActiveHitWindow {
        return Verdict::OutsideActiveWindow;
    }
    if attacker_action.already_hit.contains(&candidate.target_id) {
        return Verdict::AlreadyHit;
    }
    if target_action.is_some_and(ActiveAction::is_invulnerable) {
        return Verdict::Invulnerable;
    }
    let delta = candidate.position.sub(source);
    if !source.finite()
        || !facing.finite()
        || !candidate.position.finite()
        || !delta.finite()
        || !delta.length_sq().is_finite()
    {
        return Verdict::InvalidGeometry;
    }
    let distance_sq = delta.length_sq();
    let verdict = match attacker_action.spec.hit_shape {
        HitShape::RadialAoe { radius } => {
            if distance_sq <= radius * radius {
                Verdict::Accepted
            } else {
                Verdict::OutOfRange
            }
        }
        HitShape::SweepArc {
            reach,
            half_angle_deg,
            vertical_tolerance,
        } => {
            let horizontal_sq = delta.x * delta.x + delta.z * delta.z;
            let horizontal_facing_sq = facing.x * facing.x + facing.z * facing.z;
            if horizontal_facing_sq <= 0.0 || !horizontal_facing_sq.is_finite() {
                Verdict::InvalidGeometry
            } else if horizontal_sq > reach * reach || delta.y.abs() > vertical_tolerance {
                Verdict::OutOfRange
            } else if horizontal_sq > 0.0
                && (delta.x * facing.x + delta.z * facing.z)
                    / (horizontal_sq * horizontal_facing_sq).sqrt()
                    < half_angle_deg.to_radians().cos()
            {
                Verdict::OutsideArc
            } else {
                Verdict::Accepted
            }
        }
        shape => {
            let facing_sq = facing.length_sq();
            if facing_sq <= 0.0 || !facing_sq.is_finite() {
                Verdict::InvalidGeometry
            } else {
                let distance = distance_sq.sqrt();
                let along = delta.dot(facing) / facing_sq.sqrt();
                match shape {
                    HitShape::ThrustCapsule { reach, radius } => {
                        if along < 0.0 {
                            Verdict::OutsideArc
                        } else if along > reach + radius {
                            Verdict::OutOfRange
                        } else {
                            let clamped = along.min(reach);
                            let perpendicular_sq =
                                (distance_sq - 2.0 * clamped * along + clamped * clamped).max(0.0);
                            if perpendicular_sq <= radius * radius {
                                Verdict::Accepted
                            } else {
                                Verdict::OutOfRange
                            }
                        }
                    }
                    HitShape::ForwardCone {
                        reach,
                        half_angle_deg,
                    } => {
                        if distance > reach {
                            Verdict::OutOfRange
                        } else if distance > 0.0
                            && along / distance < half_angle_deg.to_radians().cos()
                        {
                            Verdict::OutsideArc
                        } else {
                            Verdict::Accepted
                        }
                    }
                    _ => unreachable!("other hit shapes handled above"),
                }
            }
        }
    };
    if verdict == Verdict::Accepted {
        attacker_action.already_hit.insert(candidate.target_id);
    }
    verdict
}

/// MOT1 fixed-width wire snapshot, with all integers in network byte order.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MotionTrigger {
    pub entity_id: u64,
    pub action_id: u32,
    pub combo_index: u16,
    pub hand: Hand,
    pub speed_multiplier: f32,
    pub elapsed_ticks: u32,
    pub phase: ActionPhase,
}

impl MotionTrigger {
    pub fn from_action(entity_id: u64, action: &ActiveAction) -> Self {
        Self {
            entity_id,
            action_id: action.spec.action_id,
            combo_index: action.combo_index,
            hand: action.hand,
            speed_multiplier: action.speed_multiplier,
            elapsed_ticks: action.elapsed_ticks,
            phase: action.phase(),
        }
    }
}

pub fn encode_motion_trigger(
    trigger: &MotionTrigger,
) -> Result<[u8; MOT1_PACKET_LEN], MotionError> {
    if !trigger.speed_multiplier.is_finite() {
        return Err(MotionError::NonFinite);
    }
    if !(0.25..=4.0).contains(&trigger.speed_multiplier) {
        return Err(MotionError::InvalidPacket);
    }
    let mut bytes = [0u8; MOT1_PACKET_LEN];
    bytes[..4].copy_from_slice(&MOT1_MAGIC);
    bytes[4..12].copy_from_slice(&trigger.entity_id.to_be_bytes());
    bytes[12..16].copy_from_slice(&trigger.action_id.to_be_bytes());
    bytes[16..18].copy_from_slice(&trigger.combo_index.to_be_bytes());
    bytes[18] = trigger.hand as u8;
    bytes[19..23].copy_from_slice(&trigger.speed_multiplier.to_bits().to_be_bytes());
    bytes[23..27].copy_from_slice(&trigger.elapsed_ticks.to_be_bytes());
    bytes[27] = trigger.phase as u8;
    Ok(bytes)
}

pub fn decode_motion_trigger(bytes: &[u8]) -> Result<MotionTrigger, MotionError> {
    if bytes.len() != MOT1_PACKET_LEN || bytes[..4] != MOT1_MAGIC {
        return Err(MotionError::InvalidPacket);
    }
    let speed_multiplier = f32::from_bits(u32::from_be_bytes(
        bytes[19..23]
            .try_into()
            .map_err(|_| MotionError::InvalidPacket)?,
    ));
    if !speed_multiplier.is_finite() {
        return Err(MotionError::NonFinite);
    }
    if !(0.25..=4.0).contains(&speed_multiplier) {
        return Err(MotionError::InvalidPacket);
    }
    Ok(MotionTrigger {
        entity_id: u64::from_be_bytes(
            bytes[4..12]
                .try_into()
                .map_err(|_| MotionError::InvalidPacket)?,
        ),
        action_id: u32::from_be_bytes(
            bytes[12..16]
                .try_into()
                .map_err(|_| MotionError::InvalidPacket)?,
        ),
        combo_index: u16::from_be_bytes(
            bytes[16..18]
                .try_into()
                .map_err(|_| MotionError::InvalidPacket)?,
        ),
        hand: Hand::from_byte(bytes[18])?,
        speed_multiplier,
        elapsed_ticks: u32::from_be_bytes(
            bytes[23..27]
                .try_into()
                .map_err(|_| MotionError::InvalidPacket)?,
        ),
        phase: ActionPhase::from_byte(bytes[27])?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(anticipation: u32, active: u32, recovery: u32) -> ActionSpec {
        ActionSpec {
            action_id: 7,
            anticipation_ticks: anticipation,
            active_ticks: active,
            recovery_ticks: recovery,
            cancel_after_tick: None,
            invulnerable_window: None,
            movement_speed_factor: 1.0,
            hit_shape: HitShape::RadialAoe { radius: 3.0 },
        }
    }

    /// Instant-attack spec: zero anticipation puts the action straight into
    /// the active hit window at tick 0.
    fn attacker(hit_shape: HitShape) -> ActiveAction {
        let mut s = spec(0, 10, 0);
        s.hit_shape = hit_shape;
        ActiveAction::start(s, 1.0, 100, Hand::MainHand).unwrap()
    }

    fn candidate(id: u64, x: f64, y: f64, z: f64) -> HitCandidate {
        HitCandidate {
            target_id: id,
            position: Vec3::new(x, y, z),
        }
    }

    #[test]
    fn spec_validation_rejects_malformed_timings_and_shapes() {
        assert_eq!(
            spec(1, 0, 1).validate(),
            Err(MotionError::InvalidSpec),
            "active_ticks == 0"
        );
        let mut bad = spec(1, 1, 1);
        bad.movement_speed_factor = f32::NAN;
        assert_eq!(bad.validate(), Err(MotionError::InvalidSpec));
        bad.movement_speed_factor = -0.5;
        assert_eq!(bad.validate(), Err(MotionError::InvalidSpec));
        bad.movement_speed_factor = 1.0;
        bad.cancel_after_tick = Some(4); // nominal total is 3
        assert_eq!(bad.validate(), Err(MotionError::InvalidSpec));
        bad.cancel_after_tick = None;
        bad.invulnerable_window = Some(TickWindow {
            start_tick: 2,
            end_tick: 2,
        });
        assert_eq!(bad.validate(), Err(MotionError::InvalidSpec));
        bad.invulnerable_window = Some(TickWindow {
            start_tick: 1,
            end_tick: 9,
        });
        assert_eq!(bad.validate(), Err(MotionError::InvalidSpec));
        bad.invulnerable_window = None;
        bad.hit_shape = HitShape::SweepArc {
            reach: 3.0,
            half_angle_deg: 181.0,
            vertical_tolerance: 1.0,
        };
        assert_eq!(bad.validate(), Err(MotionError::InvalidSpec));
        bad.hit_shape = HitShape::RadialAoe { radius: 0.0 };
        assert_eq!(bad.validate(), Err(MotionError::InvalidSpec));
        bad.hit_shape = HitShape::ThrustCapsule {
            reach: 2.0,
            radius: -1.0,
        };
        assert_eq!(bad.validate(), Err(MotionError::InvalidSpec));
        bad.hit_shape = HitShape::ForwardCone {
            reach: f64::INFINITY,
            half_angle_deg: 45.0,
        };
        assert_eq!(bad.validate(), Err(MotionError::InvalidSpec));
        // Phase tick sum overflowing u32 is a distinct error.
        let overflow = ActionSpec {
            anticipation_ticks: u32::MAX,
            active_ticks: 1,
            recovery_ticks: 0,
            ..spec(0, 1, 0)
        };
        assert_eq!(overflow.validate(), Err(MotionError::TickOverflow));
    }

    #[test]
    fn speed_multiplier_is_normalized_and_scales_the_timeline() {
        let base = spec(2, 4, 2);
        let fast = ActiveAction::start(base.clone(), 100.0, 0, Hand::MainHand).unwrap();
        assert_eq!(fast.speed_multiplier(), 4.0);
        // ceil(2/4) + ceil(4/4) + ceil(2/4) = 1 + 1 + 1
        assert_eq!(fast.total_ticks(), 3);
        let slow = ActiveAction::start(base.clone(), 0.1, 0, Hand::MainHand).unwrap();
        assert_eq!(slow.speed_multiplier(), 0.25);
        // ceil(n / 0.25) = 4n per phase: 8 + 16 + 8
        assert_eq!(slow.total_ticks(), 32);
        assert_eq!(
            ActiveAction::start(base, f32::NAN, 0, Hand::MainHand).unwrap_err(),
            MotionError::NonFinite
        );
        // Finite out-of-range speeds clamp to the same floor as tiny positives.
        let negative = ActiveAction::start(spec(1, 1, 1), -3.0, 0, Hand::MainHand).unwrap();
        assert_eq!(negative.speed_multiplier(), 0.25);
    }

    #[test]
    fn scaled_ticks_overflow_and_end_tick_overflow_fail_closed() {
        // ceil((u32::MAX - 10) / 0.25) exceeds u32::MAX.
        let huge = spec(u32::MAX - 10, 5, 0);
        assert_eq!(
            ActiveAction::start(huge, 0.1, 0, Hand::MainHand).unwrap_err(),
            MotionError::TickOverflow
        );
        // now_tick + total_ticks must fit in u64.
        assert_eq!(
            ActiveAction::start(spec(1, 1, 1), 1.0, u64::MAX - 1, Hand::MainHand).unwrap_err(),
            MotionError::TickOverflow
        );
    }

    #[test]
    fn timeline_phases_windows_and_tick_safety() {
        let mut s = spec(2, 4, 2);
        s.movement_speed_factor = 0.5;
        s.cancel_after_tick = Some(3);
        s.invulnerable_window = Some(TickWindow {
            start_tick: 3,
            end_tick: 6,
        });
        let mut action = ActiveAction::start(s, 1.0, 100, Hand::OffHand).unwrap();
        assert_eq!(action.phase(), ActionPhase::Anticipation);
        assert_eq!(action.hand(), Hand::OffHand);
        assert_eq!(action.movement_speed_factor(), 0.5);
        assert!(!action.can_cancel());
        assert!(!action.is_invulnerable());
        // A duplicate tick is idempotent; a rewind is rejected.
        assert_eq!(action.advance_to(100), Ok(ActionPhase::Anticipation));
        assert_eq!(action.elapsed_ticks(), 0);
        assert_eq!(action.advance_to(99), Err(MotionError::TickRewind));
        assert_eq!(action.advance_to(101), Ok(ActionPhase::Anticipation));
        assert_eq!(action.advance_to(103), Ok(ActionPhase::ActiveHitWindow));
        assert!(action.can_cancel());
        assert!(action.is_invulnerable());
        // The invulnerable window is half-open: elapsed 6 is outside [3, 6).
        assert_eq!(action.advance_to(106), Ok(ActionPhase::Recovery));
        assert!(!action.is_invulnerable());
        assert_eq!(action.advance_to(108), Ok(ActionPhase::Idle));
        assert!(!action.can_cancel());
        assert_eq!(action.movement_speed_factor(), 1.0);
        assert_eq!(action.total_ticks(), 8);
        assert_eq!(action.finished_at(), 108);
    }

    fn combo_spec() -> ActionSpec {
        let mut s = spec(1, 4, 1);
        s.cancel_after_tick = Some(2);
        s
    }

    #[test]
    fn combo_trigger_locks_same_tick_and_honors_cancel_marker() {
        let chain = ComboChain::new(vec![combo_spec(), combo_spec()], 10, true).unwrap();
        let mut state = ComboState::default();
        let first = {
            let a = state.trigger(&chain, 100, 1.0).unwrap();
            (a.combo_index(), a.hand(), a.start_tick())
        };
        assert_eq!(first, (0, Hand::MainHand, 100));
        // Same-tick retransmission is locked even though cancellation is legal.
        assert!(matches!(
            state.trigger(&chain, 100, 1.0),
            Err(MotionError::ActionLocked)
        ));
        state.advance_to(101).unwrap();
        assert!(matches!(
            state.trigger(&chain, 101, 1.0),
            Err(MotionError::ActionLocked)
        ));
        // At the cancel marker the next step starts with the alternate hand.
        let second = {
            let a = state.trigger(&chain, 102, 1.0).unwrap();
            (a.combo_index(), a.hand(), a.start_tick())
        };
        assert_eq!(second, (1, Hand::OffHand, 102));
    }

    #[test]
    fn combo_window_expiry_restarts_and_rewinds_fail() {
        let chain = ComboChain::new(vec![combo_spec(), combo_spec()], 5, true).unwrap();
        let mut state = ComboState::default();
        state.trigger(&chain, 100, 1.0).unwrap(); // finishes at 106
        // 110 - 106 = 4 <= window: continue to step 1 on the off hand.
        let second = {
            let a = state.trigger(&chain, 110, 1.0).unwrap();
            (a.combo_index(), a.hand())
        };
        assert_eq!(second, (1, Hand::OffHand)); // step 1 finishes at 116
        // 122 - 116 = 6 > window: fresh restart resets index and hand.
        let third = {
            let a = state.trigger(&chain, 122, 1.0).unwrap();
            (a.combo_index(), a.hand())
        };
        assert_eq!(third, (0, Hand::MainHand));
        assert!(matches!(
            state.advance_to(50),
            Err(MotionError::TickRewind)
        ));
        assert!(matches!(
            state.trigger(&chain, 123, f32::NAN),
            Err(MotionError::NonFinite)
        ));
    }

    #[test]
    fn combo_chain_validation_bounds() {
        assert!(matches!(
            ComboChain::new(vec![], 0, false),
            Err(MotionError::InvalidCombo)
        ));
        assert!(matches!(
            ComboChain::new(vec![spec(0, 0, 1)], 0, false),
            Err(MotionError::InvalidSpec)
        ));
        let too_many = vec![spec(0, 1, 0); usize::from(u16::MAX) + 2];
        assert!(matches!(
            ComboChain::new(too_many, 0, false),
            Err(MotionError::InvalidCombo)
        ));
    }

    #[test]
    fn hit_validation_rejects_outside_window_and_nonfinite_geometry() {
        let mut s = spec(2, 4, 2);
        s.hit_shape = HitShape::RadialAoe { radius: 3.0 };
        let mut outside = ActiveAction::start(s, 1.0, 0, Hand::MainHand).unwrap();
        let origin = Vec3::new(0.0, 0.0, 0.0);
        let facing = Vec3::new(1.0, 0.0, 0.0);
        assert_eq!(
            validate_hit(&mut outside, origin, facing, candidate(1, 1.0, 0.0, 0.0), None),
            HitValidationVerdict::OutsideActiveWindow
        );

        let mut a = attacker(HitShape::RadialAoe { radius: 3.0 });
        assert_eq!(
            validate_hit(&mut a, origin, facing, candidate(1, f64::NAN, 0.0, 0.0), None),
            HitValidationVerdict::InvalidGeometry
        );
        assert_eq!(
            validate_hit(
                &mut a,
                Vec3::new(f64::INFINITY, 0.0, 0.0),
                facing,
                candidate(1, 1.0, 0.0, 0.0),
                None
            ),
            HitValidationVerdict::InvalidGeometry
        );
        assert_eq!(
            validate_hit(&mut a, origin, Vec3::new(f64::NAN, 0.0, 0.0), candidate(1, 1.0, 0.0, 0.0), None),
            HitValidationVerdict::InvalidGeometry
        );
    }

    #[test]
    fn radial_aoe_accepts_dedups_and_bounds_range() {
        let mut a = attacker(HitShape::RadialAoe { radius: 3.0 });
        let origin = Vec3::new(0.0, 0.0, 0.0);
        let facing = Vec3::new(1.0, 0.0, 0.0);
        assert_eq!(
            validate_hit(&mut a, origin, facing, candidate(1, 0.0, 0.0, 0.0), None),
            HitValidationVerdict::Accepted,
            "zero distance is inside the radius"
        );
        assert_eq!(
            validate_hit(&mut a, origin, facing, candidate(1, 1.0, 0.0, 0.0), None),
            HitValidationVerdict::AlreadyHit
        );
        assert_eq!(
            validate_hit(&mut a, origin, facing, candidate(2, 3.0, 0.0, 0.0), None),
            HitValidationVerdict::Accepted,
            "distance == radius is inclusive"
        );
        assert_eq!(
            validate_hit(&mut a, origin, facing, candidate(3, 3.1, 0.0, 0.0), None),
            HitValidationVerdict::OutOfRange
        );
    }

    #[test]
    fn invulnerable_target_is_rejected_after_dedup_check() {
        let mut target_spec = spec(0, 10, 0);
        target_spec.invulnerable_window = Some(TickWindow {
            start_tick: 0,
            end_tick: 10,
        });
        let target = ActiveAction::start(target_spec, 1.0, 100, Hand::MainHand).unwrap();
        let mut a = attacker(HitShape::RadialAoe { radius: 3.0 });
        let origin = Vec3::new(0.0, 0.0, 0.0);
        let facing = Vec3::new(1.0, 0.0, 0.0);
        assert_eq!(
            validate_hit(&mut a, origin, facing, candidate(9, 1.0, 0.0, 0.0), Some(&target)),
            HitValidationVerdict::Invulnerable
        );
        // Once the dodge window ends the same geometry is accepted.
        let mut expired_spec = spec(0, 10, 0);
        expired_spec.invulnerable_window = Some(TickWindow {
            start_tick: 0,
            end_tick: 1,
        });
        let mut expired = ActiveAction::start(expired_spec, 1.0, 100, Hand::MainHand).unwrap();
        expired.advance_to(101).unwrap();
        assert_eq!(
            validate_hit(&mut a, origin, facing, candidate(10, 1.0, 0.0, 0.0), Some(&expired)),
            HitValidationVerdict::Accepted
        );
    }

    #[test]
    fn sweep_arc_checks_horizontal_reach_angle_and_height() {
        let mut a = attacker(HitShape::SweepArc {
            reach: 5.0,
            half_angle_deg: 90.0,
            vertical_tolerance: 2.0,
        });
        let origin = Vec3::new(0.0, 0.0, 0.0);
        let facing = Vec3::new(1.0, 0.0, 0.0);
        assert_eq!(
            validate_hit(&mut a, origin, facing, candidate(1, 3.0, 0.0, 0.0), None),
            HitValidationVerdict::Accepted
        );
        assert_eq!(
            validate_hit(&mut a, origin, facing, candidate(2, 0.0, 0.0, 3.0), None),
            HitValidationVerdict::OutsideArc,
            "perpendicular is outside a 90 degree half-angle"
        );
        assert_eq!(
            validate_hit(&mut a, origin, facing, candidate(3, -3.0, 0.0, 0.0), None),
            HitValidationVerdict::OutsideArc
        );
        assert_eq!(
            validate_hit(&mut a, origin, facing, candidate(4, 6.0, 0.0, 0.0), None),
            HitValidationVerdict::OutOfRange
        );
        assert_eq!(
            validate_hit(&mut a, origin, facing, candidate(5, 3.0, 3.0, 0.0), None),
            HitValidationVerdict::OutOfRange,
            "vertical tolerance exceeded"
        );
        assert_eq!(
            validate_hit(&mut a, origin, Vec3::new(0.0, 0.0, 0.0), candidate(6, 1.0, 0.0, 0.0), None),
            HitValidationVerdict::InvalidGeometry,
            "zero horizontal facing cannot define an arc"
        );
        // Directly overhead still counts: horizontal distance is zero.
        assert_eq!(
            validate_hit(&mut a, origin, facing, candidate(7, 0.0, 1.0, 0.0), None),
            HitValidationVerdict::Accepted
        );
    }

    #[test]
    fn thrust_capsule_and_forward_cone_geometry() {
        let mut thrust = attacker(HitShape::ThrustCapsule {
            reach: 4.0,
            radius: 1.0,
        });
        let origin = Vec3::new(0.0, 0.0, 0.0);
        let forward = Vec3::new(0.0, 0.0, 1.0);
        assert_eq!(
            validate_hit(&mut thrust, origin, forward, candidate(1, 0.0, 0.0, 3.0), None),
            HitValidationVerdict::Accepted
        );
        assert_eq!(
            validate_hit(&mut thrust, origin, forward, candidate(2, 0.0, 0.0, 0.0), None),
            HitValidationVerdict::Accepted,
            "zero distance sits inside the capsule"
        );
        assert_eq!(
            validate_hit(&mut thrust, origin, forward, candidate(3, 0.0, 0.0, -1.0), None),
            HitValidationVerdict::OutsideArc,
            "behind the attacker is outside the thrust"
        );
        assert_eq!(
            validate_hit(&mut thrust, origin, forward, candidate(4, 0.0, 0.0, 6.0), None),
            HitValidationVerdict::OutOfRange,
            "beyond reach + radius"
        );
        assert_eq!(
            validate_hit(&mut thrust, origin, forward, candidate(5, 1.5, 0.0, 3.0), None),
            HitValidationVerdict::OutOfRange,
            "perpendicular offset exceeds the capsule radius"
        );
        assert_eq!(
            validate_hit(&mut thrust, origin, Vec3::new(0.0, 0.0, 0.0), candidate(6, 0.0, 0.0, 1.0), None),
            HitValidationVerdict::InvalidGeometry
        );

        let mut cone = attacker(HitShape::ForwardCone {
            reach: 5.0,
            half_angle_deg: 45.0,
        });
        let facing = Vec3::new(1.0, 0.0, 0.0);
        assert_eq!(
            validate_hit(&mut cone, origin, facing, candidate(1, 3.0, 0.0, 0.0), None),
            HitValidationVerdict::Accepted
        );
        assert_eq!(
            validate_hit(&mut cone, origin, facing, candidate(2, 0.0, 0.0, 3.0), None),
            HitValidationVerdict::OutsideArc
        );
        assert_eq!(
            validate_hit(&mut cone, origin, facing, candidate(3, 6.0, 0.0, 0.0), None),
            HitValidationVerdict::OutOfRange
        );
        assert_eq!(
            validate_hit(&mut cone, origin, facing, candidate(4, 0.0, 0.0, 0.0), None),
            HitValidationVerdict::Accepted,
            "zero distance skips the angle test"
        );
    }

    #[test]
    fn mot1_roundtrip_and_rejection_paths() {
        let trigger = MotionTrigger {
            entity_id: u64::MAX,
            action_id: u32::MAX,
            combo_index: u16::MAX,
            hand: Hand::OffHand,
            speed_multiplier: 0.25,
            elapsed_ticks: u32::MAX,
            phase: ActionPhase::Recovery,
        };
        let bytes = encode_motion_trigger(&trigger).unwrap();
        assert_eq!(bytes.len(), MOT1_PACKET_LEN);
        assert_eq!(&bytes[..4], &MOT1_MAGIC);
        assert_eq!(decode_motion_trigger(&bytes), Ok(trigger));

        let mut s = spec(0, 10, 0);
        s.hit_shape = HitShape::RadialAoe { radius: 1.0 };
        let action = ActiveAction::start(s, 2.0, 55, Hand::MainHand).unwrap();
        let snapshot = MotionTrigger::from_action(9, &action);
        let packet = encode_motion_trigger(&snapshot).unwrap();
        assert_eq!(decode_motion_trigger(&packet), Ok(snapshot));

        assert_eq!(
            encode_motion_trigger(&MotionTrigger {
                speed_multiplier: f32::NAN,
                ..trigger
            })
            .unwrap_err(),
            MotionError::NonFinite
        );
        assert_eq!(
            encode_motion_trigger(&MotionTrigger {
                speed_multiplier: 4.5,
                ..trigger
            })
            .unwrap_err(),
            MotionError::InvalidPacket
        );
        assert_eq!(
            decode_motion_trigger(&bytes[..MOT1_PACKET_LEN - 1]),
            Err(MotionError::InvalidPacket)
        );
        let mut bad_magic = bytes;
        bad_magic[0] = b'X';
        assert_eq!(decode_motion_trigger(&bad_magic), Err(MotionError::InvalidPacket));
        let mut bad_phase = bytes;
        bad_phase[27] = 9;
        assert_eq!(decode_motion_trigger(&bad_phase), Err(MotionError::InvalidPacket));
        let mut bad_hand = bytes;
        bad_hand[18] = 2;
        assert_eq!(decode_motion_trigger(&bad_hand), Err(MotionError::InvalidPacket));
        let mut nan_speed = bytes;
        nan_speed[19..23].copy_from_slice(&f32::NAN.to_bits().to_be_bytes());
        assert_eq!(decode_motion_trigger(&nan_speed), Err(MotionError::NonFinite));
        let mut slow_speed = bytes;
        slow_speed[19..23].copy_from_slice(&0.1f32.to_bits().to_be_bytes());
        assert_eq!(decode_motion_trigger(&slow_speed), Err(MotionError::InvalidPacket));
    }
}
