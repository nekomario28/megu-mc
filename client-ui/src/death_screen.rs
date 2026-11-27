// SPDX-License-Identifier: LGPL-3.0-or-later
#![deny(unsafe_code)]
//! Deterministic state for the vanilla death and respawn screen.

use crate::vanilla_metrics;

/// Player-facing death-screen state supplied by the protocol/session host.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeathScreenState {
    /// Optional translated or server-provided explanation of the death.
    pub cause_message: Option<String>,
    /// Score shown under the death message.
    pub score: i32,
    /// Hardcore worlds do not offer a respawn action.
    pub hardcore: bool,
    /// Remaining server-directed cooldown in game ticks.
    pub respawn_cooldown_ticks: u32,
}

impl DeathScreenState {
    /// Whether the host considers a respawn request legitimate (not
    /// hardcore, and the server-side cooldown has run out). This is only
    /// half the gate — the screen's own anti-misclick delay must also have
    /// elapsed, see [`DeathEnableDelay`].
    pub const fn can_respawn(&self) -> bool {
        !self.hardcore && self.respawn_cooldown_ticks == 0
    }

    /// Advance the server-provided cooldown by one tick without underflow.
    pub fn tick_cooldown(&mut self) {
        self.respawn_cooldown_ticks = self.respawn_cooldown_ticks.saturating_sub(1);
    }
}

/// Client ticks both death buttons stay inert after the screen appears.
///
/// Vanilla's `DeathScreen` starts a `delayTicker` at zero and only enables
/// its buttons at twenty ticks, so the click that killed the player (or a
/// held mouse button) can never turn into an instant respawn.
pub const DEATH_ENABLE_DELAY_TICKS: u32 = vanilla_metrics::DEATH_ENABLE_DELAY_TICKS;

/// The local anti-misclick countdown of one death screen.
///
/// The host resets it with [`DeathEnableDelay::reset`] when a death screen
/// appears and calls [`DeathEnableDelay::tick`] once per client tick while
/// it stays up; the buttons stay disabled until the counter reaches
/// [`DEATH_ENABLE_DELAY_TICKS`]. Keeping the counter separate from
/// [`DeathScreenState`] means the per-frame cause/score refreshes the
/// session sends cannot restart the delay.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DeathEnableDelay {
    ticks: u32,
}

impl DeathEnableDelay {
    /// A fully elapsed delay — the state past the anti-misclick window.
    pub const ELAPSED: Self = Self {
        ticks: DEATH_ENABLE_DELAY_TICKS,
    };

    /// A fresh, still-locked delay: the state a new death screen starts in.
    pub const fn new() -> Self {
        Self { ticks: 0 }
    }

    /// Restart the countdown (a new death screen went up).
    pub fn reset(&mut self) {
        self.ticks = 0;
    }

    /// Advance one client tick, saturating at the delay itself.
    pub fn tick(&mut self) {
        self.ticks = self.ticks.saturating_add(1).min(DEATH_ENABLE_DELAY_TICKS);
    }

    /// Ticks elapsed so far (`0..=DEATH_ENABLE_DELAY_TICKS`).
    pub const fn ticks(self) -> u32 {
        self.ticks
    }

    /// Whether the anti-misclick window has fully elapsed.
    pub const fn enabled(self) -> bool {
        self.ticks >= DEATH_ENABLE_DELAY_TICKS
    }

    /// Ticks still missing before the buttons enable.
    pub const fn remaining(self) -> u32 {
        DEATH_ENABLE_DELAY_TICKS.saturating_sub(self.ticks)
    }
}

#[cfg(test)]
mod tests {
    use super::{DeathEnableDelay, DeathScreenState, DEATH_ENABLE_DELAY_TICKS};

    #[test]
    fn default_state_is_respawnable() {
        assert!(DeathScreenState::default().can_respawn());
    }

    #[test]
    fn cooldown_disables_respawn_until_zero() {
        let mut state = DeathScreenState {
            cause_message: Some("Fell from a high place".into()),
            score: 42,
            hardcore: false,
            respawn_cooldown_ticks: 2,
        };
        assert!(!state.can_respawn());
        state.tick_cooldown();
        assert_eq!(state.respawn_cooldown_ticks, 1);
        assert!(!state.can_respawn());
        state.tick_cooldown();
        assert_eq!(state.respawn_cooldown_ticks, 0);
        assert!(state.can_respawn());
    }

    #[test]
    fn hardcore_always_disables_respawn() {
        let mut state = DeathScreenState {
            hardcore: true,
            ..DeathScreenState::default()
        };
        assert!(!state.can_respawn());
        state.tick_cooldown();
        assert!(!state.can_respawn());
    }

    #[test]
    fn cooldown_saturates_at_zero() {
        let mut state = DeathScreenState::default();
        state.tick_cooldown();
        assert_eq!(state.respawn_cooldown_ticks, 0);
        assert!(state.can_respawn());
    }

    #[test]
    fn message_and_score_are_preserved_as_display_data() {
        let state = DeathScreenState {
            cause_message: Some("slain by a zombie".into()),
            score: 1234,
            hardcore: false,
            respawn_cooldown_ticks: 0,
        };
        assert_eq!(state.cause_message.as_deref(), Some("slain by a zombie"));
        assert_eq!(state.score, 1234);
    }

    #[test]
    fn enable_delay_locks_the_first_twenty_ticks() {
        assert_eq!(DEATH_ENABLE_DELAY_TICKS, 20);
        let mut delay = DeathEnableDelay::new();
        assert!(!delay.enabled());
        assert_eq!(delay.ticks(), 0);
        assert_eq!(delay.remaining(), 20);
        for expected in 1..DEATH_ENABLE_DELAY_TICKS {
            delay.tick();
            assert_eq!(delay.ticks(), expected);
            assert!(!delay.enabled(), "enabled at tick {expected}");
            assert_eq!(delay.remaining(), 20 - expected);
        }
        delay.tick();
        assert_eq!(delay.ticks(), 20);
        assert!(delay.enabled());
        assert_eq!(delay.remaining(), 0);
        // Saturates instead of wrapping, so a long death screen never
        // relocks itself.
        for _ in 0..100 {
            delay.tick();
        }
        assert_eq!(delay.ticks(), DEATH_ENABLE_DELAY_TICKS);
        assert!(delay.enabled());
    }

    #[test]
    fn reset_relocks_a_new_death_screen() {
        let mut delay = DeathEnableDelay::ELAPSED;
        assert!(delay.enabled());
        delay.reset();
        assert!(!delay.enabled());
        assert_eq!(delay, DeathEnableDelay::new());
    }

    #[test]
    fn enable_delay_is_independent_of_the_server_cooldown() {
        // A live respawn (no server cooldown) is still locked for the
        // first twenty ticks by the local delay alone.
        let state = DeathScreenState::default();
        assert!(state.can_respawn());
        assert!(!DeathEnableDelay::new().enabled());
    }
}
