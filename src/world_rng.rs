//! The world's random stream.
//!
//! Vanilla keeps a seeded generator as part of the level: the same seed and the same
//! inputs give the same world, and the state is saved with the level. Reading the clock
//! gives neither — nothing can be tested against it, and there is no state to save — so
//! anything whose behaviour depends on randomness has to draw from here instead.
//!
//! Every consumer that used to read the clock draws from here now: random ticks,
//! durability rolls, loot and the other gameplay rolls. The state is written to the
//! save, so a reload continues the same sequence instead of restarting it.

use std::sync::{Mutex, OnceLock};

/// xorshift64*. Small, stateless to describe, and its whole state is the one word that
/// has to be saved.
const GOLDEN: u64 = 0x2545F4914F6CDD1D;
const DEFAULT_SEED: u64 = 0x9E3779B97F4A7C15;

static STATE: OnceLock<Mutex<u64>> = OnceLock::new();

fn state() -> &'static Mutex<u64> {
    STATE.get_or_init(|| Mutex::new(DEFAULT_SEED))
}

/// Seed the stream. Called once at startup with the world seed, so two servers on the
/// same seed and the same inputs make the same rolls.
pub(crate) fn seed(s: u64) {
    let mut st = state().lock().unwrap();
    // A zero state is a fixed point of xorshift, so it must never be reached.
    *st = if s == 0 { DEFAULT_SEED } else { s };
}

/// The current state, for saving.
pub(crate) fn saved_state() -> u64 {
    *state().lock().unwrap()
}

/// Restore a saved state.
pub(crate) fn restore(s: u64) {
    *state().lock().unwrap() = if s == 0 { DEFAULT_SEED } else { s };
}

pub(crate) fn next_u64() -> u64 {
    let mut st = state().lock().unwrap();
    let mut x = *st;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    *st = x;
    x.wrapping_mul(GOLDEN)
}

/// A value in `0..bound`. `bound` of zero has no value to return, so it yields zero
/// rather than dividing by it.
pub(crate) fn next_below(bound: u32) -> u32 {
    if bound == 0 {
        return 0;
    }
    (next_u64() % u64::from(bound)) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    // The stream is one global, so a test that reseeds it is mutating shared state and
    // must hold the same lock the rest of the suite uses for the world.
    fn guard() -> std::sync::MutexGuard<'static, ()> {
        crate::WORLD_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn the_same_seed_gives_the_same_sequence() {
        let _g = guard();
        seed(12345);
        let a: Vec<u64> = (0..8).map(|_| next_u64()).collect();
        seed(12345);
        let b: Vec<u64> = (0..8).map(|_| next_u64()).collect();
        assert_eq!(a, b, "a seeded stream must be reproducible");
    }

    /// A fixed vector pins the algorithm and the draw order, not only reproducibility:
    /// two runs could agree while both used a different generator.
    #[test]
    fn the_stream_is_the_pinned_xorshift64star_vector() {
        let _g = guard();
        seed(12345);
        let got: Vec<u64> = (0..4).map(|_| next_u64()).collect();
        assert_eq!(
            got,
            vec![
                10977518812293740004,
                13893246733018840292,
                1412386850724336324,
                13578198927181985541,
            ]
        );
    }

    #[test]
    fn a_different_seed_gives_a_different_sequence() {
        let _g = guard();
        seed(1);
        let a: Vec<u64> = (0..4).map(|_| next_u64()).collect();
        seed(2);
        let b: Vec<u64> = (0..4).map(|_| next_u64()).collect();
        assert_ne!(a, b);
    }

    #[test]
    fn a_zero_seed_does_not_stick_the_stream() {
        // Zero is a fixed point of xorshift: a stream seeded with it would return zero
        // forever, which is the failure mode worth guarding.
        seed(0);
        let a = next_u64();
        let b = next_u64();
        assert_ne!(a, 0);
        assert_ne!(b, 0);
        assert_ne!(a, b);
    }

    #[test]
    fn next_below_stays_in_range_and_covers_it() {
        let _g = guard();
        seed(7);
        let mut seen = [false; 16];
        for _ in 0..4000 {
            let v = next_below(16);
            assert!(v < 16, "next_below must not exceed its bound");
            seen[v as usize] = true;
        }
        assert!(
            seen.iter().all(|&s| s),
            "every value in 0..16 must occur: {seen:?}"
        );
        assert_eq!(next_below(0), 0, "a zero bound must not divide by zero");
    }

    #[test]
    fn the_state_round_trips() {
        let _g = guard();
        seed(99);
        let _ = next_u64();
        let saved = saved_state();
        let a = next_u64();
        restore(saved);
        assert_eq!(next_u64(), a, "restoring the state must restore the stream");
    }
}
