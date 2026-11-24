// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Clean-room season-mod gameplay primitives — pure `std`, zero new
//! dependencies, and entirely deterministic tick evaluation (no wall-clock
//! `SystemTime` anywhere).
//!
//! Five families cover the core contract shared by generic season mods
//! (Serene Seasons / Fabric Seasons / Ecliptic Seasons style behaviour, plus
//! datapack season tags), implemented natively with no JVM:
//!
//! 1. [`Season`]/[`SubSeason`]/[`SeasonCalendarConfig`]/[`SeasonState`]: a
//!    12-sub-season calendar derived deterministically from `world_ticks`,
//!    with an optional locked season, a dimension allowlist, and a smooth
//!    sinusoidal daylight curve (long summer days, short winter days).
//! 2. [`BiomeClimateClass`]/[`BiomeClimateRules`]/[`SeasonalWeatherEval`]:
//!    biome climate classification (temperate four-season, tropical
//!    wet/dry, cold long-snow, seasonless) driving seasonal temperature
//!    offsets, rain-vs-snow precipitation, water freezing, and spring/summer
//!    thaw that never strips naturally snowy peaks.
//! 3. [`SeasonalTintRules`]/[`seasonal_foliage_tint`]/[`seasonal_grass_tint`]:
//!    smooth RGB multipliers interpolated continuously across all 12
//!    sub-season anchors including the `LateWinter -> EarlySpring`
//!    wrap-around, with a configurable autumn shift and evergreen
//!    preservation patterns.
//! 4. [`CropFertilityRules`]/[`CropGrowthEval`]: seasonal crop fertility
//!    driven by standard Minecraft tag JSON (`{"values": [...]}`) loaded via
//!    [`load_season_tag_json`], an off-season slow/no-growth policy, a
//!    tropical year-round exemption, and a greenhouse-glass sky scan that
//!    overrides season lockout.
//! 5. `SSN1` wire codec on channel [`SEASON_CHANNEL`]: a fixed 19-byte
//!    big-endian sync frame carrying sub-season, quantized progress,
//!    day-of-season, daylight factor, foliage/grass RGB, and climate flags —
//!    decoded fail-closed (bad magic, wrong version, out-of-range
//!    sub-season, unknown flag bits, truncation, or trailing bytes all
//!    reject).
//!
//! Clean-room note: every rule, threshold, anchor colour, and wire layout
//! here is an original declarative design — no code, constants, or data
//! tables are ported from any third-party season mod.

use std::collections::BTreeSet;

// ===========================================================================
// Shared bounds
// ===========================================================================

/// Maximum length of any identifier string (biome id, crop id, tag id,
/// block id) accepted anywhere in this module.
pub const MAX_ID_LEN: usize = 128;

/// Vanilla overworld day length in ticks.
pub const DEFAULT_TICKS_PER_DAY: u64 = 24_000;

/// Default days per sub-season (12 * 8 = 96-day year).
pub const DEFAULT_DAYS_PER_SUB_SEASON: u32 = 8;

fn finite(v: f32) -> bool {
    v.is_finite()
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_ID_LEN
}

/// Clamp `v` into `0.0..=1.0`; non-finite input clamps to `0.0`.
fn clamp_unit(v: f32) -> f32 {
    if finite(v) {
        v.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

// ===========================================================================
// 1. 12-sub-season calendar & daylight curve
// ===========================================================================

/// The four broad seasons.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Season {
    Spring,
    Summer,
    Autumn,
    Winter,
}

impl Season {
    /// `Spring -> Summer -> Autumn -> Winter -> Spring`.
    pub fn index(self) -> u8 {
        match self {
            Season::Spring => 0,
            Season::Summer => 1,
            Season::Autumn => 2,
            Season::Winter => 3,
        }
    }
}

/// The 12 ordered sub-seasons, three per [`Season`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SubSeason {
    EarlySpring,
    MidSpring,
    LateSpring,
    EarlySummer,
    MidSummer,
    LateSummer,
    EarlyAutumn,
    MidAutumn,
    LateAutumn,
    EarlyWinter,
    MidWinter,
    LateWinter,
}

impl SubSeason {
    /// All 12 sub-seasons in calendar order.
    pub const ALL: [SubSeason; 12] = [
        SubSeason::EarlySpring,
        SubSeason::MidSpring,
        SubSeason::LateSpring,
        SubSeason::EarlySummer,
        SubSeason::MidSummer,
        SubSeason::LateSummer,
        SubSeason::EarlyAutumn,
        SubSeason::MidAutumn,
        SubSeason::LateAutumn,
        SubSeason::EarlyWinter,
        SubSeason::MidWinter,
        SubSeason::LateWinter,
    ];

    /// Calendar index `0..=11`.
    pub fn index(self) -> u8 {
        match self {
            SubSeason::EarlySpring => 0,
            SubSeason::MidSpring => 1,
            SubSeason::LateSpring => 2,
            SubSeason::EarlySummer => 3,
            SubSeason::MidSummer => 4,
            SubSeason::LateSummer => 5,
            SubSeason::EarlyAutumn => 6,
            SubSeason::MidAutumn => 7,
            SubSeason::LateAutumn => 8,
            SubSeason::EarlyWinter => 9,
            SubSeason::MidWinter => 10,
            SubSeason::LateWinter => 11,
        }
    }

    /// Inverse of [`Self::index`]; `None` for `idx >= 12`.
    pub fn from_index(idx: u8) -> Option<SubSeason> {
        Self::ALL.get(idx as usize).copied()
    }

    /// The broad [`Season`] this sub-season belongs to.
    pub fn season(self) -> Season {
        match self.index() / 3 {
            0 => Season::Spring,
            1 => Season::Summer,
            2 => Season::Autumn,
            _ => Season::Winter,
        }
    }

    /// The next sub-season, wrapping `LateWinter -> EarlySpring`.
    pub fn next(self) -> SubSeason {
        SubSeason::from_index((self.index() + 1) % 12).unwrap_or(SubSeason::EarlySpring)
    }
}

/// Declarative calendar configuration. Invalid values are repaired by
/// [`SeasonCalendarConfig::sanitize`] / [`SeasonCalendarConfig::sanitized`]
/// rather than panicking.
#[derive(Clone, Debug, PartialEq)]
pub struct SeasonCalendarConfig {
    /// Days in each of the 12 sub-seasons (`>= 1`; default 8).
    pub days_per_sub_season: u32,
    /// Ticks per in-game day (`>= 1`; default 24,000).
    pub ticks_per_day: u64,
    /// Sub-season the calendar sits in at `world_ticks == 0`.
    pub starting_sub_season: SubSeason,
    /// When `Some`, the calendar is locked to this sub-season regardless of
    /// `world_ticks` (locked-season dimensions and maps).
    pub fixed_sub_season: Option<SubSeason>,
    /// Dimension ids where seasons run (default: only
    /// `minecraft:overworld`; Nether/End-style dimensions stay seasonless
    /// unless explicitly added).
    pub enabled_dimensions: BTreeSet<String>,
    /// Master switch for the seasonal daylight curve (default `true`;
    /// `false` pins [`SeasonState::daylight_fraction`] to `0.5`).
    pub enable_daylight_cycle_shift: bool,
    /// Half the peak-to-trough swing of the daylight curve
    /// (`0.0..=0.45`; default `0.12` -> ~0.62 mid-summer, ~0.38
    /// mid-winter, 0.50 at the equinoxes).
    pub daylight_amplitude: f32,
}

impl Default for SeasonCalendarConfig {
    fn default() -> Self {
        let mut enabled_dimensions = BTreeSet::new();
        enabled_dimensions.insert("minecraft:overworld".to_string());
        Self {
            days_per_sub_season: DEFAULT_DAYS_PER_SUB_SEASON,
            ticks_per_day: DEFAULT_TICKS_PER_DAY,
            starting_sub_season: SubSeason::EarlySpring,
            fixed_sub_season: None,
            enabled_dimensions,
            enable_daylight_cycle_shift: true,
            daylight_amplitude: 0.12,
        }
    }
}

impl SeasonCalendarConfig {
    /// Repair invalid fields in place: `days_per_sub_season` and
    /// `ticks_per_day` bottom out at 1, and `daylight_amplitude` is forced
    /// finite and clamped to `0.0..=0.45`.
    pub fn sanitize(&mut self) {
        self.days_per_sub_season = self.days_per_sub_season.max(1);
        self.ticks_per_day = self.ticks_per_day.max(1);
        self.daylight_amplitude = if finite(self.daylight_amplitude) {
            self.daylight_amplitude.clamp(0.0, 0.45)
        } else {
            0.0
        };
    }

    /// A sanitized clone; the input is left untouched.
    pub fn sanitized(&self) -> Self {
        let mut out = self.clone();
        out.sanitize();
        out
    }

    /// Whether seasons run in `dimension_id`.
    pub fn is_dimension_enabled(&self, dimension_id: &str) -> bool {
        self.enabled_dimensions.contains(dimension_id)
    }

    /// Days in one sub-season (sanitized lower bound applied).
    pub fn sub_season_days(&self) -> u64 {
        u64::from(self.days_per_sub_season.max(1))
    }

    /// Days in a full 12-sub-season year.
    pub fn year_days(&self) -> u64 {
        self.sub_season_days() * 12
    }
}

/// Point-in-time season snapshot derived from `world_ticks`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SeasonState {
    /// Current sub-season.
    pub sub_season: SubSeason,
    /// Broad season (`sub_season.season()`).
    pub season: Season,
    /// Progress through the current sub-season, always in `0.0..1.0`
    /// (`0.0` when the calendar is fixed).
    pub progress_in_sub_season: f32,
    /// Whole days elapsed inside the current sub-season.
    pub day_in_sub_season: u16,
    /// Whole days elapsed inside the current broad [`Season`].
    pub day_of_season: u16,
    /// Whole days elapsed since the year start (`EarlySpring` index 0).
    pub day_of_year: u32,
    /// Fraction of the day that is daylight, `0.0..=1.0` (`0.5` when the
    /// daylight curve is disabled).
    pub daylight_fraction: f32,
    /// Position in the year `0.0..1.0` (`(index + progress) / 12`).
    pub year_phase: f32,
}

/// Year phase of the mid-summer daylight peak: the centre of `MidSummer`
/// (index 4) sits at `(4 + 0.5) / 12`.
const SUMMER_SOLSTICE_PHASE: f32 = 4.5 / 12.0;

impl SeasonState {
    /// Deterministically map `world_ticks` onto the calendar.
    ///
    /// `cfg` is sanitized on a clone first, so a zero `ticks_per_day` or a
    /// wild `daylight_amplitude` can never divide by zero or produce NaN.
    pub fn from_world_ticks(world_ticks: u64, cfg: &SeasonCalendarConfig) -> SeasonState {
        let cfg = cfg.sanitized();
        let dps = cfg.sub_season_days();
        let tpd = cfg.ticks_per_day; // >= 1 after sanitize

        let (idx_u64, day_in_sub, progress);
        if let Some(fixed) = cfg.fixed_sub_season {
            idx_u64 = u64::from(fixed.index());
            day_in_sub = 0u64;
            progress = 0.0f32;
        } else {
            let total_days = world_ticks / tpd;
            let frac_day = (world_ticks % tpd) as f32 / tpd as f32;
            idx_u64 = (u64::from(cfg.starting_sub_season.index()) + total_days / dps) % 12;
            day_in_sub = total_days % dps;
            progress = ((day_in_sub as f32) + frac_day) / dps as f32;
        }

        let sub_season = SubSeason::from_index(idx_u64 as u8).unwrap_or(SubSeason::EarlySpring);
        let progress = clamp_unit(progress);
        let day_in_sub = day_in_sub.min(u64::from(u16::MAX)) as u16;
        let day_of_year = (idx_u64 * dps + u64::from(day_in_sub)) as u32;
        let day_of_season =
            ((idx_u64 % 3) * dps + u64::from(day_in_sub)).min(u64::from(u16::MAX)) as u16;
        let year_phase = ((idx_u64 as f32) + progress) / 12.0;

        let daylight_fraction = if cfg.enable_daylight_cycle_shift {
            let phase = 2.0 * std::f32::consts::PI * (year_phase - SUMMER_SOLSTICE_PHASE);
            (0.5 + cfg.daylight_amplitude * phase.cos()).clamp(0.0, 1.0)
        } else {
            0.5
        };

        SeasonState {
            sub_season,
            season: sub_season.season(),
            progress_in_sub_season: progress,
            day_in_sub_season: day_in_sub,
            day_of_season,
            day_of_year,
            daylight_fraction,
            year_phase,
        }
    }
}

// ===========================================================================
// 2. Biome climate, snow/ice & thaw rules
// ===========================================================================

/// How a biome experiences seasons.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BiomeClimateClass {
    /// Full four seasons: winter snow & freeze, spring thaw.
    Temperate,
    /// Wet/dry seasons instead of snow (jungle, savanna, mangrove, ...);
    /// never snows even in `MidWinter`.
    Tropical,
    /// Long snow cover with only a brief summer thaw (taiga, snowy slopes).
    Cold,
    /// No seasons at all (nether, end, deep dark, lush caves, blacklisted).
    Seasonless,
}

/// Precipitation a biome produces right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PrecipitationKind {
    None,
    Rain,
    Snow,
}

/// Declarative climate rules: name-pattern classification tables plus the
/// thermal thresholds used by [`evaluate_biome_weather`]. Pattern matching
/// is a plain `str::contains` on the lowercased biome id, checked in the
/// order seasonless -> tropical -> cold -> temperature fallback.
#[derive(Clone, Debug, PartialEq)]
pub struct BiomeClimateRules {
    /// Ids matching any pattern here are [`BiomeClimateClass::Seasonless`].
    pub seasonless_patterns: Vec<String>,
    /// Ids matching any pattern here are [`BiomeClimateClass::Tropical`].
    pub tropical_patterns: Vec<String>,
    /// Ids matching any pattern here are [`BiomeClimateClass::Cold`].
    pub cold_patterns: Vec<String>,
    /// Ids matching any pattern here never precipitate (deserts, badlands).
    pub dry_patterns: Vec<String>,
    /// Fallback: `base_temp` at or above this is Tropical (default 0.95 —
    /// jungle/savanna territory; plains at 0.8 stays temperate).
    pub tropical_min_temp: f32,
    /// Fallback: `base_temp` at or below this is Cold (default 0.30).
    pub cold_max_temp: f32,
    /// Below this effective temperature precipitation falls as snow and
    /// snow cover is expected (default 0.15, vanilla-style).
    pub snow_temp_threshold: f32,
    /// Below this effective temperature exposed water freezes
    /// (default 0.15).
    pub freeze_temp_threshold: f32,
    /// Effective temperature at or above this melts seasonal snow
    /// (default 0.20).
    pub melt_temp_threshold: f32,
    /// A biome whose *base* temperature is at or below this is naturally
    /// snowy — its cover is permanent, never flagged for seasonal melt
    /// (default 0.15; snowy peaks sit far below).
    pub natural_snow_max_temp: f32,
    /// Effective-temperature drop per block of altitude above
    /// `sea_level_y` (default ~0.0016/block).
    pub altitude_lapse_per_y: f32,
    /// Altitude the lapse rate is measured from (default 64).
    pub sea_level_y: f32,
    /// Seasonal effective-temperature offsets for Temperate biomes,
    /// ordered `Spring, Summer, Autumn, Winter`.
    pub temperate_offsets: [f32; 4],
    /// Seasonal effective-temperature offsets for Cold biomes,
    /// ordered `Spring, Summer, Autumn, Winter`. The positive summer
    /// offset is what opens the brief thaw window.
    pub cold_offsets: [f32; 4],
    /// Effective-temperature offset applied to Tropical biomes during the
    /// wet season (default 0.0 — wet/dry is a precipitation change, not a
    /// temperature one).
    pub tropical_wet_offset: f32,
}

impl Default for BiomeClimateRules {
    fn default() -> Self {
        Self {
            seasonless_patterns: vec![
                "nether".to_string(),
                "the_end".to_string(),
                "end_".to_string(),
                "_end".to_string(),
                "deep_dark".to_string(),
                "lush_caves".to_string(),
                "void".to_string(),
            ],
            tropical_patterns: vec![
                "jungle".to_string(),
                "desert".to_string(),
                "savanna".to_string(),
                "badlands".to_string(),
                "mangrove".to_string(),
                "mesa".to_string(),
                "rainforest".to_string(),
                "oasis".to_string(),
                "tropical".to_string(),
            ],
            cold_patterns: vec![
                "taiga".to_string(),
                "snowy".to_string(),
                "snow".to_string(),
                "frozen".to_string(),
                "ice_".to_string(),
                "glacier".to_string(),
                "tundra".to_string(),
                "grove".to_string(),
                "peaks".to_string(),
                "slopes".to_string(),
            ],
            dry_patterns: vec![
                "desert".to_string(),
                "badlands".to_string(),
                "mesa".to_string(),
                "nether".to_string(),
                "the_end".to_string(),
                "end_".to_string(),
                "void".to_string(),
            ],
            tropical_min_temp: 0.95,
            cold_max_temp: 0.30,
            snow_temp_threshold: 0.15,
            freeze_temp_threshold: 0.15,
            melt_temp_threshold: 0.20,
            natural_snow_max_temp: 0.15,
            altitude_lapse_per_y: 0.0016,
            sea_level_y: 64.0,
            temperate_offsets: [0.10, 0.30, -0.05, -0.70],
            cold_offsets: [-0.10, 0.30, -0.15, -0.45],
            tropical_wet_offset: 0.0,
        }
    }
}

impl BiomeClimateRules {
    /// Force every scalar threshold finite (non-finite falls back to the
    /// default value for that field).
    pub fn sanitize(&mut self) {
        let d = Self::default();
        let fix = |v: &mut f32, fallback: f32| {
            if !finite(*v) {
                *v = fallback;
            }
        };
        fix(&mut self.tropical_min_temp, d.tropical_min_temp);
        fix(&mut self.cold_max_temp, d.cold_max_temp);
        fix(&mut self.snow_temp_threshold, d.snow_temp_threshold);
        fix(&mut self.freeze_temp_threshold, d.freeze_temp_threshold);
        fix(&mut self.melt_temp_threshold, d.melt_temp_threshold);
        fix(&mut self.natural_snow_max_temp, d.natural_snow_max_temp);
        fix(&mut self.altitude_lapse_per_y, d.altitude_lapse_per_y);
        fix(&mut self.sea_level_y, d.sea_level_y);
        fix(&mut self.tropical_wet_offset, d.tropical_wet_offset);
        for (v, fallback) in self
            .temperate_offsets
            .iter_mut()
            .zip(d.temperate_offsets.iter())
            .chain(self.cold_offsets.iter_mut().zip(d.cold_offsets.iter()))
        {
            fix(v, *fallback);
        }
    }

    /// A sanitized clone; the input is left untouched.
    pub fn sanitized(&self) -> Self {
        let mut out = self.clone();
        out.sanitize();
        out
    }

    fn matches_any(patterns: &[String], lowered_id: &str) -> bool {
        patterns.iter().any(|p| lowered_id.contains(p.as_str()))
    }

    /// Whether the biome produces any precipitation at all.
    pub fn has_precipitation(&self, biome_id: &str) -> bool {
        let id = biome_id.to_ascii_lowercase();
        !Self::matches_any(&self.dry_patterns, &id)
    }

    /// Whether `sub_season` falls inside the tropical wet season.
    ///
    /// The wet season spans summer through late autumn (indices 3..=8);
    /// winter and spring are the dry season.
    pub fn is_tropical_wet_season(sub_season: SubSeason) -> bool {
        (3..=8).contains(&sub_season.index())
    }

    /// Classify `biome_id` (with `base_temp` as the fallback signal for
    /// ids that match no pattern).
    pub fn classify_biome(&self, biome_id: &str, base_temp: f32) -> BiomeClimateClass {
        let id = biome_id.to_ascii_lowercase();
        if Self::matches_any(&self.seasonless_patterns, &id) {
            return BiomeClimateClass::Seasonless;
        }
        if Self::matches_any(&self.tropical_patterns, &id) {
            return BiomeClimateClass::Tropical;
        }
        if Self::matches_any(&self.cold_patterns, &id) {
            return BiomeClimateClass::Cold;
        }
        let temp = if finite(base_temp) { base_temp } else { 0.5 };
        if temp >= self.tropical_min_temp {
            BiomeClimateClass::Tropical
        } else if temp <= self.cold_max_temp {
            BiomeClimateClass::Cold
        } else {
            BiomeClimateClass::Temperate
        }
    }

    /// The seasonal temperature offset for `climate` in `season`.
    fn seasonal_offset(&self, climate: BiomeClimateClass, season: Season, wet: bool) -> f32 {
        let i = season.index() as usize;
        match climate {
            BiomeClimateClass::Temperate => self.temperate_offsets[i],
            BiomeClimateClass::Cold => self.cold_offsets[i],
            BiomeClimateClass::Tropical => {
                if wet {
                    self.tropical_wet_offset
                } else {
                    0.0
                }
            }
            BiomeClimateClass::Seasonless => 0.0,
        }
    }
}

/// Result of [`evaluate_biome_weather`]: what one biome column does under
/// the current [`SeasonState`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SeasonalWeatherEval {
    /// Climate the biome was classified into.
    pub climate: BiomeClimateClass,
    /// `base_temp` plus the seasonal offset and altitude lapse.
    pub effective_temp: f32,
    /// Precipitation falling right now.
    pub precipitation: PrecipitationKind,
    /// Tropical wet season flag (`false` for non-tropical climates).
    pub wet_season: bool,
    /// Whether exposed water can freeze into ice right now.
    pub can_freeze_water: bool,
    /// Whether the ground is expected to carry snow cover right now.
    pub snow_cover_expected: bool,
    /// Whether *seasonal* snow placed by winter should melt now. Naturally
    /// snowy biomes (low `base_temp`) are never flagged — their cover is
    /// permanent, not seasonal.
    pub should_melt_seasonal_snow: bool,
}

/// Evaluate one biome column against the current season state.
///
/// `altitude_y` cools the column at `altitude_lapse_per_y` above
/// `sea_level_y`, so mountain tops can stay snowy while their feet thaw.
/// All rules scalars are sanitized on a clone; non-finite `base_temp`
/// degrades to `0.5` (a mild temperate default) rather than NaN.
pub fn evaluate_biome_weather(
    biome_id: &str,
    base_temp: f32,
    altitude_y: f32,
    state: &SeasonState,
    rules: &BiomeClimateRules,
) -> SeasonalWeatherEval {
    let rules = rules.sanitized();
    let climate = rules.classify_biome(biome_id, base_temp);
    let base = if finite(base_temp) { base_temp } else { 0.5 };
    let altitude = if finite(altitude_y) {
        altitude_y
    } else {
        rules.sea_level_y
    };

    let wet = climate == BiomeClimateClass::Tropical
        && BiomeClimateRules::is_tropical_wet_season(state.sub_season);
    let effective_temp =
        base + rules.seasonal_offset(climate, state.season, wet) - altitude_lapse(&rules, altitude);

    let precipitates = rules.has_precipitation(biome_id);
    let precipitation = match climate {
        BiomeClimateClass::Seasonless => PrecipitationKind::None,
        BiomeClimateClass::Tropical => {
            if wet && precipitates {
                PrecipitationKind::Rain
            } else {
                PrecipitationKind::None
            }
        }
        BiomeClimateClass::Temperate | BiomeClimateClass::Cold => {
            if !precipitates {
                PrecipitationKind::None
            } else if effective_temp < rules.snow_temp_threshold {
                PrecipitationKind::Snow
            } else {
                PrecipitationKind::Rain
            }
        }
    };

    let seasonal_snow_biome = matches!(
        climate,
        BiomeClimateClass::Temperate | BiomeClimateClass::Cold
    );
    let can_freeze_water = seasonal_snow_biome && effective_temp < rules.freeze_temp_threshold;
    let snow_cover_expected = seasonal_snow_biome && effective_temp < rules.snow_temp_threshold;

    // Seasonal thaw: only for biomes that are not naturally snowy, inside
    // their melt window (temperate: spring+summer; cold: summer only), and
    // only once the warmed column actually clears the melt threshold.
    let in_melt_window = match climate {
        BiomeClimateClass::Temperate => matches!(state.season, Season::Spring | Season::Summer),
        BiomeClimateClass::Cold => matches!(state.season, Season::Summer),
        _ => false,
    };
    let should_melt_seasonal_snow = seasonal_snow_biome
        && base > rules.natural_snow_max_temp
        && in_melt_window
        && effective_temp >= rules.melt_temp_threshold;

    SeasonalWeatherEval {
        climate,
        effective_temp,
        precipitation,
        wet_season: wet,
        can_freeze_water,
        snow_cover_expected,
        should_melt_seasonal_snow,
    }
}

fn altitude_lapse(rules: &BiomeClimateRules, altitude_y: f32) -> f32 {
    (altitude_y - rules.sea_level_y).max(0.0) * rules.altitude_lapse_per_y
}

// ===========================================================================
// 3. Smooth seasonal foliage & grass tint
// ===========================================================================

/// Anchor foliage multipliers at the *start* of each of the 12 sub-seasons
/// (index = `SubSeason::index()`). The returned tint lerps between
/// consecutive anchors by `progress_in_sub_season`, so the wrap from
/// `LateWinter` back to `EarlySpring` is as smooth as every other step.
const FOLIAGE_ANCHORS: [[f32; 3]; 12] = [
    [0.90, 1.12, 0.82], // EarlySpring — fresh bright green
    [0.85, 1.12, 0.85], // MidSpring
    [0.82, 1.08, 0.85], // LateSpring
    [0.80, 1.05, 0.82], // EarlySummer — lush warm green
    [0.80, 1.02, 0.80], // MidSummer
    [0.88, 1.00, 0.76], // LateSummer — first dry gold hint
    [1.10, 0.92, 0.62], // EarlyAutumn — amber shift begins
    [1.18, 0.82, 0.52], // MidAutumn — rich amber/crimson
    [1.14, 0.72, 0.52], // LateAutumn — deep gold/crimson
    [1.00, 0.84, 0.68], // EarlyWinter — browns fading out
    [0.90, 0.90, 0.95], // MidWinter — desaturated frosty
    [0.92, 0.96, 0.90], // LateWinter — first thaw-green hint
];

/// Anchor grass multipliers — same rhythm as foliage but milder, so lawns
/// and meadows stay readable.
const GRASS_ANCHORS: [[f32; 3]; 12] = [
    [0.95, 1.08, 0.88], // EarlySpring
    [0.92, 1.08, 0.90], // MidSpring
    [0.90, 1.05, 0.90], // LateSpring
    [0.88, 1.03, 0.88], // EarlySummer
    [0.88, 1.00, 0.86], // MidSummer
    [0.95, 0.98, 0.82], // LateSummer
    [1.05, 0.95, 0.75], // EarlyAutumn
    [1.10, 0.88, 0.70], // MidAutumn
    [1.06, 0.84, 0.70], // LateAutumn
    [0.98, 0.90, 0.80], // EarlyWinter
    [0.92, 0.92, 0.98], // MidWinter
    [0.94, 0.97, 0.94], // LateWinter
];

/// First anchor index of the autumn foliage shift (`EarlyAutumn`).
const AUTUMN_ANCHOR_START: usize = 6;
/// One-past-last anchor index of the autumn foliage shift.
const AUTUMN_ANCHOR_END: usize = 9;

/// Declarative tint configuration.
#[derive(Clone, Debug, PartialEq)]
pub struct SeasonalTintRules {
    /// Scales the autumn foliage deviation from neutral,
    /// `0.0..=1.0` (default `1.0` = full amber/crimson shift; `0.0` keeps
    /// autumn at plain green).
    pub autumn_foliage_strength: f32,
    /// Biome-id patterns whose *foliage* never retints — evergreen
    /// conifers stay green through autumn and winter, and optional
    /// signature-foliage biomes (birch, cherry) keep their authored colour.
    /// Grass still tints seasonally in these biomes.
    pub preserve_foliage_patterns: Vec<String>,
}

impl Default for SeasonalTintRules {
    fn default() -> Self {
        Self {
            autumn_foliage_strength: 1.0,
            preserve_foliage_patterns: vec![
                "spruce".to_string(),
                "taiga".to_string(),
                "pine".to_string(),
                "fir".to_string(),
                "redwood".to_string(),
                "conifer".to_string(),
                "evergreen".to_string(),
                "birch".to_string(),
                "cherry".to_string(),
            ],
        }
    }
}

impl SeasonalTintRules {
    /// Clamp `autumn_foliage_strength` to `0.0..=1.0` (non-finite -> `0.0`).
    pub fn sanitize(&mut self) {
        self.autumn_foliage_strength = clamp_unit(self.autumn_foliage_strength);
    }

    /// A sanitized clone; the input is left untouched.
    pub fn sanitized(&self) -> Self {
        let mut out = self.clone();
        out.sanitize();
        out
    }

    /// Whether `biome_id` keeps its authored foliage colour year-round.
    pub fn preserves_foliage(&self, biome_id: &str) -> bool {
        let id = biome_id.to_ascii_lowercase();
        self.preserve_foliage_patterns
            .iter()
            .any(|p| id.contains(p.as_str()))
    }
}

const NEUTRAL_TINT: [f32; 3] = [1.0, 1.0, 1.0];

/// Interpolated seasonal foliage multiplier for one biome column.
///
/// Tropical and seasonless biomes and foliage-preserved biomes (spruce,
/// birch, cherry, ...) always return neutral `[1.0, 1.0, 1.0]`. Temperate
/// and cold biomes lerp the 12 foliage anchors by
/// `state.progress_in_sub_season`, including the `LateWinter ->
/// EarlySpring` wrap.
pub fn seasonal_foliage_tint(
    state: &SeasonState,
    climate: BiomeClimateClass,
    biome_id: &str,
    rules: &SeasonalTintRules,
) -> [f32; 3] {
    let rules = rules.sanitized();
    if matches!(
        climate,
        BiomeClimateClass::Tropical | BiomeClimateClass::Seasonless
    ) || rules.preserves_foliage(biome_id)
    {
        return NEUTRAL_TINT;
    }
    lerp_anchors(
        &adjusted_anchors(FOLIAGE_ANCHORS, rules.autumn_foliage_strength),
        state,
    )
}

/// Interpolated seasonal grass multiplier for one biome column. Tropical
/// and seasonless biomes return neutral; grass tints everywhere else,
/// including foliage-preserved biomes.
pub fn seasonal_grass_tint(
    state: &SeasonState,
    climate: BiomeClimateClass,
    _rules: &SeasonalTintRules,
) -> [f32; 3] {
    match climate {
        BiomeClimateClass::Tropical | BiomeClimateClass::Seasonless => NEUTRAL_TINT,
        _ => lerp_anchors(&adjusted_anchors(GRASS_ANCHORS, 1.0), state),
    }
}

/// Scale the autumn anchors' deviation from `[1,1,1]` by `strength`.
fn adjusted_anchors(base: [[f32; 3]; 12], strength: f32) -> [[f32; 3]; 12] {
    let mut out = base;
    for anchor in out
        .iter_mut()
        .take(AUTUMN_ANCHOR_END)
        .skip(AUTUMN_ANCHOR_START)
    {
        for c in anchor.iter_mut() {
            *c = 1.0 + (*c - 1.0) * strength;
        }
    }
    out
}

/// Linear interpolation between `anchors[i]` and `anchors[i+1]` with
/// wrap-around, using `state.progress_in_sub_season` (already `0.0..1.0`).
fn lerp_anchors(anchors: &[[f32; 3]; 12], state: &SeasonState) -> [f32; 3] {
    let i = state.sub_season.index() as usize;
    let j = (i + 1) % 12;
    let t = clamp_unit(state.progress_in_sub_season);
    let mut out = [0.0f32; 3];
    for (k, o) in out.iter_mut().enumerate() {
        *o = anchors[i][k] + (anchors[j][k] - anchors[i][k]) * t;
    }
    out
}

// ===========================================================================
// 4. Crop fertility & greenhouse glass
// ===========================================================================

/// Which seasonal crop tag a [`CropTagSet`] was loaded for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum SeasonTagKind {
    #[default]
    Spring,
    Summer,
    Autumn,
    Winter,
    YearRound,
}

/// One loaded crop tag: literal crop ids plus `#tag` references resolved
/// against the tags a crop item declares at query time.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CropTagSet {
    /// Tag this set was loaded for (`Spring` when built empty).
    pub kind: SeasonTagKind,
    /// Literal crop ids (`"minecraft:wheat"`).
    pub ids: BTreeSet<String>,
    /// Referenced tag ids with the leading `#` stripped (`"c:crops/corn"`).
    pub tag_refs: BTreeSet<String>,
}

impl CropTagSet {
    /// Whether `crop_id` belongs to this set — directly, or via one of the
    /// tag ids listed in `crop_tags` matching a `#tag` reference.
    pub fn contains(&self, crop_id: &str, crop_tags: &BTreeSet<String>) -> bool {
        if self.ids.contains(crop_id) {
            return true;
        }
        crop_tags.iter().any(|t| {
            let t = t.strip_prefix('#').unwrap_or(t.as_str());
            self.tag_refs.contains(t)
        })
    }
}

/// Whether `id` looks like a namespaced identifier (`a-z0-9_./:-`,
/// namespaced or bare).
fn valid_entry_id(id: &str) -> bool {
    valid_id(id)
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | ':'))
}

/// Load a standard Minecraft tag JSON into a [`CropTagSet`].
///
/// Accepts `{"values": ["minecraft:wheat", "#c:crops/corn", ...]}` plus any
/// sibling keys (`"replace": false`, `"optional": [...]`, ...) — unknown
/// members are ignored, malformed input returns `None`. Entries starting
/// with `#` land in [`CropTagSet::tag_refs`] (with `#` stripped), plain
/// entries in [`CropTagSet::ids`]. Empty, oversized, or non-identifier
/// entries fail closed.
pub fn load_season_tag_json(kind: SeasonTagKind, json_str: &str) -> Option<CropTagSet> {
    use megu_core::json::JsonValue as CoreJson;

    let root = megu_core::json::parse_json(json_str).ok()?;
    let members = match &root {
        CoreJson::Object(members) => members,
        _ => return None,
    };

    let mut set = CropTagSet {
        kind,
        ..CropTagSet::default()
    };
    let mut saw_values = false;

    for (key, value) in members {
        if key != "values" {
            continue;
        }
        let items = match value {
            CoreJson::Array(items) => items,
            _ => return None,
        };
        saw_values = true;
        for item in items {
            let entry = match item {
                CoreJson::String(s) => s.as_str(),
                _ => return None,
            };
            let (is_tag, body) = match entry.strip_prefix('#') {
                Some(rest) => (true, rest),
                None => (false, entry),
            };
            if !valid_entry_id(body) {
                return None;
            }
            if is_tag {
                set.tag_refs.insert(body.to_string());
            } else {
                set.ids.insert(body.to_string());
            }
        }
    }

    if !saw_values {
        return None;
    }
    Some(set)
}

/// What happens to a crop planted out of season.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OutOfSeasonPolicy {
    /// Growth continues at `off_season_multiplier`.
    SlowGrowth,
    /// Growth stops entirely (`allowed = false`, multiplier `0.0`).
    NoGrowth,
}

/// Declarative seasonal crop fertility configuration.
#[derive(Clone, Debug, PartialEq)]
pub struct CropFertilityRules {
    /// Crops that are in season during spring.
    pub spring_crops: CropTagSet,
    /// Crops that are in season during summer.
    pub summer_crops: CropTagSet,
    /// Crops that are in season during autumn.
    pub autumn_crops: CropTagSet,
    /// Crops that are in season during winter.
    pub winter_crops: CropTagSet,
    /// Crops that grow in every season.
    pub year_round_crops: CropTagSet,
    /// Off-season behaviour (default [`OutOfSeasonPolicy::SlowGrowth`]).
    pub off_season_policy: OutOfSeasonPolicy,
    /// Growth multiplier for [`OutOfSeasonPolicy::SlowGrowth`]
    /// (`0.0..=1.0`, default `0.25`).
    pub off_season_multiplier: f32,
    /// Whether a [`BiomeClimateClass::Tropical`] column lets every crop
    /// grow year-round (default `true` — the jungle doesn't have winter).
    pub tropical_growth_override: bool,
    /// How many blocks above the crop
    /// [`is_under_greenhouse_glass`] scans (default 8).
    pub greenhouse_scan_height: usize,
    /// Block-id patterns counted as greenhouse glass.
    pub greenhouse_glass_patterns: Vec<String>,
}

impl Default for CropFertilityRules {
    fn default() -> Self {
        Self {
            spring_crops: CropTagSet {
                kind: SeasonTagKind::Spring,
                ..CropTagSet::default()
            },
            summer_crops: CropTagSet {
                kind: SeasonTagKind::Summer,
                ..CropTagSet::default()
            },
            autumn_crops: CropTagSet {
                kind: SeasonTagKind::Autumn,
                ..CropTagSet::default()
            },
            winter_crops: CropTagSet {
                kind: SeasonTagKind::Winter,
                ..CropTagSet::default()
            },
            year_round_crops: CropTagSet {
                kind: SeasonTagKind::YearRound,
                ..CropTagSet::default()
            },
            off_season_policy: OutOfSeasonPolicy::SlowGrowth,
            off_season_multiplier: 0.25,
            tropical_growth_override: true,
            greenhouse_scan_height: 8,
            greenhouse_glass_patterns: vec!["greenhouse_glass".to_string()],
        }
    }
}

impl CropFertilityRules {
    /// Clamp `off_season_multiplier` to `0.0..=1.0` and cap
    /// `greenhouse_scan_height` at a sane bound.
    pub fn sanitize(&mut self) {
        self.off_season_multiplier = clamp_unit(self.off_season_multiplier);
        self.greenhouse_scan_height = self.greenhouse_scan_height.min(64);
    }

    /// A sanitized clone; the input is left untouched.
    pub fn sanitized(&self) -> Self {
        let mut out = self.clone();
        out.sanitize();
        out
    }

    /// The crop set in season during `season`.
    pub fn set_for(&self, season: Season) -> &CropTagSet {
        match season {
            Season::Spring => &self.spring_crops,
            Season::Summer => &self.summer_crops,
            Season::Autumn => &self.autumn_crops,
            Season::Winter => &self.winter_crops,
        }
    }

    /// Whether `crop_id` is in season under `state`.
    pub fn in_season(
        &self,
        crop_id: &str,
        crop_tags: &BTreeSet<String>,
        state: &SeasonState,
    ) -> bool {
        self.year_round_crops.contains(crop_id, crop_tags)
            || self.set_for(state.season).contains(crop_id, crop_tags)
    }
}

/// Result of [`evaluate_crop_growth`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CropGrowthEval {
    /// Whether the crop may grow at all right now.
    pub allowed: bool,
    /// Growth speed multiplier (`1.0` in season, reduced or `0.0`
    /// off-season, `1.0` under any override).
    pub growth_multiplier: f32,
    /// Whether the crop is in its listed season.
    pub in_season: bool,
    /// Whether greenhouse glass forced `allowed`/`1.0`.
    pub greenhouse_override: bool,
    /// Whether the tropical year-round exemption applied.
    pub climate_exempt: bool,
}

/// Evaluate one crop's growth against the current season.
///
/// In-season crops grow at `1.0`. Out-of-season crops fall to the
/// configured policy — *unless* `has_greenhouse_glass_above` (greenhouse
/// glass overrides the season) or the column is
/// [`BiomeClimateClass::Tropical`] with `tropical_growth_override`
/// (tropical crops grow year-round in the tropics).
pub fn evaluate_crop_growth(
    crop_id: &str,
    crop_tags: &BTreeSet<String>,
    state: &SeasonState,
    climate: BiomeClimateClass,
    has_greenhouse_glass_above: bool,
    rules: &CropFertilityRules,
) -> CropGrowthEval {
    let rules = rules.sanitized();
    let in_season = rules.in_season(crop_id, crop_tags, state);

    if in_season {
        return CropGrowthEval {
            allowed: true,
            growth_multiplier: 1.0,
            in_season,
            greenhouse_override: false,
            climate_exempt: false,
        };
    }
    if has_greenhouse_glass_above {
        return CropGrowthEval {
            allowed: true,
            growth_multiplier: 1.0,
            in_season,
            greenhouse_override: true,
            climate_exempt: false,
        };
    }
    if climate == BiomeClimateClass::Tropical && rules.tropical_growth_override {
        return CropGrowthEval {
            allowed: true,
            growth_multiplier: 1.0,
            in_season,
            greenhouse_override: false,
            climate_exempt: true,
        };
    }
    match rules.off_season_policy {
        OutOfSeasonPolicy::SlowGrowth => CropGrowthEval {
            allowed: true,
            growth_multiplier: rules.off_season_multiplier,
            in_season,
            greenhouse_override: false,
            climate_exempt: false,
        },
        OutOfSeasonPolicy::NoGrowth => CropGrowthEval {
            allowed: false,
            growth_multiplier: 0.0,
            in_season,
            greenhouse_override: false,
            climate_exempt: false,
        },
    }
}

/// Default greenhouse glass scan height.
pub const DEFAULT_GREENHOUSE_SCAN_HEIGHT: usize = 8;

/// Block-id patterns that let light through — anything not matching is
/// treated as an opaque sky blocker and stops the scan (fail closed:
/// unknown blocks deny the greenhouse bonus rather than grant it).
const TRANSPARENT_PATTERNS: &[&str] = &[
    "air",
    "glass",
    "pane",
    "leaves",
    "fence",
    "torch",
    "lantern",
    "vine",
    "ladder",
    "rail",
    "sign",
    "banner",
    "sapling",
    "flower",
    "tulip",
    "fern",
    "tall_grass",
    "short_grass",
    "mushroom",
    "chain",
    "rod",
    "candle",
];

fn is_air(id: &str) -> bool {
    matches!(
        id,
        "air"
            | "cave_air"
            | "void_air"
            | "minecraft:air"
            | "minecraft:cave_air"
            | "minecraft:void_air"
    ) || id.ends_with(":air")
        || id.ends_with(":cave_air")
        || id.ends_with(":void_air")
}

/// Scan the column above a crop for greenhouse glass.
///
/// Walks `blocks_above` (bottom-to-top order) for at most `max_height`
/// entries — pass [`DEFAULT_GREENHOUSE_SCAN_HEIGHT`] or
/// [`CropFertilityRules::greenhouse_scan_height`]. Returns `true` on the
/// first entry whose id contains `greenhouse_glass` (a `#greenhouse_glass`
/// tag-style entry is accepted too). Air and known-transparent blocks are
/// skipped; the first unrecognized (opaque) block stops the scan — a stone
/// roof above the glass denies the bonus.
pub fn is_under_greenhouse_glass(blocks_above: &[&str], max_height: usize) -> bool {
    for raw in blocks_above.iter().take(max_height) {
        let id = raw.strip_prefix('#').unwrap_or(raw).to_ascii_lowercase();
        if id.contains("greenhouse_glass") {
            return true;
        }
        if is_air(&id) {
            continue;
        }
        if TRANSPARENT_PATTERNS.iter().any(|p| id.contains(p)) {
            continue;
        }
        return false;
    }
    false
}

// ===========================================================================
// 5. SSN1 wire codec (channel `megu:season_v1`)
// ===========================================================================

/// Custom-payload channel for `SSN1` season sync frames.
pub const SEASON_CHANNEL: &str = "megu:season_v1";
/// 4-byte frame magic.
pub const SSN1_MAGIC: &[u8; 4] = b"SSN1";
/// Wire format version.
pub const SSN1_VERSION: u8 = 1;
/// Exact `SSN1` frame length in bytes.
pub const SSN1_FRAME_LEN: usize = 19;

const FLAG_TROPICAL_DIM: u8 = 1 << 0;
const FLAG_FROZEN_SEASON: u8 = 1 << 1;
const FLAG_SNOW_ENABLED: u8 = 1 << 2;
const FLAG_KNOWN_MASK: u8 = FLAG_TROPICAL_DIM | FLAG_FROZEN_SEASON | FLAG_SNOW_ENABLED;

/// Season sync payload carried by one `SSN1` frame.
///
/// `progress_in_sub_season` and `daylight_fraction` cross the wire as
/// `q16` fixed point (`value * 65535`, rounded, clamped); the RGB
/// multipliers are quantized to bytes by the caller's tint pipeline or by
/// [`SeasonSyncState::from_state`].
#[derive(Clone, Debug, PartialEq)]
pub struct SeasonSyncState {
    /// Current sub-season.
    pub sub_season: SubSeason,
    /// Progress through the sub-season, `0.0..=1.0`.
    pub progress_in_sub_season: f32,
    /// Whole days elapsed inside the current broad season.
    pub day_of_season: u16,
    /// Daylight fraction, `0.0..=1.0`.
    pub daylight_fraction: f32,
    /// Quantized foliage RGB multiplier (`255 = 1.0`).
    pub foliage_rgb: [u8; 3],
    /// Quantized grass RGB multiplier (`255 = 1.0`).
    pub grass_rgb: [u8; 3],
    /// Dimension runs wet/dry seasons (tropical climate).
    pub tropical_dim: bool,
    /// Calendar is locked (`fixed_sub_season` set).
    pub frozen_season: bool,
    /// Seasonal snow/freeze is enabled on this dimension.
    pub snow_enabled: bool,
}

impl SeasonSyncState {
    /// Build a wire state from a [`SeasonState`] plus already-computed
    /// `[f32; 3]` tint multipliers (each channel clamped `0.0..=1.0`,
    /// non-finite -> `0.0`) and the three flags.
    pub fn from_state(
        state: &SeasonState,
        foliage_rgb: [f32; 3],
        grass_rgb: [f32; 3],
        tropical_dim: bool,
        frozen_season: bool,
        snow_enabled: bool,
    ) -> SeasonSyncState {
        let q = |c: f32| (clamp_unit(c) * 255.0).round() as u8;
        SeasonSyncState {
            sub_season: state.sub_season,
            progress_in_sub_season: clamp_unit(state.progress_in_sub_season),
            day_of_season: state.day_of_season,
            daylight_fraction: clamp_unit(state.daylight_fraction),
            foliage_rgb: [q(foliage_rgb[0]), q(foliage_rgb[1]), q(foliage_rgb[2])],
            grass_rgb: [q(grass_rgb[0]), q(grass_rgb[1]), q(grass_rgb[2])],
            tropical_dim,
            frozen_season,
            snow_enabled,
        }
    }
}

/// Quantize a unit float to `q16`; non-finite input becomes `0`.
fn write_q16(out: &mut Vec<u8>, v: f32) {
    let q = (clamp_unit(v) * 65535.0).round() as u16;
    out.extend_from_slice(&q.to_be_bytes());
}

fn read_q16(bytes: &[u8], cursor: &mut usize) -> Option<f32> {
    let slice = bytes.get(*cursor..*cursor + 2)?;
    *cursor += 2;
    Some(f32::from(u16::from_be_bytes(slice.try_into().ok()?)) / 65535.0)
}

/// Encode `state` as a fixed 19-byte big-endian `SSN1` frame:
/// `SSN1 | u8 version | u8 sub_season | u16 progress_q16 | u16
/// day_of_season | u16 daylight_q16 | u8 foliage_rgb[3] | u8 grass_rgb[3]
/// | u8 flags`. Encoding a valid [`SeasonSyncState`] cannot fail.
pub fn encode_ssn1(state: &SeasonSyncState) -> Vec<u8> {
    let mut out = Vec::with_capacity(SSN1_FRAME_LEN);
    out.extend_from_slice(SSN1_MAGIC);
    out.push(SSN1_VERSION);
    out.push(state.sub_season.index());
    write_q16(&mut out, state.progress_in_sub_season);
    out.extend_from_slice(&state.day_of_season.to_be_bytes());
    write_q16(&mut out, state.daylight_fraction);
    out.extend_from_slice(&state.foliage_rgb);
    out.extend_from_slice(&state.grass_rgb);
    let mut flags = 0u8;
    if state.tropical_dim {
        flags |= FLAG_TROPICAL_DIM;
    }
    if state.frozen_season {
        flags |= FLAG_FROZEN_SEASON;
    }
    if state.snow_enabled {
        flags |= FLAG_SNOW_ENABLED;
    }
    out.push(flags);
    out
}

/// Decode an `SSN1` frame, fail-closed: `None` on wrong length, bad magic,
/// wrong version, `sub_season >= 12`, or any flag bit outside the known
/// mask.
pub fn decode_ssn1(bytes: &[u8]) -> Option<SeasonSyncState> {
    if bytes.len() != SSN1_FRAME_LEN {
        return None;
    }
    if bytes.get(..4)? != SSN1_MAGIC {
        return None;
    }
    let mut cursor = 4usize;
    if *bytes.get(cursor)? != SSN1_VERSION {
        return None;
    }
    cursor += 1;
    let sub_season = SubSeason::from_index(*bytes.get(cursor)?)?;
    cursor += 1;
    let progress_in_sub_season = read_q16(bytes, &mut cursor)?;
    let day_of_season = u16::from_be_bytes(bytes.get(cursor..cursor + 2)?.try_into().ok()?);
    cursor += 2;
    let daylight_fraction = read_q16(bytes, &mut cursor)?;
    let foliage_rgb: [u8; 3] = bytes.get(cursor..cursor + 3)?.try_into().ok()?;
    cursor += 3;
    let grass_rgb: [u8; 3] = bytes.get(cursor..cursor + 3)?.try_into().ok()?;
    cursor += 3;
    let flags = *bytes.get(cursor)?;
    cursor += 1;
    if flags & !FLAG_KNOWN_MASK != 0 {
        return None;
    }
    debug_assert_eq!(cursor, SSN1_FRAME_LEN);
    Some(SeasonSyncState {
        sub_season,
        progress_in_sub_season,
        day_of_season,
        daylight_fraction,
        foliage_rgb,
        grass_rgb,
        tropical_dim: flags & FLAG_TROPICAL_DIM != 0,
        frozen_season: flags & FLAG_FROZEN_SEASON != 0,
        snow_enabled: flags & FLAG_SNOW_ENABLED != 0,
    })
}
