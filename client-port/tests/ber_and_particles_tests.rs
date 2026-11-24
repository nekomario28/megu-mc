// SPDX-License-Identifier: LGPL-3.0-or-later

use megu_client_mesh::ber_instancing::*;
use megu_client_mesh::vanilla_plus_3d::{ExtrudedMesh, MAX_UPGRADE_QUADS};

const SOUTH: VanillaBerFacing = VanillaBerFacing::South;

fn states() -> [VanillaBerState; 12] {
    [
        VanillaBerState::Chest {
            lid_open: 0.5,
            double: false,
            facing: SOUTH,
        },
        VanillaBerState::Sign {
            wall: false,
            hanging: false,
            glowing: false,
            facing_yaw: 0.0,
        },
        VanillaBerState::Banner {
            wall: false,
            sway_rad: 0.0,
            facing_yaw: 0.0,
        },
        VanillaBerState::Beacon {
            beam_height: 15.0,
            active: true,
            rgb: [0.5, 0.75, 1.0],
        },
        VanillaBerState::Bed {
            is_head: true,
            facing: SOUTH,
        },
        VanillaBerState::Skull {
            wall: false,
            yaw_deg: 0.0,
        },
        VanillaBerState::ItemFrame {
            facing: SOUTH,
            glow: true,
            rotation: 3,
        },
        VanillaBerState::Campfire {
            lit: true,
            item_count: 4,
        },
        VanillaBerState::BrewingStand { bottle_mask: 0b111 },
        VanillaBerState::Lectern {
            has_book: true,
            facing: SOUTH,
        },
        VanillaBerState::DecoratedPot {
            facing: SOUTH,
            wobble: 0.03,
        },
        VanillaBerState::ShulkerBox {
            open_progress: 0.5,
            facing: SOUTH,
        },
    ]
}

fn check_mesh(mesh: &ExtrudedMesh) {
    assert!(!mesh.quads.is_empty(), "BER mesh must have geometry");
    assert!(mesh.quad_count() <= MAX_UPGRADE_QUADS);
    for quad in &mesh.quads {
        for vertex in quad.positions {
            assert!(
                vertex
                    .iter()
                    .all(|v| v.is_finite() && (0.0..=1.0).contains(v)),
                "out of range vertex: {vertex:?}"
            );
        }
        for uv in quad.uv {
            assert!(uv.iter().all(|v| v.is_finite() && (0.0..=1.0).contains(v)));
        }
        assert!(quad.normal.iter().all(|v| v.is_finite()));
        let a = (0..3)
            .map(|i| quad.positions[1][i] - quad.positions[0][i])
            .collect::<Vec<_>>();
        let b = (0..3)
            .map(|i| quad.positions[2][i] - quad.positions[0][i])
            .collect::<Vec<_>>();
        let n = [
            a[1] * b[2] - a[2] * b[1],
            a[2] * b[0] - a[0] * b[2],
            a[0] * b[1] - a[1] * b[0],
        ];
        let magnitude = n.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(magnitude > 1e-8, "degenerate face");
        let dot = n.iter().zip(quad.normal).map(|(a, b)| a * b).sum::<f32>();
        assert!(dot > 0.0, "normal disagrees with CCW winding");
    }
}

#[test]
fn all_twelve_ber_families_are_bounded_ccw_and_deterministic() {
    for state in states() {
        let mesh = build_ber_mesh(state.kind(), state);
        check_mesh(&mesh);
        assert_eq!(mesh, build_ber_mesh(state.kind(), state));
    }
    let state = VanillaBerState::Bed {
        is_head: false,
        facing: SOUTH,
    };
    assert!(build_ber_mesh(VanillaBerKind::Chest, state)
        .quads
        .is_empty());
}

#[test]
fn chest_lid_moves_with_animation_and_beacon_requires_active_beam() {
    let chest = |lid_open| {
        build_ber_mesh(
            VanillaBerKind::Chest,
            VanillaBerState::Chest {
                lid_open,
                double: false,
                facing: SOUTH,
            },
        )
    };
    let closed = chest(0.0);
    let open = chest(1.0);
    check_mesh(&closed);
    check_mesh(&open);
    assert_eq!(closed.quads.len(), open.quads.len());
    assert!(closed
        .quads
        .iter()
        .zip(&open.quads)
        .any(|(a, b)| a.positions != b.positions));
    assert_eq!(chest(-10.0), closed);
    assert_eq!(chest(99.0), open);
    assert_eq!(chest(f32::INFINITY), open);

    let beacon = |active, height, rgb| {
        build_ber_mesh(
            VanillaBerKind::Beacon,
            VanillaBerState::Beacon {
                active,
                beam_height: height,
                rgb,
            },
        )
    };
    let inactive = beacon(false, 384.0, [1.0; 3]);
    assert_eq!(inactive, beacon(true, 0.0, [1.0; 3]));
    let active = beacon(true, 384.0, [1.0; 3]);
    assert_eq!(active.quads.len(), inactive.quads.len() + 6);
    check_mesh(&active);
    assert_eq!(active, beacon(true, 10000.0, [1.0; 3]));
    assert_eq!(active, beacon(true, f32::INFINITY, [1.0; 3]));
    assert_ne!(active, beacon(true, 384.0, [0.0, 0.5, 1.0]));
}

#[test]
fn ber_alternate_states_and_invalid_floats_fail_safe() {
    let alternate = [
        VanillaBerState::Chest {
            lid_open: f32::NAN,
            double: true,
            facing: VanillaBerFacing::West,
        },
        VanillaBerState::Sign {
            wall: true,
            hanging: true,
            glowing: true,
            facing_yaw: f32::INFINITY,
        },
        VanillaBerState::Banner {
            wall: true,
            sway_rad: f32::NAN,
            facing_yaw: f32::NEG_INFINITY,
        },
        VanillaBerState::Beacon {
            active: true,
            beam_height: f32::INFINITY,
            rgb: [f32::NAN, f32::INFINITY, -1.0],
        },
        VanillaBerState::Bed {
            is_head: false,
            facing: VanillaBerFacing::North,
        },
        VanillaBerState::Skull {
            wall: true,
            yaw_deg: f32::NAN,
        },
        VanillaBerState::ItemFrame {
            facing: VanillaBerFacing::Up,
            glow: false,
            rotation: 255,
        },
        VanillaBerState::Campfire {
            lit: false,
            item_count: 255,
        },
        VanillaBerState::BrewingStand { bottle_mask: 255 },
        VanillaBerState::Lectern {
            has_book: false,
            facing: VanillaBerFacing::East,
        },
        VanillaBerState::DecoratedPot {
            facing: VanillaBerFacing::West,
            wobble: f32::INFINITY,
        },
        VanillaBerState::ShulkerBox {
            open_progress: f32::NAN,
            facing: VanillaBerFacing::Down,
        },
    ];
    for state in alternate {
        check_mesh(&build_ber_mesh(state.kind(), state));
    }
    for facing in [
        VanillaBerFacing::North,
        VanillaBerFacing::South,
        VanillaBerFacing::East,
        VanillaBerFacing::West,
        VanillaBerFacing::Up,
        VanillaBerFacing::Down,
    ] {
        check_mesh(&build_ber_mesh(
            VanillaBerKind::ShulkerBox,
            VanillaBerState::ShulkerBox {
                open_progress: 1.0,
                facing,
            },
        ));
        check_mesh(&build_ber_mesh(
            VanillaBerKind::ItemFrame,
            VanillaBerState::ItemFrame {
                facing,
                glow: true,
                rotation: 7,
            },
        ));
    }
}

#[test]
fn animated_ber_angles_remain_local_and_faces_stay_ccw() {
    for n in -32..=32 {
        let radians = n as f32 * std::f32::consts::PI / 16.0;
        let animated = [
            VanillaBerState::Sign {
                wall: false,
                hanging: true,
                glowing: true,
                facing_yaw: radians,
            },
            VanillaBerState::Banner {
                wall: false,
                sway_rad: radians,
                facing_yaw: radians,
            },
            VanillaBerState::Skull {
                wall: false,
                yaw_deg: radians.to_degrees(),
            },
            VanillaBerState::DecoratedPot {
                facing: SOUTH,
                wobble: radians,
            },
        ];
        for state in animated {
            check_mesh(&build_ber_mesh(state.kind(), state));
        }
    }
}

const KINDS: [VanillaParticleKind; 12] = [
    VanillaParticleKind::BlockBreak,
    VanillaParticleKind::BlockDig,
    VanillaParticleKind::Flame,
    VanillaParticleKind::Smoke,
    VanillaParticleKind::WaterSplash,
    VanillaParticleKind::Bubble,
    VanillaParticleKind::RainDrop,
    VanillaParticleKind::SnowFlake,
    VanillaParticleKind::LightningFlash,
    VanillaParticleKind::PotionSwirl,
    VanillaParticleKind::Crit,
    VanillaParticleKind::Explosion,
];

#[test]
fn particle_kinds_produce_finite_camera_aligned_quads() {
    let mut world = VanillaParticleWorld::new();
    for kind in KINDS {
        assert!(world.spawn(
            kind,
            [1.0, 2.0, 3.0],
            [0.5, 1.0, -0.5],
            [1.0, 0.5, 0.3, 0.9],
            4.0
        ));
    }
    world.step(0.125);
    let quads = world.billboard_quads([1.0, 0.0, 0.0], [0.0, 1.0, 0.0]);
    assert_eq!(quads.len(), KINDS.len());
    for (quad, kind) in quads.iter().zip(KINDS) {
        assert_eq!(quad.kind, kind);
        assert!(quad.positions.iter().flatten().all(|v| v.is_finite()));
        assert!(quad.uv.iter().flatten().all(|v| (0.0..=1.0).contains(v)));
        assert!(quad.color.iter().all(|v| (0.0..=1.0).contains(v)));
        assert!(quad.positions[0][0] < quad.positions[1][0]);
        assert!(quad.positions[0][1] < quad.positions[3][1]);
    }
    assert_eq!(
        world.billboard_quads([1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
        quads
    );
    assert_eq!(
        world
            .billboard_quads([f32::NAN; 3], [f32::INFINITY; 3])
            .len(),
        12
    );
    assert_eq!(world.billboard_quads([0.0; 3], [0.0; 3]).len(), 12);
}

#[test]
fn particles_evict_the_oldest_and_never_exceed_4096() {
    let mut world = VanillaParticleWorld::new();
    for i in 0..10_000 {
        assert!(world.spawn(
            VanillaParticleKind::BlockDig,
            [i as f32, 0.0, 0.0],
            [0.0; 3],
            [1.0; 4],
            10.0
        ));
        assert!(world.len() <= MAX_VANILLA_PARTICLES);
    }
    assert_eq!(world.len(), 4096);
    assert_eq!(world.particles().front().unwrap().pos[0], 5904.0);
    assert_eq!(world.particles().back().unwrap().pos[0], 9999.0);
    assert_eq!(
        world
            .billboard_quads([1.0, 0.0, 0.0], [0.0, 1.0, 0.0])
            .len(),
        4096
    );
    world.clear();
    assert!(world.is_empty());
}

#[test]
fn particles_have_frame_rate_independent_drag_and_gravity() {
    for kind in KINDS {
        let mut slow = VanillaParticleWorld::new();
        let mut fast = VanillaParticleWorld::new();
        for world in [&mut slow, &mut fast] {
            world.spawn(kind, [0.2, 12.0, 0.5], [1.0, 2.5, -1.0], [1.0; 4], 5.0);
        }
        for _ in 0..30 {
            slow.step(1.0 / 30.0);
        }
        for _ in 0..120 {
            fast.step(1.0 / 120.0);
        }
        let a = slow.particles().front().unwrap();
        let b = fast.particles().front().unwrap();
        for i in 0..3 {
            assert!(
                (a.pos[i] - b.pos[i]).abs() < 1e-5,
                "position mismatch: {kind:?}"
            );
            assert!(
                (a.vel[i] - b.vel[i]).abs() < 1e-5,
                "velocity mismatch: {kind:?}"
            );
        }
    }
    let mut world = VanillaParticleWorld::new();
    world.spawn(
        VanillaParticleKind::Flame,
        [0.0; 3],
        [1.0, 0.0, 0.0],
        [1.0; 4],
        2.0,
    );
    world.step(0.5);
    let expected = (-1.8_f64 * 0.5).exp() as f32;
    assert!((world.particles().front().unwrap().vel[0] - expected).abs() < 1e-6);
}

#[test]
fn particles_expire_deterministically_and_reject_bad_inputs() {
    let mut world = VanillaParticleWorld::new();
    assert!(!world.spawn(
        VanillaParticleKind::Smoke,
        [f32::NAN; 3],
        [0.0; 3],
        [1.0; 4],
        1.0
    ));
    assert!(!world.spawn(
        VanillaParticleKind::Smoke,
        [0.0; 3],
        [f32::INFINITY; 3],
        [1.0; 4],
        1.0
    ));
    assert!(!world.spawn(
        VanillaParticleKind::Smoke,
        [0.0; 3],
        [0.0; 3],
        [1.0; 4],
        f32::INFINITY
    ));
    assert!(!world.spawn(
        VanillaParticleKind::Smoke,
        [0.0; 3],
        [0.0; 3],
        [1.0; 4],
        -1.0
    ));
    assert!(world.is_empty());

    world.spawn(
        VanillaParticleKind::Smoke,
        [0.0; 3],
        [1.0; 3],
        [f32::NAN, 2.0, -1.0, f32::INFINITY],
        0.5,
    );
    let original = world.clone();
    for dt in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -1.0, 0.0] {
        world.step(dt);
        assert_eq!(world, original);
    }
    assert_eq!(
        world.particles().front().unwrap().color,
        [0.0, 1.0, 0.0, 1.0]
    );
    world.step(0.25);
    assert_eq!(world.len(), 1);
    world.step(0.25);
    assert!(world.is_empty());
    world.spawn(
        VanillaParticleKind::Explosion,
        [0.0; 3],
        [0.0; 3],
        [1.0; 4],
        99.0,
    );
    assert_eq!(world.particles().front().unwrap().max_age_secs, 99.0);
    world.step(60.0);
    assert_eq!(world.len(), 1);
    world.step(39.0);
    assert!(world.is_empty());
}
