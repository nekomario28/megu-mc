// SPDX-License-Identifier: LGPL-3.0-or-later

use std::mem::size_of;

use megu_client_mesh::ffi::{megu_client_mesh_pack_vertices, ABI_OK};
use megu_client_mesh::{
    mesh_section, pack_vertices_compact, try_pack_vertices_compact, Block, CompactVoxelVertex,
    Light, PackedVertex, Section, COMPACT_VERTEX_MAX_STATE_ID, COMPACT_VERTEX_WGSL_UNPACK, WATER,
};

#[allow(clippy::too_many_arguments)]
fn attributes(
    direction: u32,
    ao: u32,
    block_light: u32,
    sky_light: u32,
    layer: u32,
    fluid_kind: u32,
    diagonal: u32,
    flags: u32,
    width: u32,
    height: u32,
    corner: u32,
) -> u32 {
    direction
        | (ao << 3)
        | (block_light << 5)
        | (sky_light << 9)
        | (layer << 13)
        | (fluid_kind << 15)
        | (diagonal << 17)
        | (flags << 18)
        | (width << 20)
        | (height << 25)
        | (corner << 30)
}

fn vertex(x: u32, z: u32, y_fixed: u32, state_id: u32, attributes: u32, tint: u32) -> PackedVertex {
    PackedVertex {
        position: x | (z << 5) | (y_fixed << 10),
        state_id,
        attributes,
        tint,
    }
}

fn base_attributes() -> u32 {
    attributes(1, 3, 15, 15, 0, 0, 0, 0, 16, 16, 0)
}

fn assert_roundtrip(vertex: &PackedVertex) {
    assert!(CompactVoxelVertex::is_representable(vertex));
    let compact = CompactVoxelVertex::from_packed(vertex);
    assert_eq!(compact.to_packed(), *vertex);
}

#[test]
fn compact_vertex_is_strictly_smaller_and_reports_ratio() {
    assert_eq!(size_of::<PackedVertex>(), 16);
    assert_eq!(size_of::<CompactVoxelVertex>(), 12);
    assert!(size_of::<CompactVoxelVertex>() < size_of::<PackedVertex>());
    assert!((CompactVoxelVertex::compression_ratio() - (16.0 / 12.0)).abs() < f32::EPSILON);
    assert!(COMPACT_VERTEX_WGSL_UNPACK.contains("word0"));
    assert!(COMPACT_VERTEX_WGSL_UNPACK.contains("289u"));
    assert!(COMPACT_VERTEX_WGSL_UNPACK.contains("0x7fffffu"));
}

#[test]
fn bit_layout_matches_documented_words() {
    let packed = vertex(
        3,
        5,
        1024,
        12_345,
        attributes(5, 2, 7, 11, 1, 2, 1, 3, 9, 13, 2),
        0x123456,
    );
    let compact = CompactVoxelVertex::from_packed(&packed);

    let position_index = 3 + 17 * (5 + 17 * 1024);
    let compact_attributes =
        (packed.attributes & 0x000f_ffff) | ((9 - 1) << 20) | ((13 - 1) << 24) | (2 << 28);
    assert_eq!(compact.position_index(), position_index);
    assert_eq!(compact.compact_attributes(), compact_attributes);
    assert_eq!(compact.state_id(), 12_345);
    assert_eq!(compact.tint(), 0x123456);

    assert_eq!(compact.word0 & 0x7f_ffff, position_index);
    assert_eq!(compact.word0 >> 23, compact_attributes & 0x1ff);
    assert_eq!(
        compact.word1 & 0x1f_ffff,
        (compact_attributes >> 9) & 0x1f_ffff
    );
    assert_eq!(compact.word1 >> 21, 12_345 & 0x7ff);
    assert_eq!(compact.word2 & 0xff, 12_345 >> 11);
    assert_eq!(compact.word2 >> 8, 0x123456);
    assert_eq!(compact.to_packed(), packed);
}

#[test]
fn integer_block_corners_roundtrip() {
    for x in 0..=16 {
        for z in 0..=16 {
            for y in 0..=16 {
                let packed = vertex(
                    x,
                    z,
                    y * 1024,
                    26_683,
                    attributes(
                        (x + y + z) % 6,
                        (x + z) % 4,
                        (x + y) % 16,
                        (y + z) % 16,
                        (x + y) % 4,
                        (x + z) % 3,
                        (x + y + z) % 2,
                        (x + z) % 4,
                        1 + (x + y) % 16,
                        1 + (y + z) % 16,
                        (x + y + z) % 4,
                    ),
                    0x7cbd6b,
                );
                assert_roundtrip(&packed);
            }
        }
    }
}

#[test]
fn fractional_height_grid_roundtrip() {
    let cases: [(&str, f32); 7] = [
        ("slab", 0.5),
        ("snow", 0.125),
        ("carpet", 0.0625),
        ("fence", 0.375),
        ("sixteenth", 1.0 / 16.0),
        ("thirtysecond", 1.0 / 32.0),
        ("fine_y", 1.0 / 1024.0),
    ];
    for (name, y) in cases {
        let y_fixed = (y * 1024.0).round() as u32;
        let packed = vertex(7, 11, y_fixed, 42, base_attributes(), 0x336699);
        let compact = CompactVoxelVertex::from_packed(&packed);
        assert_eq!(compact.to_packed(), packed, "{name} y={y}");
        assert!(
            (compact.local_position()[1] - y).abs() < 0.000_001,
            "{name}"
        );
    }
}

#[test]
fn all_six_face_normals_roundtrip() {
    for direction in 0..6 {
        let packed = vertex(
            4,
            9,
            2048,
            100 + direction,
            attributes(direction, 1, 8, 9, 0, 0, 0, 0, 16, 16, direction % 4),
            0x102030,
        );
        assert_roundtrip(&packed);
        assert_eq!(
            CompactVoxelVertex::from_packed(&packed)
                .to_packed()
                .attributes
                & 0x7,
            direction
        );
    }
}

#[test]
fn all_four_ao_levels_roundtrip() {
    for ao in 0..4 {
        let packed = vertex(
            2,
            3,
            512,
            200 + ao,
            attributes(1, ao, 4, 5, 0, 0, 0, 0, 16, 16, ao),
            0x445566,
        );
        assert_roundtrip(&packed);
        assert_eq!(
            (CompactVoxelVertex::from_packed(&packed)
                .to_packed()
                .attributes
                >> 3)
                & 0x3,
            ao
        );
    }
}

#[test]
fn all_block_and_sky_light_levels_roundtrip() {
    for block_light in 0..16 {
        for sky_light in 0..16 {
            let packed = vertex(
                6,
                8,
                1536,
                300 + block_light * 16 + sky_light,
                attributes(2, 2, block_light, sky_light, 0, 0, 0, 0, 16, 16, 1),
                0x778899,
            );
            assert_roundtrip(&packed);
        }
    }
}

#[test]
fn atlas_uv_width_height_and_corner_roundtrip() {
    for width in 1..=16 {
        for height in 1..=16 {
            for corner in 0..4 {
                let packed = vertex(
                    5,
                    6,
                    1024,
                    400 + width * 16 + height * 4 + corner,
                    attributes(3, 3, 15, 15, 0, 0, 0, 0, width, height, corner),
                    0x99aabb,
                );
                let compact = CompactVoxelVertex::from_packed(&packed);
                assert_eq!(compact.to_packed(), packed);
                let restored = compact.to_packed().attributes;
                assert_eq!((restored >> 20) & 0x1f, width);
                assert_eq!((restored >> 25) & 0x1f, height);
                assert_eq!(restored >> 30, corner);
            }
        }
    }
}

#[test]
fn layer_fluid_diagonal_flags_and_state_boundaries_roundtrip() {
    for layer in 0..4 {
        for fluid_kind in 0..4 {
            for diagonal in 0..2 {
                for flags in 0..4 {
                    for state_id in [0, 1, 26_683, COMPACT_VERTEX_MAX_STATE_ID] {
                        let packed = vertex(
                            1,
                            2,
                            768,
                            state_id,
                            attributes(4, 2, 12, 13, layer, fluid_kind, diagonal, flags, 8, 4, 3),
                            0x010203,
                        );
                        assert_roundtrip(&packed);
                    }
                }
            }
        }
    }
}

#[test]
fn out_of_domain_vertices_are_rejected_without_truncation() {
    let cases = [
        vertex(17, 0, 0, 1, base_attributes(), 0),
        vertex(0, 17, 0, 1, base_attributes(), 0),
        vertex(0, 0, 16 * 1024 + 1, 1, base_attributes(), 0),
        vertex(
            0,
            0,
            0,
            COMPACT_VERTEX_MAX_STATE_ID + 1,
            base_attributes(),
            0,
        ),
        vertex(0, 0, 0, 1, base_attributes(), 0x0100_0000),
        vertex(
            0,
            0,
            0,
            1,
            attributes(1, 3, 15, 15, 0, 0, 0, 0, 0, 16, 0),
            0,
        ),
        vertex(
            0,
            0,
            0,
            1,
            attributes(1, 3, 15, 15, 0, 0, 0, 0, 17, 16, 0),
            0,
        ),
        PackedVertex {
            position: 1 << 30,
            state_id: 1,
            attributes: base_attributes(),
            tint: 0,
        },
    ];

    for packed in &cases {
        assert!(!CompactVoxelVertex::is_representable(packed));
        assert!(CompactVoxelVertex::try_from_packed(packed).is_none());
    }
    let valid = vertex(0, 0, 0, 1, base_attributes(), 0);
    assert!(try_pack_vertices_compact(&[valid]).is_some());
    assert!(try_pack_vertices_compact(&cases).is_none());
}

#[test]
#[should_panic(expected = "outside CompactVoxelVertex")]
fn from_packed_panics_on_out_of_domain_vertex() {
    let packed = vertex(0, 0, 16 * 1024 + 1, 1, base_attributes(), 0);
    let _ = CompactVoxelVertex::from_packed(&packed);
}

#[test]
fn full_meshed_section_roundtrips_through_compact_vertices() {
    let mut section = Section::filled(Block::opaque_cube(11), Light::new(0, 15));
    let dirt = Block {
        state_id: 12,
        tint: 0x336699,
        flags: megu_client_mesh::OPAQUE | megu_client_mesh::TINTED,
        occlusion: megu_client_mesh::ALL_FACES,
        ..Block::AIR
    };
    for x in 0..16 {
        for z in 0..16 {
            if (x + z) % 5 == 0 {
                section.set(x, 0, z, dirt, Light::new(4, 15));
            }
        }
    }
    for x in 4..12 {
        for y in 4..12 {
            for z in 4..12 {
                if (x + y + z) % 3 == 0 {
                    section.set(x, y, z, Block::AIR, Light::new(15, 0));
                }
            }
        }
    }
    let water = Block {
        state_id: 13,
        fluid_tint: 0x224466,
        flags: megu_client_mesh::FLUID_TINTED,
        fluid_kind: WATER,
        fluid_level: 8,
        ..Block::AIR
    };
    for x in 2..6 {
        for z in 2..6 {
            section.set(x, 1, z, water, Light::new(2, 15));
        }
    }

    let view = section.view();
    let quads = mesh_section(&view);
    assert!(!quads.is_empty());

    let mut packed = vec![PackedVertex::default(); quads.len() * 4];
    let mut count = 0usize;
    // SAFETY: quads and packed are live for their lengths, output capacity is
    // exactly four vertices per quad, and count is a disjoint writable slot.
    let status = unsafe {
        megu_client_mesh_pack_vertices(
            quads.as_ptr(),
            quads.len(),
            packed.as_mut_ptr(),
            packed.len(),
            &mut count,
        )
    };
    assert_eq!(status, ABI_OK);
    assert_eq!(count, quads.len() * 4);
    packed.truncate(count);
    assert!(packed.iter().all(CompactVoxelVertex::is_representable));

    let compact = pack_vertices_compact(&packed);
    assert_eq!(compact.len(), packed.len());
    for (original, compact) in packed.iter().zip(&compact) {
        assert_eq!(compact.to_packed(), *original);
    }
}
