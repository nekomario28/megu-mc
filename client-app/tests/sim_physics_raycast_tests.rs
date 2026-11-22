// SPDX-License-Identifier: LGPL-3.0-or-later
//! Focused headless-client integration coverage for the shared 1.21.1 kernels.

use mc_spine_client::protocol::{
    DecodedChunk, DecodedLightData, DecodedSection, DimensionMeta, TeleportSync,
};
use mc_spine_client::sim::{PlayerPhysics, TickInput};
use mc_spine_client::world::{BlockTable, WorldState};
use mc_spine_client_mesh::section_index;
use mc_spine_kernel::{BlockHitFace, FluidClipMode};

fn world_with_blocks(blocks: impl IntoIterator<Item = ([usize; 3], u32)>) -> WorldState {
    let dimension = DimensionMeta {
        min_y: 0,
        height: 16,
        has_skylight: false,
        ..DimensionMeta::default()
    };
    let mut states = Box::new([0u32; 4096]);
    let mut non_air = 0;
    for ([x, y, z], state) in blocks {
        assert!(x < 16 && y < 16 && z < 16);
        let index = section_index(x, y, z);
        if states[index] == 0 && state != 0 {
            non_air += 1;
        }
        states[index] = state;
    }
    let mut world = WorldState::new(dimension, BlockTable::builtin());
    world
        .apply_chunk(DecodedChunk {
            cx: 0,
            cz: 0,
            sections: vec![DecodedSection {
                non_air,
                states,
                biomes: [0; 64],
            }],
            light: DecodedLightData {
                sky_mask: vec![],
                block_mask: vec![],
                empty_sky_mask: vec![],
                empty_block_mask: vec![],
                sky_arrays: vec![],
                block_arrays: vec![],
            },
            block_entities: vec![],
        })
        .unwrap();
    world
}

fn flat_world() -> WorldState {
    world_with_blocks((0..16).flat_map(|x| (0..16).map(move |z| ([x, 0, z], 1))))
}

#[test]
fn sneaking_stops_at_a_cliff_without_falling() {
    let world = world_with_blocks((0..6).flat_map(|x| (0..16).map(move |z| ([x, 0, z], 1))));
    let mut player = PlayerPhysics::new([5.25, 1.0, 7.5], -90.0, 0.0);
    player.on_ground = true;
    for _ in 0..100 {
        player
            .step(
                &world,
                TickInput {
                    forward: 1.0,
                    sneak: true,
                    ..TickInput::default()
                },
            )
            .unwrap();
        assert!(player.on_ground, "fell off at x={}", player.pos[0]);
        assert!((player.pos[1] - 1.0).abs() < 1e-8);
    }
    assert!(player.pos[0] > 5.25);
    assert!(player.pos[0] <= 6.31, "sneak edge x={}", player.pos[0]);
}

#[test]
fn jump_apex_matches_vanilla_1211() {
    let world = flat_world();
    let mut player = PlayerPhysics::new([7.5, 1.0, 7.5], 0.0, 0.0);
    player.on_ground = true;
    let mut apex: f64 = 1.0;
    let mut landed = false;
    for tick in 0..40 {
        player
            .step(
                &world,
                TickInput {
                    jump: tick == 0,
                    ..TickInput::default()
                },
            )
            .unwrap();
        apex = apex.max(player.pos[1]);
        if tick > 1 && player.on_ground {
            landed = true;
            break;
        }
    }
    assert!((apex - 1.0 - 1.2522).abs() < 0.0002, "apex={apex}");
    assert!(landed);
}

#[test]
fn immersed_water_jumping_has_upward_buoyancy() {
    let mut blocks: Vec<_> = (0..16)
        .flat_map(|x| (0..16).map(move |z| ([x, 0, z], 1)))
        .collect();
    for x in 3..=5 {
        for y in 1..=4 {
            for z in 3..=5 {
                blocks.push(([x, y, z], 80));
            }
        }
    }
    let world = world_with_blocks(blocks);
    let mut player = PlayerPhysics::new([4.5, 2.0, 4.5], 0.0, 0.0);
    let outcome = player
        .step(
            &world,
            TickInput {
                jump: true,
                ..TickInput::default()
            },
        )
        .unwrap();
    assert!(outcome.in_fluid && player.in_water);
    assert!(player.pos[1] > 2.0, "water ascent={}", player.pos[1]);
    assert!(player.vel[1] > 0.0);
}

#[test]
fn sprinting_uses_kernel_movement_multiplier() {
    let world = flat_world();
    let mut walking = PlayerPhysics::new([5.5, 1.0, 3.5], 0.0, 0.0);
    let mut sprinting = walking.clone();
    walking.on_ground = true;
    sprinting.on_ground = true;
    for _ in 0..20 {
        walking
            .step(
                &world,
                TickInput {
                    forward: 1.0,
                    ..TickInput::default()
                },
            )
            .unwrap();
        sprinting
            .step(
                &world,
                TickInput {
                    forward: 1.0,
                    sprint: true,
                    ..TickInput::default()
                },
            )
            .unwrap();
    }
    let ratio = (sprinting.pos[2] - 3.5) / (walking.pos[2] - 3.5);
    assert!((1.28..1.32).contains(&ratio), "sprint/walk={ratio}");
}

#[test]
fn legacy_forward_input_still_reaches_a_wall() {
    let mut blocks: Vec<_> = (0..16)
        .flat_map(|x| (0..16).map(move |z| ([x, 0, z], 1)))
        .collect();
    for x in 6..=10 {
        for y in 1..=2 {
            blocks.push(([x, y, 10], 1));
        }
    }
    let world = world_with_blocks(blocks);
    let mut player = PlayerPhysics::new([8.5, 1.0, 8.5], 0.0, 0.0);
    player.on_ground = true;

    let mut hit_wall = false;
    for _ in 0..20 {
        hit_wall |= player
            .step(
                &world,
                TickInput {
                    forward: 0.2,
                    ..TickInput::default()
                },
            )
            .unwrap()
            .horizontal_collision;
    }
    assert!(hit_wall, "legacy forward=0.2 never reached the wall");
    assert!(player.pos[2] <= 9.7 + 1e-6);
}

#[test]
fn relative_teleport_snaps_pose_and_clears_motion() {
    let mut player = PlayerPhysics::new([5.5, 1.0, 5.5], 10.0, 5.0);
    player.vel = [0.5, 0.7, -0.2];
    let distance = player.apply_teleport(&TeleportSync {
        x: 1.0,
        y: 2.0,
        z: -1.0,
        yaw: 35.0,
        pitch: -10.0,
        flags: 0x1f,
        teleport_id: 21,
    });
    assert!((distance - 6.0f64.sqrt()).abs() < 1e-10);
    assert_eq!(player.pos, [6.5, 3.0, 4.5]);
    assert_eq!(player.vel, [0.0; 3]);
    assert_eq!((player.yaw, player.pitch), (45.0, -5.0));

    let world = flat_world();
    let outcome = player.step(&world, TickInput::default()).unwrap();
    assert_eq!((outcome.yaw, outcome.pitch), (45.0, -5.0));
    assert_eq!(outcome.pos[0], 6.5);
    assert_eq!(outcome.pos[2], 4.5);
}

#[test]
fn picks_stone_face_and_cursor_with_dda_and_enforces_reach() {
    let world = world_with_blocks([([5, 2, 9], 1), ([5, 2, 12], 1)]);
    let player = PlayerPhysics::new([5.5, 1.0, 5.5], 0.0, 0.0);
    let hit = player
        .pick_target_block(&world, 4.5, FluidClipMode::None)
        .expect("stone is within reach");
    assert_eq!(hit.block_pos, [5, 2, 9]);
    assert_eq!(hit.face, BlockHitFace::North);
    assert_eq!(hit.hit_pos, [5.5, 2.62, 9.0]);
    assert_eq!(hit.cursor_in_block, [0.5, 0.62, 0.0]);
    assert!((hit.distance - 3.5).abs() < 1e-9);
    assert!(!hit.inside);

    assert!(player.pick_target_block(&world, 3.4, FluidClipMode::None).is_none());
    let far_world = world_with_blocks([([5, 2, 12], 1)]);
    assert!(player
        .pick_target_block(&far_world, 4.5, FluidClipMode::None)
        .is_none());
}

#[test]
fn raycast_respects_fluid_clip_mode() {
    let world = world_with_blocks([([5, 2, 9], 80)]);
    let player = PlayerPhysics::new([5.5, 1.0, 5.5], 0.0, 0.0);
    assert!(player.pick_target_block(&world, 4.5, FluidClipMode::None).is_none());
    let hit = player
        .pick_target_block(&world, 4.5, FluidClipMode::SourceOnly)
        .expect("water source is selectable");
    assert_eq!(hit.block_pos, [5, 2, 9]);
    assert_eq!(hit.face, BlockHitFace::North);
}
