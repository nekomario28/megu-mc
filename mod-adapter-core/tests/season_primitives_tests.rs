// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Integration tests for [`mod_adapter_core::season_primitives`]:
//! the 12-sub-season calendar and daylight curve, biome climate /
//! snow / thaw evaluation, smooth foliage & grass tinting, seasonal crop
//! fertility with the greenhouse-glass override, and the fail-closed
//! `SSN1` wire codec. All biome ids and crop ids below are vanilla or
//! invented test data — no real season mod is referenced.

use std::collections::BTreeSet;

use mod_adapter_core::season_primitives::*;

fn cfg() -> SeasonCalendarConfig {
    SeasonCalendarConfig::default()
}

fn rules() -> BiomeClimateRules {
    BiomeClimateRules::default()
}

/// World tick at `day` days plus `offset` ticks into the day.
fn tick_at(day: u64, offset: u64) -> u64 {
    day * DEFAULT_TICKS_PER_DAY + offset
}

/// State at the exact centre (progress = 0.5) of `sub`.
fn mid(sub: SubSeason, cfg: &SeasonCalendarConfig) -> SeasonState {
    let day = u64::from(sub.index()) * u64::from(cfg.days_per_sub_season)
        + u64::from(cfg.days_per_sub_season) / 2;
    SeasonState::from_world_ticks(tick_at(day, 0), cfg)
}

fn tags(ids: &[&str]) -> BTreeSet<String> {
    ids.iter().map(|s| (*s).to_string()).collect()
}

fn no_tags() -> BTreeSet<String> {
    BTreeSet::new()
}

// ===========================================================================
// 1. Calendar
// ===========================================================================

#[test]
fn calendar_starts_at_early_spring() {
    let s = SeasonState::from_world_ticks(0, &cfg());
    assert_eq!(s.sub_season, SubSeason::EarlySpring);
    assert_eq!(s.season, Season::Spring);
    assert_eq!(s.progress_in_sub_season, 0.0);
    assert_eq!(s.day_of_year, 0);
    assert_eq!(s.day_of_season, 0);
}

#[test]
fn calendar_progresses_through_all_twelve_sub_seasons() {
    let cfg = cfg();
    let mut seen = Vec::new();
    for i in 0..12u64 {
        let s = SeasonState::from_world_ticks(tick_at(i * 8, 0), &cfg);
        seen.push(s.sub_season);
        assert_eq!(s.sub_season, SubSeason::from_index(i as u8).unwrap());
        assert_eq!(s.season, s.sub_season.season());
        assert_eq!(s.day_in_sub_season, 0);
    }
    assert_eq!(&seen[..3], &SubSeason::ALL[..3]);
    assert_eq!(seen[9], SubSeason::EarlyWinter);
    assert_eq!(seen[11], SubSeason::LateWinter);
}

#[test]
fn calendar_wraps_year_boundary() {
    let cfg = cfg();
    // Day 95 is the last LateWinter day; day 96 begins a new EarlySpring.
    let last = SeasonState::from_world_ticks(tick_at(95, 23_999), &cfg);
    assert_eq!(last.sub_season, SubSeason::LateWinter);
    let first = SeasonState::from_world_ticks(tick_at(96, 0), &cfg);
    assert_eq!(first.sub_season, SubSeason::EarlySpring);
    assert_eq!(first.day_of_year, 0);
    assert!((0.0..1.0).contains(&first.progress_in_sub_season));
}

#[test]
fn calendar_progress_tracks_partial_days() {
    let cfg = cfg();
    // Day 4 of EarlySpring, half the day elapsed: (4 + 0.5) / 8.
    let s = SeasonState::from_world_ticks(tick_at(4, 12_000), &cfg);
    assert_eq!(s.sub_season, SubSeason::EarlySpring);
    assert!((s.progress_in_sub_season - 0.5625).abs() < 1e-6);
    assert_eq!(s.day_in_sub_season, 4);
    assert_eq!(s.day_of_season, 4);
    assert_eq!(s.day_of_year, 4);
    // Progress always stays inside 0.0..1.0 at sub-season edges.
    for day in 0..96u64 {
        for off in [0u64, 11_999, 12_000, 23_999] {
            let p = SeasonState::from_world_ticks(tick_at(day, off), &cfg).progress_in_sub_season;
            assert!((0.0..1.0).contains(&p), "progress {p} out of range");
        }
    }
}

#[test]
fn calendar_fixed_sub_season_locks_state() {
    let cfg = SeasonCalendarConfig {
        fixed_sub_season: Some(SubSeason::MidWinter),
        ..cfg()
    };
    for t in [0u64, 100_000, u64::MAX / 2] {
        let s = SeasonState::from_world_ticks(t, &cfg);
        assert_eq!(s.sub_season, SubSeason::MidWinter);
        assert_eq!(s.season, Season::Winter);
        assert_eq!(s.progress_in_sub_season, 0.0);
    }
}

#[test]
fn calendar_dimension_allowlist() {
    let cfg = cfg();
    assert!(cfg.is_dimension_enabled("minecraft:overworld"));
    assert!(!cfg.is_dimension_enabled("minecraft:the_nether"));
    assert!(!cfg.is_dimension_enabled("minecraft:the_end"));

    let mut opted_in = cfg.clone();
    opted_in
        .enabled_dimensions
        .insert("minecraft:the_nether".to_string());
    assert!(opted_in.is_dimension_enabled("minecraft:the_nether"));
    assert!(!opted_in.is_dimension_enabled("minecraft:the_end"));
}

#[test]
fn calendar_sanitizes_garbage_config() {
    let cfg = SeasonCalendarConfig {
        days_per_sub_season: 0,
        ticks_per_day: 0,
        daylight_amplitude: f32::NAN,
        ..cfg()
    };
    let s = SeasonState::from_world_ticks(123_456, &cfg);
    assert!(s.progress_in_sub_season.is_finite());
    assert!(s.daylight_fraction.is_finite());
}

#[test]
fn daylight_curve_peaks_summer_troughs_winter() {
    let cfg = cfg();
    let summer = mid(SubSeason::MidSummer, &cfg).daylight_fraction;
    let winter = mid(SubSeason::MidWinter, &cfg).daylight_fraction;
    let spring = mid(SubSeason::MidSpring, &cfg).daylight_fraction;
    let autumn = mid(SubSeason::MidAutumn, &cfg).daylight_fraction;

    assert!((summer - 0.62).abs() < 0.01, "summer {summer}");
    assert!((winter - 0.38).abs() < 0.01, "winter {winter}");
    // Equinox points sit at ~0.5 on the cosine curve.
    assert!((spring - 0.5).abs() < 0.02, "spring {spring}");
    assert!((autumn - 0.5).abs() < 0.02, "autumn {autumn}");
    assert!(summer > spring && summer > autumn && winter < spring && winter < autumn);
}

#[test]
fn daylight_curve_is_continuous_and_bounded() {
    let cfg = cfg();
    let mut prev = SeasonState::from_world_ticks(0, &cfg).daylight_fraction;
    for t in (6_000..tick_at(96, 0)).step_by(6_000) {
        let d = SeasonState::from_world_ticks(t, &cfg).daylight_fraction;
        assert!(d.is_finite() && (0.0..=1.0).contains(&d));
        assert!((d - prev).abs() < 0.01, "daylight jump {prev} -> {d}");
        prev = d;
    }
}

#[test]
fn daylight_curve_disabled_is_neutral() {
    let cfg = SeasonCalendarConfig {
        enable_daylight_cycle_shift: false,
        ..cfg()
    };
    for sub in SubSeason::ALL {
        assert_eq!(mid(sub, &cfg).daylight_fraction, 0.5);
    }
}

// ===========================================================================
// 2. Biome climate, snow/ice & thaw
// ===========================================================================

fn eval(biome: &str, temp: f32, y: f32, sub: SubSeason) -> SeasonalWeatherEval {
    let cfg = cfg();
    let state = mid(sub, &cfg);
    evaluate_biome_weather(biome, temp, y, &state, &rules())
}

#[test]
fn temperate_plains_snows_and_freezes_in_midwinter() {
    let e = eval("minecraft:plains", 0.8, 64.0, SubSeason::MidWinter);
    assert_eq!(e.climate, BiomeClimateClass::Temperate);
    assert_eq!(e.precipitation, PrecipitationKind::Snow);
    assert!(e.can_freeze_water);
    assert!(e.snow_cover_expected);
    assert!(!e.should_melt_seasonal_snow);
    assert!(e.effective_temp < 0.15);
}

#[test]
fn temperate_plains_melts_in_spring() {
    let e = eval("minecraft:plains", 0.8, 64.0, SubSeason::MidSpring);
    assert_eq!(e.precipitation, PrecipitationKind::Rain);
    assert!(!e.can_freeze_water);
    assert!(e.should_melt_seasonal_snow);
}

#[test]
fn tropical_jungle_never_snows() {
    for sub in SubSeason::ALL {
        let e = eval("minecraft:jungle", 0.95, 70.0, sub);
        assert_eq!(e.climate, BiomeClimateClass::Tropical);
        assert_ne!(e.precipitation, PrecipitationKind::Snow, "snow in {sub:?}");
        assert!(!e.can_freeze_water);
        assert!(!e.should_melt_seasonal_snow);
    }
    // Wet season (summer..late autumn) rains; dry season does not.
    assert_eq!(
        eval("minecraft:jungle", 0.95, 70.0, SubSeason::MidSummer).precipitation,
        PrecipitationKind::Rain
    );
    assert_eq!(
        eval("minecraft:jungle", 0.95, 70.0, SubSeason::MidWinter).precipitation,
        PrecipitationKind::None
    );
}

#[test]
fn tropical_desert_is_dry_even_in_wet_season() {
    for sub in SubSeason::ALL {
        let e = eval("minecraft:desert", 2.0, 64.0, sub);
        assert_eq!(e.climate, BiomeClimateClass::Tropical);
        assert_eq!(e.precipitation, PrecipitationKind::None);
    }
}

#[test]
fn snowy_peaks_never_flags_seasonal_melt() {
    // Naturally snowy biomes keep their cover: base temp far below the
    // natural-snow line means every thaw flag stays off year-round.
    for sub in SubSeason::ALL {
        let e = eval("minecraft:snowy_peaks", -0.7, 160.0, sub);
        assert_eq!(e.climate, BiomeClimateClass::Cold);
        assert!(!e.should_melt_seasonal_snow, "melt in {sub:?}");
    }
}

#[test]
fn cold_taiga_gets_only_a_brief_summer_thaw() {
    let spring = eval("minecraft:taiga", 0.25, 64.0, SubSeason::MidSpring);
    assert_eq!(spring.climate, BiomeClimateClass::Cold);
    assert!(!spring.should_melt_seasonal_snow);

    let summer = eval("minecraft:taiga", 0.25, 64.0, SubSeason::MidSummer);
    assert!(summer.should_melt_seasonal_snow);
    assert_eq!(summer.precipitation, PrecipitationKind::Rain);

    let winter = eval("minecraft:taiga", 0.25, 64.0, SubSeason::MidWinter);
    assert!(winter.can_freeze_water);
    assert!(!winter.should_melt_seasonal_snow);
}

#[test]
fn deep_dark_stays_seasonless() {
    for sub in SubSeason::ALL {
        let e = eval("minecraft:deep_dark", 0.8, -20.0, sub);
        assert_eq!(e.climate, BiomeClimateClass::Seasonless);
        assert_eq!(e.precipitation, PrecipitationKind::None);
        assert!(!e.can_freeze_water);
        assert!(!e.snow_cover_expected);
        assert!(!e.should_melt_seasonal_snow);
    }
}

#[test]
fn nether_and_end_are_seasonless_by_default() {
    for (biome, temp) in [
        ("minecraft:nether_wastes", 2.0),
        ("minecraft:the_end", 0.5),
        ("minecraft:end_highlands", 0.5),
    ] {
        let e = eval(biome, temp, 64.0, SubSeason::MidWinter);
        assert_eq!(e.climate, BiomeClimateClass::Seasonless, "{biome}");
    }
}

#[test]
fn altitude_lapse_keeps_peaks_snowy() {
    // Same temperate biome id, warm feet, frozen summit.
    let low = eval("minecraft:meadow", 0.5, 64.0, SubSeason::MidSpring);
    let high = eval("minecraft:meadow", 0.5, 320.0, SubSeason::MidSpring);
    assert!(high.effective_temp < low.effective_temp);
}

// ===========================================================================
// 3. Seasonal tint
// ===========================================================================

#[test]
fn tropical_and_seasonless_tints_are_neutral() {
    let cfg = cfg();
    let tint_rules = SeasonalTintRules::default();
    for sub in SubSeason::ALL {
        let s = mid(sub, &cfg);
        for climate in [BiomeClimateClass::Tropical, BiomeClimateClass::Seasonless] {
            assert_eq!(
                seasonal_foliage_tint(&s, climate, "minecraft:jungle", &tint_rules),
                [1.0, 1.0, 1.0]
            );
            assert_eq!(
                seasonal_grass_tint(&s, climate, &tint_rules),
                [1.0, 1.0, 1.0]
            );
        }
    }
}

#[test]
fn autumn_foliage_shifts_warm() {
    let cfg = cfg();
    let s = mid(SubSeason::MidAutumn, &cfg);
    let t = seasonal_foliage_tint(
        &s,
        BiomeClimateClass::Temperate,
        "minecraft:plains",
        &SeasonalTintRules::default(),
    );
    assert!(t[0] > 1.05, "red boost expected, got {t:?}");
    assert!(t[2] < 0.7, "blue cut expected, got {t:?}");
    assert!(t[0] > t[1] && t[1] > t[2], "warm ordering broken: {t:?}");
}

#[test]
fn winter_foliage_desaturates() {
    let cfg = cfg();
    let s = mid(SubSeason::MidWinter, &cfg);
    let t = seasonal_foliage_tint(
        &s,
        BiomeClimateClass::Temperate,
        "minecraft:plains",
        &SeasonalTintRules::default(),
    );
    let spread =
        t.iter().copied().fold(f32::MIN, f32::max) - t.iter().copied().fold(f32::MAX, f32::min);
    assert!(spread < 0.15, "winter tint not desaturated: {t:?}");
}

#[test]
fn evergreen_biomes_keep_foliage_year_round() {
    let cfg = cfg();
    let tint_rules = SeasonalTintRules::default();
    for sub in SubSeason::ALL {
        let s = mid(sub, &cfg);
        for biome in [
            "anyforest:spruce_wood",
            "minecraft:taiga",
            "anyland:cherry_grove",
        ] {
            assert_eq!(
                seasonal_foliage_tint(&s, BiomeClimateClass::Temperate, biome, &tint_rules),
                [1.0, 1.0, 1.0],
                "{biome} retinted in {sub:?}"
            );
        }
    }
}

#[test]
fn autumn_strength_zero_disables_foliage_shift() {
    let cfg = cfg();
    let s = mid(SubSeason::MidAutumn, &cfg);
    let tint_rules = SeasonalTintRules {
        autumn_foliage_strength: 0.0,
        ..SeasonalTintRules::default()
    };
    let t = seasonal_foliage_tint(
        &s,
        BiomeClimateClass::Temperate,
        "minecraft:plains",
        &tint_rules,
    );
    for c in t {
        assert!((c - 1.0).abs() < 0.15, "strength-0 autumn drifted: {t:?}");
    }
}

#[test]
fn tint_interpolates_continuously_across_wrap() {
    let cfg = cfg();
    let tint_rules = SeasonalTintRules::default();
    let mut prev = seasonal_foliage_tint(
        &SeasonState::from_world_ticks(0, &cfg),
        BiomeClimateClass::Temperate,
        "minecraft:plains",
        &tint_rules,
    );
    // Sweep a full year plus a day at 6-hour granularity — every
    // transition including LateWinter -> EarlySpring must be smooth.
    for t in (6_000..=tick_at(97, 0)).step_by(6_000) {
        let s = SeasonState::from_world_ticks(t, &cfg);
        let cur = seasonal_foliage_tint(
            &s,
            BiomeClimateClass::Temperate,
            "minecraft:plains",
            &tint_rules,
        );
        for k in 0..3 {
            assert!(cur[k].is_finite(), "non-finite tint {cur:?} at t={t}");
            assert!(
                (cur[k] - prev[k]).abs() < 0.05,
                "tint jump {prev:?} -> {cur:?} at t={t}"
            );
        }
        prev = cur;
    }
}

// ===========================================================================
// 4. Crop fertility & greenhouse glass
// ===========================================================================

fn crop_rules() -> CropFertilityRules {
    CropFertilityRules {
        spring_crops: load_season_tag_json(
            SeasonTagKind::Spring,
            r##"{"values": ["minecraft:wheat", "farmmod:lettuce", "#c:crops/spring"]}"##,
        )
        .unwrap(),
        summer_crops: load_season_tag_json(
            SeasonTagKind::Summer,
            r##"{"replace": false, "values": ["farmmod:corn", "#c:crops/corn"]}"##,
        )
        .unwrap(),
        autumn_crops: load_season_tag_json(
            SeasonTagKind::Autumn,
            r#"{"values": ["farmmod:pumpkin"]}"#,
        )
        .unwrap(),
        winter_crops: load_season_tag_json(
            SeasonTagKind::Winter,
            r#"{"values": ["farmmod:winter_rye"]}"#,
        )
        .unwrap(),
        year_round_crops: load_season_tag_json(
            SeasonTagKind::YearRound,
            r#"{"values": ["minecraft:carrot"]}"#,
        )
        .unwrap(),
        ..CropFertilityRules::default()
    }
}

#[test]
fn tag_json_loader_parses_ids_and_tag_refs() {
    let set = load_season_tag_json(
        SeasonTagKind::Summer,
        r##"{
            "replace": false,
            "values": ["minecraft:melon", "#c:crops/corn", "  spaced  "]
        }"##,
    );
    // Entries must be bare identifiers — whitespace fails closed.
    assert!(set.is_none());

    let set = load_season_tag_json(
        SeasonTagKind::Summer,
        r##"{"values": ["minecraft:melon", "#c:crops/corn"]}"##,
    )
    .unwrap();
    assert_eq!(set.kind, SeasonTagKind::Summer);
    assert!(set.ids.contains("minecraft:melon"));
    assert!(set.tag_refs.contains("c:crops/corn"));
    // Literal membership.
    assert!(set.contains("minecraft:melon", &no_tags()));
    // Tag membership via a crop's declared tags.
    assert!(set.contains("farmmod:sweetcorn", &tags(&["c:crops/corn"])));
    assert!(!set.contains("farmmod:beet", &tags(&["c:crops/beets"])));
}

#[test]
fn tag_json_loader_rejects_malformed_input() {
    for bad in [
        "",
        "not json",
        "{}",
        r#"{"values": "oops"}"#,
        r#"{"values": ["ok", 42]}"#,
        r#"{"values": ["ok", ]}"#,
        r#"{"values": ["ok"]} trailing"#,
        r#"{"values": [""]}"#,
        r#"{"values": ["has space"]}"#,
        r#"{"values": ["unclosed"]"#,
        r#"["not", "an", "object"]"#,
    ] {
        assert!(
            load_season_tag_json(SeasonTagKind::Spring, bad).is_none(),
            "accepted malformed tag json: {bad}"
        );
    }
}

#[test]
fn tag_json_loader_ignores_unknown_keys() {
    let set = load_season_tag_json(
        SeasonTagKind::Winter,
        r#"{"fabric:load_conditions": [{"condition": "x", "values": [1, {"a": [2]}]}],
            "optional": true,
            "values": ["minecraft:snowball"]}"#,
    )
    .unwrap();
    assert!(set.ids.contains("minecraft:snowball"));
}

#[test]
fn in_season_crops_grow_normally() {
    let cfg = cfg();
    let r = crop_rules();
    let spring = mid(SubSeason::MidSpring, &cfg);
    let e = evaluate_crop_growth(
        "minecraft:wheat",
        &no_tags(),
        &spring,
        BiomeClimateClass::Temperate,
        false,
        &r,
    );
    assert!(e.allowed && e.in_season);
    assert_eq!(e.growth_multiplier, 1.0);

    // Tag-referenced membership counts too.
    let e = evaluate_crop_growth(
        "farmmod:sweetcorn",
        &tags(&["c:crops/corn"]),
        &mid(SubSeason::MidSummer, &cfg),
        BiomeClimateClass::Temperate,
        false,
        &r,
    );
    assert!(e.in_season && e.growth_multiplier == 1.0);

    // Year-round crops ignore the season entirely.
    for sub in SubSeason::ALL {
        let e = evaluate_crop_growth(
            "minecraft:carrot",
            &no_tags(),
            &mid(sub, &cfg),
            BiomeClimateClass::Temperate,
            false,
            &r,
        );
        assert!(
            e.allowed && e.growth_multiplier == 1.0,
            "carrot failed in {sub:?}"
        );
    }
}

#[test]
fn out_of_season_crops_slow_or_stop_by_policy() {
    let cfg = cfg();
    let winter = mid(SubSeason::MidWinter, &cfg);
    let mut r = crop_rules();
    let e = evaluate_crop_growth(
        "minecraft:wheat",
        &no_tags(),
        &winter,
        BiomeClimateClass::Temperate,
        false,
        &r,
    );
    assert!(e.allowed && !e.in_season);
    assert!((e.growth_multiplier - 0.25).abs() < 1e-6);

    r.off_season_policy = OutOfSeasonPolicy::NoGrowth;
    let e = evaluate_crop_growth(
        "minecraft:wheat",
        &no_tags(),
        &winter,
        BiomeClimateClass::Temperate,
        false,
        &r,
    );
    assert!(!e.allowed && e.growth_multiplier == 0.0);
}

#[test]
fn greenhouse_glass_overrides_season() {
    let cfg = cfg();
    let winter = mid(SubSeason::MidWinter, &cfg);
    let r = CropFertilityRules {
        off_season_policy: OutOfSeasonPolicy::NoGrowth,
        ..crop_rules()
    };
    let e = evaluate_crop_growth(
        "minecraft:wheat",
        &no_tags(),
        &winter,
        BiomeClimateClass::Temperate,
        true,
        &r,
    );
    assert!(e.allowed && e.greenhouse_override && e.growth_multiplier == 1.0);
}

#[test]
fn tropical_climate_grows_year_round() {
    let cfg = cfg();
    let r = crop_rules();
    let e = evaluate_crop_growth(
        "minecraft:wheat",
        &no_tags(),
        &mid(SubSeason::MidWinter, &cfg),
        BiomeClimateClass::Tropical,
        false,
        &r,
    );
    assert!(e.allowed && e.climate_exempt && e.growth_multiplier == 1.0);
}

#[test]
fn greenhouse_scan_finds_glass_through_air_and_transparents() {
    assert!(is_under_greenhouse_glass(
        &["minecraft:air", "minecraft:air", "mymod:greenhouse_glass"],
        8
    ));
    assert!(is_under_greenhouse_glass(
        &["minecraft:oak_leaves", "mymod:greenhouse_glass"],
        8
    ));
    assert!(is_under_greenhouse_glass(&["#mymod:greenhouse_glass"], 8));
    // Opaque blocker between crop and glass denies the bonus.
    assert!(!is_under_greenhouse_glass(
        &["minecraft:stone", "mymod:greenhouse_glass"],
        8
    ));
    // Glass beyond the scan height doesn't count.
    assert!(!is_under_greenhouse_glass(
        &["minecraft:air", "mymod:greenhouse_glass"],
        1
    ));
    assert!(!is_under_greenhouse_glass(&["minecraft:air"; 9], 8));
}

// ===========================================================================
// 5. SSN1 wire codec
// ===========================================================================

fn wire_state() -> SeasonSyncState {
    SeasonSyncState {
        sub_season: SubSeason::MidAutumn,
        progress_in_sub_season: 0.5,
        day_of_season: 13,
        daylight_fraction: 0.5,
        foliage_rgb: [255, 200, 100],
        grass_rgb: [240, 190, 90],
        tropical_dim: false,
        frozen_season: true,
        snow_enabled: true,
    }
}

#[test]
fn ssn1_round_trip() {
    let state = wire_state();
    let frame = encode_ssn1(&state);
    assert_eq!(frame.len(), SSN1_FRAME_LEN);
    assert_eq!(&frame[..4], b"SSN1");
    assert_eq!(frame[4], 1);
    assert_eq!(frame[5], SubSeason::MidAutumn.index());

    let back = decode_ssn1(&frame).unwrap();
    assert_eq!(back.sub_season, state.sub_season);
    assert!((back.progress_in_sub_season - state.progress_in_sub_season).abs() < 1e-4);
    assert_eq!(back.day_of_season, state.day_of_season);
    assert!((back.daylight_fraction - state.daylight_fraction).abs() < 1e-4);
    assert_eq!(back.foliage_rgb, state.foliage_rgb);
    assert_eq!(back.grass_rgb, state.grass_rgb);
    assert!(back.frozen_season && back.snow_enabled && !back.tropical_dim);

    // Encoding is deterministic: decode -> encode reproduces the frame.
    assert_eq!(encode_ssn1(&back), frame);
}

#[test]
fn ssn1_from_state_quantizes_tints() {
    let cfg = cfg();
    let s = mid(SubSeason::MidSummer, &cfg);
    let sync = SeasonSyncState::from_state(
        &s,
        [1.2, 0.5, -3.0],     // clamps to 255 / ~128 / 0
        [f32::NAN, 0.0, 1.0], // NaN -> 0
        false,
        false,
        true,
    );
    assert_eq!(sync.foliage_rgb, [255, 128, 0]);
    assert_eq!(sync.grass_rgb, [0, 0, 255]);
    let frame = encode_ssn1(&sync);
    let back = decode_ssn1(&frame).unwrap();
    assert!((back.progress_in_sub_season - sync.progress_in_sub_season).abs() < 1e-4);
    assert!((back.daylight_fraction - sync.daylight_fraction).abs() < 1e-4);
    assert_eq!(back.sub_season, sync.sub_season);
    assert_eq!(back.foliage_rgb, sync.foliage_rgb);
    assert_eq!(back.grass_rgb, sync.grass_rgb);
    // The decoded (already quantized) form re-encodes byte-identically.
    assert_eq!(encode_ssn1(&back), frame);
}

#[test]
fn ssn1_rejects_hostile_frames() {
    let good = encode_ssn1(&wire_state());

    // Wrong magic.
    let mut bad = good.clone();
    bad[0] = b'X';
    assert!(decode_ssn1(&bad).is_none());

    // Wrong version.
    let mut bad = good.clone();
    bad[4] = 2;
    assert!(decode_ssn1(&bad).is_none());

    // Out-of-range sub-season (>= 12).
    let mut bad = good.clone();
    bad[5] = 12;
    assert!(decode_ssn1(&bad).is_none());
    let mut bad = good.clone();
    bad[5] = 255;
    assert!(decode_ssn1(&bad).is_none());

    // Unknown flag bits.
    let mut bad = good.clone();
    bad[SSN1_FRAME_LEN - 1] = 0xF8;
    assert!(decode_ssn1(&bad).is_none());

    // Truncated at every length below the frame size.
    for len in 0..SSN1_FRAME_LEN {
        assert!(decode_ssn1(&good[..len]).is_none(), "len {len} accepted");
    }
    // Any trailing byte.
    let mut bad = good.clone();
    bad.push(0);
    assert!(decode_ssn1(&bad).is_none());
}
