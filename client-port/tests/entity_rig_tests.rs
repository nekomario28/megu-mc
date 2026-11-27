// SPDX-License-Identifier: LGPL-3.0-or-later

use std::f32::consts::FRAC_PI_2;

use megu_client_mesh::entity_rig::{
    build_entity_from_cem_boxes, build_entity_rig_mesh, classify_entity_rig,
    entity_mesh_fingerprint, CemBox, EntityPose, EntityRigArchetype, EntityRigPart,
};
use megu_client_mesh::vanilla_plus_3d::ExtrudedMesh;

fn verify_mesh(mesh: &ExtrudedMesh) {
    assert!(!mesh.quads.is_empty());
    for quad in &mesh.quads {
        assert!(quad.positions.iter().flatten().all(|v| v.is_finite()));
        assert!(quad.normal.iter().all(|v| v.is_finite()));
        assert!(quad
            .uv
            .iter()
            .flatten()
            .all(|v| v.is_finite() && (0.0..=1.0).contains(v)));
        let [a, b, c, _] = quad.positions;
        let x = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
        let y = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
        let cross = [
            x[1] * y[2] - x[2] * y[1],
            x[2] * y[0] - x[0] * y[2],
            x[0] * y[1] - x[1] * y[0],
        ];
        let dot = cross
            .iter()
            .zip(quad.normal)
            .map(|(a, b)| a * b)
            .sum::<f32>();
        assert!(dot > 0.0, "quad is not outward CCW");
        let normal_len = quad.normal.iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!((normal_len - 1.0).abs() < 1e-4, "non-unit normal");
    }
}

fn y_extent(mesh: &ExtrudedMesh) -> f32 {
    mesh.quads
        .iter()
        .flat_map(|q| q.positions.iter().map(|p| p[1]))
        .fold(f32::NEG_INFINITY, f32::max)
}

#[test]
fn all_130_entity_ids_have_finite_ccw_meshes_and_exact_boundaries() {
    for id in 0..=129 {
        let spec = classify_entity_rig(id).expect("protocol 767 entity must exist");
        assert_eq!(spec.entity_type_id, id);
        assert!(spec.name.starts_with("minecraft:"));
        assert!(spec.width >= 0.0 && spec.height >= 0.0);
        let mesh = build_entity_rig_mesh(spec, EntityPose::default());
        verify_mesh(&mesh);
        assert_eq!(
            mesh,
            build_entity_rig_mesh(spec, EntityPose::default()),
            "id {id}"
        );
        assert_eq!(
            entity_mesh_fingerprint(&mesh),
            entity_mesh_fingerprint(&build_entity_rig_mesh(spec, EntityPose::default()))
        );
    }
    assert!(classify_entity_rig(130).is_none());
    assert!(classify_entity_rig(u16::MAX).is_none());
}

#[test]
fn canonical_registry_dimensions_and_flags() {
    let player = classify_entity_rig(128).unwrap();
    assert_eq!(
        (player.name, player.width, player.height),
        ("minecraft:player", 0.6, 1.8)
    );
    assert_eq!(player.archetype, EntityRigArchetype::Biped);
    assert!(player.is_player);
    assert_eq!(classify_entity_rig(9).unwrap().width, 0.0);
    assert_eq!(classify_entity_rig(31).unwrap().width, 16.0);
    assert_eq!(classify_entity_rig(14).unwrap().height, 2.375);
    assert!(classify_entity_rig(93).unwrap().translucent_shell);
    assert!(!classify_entity_rig(67).unwrap().translucent_shell);
}

#[test]
fn eight_archetypes_are_distinct_and_representative() {
    use EntityRigArchetype::*;
    let cases = [
        (128, Biped),
        (22, Quadruped),
        (19, Avian),
        (100, Arthropod),
        (93, SlimeCube),
        (20, Aquatic),
        (4, ProjectileOrItem),
        (10, VehicleOrHanging),
    ];
    for (id, expected) in cases {
        let spec = classify_entity_rig(id).unwrap();
        assert_eq!(spec.archetype, expected);
        verify_mesh(&build_entity_rig_mesh(spec, EntityPose::default()));
    }
}

#[test]
fn rig_proportions_control_geometry() {
    let base = classify_entity_rig(128).unwrap();
    let mut modified = base;
    modified.proportions.head *= 0.8;
    let original_mesh = build_entity_rig_mesh(base, EntityPose::default());
    let modified_mesh = build_entity_rig_mesh(modified, EntityPose::default());
    assert_ne!(original_mesh, modified_mesh);
    verify_mesh(&modified_mesh);
}

#[test]
fn walk_attack_swim_sneak_elytra_and_hurt_are_deterministic() {
    let spec = classify_entity_rig(128).unwrap();
    let base = EntityPose {
        limb_swing_amount: 1.0,
        ..EntityPose::default()
    };
    let walk0 = build_entity_rig_mesh(spec, base);
    let walk1 = build_entity_rig_mesh(
        spec,
        EntityPose {
            limb_swing: FRAC_PI_2,
            ..base
        },
    );
    assert_ne!(walk0, walk1);
    let effects = [
        EntityPose {
            swing_progress: 0.5,
            ..base
        },
        EntityPose {
            sneaking: true,
            ..base
        },
        EntityPose {
            swimming: true,
            ..base
        },
        EntityPose {
            elytra_flying: true,
            ..base
        },
        EntityPose {
            head_yaw_deg: 45.0,
            head_pitch_deg: 20.0,
            ..base
        },
        EntityPose {
            hurt_flash: true,
            ..base
        },
    ];
    for pose in effects {
        let changed = build_entity_rig_mesh(spec, pose);
        assert_ne!(changed, walk0);
        assert_eq!(changed, build_entity_rig_mesh(spec, pose));
        verify_mesh(&changed);
    }
    assert!(build_entity_rig_mesh(spec, effects[5])
        .quads
        .iter()
        .all(|q| q.tint_index == 1));
}

#[test]
fn animated_archetype_limbs_and_tail_move() {
    for id in [22_u16, 19, 100, 20] {
        let spec = classify_entity_rig(id).unwrap();
        let before = build_entity_rig_mesh(
            spec,
            EntityPose {
                limb_swing_amount: 1.0,
                age_ticks: 0.0,
                ..EntityPose::default()
            },
        );
        let after = build_entity_rig_mesh(
            spec,
            EntityPose {
                limb_swing_amount: 1.0,
                limb_swing: FRAC_PI_2,
                age_ticks: 11.0,
                ..EntityPose::default()
            },
        );
        assert_ne!(before, after, "id {id}");
        verify_mesh(&after);
    }
}

#[test]
fn baby_is_shorter_and_skin_overlay_adds_quads() {
    for id in [128, 22, 19, 100, 20] {
        let spec = classify_entity_rig(id).unwrap();
        let adult = build_entity_rig_mesh(spec, EntityPose::default());
        let baby = build_entity_rig_mesh(
            spec,
            EntityPose {
                baby: true,
                ..EntityPose::default()
            },
        );
        assert!(y_extent(&baby) < y_extent(&adult), "id {id}");
        verify_mesh(&baby);
    }
    for id in [128, 124, 91] {
        let spec = classify_entity_rig(id).unwrap();
        let base = build_entity_rig_mesh(spec, EntityPose::default());
        let layered = build_entity_rig_mesh(
            spec,
            EntityPose {
                enable_skin_3d_layers: true,
                ..EntityPose::default()
            },
        );
        assert_eq!(layered.quad_count(), base.quad_count() + 36);
        verify_mesh(&layered);
    }
}

#[test]
fn non_finite_pose_values_fail_safe_without_panics() {
    let spec = classify_entity_rig(128).unwrap();
    let bad = EntityPose {
        limb_swing: f32::NAN,
        limb_swing_amount: f32::INFINITY,
        head_yaw_deg: f32::NEG_INFINITY,
        head_pitch_deg: f32::NAN,
        swing_progress: f32::INFINITY,
        age_ticks: f32::NAN,
        ..EntityPose::default()
    };
    let expected = build_entity_rig_mesh(spec, EntityPose::default());
    let actual = build_entity_rig_mesh(spec, bad);
    assert_eq!(actual, expected);
    verify_mesh(&actual);
}

#[test]
fn cem_boxes_replace_base_mesh_and_preserve_animation() {
    let spec = classify_entity_rig(128).unwrap();
    let box_part = CemBox::from_model_pixels(
        EntityRigPart::RightArm,
        [4.0, 8.0, -2.0],
        [4.0, 12.0, 4.0],
        [6.0, 20.0, 0.0],
        [0.0, 0.0, 0.0],
        [0.0, 0.0, 16.0, 16.0],
        [64.0, 64.0],
    )
    .unwrap();
    let pose = EntityPose {
        limb_swing_amount: 1.0,
        ..EntityPose::default()
    };
    let overridden = build_entity_from_cem_boxes(spec, pose, &[box_part]);
    assert_eq!(overridden.quad_count(), 6);
    verify_mesh(&overridden);
    assert_ne!(
        overridden,
        build_entity_from_cem_boxes(
            spec,
            EntityPose {
                limb_swing: FRAC_PI_2,
                ..pose
            },
            &[box_part]
        )
    );
    assert_eq!(
        overridden,
        build_entity_from_cem_boxes(spec, pose, &[box_part])
    );
    assert_eq!(
        build_entity_from_cem_boxes(spec, pose, &[]),
        build_entity_rig_mesh(spec, pose)
    );
    let mut invalid = box_part;
    invalid.origin[1] = f32::NAN;
    assert_eq!(
        build_entity_from_cem_boxes(spec, pose, &[invalid]),
        build_entity_rig_mesh(spec, pose)
    );
    assert!(CemBox::from_model_pixels(
        EntityRigPart::Other,
        [0.0; 3],
        [1.0; 3],
        [0.0; 3],
        [0.0; 3],
        [0.0, 0.0, 1.0, 1.0],
        [f32::NAN, 64.0]
    )
    .is_none());
}

#[test]
fn golden_fingerprints_cover_all_eight_templates() {
    let ids = [128_u16, 22, 19, 100, 93, 20, 4, 10];
    let hashes = ids.map(|id| {
        let spec = classify_entity_rig(id).unwrap();
        entity_mesh_fingerprint(&build_entity_rig_mesh(spec, EntityPose::default()))
    });
    assert_eq!(
        hashes,
        // Positions, normals and the per-face Vanilla UV layout are hashed.
        [
            3592591952937024729,
            15085160042208170496,
            10736852550645372078,
            8493232011285385773,
            10887454203213746611,
            6875287801684049146,
            41524942627765577,
            14271231537484498720,
        ]
    );
}
