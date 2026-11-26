// SPDX-License-Identifier: LGPL-3.0-or-later
#![forbid(unsafe_code)]
//! Closed-form Vanilla experience curve and shared moon-phase conventions.

/// XP required to advance from `level` to `level + 1` in vanilla Minecraft.
///
/// The three linear segments are exact integer arithmetic: levels `0..=14`
/// increment by 2 starting at 7, `15..=29` increment by 5 starting at 37, and
/// `30+` increment by 9 starting at 112.
pub fn xp_for_next_level(level: u32) -> u64 {
    let level = u64::from(level);
    match level {
        0..=14 => 7 + level * 2,
        15..=29 => 37 + (level - 15) * 5,
        _ => 112 + (level - 30) * 9,
    }
}

fn arithmetic_series(count: u32, first: u64, step: u64) -> u128 {
    let n = u128::from(count);
    if n == 0 {
        return 0;
    }
    n * u128::from(first) + u128::from(step) * n * (n - 1) / 2
}

/// Total XP required to reach `level` from level zero.
///
/// This is the closed form of the iterative prefix sum
/// `sum(xp_for_next_level(0..level))`, evaluated in `O(1)` with `u128`
/// intermediates so the arithmetic itself cannot overflow. The mathematically
/// exact value exceeds `u64` only for extremely large inputs near `u32::MAX`;
/// those unreachable values saturate at `u64::MAX` rather than wrapping.
pub fn total_xp_for_level(level: u32) -> u64 {
    let total = match level {
        0..=15 => arithmetic_series(level, 7, 2),
        16..=30 => arithmetic_series(15, 7, 2) + arithmetic_series(level - 15, 37, 5),
        _ => {
            arithmetic_series(15, 7, 2)
                + arithmetic_series(15, 37, 5)
                + arithmetic_series(level - 30, 112, 9)
        }
    };
    u64::try_from(total).unwrap_or(u64::MAX)
}

/// Vanilla moon phase (`0..=7`) for a world day-time in ticks.
///
/// Java evaluates `(day_time / 24000) mod 8` with floor-style modular
/// semantics; Euclidean division/remainder preserve the same wrapped result
/// for negative times too.
pub const fn moon_phase_from_day_time(day_time: i64) -> u8 {
    day_time.div_euclid(24_000).rem_euclid(8) as u8
}

/// Normalized UV rectangle for one phase in vanilla's 4-by-2
/// `moon_phases.png` atlas, returned as `[u0, v0, u1, v1]`.
///
/// Out-of-range phase bytes wrap the same way the renderer's uniform does.
pub fn moon_atlas_uv(phase: u8) -> [f32; 4] {
    let cell = phase % 8;
    let column = cell % 4;
    let row = cell / 4;
    let u0 = f32::from(column) * 0.25;
    let v0 = f32::from(row) * 0.5;
    [u0, v0, u0 + 0.25, v0 + 0.5]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xp_for_next_level_matches_the_vanilla_segment_boundaries() {
        assert_eq!(xp_for_next_level(0), 7);
        assert_eq!(xp_for_next_level(1), 9);
        assert_eq!(xp_for_next_level(14), 35);
        assert_eq!(xp_for_next_level(15), 37);
        assert_eq!(xp_for_next_level(16), 42);
        assert_eq!(xp_for_next_level(29), 107);
        assert_eq!(xp_for_next_level(30), 112);
        assert_eq!(xp_for_next_level(31), 121);
    }

    #[test]
    fn total_xp_closed_form_matches_iterative_prefix_sum_through_100000() {
        let mut iterative = 0u64;
        for level in 0..=100_000u32 {
            assert_eq!(
                total_xp_for_level(level),
                iterative,
                "total XP mismatch at level {level}"
            );
            iterative += xp_for_next_level(level);
        }
    }

    #[test]
    fn total_xp_matches_the_canonical_vanilla_polynomial_values() {
        assert_eq!(total_xp_for_level(0), 0);
        assert_eq!(total_xp_for_level(1), 7);
        assert_eq!(total_xp_for_level(15), 315);
        assert_eq!(total_xp_for_level(16), 352);
        assert_eq!(total_xp_for_level(17), 394);
        assert_eq!(total_xp_for_level(30), 1395);
        assert_eq!(total_xp_for_level(31), 1507);
        assert_eq!(total_xp_for_level(32), 1628);
    }

    #[test]
    fn moon_phase_follows_vanilla_day_time_wrapping() {
        assert_eq!(moon_phase_from_day_time(0), 0);
        assert_eq!(moon_phase_from_day_time(23_999), 0);
        assert_eq!(moon_phase_from_day_time(24_000), 1);
        assert_eq!(moon_phase_from_day_time(24_000 * 7), 7);
        assert_eq!(moon_phase_from_day_time(24_000 * 8), 0);
        assert_eq!(moon_phase_from_day_time(-1), 7);
        assert_eq!(moon_phase_from_day_time(-24_000), 7);
    }

    #[test]
    fn moon_atlas_uv_selects_the_vanilla_four_by_two_sheet() {
        for phase in 0..8u8 {
            let column = f32::from(phase % 4);
            let row = f32::from(phase / 4);
            assert_eq!(
                moon_atlas_uv(phase),
                [
                    column * 0.25,
                    row * 0.5,
                    (column + 1.0) * 0.25,
                    (row + 1.0) * 0.5
                ]
            );
        }
        assert_eq!(moon_atlas_uv(8), moon_atlas_uv(0));
        assert_eq!(moon_atlas_uv(31), moon_atlas_uv(7));
    }
}
