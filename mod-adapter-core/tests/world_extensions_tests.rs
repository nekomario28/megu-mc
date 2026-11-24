// SPDX-License-Identifier: LGPL-3.0-or-later

//! Integration tests for [`mod_adapter_core::world_extensions`]: the
//! declarative progression barrier chain, multi-part boss hitboxes, the
//! bounded `CGD1`/`EAD1` delegate wire contracts, AppleSkin + inventory
//! sorter QoL support, and the TerraBlender-style region blender.

use std::collections::{BTreeMap, BTreeSet};

use mod_adapter_core::world_extensions::*;

fn advancements(ids: &[&str]) -> BTreeSet<String> {
    ids.iter().map(|s| (*s).to_string()).collect()
}

// ===========================================================================
// Progression barriers
// ===========================================================================

#[test]
fn twilight_chain_locks_biome_until_required_advancement_is_held() {
    let engine = ProgressionBarrierEngine::new(twilight_progression_example_rules());
    let dim = "twilightforest:twilight_forest";

    // A fresh player inside the lich tower is locked: break & place are both
    // blocked, the missing advancement is named, and debuffs/damage ride on.
    let verdict = engine.check_progression(
        dim,
        "twilightforest:lich_tower",
        &advancements(&["twilightforest:progress_naga_entry"]),
        false,
    );
    let ProgressionVerdict::Locked {
        rule_id,
        missing_advancements,
        block_break_blocked,
        block_place_blocked,
        effects,
        periodic_damage,
    } = verdict
    else {
        panic!("fresh player in the lich tower must be locked");
    };
    assert_eq!(rule_id, "twilight_gate:lich_tower");
    assert_eq!(missing_advancements, ["twilightforest:progress_naga"]);
    assert!(block_break_blocked);
    assert!(block_place_blocked);
    assert_eq!(effects, [("minecraft:mining_fatigue".to_string(), 100, 4)]);
    assert_eq!(periodic_damage, Some((0.5, 40)));

    // The chain is sequential: holding the naga kill opens the tower while
    // the hydra gate further down stays sealed.
    assert_eq!(
        engine.check_progression(
            dim,
            "twilightforest:lich_tower",
            &advancements(&["twilightforest:progress_naga"]),
            false,
        ),
        ProgressionVerdict::Allowed
    );
    assert!(matches!(
        engine.check_progression(
            dim,
            "twilightforest:fire_swamp",
            &advancements(&["twilightforest:progress_naga"]),
            false,
        ),
        ProgressionVerdict::Locked { ref rule_id, .. } if rule_id == "twilight_gate:hydra_lair"
    ));

    // A biome no rule targets is always open, and an unrelated dimension
    // never gates even on matching biome names.
    assert_eq!(
        engine.check_progression(
            dim,
            "twilightforest:twilight_forest",
            &advancements(&[]),
            false,
        ),
        ProgressionVerdict::Allowed
    );
    assert_eq!(
        engine.check_progression(
            "minecraft:overworld",
            "twilightforest:lich_tower",
            &advancements(&[]),
            false,
        ),
        ProgressionVerdict::Allowed
    );
}

#[test]
fn creative_and_spectator_bypass_every_progression_gate() {
    let engine = ProgressionBarrierEngine::new(twilight_progression_example_rules());
    for biome in [
        "twilightforest:naga_courtyard",
        "twilightforest:lich_tower",
        "twilightforest:fire_swamp",
        "twilightforest:glacier",
    ] {
        assert_eq!(
            engine.check_progression(
                "twilightforest:twilight_forest",
                biome,
                &advancements(&[]),
                true,
            ),
            ProgressionVerdict::Allowed,
            "creative/spectator must bypass {biome}"
        );
    }
}

#[test]
fn generic_progression_rules_cover_non_twilight_packs() {
    // The engine is fully declarative — the same shape expresses a Cataclysm
    // arena or any RPG pack without touching Rust.
    let rules = vec![ProgressionRule {
        id: "rpgpack:crypt_gate".to_string(),
        dimension: "rpgpack:crypt".to_string(),
        target_biomes: ["rpgpack:bone_halls".to_string()].into_iter().collect(),
        required_advancements: vec![
            "rpgpack:tier1_key".to_string(),
            "rpgpack:tier2_key".to_string(),
        ],
        block_break_protected: true,
        block_place_protected: false,
        enforce_effects: vec![("minecraft:weakness".to_string(), 60, 0)],
        periodic_damage: None,
    }];
    let engine = ProgressionBarrierEngine::new(rules);
    let ProgressionVerdict::Locked {
        missing_advancements,
        block_place_blocked,
        ..
    } = engine.check_progression(
        "rpgpack:crypt",
        "rpgpack:bone_halls",
        &advancements(&["rpgpack:tier1_key"]),
        false,
    ) else {
        panic!("partially-keyed player must stay locked");
    };
    assert_eq!(missing_advancements, ["rpgpack:tier2_key"]);
    assert!(!block_place_blocked);
}

// ===========================================================================
// Multi-part boss encounters
// ===========================================================================

fn hydra_like_boss() -> BossEncounterState {
    BossEncounterState::new(
        42,
        360.0,
        vec![0.5],
        vec![
            SubPartHitbox {
                part_id: "head".to_string(),
                offset_xyz: [0.0, 3.0, 2.0],
                half_extents: [1.0, 1.0, 1.0],
                damage_multiplier: 2.0,
                invulnerable: false,
            },
            SubPartHitbox {
                part_id: "body".to_string(),
                offset_xyz: [0.0, 1.0, 0.0],
                half_extents: [1.5, 1.0, 1.5],
                damage_multiplier: 1.0,
                invulnerable: false,
            },
            SubPartHitbox {
                part_id: "armored_tail".to_string(),
                offset_xyz: [0.0, 1.0, -3.0],
                half_extents: [0.75, 0.75, 0.75],
                damage_multiplier: 0.0,
                invulnerable: true,
            },
        ],
        [0.0, 64.0, 0.0],
        [-32.0, 0.0, -32.0],
        [32.0, 128.0, 32.0],
    )
}

#[test]
fn sub_part_hits_scale_damage_and_armored_parts_absorb_it() {
    let mut boss = hydra_like_boss();

    // Head shot: 10 base damage through the 2.0x multiplier costs the boss 20.
    let outcome = boss.resolve_ray_hit(
        [0.0, 67.0, 40.0], // in front of the boss, at head height
        [0.0, 0.0, -1.0],
        64.0,
        10.0,
    );
    assert_eq!(
        outcome,
        PartHitOutcome::Landed {
            part_id: "head".to_string(),
            damage_applied: 20.0,
            health_remaining: 340.0,
            phase: 1,
            phase_changed: false,
        }
    );
    assert!((boss.bossbar_progress() - 340.0 / 360.0).abs() < 1e-6);

    // The armored tail is a valid hit target but deals zero damage.
    let tail_outcome = boss.resolve_ray_hit(
        [0.0, 65.0, -40.0],
        [0.0, 0.0, 1.0],
        64.0,
        50.0,
    );
    assert_eq!(
        tail_outcome,
        PartHitOutcome::Invulnerable {
            part_id: "armored_tail".to_string()
        }
    );
    assert_eq!(boss.health, 340.0);

    // A point hit straight into the body forwards base damage 1:1.
    let body_outcome = boss.resolve_point_hit([0.5, 65.0, 0.5], 10.0);
    assert!(matches!(
        body_outcome,
        PartHitOutcome::Landed {
            ref part_id,
            damage_applied,
            ..
        } if part_id == "body" && damage_applied == 10.0
    ));
    assert_eq!(boss.health, 330.0);
}

#[test]
fn phase_thresholds_bossbar_and_arena_protection_hold() {
    let mut boss = hydra_like_boss();
    assert_eq!(boss.current_phase(), 1);
    assert!((boss.bossbar_progress() - 1.0).abs() < f32::EPSILON);

    // Drive the boss to exactly 50% — the phase-2 threshold.
    boss.apply_part_hit("body", 180.0, [0.0, 65.0, 0.0]);
    assert_eq!(boss.health, 180.0);
    assert!((boss.bossbar_progress() - 0.5).abs() < 1e-6);
    assert_eq!(boss.current_phase(), 2);

    // The hit that crosses the threshold reports the transition once.
    let mut fresh = hydra_like_boss();
    let crossing = fresh.apply_part_hit("body", 181.0, [0.0, 65.0, 0.0]);
    assert!(matches!(
        crossing,
        PartHitOutcome::Landed {
            phase: 2,
            phase_changed: true,
            ..
        }
    ));

    // Damage may only land on hit points inside the arena bounds — an
    // out-of-bounds resolution is rejected before any HP moves.
    let outside = boss.apply_part_hit("head", 500.0, [0.0, 67.0, 100.0]);
    assert_eq!(outside, PartHitOutcome::OutsideArena);
    assert_eq!(boss.health, 180.0);
    let unknown = boss.apply_part_hit("wing", 10.0, [0.0, 65.0, 0.0]);
    assert_eq!(unknown, PartHitOutcome::UnknownPart);

    // Ray misses hit nothing; the bossbar floor is 0.0 on death, never -x.
    assert_eq!(
        boss.resolve_ray_hit([0.0, 200.0, 0.0], [0.0, 1.0, 0.0], 64.0, 10.0),
        PartHitOutcome::Miss
    );
    boss.apply_part_hit("body", 9_999.0, [0.0, 65.0, 0.0]);
    assert_eq!(boss.health, 0.0);
    assert!(!boss.is_alive());
    assert_eq!(boss.bossbar_progress(), 0.0);
    assert_eq!(boss.current_phase(), 2);
}

// ===========================================================================
// CGD1 — chunk-generator delegate wire contract
// ===========================================================================

#[test]
fn cgd1_request_and_response_round_trip() {
    let req = ChunkGenDelegateRequest {
        dim: "twilightforest:twilight_forest".to_string(),
        seed: -0x5eed_5eed,
        cx: -17,
        cz: 402,
        min_y: -64,
        height: 384,
    };
    let frame = encode_chunk_gen_request(&req).unwrap();
    assert_eq!(&frame[..4], b"CGD1");
    assert_eq!(decode_chunk_gen_request(&frame), Some(req.clone()));

    // 24 sections of 4096 palette states each — the tall-dimension shape.
    let resp = ChunkGenDelegateResponse {
        cx: req.cx,
        cz: req.cz,
        sections: (0..req.height / 16)
            .map(|s| vec![s; 4096].into_iter().map(|v| v * 7).collect())
            .collect(),
    };
    let resp_frame = encode_chunk_gen_response(&resp).unwrap();
    assert_eq!(&resp_frame[..4], b"CGD1");
    let decoded = decode_chunk_gen_response(&resp_frame).unwrap();
    assert_eq!(decoded, resp);
    assert!(validate_chunk_gen_response(&req, &decoded));

    // A response for a different chunk or wrong section count fails the
    // request pairing check even though it decodes cleanly.
    let wrong_chunk = ChunkGenDelegateResponse {
        cx: req.cx + 1,
        ..resp.clone()
    };
    assert!(!validate_chunk_gen_response(&req, &wrong_chunk));
    let short = ChunkGenDelegateResponse {
        sections: resp.sections[..8].to_vec(),
        ..resp.clone()
    };
    assert!(!validate_chunk_gen_response(&req, &short));
}

#[test]
fn cgd1_fails_closed_on_corruption_and_bounds_violations() {
    let req = ChunkGenDelegateRequest {
        dim: "minecraft:overworld".to_string(),
        seed: 1,
        cx: 0,
        cz: 0,
        min_y: -64,
        height: 384,
    };
    let frame = encode_chunk_gen_request(&req).unwrap();

    // Magic, CRC, truncation, and trailing garbage all fail closed.
    let mut bad_magic = frame.clone();
    bad_magic[0] = b'X';
    assert!(decode_chunk_gen_request(&bad_magic).is_none());
    let mut bad_crc = frame.clone();
    let n = bad_crc.len();
    bad_crc[n - 1] ^= 0xFF;
    assert!(decode_chunk_gen_request(&bad_crc).is_none());
    assert!(decode_chunk_gen_request(&frame[..frame.len() - 1]).is_none());
    let mut trailing = frame.clone();
    trailing.push(0);
    assert!(decode_chunk_gen_request(&trailing).is_none());

    // Out-of-contract heights are rejected at both encode and decode time.
    for height in [0, 15, 17, (256 + 1) * 16] {
        let bad = ChunkGenDelegateRequest {
            height,
            ..req.clone()
        };
        assert!(encode_chunk_gen_request(&bad).is_err());
        let mut body = Vec::new();
        body.extend_from_slice(&(3u16).to_be_bytes());
        body.extend_from_slice(b"a:b");
        body.extend_from_slice(&1i64.to_be_bytes());
        body.extend_from_slice(&[0u8; 8]); // cx + cz
        body.extend_from_slice(&0i32.to_be_bytes()); // min_y
        body.extend_from_slice(&height.to_be_bytes());
        let mut f = b"CGD1".to_vec();
        f.extend_from_slice(&crc32_of(&body).to_be_bytes());
        f.extend_from_slice(&body);
        assert!(
            decode_chunk_gen_request(&f).is_none(),
            "height {height} must fail bounds"
        );
    }

    // Response-side bounds: a claimed section count above the cap, or a
    // section claiming more than 4096 states, fails BEFORE allocation.
    let resp = ChunkGenDelegateResponse {
        cx: 0,
        cz: 0,
        sections: vec![vec![1u32; 4096]],
    };
    let frame = encode_chunk_gen_response(&resp).unwrap();
    // Rewrite the section-count field to 1000 > MAX_DELEGATE_SECTIONS.
    let mut oversized = frame.clone();
    oversized[8 + 8..8 + 12].copy_from_slice(&1000u32.to_be_bytes());
    let crc = crc32_of(&oversized[8..]);
    oversized[4..8].copy_from_slice(&crc.to_be_bytes());
    assert!(decode_chunk_gen_response(&oversized).is_none());
    // Rewrite the per-section state count to 4097 > 4096 (CRC still valid).
    let mut fat_section = frame.clone();
    fat_section[8 + 12..8 + 16].copy_from_slice(&4097u32.to_be_bytes());
    let crc = crc32_of(&fat_section[8..]);
    fat_section[4..8].copy_from_slice(&crc.to_be_bytes());
    assert!(decode_chunk_gen_response(&fat_section).is_none());
    // Encoder refuses malformed responses outright.
    assert!(encode_chunk_gen_response(&ChunkGenDelegateResponse {
        cx: 0,
        cz: 0,
        sections: vec![vec![0; 4096]; 257],
    })
    .is_err());
    assert!(encode_chunk_gen_response(&ChunkGenDelegateResponse {
        cx: 0,
        cz: 0,
        sections: vec![vec![0; 100]],
    })
    .is_err());
}

/// Test-local CRC-32 identical to the module's private one — keeps the
/// corruption fixtures honest without exposing the helper.
use megu_core::hash::crc32_ieee as crc32_of;

// ===========================================================================
// EAD1 — entity-AI delegate wire contract
// ===========================================================================

#[test]
fn ead1_snapshot_and_mutation_batch_round_trip() {
    let snapshot = EntityAiDelegateSnapshot {
        entity_id: 9_001,
        pos_xyz: [128.5, 64.0, -33.25],
        yaw_deg: 270.5,
        pitch_deg: -12.25,
        health: 88.0,
        max_health: 100.0,
        target_entity_id: Some(7),
    };
    let frame = encode_ai_snapshot(&snapshot);
    assert_eq!(&frame[..4], b"EAD1");
    assert_eq!(decode_ai_snapshot(&frame), Some(snapshot));

    let batch = EntityAiMutationBatch {
        entity_id: 9_001,
        mutations: vec![
            EntityAiMutation::MoveTo {
                xyz: [130.0, 64.0, -30.0],
                speed: 0.6,
            },
            EntityAiMutation::SetVelocity {
                xyz: [0.1, 0.42, -0.2],
            },
            EntityAiMutation::AttackEntity {
                target_entity_id: 7,
                damage: 6.5,
            },
            EntityAiMutation::SetBlock {
                pos: [130, 63, -30],
                state_id: 12_345,
            },
            EntityAiMutation::SpawnParticle {
                particle_id: 77,
                xyz: [130.0, 65.0, -30.0],
                count: 40,
            },
            EntityAiMutation::PlaySound {
                sound_id: "twilightforest:entity.hydra.roar".to_string(),
                volume: 2.0,
                pitch: 0.8,
            },
        ],
    };
    let batch_frame = encode_ai_mutation_batch(&batch).unwrap();
    assert_eq!(decode_ai_mutation_batch(&batch_frame), Some(batch));
    // A snapshot frame never decodes as a batch and vice versa.
    assert!(decode_ai_mutation_batch(&frame).is_none());
    assert!(decode_ai_snapshot(&batch_frame).is_none());
}

#[test]
fn ead1_enforces_the_per_tick_budget_and_fails_closed() {
    // Exactly at budget: allowed. One over: refused at encode AND decode.
    let at_cap = EntityAiMutationBatch {
        entity_id: 1,
        mutations: vec![
            EntityAiMutation::SetVelocity { xyz: [0.0; 3] };
            MAX_AI_MUTATIONS_PER_TICK as usize
        ],
    };
    assert!(encode_ai_mutation_batch(&at_cap).is_ok());
    let over_cap = EntityAiMutationBatch {
        entity_id: 1,
        mutations: vec![
            EntityAiMutation::SetVelocity { xyz: [0.0; 3] };
            MAX_AI_MUTATIONS_PER_TICK as usize + 1
        ],
    };
    assert!(encode_ai_mutation_batch(&over_cap).is_err());

    // Forged frame claiming 300 mutations decodes to None before allocating.
    let mut body = vec![0x02u8];
    body.extend_from_slice(&1u64.to_be_bytes());
    body.extend_from_slice(&300u32.to_be_bytes());
    body.extend_from_slice(&[0u8; 64]); // plenty of trailing bytes, still N/A
    let mut forged = b"EAD1".to_vec();
    forged.extend_from_slice(&crc32_of(&body).to_be_bytes());
    forged.extend_from_slice(&body);
    assert!(decode_ai_mutation_batch(&forged).is_none());

    // Unknown mutation tag, oversized particle count, corrupt CRC.
    let mut bad_tag_body = vec![0x02u8];
    bad_tag_body.extend_from_slice(&1u64.to_be_bytes());
    bad_tag_body.extend_from_slice(&1u32.to_be_bytes());
    bad_tag_body.push(99);
    bad_tag_body.extend_from_slice(&[0u8; 32]);
    let mut bad_tag = b"EAD1".to_vec();
    bad_tag.extend_from_slice(&crc32_of(&bad_tag_body).to_be_bytes());
    bad_tag.extend_from_slice(&bad_tag_body);
    assert!(decode_ai_mutation_batch(&bad_tag).is_none());

    let mut fat_particles_body = vec![0x02u8];
    fat_particles_body.extend_from_slice(&1u64.to_be_bytes());
    fat_particles_body.extend_from_slice(&1u32.to_be_bytes());
    fat_particles_body.push(5);
    fat_particles_body.extend_from_slice(&1u32.to_be_bytes());
    fat_particles_body.extend_from_slice(&[0u8; 24]);
    fat_particles_body.extend_from_slice(&(MAX_PARTICLE_SPAWN_COUNT + 1).to_be_bytes());
    let mut fat_particles = b"EAD1".to_vec();
    fat_particles.extend_from_slice(&crc32_of(&fat_particles_body).to_be_bytes());
    fat_particles.extend_from_slice(&fat_particles_body);
    assert!(decode_ai_mutation_batch(&fat_particles).is_none());

    let good = encode_ai_mutation_batch(&EntityAiMutationBatch {
        entity_id: 5,
        mutations: vec![EntityAiMutation::AttackEntity {
            target_entity_id: 9,
            damage: 2.0,
        }],
    })
    .unwrap();
    let mut corrupt = good.clone();
    *corrupt.last_mut().unwrap() ^= 0x55;
    assert!(decode_ai_mutation_batch(&corrupt).is_none());
}

// ===========================================================================
// AppleSkin + inventory sorter QoL
// ===========================================================================

#[test]
fn appleskin_wire_is_exactly_four_big_endian_bytes_with_delta_suppression() {
    // The canonical payload layout: one big-endian IEEE-754 f32.
    assert_eq!(encode_appleskin_f32(4.25), [0x40, 0x88, 0x00, 0x00]);
    assert_eq!(encode_appleskin_f32(0.0), [0x00; 4]);
    assert_eq!(decode_appleskin_f32(&encode_appleskin_f32(18.375)), Some(18.375));
    assert_eq!(decode_appleskin_f32(&[0x40, 0x88, 0x00]), None);
    assert_eq!(decode_appleskin_f32(&[0x40, 0x88, 0x00, 0x00, 0x00]), None);
    assert_eq!(CHANNEL_APPLESKIN_EXHAUSTION, "appleskin:exhaustion_sync");
    assert_eq!(CHANNEL_APPLESKIN_SATURATION, "appleskin:saturation_sync");

    let mut tracker = AppleSkinDeltaTracker::new();
    // First observation always sends.
    assert_eq!(tracker.poll_exhaustion(3.5), Some(encode_appleskin_f32(3.5)));
    // Sub-epsilon drift is suppressed; a clear >= epsilon step sends once,
    // then settles. (Margins stay wide of f32 rounding: 3.51-3.5 in f32 is
    // *below* 0.01, so boundary-adjacent literals are deliberately avoided.)
    assert_eq!(tracker.poll_exhaustion(3.505), None);
    assert_eq!(tracker.poll_exhaustion(3.52), Some(encode_appleskin_f32(3.52)));
    assert_eq!(tracker.poll_exhaustion(3.52), None);
    // Exact boundary: a delta bit-identical to epsilon sends (>=, not >).
    let mut boundary = AppleSkinDeltaTracker::new();
    assert_eq!(boundary.poll_exhaustion(0.0), Some(encode_appleskin_f32(0.0)));
    assert_eq!(
        boundary.poll_exhaustion(APPLESKIN_DELTA_EPSILON),
        Some(encode_appleskin_f32(APPLESKIN_DELTA_EPSILON))
    );
    // The two channels track independently.
    assert_eq!(tracker.poll_saturation(0.0), Some(encode_appleskin_f32(0.0)));
    assert_eq!(tracker.poll_saturation(0.005), None);
    assert_eq!(tracker.poll_saturation(-0.02), Some(encode_appleskin_f32(-0.02)));
}

fn stack(item: &str, hash: u64, count: u32, max: u32) -> Option<SortableStack> {
    Some(SortableStack {
        item_id: item.to_string(),
        component_hash: hash,
        count,
        max_stack_size: max,
    })
}

#[test]
fn inventory_sorter_merges_sorts_and_conserves_every_item() {
    // A 54-slot double chest mixing max-stack sizes 1/16/64 and component
    // variants, interleaved so merging must cross the whole inventory.
    let mut slots: Vec<Option<SortableStack>> = vec![
        stack("minecraft:cobblestone", 0, 30, 64),
        stack("rpgpack:excalibur", 0xAB, 1, 1),
        stack("minecraft:ender_pearl", 0, 12, 16),
        stack("minecraft:cobblestone", 0, 40, 64),
        None,
        stack("minecraft:stick", 0x11, 20, 64), // enchanted variant hash
        stack("minecraft:stick", 0, 50, 64),
        stack("minecraft:ender_pearl", 0, 9, 16),
        stack("minecraft:cobblestone", 0, 5, 64),
        stack("rpgpack:excalibur", 0xAB, 1, 1), // max 1: can never merge
        stack("minecraft:stick", 0, 30, 64),
        stack("minecraft:stick", 0x11, 44, 64),
        stack("minecraft:ender_pearl", 0, 3, 16),
    ];
    slots.resize(54, None);

    // Conservation baseline: per-(item, component) totals before sorting.
    let mut totals_before: BTreeMap<(String, u64), u64> = BTreeMap::new();
    for s in slots.iter().flatten() {
        *totals_before
            .entry((s.item_id.clone(), s.component_hash))
            .or_default() += u64::from(s.count);
    }
    let occupied_before = slots.iter().flatten().count();

    sort_and_merge_inventory(&mut slots);

    // Exact conservation — zero duping, zero loss.
    let mut totals_after: BTreeMap<(String, u64), u64> = BTreeMap::new();
    for s in slots.iter().flatten() {
        *totals_after
            .entry((s.item_id.clone(), s.component_hash))
            .or_default() += u64::from(s.count);
    }
    assert_eq!(totals_before, totals_after);

    // Every stack respects its item's max, and merging strictly compacts:
    // cobblestone 75 -> [64,11]; pearls 24 -> [16,8]; sticks(0) 80 -> [64,16];
    // sticks(0x11) 64 -> [64]; two swords stay two slots.
    let taken: Vec<&SortableStack> = slots.iter().flatten().collect();
    assert_eq!(taken.len(), 2 + 2 + 2 + 1 + 2);
    assert!(taken.len() < occupied_before);
    assert!(taken.iter().all(|s| s.count <= s.max_stack_size.max(1)));

    // Deterministic order: (item_id, component_hash, Reverse(count)),
    // empty slots trailing.
    let order: Vec<(&str, u64, u32)> = taken
        .iter()
        .map(|s| (s.item_id.as_str(), s.component_hash, s.count))
        .collect();
    assert_eq!(
        order,
        [
            ("minecraft:cobblestone", 0, 64),
            ("minecraft:cobblestone", 0, 11),
            ("minecraft:ender_pearl", 0, 16),
            ("minecraft:ender_pearl", 0, 8),
            ("minecraft:stick", 0, 64),
            ("minecraft:stick", 0, 16),
            ("minecraft:stick", 0x11, 64),
            ("rpgpack:excalibur", 0xAB, 1),
            ("rpgpack:excalibur", 0xAB, 1),
        ]
    );
    assert!(slots[order.len()..].iter().all(|s| s.is_none()));

    // Idempotent: sorting a sorted inventory is a fixed point.
    let snapshot = slots.clone();
    sort_and_merge_inventory(&mut slots);
    assert_eq!(slots, snapshot);
}

// ===========================================================================
// TerraBlender-style region blending
// ===========================================================================

fn three_mod_blender() -> BiomeRegionBlender {
    let mut blender = BiomeRegionBlender::new(4).with_region_size_chunks(32);
    blender.add_region(BiomeRegion {
        mod_id: "biomesoplenty".to_string(),
        weight: 3,
        dimension: "minecraft:overworld".to_string(),
        biome_overrides: [(
            "minecraft:plains".to_string(),
            "biomesoplenty:meadow".to_string(),
        )]
        .into_iter()
        .collect(),
    });
    blender.add_region(BiomeRegion {
        mod_id: "regions_unexplored".to_string(),
        weight: 3,
        dimension: "minecraft:overworld".to_string(),
        biome_overrides: [(
            "minecraft:plains".to_string(),
            "regions_unexplored:prairie".to_string(),
        )]
        .into_iter()
        .collect(),
    });
    blender.add_region(BiomeRegion {
        mod_id: "bwg".to_string(),
        weight: 2,
        dimension: "minecraft:overworld".to_string(),
        biome_overrides: BTreeMap::new(),
    });
    // A nether-only region must never win overworld cells.
    blender.add_region(BiomeRegion {
        mod_id: "netherx".to_string(),
        weight: 100,
        dimension: "minecraft:the_nether".to_string(),
        biome_overrides: BTreeMap::new(),
    });
    blender
}

#[test]
fn region_blender_distribution_matches_weights_and_stays_deterministic() {
    let blender = three_mod_blender();
    let seed = 0x5EED_u64 as i64;
    let dim = "minecraft:overworld";

    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    const SAMPLES: usize = 10_000;
    for i in 0..SAMPLES {
        // Deterministic pseudo-scatter over ~960k quart coords — every sample
        // lands in an essentially independent Voronoi cell.
        let qx = (i as i64 * 79_193 % 1_200_000 - 600_000) as i32;
        let qz = (i as i64 * 104_729 % 1_200_000 - 600_000) as i32;
        *counts
            .entry(
                blender
                    .winning_region_id(seed, dim, qx, qz)
                    .to_string(),
            )
            .or_default() += 1;
    }

    // Weights 4:3:3:2 of 12 -> 33.3/25/25/16.7%. Jittered Voronoi preserves
    // the per-cell weighted pick, so proportions hold within a few percent.
    let expect = |id: &str, w: f64| {
        let got = *counts.get(id).unwrap_or(&0) as f64 / SAMPLES as f64;
        assert!(
            (got - w / 12.0).abs() < 0.05,
            "{id}: got {:.3}, expected {:.3} (counts {counts:?})",
            got,
            w / 12.0
        );
        got
    };
    expect(VANILLA_REGION_MOD_ID, 4.0);
    expect("biomesoplenty", 3.0);
    expect("regions_unexplored", 3.0);
    expect("bwg", 2.0);
    assert_eq!(counts.get("netherx"), None, "nether region leaked into the overworld");
    assert_eq!(counts.values().sum::<usize>(), SAMPLES);

    // 100% seed-deterministic: two independent blenders with identical
    // config agree on every sample, and a different seed changes the map.
    let twin = three_mod_blender();
    let mut seed_differences = 0usize;
    for i in 0..2_000usize {
        let qx = (i as i64 * 6_371 % 400_000 - 200_000) as i32;
        let qz = (i as i64 * 9_173 % 400_000 - 200_000) as i32;
        let a = blender.winning_region_id(seed, dim, qx, qz);
        let b = twin.winning_region_id(seed, dim, qx, qz);
        assert_eq!(a, b);
        if blender.winning_region_id(seed + 1, dim, qx, qz) != a {
            seed_differences += 1;
        }
    }
    assert!(
        seed_differences > 100,
        "a reseed must remap the region layout ({seed_differences} diffs)"
    );
}

#[test]
fn region_blender_resolves_biome_overrides_and_cell_boundaries() {
    let blender = three_mod_blender();
    let seed = 42i64;
    let dim = "minecraft:overworld";

    // Walk until we have a sample inside each region, then verify override
    // resolution and same-cell coherence.
    let mut seen: BTreeMap<String, (i32, i32)> = BTreeMap::new();
    for i in 0..50_000usize {
        let qx = (i as i64 * 31_577 % 800_000 - 400_000) as i32;
        let qz = (i as i64 * 51_733 % 800_000 - 400_000) as i32;
        let id = blender.winning_region_id(seed, dim, qx, qz).to_string();
        seen.entry(id).or_insert((qx, qz));
        if seen.len() == 4 {
            break;
        }
    }
    assert_eq!(seen.len(), 4, "expected all four regions to appear");

    // BOP overrides plains -> meadow; an unmapped vanilla biome passes
    // through; vanilla cells never substitute.
    let bop = seen["biomesoplenty"];
    assert_eq!(
        blender.resolve_biome(seed, dim, bop.0, bop.1, "minecraft:plains"),
        "biomesoplenty:meadow"
    );
    assert_eq!(
        blender.resolve_biome(seed, dim, bop.0, bop.1, "minecraft:desert"),
        "minecraft:desert"
    );
    let ru = seen["regions_unexplored"];
    assert_eq!(
        blender.resolve_biome(seed, dim, ru.0, ru.1, "minecraft:plains"),
        "regions_unexplored:prairie"
    );
    let vanilla = seen[VANILLA_REGION_MOD_ID];
    assert_eq!(
        blender.resolve_biome(seed, dim, vanilla.0, vanilla.1, "minecraft:plains"),
        "minecraft:plains"
    );
    // BWG registered no overrides: winning still passes the vanilla biome through.
    let bwg = seen["bwg"];
    assert_eq!(
        blender.resolve_biome(seed, dim, bwg.0, bwg.1, "minecraft:plains"),
        "minecraft:plains"
    );

    // Two distant points in the same Voronoi cell agree on the winner —
    // regions are coherent areas, not per-coordinate noise.
    let cell_edge_quarts = blender.region_size_chunks() * 4; // chunks -> quarts
    let (qx, qz) = bop;
    let neighbor = (qx + cell_edge_quarts / 4, qz);
    assert_eq!(
        blender.winning_region_id(seed, dim, qx, qz),
        blender.winning_region_id(seed, dim, neighbor.0, neighbor.1),
        "same-cell points must share a region"
    );

    // An empty blender is 100% vanilla, regardless of weights elsewhere.
    let empty = BiomeRegionBlender::new(1);
    assert_eq!(
        empty.winning_region_id(seed, dim, qx, qz),
        VANILLA_REGION_MOD_ID
    );
    let zero_weight = BiomeRegionBlender::new(0);
    assert_eq!(
        zero_weight.winning_region_id(seed, dim, qx, qz),
        VANILLA_REGION_MOD_ID
    );
}
