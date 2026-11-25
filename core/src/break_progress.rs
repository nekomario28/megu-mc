// SPDX-License-Identifier: LGPL-3.0-or-later
#![forbid(unsafe_code)]
//! `megu_core::break_progress` — the canonical Vanilla 1.21.1
//! `getDestroyProgress` model every block-breaking path shares.
//!
//! Survival mining, insta-mine prediction, and mod/contraption breakers all
//! reduce to one formula: each tick the break accumulates
//! `effective_speed / hardness / (can_harvest ? 30 : 100)` and the block
//! drops when the running sum reaches `1.0`. [`destroy_progress_per_tick`]
//! is that formula, [`instant_break`] is `ServerPlayerGameMode`'s
//! one-tick-break predicate, and [`ticks_to_break`] is the closed-form tick
//! count the same code computes by division.
//!
//! The model is deliberately tool-agnostic: pickaxes on stone, shovels on
//! dirt, axes on logs, hoes on sculk, shears on wool, swords on bamboo,
//! `minecraft:tool` components, and machine breakers (Create drills/saws,
//! mod contraptions) all feed `effective_speed` through the same entry
//! points — the caller owns *how* the speed was computed, this module owns
//! what the speed *does*.

/// `getDestroyProgress`: destroy progress accumulated in one tick.
///
/// `hardness <= 0.0` (insta-break blocks and indestructible blocks alike)
/// and non-positive or non-finite speed contribute `0.0` — indestructible
/// blocks never progress and a broken speed never fabricates one.
#[inline]
pub fn destroy_progress_per_tick(effective_speed: f32, hardness: f32, can_harvest: bool) -> f32 {
    if !(effective_speed.is_finite() && hardness.is_finite())
        || hardness <= 0.0
        || effective_speed <= 0.0
    {
        return 0.0;
    }
    effective_speed / hardness / if can_harvest { 30.0 } else { 100.0 }
}

/// Margin under the exact `damage_per_tick >= 1.0` boundary: IEEE-754
/// `f32` edge cases like `45.0 / 1.5 / 30.0` land one ulp below `1.0`, and
/// treating them as non-instant would desync the client that insta-mines
/// them — the classic boundary ghost block.
pub const INSTANT_BREAK_EPSILON: f32 = 1e-5;

/// `ServerPlayerGameMode` insta-break predicate: a block with no destroy
/// time (`hardness == 0.0`), or one whose first tick of progress reaches
/// the break threshold. This is the `destroy_progress_per_tick >= 1` half
/// that makes a vanilla client send `START_DESTROY_BLOCK` and never a
/// `STOP_DESTROY_BLOCK` — the server must break on START too. The verdict
/// is computed directly in `f32` against `1.0 - INSTANT_BREAK_EPSILON`,
/// never via a seconds round-trip.
#[inline]
pub fn instant_break(effective_speed: f32, hardness: f32, can_harvest: bool) -> bool {
    hardness == 0.0
        || (hardness > 0.0
            && destroy_progress_per_tick(effective_speed, hardness, can_harvest)
                >= 1.0 - INSTANT_BREAK_EPSILON)
}

/// Ticks of accumulated progress the block needs to break: `0` is instant
/// (zero hardness or `damage_per_tick >= 1`), `u32::MAX` is never — a
/// negative-hardness block like bedrock keeps returning zero progress, and
/// so does mining with no effective speed.
#[inline]
pub fn ticks_to_break(hardness: f32, can_harvest: bool, effective_speed: f32) -> u32 {
    if hardness == 0.0 {
        return 0;
    }
    if !(hardness.is_finite() && effective_speed.is_finite())
        || hardness < 0.0
        || effective_speed <= 0.0
    {
        return u32::MAX;
    }
    let damage_per_tick = destroy_progress_per_tick(effective_speed, hardness, can_harvest);
    if damage_per_tick >= 1.0 {
        return 0;
    }
    (1.0 / damage_per_tick).ceil() as u32
}

/// Stateful accumulator matching `ServerPlayerGameMode.tick`: a dig
/// completes once the summed per-tick progress reaches `1.0`. Zero-size and
/// reusable across digs — the same struct drives player mining, machine
/// breakers, and prediction-side dig accounting.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BreakProgressModel {
    progress: f32,
}

impl BreakProgressModel {
    /// Empty accumulator.
    pub fn new() -> Self {
        Self { progress: 0.0 }
    }

    /// Accumulated destroy progress so far, `0.0..1.0`.
    pub fn progress(&self) -> f32 {
        self.progress
    }

    /// Abandon the dig (cancel, block changed underneath, disconnect).
    pub fn reset(&mut self) {
        self.progress = 0.0;
    }

    /// Add one tick of `getDestroyProgress`. Returns `true` the tick the
    /// running sum reaches the break threshold — the moment the block drops.
    pub fn accumulate(&mut self, effective_speed: f32, hardness: f32, can_harvest: bool) -> bool {
        self.progress +=
            destroy_progress_per_tick(effective_speed, hardness, can_harvest);
        self.progress >= 1.0
    }

    /// The crack overlay stage `0..=9` `Level.destroyBlockProgress`
    /// broadcasts for this progress fraction, `-1` before any progress.
    pub fn crack_stage(&self) -> i8 {
        if self.progress <= 0.0 {
            -1
        } else {
            ((self.progress * 10.0) as i8).clamp(0, 9)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instant_break_covers_every_tool_family() {
        // Zero-hardness insta-mines regardless of speed or tool.
        for speed in [0.0, 1.0, 34.0] {
            assert!(instant_break(speed, 0.0, true));
            assert!(instant_break(speed, 0.0, false));
        }
        // Efficiency V diamond pick + Haste II on stone: 47.6/1.5/30 >= 1.
        assert!(instant_break(47.6, 1.5, true));
        // Netherite pick + Eff V + Haste II: 49.0/1.5/30 >= 1.
        assert!(instant_break(49.0, 1.5, true));
        // Shovel on dirt (h 0.5): Efficiency V golden shovel insta-mines
        // (38/0.5/30 = 2.53), a bare golden shovel does not (12/0.5/30 = 0.8).
        assert!(instant_break(38.0, 0.5, true));
        assert!(!instant_break(12.0, 0.5, true));
        // Axe + Eff V + Haste II on a log (h 2.0): (8+26)*1.4/2/30 = 0.79 — no.
        assert!(!instant_break(34.0 * 1.4, 2.0, true));
        // Non-harvest divisor /100 keeps the same speed legal-but-not-instant.
        assert!(!instant_break(47.6, 1.5, false));
        // Bedrock never insta-mines at any speed.
        assert!(!instant_break(f32::MAX, -1.0, true));
        // Bare hand on stone: no.
        assert!(!instant_break(1.0, 1.5, true));
    }

    #[test]
    fn instant_break_ieee754_boundary_never_desyncs() {
        // `45.0 / 1.5 / 30.0` is mathematically 1.0; the epsilon keeps any
        // f32 rounding of it on the instant side.
        assert_eq!(destroy_progress_per_tick(45.0, 1.5, true), 1.0);
        assert!(instant_break(45.0, 1.5, true));
        // One ulp below the threshold still counts as instant — the client
        // insta-mines these and sends no FINISH.
        assert!(instant_break(44.99999, 1.5, true));
        // Eff V diamond pick + Haste II on stone: 47.6/1.5/30 = 1.0577…
        assert!(instant_break(47.6, 1.5, true));
        // Clearly below the boundary stays multi-tick for every tool family.
        assert!(!instant_break(29.9, 1.0, true));
        assert!(!instant_break(44.99, 1.5, true));
        // Shovel on dirt (h 0.5): bare iron shovel 6.0/0.5/30 = 0.4 — no.
        assert!(!instant_break(6.0, 0.5, true));
        // Wrong-tool divisor /100: the same 45.0 speed is not instant.
        assert!(!instant_break(45.0, 1.5, false));
        // A Create-style machine breaker speed (34.0) on a log (h 2.0)
        // still evaluates through the same canonical formula.
        assert!(instant_break(60.0, 2.0, true));
        assert!(!instant_break(59.0, 2.0, true));
    }

    #[test]
    fn ticks_to_break_matches_the_vanilla_table() {
        assert_eq!(ticks_to_break(1.5, true, 8.0), 6);
        assert_eq!(ticks_to_break(1.5, false, 8.0), 19);
        assert_eq!(ticks_to_break(0.0, true, 1.0), 0);
        assert_eq!(ticks_to_break(1.0, true, 30.0), 0);
        assert_eq!(ticks_to_break(1.0, true, 29.9), 2);
        assert_eq!(ticks_to_break(-1.0, true, 34.0), u32::MAX);
        assert_eq!(ticks_to_break(50.0, true, 0.0), u32::MAX);
        assert_eq!(ticks_to_break(50.0, true, 8.0), 188);
    }

    #[test]
    fn progress_model_accumulates_to_break_and_reports_cracks() {
        let mut m = BreakProgressModel::new();
        assert_eq!(m.crack_stage(), -1);
        // Diamond pick on stone without haste: 8/1.5/30 ≈ 0.1778/tick -> 6 ticks.
        for i in 1..6 {
            assert!(!m.accumulate(8.0, 1.5, true), "tick {i} must not break");
        }
        assert!(m.accumulate(8.0, 1.5, true));
        assert!(m.progress() >= 1.0);
        let mut m = BreakProgressModel::new();
        m.accumulate(47.6, 1.5, true);
        assert_eq!(m.crack_stage(), 9);
        m.reset();
        assert_eq!(m.progress(), 0.0);
        assert_eq!(m.crack_stage(), -1);
    }

    #[test]
    fn nan_and_broken_inputs_never_fabricate_progress() {
        assert_eq!(destroy_progress_per_tick(f32::NAN, 1.0, true), 0.0);
        assert_eq!(destroy_progress_per_tick(1.0, f32::NAN, true), 0.0);
        assert!(!instant_break(f32::NAN, 1.0, true));
        assert_eq!(ticks_to_break(f32::INFINITY, true, 1.0), u32::MAX);
        let mut m = BreakProgressModel::new();
        assert!(!m.accumulate(f32::NAN, 1.5, true));
        assert_eq!(m.progress(), 0.0);
    }
}
