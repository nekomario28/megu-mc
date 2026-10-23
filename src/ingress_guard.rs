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
//! Deliberately *not* here: a packet rate limit. Vanilla has none, and dropping
//! a legitimate burst is a visible divergence from the behaviour this port
//! exists to preserve. The branch this was taken from had a token bucket; it was
//! left there with no measurement to justify it.

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
    let finite = |o: usize| f64::from_be_bytes(body[o..o + 8].try_into().unwrap()).is_finite();
    for offset in [0, 8, 16] {
        if !finite(offset) {
            return false;
        }
        let v = f64::from_be_bytes(body[offset..offset + 8].try_into().unwrap());
        if v.abs() > WORLD_BOUND {
            return false;
        }
    }
    if packet_id == MOVE_POS_ROT {
        let yaw = f32::from_be_bytes(body[24..28].try_into().unwrap());
        let pitch = f32::from_be_bytes(body[28..32].try_into().unwrap());
        return yaw.is_finite() && pitch.is_finite();
    }
    true
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
        let mut b = body(&[x, y, z]);
        b.push(0); // flags
        b
    }

    #[test]
    fn a_normal_position_is_accepted() {
        assert!(valid_movement_frame(MOVE_POS, &pos(0.5, 64.0, -12.5)));
        let mut b = pos(0.5, 64.0, -12.5);
        b.extend_from_slice(&90f32.to_be_bytes());
        b.extend_from_slice(&0f32.to_be_bytes());
        assert!(valid_movement_frame(MOVE_POS_ROT, &b));
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
        assert!(!valid_movement_frame(MOVE_POS, &full[..full.len() - 1]));
        let mut padded = full.clone();
        padded.push(7);
        assert!(!valid_movement_frame(MOVE_POS, &padded));
        // ...but the rotation packet carries four more bytes than the plain one
        let mut rot = pos(1.0, 2.0, 3.0);
        rot.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0]);
        assert!(valid_movement_frame(MOVE_POS_ROT, &rot));
        assert!(!valid_movement_frame(MOVE_POS_ROT, &full));
    }

    #[test]
    fn other_packets_are_not_move_packets() {
        assert!(valid_movement_frame(0x00, &[]));
        assert!(valid_movement_frame(0x08, &[0, 0, 0, 0]));
    }
}
