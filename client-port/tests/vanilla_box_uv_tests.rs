// SPDX-License-Identifier: LGPL-3.0-or-later

use megu_client_mesh::vanilla_plus_3d::{vanilla_box_face_uvs, BoxUv, ExtrudedMesh};
use megu_client_mesh::Direction;

fn px(uvs: [[[f32; 2]; 4]; 6], size: [u16; 2]) -> [[[u16; 2]; 4]; 6] {
    uvs.map(|face| {
        face.map(|uv| {
            [
                (uv[0] * f32::from(size[0])).round() as u16,
                (uv[1] * f32::from(size[1])).round() as u16,
            ]
        })
    })
}

fn box_uv([u, v]: [u16; 2], [tex_width, tex_height]: [u16; 2]) -> BoxUv {
    BoxUv::new(u, v, tex_width, tex_height)
}

fn assert_box(size: [f32; 3], tex_off: [u16; 2], mirror: bool, expected: [[[u16; 2]; 4]; 6]) {
    let tex_size = [64, 64];
    let uvs = vanilla_box_face_uvs(size, box_uv(tex_off, tex_size).with_mirror(mirror));
    assert_eq!(
        px(uvs, tex_size),
        expected,
        "size {size:?}, tex_off {tex_off:?}, mirror {mirror}"
    );
}

#[test]
fn steve_zombie_head_exact_vanilla_texels() {
    assert_box(
        [8.0, 8.0, 8.0],
        [0, 0],
        false,
        [
            // Down, Up, North, South, West, East. These are physical
            // renderer-space faces: Vanilla entity rendering flips authored
            // model-space X and Y, so caps and east/west strips exchange.
            [[24, 0], [24, 8], [16, 8], [16, 0]],
            [[16, 8], [16, 0], [8, 0], [8, 8]],
            [[8, 16], [16, 16], [16, 8], [8, 8]],
            [[24, 16], [32, 16], [32, 8], [24, 8]],
            [[16, 16], [24, 16], [24, 8], [16, 8]],
            [[0, 16], [8, 16], [8, 8], [0, 8]],
        ],
    );
}

#[test]
fn steve_zombie_body_exact_vanilla_texels() {
    assert_box(
        [8.0, 12.0, 4.0],
        [16, 16],
        false,
        [
            [[36, 16], [36, 20], [28, 20], [28, 16]],
            [[28, 20], [28, 16], [20, 16], [20, 20]],
            [[20, 32], [28, 32], [28, 20], [20, 20]],
            [[32, 32], [40, 32], [40, 20], [32, 20]],
            [[28, 32], [32, 32], [32, 20], [28, 20]],
            [[16, 32], [20, 32], [20, 20], [16, 20]],
        ],
    );
}

fn leg_expected(mirror: bool) -> [[[u16; 2]; 4]; 6] {
    if mirror {
        [
            [[8, 16], [8, 20], [12, 20], [12, 16]],
            [[4, 20], [4, 16], [8, 16], [8, 20]],
            [[8, 32], [4, 32], [4, 20], [8, 20]],
            [[16, 32], [12, 32], [12, 20], [16, 20]],
            [[4, 32], [0, 32], [0, 20], [4, 20]],
            [[12, 32], [8, 32], [8, 20], [12, 20]],
        ]
    } else {
        [
            [[12, 16], [12, 20], [8, 20], [8, 16]],
            [[8, 20], [8, 16], [4, 16], [4, 20]],
            [[4, 32], [8, 32], [8, 20], [4, 20]],
            [[12, 32], [16, 32], [16, 20], [12, 20]],
            [[8, 32], [12, 32], [12, 20], [8, 20]],
            [[0, 32], [4, 32], [4, 20], [0, 20]],
        ]
    }
}

#[test]
fn right_leg_unmirrored_and_left_leg_mirrored_exact_vanilla_texels() {
    assert_box([4.0, 12.0, 4.0], [0, 16], false, leg_expected(false));
    assert_box([4.0, 12.0, 4.0], [0, 16], true, leg_expected(true));
}

#[test]
fn mirror_swaps_west_east_strips_and_horizontally_flips_all_faces() {
    let unmirrored = vanilla_box_face_uvs(
        [4.0, 12.0, 4.0],
        box_uv([0, 16], [64, 64]),
    );
    let mirrored = vanilla_box_face_uvs(
        [4.0, 12.0, 4.0],
        box_uv([0, 16], [64, 64]).mirrored(),
    );
    let flip_x = |uv: [f32; 2], left: f32, right: f32| [left + right - uv[0], uv[1]];

    for direction in [
        Direction::Down,
        Direction::Up,
        Direction::North,
        Direction::South,
    ] {
        let index = direction as usize;
        let (left, right) = (
            unmirrored[index]
                .iter()
                .map(|uv| uv[0])
                .fold(f32::INFINITY, f32::min),
            unmirrored[index]
                .iter()
                .map(|uv| uv[0])
                .fold(f32::NEG_INFINITY, f32::max),
        );
        assert_eq!(
            mirrored[index],
            unmirrored[index].map(|uv| flip_x(uv, left, right)),
            "horizontal flip mismatch on {direction:?}"
        );
    }

    for (mirrored_face, unmirrored_face) in [
        (Direction::West as usize, Direction::East as usize),
        (Direction::East as usize, Direction::West as usize),
    ] {
        let (left, right) = (
            unmirrored[unmirrored_face]
                .iter()
                .map(|uv| uv[0])
                .fold(f32::INFINITY, f32::min),
            unmirrored[unmirrored_face]
                .iter()
                .map(|uv| uv[0])
                .fold(f32::NEG_INFINITY, f32::max),
        );
        assert_eq!(
            mirrored[mirrored_face],
            unmirrored[unmirrored_face].map(|uv| flip_x(uv, left, right)),
            "mirrored side strip mismatch"
        );
    }
}

#[test]
fn creeper_uses_64_by_32_sheet_exact_vanilla_texels() {
    let size = [64, 32];
    let head = vanilla_box_face_uvs([8.0, 8.0, 8.0], box_uv([0, 0], size));
    assert_eq!(
        px(head, size)[Direction::North as usize],
        [[8, 16], [16, 16], [16, 8], [8, 8]]
    );

    let body = vanilla_box_face_uvs([4.0, 12.0, 4.0], box_uv([16, 16], size));
    assert_eq!(
        px(body, size),
        [
            [[28, 16], [28, 20], [24, 20], [24, 16]],
            [[24, 20], [24, 16], [20, 16], [20, 20]],
            [[20, 32], [24, 32], [24, 20], [20, 20]],
            [[28, 32], [32, 32], [32, 20], [28, 20]],
            [[24, 32], [28, 32], [28, 20], [24, 20]],
            [[16, 32], [20, 32], [20, 20], [16, 20]],
        ]
    );

    let leg = vanilla_box_face_uvs([4.0, 6.0, 4.0], box_uv([0, 16], size));
    assert_eq!(
        px(leg, size)[Direction::North as usize],
        [[4, 26], [8, 26], [8, 20], [4, 20]]
    );
}

#[test]
fn inflate_grows_positions_without_changing_uv_rectangles() {
    let uv = box_uv([16, 16], [64, 64]);
    let size = [0.5, 0.75, 0.25];
    let plain_uvs = vanilla_box_face_uvs([8.0, 12.0, 4.0], uv);
    let mut plain = ExtrudedMesh::new();
    let mut inflated = ExtrudedMesh::new();
    plain.push_vanilla_box([0.0, 0.0, 0.0], size, 0.0, uv, -1);
    inflated.push_vanilla_box([0.0, 0.0, 0.0], size, 0.05, uv, -1);

    assert_eq!(plain.quads.len(), 6);
    assert_eq!(inflated.quads.len(), 6);
    for (plain_quad, inflated_quad) in plain.quads.iter().zip(&inflated.quads) {
        assert_eq!(plain_quad.uv, inflated_quad.uv);
    }
    assert_eq!(
        inflated.quads[Direction::Up as usize].uv,
        plain_uvs[Direction::Up as usize]
    );

    let plain_north = plain.quads[Direction::North as usize].positions;
    let north = inflated.quads[Direction::North as usize].positions;
    assert!((north[0][0] - plain_north[0][0] - 0.05).abs() < 1e-6);
    assert!((plain_north[0][1] - north[0][1] - 0.05).abs() < 1e-6);
    assert!((plain_north[0][2] - north[0][2] - 0.05).abs() < 1e-6);
}

#[test]
fn mesh_builder_assigns_box_uvs_to_matching_direction_quads() {
    let uv = box_uv([0, 16], [64, 64]);
    let expected = vanilla_box_face_uvs([4.0, 12.0, 4.0], uv);
    let mut mesh = ExtrudedMesh::new();
    mesh.push_vanilla_box([0.0, 0.0, 0.0], [0.25, 0.75, 0.25], 0.0, uv, -1);
    assert_eq!(mesh.quads.len(), 6);
    for direction in [
        Direction::Down,
        Direction::Up,
        Direction::North,
        Direction::South,
        Direction::West,
        Direction::East,
    ] {
        assert_eq!(mesh.quads[direction as usize].uv, expected[direction as usize]);
    }

    // A physical east leg face uses the authored west strip (u 0..4): bottom
    // corners carry the bottom edge of that strip in left-to-right order.
    assert_eq!(
        mesh.quads[Direction::East as usize].uv[0],
        [0.0, 32.0 / 64.0]
    );
}

#[test]
fn invalid_input_fails_closed_without_panicking() {
    assert_eq!(
        vanilla_box_face_uvs([8.0, 8.0, 8.0], box_uv([0, 0], [0, 64])),
        [[[0.0, 0.0]; 4]; 6]
    );
    assert_eq!(
        vanilla_box_face_uvs([f32::NAN, 8.0, 8.0], box_uv([0, 0], [64, 64])),
        [[[0.0, 0.0]; 4]; 6]
    );
    let mut mesh = ExtrudedMesh::new();
    mesh.push_vanilla_box(
        [0.0; 3],
        [0.5, 0.5, 0.5],
        0.0,
        box_uv([0, 0], [64, 0]),
        -1,
    );
    assert_eq!(mesh.quads.len(), 6);
    assert!(mesh
        .quads
        .iter()
        .all(|quad| quad.uv.iter().all(|uv| *uv == [0.0, 0.0])));
}
