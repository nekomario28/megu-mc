// SPDX-License-Identifier: LGPL-3.0-or-later

use megu_client_mesh::ber_instancing::{
    build_ber_mesh, build_extended_ber_mesh, BerBellMount, BerChestKind, BerDyeColor, BerSkullKind,
    VanillaBerExtendedKind, VanillaBerExtendedState, VanillaBerFacing, VanillaParticleKind,
    VanillaParticlePayload, VanillaParticlePayloadKind, VanillaParticleWorld, MAX_VANILLA_PARTICLES,
    PROTOCOL767_PARTICLE_COUNT, PROTOCOL767_PARTICLE_KINDS,
};
use megu_client_mesh::tactile_effects::{
    FluidEntryBody, GroundSurfaceKind, ImpactMaterial, TactileEffectsConfig, TactileEffectsSim,
    TactileParticleKind, MAX_BLOCK_CRACK_FEEDBACK,
};
use megu_client_mesh::vfx_engine::VfxHardwareTier;

const EXPECTED_PROTOCOL767_NAMES: [&str; PROTOCOL767_PARTICLE_COUNT] = [
    "angry_villager", "block", "block_marker", "bubble", "cloud", "crit", "damage_indicator",
    "dragon_breath", "dripping_lava", "falling_lava", "landing_lava", "dripping_water",
    "falling_water", "dust", "dust_color_transition", "effect", "elder_guardian", "enchanted_hit",
    "enchant", "end_rod", "entity_effect", "explosion_emitter", "explosion", "gust", "small_gust",
    "gust_emitter_large", "gust_emitter_small", "sonic_boom", "falling_dust", "firework", "fishing",
    "flame", "infested", "cherry_leaves", "sculk_soul", "sculk_charge", "sculk_charge_pop",
    "soul_fire_flame", "soul", "flash", "happy_villager", "composter", "heart", "instant_effect",
    "item", "vibration", "item_slime", "item_cobweb", "item_snowball", "large_smoke", "lava",
    "mycelium", "note", "poof", "portal", "rain", "smoke", "white_smoke", "sneeze", "spit",
    "squid_ink", "sweep_attack", "totem_of_undying", "underwater", "splash", "witch", "bubble_pop",
    "current_down", "bubble_column_up", "nautilus", "dolphin", "campfire_cosy_smoke",
    "campfire_signal_smoke", "dripping_honey", "falling_honey", "landing_honey", "falling_nectar",
    "falling_spore_blossom", "ash", "crimson_spore", "warped_spore", "spore_blossom_air",
    "dripping_obsidian_tear", "falling_obsidian_tear", "landing_obsidian_tear", "reverse_portal",
    "white_ash", "small_flame", "snowflake", "dripping_dripstone_lava", "falling_dripstone_lava",
    "dripping_dripstone_water", "falling_dripstone_water", "glow_squid_ink", "glow", "wax_on",
    "wax_off", "electric_spark", "scrape", "shriek", "egg_crack", "dust_plume",
    "trial_spawner_detected_player", "trial_spawner_detected_player_ominous", "vault_connection",
    "dust_pillar", "ominous_spawning", "raid_omen", "trial_omen",
];

fn assert_finite_positions(positions: &[[f32; 3]; 4]) {
    assert!(positions.iter().flatten().all(|v| v.is_finite()));
}

fn quad_area(positions: &[[f32; 3]; 4]) -> f32 {
    let a = [
        positions[1][0] - positions[0][0],
        positions[1][1] - positions[0][1],
        positions[1][2] - positions[0][2],
    ];
    let b = [
        positions[3][0] - positions[0][0],
        positions[3][1] - positions[0][1],
        positions[3][2] - positions[0][2],
    ];
    let cross = [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ];
    (cross[0] * cross[0] + cross[1] * cross[1] + cross[2] * cross[2]).sqrt()
}

fn representative_states() -> Vec<VanillaBerExtendedState> {
    vec![
        VanillaBerExtendedState::Chest { lid_open: 0.8, double: true, facing: VanillaBerFacing::East, chest: BerChestKind::Trapped },
        VanillaBerExtendedState::Chest { lid_open: 0.4, double: false, facing: VanillaBerFacing::North, chest: BerChestKind::Ender },
        VanillaBerExtendedState::ShulkerBox { open_progress: 0.75, facing: VanillaBerFacing::Up, dye: BerDyeColor::Lime },
        VanillaBerExtendedState::Bed { is_head: true, facing: VanillaBerFacing::South, dye: BerDyeColor::Cyan },
        VanillaBerExtendedState::Bed { is_head: false, facing: VanillaBerFacing::West, dye: BerDyeColor::Black },
        VanillaBerExtendedState::Sign { wall: false, hanging: false, glowing: true, facing_yaw: 0.6 },
        VanillaBerExtendedState::Sign { wall: true, hanging: true, glowing: false, facing_yaw: -1.2 },
        VanillaBerExtendedState::Banner { wall: false, sway_rad: 0.12, facing_yaw: 2.1, dye: BerDyeColor::Blue },
        VanillaBerExtendedState::Banner { wall: true, sway_rad: -0.12, facing_yaw: 0.0, dye: BerDyeColor::Yellow },
        VanillaBerExtendedState::Bell { mount: BerBellMount::Floor, facing: VanillaBerFacing::North, swing_rad: 0.45 },
        VanillaBerExtendedState::Bell { mount: BerBellMount::Wall, facing: VanillaBerFacing::East, swing_rad: -0.45 },
        VanillaBerExtendedState::EnchantingTable { facing_yaw: 0.2, book_yaw_rad: 1.4, page_open: 0.9 },
        VanillaBerExtendedState::Beacon { beam_height: 24.0, active: true, rgb: [0.2, 0.6, 1.0], beam_yaw: 0.8 },
        VanillaBerExtendedState::Conduit { cage_yaw_rad: 1.1, eye_open: 1.0 },
        VanillaBerExtendedState::Campfire { lit: true, item_count: 4, soul: false },
        VanillaBerExtendedState::Campfire { lit: true, item_count: 2, soul: true },
        VanillaBerExtendedState::Lectern { has_book: true, facing: VanillaBerFacing::East },
        VanillaBerExtendedState::DecoratedPot { facing: VanillaBerFacing::South, wobble: 0.08, sherds: [1, 2, 3, 4] },
        VanillaBerExtendedState::BrushableSand { dusted: 3, item_id: 7, facing: VanillaBerFacing::Up },
        VanillaBerExtendedState::BrushableGravel { dusted: 2, item_id: 3, facing: VanillaBerFacing::North },
        VanillaBerExtendedState::TrialSpawner { spin_rad: 1.2, active: true, ominous: true, mob_scale: 0.8 },
        VanillaBerExtendedState::Vault { spin_rad: 2.3, active: true, ominous: false },
        VanillaBerExtendedState::MovingPiston { facing: VanillaBerFacing::East, sticky: true, extension_progress: 0.7 },
        VanillaBerExtendedState::Skull { skull: BerSkullKind::EnderDragon, wall: false, yaw_deg: 75.0, jaw_open: 0.8 },
        VanillaBerExtendedState::Skull { skull: BerSkullKind::Piglin, wall: true, yaw_deg: -30.0, jaw_open: 0.0 },
        VanillaBerExtendedState::ItemFrame { facing: VanillaBerFacing::North, glow: true, rotation: 5 },
        VanillaBerExtendedState::BrewingStand { bottle_mask: 0b101 },
    ]
}

#[test]
fn protocol767_registry_is_contiguous_complete_and_named() {
    assert_eq!(PROTOCOL767_PARTICLE_KINDS.len(), 109);
    for (id, (kind, expected_name)) in PROTOCOL767_PARTICLE_KINDS
        .iter()
        .copied()
        .zip(EXPECTED_PROTOCOL767_NAMES)
        .enumerate()
    {
        assert_eq!(kind.protocol_id() as usize, id, "{expected_name}");
        assert_eq!(kind.registry_name(), expected_name);
        assert_eq!(VanillaParticleKind::from_protocol_id(id as u8), Some(kind));
        assert_eq!(VanillaParticleKind::from_registry_name(expected_name), Some(kind));
        assert_eq!(VanillaParticleKind::from_registry_name(&format!("minecraft:{expected_name}")), Some(kind));
        let spec = kind.spec();
        assert_eq!(spec.protocol_id as usize, id);
        assert_eq!(spec.registry_name, expected_name);
        assert!(spec.min_lifetime_ticks > 0);
        assert!(spec.max_lifetime_ticks >= spec.min_lifetime_ticks);
        assert!(spec.base_radius.is_finite() && spec.base_radius > 0.0);
        assert!(spec.tint_rgba.iter().all(|v| v.is_finite() && (0.0..=1.0).contains(v)));
        assert!(spec.drag_per_sec.is_finite() && spec.gravity_per_sec2.is_finite());
    }
    assert_eq!(VanillaParticleKind::from_protocol_id(109), None);
    assert_eq!(VanillaParticleKind::from_registry_name("not_a_particle"), None);
    assert_eq!(
        VanillaParticleKind::from_registry_name("trial_spawner_detection"),
        Some(VanillaParticleKind::TrialSpawnerDetectedPlayer)
    );
}

#[test]
fn protocol_payload_shapes_are_exact_and_validated() {
    for kind in PROTOCOL767_PARTICLE_KINDS {
        let payload_kind = kind.spec().payload_kind;
        let expected = match kind.registry_name() {
            "block" | "block_marker" | "falling_dust" | "dust_pillar" => VanillaParticlePayloadKind::BlockState,
            "item" => VanillaParticlePayloadKind::ItemStack,
            "dust" => VanillaParticlePayloadKind::Dust,
            "dust_color_transition" => VanillaParticlePayloadKind::DustColorTransition,
            "vibration" => VanillaParticlePayloadKind::Vibration,
            "entity_effect" => VanillaParticlePayloadKind::EntityEffect,
            "sculk_charge" => VanillaParticlePayloadKind::SculkCharge,
            "shriek" => VanillaParticlePayloadKind::Shriek,
            _ => VanillaParticlePayloadKind::None,
        };
        assert_eq!(payload_kind, expected, "{}", kind.registry_name());
    }
    let mut world = VanillaParticleWorld::new();
    assert!(!world.spawn_with_payload(
        VanillaParticleKind::Dust,
        [0.0; 3],
        [0.0; 3],
        [1.0; 4],
        1.0,
        VanillaParticlePayload::BlockState(4),
    ));
    assert!(world.spawn_with_payload(
        VanillaParticleKind::Dust,
        [0.0; 3],
        [0.0; 3],
        [1.0; 4],
        1.0,
        VanillaParticlePayload::Dust { rgb: [0.2, 0.4, 0.8], scale: 2.0 },
    ));
    assert!(!world.spawn_with_payload(
        VanillaParticleKind::Dust,
        [0.0; 3],
        [0.0; 3],
        [1.0; 4],
        1.0,
        VanillaParticlePayload::Dust { rgb: [f32::NAN; 3], scale: 1.0 },
    ));
}

#[test]
fn tactile_impacts_follow_the_protocol_particle_registry() {
    for (id, kind) in PROTOCOL767_PARTICLE_KINDS.into_iter().enumerate() {
        let expected = match id {
            5 | 6 | 17 => Some(ImpactMaterial::CriticalHit),
            22 => Some(ImpactMaterial::OreOrStone),
            _ => None,
        };
        assert_eq!(
            ImpactMaterial::for_vanilla_particle(kind),
            expected,
            "{id}: {kind:?}"
        );
    }
}

#[test]
fn particle_payloads_drive_travel_delay_and_billboard_roll() {
    let mut world = VanillaParticleWorld::new();
    let origin = [2.0, -3.0, 4.0];
    let destination = [6.0, -1.0, 8.0];
    let mut spawn = |kind: VanillaParticleKind, payload| {
        assert!(world.spawn_protocol(kind.protocol_id(), origin, [0.0; 3], 0, payload));
    };
    spawn(
        VanillaParticleKind::Vibration,
        VanillaParticlePayload::Vibration {
            origin,
            destination,
            travel_ticks: 20,
        },
    );
    spawn(
        VanillaParticleKind::Shriek,
        VanillaParticlePayload::Shriek { delay_ticks: 15 },
    );
    spawn(
        VanillaParticleKind::SculkCharge,
        VanillaParticlePayload::SculkCharge {
            roll: std::f32::consts::FRAC_PI_2,
        },
    );
    let right = [1.0, 0.0, 0.0];
    let up = [0.0, 1.0, 0.0];
    let initial = world.billboard_quads(right, up);
    assert_eq!(initial.len(), 2);
    let rolled = &initial[1];
    assert_eq!(rolled.kind, VanillaParticleKind::SculkCharge);
    assert!(rolled.positions[0][0] > origin[0]);
    assert!(rolled.positions[0][1] < origin[1]);

    world.step(0.5);
    let traveling = &world.particles()[0];
    assert_eq!(traveling.pos, [4.0, -2.0, 6.0]);
    assert_eq!(traveling.vel, [4.0, 2.0, 4.0]);
    assert!(world
        .billboard_quads(right, up)
        .iter()
        .all(|q| q.kind != VanillaParticleKind::Shriek));
    world.step(0.3);
    assert!(world
        .billboard_quads(right, up)
        .iter()
        .any(|q| q.kind == VanillaParticleKind::Shriek));
    world.step(0.2);
    assert!(world
        .particles()
        .iter()
        .all(|p| p.kind != VanillaParticleKind::Vibration));
}

#[test]
fn shulker_lid_moves_along_facing_and_has_separate_face_uvs() {
    for (facing, axis, sign) in [
        (VanillaBerFacing::Up, 1, 1.0),
        (VanillaBerFacing::Down, 1, -1.0),
        (VanillaBerFacing::East, 0, 1.0),
        (VanillaBerFacing::West, 0, -1.0),
        (VanillaBerFacing::South, 2, 1.0),
        (VanillaBerFacing::North, 2, -1.0),
    ] {
        let state = |open_progress| VanillaBerExtendedState::ShulkerBox {
            facing,
            open_progress,
            dye: BerDyeColor::Cyan,
        };
        let closed = build_extended_ber_mesh(VanillaBerExtendedKind::ShulkerBox, state(0.0));
        let open = build_extended_ber_mesh(VanillaBerExtendedKind::ShulkerBox, state(1.0));
        let lid_center = |mesh: &megu_client_mesh::vanilla_plus_3d::ExtrudedMesh| {
            let lid = &mesh.quads[12..18];
            lid.iter()
                .flat_map(|q| q.positions)
                .map(|p| p[axis])
                .sum::<f32>()
                / 24.0
        };
        assert!(((lid_center(&open) - lid_center(&closed)) * sign - 0.14).abs() < 1e-5);
        for (i, face) in open.quads[..6].iter().enumerate() {
            assert_eq!(face.tint_index, BerDyeColor::Cyan.tint_index());
            assert!(face.uv.iter().flatten().all(|v| (0.0..=1.0).contains(v)));
            assert!(open.quads[..i].iter().all(|earlier| earlier.uv != face.uv));
        }
    }
}

#[test]
fn all_protocol_particles_simulate_and_billboard_without_degenerates() {
    let mut world = VanillaParticleWorld::new();
    for kind in PROTOCOL767_PARTICLE_KINDS {
        assert!(world.spawn_protocol(
            kind.protocol_id(),
            [0.25, 1.5, -0.5],
            [0.15, 0.4, -0.1],
            0x1234_5678 + u64::from(kind.protocol_id()),
            VanillaParticlePayload::None,
        ));
    }
    assert_eq!(world.len(), PROTOCOL767_PARTICLE_COUNT);
    world.step(1.0 / 60.0);
    world.step(1.0 / 30.0);
    let quads = world.billboard_quads([1.0, 0.0, 0.0], [0.0, 1.0, 0.0]);
    assert_eq!(quads.len(), PROTOCOL767_PARTICLE_COUNT);
    for quad in quads {
        assert_finite_positions(&quad.positions);
        assert!(quad.color.iter().all(|v| v.is_finite()));
        assert!(quad_area(&quad.positions) > 1.0e-8, "{:?}", quad.kind);
    }
    let axes = VanillaParticleWorld::billboard_axes([1.0, 0.0, 0.0], [1.0, 0.0, 0.0]);
    assert!(axes.0.iter().chain(axes.1.iter()).all(|v| v.is_finite()));
}

#[test]
fn particle_pool_is_bounded_fifo_and_rejects_nan() {
    let mut world = VanillaParticleWorld::new();
    assert!(!world.spawn(
        VanillaParticleKind::Flame,
        [f32::NAN, 0.0, 0.0],
        [0.0; 3],
        [1.0; 4],
        1.0,
    ));
    for i in 0..(MAX_VANILLA_PARTICLES + 257) {
        assert!(world.spawn(
            VanillaParticleKind::Smoke,
            [i as f32 * 0.001, 0.0, 0.0],
            [0.0, 0.1, 0.0],
            [1.0; 4],
            60.0,
        ));
    }
    assert_eq!(world.len(), MAX_VANILLA_PARTICLES);
    assert_eq!(world.particles()[0].pos[0], 257.0 * 0.001);
}

#[test]
fn extended_ber_catalog_covers_all_kinds_and_states() {
    for state in representative_states() {
        let kind = state.kind();
        let mesh = build_extended_ber_mesh(kind, state);
        assert!(!mesh.quads.is_empty(), "{kind:?}");
        assert!(mesh.quads.len() <= megu_client_mesh::vanilla_plus_3d::MAX_UPGRADE_QUADS);
        for quad in &mesh.quads {
            assert!(quad.positions.iter().flatten().all(|v| v.is_finite()));
            assert!(quad_area(&quad.positions) > 1.0e-8, "{kind:?} degenerate quad");
        }
        let generic = build_ber_mesh(kind, state);
        assert_eq!(generic, mesh);
    }
    for kind in megu_client_mesh::ber_instancing::VANILLA_EXTENDED_BER_KINDS {
        assert!(representative_states().iter().any(|state| state.kind() == kind), "missing {kind:?}");
    }
}

#[test]
fn ber_animation_state_changes_geometry_and_materials() {
    let chest_closed = build_extended_ber_mesh(
        VanillaBerExtendedKind::Chest,
        VanillaBerExtendedState::Chest { lid_open: 0.0, double: false, facing: VanillaBerFacing::North, chest: BerChestKind::Normal },
    );
    let chest_open = build_extended_ber_mesh(
        VanillaBerExtendedKind::Chest,
        VanillaBerExtendedState::Chest { lid_open: 1.0, double: false, facing: VanillaBerFacing::North, chest: BerChestKind::Normal },
    );
    assert_ne!(chest_closed, chest_open);

    let mut dye_tints = std::collections::BTreeSet::new();
    for dye in BerDyeColor::ALL {
        let mesh = build_extended_ber_mesh(
            VanillaBerExtendedKind::ShulkerBox,
            VanillaBerExtendedState::ShulkerBox { open_progress: 0.2, facing: VanillaBerFacing::Up, dye },
        );
        assert!(mesh.quads.iter().any(|q| q.tint_index == dye.tint_index()));
        dye_tints.insert(dye.tint_index());
    }
    assert_eq!(dye_tints.len(), 16);

    let piston_in = build_extended_ber_mesh(
        VanillaBerExtendedKind::MovingPiston,
        VanillaBerExtendedState::MovingPiston { facing: VanillaBerFacing::North, sticky: true, extension_progress: 0.0 },
    );
    let piston_out = build_extended_ber_mesh(
        VanillaBerExtendedKind::MovingPiston,
        VanillaBerExtendedState::MovingPiston { facing: VanillaBerFacing::North, sticky: true, extension_progress: 1.0 },
    );
    assert_ne!(piston_in, piston_out);

    for stage in 0..=3 {
        let mesh = build_extended_ber_mesh(
            VanillaBerExtendedKind::BrushableSand,
            VanillaBerExtendedState::BrushableSand { dusted: stage, item_id: 2, facing: VanillaBerFacing::Up },
        );
        assert!(!mesh.quads.is_empty());
    }

    let shulker_closed = build_extended_ber_mesh(
        VanillaBerExtendedKind::ShulkerBox,
        VanillaBerExtendedState::ShulkerBox { open_progress: 0.0, facing: VanillaBerFacing::Up, dye: BerDyeColor::Red },
    );
    let shulker_spun = build_extended_ber_mesh(
        VanillaBerExtendedKind::ShulkerBox,
        VanillaBerExtendedState::ShulkerBox { open_progress: 1.0, facing: VanillaBerFacing::Up, dye: BerDyeColor::Red },
    );
    assert_ne!(shulker_closed, shulker_spun);

    let beam_a = build_extended_ber_mesh(
        VanillaBerExtendedKind::Beacon,
        VanillaBerExtendedState::Beacon { beam_height: 8.0, active: true, rgb: [1.0; 3], beam_yaw: 0.0 },
    );
    let beam_b = build_extended_ber_mesh(
        VanillaBerExtendedKind::Beacon,
        VanillaBerExtendedState::Beacon { beam_height: 8.0, active: true, rgb: [1.0; 3], beam_yaw: 0.4 },
    );
    assert_ne!(beam_a, beam_b);
    assert!(beam_a.quads.iter().flat_map(|q| q.positions.iter().flatten()).any(|v| *v > 8.0));

    let spawner_a = build_extended_ber_mesh(
        VanillaBerExtendedKind::TrialSpawner,
        VanillaBerExtendedState::TrialSpawner { spin_rad: 0.0, active: true, ominous: false, mob_scale: 0.8 },
    );
    let spawner_b = build_extended_ber_mesh(
        VanillaBerExtendedKind::TrialSpawner,
        VanillaBerExtendedState::TrialSpawner { spin_rad: 0.9, active: true, ominous: false, mob_scale: 0.8 },
    );
    assert_ne!(spawner_a, spawner_b);

    for skull in [
        BerSkullKind::Player, BerSkullKind::Skeleton, BerSkullKind::WitherSkeleton,
        BerSkullKind::Zombie, BerSkullKind::Creeper, BerSkullKind::Piglin, BerSkullKind::EnderDragon,
    ] {
        let mesh = build_extended_ber_mesh(
            VanillaBerExtendedKind::Skull,
            VanillaBerExtendedState::Skull { skull, wall: false, yaw_deg: 0.0, jaw_open: 0.5 },
        );
        assert!(!mesh.quads.is_empty());
    }
}

#[test]
fn vanilla_tactile_feedback_is_available_when_cosmetic_preset_is_off() {
    let mut sim = TactileEffectsSim::new(TactileEffectsConfig::default(), VfxHardwareTier::Balanced);
    assert!(!sim.config().preset.is_active());
    let pos = [4.0, 65.0, -3.0];
    for stage in 0..=9 {
        assert!(sim.set_block_crack_stage(pos, [0.0, 1.0, 0.0], stage));
        assert_eq!(sim.crack_feedback()[0].stage, stage);
        assert_eq!(sim.crack_feedback()[0].progress(), f32::from(stage) / 10.0);
    }
    assert!(sim.emit_block_break_debris(pos, [0.4, 0.3, 0.2], 12, 7) > 0);
    assert!(sim.emit_footstep_dust(pos, GroundSurfaceKind::Sand, 8) > 0);
    assert!(sim.emit_landing_puffs(pos, GroundSurfaceKind::Gravel, 8.0, 9) > 0);
    assert!(sim.emit_water_entry_splash(pos, 5.0, FluidEntryBody::PlayerOrMediumMob, 10) > 0);
    assert!(sim.emit_sword_sweep_arc(pos, 0.4, 1.0, 11) == 7);
    assert!(sim.emit_critical_hit_sparks(pos, 12) == 8);
    assert!(sim.emit_mace_smash_shockwave(pos, 12.0, 13) > 0);
    assert!(sim.particles().iter().any(|p| matches!(p.kind, TactileParticleKind::BlockBreakDebris)));
    assert!(sim.particles().iter().any(|p| matches!(p.kind, TactileParticleKind::SwordSweepArc)));
    assert!(sim.particles().iter().any(|p| matches!(p.kind, TactileParticleKind::CriticalHitSpark)));
    assert!(sim.particles().iter().any(|p| matches!(p.kind, TactileParticleKind::MaceSmashShockwaveRing { .. })));
    assert!(sim.len() <= sim.particle_cap());
    sim.step(1.0 / 20.0);
    assert!(sim.particles().iter().all(|p| p.pos.iter().chain(p.vel.iter()).all(|v| v.is_finite())));
}

#[test]
fn crack_feedback_capacity_is_bounded() {
    let mut sim = TactileEffectsSim::default();
    for i in 0..(MAX_BLOCK_CRACK_FEEDBACK + 4) {
        assert!(sim.set_block_crack_stage([i as f32, 0.0, 0.0], [0.0, 1.0, 0.0], i as u8));
    }
    assert_eq!(sim.crack_feedback().len(), MAX_BLOCK_CRACK_FEEDBACK);
    assert_eq!(sim.crack_feedback()[0].pos[0], 4.0);
}

#[test]
fn rotating_ber_parts_stay_finite_nondegenerate_across_animation_range() {
    use megu_client_mesh::ber_instancing::VanillaBerExtendedState as S;
    for step in 0..=16 {
        let f = step as f32 / 16.0;
        let angle = f * std::f32::consts::TAU;
        let states = [
            S::ShulkerBox { open_progress: f, facing: VanillaBerFacing::Up, dye: BerDyeColor::Red },
            S::Sign { wall: false, hanging: false, glowing: false, facing_yaw: angle },
            S::Banner { wall: false, sway_rad: f - 0.5, facing_yaw: angle, dye: BerDyeColor::White },
            S::EnchantingTable { facing_yaw: angle, book_yaw_rad: angle * 1.7, page_open: f },
            S::Beacon { beam_height: 12.0, active: true, rgb: [1.0, 0.2, 0.2], beam_yaw: angle },
            S::Conduit { cage_yaw_rad: angle, eye_open: f },
            S::TrialSpawner { spin_rad: angle, active: true, ominous: step % 2 == 0, mob_scale: 0.2 + 0.8 * f },
            S::Vault { spin_rad: angle, active: true, ominous: step % 2 == 0 },
            S::Skull { skull: BerSkullKind::EnderDragon, wall: step % 2 == 0, yaw_deg: angle.to_degrees(), jaw_open: f },
        ];
        for state in states {
            let mesh = build_extended_ber_mesh(state.kind(), state);
            assert!(!mesh.quads.is_empty(), "{state:?}");
            for quad in &mesh.quads {
                assert!(quad.positions.iter().flatten().all(|v| v.is_finite()));
                assert!(quad_area(&quad.positions) > 1.0e-8, "{state:?} degenerate at step {step}");
            }
        }
    }
}

#[test]
fn extended_ber_classifier_covers_vanilla_registry_families() {
    use megu_client_mesh::ber_instancing::{
        classify_vanilla_ber_extended, default_extended_ber_state,
    };
    let cases = [
        ("minecraft:chest", VanillaBerExtendedKind::Chest),
        ("trapped_chest", VanillaBerExtendedKind::Chest),
        ("ender_chest", VanillaBerExtendedKind::Chest),
        ("red_shulker_box", VanillaBerExtendedKind::ShulkerBox),
        ("red_bed", VanillaBerExtendedKind::Bed),
        ("oak_sign", VanillaBerExtendedKind::Sign),
        ("oak_wall_sign", VanillaBerExtendedKind::Sign),
        ("oak_hanging_sign", VanillaBerExtendedKind::Sign),
        ("red_banner", VanillaBerExtendedKind::Banner),
        ("red_wall_banner", VanillaBerExtendedKind::Banner),
        ("bell", VanillaBerExtendedKind::Bell),
        ("enchanting_table", VanillaBerExtendedKind::EnchantingTable),
        ("beacon", VanillaBerExtendedKind::Beacon),
        ("conduit", VanillaBerExtendedKind::Conduit),
        ("soul_campfire", VanillaBerExtendedKind::Campfire),
        ("lectern", VanillaBerExtendedKind::Lectern),
        ("decorated_pot", VanillaBerExtendedKind::DecoratedPot),
        ("suspicious_sand", VanillaBerExtendedKind::BrushableSand),
        ("suspicious_gravel", VanillaBerExtendedKind::BrushableGravel),
        ("trial_spawner", VanillaBerExtendedKind::TrialSpawner),
        ("vault", VanillaBerExtendedKind::Vault),
        ("moving_piston", VanillaBerExtendedKind::MovingPiston),
        ("dragon_head", VanillaBerExtendedKind::Skull),
        ("piglin_wall_head", VanillaBerExtendedKind::Skull),
        ("brewing_stand", VanillaBerExtendedKind::BrewingStand),
    ];
    for (id, kind) in cases {
        assert_eq!(classify_vanilla_ber_extended(id), Some(kind), "{id}");
        let state = default_extended_ber_state(kind);
        assert_eq!(state.kind(), kind);
        assert!(!build_extended_ber_mesh(kind, state).quads.is_empty());
    }
    assert_eq!(classify_vanilla_ber_extended("minecraft:stone"), None);
}

#[test]
fn vfx_adapter_preserves_protocol_lifetime_bounds_and_fullbright_flags() {
    use megu_client_mesh::vfx_engine::{vanilla_particle_emitter_spec, vanilla_particle_is_fullbright};
    for kind in PROTOCOL767_PARTICLE_KINDS {
        let spec = vanilla_particle_emitter_spec(kind, [0.0; 3], [0.0; 3]);
        assert_eq!(spec.particle_lifetime_ticks, kind.spec().max_lifetime_ticks);
        assert!(spec.drag.is_finite() && (0.0..=1.0).contains(&spec.drag));
        assert!(spec.gravity_per_tick.is_finite());
        assert!(spec.size.sample(0.0) > 0.0);
        assert_eq!(vanilla_particle_is_fullbright(kind), kind.spec().fullbright, "{}", kind.registry_name());
    }
}
