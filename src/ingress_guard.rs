//! Ingress bounds that vanilla itself applies, at the frame boundary.
//!
//! Two of them, both taken from the protocol rather than invented:
//!
//! - `MAX_PLAY_FRAME` is vanilla's `Varint21FrameDecoder` limit: the length
//!   prefix is at most three bytes, so a frame cannot exceed 2 MiB - 1. This
//!   port allowed 16 MiB, which is 8x what any client the server claims to
//!   support will ever send.
//! - `valid_movement_frame` rejects the position values vanilla refuses before
//!   they reach game state: non-finite coordinates and |x|,|y|,|z| beyond the
//!   world border limit. A NaN position otherwise propagates into worldgen,
//!   visibility, pathfinding and every float-to-int conversion on its way.
//!
//! Deliberately *not* here: a token bucket over gameplay packets. Vanilla has
//! none, and dropping a legitimate burst is a visible divergence from the
//! behaviour this port exists to preserve. The branch this was taken from had a
//! token bucket; it was left there with no measurement to justify it.
//!
//! The one packet-rate bound that does exist is a per-tick ceiling on
//! movement, not a rate limit: `MovementFloodGuard` admits at most
//! `MAX_MOVES_PER_TICK` (10) `0x1a`/`0x1b` frames per connection per server
//! tick and resets the counter every tick. A real client sends movement at
//! ~20 Hz, so it lands one or two rotations per tick - an order of magnitude
//! below the cap, which therefore cannot drop a legitimate burst. It exists so
//! a single client cannot pipeline thousands of movement frames into one tick
//! and amplify the anti-cheat scans and tracker fanout each one triggers.
//! Position/rotation frames are the *only* packets under such a cap: block
//! place/break, interaction, chat and inventory still have no packet-rate
//! limit.
//!
//! That is not the same as "no limits at all", and an earlier version of this
//! comment overstated it. The remaining limits are connection- and abuse-level
//! and live in `connection_limits.rs`: a per-IP concurrent connection cap, a
//! handshake/login/configuration deadline, a play-state idle deadline,
//! vanilla's own chat length rule and spam counter (the only per-message
//! throttle vanilla has), and RCON failed-password lockout. There is still *no*
//! per-IP connection *rate* limit; put a firewall / fail2ban in front of a
//! public listener.

/// Vanilla's limit: a three-byte VarInt length prefix, so at most 2^21 - 1 bytes.
pub(crate) const MAX_PLAY_FRAME: usize = 2 * 1024 * 1024 - 1;

/// Vanilla's world border limit; a position outside it is not reachable and is
/// not accepted as a movement update.
const WORLD_BOUND: f64 = 30_000_000.0;

/// Java 1.21.1 serverbound movement packets: 0x1a `move_player_pos` (x, y, z,
/// flags) and 0x1b `move_player_pos_rot` (x, y, z, yaw, pitch, flags).
pub(crate) const MOVE_POS: i64 = 0x1a;
pub(crate) const MOVE_POS_ROT: i64 = 0x1b;

pub(crate) fn valid_movement_frame(packet_id: i64, body: &[u8]) -> bool {
    if packet_id != MOVE_POS && packet_id != MOVE_POS_ROT {
        return true;
    }
    // 24 bytes of position + flags, plus 8 when the angles are present.
    let required = if packet_id == MOVE_POS_ROT { 33 } else { 25 };
    if body.len() != required {
        return false;
    }
    let f64_at = |o: usize| {
        body.get(o..o + 8)
            .and_then(|s| s.try_into().ok())
            .map(f64::from_be_bytes)
    };
    for offset in [0, 8, 16] {
        let Some(v) = f64_at(offset) else {
            return false;
        };
        if !v.is_finite() || v.abs() > WORLD_BOUND {
            return false;
        }
    }
    if packet_id == MOVE_POS_ROT {
        let f32_at = |o: usize| {
            body.get(o..o + 4)
                .and_then(|s| s.try_into().ok())
                .map(f32::from_be_bytes)
        };
        return matches!(
            (f32_at(24), f32_at(28)),
            (Some(yaw), Some(pitch)) if yaw.is_finite() && pitch.is_finite()
        );
    }
    true
}

/// Per-server-tick ceiling on processed movement packets (`0x1a`/`0x1b`) per
/// connection. Generous enough for normal client catch-up bursts while stopping
/// a single client from amplifying anti-cheat scans and tracker fanout.
pub(crate) const MAX_MOVES_PER_TICK: u8 = 10;

pub(crate) struct MovementFloodGuard {
    tick: u64,
    count: u8,
}

impl MovementFloodGuard {
    pub(crate) const fn new() -> Self {
        Self { tick: u64::MAX, count: 0 }
    }

    pub(crate) fn allow_tick(&mut self, server_tick: u64) -> bool {
        if self.tick != server_tick {
            self.tick = server_tick;
            self.count = 1;
            return true;
        }
        if self.count >= MAX_MOVES_PER_TICK {
            return false;
        }
        self.count += 1;
        true
    }
}

pub(crate) fn movement_pose_changed(
    before: (f64, f64, f64, f32, f32, bool),
    after: (f64, f64, f64, f32, f32, bool),
) -> bool {
    before.0.to_bits() != after.0.to_bits()
        || before.1.to_bits() != after.1.to_bits()
        || before.2.to_bits() != after.2.to_bits()
        || before.3.to_bits() != after.3.to_bits()
        || before.4.to_bits() != after.4.to_bits()
        || before.5 != after.5
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(parts: &[f64]) -> Vec<u8> {
        let mut out = Vec::new();
        for p in parts {
            out.extend_from_slice(&p.to_be_bytes());
        }
        out
    }

    fn pos(x: f64, y: f64, z: f64) -> Vec<u8> {
        pos_flags(x, y, z, 0)
    }

    fn pos_flags(x: f64, y: f64, z: f64, flags: u8) -> Vec<u8> {
        let mut b = body(&[x, y, z]);
        b.push(flags);
        b
    }

    /// `0x1b` wire order is x, y, z, **yaw, pitch**, flags - the angles sit
    /// between the position and the flags byte, not after it.
    fn pos_rot(x: f64, y: f64, z: f64, yaw: f32, pitch: f32, flags: u8) -> Vec<u8> {
        let mut b = body(&[x, y, z]);
        b.extend_from_slice(&yaw.to_be_bytes());
        b.extend_from_slice(&pitch.to_be_bytes());
        b.push(flags);
        b
    }

    #[test]
    fn a_normal_position_is_accepted() {
        assert!(valid_movement_frame(MOVE_POS, &pos(0.5, 64.0, -12.5)));
        let rot = pos_rot(0.5, 64.0, -12.5, 90.0, 0.0, 0);
        assert_eq!(rot.len(), 33);
        assert!(valid_movement_frame(MOVE_POS_ROT, &rot));
    }

    #[test]
    fn rotation_angles_must_be_finite() {
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert!(
                !valid_movement_frame(MOVE_POS_ROT, &pos_rot(0.0, 64.0, 0.0, bad, 0.0, 0)),
                "yaw {bad}"
            );
            assert!(
                !valid_movement_frame(MOVE_POS_ROT, &pos_rot(0.0, 64.0, 0.0, 0.0, bad, 0)),
                "pitch {bad}"
            );
        }
    }

    #[test]
    fn movement_flags_bits_are_not_validated() {
        // The flags byte is a bitfield of which position components changed;
        // every combination is legal and none of them is a bound to enforce.
        for flags in [0x00, 0x01, 0x02, 0x04, 0x08, 0x1f] {
            assert!(
                valid_movement_frame(MOVE_POS, &pos_flags(1.0, 2.0, 3.0, flags)),
                "0x1a flags {flags:#04x}"
            );
            assert!(
                valid_movement_frame(MOVE_POS_ROT, &pos_rot(1.0, 2.0, 3.0, 0.0, 0.0, flags)),
                "0x1b flags {flags:#04x}"
            );
        }
    }

    #[test]
    fn non_finite_coordinates_are_refused() {
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(
                !valid_movement_frame(MOVE_POS, &pos(bad, 64.0, 0.0)),
                "{bad}"
            );
            assert!(
                !valid_movement_frame(MOVE_POS, &pos(0.0, bad, 0.0)),
                "{bad}"
            );
            assert!(
                !valid_movement_frame(MOVE_POS, &pos(0.0, 0.0, bad)),
                "{bad}"
            );
        }
    }

    #[test]
    fn positions_beyond_the_world_border_are_refused() {
        assert!(!valid_movement_frame(
            MOVE_POS,
            &pos(WORLD_BOUND + 1.0, 64.0, 0.0)
        ));
        assert!(!valid_movement_frame(
            MOVE_POS,
            &pos(0.0, 64.0, -(WORLD_BOUND + 1.0))
        ));
        // the border itself is inside the world
        assert!(valid_movement_frame(
            MOVE_POS,
            &pos(WORLD_BOUND, 64.0, -WORLD_BOUND)
        ));
    }

    #[test]
    fn a_short_or_padded_movement_frame_is_refused() {
        let full = pos(1.0, 2.0, 3.0);
        assert_eq!(full.len(), 25);
        assert!(!valid_movement_frame(MOVE_POS, &full[..full.len() - 1]));
        let mut padded = full.clone();
        padded.push(7);
        assert!(!valid_movement_frame(MOVE_POS, &padded));
        // ...but the rotation packet carries four more bytes than the plain one
        let rot = pos_rot(1.0, 2.0, 3.0, 0.0, 0.0, 0);
        assert_eq!(rot.len(), 33);
        assert!(valid_movement_frame(MOVE_POS_ROT, &rot));
        // length is exact in both directions for both packet ids
        assert!(!valid_movement_frame(MOVE_POS_ROT, &rot[..rot.len() - 1]));
        let mut rot_padded = rot.clone();
        rot_padded.push(0);
        assert!(!valid_movement_frame(MOVE_POS_ROT, &rot_padded));
        assert!(!valid_movement_frame(MOVE_POS_ROT, &full));
        assert!(!valid_movement_frame(MOVE_POS, &rot));
    }

    #[test]
    fn other_packets_are_not_move_packets() {
        assert!(valid_movement_frame(0x00, &[]));
        assert!(valid_movement_frame(0x08, &[0, 0, 0, 0]));
    }

    #[test]
    fn movement_flood_guard_caps_single_tick_and_resets_next_tick() {
        let mut g = MovementFloodGuard::new();
        for _ in 0..MAX_MOVES_PER_TICK {
            assert!(g.allow_tick(42));
        }
        assert!(!g.allow_tick(42));
        assert!(!g.allow_tick(42));
        assert!(g.allow_tick(43));
    }

    #[test]
    fn movement_pose_changed_suppresses_identical_idle_updates() {
        let p = (1.5, 64.0, -2.5, 90.0_f32, 0.0_f32, true);
        assert!(!movement_pose_changed(p, p));
        assert!(movement_pose_changed(p, (1.5, 64.0, -2.5, 90.0, 0.0, false)));
        assert!(movement_pose_changed(p, (1.6, 64.0, -2.5, 90.0, 0.0, true)));
    }
}
