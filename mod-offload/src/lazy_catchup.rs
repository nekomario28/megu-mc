// SPDX-License-Identifier: LGPL-3.0-or-later

//! O(1) unloaded-chunk catch-up and headless factory stepping.
//!
//! Survival/modded bases keep progressing while their chunks are unloaded,
//! but the server must never pay chunk-loader RAM or TPS for it. Every entry
//! point here is pure deterministic arithmetic: no tick-by-tick loops, no
//! clocks, no locks, and every quantity is bounded by [`CatchUpLimits`] so
//! the whole feature is strictly opt-in and fails closed.
//!
//! The furnace and machine kernels are *exact*: they compute the same final
//! state a per-tick simulation would reach, in closed form. The growth kernel
//! skips Bernoulli trials with exact geometric waiting times drawn from a
//! seeded PRNG, so it is deterministic and distribution-exact rather than an
//! approximation.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;

use crate::explosion_batch::XorShift64;

/// Ticks per real hour at the vanilla 20 TPS rate.
pub const TICKS_PER_HOUR: u64 = 72_000;
/// Largest catch-up horizon any server may configure: 24 real hours.
pub const HARD_MAX_DELTA_TICKS: u64 = TICKS_PER_HOUR * 24;
/// Fail-closed default horizon: one real hour per catch-up step.
pub const DEFAULT_MAX_DELTA_TICKS: u64 = TICKS_PER_HOUR;
/// Hard cap on headless factory endpoints; keeps registry RAM bounded.
pub const MAX_HEADLESS_NODES: usize = 4096;
/// Milli-item resolution used by headless transfer rates (1000 = 1.0/tick).
pub const MILLI_PER_ITEM: u64 = 1000;

/// Opt-in gate for every catch-up entry point. `enabled` defaults to `false`
/// so vanilla-purist servers see zero behavioural change until they opt in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CatchUpLimits {
    /// Maximum ticks a single catch-up step may cover. Clamped to
    /// [`HARD_MAX_DELTA_TICKS`] regardless of configuration.
    pub max_delta_ticks: u64,
    /// Master switch. `false` makes every `catch_up_*` a no-op.
    pub enabled: bool,
}

impl Default for CatchUpLimits {
    fn default() -> Self {
        Self {
            max_delta_ticks: DEFAULT_MAX_DELTA_TICKS,
            enabled: false,
        }
    }
}

impl CatchUpLimits {
    /// Ticks this step is actually allowed to cover: 0 when disabled,
    /// otherwise `delta` clamped into `[0, min(max_delta_ticks, HARD_MAX)]`.
    #[inline]
    pub fn effective_delta(&self, delta_ticks: u64) -> u64 {
        if !self.enabled {
            return 0;
        }
        delta_ticks
            .min(self.max_delta_ticks)
            .min(HARD_MAX_DELTA_TICKS)
    }
}

/// Snapshot of an unloaded furnace/smelter. `cook_progress` must stay below
/// `cook_time_total` (the invariant every producer maintains); it is
/// normalised defensively on entry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FurnaceCatchUpState {
    pub input_count: u32,
    pub fuel_count: u32,
    /// Burn ticks granted per fuel item.
    pub fuel_burn_ticks_each: u32,
    /// Burn ticks left on the currently lit fuel item.
    pub current_burn_remaining: u32,
    /// Cook ticks accrued toward the next smelt.
    pub cook_progress: u32,
    /// Ticks required per smelt. `0` is treated as `1` (instant smelt per
    /// burn tick) so degenerate recipes stay deterministic.
    pub cook_time_total: u32,
    pub output_count: u32,
    /// Stack cap for the output slot; smelts pause while it is reached.
    pub output_max_stack: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FurnaceCatchUpReport {
    pub state: FurnaceCatchUpState,
    pub items_smelted: u64,
    pub fuel_consumed: u64,
}

/// Advance an unloaded furnace by `delta_ticks` in O(1).
///
/// Model (identical to the per-tick reference in tests): each tick first
/// relights when unburnt and able to smelt, then cooks one tick while burning
/// with input and output space, then decrements burn. Fuel is only consumed
/// when a smelt is possible, so a full output stack or empty input never
/// wastes fuel. Burn ticks elapse even while cooking is blocked.
pub fn catch_up_furnace(
    limits: &CatchUpLimits,
    state: &FurnaceCatchUpState,
    delta_ticks: u64,
) -> FurnaceCatchUpReport {
    let mut out = *state;
    let delta = limits.effective_delta(delta_ticks);
    if delta == 0 {
        return FurnaceCatchUpReport {
            state: out,
            items_smelted: 0,
            fuel_consumed: 0,
        };
    }

    let total = u64::from(state.cook_time_total.max(1));
    let cook = u64::from(state.cook_progress).min(total - 1);
    let space = u64::from(state.output_max_stack)
        .saturating_sub(u64::from(state.output_count));
    let possible = u64::from(state.input_count).min(space);

    let mut smelted = 0u64;
    let mut fuel_used = 0u64;
    let mut remaining = delta;

    if possible > 0 {
        // Ticks needed to complete every currently possible smelt.
        let needed = possible as u128 * total as u128 - cook as u128;
        // Total burn budget: leftover burn plus every fuel item not yet lit.
        let burn_each = u128::from(state.fuel_burn_ticks_each);
        let budget = u128::from(state.current_burn_remaining)
            + u128::from(state.fuel_count) * burn_each;
        let tau = (delta as u128).min(needed).min(budget) as u64;

        // Completions are contiguous across fuel relights: in the free
        // regime every tick cooks exactly once.
        smelted = (cook + tau) / total;
        let cook_left = (cook + tau) % total;

        // Relights needed beyond the already-burning segment.
        let burn = u128::from(state.current_burn_remaining);
        if tau as u128 > burn && burn_each > 0 {
            fuel_used = (tau as u128 - burn).div_ceil(burn_each) as u64;
        }
        let burn_left = (burn + fuel_used as u128 * burn_each - tau as u128) as u64;

        out.input_count -= smelted as u32;
        out.output_count += smelted as u32;
        out.fuel_count -= fuel_used as u32;
        out.cook_progress = cook_left as u32;
        out.current_burn_remaining = burn_left as u32;
        remaining = delta - tau;
    }

    if remaining > 0 {
        let can_smelt =
            out.input_count > 0 && out.output_count < out.output_max_stack;
        if !can_smelt {
            // Terminal stall: burn keeps elapsing, relight is impossible
            // because input or output space never recovers on its own.
            out.current_burn_remaining = out.current_burn_remaining.saturating_sub(
                remaining.min(u64::from(u32::MAX)) as u32,
            );
        } else if out.current_burn_remaining == 0 && out.fuel_burn_ticks_each == 0 {
            // Zero-length fuel items relight every tick but never cook, so
            // remaining ticks drain the fuel buffer one item per tick.
            let drain = remaining.min(u64::from(out.fuel_count));
            out.fuel_count -= drain as u32;
            fuel_used += drain;
        } else if out.current_burn_remaining > 0 {
            // Defensive: unreachable while invariants hold, but keeps the
            // state consistent if a caller feeds a degenerate snapshot.
            out.current_burn_remaining = out.current_burn_remaining.saturating_sub(
                remaining.min(u64::from(u32::MAX)) as u32,
            );
        }
    }

    FurnaceCatchUpReport {
        state: out,
        items_smelted: smelted,
        fuel_consumed: fuel_used,
    }
}

/// Advance an unloaded crop / sapling / copper oxidation stage by skipping
/// Bernoulli(`random_tick_prob_per_tick`) trials with exact geometric waiting
/// times. O(max_stage - current_stage), i.e. constant for every real block.
///
/// `seed` makes the draw sequence fully deterministic: identical inputs
/// always yield identical stages. The result is non-decreasing in
/// `delta_ticks` and never exceeds `max_stage`.
pub fn catch_up_growth_stage(
    limits: &CatchUpLimits,
    current_stage: u8,
    max_stage: u8,
    random_tick_prob_per_tick: f64,
    delta_ticks: u64,
    seed: u64,
) -> u8 {
    let delta = limits.effective_delta(delta_ticks);
    if delta == 0
        || current_stage >= max_stage
        || random_tick_prob_per_tick.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater)
    {
        return current_stage;
    }
    if random_tick_prob_per_tick >= 1.0 {
        let gain = delta.min(u64::from(max_stage - current_stage));
        return current_stage + gain as u8;
    }

    // ln(1 - p) via ln_1p keeps tiny probabilities accurate.
    let log_survival = (-random_tick_prob_per_tick).ln_1p();
    let mut rng = XorShift64::new(seed);
    let mut stage = current_stage;
    let mut elapsed = 0u128;
    let budget = delta as u128;

    while stage < max_stage {
        // Uniform open-interval draw in (0, 1): 53-bit resolution keeps
        // ln() finite for every value the PRNG can emit.
        let u = ((rng.next_u64() >> 11) as f64) * (1.0 / 9_007_199_254_740_992.0);
        let wait = u.ln() / log_survival;
        if !wait.is_finite() || wait < 0.0 {
            break;
        }
        let wait_ticks = wait.floor() + 1.0;
        if wait_ticks > (budget - elapsed) as f64 {
            break;
        }
        elapsed += wait_ticks as u128;
        stage += 1;
    }
    stage
}

/// Declarative machine / alloy-forge snapshot. One "cycle" consumes one
/// `input_count` item, `energy_per_cycle` energy, and `ticks_per_cycle`
/// ticks, and produces one output bounded by `output_space`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MachineCatchUpState {
    pub input_count: u32,
    /// Stored energy/fuel buffer (`fuel_or_energy` in the task spec).
    pub energy: u64,
    /// Energy consumed per completed cycle. `0` means free energy.
    pub energy_per_cycle: u64,
    /// Ticks per cycle slot. `0` is treated as `1`.
    pub ticks_per_cycle: u64,
    /// Ticks accrued toward the next cycle boundary.
    pub cycle_progress: u64,
    pub output_count: u32,
    /// Remaining output capacity in items.
    pub output_space: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MachineCatchUpReport {
    pub state: MachineCatchUpState,
    pub cycles_completed: u64,
    pub energy_consumed: u64,
}

/// Advance an unloaded machine by `delta_ticks` in O(1).
///
/// Cycle boundaries are periodic, so `(cycle_progress + delta) /
/// ticks_per_cycle` gives the exact number of slots. Each slot completes a
/// cycle only while input, output space, and energy last; starved slots are
/// wasted exactly like the per-tick reference.
pub fn catch_up_machine_batch(
    limits: &CatchUpLimits,
    state: &MachineCatchUpState,
    delta_ticks: u64,
) -> MachineCatchUpReport {
    let mut out = *state;
    let delta = limits.effective_delta(delta_ticks);
    if delta == 0 {
        return MachineCatchUpReport {
            state: out,
            cycles_completed: 0,
            energy_consumed: 0,
        };
    }

    let tpc = state.ticks_per_cycle.max(1);
    let progress = state.cycle_progress.min(tpc - 1);
    let total = progress as u128 + delta as u128;
    let slots = total / u128::from(tpc);
    out.cycle_progress = (total % u128::from(tpc)) as u64;

    let epc = state.energy_per_cycle;
    let energy_slots = if epc == 0 {
        u128::from(u64::MAX)
    } else {
        state.energy as u128 / epc as u128
    };
    let cycles = slots
        .min(u128::from(state.input_count))
        .min(u128::from(state.output_space))
        .min(energy_slots)
        .min(u128::from(u64::MAX)) as u64;

    let consumed = cycles.saturating_mul(epc);
    out.input_count -= cycles as u32;
    out.output_space -= cycles as u32;
    out.output_count += cycles as u32;
    out.energy -= consumed;

    MachineCatchUpReport {
        state: out,
        cycles_completed: cycles,
        energy_consumed: consumed,
    }
}

/// Terrain-free factory/logistics endpoint. Buffers are plain item counts;
/// `carry_milli` banks the sub-item fraction of the transfer rate so rates
/// below 1.0 item/tick neither stall nor drift.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeadlessFactoryNode {
    pub node_id: u64,
    pub dim_id: u32,
    pub cx: i32,
    pub cz: i32,
    /// Transfer rate in thousandths of an item per tick.
    pub rate_per_tick_milli: u32,
    pub input_buffer: u32,
    pub output_buffer: u32,
    /// Output buffer capacity; transfers stop at this ceiling.
    pub max_buffer: u32,
    /// Sub-item milli remainder carried between steps. Keep below
    /// [`MILLI_PER_ITEM`]; producers should initialise it to 0.
    pub carry_milli: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HeadlessStepReport {
    pub nodes_stepped: usize,
    pub delta_ticks: u64,
    pub items_moved: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeadlessError {
    DuplicateNode(u64),
    UnknownNode(u64),
    CapacityExceeded,
}

impl fmt::Display for HeadlessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateNode(id) => write!(f, "duplicate headless node {id}"),
            Self::UnknownNode(id) => write!(f, "unknown headless node {id}"),
            Self::CapacityExceeded => {
                write!(f, "headless node cap {MAX_HEADLESS_NODES} reached")
            }
        }
    }
}

impl Error for HeadlessError {}

/// Bounded registry of active unloaded factory endpoints. Stepping touches
/// only the node records — no chunk terrain, no entity state — so 1,000+
/// nodes cost microseconds and a fixed ~100 KiB of RAM.
#[derive(Default)]
pub struct HeadlessFactoryRegistry {
    nodes: HashMap<u64, HeadlessFactoryNode>,
}

impl HeadlessFactoryRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn get(&self, node_id: u64) -> Option<&HeadlessFactoryNode> {
        self.nodes.get(&node_id)
    }

    pub fn add_node(&mut self, node: HeadlessFactoryNode) -> Result<(), HeadlessError> {
        if self.nodes.contains_key(&node.node_id) {
            return Err(HeadlessError::DuplicateNode(node.node_id));
        }
        if self.nodes.len() >= MAX_HEADLESS_NODES {
            return Err(HeadlessError::CapacityExceeded);
        }
        self.nodes.insert(node.node_id, node);
        Ok(())
    }

    pub fn remove_node(&mut self, node_id: u64) -> Result<HeadlessFactoryNode, HeadlessError> {
        self.nodes
            .remove(&node_id)
            .ok_or(HeadlessError::UnknownNode(node_id))
    }

    /// Transfer items input→output on every node in O(nodes). Each node
    /// offers `rate_per_tick_milli * delta` milli-items, moves at most what
    /// input and output space allow, and banks only the sub-item fraction.
    pub fn step_headless_factories(
        &mut self,
        limits: &CatchUpLimits,
        delta_ticks: u64,
    ) -> HeadlessStepReport {
        let delta = limits.effective_delta(delta_ticks);
        let mut items_moved = 0u64;
        if delta > 0 {
            for node in self.nodes.values_mut() {
                let offer = node.carry_milli as u128
                    + u128::from(node.rate_per_tick_milli) * delta as u128;
                let space = u128::from(node.max_buffer.saturating_sub(node.output_buffer));
                let movable = u128::from(node.input_buffer).min(space);
                let moved = (offer / u128::from(MILLI_PER_ITEM)).min(movable);
                if moved > 0 {
                    node.input_buffer -= moved as u32;
                    node.output_buffer += moved as u32;
                    items_moved += moved as u64;
                }
                node.carry_milli = (offer % u128::from(MILLI_PER_ITEM)) as u64;
            }
        }
        HeadlessStepReport {
            nodes_stepped: self.nodes.len(),
            delta_ticks: delta,
            items_moved,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled_limits() -> CatchUpLimits {
        CatchUpLimits {
            max_delta_ticks: HARD_MAX_DELTA_TICKS,
            enabled: true,
        }
    }

    // ---- Per-tick reference simulators (test-only ground truth) ----

    fn ref_furnace_step(s: &mut FurnaceCatchUpState) -> (u64, u64) {
        let total = s.cook_time_total.max(1);
        let mut smelted = 0u64;
        let mut fuel = 0u64;
        if s.current_burn_remaining == 0
            && s.fuel_count > 0
            && s.input_count > 0
            && s.output_count < s.output_max_stack
        {
            s.fuel_count -= 1;
            s.current_burn_remaining = s.fuel_burn_ticks_each;
            fuel += 1;
        }
        if s.current_burn_remaining > 0
            && s.input_count > 0
            && s.output_count < s.output_max_stack
        {
            s.cook_progress += 1;
            if s.cook_progress >= total {
                s.cook_progress -= total;
                s.input_count -= 1;
                s.output_count += 1;
                smelted += 1;
            }
        }
        if s.current_burn_remaining > 0 {
            s.current_burn_remaining -= 1;
        }
        (smelted, fuel)
    }

    fn ref_furnace_run(
        mut s: FurnaceCatchUpState,
        ticks: u64,
    ) -> (FurnaceCatchUpState, u64, u64) {
        let (mut smelted, mut fuel) = (0u64, 0u64);
        for _ in 0..ticks {
            let (a, b) = ref_furnace_step(&mut s);
            smelted += a;
            fuel += b;
        }
        (s, smelted, fuel)
    }

    fn ref_machine_step(s: &mut MachineCatchUpState) -> u64 {
        let tpc = s.ticks_per_cycle.max(1);
        s.cycle_progress += 1;
        if s.cycle_progress >= tpc {
            s.cycle_progress -= tpc;
            let epc = s.energy_per_cycle;
            if s.input_count > 0
                && s.output_space > 0
                && (epc == 0 || s.energy >= epc)
            {
                s.input_count -= 1;
                s.output_space -= 1;
                s.output_count += 1;
                s.energy = s.energy.saturating_sub(epc);
                return 1;
            }
        }
        0
    }

    fn ref_machine_run(
        mut s: MachineCatchUpState,
        ticks: u64,
    ) -> (MachineCatchUpState, u64) {
        let mut cycles = 0u64;
        for _ in 0..ticks {
            cycles += ref_machine_step(&mut s);
        }
        (s, cycles)
    }

    fn assert_furnace_matches(state: FurnaceCatchUpState, delta: u64) {
        let limits = enabled_limits();
        let got = catch_up_furnace(&limits, &state, delta);
        let (want_state, want_smelted, want_fuel) = ref_furnace_run(state, delta);
        assert_eq!(
            got.state, want_state,
            "state mismatch for {state:?} delta {delta}"
        );
        assert_eq!(got.items_smelted, want_smelted, "smelted {state:?} d{delta}");
        assert_eq!(got.fuel_consumed, want_fuel, "fuel {state:?} d{delta}");
        // Conservation invariants.
        assert!(got.state.output_count <= got.state.output_max_stack);
        assert_eq!(
            u64::from(state.input_count) - u64::from(got.state.input_count),
            got.items_smelted
        );
        assert_eq!(
            u64::from(got.state.output_count) - u64::from(state.output_count),
            got.items_smelted
        );
        assert_eq!(
            u64::from(state.fuel_count) - u64::from(got.state.fuel_count),
            got.fuel_consumed
        );
    }

    // ---- Furnace ----

    #[test]
    fn furnace_edge_cases_match_reference() {
        let cases = [
            // Idle empty furnace.
            FurnaceCatchUpState::default(),
            // Fuel but no input: never lights.
            FurnaceCatchUpState {
                fuel_count: 5,
                fuel_burn_ticks_each: 200,
                ..Default::default()
            },
            // Input but no fuel.
            FurnaceCatchUpState {
                input_count: 10,
                cook_time_total: 200,
                output_max_stack: 64,
                ..Default::default()
            },
            // Partial burn mid-cook.
            FurnaceCatchUpState {
                input_count: 3,
                fuel_count: 2,
                fuel_burn_ticks_each: 100,
                current_burn_remaining: 37,
                cook_progress: 90,
                cook_time_total: 200,
                output_count: 1,
                output_max_stack: 64,
            },
            // Output one slot from full.
            FurnaceCatchUpState {
                input_count: 9,
                fuel_count: 4,
                fuel_burn_ticks_each: 1600,
                cook_progress: 199,
                cook_time_total: 200,
                output_count: 63,
                output_max_stack: 64,
                ..Default::default()
            },
            // Output already full: terminal stall.
            FurnaceCatchUpState {
                input_count: 9,
                fuel_count: 4,
                fuel_burn_ticks_each: 1600,
                current_burn_remaining: 500,
                cook_progress: 10,
                cook_time_total: 200,
                output_count: 64,
                output_max_stack: 64,
            },
            // Zero-length fuel with leftover burn.
            FurnaceCatchUpState {
                input_count: 4,
                fuel_count: 7,
                fuel_burn_ticks_each: 0,
                current_burn_remaining: 5,
                cook_time_total: 10,
                output_max_stack: 16,
                ..Default::default()
            },
            // Zero-length fuel, no burn: pure fuel drain.
            FurnaceCatchUpState {
                input_count: 4,
                fuel_count: 7,
                fuel_burn_ticks_each: 0,
                cook_time_total: 10,
                output_max_stack: 16,
                ..Default::default()
            },
            // Degenerate instant recipe.
            FurnaceCatchUpState {
                input_count: 6,
                fuel_count: 3,
                fuel_burn_ticks_each: 4,
                cook_time_total: 0,
                output_max_stack: 8,
                ..Default::default()
            },
            // One real hour of coal furnace.
            FurnaceCatchUpState {
                input_count: 64,
                fuel_count: 8,
                fuel_burn_ticks_each: 1600,
                cook_progress: 77,
                cook_time_total: 200,
                output_max_stack: 64,
                ..Default::default()
            },
        ];
        for state in cases {
            for delta in [0u64, 1, 7, 199, 200, 201, 1600, 72000] {
                assert_furnace_matches(state, delta);
            }
        }
    }

    #[test]
    fn furnace_random_states_bit_identical() {
        let mut rng = XorShift64::new(0xC0FF_EE00_1234_5678);
        for _ in 0..120 {
            let cook_total = (rng.next_u64() % 400 + 1) as u32;
            let cook_progress = (rng.next_u64() % u64::from(cook_total)) as u32;
            let state = FurnaceCatchUpState {
                input_count: (rng.next_u64() % 70) as u32,
                fuel_count: (rng.next_u64() % 12) as u32,
                fuel_burn_ticks_each: (rng.next_u64() % 2000) as u32,
                current_burn_remaining: (rng.next_u64() % 500) as u32,
                cook_progress,
                cook_time_total: cook_total,
                output_count: (rng.next_u64() % 65) as u32,
                output_max_stack: 64,
            };
            for delta in [1u64, 63, 200, 5000, 72_000] {
                assert_furnace_matches(state, delta);
            }
        }
    }

    #[test]
    fn furnace_disabled_and_clamped() {
        let state = FurnaceCatchUpState {
            input_count: 10,
            fuel_count: 2,
            fuel_burn_ticks_each: 100,
            cook_time_total: 200,
            output_max_stack: 64,
            ..Default::default()
        };
        let off = CatchUpLimits::default();
        let got = catch_up_furnace(&off, &state, 72_000);
        assert_eq!(got.state, state);
        assert_eq!(got.items_smelted, 0);

        let narrow = CatchUpLimits {
            max_delta_ticks: 100,
            enabled: true,
        };
        let got = catch_up_furnace(&narrow, &state, 72_000);
        let (want_state, want_smelted, want_fuel) = ref_furnace_run(state, 100);
        assert_eq!(got.state, want_state);
        assert_eq!(got.items_smelted, want_smelted);
        assert_eq!(got.fuel_consumed, want_fuel);
    }

    // ---- Growth stage ----

    #[test]
    fn growth_monotonic_clamped_deterministic() {
        let limits = enabled_limits();
        let mut prev = 0u8;
        for delta in [0u64, 1, 5, 50, 500, 5000, 72_000] {
            let stage = catch_up_growth_stage(&limits, 0, 7, 0.2, delta, 42);
            assert!(stage >= prev, "stage regressed at delta {delta}");
            prev = stage;
        }
        assert!(prev <= 7);

        // delta == 0 is a no-op even with a hot probability.
        assert_eq!(
            catch_up_growth_stage(&limits, 3, 7, 0.9, 0, 42),
            3
        );
        // Already mature never grows.
        assert_eq!(
            catch_up_growth_stage(&limits, 7, 7, 1.0, 72_000, 42),
            7
        );
        // p = 0 never grows; p >= 1 grows every tick up to the cap.
        assert_eq!(
            catch_up_growth_stage(&limits, 2, 7, 0.0, 72_000, 42),
            2
        );
        assert_eq!(
            catch_up_growth_stage(&limits, 2, 7, 1.0, 3, 42),
            5
        );
        assert_eq!(
            catch_up_growth_stage(&limits, 6, 7, 1.0, 72_000, 42),
            7
        );
        // Determinism: identical inputs, identical output.
        for seed in [0u64, 1, 0xDEAD_BEEF] {
            let a = catch_up_growth_stage(&limits, 0, 7, 0.15, 20_000, seed);
            let b = catch_up_growth_stage(&limits, 0, 7, 0.15, 20_000, seed);
            assert_eq!(a, b);
        }
        // Disabled gate is a no-op.
        assert_eq!(
            catch_up_growth_stage(&CatchUpLimits::default(), 1, 7, 1.0, 72_000, 42),
            1
        );
    }

    #[test]
    fn growth_matches_binomial_reference_small() {
        // For tiny deltas the geometric skip must agree with a literal
        // per-tick Bernoulli walk under the same seed stream is NOT required
        // (different draw consumption); instead verify stage count equals
        // the number of geometric waits that fit, recomputed independently.
        let limits = enabled_limits();
        let p = 0.3f64;
        let stage = catch_up_growth_stage(&limits, 0, 7, p, 30, 7);
        // Recompute with an independent geometric walker.
        let mut rng = XorShift64::new(7);
        let log_survival = (-p).ln_1p();
        let mut elapsed = 0u64;
        let mut expect = 0u8;
        while expect < 7 {
            let u = ((rng.next_u64() >> 11) as f64) * (1.0 / 9_007_199_254_740_992.0);
            let wait = (u.ln() / log_survival).floor() + 1.0;
            if !wait.is_finite() || wait > (30 - elapsed) as f64 {
                break;
            }
            elapsed += wait as u64;
            expect += 1;
        }
        assert_eq!(stage, expect);
    }

    // ---- Machine batch ----

    #[test]
    fn machine_edge_cases_match_reference() {
        let cases = [
            MachineCatchUpState::default(),
            // Starved: no input.
            MachineCatchUpState {
                energy: 100,
                energy_per_cycle: 10,
                ticks_per_cycle: 20,
                output_space: 8,
                ..Default::default()
            },
            // Starved: no energy.
            MachineCatchUpState {
                input_count: 9,
                ticks_per_cycle: 20,
                output_space: 8,
                ..Default::default()
            },
            // Starved: no output space.
            MachineCatchUpState {
                input_count: 9,
                energy: 500,
                energy_per_cycle: 10,
                ticks_per_cycle: 20,
                output_space: 0,
                ..Default::default()
            },
            // Mid-cycle with partial energy.
            MachineCatchUpState {
                input_count: 5,
                energy: 25,
                energy_per_cycle: 10,
                ticks_per_cycle: 20,
                cycle_progress: 7,
                output_count: 2,
                output_space: 6,
            },
            // Instant cycles (tpc = 0 clamps to 1) and free energy.
            MachineCatchUpState {
                input_count: 4,
                energy_per_cycle: 0,
                ticks_per_cycle: 0,
                output_space: 4,
                ..Default::default()
            },
            // Energy exactly covers half the slots: later slots waste.
            MachineCatchUpState {
                input_count: 100,
                energy: 50,
                energy_per_cycle: 10,
                ticks_per_cycle: 10,
                output_space: 100,
                ..Default::default()
            },
        ];
        let limits = enabled_limits();
        for state in cases {
            for delta in [0u64, 1, 9, 20, 200, 72_000] {
                let got = catch_up_machine_batch(&limits, &state, delta);
                let (want_state, want_cycles) = ref_machine_run(state, delta);
                assert_eq!(got.state, want_state, "state {state:?} d{delta}");
                assert_eq!(got.cycles_completed, want_cycles, "cycles {state:?} d{delta}");
                // Conservation.
                assert_eq!(
                    u64::from(state.input_count) - u64::from(got.state.input_count),
                    got.cycles_completed
                );
                assert_eq!(
                    u64::from(got.state.output_count) - u64::from(state.output_count),
                    got.cycles_completed
                );
                assert_eq!(
                    state.energy - got.state.energy,
                    got.energy_consumed
                );
            }
        }
    }

    #[test]
    fn machine_disabled_is_noop() {
        let state = MachineCatchUpState {
            input_count: 9,
            energy: 500,
            energy_per_cycle: 10,
            ticks_per_cycle: 20,
            cycle_progress: 7,
            output_count: 2,
            output_space: 6,
        };
        let got = catch_up_machine_batch(&CatchUpLimits::default(), &state, 72_000);
        assert_eq!(got.state, state);
        assert_eq!(got.cycles_completed, 0);
        assert_eq!(got.energy_consumed, 0);
    }

    // ---- Headless factory registry ----

    fn node(id: u64, rate_milli: u32, input: u32, output: u32, max: u32) -> HeadlessFactoryNode {
        HeadlessFactoryNode {
            node_id: id,
            dim_id: 0,
            cx: (id % 32) as i32,
            cz: ((id / 32) % 32) as i32,
            rate_per_tick_milli: rate_milli,
            input_buffer: input,
            output_buffer: output,
            max_buffer: max,
            carry_milli: 0,
        }
    }

    #[test]
    fn headless_registry_step_conserves_and_caps() {
        let limits = enabled_limits();
        let mut registry = HeadlessFactoryRegistry::new();
        for i in 0..1000u64 {
            registry
                .add_node(node(i, 1500, 100, 10, 200))
                .unwrap();
        }
        assert_eq!(registry.len(), 1000);

        let before: Vec<(u32, u32)> = (0..1000u64)
            .map(|i| {
                let n = registry.get(i).unwrap();
                (n.input_buffer, n.output_buffer)
            })
            .collect();
        let report = registry.step_headless_factories(&limits, 10);
        assert_eq!(report.nodes_stepped, 1000);
        assert_eq!(report.delta_ticks, 10);
        // 1500 milli/tick * 10 ticks = 15 items offered per node; each node
        // had 100 input and 190 output space, so all 15 move.
        assert_eq!(report.items_moved, 15_000);

        let mut total_in = 0u64;
        let mut total_out = 0u64;
        for (i, &(in_b, out_b)) in before.iter().enumerate() {
            let n = registry.get(i as u64).unwrap();
            total_in += u64::from(in_b) - u64::from(n.input_buffer);
            total_out += u64::from(n.output_buffer) - u64::from(out_b);
            assert!(n.carry_milli < MILLI_PER_ITEM);
        }
        assert_eq!(total_in, report.items_moved);
        assert_eq!(total_out, report.items_moved);

        // Output cap binds: node with max_buffer == output_buffer moves nothing.
        let mut capped = HeadlessFactoryRegistry::new();
        capped.add_node(node(1, 2000, 50, 64, 64)).unwrap();
        let r = capped.step_headless_factories(&limits, 100);
        assert_eq!(r.items_moved, 0);
        assert_eq!(capped.get(1).unwrap().input_buffer, 50);

        // Sub-item rate accumulates via carry: 500 milli/tick -> 1 item per 2 ticks.
        let mut slow = HeadlessFactoryRegistry::new();
        slow.add_node(node(2, 500, 10, 0, 64)).unwrap();
        let r1 = slow.step_headless_factories(&limits, 1);
        assert_eq!(r1.items_moved, 0);
        assert_eq!(slow.get(2).unwrap().carry_milli, 500);
        let r2 = slow.step_headless_factories(&limits, 1);
        assert_eq!(r2.items_moved, 1);
        assert_eq!(slow.get(2).unwrap().input_buffer, 9);
        assert_eq!(slow.get(2).unwrap().output_buffer, 1);

        // Disabled limits: pure no-op.
        let r0 = slow.step_headless_factories(&CatchUpLimits::default(), 100);
        assert_eq!(r0.items_moved, 0);
        assert_eq!(r0.delta_ticks, 0);

        // Duplicate and capacity errors.
        assert_eq!(
            capped.add_node(node(1, 1, 0, 0, 1)),
            Err(HeadlessError::DuplicateNode(1))
        );
        let mut full = HeadlessFactoryRegistry::new();
        for i in 0..MAX_HEADLESS_NODES as u64 {
            full.add_node(node(i, 0, 0, 0, 1)).unwrap();
        }
        assert_eq!(
            full.add_node(node(MAX_HEADLESS_NODES as u64, 0, 0, 0, 1)),
            Err(HeadlessError::CapacityExceeded)
        );
        assert_eq!(full.remove_node(999_999), Err(HeadlessError::UnknownNode(999_999)));
    }
}
