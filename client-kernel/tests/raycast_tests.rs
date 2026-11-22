// SPDX-License-Identifier: LGPL-3.0-or-later
use mc_spine_kernel::{
    raycast::RayBlockShapes, raycast_blocks, raycast_entities, Aabb, BlockHitFace, FluidClipMode,
};

fn bounds(min: [f64; 3], max: [f64; 3]) -> Aabb {
    Aabb { min, max }
}

fn solid(boxes: Vec<Aabb>) -> RayBlockShapes {
    RayBlockShapes {
        boxes,
        ..RayBlockShapes::default()
    }
}

fn cube() -> RayBlockShapes {
    solid(vec![bounds([0.0; 3], [1.0; 3])])
}

#[test]
fn six_cardinal_faces_and_exact_cursor_coordinates() {
    let cases = [
        (
            [0.0, 3.25, 4.75],
            [1.0, 0.0, 0.0],
            BlockHitFace::West,
            4,
            [2.0, 3.25, 4.75],
            [0.0, 0.25, 0.75],
        ),
        (
            [5.0, 3.25, 4.75],
            [-1.0, 0.0, 0.0],
            BlockHitFace::East,
            5,
            [3.0, 3.25, 4.75],
            [1.0, 0.25, 0.75],
        ),
        (
            [2.25, 0.0, 4.75],
            [0.0, 1.0, 0.0],
            BlockHitFace::Down,
            0,
            [2.25, 3.0, 4.75],
            [0.25, 0.0, 0.75],
        ),
        (
            [2.25, 6.0, 4.75],
            [0.0, -1.0, 0.0],
            BlockHitFace::Up,
            1,
            [2.25, 4.0, 4.75],
            [0.25, 1.0, 0.75],
        ),
        (
            [2.25, 3.75, 2.0],
            [0.0, 0.0, 1.0],
            BlockHitFace::North,
            2,
            [2.25, 3.75, 4.0],
            [0.25, 0.75, 0.0],
        ),
        (
            [2.25, 3.75, 7.0],
            [0.0, 0.0, -1.0],
            BlockHitFace::South,
            3,
            [2.25, 3.75, 5.0],
            [0.25, 0.75, 1.0],
        ),
    ];

    for (origin, direction, face, wire_face, expected_hit, expected_cursor) in cases {
        let hit = raycast_blocks(origin, direction, 6.0, FluidClipMode::None, |x, y, z| {
            if [x, y, z] == [2, 3, 4] {
                cube()
            } else {
                RayBlockShapes::default()
            }
        })
        .unwrap();
        assert_eq!(hit.block_pos, [2, 3, 4]);
        assert_eq!(hit.face, face);
        assert_eq!(hit.face as u8, wire_face);
        assert_eq!(hit.hit_pos, expected_hit);
        assert_eq!(hit.cursor_in_block, expected_cursor);
        assert!(!hit.inside);
    }
}

#[test]
fn slab_height_controls_hit_and_miss() {
    let slab = || solid(vec![bounds([0.0; 3], [1.0, 0.5, 1.0])]);
    let sample = |x, y, z| {
        if [x, y, z] == [0, 0, 0] {
            slab()
        } else {
            RayBlockShapes::default()
        }
    };
    assert_eq!(
        raycast_blocks(
            [-1.0, 0.75, 0.5],
            [1.0, 0.0, 0.0],
            4.5,
            FluidClipMode::None,
            sample,
        ),
        None
    );

    let hit = raycast_blocks(
        [-1.0, 0.25, 0.5],
        [1.0, 0.0, 0.0],
        4.5,
        FluidClipMode::None,
        sample,
    )
    .unwrap();
    assert_eq!(hit.face, BlockHitFace::West);
    assert_eq!(hit.hit_pos, [0.0, 0.25, 0.5]);
    assert_eq!(hit.cursor_in_block, [0.0, 0.25, 0.5]);
}

#[test]
fn diagonal_traverses_chunks_and_negative_voxels() {
    for target in [[-34, 1, -34], [34, 1, 34]] {
        let mut visits = 0usize;
        let hit = raycast_blocks(
            [-38.5, 1.25, -38.5],
            [1.0, 0.0, 1.0],
            110.0,
            FluidClipMode::None,
            |x, y, z| {
                visits += 1;
                if [x, y, z] == target {
                    cube()
                } else {
                    RayBlockShapes::default()
                }
            },
        )
        .unwrap();
        assert_eq!(hit.block_pos, target);
        assert!(!hit.inside);
        assert!(visits <= (110usize + 4) * 4);
        assert!(hit.distance < 110.0);
    }
}

#[test]
fn fluid_modes_select_source_flowing_or_stone() {
    let source = |x, y, z| match [x, y, z] {
        [0, 0, 0] => RayBlockShapes {
            is_fluid: true,
            is_fluid_source: true,
            ..RayBlockShapes::default()
        },
        [2, 0, 0] => cube(),
        _ => RayBlockShapes::default(),
    };
    let trace_source =
        |mode| raycast_blocks([-1.0, 0.4, 0.5], [1.0, 0.0, 0.0], 5.0, mode, source).unwrap();
    assert_eq!(trace_source(FluidClipMode::None).block_pos, [2, 0, 0]);
    let water = trace_source(FluidClipMode::SourceOnly);
    assert_eq!(water.block_pos, [0, 0, 0]);
    assert_eq!(water.face, BlockHitFace::West);
    assert_eq!(water.cursor_in_block, [0.0, 0.4, 0.5]);
    assert_eq!(trace_source(FluidClipMode::Any).block_pos, [0, 0, 0]);

    let flowing = |x, y, z| match [x, y, z] {
        [0, 0, 0] => RayBlockShapes {
            is_fluid: true,
            ..RayBlockShapes::default()
        },
        [2, 0, 0] => cube(),
        _ => RayBlockShapes::default(),
    };
    assert_eq!(
        raycast_blocks(
            [-1.0, 0.4, 0.5],
            [1.0, 0.0, 0.0],
            5.0,
            FluidClipMode::SourceOnly,
            flowing,
        )
        .unwrap()
        .block_pos,
        [2, 0, 0]
    );
    assert_eq!(
        raycast_blocks(
            [-1.0, 0.4, 0.5],
            [1.0, 0.0, 0.0],
            5.0,
            FluidClipMode::Any,
            flowing,
        )
        .unwrap()
        .block_pos,
        [0, 0, 0]
    );
}

#[test]
fn inside_shape_and_multiple_boxes_pick_closest() {
    let hit = raycast_blocks(
        [0.25, 0.25, 0.25],
        [2.0, 0.0, 0.0],
        4.5,
        FluidClipMode::None,
        |x, y, z| {
            if [x, y, z] == [0, 0, 0] {
                cube()
            } else {
                RayBlockShapes::default()
            }
        },
    )
    .unwrap();
    assert!(hit.inside);
    assert_eq!(hit.distance, 0.0);
    assert_eq!(hit.face, BlockHitFace::West);

    let hit = raycast_blocks(
        [-1.0, 0.25, 0.25],
        [1.0, 0.0, 0.0],
        4.5,
        FluidClipMode::None,
        |x, y, z| {
            if [x, y, z] == [0, 0, 0] {
                solid(vec![
                    bounds([0.75, 0.0, 0.0], [1.0, 0.5, 1.0]),
                    bounds([0.25, 0.0, 0.0], [0.5, 0.5, 1.0]),
                ])
            } else {
                RayBlockShapes::default()
            }
        },
    )
    .unwrap();
    assert_eq!(hit.distance, 1.25);
    assert_eq!(hit.cursor_in_block, [0.25, 0.25, 0.25]);
}

#[test]
fn negative_boundary_and_reach_endpoint_are_deterministic() {
    let hit = raycast_blocks(
        [0.0, 0.5, 0.5],
        [-1.0, 0.0, 0.0],
        4.5,
        FluidClipMode::None,
        |x, y, z| {
            if [x, y, z] == [-1, 0, 0] {
                cube()
            } else {
                RayBlockShapes::default()
            }
        },
    )
    .unwrap();
    assert_eq!(hit.block_pos, [-1, 0, 0]);
    assert_eq!(hit.face, BlockHitFace::East);
    assert_eq!(hit.distance, 0.0);
    assert!(!hit.inside);

    let block = raycast_blocks(
        [0.0, 0.5, 0.5],
        [1.0, 0.0, 0.0],
        4.0,
        FluidClipMode::None,
        |x, y, z| {
            if [x, y, z] == [4, 0, 0] {
                cube()
            } else {
                RayBlockShapes::default()
            }
        },
    )
    .unwrap();
    assert_eq!(block.distance, 4.0);
    assert_eq!(block.block_pos, [4, 0, 0]);

    // Entity reach is strictly less than the supplied maximum.
    let target = [(12, bounds([4.0, 0.0, 0.0], [5.0, 1.0, 1.0]))];
    assert!(raycast_entities([0.0, 0.5, 0.5], [1.0, 0.0, 0.0], 4.0, &target).is_none());
    assert!(raycast_entities([0.0, 0.5, 0.5], [1.0, 0.0, 0.0], 4.01, &target).is_some());
}

#[test]
fn entities_nearest_occluded_tied_and_inside() {
    let entities = [
        (9, bounds([4.0, 0.0, 0.0], [5.0, 1.0, 1.0])),
        (20, bounds([2.0, 0.0, 0.0], [3.0, 1.0, 1.0])),
        (3, bounds([2.0, 0.0, 0.0], [3.0, 1.0, 1.0])),
    ];
    let origin = [0.0, 0.25, 0.25];
    let direction = [10.0, 0.0, 0.0];
    assert_eq!(
        raycast_entities(origin, direction, 10.0, &entities)
            .unwrap()
            .entity_id,
        3
    );
    assert_eq!(
        raycast_entities(origin, direction, 10.0, &entities)
            .unwrap()
            .hit_pos,
        [2.0, 0.25, 0.25]
    );
    assert!(raycast_entities(origin, direction, 2.0, &entities).is_none());
    assert!(raycast_entities(origin, direction, 1.99, &entities).is_none());
    assert_eq!(
        raycast_entities(origin, direction, 2.01, &entities)
            .unwrap()
            .entity_id,
        3
    );

    let inside = raycast_entities([2.5, 0.5, 0.5], direction, 3.0, &entities).unwrap();
    assert_eq!(inside.distance, 0.0);
    assert_eq!(inside.entity_id, 3);
}

#[test]
fn invalid_rays_and_boxes_fail_closed() {
    let origin = [0.0, 0.0, 0.0];
    let direction = [1.0, 0.0, 0.0];
    for (from, dir, distance) in [
        ([f64::NAN, 0.0, 0.0], direction, 4.5),
        ([f64::INFINITY, 0.0, 0.0], direction, 4.5),
        (origin, [f64::NAN, 0.0, 0.0], 4.5),
        (origin, [f64::INFINITY, 0.0, 0.0], 4.5),
        (origin, [0.0; 3], 4.5),
        (origin, direction, 0.0),
        (origin, direction, -1.0),
        (origin, direction, f64::NAN),
        (origin, direction, f64::INFINITY),
    ] {
        assert!(
            raycast_blocks(from, dir, distance, FluidClipMode::Any, |_, _, _| {
                panic!("invalid ray must not invoke sampler")
            })
            .is_none()
        );
        assert!(
            raycast_entities(from, dir, distance, &[(1, bounds([0.0; 3], [1.0; 3]))]).is_none()
        );
    }

    assert!(raycast_blocks(
        [f64::from(i32::MAX) + 1.0, 0.0, 0.0],
        direction,
        4.5,
        FluidClipMode::None,
        |_, _, _| panic!("out-of-range coordinate must not invoke sampler"),
    )
    .is_none());
    assert!(raycast_entities(
        origin,
        direction,
        4.5,
        &[(1, bounds([f64::NAN, 0.0, 0.0], [1.0; 3]))]
    )
    .is_none());

    // Normalization must handle the full finite f64 range without overflow.
    let hit = raycast_entities(
        origin,
        [f64::MAX, f64::MAX, 0.0],
        4.5,
        &[(7, bounds([1.0, 1.0, -0.5], [2.0, 2.0, 0.5]))],
    )
    .unwrap();
    assert_eq!(hit.entity_id, 7);
    assert!((hit.distance - 2f64.sqrt()).abs() < 1e-12);
}
