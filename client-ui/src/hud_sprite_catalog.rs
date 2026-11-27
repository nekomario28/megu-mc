// SPDX-License-Identifier: LGPL-3.0-or-later
//! Vanilla 1.21.1 heart sprite catalog and the animations the HUD applies
//! to it.
//!
//! `textures/gui/sprites/hud/heart/` ships one container per heart family
//! plus a full/half pair per fill, each with a `_blinking` twin that the
//! damage blink swaps in. The layout decides *which* family a row needs;
//! this module owns the exact keys and the pure animation math (blink
//! cadence, regeneration bounce, low-health jitter) so the textured and
//! procedural paths cannot drift apart, and so every key is spelled in one
//! place instead of being scattered through the painter.

use crate::draw::hud_sprite_ids;
use crate::state::HeartStyle;

/// Heart container behind the fills — the empty 9×9 slot.
///
/// Vanilla keeps one container per family: the plain slot, the darker
/// hardcore slot, and the mount slot the vehicle health row uses.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HeartContainer {
    /// `hud/heart/container` — survival, poison, wither, frozen, absorption.
    #[default]
    Normal,
    /// `hud/heart/container_hardcore` — hardcore worlds.
    Hardcore,
    /// `hud/heart/vehicle_container` — the mount health row.
    Vehicle,
}

impl HeartContainer {
    /// Vanilla container key, with the `_blinking` twin for the damage blink.
    pub const fn key(self, blinking: bool) -> &'static str {
        match (self, blinking) {
            (Self::Normal, false) => hud_sprite_ids::HEART_CONTAINER,
            (Self::Normal, true) => "hud/heart/container_blinking",
            (Self::Hardcore, false) => "hud/heart/container_hardcore",
            (Self::Hardcore, true) => "hud/heart/container_hardcore_blinking",
            (Self::Vehicle, false) => "hud/heart/vehicle_container",
            (Self::Vehicle, true) => "hud/heart/vehicle_container_blinking",
        }
    }
}

/// Heart fill family — the coloured heart drawn over the container.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HeartFill {
    /// `hud/heart/full` / `hud/heart/half` — plain red hearts.
    #[default]
    Normal,
    /// `hud/heart/poisoned_*` — Poison.
    Poisoned,
    /// `hud/heart/withered_*` — Wither.
    Withered,
    /// `hud/heart/frozen_*` — powdered-snow freezing.
    Frozen,
    /// `hud/heart/absorbing_*` — the golden absorption row.
    Absorbing,
    /// `hud/heart/hardcore_*` — hardcore worlds.
    Hardcore,
    /// `hud/heart/vehicle_*` — the mount health row.
    Vehicle,
}

impl HeartFill {
    /// Fill family for a HUD [`HeartStyle`].
    pub const fn from_style(style: HeartStyle) -> Self {
        match style {
            HeartStyle::Normal => Self::Normal,
            HeartStyle::Poisoned => Self::Poisoned,
            HeartStyle::Withered => Self::Withered,
            HeartStyle::Frozen => Self::Frozen,
            HeartStyle::Hardcore => Self::Hardcore,
        }
    }

    /// Vanilla fill key; `half` selects the half-heart sprite and
    /// `blinking` the damage-blink twin.
    pub const fn key(self, half: bool, blinking: bool) -> &'static str {
        match (self, half, blinking) {
            (Self::Normal, false, false) => hud_sprite_ids::HEART_FULL,
            (Self::Normal, true, false) => hud_sprite_ids::HEART_HALF,
            (Self::Normal, false, true) => "hud/heart/full_blinking",
            (Self::Normal, true, true) => "hud/heart/half_blinking",
            (Self::Poisoned, false, false) => "hud/heart/poisoned_full",
            (Self::Poisoned, true, false) => "hud/heart/poisoned_half",
            (Self::Poisoned, false, true) => "hud/heart/poisoned_full_blinking",
            (Self::Poisoned, true, true) => "hud/heart/poisoned_half_blinking",
            (Self::Withered, false, false) => "hud/heart/withered_full",
            (Self::Withered, true, false) => "hud/heart/withered_half",
            (Self::Withered, false, true) => "hud/heart/withered_full_blinking",
            (Self::Withered, true, true) => "hud/heart/withered_half_blinking",
            (Self::Frozen, false, false) => "hud/heart/frozen_full",
            (Self::Frozen, true, false) => "hud/heart/frozen_half",
            (Self::Frozen, false, true) => "hud/heart/frozen_full_blinking",
            (Self::Frozen, true, true) => "hud/heart/frozen_half_blinking",
            (Self::Absorbing, false, false) => "hud/heart/absorbing_full",
            (Self::Absorbing, true, false) => "hud/heart/absorbing_half",
            (Self::Absorbing, false, true) => "hud/heart/absorbing_full_blinking",
            (Self::Absorbing, true, true) => "hud/heart/absorbing_half_blinking",
            (Self::Hardcore, false, false) => "hud/heart/hardcore_full",
            (Self::Hardcore, true, false) => "hud/heart/hardcore_half",
            (Self::Hardcore, false, true) => "hud/heart/hardcore_full_blinking",
            (Self::Hardcore, true, true) => "hud/heart/hardcore_half_blinking",
            (Self::Vehicle, false, false) => "hud/heart/vehicle_full",
            (Self::Vehicle, true, false) => "hud/heart/vehicle_half",
            (Self::Vehicle, false, true) => "hud/heart/vehicle_full_blinking",
            (Self::Vehicle, true, true) => "hud/heart/vehicle_half_blinking",
        }
    }
}

/// The three sprite keys one heart row draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeartRow {
    /// Empty slot drawn under every heart.
    pub container: &'static str,
    /// Full-heart fill.
    pub full: &'static str,
    /// Half-heart fill.
    pub half: &'static str,
}

impl HeartRow {
    /// Whether the host atlas binds all three keys, so the row can render
    /// as Vanilla sprites instead of the procedural fallback.
    pub fn present(self, has: impl Fn(&str) -> bool) -> bool {
        has(self.container) && has(self.full) && has(self.half)
    }
}

/// Sprite keys for `container` + `fill` in the blinking or resting set.
pub const fn heart_row(container: HeartContainer, fill: HeartFill, blinking: bool) -> HeartRow {
    HeartRow {
        container: container.key(blinking),
        full: fill.key(false, blinking),
        half: fill.key(true, blinking),
    }
}

/// Every heart key this catalog can emit, `_blinking` twins included, in
/// binding order — a host packs exactly these rectangles and the HUD can then
/// draw the whole family without a procedural fallback.
pub const HEART_SPRITE_KEYS: [&str; 34] = [
    "hud/heart/container",
    "hud/heart/container_blinking",
    "hud/heart/container_hardcore",
    "hud/heart/container_hardcore_blinking",
    "hud/heart/vehicle_container",
    "hud/heart/vehicle_container_blinking",
    "hud/heart/full",
    "hud/heart/full_blinking",
    "hud/heart/half",
    "hud/heart/half_blinking",
    "hud/heart/poisoned_full",
    "hud/heart/poisoned_full_blinking",
    "hud/heart/poisoned_half",
    "hud/heart/poisoned_half_blinking",
    "hud/heart/withered_full",
    "hud/heart/withered_full_blinking",
    "hud/heart/withered_half",
    "hud/heart/withered_half_blinking",
    "hud/heart/frozen_full",
    "hud/heart/frozen_full_blinking",
    "hud/heart/frozen_half",
    "hud/heart/frozen_half_blinking",
    "hud/heart/absorbing_full",
    "hud/heart/absorbing_full_blinking",
    "hud/heart/absorbing_half",
    "hud/heart/absorbing_half_blinking",
    "hud/heart/hardcore_full",
    "hud/heart/hardcore_full_blinking",
    "hud/heart/hardcore_half",
    "hud/heart/hardcore_half_blinking",
    "hud/heart/vehicle_full",
    "hud/heart/vehicle_full_blinking",
    "hud/heart/vehicle_half",
    "hud/heart/vehicle_half_blinking",
];

/// All heart families, for hosts and tests that walk the whole set.
pub const HEART_CONTAINERS: [HeartContainer; 3] =
    [HeartContainer::Normal, HeartContainer::Hardcore, HeartContainer::Vehicle];

/// All heart fills, for hosts and tests that walk the whole set.
pub const HEART_FILLS: [HeartFill; 7] = [
    HeartFill::Normal,
    HeartFill::Poisoned,
    HeartFill::Withered,
    HeartFill::Frozen,
    HeartFill::Absorbing,
    HeartFill::Hardcore,
    HeartFill::Vehicle,
];

/// Ticks the damage blink stays visible after a hit. Vanilla stamps
/// `Gui#healthBlinkTime = tickCount + 20` whenever health drops.
pub const HEALTH_BLINK_TICKS: i64 = 20;

/// Blink cadence: vanilla takes the blinking twin while
/// `(healthBlinkTime - tickCount) / 3` is odd, so the container and hearts
/// flicker every third tick.
pub const BLINK_PERIOD_TICKS: i64 = 3;

/// Hit points at or below which vanilla jitters the hearts
/// (`Gui#renderHearts`), i.e. two hearts on the standard twenty-point bar.
pub const LOW_HEALTH_THRESHOLD: f32 = 4.0;

/// Vanilla spreads one regeneration sweep over `ceil(maxHealth + 5)` ticks.
pub const REGENERATION_SLACK_TICKS: f32 = 5.0;

/// Pixels the regeneration wave lifts the heart it currently crosses.
pub const REGENERATION_BOUNCE_PX: i32 = 1;

/// `Gui#renderPlayerHealth` reseeds its `RandomSource` with
/// `tickCount * 312871` on every frame.
pub const LOW_HEALTH_SEED_MULTIPLIER: i64 = 312_871;

/// Whether the blinking sprite twins are lit, given the ticks left on
/// vanilla's `healthBlinkTime` countdown.
pub const fn blink_visible(remaining_ticks: i64) -> bool {
    remaining_ticks > 0 && (remaining_ticks / BLINK_PERIOD_TICKS) % 2 == 1
}

/// Recover the blink countdown from the HUD damage-flash alpha.
///
/// The HUD snapshot carries the hurt flash as a `0.0..=1.0` alpha that
/// decays over the vanilla hurt timer; scaling it onto
/// [`HEALTH_BLINK_TICKS`] gives the same flicker the tick countdown would,
/// without the UI crate ever reading a clock. Non-finite alpha (or an
/// already faded flash) reads as `0`, i.e. no blink.
pub fn blink_ticks(hurt_alpha: f32) -> i64 {
    if !hurt_alpha.is_finite() {
        return 0;
    }
    let scaled = f64::from(hurt_alpha.clamp(0.0, 1.0)) * HEALTH_BLINK_TICKS as f64;
    scaled.round() as i64
}

/// Vanilla regeneration sweep period in ticks: `ceil(maxHealth + 5)`, with a
/// non-finite or non-positive maximum falling back to a full bar.
pub fn regeneration_window_ticks(max_health: f32) -> i64 {
    let max_health = if max_health.is_finite() && max_health > 0.0 {
        max_health
    } else {
        20.0
    };
    let window = f64::from(max_health) + f64::from(REGENERATION_SLACK_TICKS);
    window.ceil().max(1.0) as i64
}

/// Heart index (`0` = leftmost icon of the row) the regeneration wave lifts
/// at sweep position `sweep`, counted in half-hearts. Two sweep ticks share
/// one heart, so the wave steps one icon every second tick and walks the row
/// once per [`regeneration_window_ticks`] period. `-1` means "no heart" for
/// an empty row.
pub const fn regeneration_bounce_index(sweep: i64, icon_count: i32) -> i32 {
    if sweep < 0 || icon_count <= 0 {
        return -1;
    }
    ((sweep / 2) % icon_count as i64) as i32
}

/// Whether heart `icon_index` carries the one-pixel regeneration bounce.
pub const fn regeneration_bounces(sweep: i64, icon_index: i32, icon_count: i32) -> bool {
    regeneration_bounce_index(sweep, icon_count) == icon_index
}

// `java.util.Random` constants: the `Gui` HUD random is a
// `LegacyRandomSource`, i.e. exactly this 48-bit LCG.
const RANDOM_MULTIPLIER: i64 = 0x0005_DEEC_E66D;
const RANDOM_ADDEND: i64 = 0xB;
const RANDOM_MASK: i64 = (1 << 48) - 1;

/// The low-health heart jitter, as the vanilla RNG stream the HUD would draw
/// from on one tick.
///
/// Vanilla seeds `Gui.random` with `tickCount * 312871` and takes one
/// `nextInt(2)` per heart while it walks the row, so the whole row shifts by
/// zero or one pixel per heart. Reproducing the LCG keeps the bounce on the
/// same pixel Vanilla picks for that tick while staying a pure function of
/// the snapshot.
#[derive(Clone, Copy, Debug)]
pub struct HeartJitter {
    state: i64,
    enabled: bool,
}

impl HeartJitter {
    /// Jitter for `health_half_units` on phase tick `tick`. The vanilla gate
    /// is `health <= 4`, so a healthy player always gets zero offsets.
    pub fn new(tick: i64, health_half_units: f32) -> Self {
        let seed = tick.wrapping_mul(LOW_HEALTH_SEED_MULTIPLIER);
        Self {
            state: (seed ^ RANDOM_MULTIPLIER) & RANDOM_MASK,
            enabled: health_half_units <= LOW_HEALTH_THRESHOLD,
        }
    }

    /// Next vertical offset in `0..=1` pixels; draws advance the stream
    /// left to right across the row exactly like the vanilla loop.
    pub fn next_offset(&mut self) -> i32 {
        if !self.enabled {
            return 0;
        }
        self.state = self
            .state
            .wrapping_mul(RANDOM_MULTIPLIER)
            .wrapping_add(RANDOM_ADDEND)
            & RANDOM_MASK;
        let next_31 = (self.state >> 17) & 0x7FFF_FFFF;
        (((next_31 * 2) >> 31) & 1) as i32
    }
}

/// Per-frame heart animation state, resolved from the HUD snapshot by the
/// layout and consumed by the row painter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HeartAnimation {
    /// Swap in the `_blinking` sprite twins (damage blink).
    pub blinking: bool,
    /// Regeneration sweep position in half-hearts while the wave is up.
    pub sweep: Option<i64>,
    /// Phase tick the low-health jitter draws from.
    pub jitter_tick: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn containers_cover_the_vanilla_families() {
        assert_eq!(HeartContainer::Normal.key(false), "hud/heart/container");
        assert_eq!(HeartContainer::Normal.key(true), "hud/heart/container_blinking");
        assert_eq!(
            HeartContainer::Hardcore.key(false),
            "hud/heart/container_hardcore"
        );
        assert_eq!(
            HeartContainer::Hardcore.key(true),
            "hud/heart/container_hardcore_blinking"
        );
        assert_eq!(HeartContainer::Vehicle.key(false), "hud/heart/vehicle_container");
        assert_eq!(
            HeartContainer::Vehicle.key(true),
            "hud/heart/vehicle_container_blinking"
        );
    }

    #[test]
    fn fills_cover_every_family_with_full_half_and_blinking_twins() {
        for (fill, stem) in [
            (HeartFill::Normal, ""),
            (HeartFill::Poisoned, "poisoned_"),
            (HeartFill::Withered, "withered_"),
            (HeartFill::Frozen, "frozen_"),
            (HeartFill::Absorbing, "absorbing_"),
            (HeartFill::Hardcore, "hardcore_"),
            (HeartFill::Vehicle, "vehicle_"),
        ] {
            let full = fill.key(false, false);
            let half = fill.key(true, false);
            assert_eq!(full, format!("hud/heart/{stem}full"), "{fill:?} full");
            assert_eq!(half, format!("hud/heart/{stem}half"), "{fill:?} half");
            assert_eq!(
                fill.key(false, true),
                format!("{full}_blinking"),
                "{fill:?} full blink"
            );
            assert_eq!(
                fill.key(true, true),
                format!("{half}_blinking"),
                "{fill:?} half blink"
            );
        }
    }

    #[test]
    fn heart_styles_map_onto_the_fill_families() {        assert_eq!(HeartFill::from_style(HeartStyle::Normal), HeartFill::Normal);
        assert_eq!(
            HeartFill::from_style(HeartStyle::Poisoned),
            HeartFill::Poisoned
        );
        assert_eq!(
            HeartFill::from_style(HeartStyle::Withered),
            HeartFill::Withered
        );
        assert_eq!(HeartFill::from_style(HeartStyle::Frozen), HeartFill::Frozen);
        assert_eq!(
            HeartFill::from_style(HeartStyle::Hardcore),
            HeartFill::Hardcore
        );
    }

    #[test]
    fn sprite_key_list_covers_every_container_and_fill() {
        let mut expected: Vec<&'static str> = Vec::new();
        for container in HEART_CONTAINERS {
            expected.extend([container.key(false), container.key(true)]);
        }
        for fill in HEART_FILLS {
            expected.extend([
                fill.key(false, false),
                fill.key(false, true),
                fill.key(true, false),
                fill.key(true, true),
            ]);
        }
        expected.sort_unstable();
        let mut listed = HEART_SPRITE_KEYS;
        listed.sort_unstable();
        assert_eq!(listed.as_slice(), expected.as_slice());

        // No duplicate bindings: a host packs each rectangle exactly once.
        let mut unique: Vec<&'static str> = HEART_SPRITE_KEYS.to_vec();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), HEART_SPRITE_KEYS.len());
    }

    #[test]
    fn heart_rows_pair_the_requested_container_and_fill() {
        let row = heart_row(HeartContainer::Hardcore, HeartFill::Hardcore, false);
        assert_eq!(
            row,
            HeartRow {
                container: "hud/heart/container_hardcore",
                full: "hud/heart/hardcore_full",
                half: "hud/heart/hardcore_half",
            }
        );
        let blinking = heart_row(HeartContainer::Normal, HeartFill::Absorbing, true);
        assert_eq!(blinking.container, "hud/heart/container_blinking");
        assert_eq!(blinking.full, "hud/heart/absorbing_full_blinking");
        assert_eq!(blinking.half, "hud/heart/absorbing_half_blinking");
        assert!(row.present(|key| key.starts_with("hud/heart/")));
        assert!(!row.present(|key| key != "hud/heart/container_hardcore"));
    }

    #[test]
    fn blink_follows_the_vanilla_three_tick_cadence() {
        // (20 / 3, 19 / 3, ...) alternating odd/even quotients.
        assert!(!blink_visible(0));
        assert!(!blink_visible(-3));
        for remaining in 1..=20i64 {
            assert_eq!(
                blink_visible(remaining),
                (remaining / BLINK_PERIOD_TICKS) % 2 == 1,
                "remaining {remaining}"
            );
        }
        // Twenty ticks of countdown lights the blink on and off.
        let lit: Vec<i64> = (1..=20i64).filter(|r| blink_visible(*r)).collect();
        assert_eq!(lit, vec![3, 4, 5, 9, 10, 11, 15, 16, 17]);
    }

    #[test]
    fn blink_countdown_reads_the_damage_flash() {
        assert_eq!(blink_ticks(1.0), HEALTH_BLINK_TICKS);
        assert_eq!(blink_ticks(0.5), 10);
        assert_eq!(blink_ticks(0.0), 0);
        assert_eq!(blink_ticks(-1.0), 0);
        assert_eq!(blink_ticks(4.0), HEALTH_BLINK_TICKS);
        assert_eq!(blink_ticks(f32::NAN), 0);
        assert_eq!(blink_ticks(f32::INFINITY), 0);
        // A full flash starts lit (20 / 3 = 6, even) and the fade flickers.
        assert!(!blink_visible(blink_ticks(1.0)));
        assert!(blink_visible(blink_ticks(0.5)));
    }

    #[test]
    fn regeneration_window_matches_the_vanilla_period() {
        assert_eq!(regeneration_window_ticks(20.0), 25);
        assert_eq!(regeneration_window_ticks(10.0), 15);
        assert_eq!(regeneration_window_ticks(7.5), 13);
        // Non-finite / non-positive maxima fall back to a full bar.
        assert_eq!(regeneration_window_ticks(f32::NAN), 25);
        assert_eq!(regeneration_window_ticks(0.0), 25);
    }

    #[test]
    fn regeneration_bounce_walks_one_heart_every_two_ticks() {
        assert_eq!(regeneration_bounce_index(0, 10), 0);
        assert_eq!(regeneration_bounce_index(1, 10), 0);
        assert_eq!(regeneration_bounce_index(2, 10), 1);
        assert_eq!(regeneration_bounce_index(3, 10), 1);
        assert_eq!(regeneration_bounce_index(18, 10), 9);
        // The sweep wraps instead of running off the row.
        assert_eq!(regeneration_bounce_index(20, 10), 0);
        assert_eq!(regeneration_bounce_index(24, 10), 2);
        // Empty rows and out-of-range sweeps never lift a heart.
        assert_eq!(regeneration_bounce_index(5, 0), -1);
        assert_eq!(regeneration_bounce_index(-1, 10), -1);
        assert!(!regeneration_bounces(-1, 0, 10));
        assert!(regeneration_bounces(4, 2, 10));
        assert!(!regeneration_bounces(4, 3, 10));
    }

    #[test]
    fn low_health_jitter_is_vanilla_seeded_and_gated() {
        // Above four hit points every heart stays put whatever the tick.
        for tick in [0, 1, 7, 1_000] {
            let mut jitter = HeartJitter::new(tick, LOW_HEALTH_THRESHOLD + 1.0);
            for _ in 0..10 {
                assert_eq!(jitter.next_offset(), 0);
            }
        }

        // At or below four the stream is the vanilla LCG: deterministic per
        // tick, in 0..=1, and different ticks pick different patterns.
        let sample = |tick: i64| {
            let mut jitter = HeartJitter::new(tick, 4.0);
            (0..10).map(|_| jitter.next_offset()).collect::<Vec<_>>()
        };
        let first = sample(0);
        assert_eq!(first, sample(0));
        assert!(first.iter().all(|offset| (0..=1).contains(offset)));
        assert_ne!(first, sample(1));
        assert!(first.contains(&1), "jitter must move");

        // `java.util.Random(0).nextInt(2)` sequence, pinned so the LCG can
        // not silently drift away from the vanilla stream.
        let mut reference = HeartJitter::new(0, 4.0);
        let pinned: Vec<i32> = (0..8).map(|_| reference.next_offset()).collect();
        assert_eq!(pinned, vec![1, 1, 0, 1, 1, 0, 1, 0]);
    }
}
