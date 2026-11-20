// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

use std::{
    mem::{offset_of, size_of, MaybeUninit},
    ptr,
};

use mc_spine_client_mesh::{ffi::*, *};

#[test]
fn ffi_output_matches_rust_with_both_border_kinds_and_light_channels() {
    let mut section = Section::filled(Block::AIR, Light::new(0, 15));
    section.set(15, 15, 2, Block::opaque_cube(26_683), Light::new(10, 0));
    let mut top = [Sample::new(Block::AIR, Light::new(2, 9)); FACE_CELLS];
    top[16 + 15] = Sample::new(Block::AIR, Light::new(8, 12));
    let east = [Sample::new(Block::opaque_cube(1), Light::new(0, 0)); FACE_CELLS];
    let mut halo = [Sample::new(Block::AIR, Light::new(0, 15)); EDGE_CORNER_CELLS];
    halo[edge_corner_index([16, 16, 1]).unwrap()] =
        Sample::new(Block::opaque_cube(1), Light::new(0, 0));
    let mut view = section.view();
    view.borders[Direction::Up as usize] = Some(&top);
    view.borders[Direction::East as usize] = Some(&east);
    view.edges_and_corners = Some(&halo);
    let expected = mesh_section(&view);
    assert_eq!(expected.len(), 5);
    let top = expected
        .iter()
        .find(|q| q.direction == Direction::Up)
        .unwrap();
    assert_eq!(top.face_light, Light::new(10, 9));
    assert_eq!(top.ao, [3, 3, 3, 2]);

    let input = MeshInput {
        abi_version: mc_spine_mesh_abi_version(),
        missing: ABI_AIR,
        blocks: view.blocks.as_ptr(),
        block_light: view.block_light.as_ptr(),
        sky_light: view.sky_light.as_ptr(),
        borders: view
            .borders
            .map(|border| border.map_or(ptr::null(), |b| b.as_ptr())),
        edges_and_corners: halo.as_ptr(),
    };
    let mut count = 0;
    // SAFETY: input references live arrays, and the count slot is disjoint.
    assert_eq!(
        unsafe { mc_spine_mesh_section(&input, ptr::null_mut(), 0, &mut count) },
        ABI_OK
    );
    assert_eq!(count, expected.len());

    let mut too_small = [expected[0]];
    // SAFETY: output is writable for the supplied capacity.
    assert_eq!(
        unsafe { mc_spine_mesh_section(&input, too_small.as_mut_ptr(), 1, &mut count) },
        ABI_BUFFER_TOO_SMALL
    );
    assert_eq!(count, expected.len());
    assert_eq!(too_small, [expected[0]]);

    let mut out = vec![MaybeUninit::<Quad>::uninit(); count];
    // SAFETY: the allocation has count writable slots, and input is unchanged.
    assert_eq!(
        unsafe { mc_spine_mesh_section(&input, out.as_mut_ptr().cast(), out.len(), &mut count) },
        ABI_OK
    );
    assert_eq!(count, expected.len());
    // SAFETY: the successful call initialized the returned prefix.
    let out: Vec<_> = out
        .into_iter()
        .take(count)
        .map(|q| unsafe { q.assume_init() })
        .collect();
    assert_eq!(out, expected);
}

#[test]
fn ffi_rejects_invalid_scalars_and_missing_required_buffers() {
    let section = Section::filled(Block::AIR, Light::new(0, 15));
    let valid = MeshInput {
        abi_version: ABI_VERSION,
        missing: ABI_OPAQUE,
        blocks: section.blocks.as_ptr(),
        block_light: section.block_light.as_ptr(),
        sky_light: section.sky_light.as_ptr(),
        borders: [ptr::null(); 6],
        edges_and_corners: ptr::null(),
    };
    for input in [
        MeshInput {
            abi_version: ABI_VERSION + 1,
            ..valid
        },
        MeshInput {
            missing: 2,
            ..valid
        },
        MeshInput {
            blocks: ptr::null(),
            ..valid
        },
        MeshInput {
            block_light: ptr::null(),
            ..valid
        },
        MeshInput {
            sky_light: ptr::null(),
            ..valid
        },
    ] {
        let mut count = 99;
        // SAFETY: all non-null input pointers are valid; count is writable.
        assert_eq!(
            unsafe { mc_spine_mesh_section(&input, ptr::null_mut(), 0, &mut count) },
            ABI_INVALID_ARGUMENT
        );
        assert_eq!(count, 0);
    }
    let mut count = 99;
    // SAFETY: input arrays and the count slot are live. Null output is rejected.
    assert_eq!(
        unsafe { mc_spine_mesh_section(&valid, ptr::null_mut(), 1, &mut count) },
        ABI_INVALID_ARGUMENT
    );
    assert_eq!(count, 0);
    // SAFETY: null input is rejected before dereference.
    assert_eq!(
        unsafe { mc_spine_mesh_section(ptr::null(), ptr::null_mut(), 0, &mut count) },
        ABI_INVALID_ARGUMENT
    );
    // SAFETY: count is rejected before dereference.
    assert_eq!(
        unsafe { mc_spine_mesh_section(&valid, ptr::null_mut(), 0, ptr::null_mut()) },
        ABI_INVALID_ARGUMENT
    );
    // SAFETY: valid empty input and a writable count.
    assert_eq!(
        unsafe { mc_spine_mesh_section(&valid, ptr::null_mut(), 0, &mut count) },
        ABI_OK
    );
    assert_eq!(count, 0);
}

#[test]
fn ffi_types_use_the_header_layout() {
    assert_eq!(size_of::<Block>(), 16);
    assert_eq!(offset_of!(Block, fluid_tint), 8);
    assert_eq!(offset_of!(Block, flags), 12);
    assert_eq!(size_of::<Sample>(), 20);
    assert_eq!(offset_of!(Sample, light), 16);
    assert_eq!(size_of::<Quad>(), 84);
    assert_eq!(offset_of!(Quad, state_id), 48);
    assert_eq!(offset_of!(Quad, flow), 56);
    assert_eq!(offset_of!(Quad, ao), 64);
    assert_eq!(offset_of!(Quad, block_light), 68);
    assert_eq!(offset_of!(Quad, sky_light), 72);
    assert_eq!(offset_of!(Quad, face_light), 76);
    assert_eq!(offset_of!(Quad, diagonal), 79);
    assert_eq!(offset_of!(Quad, flags), 81);
    assert_eq!(offset_of!(MeshInput, blocks), 8);
    assert_eq!(offset_of!(MeshInput, borders), 8 + 3 * size_of::<usize>());
    assert_eq!(
        offset_of!(MeshInput, edges_and_corners),
        8 + 9 * size_of::<usize>()
    );
    assert_eq!(size_of::<MeshInput>(), 8 + 10 * size_of::<usize>());
}

#[test]
fn column_skips_air_and_packs_the_maximum_height_endpoint() {
    let solid = Section::filled(Block::opaque_cube(u32::MAX), Light::new(0, 0));
    let air = Section::filled(Block::AIR, Light::new(0, 15));
    let sections: [_; ABI_MAX_COLUMN_SECTIONS] = std::array::from_fn(|i| {
        if i == ABI_MAX_COLUMN_SECTIONS - 1 {
            MeshColumnSection {
                flags: 0,
                blocks: solid.blocks.as_ptr(),
                block_light: solid.block_light.as_ptr(),
                sky_light: solid.sky_light.as_ptr(),
            }
        } else {
            MeshColumnSection {
                flags: ABI_COLUMN_UNIFORM_AIR,
                blocks: ptr::null(),
                block_light: air.block_light.as_ptr(),
                sky_light: air.sky_light.as_ptr(),
            }
        }
    });
    let mut input = MeshColumnInput {
        abi_version: ABI_VERSION,
        missing: ABI_AIR,
        sections: sections.as_ptr(),
        section_count: sections.len(),
        borders: [ptr::null(); 4],
    };
    let mut count = 99;
    let mut quads = mesh_section(&solid.view());
    assert_eq!(quads.len(), 6);
    let mut output = [PackedVertex::default(); 24];
    // SAFETY: all non-null buffers are live, sized as supplied and disjoint.
    unsafe {
        for invalid_count in [0, ABI_MAX_COLUMN_SECTIONS + 1] {
            input.section_count = invalid_count;
            assert_eq!(
                mc_spine_client_mesh_column(&input, ptr::null_mut(), 0, &mut count),
                ABI_INVALID_ARGUMENT
            );
            assert_eq!(count, 0);
        }
        input.section_count = 24;
        assert_eq!(
            mc_spine_client_mesh_column(&input, ptr::null_mut(), 0, &mut count),
            ABI_OK
        );
        assert_eq!(count, 0);
        input.section_count = sections.len();
        assert_eq!(
            mc_spine_client_mesh_column(&input, quads.as_mut_ptr(), quads.len(), &mut count),
            ABI_OK
        );
        assert_eq!(count, 6);
        assert_eq!(quads[1].positions.map(|p| p[1]), [512.0; 4]);

        assert_eq!(
            mc_spine_client_mesh_pack_vertices(
                quads.as_ptr(),
                quads.len(),
                output.as_mut_ptr(),
                23,
                &mut count
            ),
            ABI_BUFFER_TOO_SMALL
        );
        assert_eq!(count, 24);
        assert_eq!(output, [PackedVertex::default(); 24]);
        assert_eq!(
            mc_spine_client_mesh_pack_vertices(
                quads.as_ptr(),
                quads.len(),
                output.as_mut_ptr(),
                output.len(),
                &mut count
            ),
            ABI_OK
        );
    }
    assert_eq!(count, 24);
    let first_top = output[4];
    assert_eq!(first_top.position, (512 * 1024) << 10);
    assert_eq!(first_top.state_id, u32::MAX);
    assert_eq!(
        first_top.attributes,
        Direction::Up as u32 | (3 << 3) | (15 << 9) | (16 << 20) | (16 << 25)
    );
    assert_eq!(output[6].position & 1023, 16 | (16 << 5));
    assert_eq!(output[6].attributes >> 30, 2);
}

#[test]
fn translucent_sort_uses_camera_distance_and_preserves_index_order_on_ties() {
    let mut section = Section::filled(Block::AIR, Light::new(0, 15));
    section.set(1, 4, 1, Block::opaque_cube(1), Light::new(0, 0));
    let opaque = mesh_section(&section.view())[0];
    let mut glass = opaque;
    glass.layer = Layer::Translucent;
    glass.state_id = 519;
    let mut far = glass;
    far.positions = glass.positions.map(|[x, y, z]| [x + 10.0, y, z]);
    let mut water = far;
    water.layer = Layer::Fluid;
    water.fluid_kind = WATER;
    water.state_id = 80;
    let mut lava = water;
    lava.fluid_kind = LAVA;
    let quads = [glass, water, opaque, far, lava];
    let mut output = [u32::MAX; 4];
    let mut count = 99;
    // SAFETY: all non-null buffers are live, sized as supplied and disjoint.
    unsafe {
        for bad in [f32::NAN, f32::NEG_INFINITY, f32::INFINITY] {
            assert_eq!(
                mc_spine_client_mesh_sort_translucent(
                    quads.as_ptr(),
                    quads.len(),
                    0.0,
                    bad,
                    0.0,
                    output.as_mut_ptr(),
                    output.len(),
                    &mut count
                ),
                ABI_INVALID_ARGUMENT
            );
            assert_eq!(count, 0);
        }
        assert_eq!(
            mc_spine_client_mesh_sort_translucent(
                quads.as_ptr(),
                quads.len(),
                0.0,
                0.0,
                0.0,
                output.as_mut_ptr(),
                2,
                &mut count
            ),
            ABI_BUFFER_TOO_SMALL
        );
        assert_eq!(count, 3);
        assert_eq!(output, [u32::MAX; 4]);
        for (camera_x, expected) in [(0.0, [1, 3, 0]), (16.0, [0, 1, 3]), (f32::MAX, [0, 1, 3])] {
            assert_eq!(
                mc_spine_client_mesh_sort_translucent(
                    quads.as_ptr(),
                    quads.len(),
                    camera_x,
                    0.0,
                    0.0,
                    output.as_mut_ptr(),
                    output.len(),
                    &mut count
                ),
                ABI_OK
            );
            assert_eq!(count, 3);
            assert_eq!(output[..count], expected);
            assert_eq!(output[count], u32::MAX);
        }
    }
}

#[test]
fn packed_fluid_vertices_preserve_fractional_heights_and_material() {
    let mut section = Section::filled(Block::AIR, Light::new(0, 15));
    let water = Block {
        state_id: 88,
        fluid_kind: WATER,
        fluid_level: 8,
        flags: FLUID_TINTED,
        fluid_tint: 0x113355,
        ..Block::AIR
    };
    section.set(2, 7, 5, water, Light::new(5, 7));
    let quads = mesh_section(&section.view());
    let top = quads
        .iter()
        .find(|quad| quad.direction == Direction::Up)
        .unwrap();
    let mut output = [PackedVertex::default(); 4];
    let mut count = 0;
    // SAFETY: one valid input and four disjoint writable vertex slots.
    assert_eq!(
        unsafe {
            mc_spine_client_mesh_pack_vertices(
                top,
                1,
                output.as_mut_ptr(),
                output.len(),
                &mut count,
            )
        },
        ABI_OK
    );
    assert_eq!(count, 4);
    for (i, vertex) in output.into_iter().enumerate() {
        let [x, y, z] = top.positions[i];
        assert!(y.fract() > 0.0);
        assert_eq!(vertex.position & 31, x as u32);
        assert_eq!((vertex.position >> 5) & 31, z as u32);
        assert!(((vertex.position >> 10) as f32 / 1024.0 - y).abs() <= 0.5 / 1024.0);
        assert_eq!(vertex.state_id, water.state_id);
        assert_eq!((vertex.attributes >> 15) & 3, u32::from(WATER));
        assert_eq!(
            (vertex.attributes >> 18) & 3,
            u32::from(QUAD_TINTED | QUAD_FALLING)
        );
        assert_eq!(vertex.tint, water.fluid_tint);
    }
}
