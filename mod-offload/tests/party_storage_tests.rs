// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Integration coverage for the generic collector/companion primitives:
//! party/PC-box slot conservation, the seeded capture roll, growth-curve
//! leveling, declarative evolution triggers, and the fail-closed `PRTY`
//! codec.

use std::collections::BTreeSet;

use mod_offload::party_storage::{
    award_experience, decode_storage, eligible_evolutions, encode_storage,
    evaluate_capture_attempt, experience_for_level, level_for_experience, CaptureDeviceSpec,
    CaptureOutcome, CompanionRecord, DayPhase, EvolutionCondition, EvolutionContext,
    EvolutionRule, GrowthCurve, PlayerCompanionStorage, RecordError, StorageDecodeError,
    StorageError, StorageSlotRef, BOX_SLOTS, DEFAULT_BOX_COUNT, GUARANTEED_CAPTURE_MULTIPLIER,
    MAX_BOX_COUNT, MAX_LEVEL, PARTY_SLOTS, PRTY_SCHEMA_VERSION, SHAKE_CHECKS,
};

// ---------------------------------------------------------------- helpers

/// A deterministic IEEE CRC32 used to re-seal hand-mutated streams, so a
/// deliberately corrupt body still passes the envelope check and reaches
/// the parser. Duplicating the table here keeps the test honest — a codec
/// bug inside the crate cannot mask itself through shared code.
fn crc32_ieee(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                0xEDB8_8320 ^ (crc >> 1)
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// Attach a correct CRC32 trailer to a (possibly hand-corrupted) body.
fn seal(mut body: Vec<u8>) -> Vec<u8> {
    let crc = crc32_ieee(&body);
    body.extend_from_slice(&crc.to_be_bytes());
    body
}

fn uid(n: u8) -> [u8; 16] {
    let mut u = [0u8; 16];
    u[0] = n;
    u[15] = 0xA5;
    u
}

fn owner() -> [u8; 16] {
    let mut u = [0u8; 16];
    u[0] = 0x0B;
    u[7] = 0xE5;
    u
}

/// A valid, fully populated record. `tag` picks a unique uid, nickname and
/// stat spread so records are distinguishable in conservation checks.
fn make_record(tag: u8) -> CompanionRecord {
    let mut r = CompanionRecord::new(uid(tag), owner(), "test:ember_cub");
    r.nickname = Some(format!("cub-{tag:03}"));
    r.level = 20 + (tag % 40);
    r.experience = u32::from(tag) * 1_000;
    r.current_hp = 30;
    r.max_hp = 40;
    r.friendship = tag;
    r.held_item = Some("test:soothe_charm".into());
    r.status_condition = None;
    r.nature_or_trait = "brave".into();
    r.shiny_or_rare = tag.is_multiple_of(2);
    r.moves = vec![
        "test:scratch".into(),
        "test:ember_dash".into(),
        format!("test:move_{tag}"),
    ];
    r.custom_stats = [tag as u16, 2, 3, 4, 5, 6];
    r
}

/// Sorted multiset of every stored uid — the conservation invariant.
fn uid_multiset(storage: &PlayerCompanionStorage) -> Vec<[u8; 16]> {
    let mut uids: Vec<[u8; 16]> = storage
        .occupied_slots()
        .into_iter()
        .map(|s| storage.get(s).unwrap().uid)
        .collect();
    uids.sort();
    uids
}

fn plain_device(multiplier: f32) -> CaptureDeviceSpec {
    CaptureDeviceSpec {
        device_id: "test:capture_orb".into(),
        device_multiplier: multiplier,
    }
}

/// Rank an outcome so "better" results compare higher: capture beats any
/// escape, and among escapes more shakes beat fewer.
fn outcome_rank(o: CaptureOutcome) -> u8 {
    match o {
        CaptureOutcome::Captured => u8::MAX,
        CaptureOutcome::Escaped { shakes } => shakes,
    }
}

// ------------------------------------------------- party & box conservation

#[test]
fn capturing_eight_companions_fills_party_then_overflows_into_pc_box() {
    let mut storage = PlayerCompanionStorage::new();
    for tag in 0..8u8 {
        let slot = storage.add_captured(make_record(tag)).unwrap();
        let expected = if tag < 6 {
            StorageSlotRef::Party(usize::from(tag))
        } else {
            StorageSlotRef::PcBox {
                box_index: 0,
                slot_index: usize::from(tag - 6),
            }
        };
        assert_eq!(slot, expected, "companion {tag} landed in the wrong slot");
    }
    assert_eq!(storage.total_occupied(), 8);
    // Slots 0..=1 of box 0 are occupied; slot 2 is the next free position.
    assert!(storage
        .get(StorageSlotRef::PcBox {
            box_index: 0,
            slot_index: 2
        })
        .is_none());
}

#[test]
fn swap_between_party_and_box_conserves_every_uid() {
    let mut storage = PlayerCompanionStorage::new();
    for tag in 0..8u8 {
        storage.add_captured(make_record(tag)).unwrap();
    }
    let before = uid_multiset(&storage);
    assert_eq!(before.len(), 8);
    assert_eq!(
        BTreeSet::from_iter(before.iter()).len(),
        8,
        "uids must be unique before the swap"
    );

    storage
        .swap_slots(
            StorageSlotRef::Party(0),
            StorageSlotRef::PcBox {
                box_index: 0,
                slot_index: 1,
            },
        )
        .unwrap();

    let after = uid_multiset(&storage);
    assert_eq!(
        before, after,
        "a party↔box swap duplicated or lost a companion"
    );
    assert_eq!(storage.get(StorageSlotRef::Party(0)).unwrap().uid, uid(7));
    assert_eq!(
        storage
            .get(StorageSlotRef::PcBox {
                box_index: 0,
                slot_index: 1
            })
            .unwrap()
            .uid,
        uid(0)
    );
    // The swapped record, not a clone, moved: its nickname follows.
    assert_eq!(
        storage.get(StorageSlotRef::Party(0)).unwrap().nickname,
        Some("cub-007".to_string())
    );
}

#[test]
fn swap_with_empty_slot_is_an_atomic_move() {
    let mut storage = PlayerCompanionStorage::new();
    storage.add_captured(make_record(1)).unwrap();
    let empty = StorageSlotRef::PcBox {
        box_index: 3,
        slot_index: 29,
    };
    storage.swap_slots(StorageSlotRef::Party(0), empty).unwrap();
    assert!(storage.get(StorageSlotRef::Party(0)).is_none());
    assert_eq!(storage.get(empty).unwrap().uid, uid(1));
}

#[test]
fn completely_full_storage_rejects_captures() {
    let mut storage = PlayerCompanionStorage::with_box_count(1).unwrap();
    let capacity = storage.capacity();
    for tag in 0..capacity {
        storage
            .add_captured(make_record((tag % 200) as u8))
            .unwrap_or_else(|e| panic!("slot {tag} of {capacity} failed: {e}"));
    }
    assert_eq!(storage.total_occupied(), capacity);
    assert_eq!(
        storage.add_captured(make_record(250)),
        Err(StorageError::Full)
    );
    // Nothing was lost or duplicated by the rejected insert.
    assert_eq!(storage.total_occupied(), capacity);
}

#[test]
fn invalid_slot_and_invalid_record_are_refused_without_side_effects() {
    let mut storage = PlayerCompanionStorage::new();
    storage.add_captured(make_record(9)).unwrap();
    let before = uid_multiset(&storage);

    let bad_box = StorageSlotRef::PcBox {
        box_index: MAX_BOX_COUNT + 5,
        slot_index: 0,
    };
    assert_eq!(
        storage.swap_slots(StorageSlotRef::Party(0), bad_box),
        Err(StorageError::InvalidSlot(bad_box))
    );
    // A swap that fails validation must not move the valid side either.
    assert_eq!(storage.get(StorageSlotRef::Party(0)).unwrap().uid, uid(9));

    let mut invalid = make_record(10);
    invalid.level = 0;
    assert!(matches!(
        storage.add_captured(invalid),
        Err(StorageError::InvalidRecord(RecordError::LevelOutOfRange { level: 0 }))
    ));
    let mut non_namespaced = make_record(11);
    non_namespaced.species_id = "ember_cub".into();
    assert!(matches!(
        storage.add_captured(non_namespaced),
        Err(StorageError::InvalidRecord(RecordError::InvalidSpeciesId { .. }))
    ));
    let mut too_many = make_record(12);
    too_many.moves = (0..5).map(|i| format!("test:move_{i}")).collect();
    assert!(matches!(
        storage.add_captured(too_many),
        Err(StorageError::InvalidRecord(RecordError::TooManyMoves { count: 5 }))
    ));
    assert_eq!(uid_multiset(&storage), before);
}

#[test]
fn box_count_is_capped_at_construction() {
    assert_eq!(
        PlayerCompanionStorage::with_box_count(MAX_BOX_COUNT)
            .unwrap()
            .box_count(),
        MAX_BOX_COUNT
    );
    assert_eq!(
        PlayerCompanionStorage::with_box_count(MAX_BOX_COUNT + 1),
        Err(StorageError::BoxCountExceeded {
            requested: MAX_BOX_COUNT + 1,
            cap: MAX_BOX_COUNT,
        })
    );
    assert_eq!(PlayerCompanionStorage::new().box_count(), DEFAULT_BOX_COUNT);
}

#[test]
fn heal_all_party_restores_hp_and_clears_status_only_in_party() {
    let mut storage = PlayerCompanionStorage::new();
    let mut hurt = make_record(1);
    hurt.current_hp = 3;
    hurt.status_condition = Some("poisoned".into());
    storage.add_captured(hurt).unwrap();
    let mut tired = make_record(2);
    tired.current_hp = 1;
    tired.status_condition = Some("asleep".into());
    storage.add_captured(tired).unwrap();
    // Four healthy party members at full HP with no status: heal is a
    // no-op for them, so the returned count covers only the two hurt ones.
    for tag in 3..7u8 {
        let mut healthy = make_record(tag);
        healthy.current_hp = healthy.max_hp;
        storage.add_captured(healthy).unwrap();
    }
    // A boxed companion stays hurt: heal covers the active party only.
    let mut boxed_hurt = make_record(10);
    boxed_hurt.current_hp = 1;
    boxed_hurt.status_condition = Some("burned".into());
    storage.add_captured(boxed_hurt).unwrap();

    assert_eq!(storage.heal_all_party(), 2);
    for i in 0..PARTY_SLOTS {
        let r = storage.get(StorageSlotRef::Party(i)).unwrap();
        assert_eq!(r.current_hp, r.max_hp);
        assert_eq!(r.status_condition, None);
    }
    let boxed = storage
        .get(StorageSlotRef::PcBox {
            box_index: 0,
            slot_index: 0,
        })
        .unwrap();
    assert_eq!(boxed.current_hp, 1);
    assert_eq!(boxed.status_condition.as_deref(), Some("burned"));
}

#[test]
fn release_returns_the_record_and_leaves_an_empty_slot() {
    let mut storage = PlayerCompanionStorage::new();
    storage.add_captured(make_record(5)).unwrap();
    let released = storage.release(StorageSlotRef::Party(0)).unwrap();
    assert_eq!(released.uid, uid(5));
    assert!(storage.get(StorageSlotRef::Party(0)).is_none());
    assert_eq!(storage.release(StorageSlotRef::Party(0)), None);
    assert_eq!(storage.release(StorageSlotRef::Party(9)), None);
    // The freed slot is reused by the next capture.
    assert_eq!(
        storage.add_captured(make_record(6)).unwrap(),
        StorageSlotRef::Party(0)
    );
}

// ----------------------------------------------------------- capture roll

#[test]
fn capture_roll_is_deterministic_per_seed() {
    let device = plain_device(1.0);
    for seed in 0..64u64 {
        let first = evaluate_capture_attempt(&device, 50, 200, 200, 1.0, 1.0, seed);
        let second = evaluate_capture_attempt(&device, 50, 200, 200, 1.0, 1.0, seed);
        assert_eq!(first, second, "seed {seed} diverged between runs");
        if let CaptureOutcome::Escaped { shakes } = first {
            assert!(shakes < SHAKE_CHECKS, "escape shake count {shakes} out of range");
        }
    }
}

#[test]
fn guaranteed_device_captures_every_seed() {
    for mult in [GUARANTEED_CAPTURE_MULTIPLIER, 300.0, f32::INFINITY] {
        for seed in 0..32u64 {
            assert_eq!(
                evaluate_capture_attempt(&plain_device(mult), 199, 200, 1, 1.0, 1.0, seed),
                CaptureOutcome::Captured,
                "multiplier {mult} failed on seed {seed}"
            );
        }
    }
}

#[test]
fn lower_target_hp_never_worsens_the_outcome_per_seed() {
    let device = plain_device(1.0);
    let mut high_hp_captures = 0u32;
    let mut low_hp_captures = 0u32;
    for seed in 0..512u64 {
        let high = evaluate_capture_attempt(&device, 200, 200, 200, 1.0, 1.0, seed);
        let low = evaluate_capture_attempt(&device, 2, 200, 200, 1.0, 1.0, seed);
        assert!(
            outcome_rank(low) >= outcome_rank(high),
            "seed {seed}: lower HP produced a worse outcome {low:?} vs {high:?}"
        );
        high_hp_captures += u32::from(high.is_captured());
        low_hp_captures += u32::from(low.is_captured());
    }
    // Both regimes must actually be exercised: the high-HP roll is a
    // genuine gamble and the low-HP roll is a strong favourite.
    assert!(low_hp_captures > high_hp_captures);
    assert!(high_hp_captures > 0, "high-HP roll unexpectedly never failed");
    assert!(low_hp_captures < 512, "low-HP roll unexpectedly always passed");
}

#[test]
fn status_bonus_never_worsens_the_outcome_per_seed() {
    let device = plain_device(1.0);
    let mut baseline = 0u32;
    let mut boosted = 0u32;
    for seed in 0..512u64 {
        let without = evaluate_capture_attempt(&device, 100, 200, 200, 1.0, 1.0, seed);
        let with = evaluate_capture_attempt(&device, 100, 200, 200, 2.5, 1.0, seed);
        assert!(outcome_rank(with) >= outcome_rank(without));
        baseline += u32::from(without.is_captured());
        boosted += u32::from(with.is_captured());
    }
    assert!(boosted > baseline);
}

#[test]
fn zero_catch_value_escapes_immediately() {
    let free = plain_device(0.0);
    assert_eq!(
        evaluate_capture_attempt(&free, 1, 200, 255, 5.0, 5.0, 7),
        CaptureOutcome::Escaped { shakes: 0 }
    );
    assert_eq!(
        evaluate_capture_attempt(&plain_device(1.0), 1, 200, 0, 5.0, 5.0, 7),
        CaptureOutcome::Escaped { shakes: 0 }
    );
}

#[test]
fn escapes_report_shake_counts_within_zero_to_three() {
    let device = plain_device(1.0);
    let mut saw_escape = false;
    for seed in 0..256u64 {
        match evaluate_capture_attempt(&device, 200, 200, 120, 1.0, 1.0, seed) {
            CaptureOutcome::Escaped { shakes } => {
                saw_escape = true;
                assert!(shakes <= 3);
            }
            CaptureOutcome::Captured => {}
        }
    }
    assert!(saw_escape, "params should produce some escapes");
}

// ------------------------------------------------------------ growth & xp

#[test]
fn growth_curves_match_the_polynomial_tables() {
    assert_eq!(experience_for_level(GrowthCurve::Fast, 100), 800_000);
    assert_eq!(experience_for_level(GrowthCurve::MediumFast, 100), 1_000_000);
    assert_eq!(experience_for_level(GrowthCurve::MediumSlow, 100), 1_059_860);
    assert_eq!(experience_for_level(GrowthCurve::Slow, 100), 1_250_000);
    assert_eq!(experience_for_level(GrowthCurve::MediumFast, 50), 125_000);
    for curve in [
        GrowthCurve::Fast,
        GrowthCurve::MediumFast,
        GrowthCurve::MediumSlow,
        GrowthCurve::Slow,
    ] {
        assert_eq!(experience_for_level(curve, 1), 0);
        // The table is strictly increasing across the whole level range.
        for level in 2..=MAX_LEVEL {
            assert!(
                experience_for_level(curve, level) > experience_for_level(curve, level - 1),
                "{curve:?} is not increasing at level {level}"
            );
        }
    }
}

#[test]
fn level_for_experience_is_the_exact_table_inverse() {
    for curve in [
        GrowthCurve::Fast,
        GrowthCurve::MediumFast,
        GrowthCurve::MediumSlow,
        GrowthCurve::Slow,
    ] {
        for level in [2u8, 17, 50, 99, 100] {
            let xp = experience_for_level(curve, level);
            assert_eq!(level_for_experience(curve, xp), level, "{curve:?} @ {level}");
            assert_eq!(level_for_experience(curve, xp - 1), level - 1);
        }
    }
}

#[test]
fn award_experience_spans_multiple_levels_and_caps_at_100() {
    let mut record = make_record(1);
    record.level = 5;
    record.experience = 0;

    // One large award crosses several thresholds at once.
    let target = experience_for_level(GrowthCurve::MediumFast, 40);
    let report = award_experience(&mut record, GrowthCurve::MediumFast, target);
    assert_eq!(report.previous_level, 5);
    assert_eq!(report.new_level, 40);
    assert!(report.levels_gained >= 30);
    assert_eq!(record.level, 40);

    // Awards past the cap saturate: level 100, experience pinned.
    let report = award_experience(&mut record, GrowthCurve::MediumFast, u32::MAX);
    assert_eq!(report.new_level, MAX_LEVEL);
    assert_eq!(report.experience, experience_for_level(GrowthCurve::MediumFast, MAX_LEVEL));
    assert_eq!(record.experience, report.experience);

    // A post-cap award neither gains nor loses a level.
    let report = award_experience(&mut record, GrowthCurve::MediumFast, 1);
    assert_eq!(report.new_level, MAX_LEVEL);
    assert_eq!(report.levels_gained, 0);
}

#[test]
fn award_experience_never_lowers_an_existing_level() {
    // A mod may hand a companion a higher level than its XP implies; XP
    // awards climb from there rather than dragging it down.
    let mut record = make_record(2);
    record.level = 60;
    record.experience = 0;
    let report = award_experience(&mut record, GrowthCurve::Slow, 10);
    assert_eq!(report.previous_level, 60);
    assert_eq!(report.new_level, 60);
    assert_eq!(record.level, 60);
}

// ------------------------------------------------------------- evolution

fn evolution_rules() -> Vec<EvolutionRule> {
    vec![
        EvolutionRule {
            source_species_id: "test:ember_cub".into(),
            target_species_id: "test:ember_fox".into(),
            conditions: vec![EvolutionCondition::MinLevel(35)],
        },
        EvolutionRule {
            source_species_id: "test:ember_cub".into(),
            target_species_id: "test:cinder_fox".into(),
            conditions: vec![
                EvolutionCondition::MinLevel(25),
                EvolutionCondition::Biome("test:volcanic_fields".into()),
            ],
        },
        EvolutionRule {
            source_species_id: "test:ember_cub".into(),
            target_species_id: "test:hearth_fox".into(),
            conditions: vec![EvolutionCondition::MinFriendship(220)],
        },
        EvolutionRule {
            source_species_id: "test:ember_cub".into(),
            target_species_id: "test:storm_fox".into(),
            conditions: vec![EvolutionCondition::UseItem("test:thunder_stone".into())],
        },
        EvolutionRule {
            source_species_id: "test:ember_cub".into(),
            target_species_id: "test:moon_fox".into(),
            conditions: vec![
                EvolutionCondition::TimeOfDay(DayPhase::Night),
                EvolutionCondition::HeldItem("test:moon_charm".into()),
            ],
        },
        EvolutionRule {
            source_species_id: "test:frost_pup".into(),
            target_species_id: "test:frost_wolf".into(),
            conditions: vec![EvolutionCondition::MinLevel(1)],
        },
    ]
}

fn evo_ctx() -> EvolutionContext {
    EvolutionContext {
        used_item: None,
        time_of_day: DayPhase::Day,
        biome_id: Some("test:meadow".into()),
    }
}

fn targets(candidates: &[mod_offload::party_storage::EvolutionCandidate]) -> Vec<String> {
    candidates
        .iter()
        .map(|c| c.target_species_id.clone())
        .collect()
}

#[test]
fn min_level_gate_blocks_and_opens() {
    let mut record = make_record(1);
    record.level = 30;
    assert!(eligible_evolutions(&record, &evolution_rules(), &evo_ctx()).is_empty());
    record.level = 35;
    assert_eq!(
        targets(&eligible_evolutions(&record, &evolution_rules(), &evo_ctx())),
        vec!["test:ember_fox".to_string()]
    );
}

#[test]
fn biome_condition_requires_the_matching_biome() {
    let mut record = make_record(1);
    record.level = 26;
    let mut ctx = evo_ctx();
    assert!(eligible_evolutions(&record, &evolution_rules(), &ctx).is_empty());
    ctx.biome_id = Some("test:volcanic_fields".into());
    assert_eq!(
        targets(&eligible_evolutions(&record, &evolution_rules(), &ctx)),
        vec!["test:cinder_fox".to_string()]
    );
    ctx.biome_id = None;
    assert!(eligible_evolutions(&record, &evolution_rules(), &ctx).is_empty());
}

#[test]
fn friendship_use_item_held_item_and_time_of_day_conditions() {
    let rules = evolution_rules();
    let mut record = make_record(1);
    record.level = 20;
    record.friendship = 219;

    assert!(eligible_evolutions(&record, &rules, &evo_ctx()).is_empty());
    record.friendship = 220;
    assert_eq!(
        targets(&eligible_evolutions(&record, &rules, &evo_ctx())),
        vec!["test:hearth_fox".to_string()]
    );

    let mut used = evo_ctx();
    used.used_item = Some("test:thunder_stone".into());
    assert!(targets(&eligible_evolutions(&record, &rules, &used))
        .contains(&"test:storm_fox".to_string()));
    used.used_item = Some("test:fire_stone".into()); // wrong item
    assert!(!targets(&eligible_evolutions(&record, &rules, &used))
        .contains(&"test:storm_fox".to_string()));

    // TimeOfDay AND HeldItem must both hold for the night rule.
    record.held_item = Some("test:moon_charm".into());
    let mut night = evo_ctx();
    night.time_of_day = DayPhase::Night;
    assert!(targets(&eligible_evolutions(&record, &rules, &night))
        .contains(&"test:moon_fox".to_string()));
    night.time_of_day = DayPhase::Dusk;
    assert!(!targets(&eligible_evolutions(&record, &rules, &night))
        .contains(&"test:moon_fox".to_string()));
    night.time_of_day = DayPhase::Night;
    record.held_item = None;
    assert!(!targets(&eligible_evolutions(&record, &rules, &night))
        .contains(&"test:moon_fox".to_string()));
}

#[test]
fn rules_only_match_their_source_species_and_report_rule_index() {
    let mut record = make_record(1);
    record.level = 50;
    record.friendship = 255;
    let candidates = eligible_evolutions(&record, &evolution_rules(), &evo_ctx());
    // Level-35 and friendship-220 rules qualify; the frost_pup rule (a
    // different source species) must not leak in.
    assert_eq!(
        targets(&candidates),
        vec![
            "test:ember_fox".to_string(),
            "test:hearth_fox".to_string()
        ]
    );
    assert_eq!(candidates[0].rule_index, 0);
    assert_eq!(candidates[1].rule_index, 2);
    assert_eq!(candidates[0].source_uid, record.uid);
}

// --------------------------------------------------------- PRTY persistence

#[test]
fn prty_roundtrip_preserves_every_field_and_slot() {
    let mut storage = PlayerCompanionStorage::with_box_count(2).unwrap();
    // Full party, one box slot, varied optional fields.
    for tag in 0..7u8 {
        let mut r = make_record(tag);
        if tag == 0 {
            r.nickname = None;
            r.held_item = None;
            r.status_condition = Some("paralyzed".into());
            r.moves = Vec::new();
        }
        if tag == 3 {
            r.shiny_or_rare = true;
            r.moves = vec!["test:only_move".into()];
        }
        storage.add_captured(r).unwrap();
    }
    // A sparse occupation pattern: gaps inside the party are impossible
    // through add_captured, so make one via release + box placement.
    storage.release(StorageSlotRef::Party(2));
    storage
        .swap_slots(
            StorageSlotRef::Party(5),
            StorageSlotRef::PcBox {
                box_index: 1,
                slot_index: 29,
            },
        )
        .unwrap();

    let bytes = encode_storage(&storage);
    assert_eq!(&bytes[..4], b"PRTY");
    let restored = decode_storage(&bytes).unwrap();
    assert_eq!(restored.box_count(), 2);
    assert_eq!(restored.occupied_slots(), storage.occupied_slots());
    for slot in storage.occupied_slots() {
        assert_eq!(
            restored.get(slot),
            storage.get(slot),
            "record diverged at {slot}"
        );
    }
    // Deterministic re-encode: decoding then encoding must be a fixed point.
    assert_eq!(encode_storage(&restored), bytes);
}

#[test]
fn prty_roundtrip_on_empty_storage() {
    let storage = PlayerCompanionStorage::new();
    let restored = decode_storage(&encode_storage(&storage)).unwrap();
    assert_eq!(restored.box_count(), DEFAULT_BOX_COUNT);
    assert_eq!(restored.total_occupied(), 0);
    assert_eq!(restored.capacity(), PARTY_SLOTS + DEFAULT_BOX_COUNT * BOX_SLOTS);
}

#[test]
fn prty_decode_rejects_bad_magic_crc_flips_and_truncation() {
    let mut storage = PlayerCompanionStorage::new();
    storage.add_captured(make_record(1)).unwrap();
    let bytes = encode_storage(&storage);

    // Bad magic with a re-sealed CRC: the magic check must fire first.
    let mut bad = bytes.clone();
    bad[0] = b'X';
    assert_eq!(
        decode_storage(&seal(bad[..bad.len() - 4].to_vec())),
        Err(StorageDecodeError::BadMagic)
    );

    // Every single-bit flip — in the body or in the CRC trailer itself —
    // must be an error. CRC32 detects all single-bit corruptions.
    for i in 0..bytes.len() {
        let mut corrupted = bytes.clone();
        corrupted[i] ^= 0x01;
        assert!(
            decode_storage(&corrupted).is_err(),
            "bit flip at byte {i} decoded successfully"
        );
    }

    // Every truncation length must fail closed.
    for len in 0..bytes.len() {
        assert!(
            decode_storage(&bytes[..len]).is_err(),
            "truncation to {len} bytes decoded successfully"
        );
    }
}

#[test]
fn prty_decode_rejects_unknown_schema_and_oversized_counts() {
    let storage = PlayerCompanionStorage::new();
    let bytes = encode_storage(&storage);
    let body = &bytes[..bytes.len() - 4];

    // Schema version 99, re-sealed.
    let mut bad = body.to_vec();
    bad[4..8].copy_from_slice(&99u32.to_be_bytes());
    assert_eq!(
        decode_storage(&seal(bad)),
        Err(StorageDecodeError::UnsupportedSchemaVersion(99))
    );

    // box_count past the 64-box cap, re-sealed: refused before allocation.
    let mut bad = body.to_vec();
    bad[8..12].copy_from_slice(&(MAX_BOX_COUNT as u32 + 1).to_be_bytes());
    assert_eq!(
        decode_storage(&seal(bad)),
        Err(StorageDecodeError::OversizedCount {
            field: "box_count",
            count: MAX_BOX_COUNT as u32 + 1,
            cap: MAX_BOX_COUNT as u32,
        })
    );

    // box_count below the cap but past the actual data: the stream runs
    // dry mid-box and fails closed instead of inventing slots.
    let mut bad = body.to_vec();
    bad[8..12].copy_from_slice(&(DEFAULT_BOX_COUNT as u32 + 1).to_be_bytes());
    assert_eq!(
        decode_storage(&seal(bad)),
        Err(StorageDecodeError::Truncated)
    );
}

#[test]
fn prty_decode_rejects_trailing_bytes_and_semantic_corruption() {
    let mut storage = PlayerCompanionStorage::new();
    storage.add_captured(make_record(1)).unwrap();
    let bytes = encode_storage(&storage);
    let body = bytes[..bytes.len() - 4].to_vec();

    // Garbage appended inside the CRC envelope: counts all decode, then
    // the reader finds leftover bytes.
    let mut padded = body.clone();
    padded.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
    assert_eq!(
        decode_storage(&seal(padded)),
        Err(StorageDecodeError::TrailingBytes { trailing: 4 })
    );

    // A slot-presence flag of 2 is a hard error, not a silent None.
    // Layout: magic(4) + schema(4) + box_count(4) → first slot flag at 12.
    let mut bad = body.clone();
    bad[12] = 2;
    assert_eq!(
        decode_storage(&seal(bad)),
        Err(StorageDecodeError::InvalidFlag {
            field: "slot_presence",
            value: 2,
        })
    );

    // Patch the first record's level byte to 0. Layout inside the record:
    // uid(16) + owner_uuid(16) + species_len(4) + species bytes +
    // nickname_flag(1) + nickname_len(4) + nickname bytes + level.
    let species_len = "test:ember_cub".len();
    let nickname_len = "cub-001".len();
    let level_offset = 12 // header
        + 1              // slot presence
        + 16 + 16        // uid + owner_uuid
        + 4 + species_len
        + 1 + 4 + nickname_len;
    let mut bad = body.clone();
    assert_eq!(bad[level_offset], 21); // level 20 + tag 1
    bad[level_offset] = 0;
    assert!(matches!(
        decode_storage(&seal(bad)),
        Err(StorageDecodeError::InvalidRecord(
            RecordError::LevelOutOfRange { level: 0 }
        ))
    ));
}

#[test]
fn prty_decode_rejects_oversized_lengths_before_reading() {
    // Hand-roll a body whose first party slot claims a 4 MiB species id:
    // the length check fires before a single byte of the string is read,
    // so the stream does not even need to contain it.
    let mut body = Vec::new();
    body.extend_from_slice(b"PRTY");
    body.extend_from_slice(&PRTY_SCHEMA_VERSION.to_be_bytes());
    body.extend_from_slice(&0u32.to_be_bytes()); // zero boxes
    body.push(1); // party slot 0: present
    body.extend_from_slice(&[0u8; 16]); // uid
    body.extend_from_slice(&[0u8; 16]); // owner_uuid
    body.extend_from_slice(&4_000_000u32.to_be_bytes()); // species_id len
    assert_eq!(
        decode_storage(&seal(body)),
        Err(StorageDecodeError::OversizedLength {
            field: "species_id",
            len: 4_000_000,
            cap: 1_024,
        })
    );
}

#[test]
fn codec_error_types_are_terminal() {
    // StorageError and StorageDecodeError implement Error and Display so
    // they can surface through `?` in callers.
    fn assert_error<T: std::error::Error>() {}
    assert_error::<StorageError>();
    assert_error::<StorageDecodeError>();
    assert_error::<RecordError>();
    assert!(!format!("{}", StorageError::Full).is_empty());
    assert!(!format!(
        "{}",
        StorageDecodeError::CrcMismatch {
            expected: 0,
            actual: 1
        }
    )
    .is_empty());
}
