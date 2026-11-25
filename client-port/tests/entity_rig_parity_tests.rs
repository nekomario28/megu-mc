// SPDX-License-Identifier: LGPL-3.0-or-later

use std::collections::BTreeSet;
use std::f32::consts::FRAC_PI_2;

use megu_client_mesh::entity_rig::{
    build_entity_rig_mesh, build_entity_rig_mesh_with_pose, classify_entity_rig,
    entity_mesh_fingerprint, EntityPose, EntityRigPose,
};
use megu_client_mesh::vanilla_plus_3d::{ExtrudedMesh, MAX_UPGRADE_QUADS};

fn verify_mesh(mesh: &ExtrudedMesh) {
    assert!(!mesh.quads.is_empty());
    assert!(mesh.quads.len() <= MAX_UPGRADE_QUADS);
    for quad in &mesh.quads {
        assert!(quad.positions.iter().flatten().all(|v| v.is_finite()));
        assert!(quad.normal.iter().all(|v| v.is_finite()));
        assert!(quad
            .uv
            .iter()
            .flatten()
            .all(|v| v.is_finite() && (0.0..=1.0).contains(v)));
        let normal_len = quad.normal.iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!((normal_len - 1.0).abs() < 1e-4, "non-unit normal");
        let [a, b, c, d] = quad.positions;
        let ab = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
        let ac = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
        let cross = [
            ab[1] * ac[2] - ab[2] * ac[1],
            ab[2] * ac[0] - ab[0] * ac[2],
            ab[0] * ac[1] - ab[1] * ac[0],
        ];
        let area2 = cross.iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!(
            area2 > 1e-8,
            "degenerate quad {positions:?}",
            positions = quad.positions
        );
        assert!(d.iter().all(|v| v.is_finite()));
    }
}

fn bounds(mesh: &ExtrudedMesh) -> ([f32; 3], [f32; 3]) {
    let mut lo = [f32::INFINITY; 3];
    let mut hi = [f32::NEG_INFINITY; 3];
    for quad in &mesh.quads {
        for point in &quad.positions {
            for axis in 0..3 {
                lo[axis] = lo[axis].min(point[axis]);
                hi[axis] = hi[axis].max(point[axis]);
            }
        }
    }
    (lo, hi)
}

fn y_extent(mesh: &ExtrudedMesh) -> f32 {
    mesh.quads
        .iter()
        .flat_map(|quad| quad.positions.iter().map(|point| point[1]))
        .fold(f32::NEG_INFINITY, f32::max)
}

fn quads_with_tint(mesh: &ExtrudedMesh, tint: i32) -> usize {
    mesh.quads
        .iter()
        .filter(|quad| quad.tint_index == tint)
        .count()
}

#[test]
fn all_130_protocol_ids_build_bounded_deterministic_non_degenerate_meshes() {
    let mut fingerprints = BTreeSet::new();
    for id in 0..=129 {
        let spec = classify_entity_rig(id).expect("protocol 767 entity must classify");
        let mesh = build_entity_rig_mesh(spec, EntityPose::default());
        verify_mesh(&mesh);
        assert_eq!(
            mesh,
            build_entity_rig_mesh(spec, EntityPose::default()),
            "id {id}"
        );
        assert_eq!(
            entity_mesh_fingerprint(&mesh),
            entity_mesh_fingerprint(&build_entity_rig_mesh(spec, EntityPose::default())),
            "id {id}"
        );
        fingerprints.insert(entity_mesh_fingerprint(&mesh));
    }
    // Visual variants intentionally share anatomy in a few cases (for example
    // logically invisible markers), but the catalogue should not collapse to
    // the eight old archetype fingerprints.
    assert!(
        fingerprints.len() >= 95,
        "only {} distinct fingerprints across 130 rigs",
        fingerprints.len()
    );
}

#[test]
fn boss_rigs_have_distinctive_multipart_silhouettes() {
    let dragon = classify_entity_rig(31).unwrap();
    let dragon_mesh = build_entity_rig_mesh(dragon, EntityPose::default());
    verify_mesh(&dragon_mesh);
    assert!(
        dragon_mesh.quads.len() > 220,
        "dragon quads {}",
        dragon_mesh.quads.len()
    );
    let (lo, hi) = bounds(&dragon_mesh);
    assert!(hi[0] - lo[0] > dragon.width * 0.75, "dragon wingspan");
    assert!(
        hi[2] - lo[2] > dragon.width * 0.65,
        "dragon neck+tail length"
    );

    let wither = classify_entity_rig(119).unwrap();
    let wither_mesh = build_entity_rig_mesh(wither, EntityPose::default());
    verify_mesh(&wither_mesh);
    assert!(wither_mesh.quads.len() >= 66, "wither quads");
    let (lo, hi) = bounds(&wither_mesh);
    assert!(
        hi[0] - lo[0] > wither.width * 1.35,
        "wither side heads span"
    );

    let warden = classify_entity_rig(116).unwrap();
    let warden_mesh = build_entity_rig_mesh(warden, EntityPose::default());
    verify_mesh(&warden_mesh);
    assert!(warden_mesh.quads.len() > 75, "warden quads");
    assert!(
        quads_with_tint(&warden_mesh, 9) >= 30,
        "warden soul core/tendrils"
    );

    for id in [29_u16, 50] {
        let guardian = classify_entity_rig(id).unwrap();
        let mesh = build_entity_rig_mesh(guardian, EntityPose::default());
        verify_mesh(&mesh);
        assert_eq!(
            quads_with_tint(&mesh, 12),
            72,
            "guardian id {id} spike quads"
        );
        assert!(quads_with_tint(&mesh, 11) == 6, "guardian id {id} pupil");
    }
}

#[test]
fn signature_mob_details_are_present() {
    let ghast = classify_entity_rig(45).unwrap();
    let ghast_mesh = build_entity_rig_mesh(ghast, EntityPose::default());
    verify_mesh(&ghast_mesh);
    assert!(
        ghast_mesh.quads.len() >= 60,
        "ghast body plus nine tentacles"
    );

    let blaze = classify_entity_rig(8).unwrap();
    let blaze_mesh = build_entity_rig_mesh(blaze, EntityPose::default());
    verify_mesh(&blaze_mesh);
    assert_eq!(quads_with_tint(&blaze_mesh, 6), 72, "twelve blaze rods");

    let breeze = classify_entity_rig(12).unwrap();
    let breeze_mesh = build_entity_rig_mesh(breeze, EntityPose::default());
    verify_mesh(&breeze_mesh);
    assert_eq!(
        quads_with_tint(&breeze_mesh, 5),
        72,
        "twelve breeze wind rods"
    );

    let slime = classify_entity_rig(93).unwrap();
    let slime_mesh = build_entity_rig_mesh(slime, EntityPose::default());
    verify_mesh(&slime_mesh);
    assert!(
        quads_with_tint(&slime_mesh, 16) >= 18,
        "slime eyes and mouth"
    );

    let villager = classify_entity_rig(113).unwrap();
    let villager_mesh = build_entity_rig_mesh(villager, EntityPose::default());
    let (lo, _) = bounds(&villager_mesh);
    assert!(
        lo[2] < -villager.width * 0.5,
        "villager nose protrudes forward"
    );

    let witch = classify_entity_rig(118).unwrap();
    let witch_mesh = build_entity_rig_mesh(witch, EntityPose::default());
    assert!(
        witch_mesh.quads.len() > villager_mesh.quads.len(),
        "witch hat adds geometry"
    );

    let creeper = classify_entity_rig(23).unwrap();
    let creeper_mesh = build_entity_rig_mesh(creeper, EntityPose::default());
    verify_mesh(&creeper_mesh);
    let low_quads = creeper_mesh
        .quads
        .iter()
        .filter(|quad| {
            quad.positions.iter().map(|p| p[1]).sum::<f32>() / 4.0 < creeper.height * 0.24
        })
        .count();
    assert!(low_quads >= 24, "four creeper feet");
}

#[test]
fn vehicles_projectiles_and_special_entities_are_not_plain_collision_cubes() {
    for (id, minimum_quads) in [
        (10_u16, 60), // boat hull, walls, seats, paddles
        (17, 66),     // chest boat
        (69, 30),     // minecart tub
        (98, 90),     // spawner minecart cage
        (30, 150),    // end crystal frames + core
        (3, 42),      // armor stand
        (4, 24),      // arrow shaft/head/fletching
        (109, 30),    // trident prongs
        (64, 42),     // jagged lightning
        (129, 18),    // fishing bobber + hook
    ] {
        let spec = classify_entity_rig(id).unwrap();
        let mesh = build_entity_rig_mesh(spec, EntityPose::default());
        verify_mesh(&mesh);
        assert!(
            mesh.quads.len() >= minimum_quads,
            "id {id}: {} < {minimum_quads}",
            mesh.quads.len()
        );
    }
}

#[test]
fn animation_pose_inputs_change_articulated_parts_deterministically() {
    for id in [31_u16, 116, 119, 128, 22, 7, 101] {
        let spec = classify_entity_rig(id).unwrap();
        let base = EntityPose {
            limb_swing_amount: 1.0,
            ..EntityPose::default()
        };
        let before = build_entity_rig_mesh(spec, base);
        for pose in [
            EntityPose {
                limb_swing: FRAC_PI_2,
                ..base
            },
            EntityPose {
                head_yaw_deg: 35.0,
                head_pitch_deg: -15.0,
                ..base
            },
        ] {
            let changed = build_entity_rig_mesh(spec, pose);
            assert_ne!(before, changed, "id {id} pose must animate");
            assert_eq!(
                changed,
                build_entity_rig_mesh(spec, pose),
                "id {id} determinism"
            );
            verify_mesh(&changed);
        }
    }
    // Attack swing is meaningful on rigs with arms or jaws; legless passive
    // species are not expected to invent a swing channel.
    for id in [31_u16, 116, 119, 128] {
        let spec = classify_entity_rig(id).unwrap();
        let calm = build_entity_rig_mesh(spec, EntityPose::default());
        let attacking = build_entity_rig_mesh(
            spec,
            EntityPose {
                swing_progress: 0.5,
                ..EntityPose::default()
            },
        );
        assert_ne!(calm, attacking, "id {id} attack swing");
    }
    // Ambient age-driven motion is separate: wings, rods, tentacles, tails,
    // and boss appendages move even when the entity is not walking.
    for id in [31_u16, 116, 119, 7, 101, 22] {
        let spec = classify_entity_rig(id).unwrap();
        let young = build_entity_rig_mesh(spec, EntityPose::default());
        let older = build_entity_rig_mesh(
            spec,
            EntityPose {
                age_ticks: 7.0,
                ..EntityPose::default()
            },
        );
        assert_ne!(young, older, "id {id} ambient age animation");
    }
}

#[test]
fn extended_pose_states_drive_sit_sleep_armor_items_and_entity_flags() {
    let player = classify_entity_rig(128).unwrap();
    let plain = build_entity_rig_mesh_with_pose(player, EntityRigPose::default());
    for pose in [
        EntityRigPose {
            sitting: true,
            ..EntityRigPose::default()
        },
        EntityRigPose {
            sleeping: true,
            ..EntityRigPose::default()
        },
        EntityRigPose {
            armor_visible: true,
            ..EntityRigPose::default()
        },
        EntityRigPose {
            main_hand_item: true,
            ..EntityRigPose::default()
        },
    ] {
        let changed = build_entity_rig_mesh_with_pose(player, pose);
        assert_ne!(plain, changed);
        verify_mesh(&changed);
    }

    let transformed_a = build_entity_rig_mesh_with_pose(
        player,
        EntityRigPose {
            main_hand_item: true,
            ..EntityRigPose::default()
        },
    );
    let transformed_b = build_entity_rig_mesh_with_pose(
        player,
        EntityRigPose {
            main_hand_item: true,
            main_hand_offset: [0.1, 0.0, -0.05],
            main_hand_rotation_deg: [20.0, 0.0, 35.0],
            ..EntityRigPose::default()
        },
    );
    assert_ne!(transformed_a, transformed_b);

    let wither = classify_entity_rig(119).unwrap();
    let normal_wither = build_entity_rig_mesh_with_pose(wither, EntityRigPose::default());
    let armored_wither = build_entity_rig_mesh_with_pose(
        wither,
        EntityRigPose {
            wither_armored: true,
            ..EntityRigPose::default()
        },
    );
    assert!(armored_wither.quads.len() > normal_wither.quads.len());

    let armadillo = classify_entity_rig(2).unwrap();
    let walking = build_entity_rig_mesh_with_pose(armadillo, EntityRigPose::default());
    let rolled = build_entity_rig_mesh_with_pose(
        armadillo,
        EntityRigPose {
            rolled_up: true,
            ..EntityRigPose::default()
        },
    );
    assert_ne!(walking, rolled);
    assert!(y_extent(&rolled) < y_extent(&walking));

    let enderman = classify_entity_rig(33).unwrap();
    let empty = build_entity_rig_mesh_with_pose(enderman, EntityRigPose::default());
    let carrying = build_entity_rig_mesh_with_pose(
        enderman,
        EntityRigPose {
            carried_block: true,
            screaming: true,
            ..EntityRigPose::default()
        },
    );
    assert!(carrying.quads.len() > empty.quads.len());
    assert_ne!(empty, carrying);
}

#[test]
fn baby_scaling_hurt_flash_and_shulker_creeper_states_follow_contract() {
    for id in [128_u16, 22, 42, 122] {
        let spec = classify_entity_rig(id).unwrap();
        let adult = build_entity_rig_mesh(spec, EntityPose::default());
        let baby = build_entity_rig_mesh(
            spec,
            EntityPose {
                baby: true,
                ..EntityPose::default()
            },
        );
        verify_mesh(&baby);
        assert!(y_extent(&baby) < y_extent(&adult), "id {id} baby height");
    }

    let player = classify_entity_rig(128).unwrap();
    let hurt = build_entity_rig_mesh(
        player,
        EntityPose {
            hurt_flash: true,
            ..EntityPose::default()
        },
    );
    assert!(hurt.quads.iter().all(|quad| quad.tint_index == 1));

    let shulker = classify_entity_rig(88).unwrap();
    let closed = build_entity_rig_mesh_with_pose(shulker, EntityRigPose::default());
    let open = build_entity_rig_mesh_with_pose(
        shulker,
        EntityRigPose {
            open_amount: 1.0,
            ..EntityRigPose::default()
        },
    );
    assert_ne!(closed, open);

    let creeper = classify_entity_rig(23).unwrap();
    let calm = build_entity_rig_mesh_with_pose(creeper, EntityRigPose::default());
    let swelling = build_entity_rig_mesh_with_pose(
        creeper,
        EntityRigPose {
            swell_amount: 1.0,
            ..EntityRigPose::default()
        },
    );
    assert_ne!(calm, swelling);
}

#[test]
fn non_finite_extended_pose_values_are_sanitized() {
    let spec = classify_entity_rig(128).unwrap();
    let mut pose = EntityRigPose::default();
    pose.base.age_ticks = f32::NAN;
    pose.base.limb_swing = f32::INFINITY;
    pose.open_amount = f32::NAN;
    pose.swell_amount = f32::INFINITY;
    pose.main_hand_item = true;
    pose.main_hand_offset = [f32::NAN, 0.0, f32::INFINITY];
    pose.main_hand_rotation_deg = [f32::NAN; 3];
    let mesh = build_entity_rig_mesh_with_pose(spec, pose);
    verify_mesh(&mesh);
    assert_eq!(mesh, build_entity_rig_mesh_with_pose(spec, pose));
}
