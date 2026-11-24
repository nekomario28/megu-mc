// SPDX-License-Identifier: LGPL-3.0-or-later
#![deny(unsafe_code)]
//! Deterministic state for the vanilla death and respawn screen.

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
    /// Whether the UI may enable the respawn action.
    pub const fn can_respawn(&self) -> bool {
        !self.hardcore && self.respawn_cooldown_ticks == 0
    }

    /// Advance the server-provided cooldown by one tick without underflow.
    pub fn tick_cooldown(&mut self) {
        self.respawn_cooldown_ticks = self.respawn_cooldown_ticks.saturating_sub(1);
    }
}

#[cfg(test)]
mod tests {
    use super::DeathScreenState;

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
}
