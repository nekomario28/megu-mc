// SPDX-License-Identifier: LGPL-3.0-or-later

use megu_client_mesh::compact_vertex::{
    emit_wgsl_quad_pull_unpack, emit_wgsl_unpack_helpers, BitFieldSpec, CompactVoxelVertex,
    PlanarQuadRecord, COMPACT_VERTEX_SCHEMA,
};

#[test]
fn record_strides_and_compact_schema_are_exact_and_disjoint() {
    assert_eq!(std::mem::size_of::<PlanarQuadRecord>(), 24);
    assert_eq!(std::mem::size_of::<CompactVoxelVertex>(), 12);
    assert_eq!(4 * std::mem::size_of::<CompactVoxelVertex>(), 48);

    for (index, field) in COMPACT_VERTEX_SCHEMA.iter().enumerate() {
        assert!(field.bits > 0 && field.bits <= 32);
        assert!(u16::from(field.shift) + u16::from(field.bits) <= 32);
        for other in &COMPACT_VERTEX_SCHEMA[index + 1..] {
            if field.word == other.word {
                assert_eq!(
                    field.mask() & other.mask(),
                    0,
                    "{field:?} overlaps {other:?}"
                );
            }
        }
    }
}

#[test]
fn compact_vertex_round_trips_boundary_position_and_full_compact_state_domain() {
    let cases = [
        (0, 0, 0, 0, 0),
        (16, 16_384, 16, 0x7ffff, 0xffffff),
        (1, 15_359, 15, 26_684, 0x123456),
    ];
    for (x, y, z, state_id, tint) in cases {
        let vertex = megu_client_mesh::PackedVertex {
            position: x | (z << 5) | (y << 10),
            state_id,
            attributes: 5 | (15 << 20) | (16 << 25) | (3 << 30),
            tint,
        };
        let compact = CompactVoxelVertex::try_from_packed(&vertex).unwrap();
        assert_eq!(compact.to_packed(), vertex);
    }
}

fn record(face: u8, diagonal: bool) -> PlanarQuadRecord {
    PlanarQuadRecord::pack(
        1_500_000,
        [2 * 1024, 3 * 1024, 4 * 1024],
        [2 * 1024, 3 * 1024],
        face,
        diagonal,
        [0, 1, 2, 3],
        [0, 5, 10, 15],
        [15, 10, 5, 0],
        0xabcdef,
        0xa5,
    )
    .unwrap()
}

#[test]
fn planar_quad_preserves_full_state_shading_and_all_face_corners() {
    const TANGENTS: [[usize; 2]; 6] = [[0, 2], [2, 0], [1, 0], [0, 1], [2, 1], [1, 2]];
    const NORMALS: [[f32; 3]; 6] = [
        [0.0, -1.0, 0.0],
        [0.0, 1.0, 0.0],
        [0.0, 0.0, -1.0],
        [0.0, 0.0, 1.0],
        [-1.0, 0.0, 0.0],
        [1.0, 0.0, 0.0],
    ];
    const UV_AXES: [[usize; 2]; 6] = [[0, 2], [0, 2], [0, 1], [0, 1], [2, 1], [2, 1]];
    for face in 0..6 {
        let quad = record(face, false);
        let corners = quad.expand_corners();
        assert_eq!(quad.state_id, 1_500_000);
        for (index, corner) in corners.iter().enumerate() {
            let mut expected = [2.0, 3.0, 4.0];
            if index == 1 || index == 2 {
                expected[TANGENTS[face as usize][0]] += 2.0;
            }
            if index >= 2 {
                expected[TANGENTS[face as usize][1]] += 3.0;
            }
            assert_eq!(corner.position, expected, "face {face}, corner {index}");
            assert_eq!(corner.normal, NORMALS[face as usize]);
            assert_eq!(
                corner.uv,
                [
                    expected[UV_AXES[face as usize][0]],
                    expected[UV_AXES[face as usize][1]]
                ]
            );
            assert_eq!(corner.ao, index as u8);
            assert_eq!(corner.block_light, (index * 5) as u8);
            assert_eq!(corner.sky_light, (15 - index * 5) as u8);
            assert_eq!(corner.state_id, 1_500_000);
            assert_eq!(corner.tint, 0xabcdef);
            assert_eq!(corner.material_flags, 0xa5);
        }
        assert_eq!(quad.indices(), [0, 1, 2, 0, 2, 3]);

        let flipped = record(face, true);
        assert_eq!(flipped.expand_corners(), corners);
        assert_eq!(flipped.indices(), [0, 1, 3, 1, 2, 3]);
        for quad in [&quad, &flipped] {
            let vertices = quad.expand_corners();
            let indices = quad.indices();
            let (triangles, remainder) = indices.as_chunks::<3>();
            assert!(remainder.is_empty());
            for tri in triangles {
                let a = vertices[usize::from(tri[0])].position;
                let b = vertices[usize::from(tri[1])].position;
                let c = vertices[usize::from(tri[2])].position;
                let ab = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
                let ac = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
                let cross = [
                    ab[1] * ac[2] - ab[2] * ac[1],
                    ab[2] * ac[0] - ab[0] * ac[2],
                    ab[0] * ac[1] - ab[1] * ac[0],
                ];
                let dot = cross[0] * NORMALS[face as usize][0]
                    + cross[1] * NORMALS[face as usize][1]
                    + cross[2] * NORMALS[face as usize][2];
                assert!(dot > 0.0, "face {face}, tri {tri:?} reversed winding");
            }
        }
    }

    let boundary = PlanarQuadRecord::pack(
        1_500_000,
        [0; 3],
        [16_384; 2],
        0,
        false,
        [0, 1, 2, 3],
        [0, 5, 10, 15],
        [15, 10, 5, 0],
        0xffffff,
        0xff,
    )
    .unwrap();
    assert_eq!(boundary.expand_corners()[2].position, [16.0, 0.0, 16.0]);
    let upper_face = PlanarQuadRecord::pack(
        1_500_000,
        [0, 16_384, 0],
        [16_384; 2],
        1,
        false,
        [0, 1, 2, 3],
        [0, 5, 10, 15],
        [15, 10, 5, 0],
        0xffffff,
        0xff,
    )
    .unwrap();
    assert_eq!(upper_face.expand_corners()[2].position, [16.0, 16.0, 16.0]);
}

#[test]
fn planar_pack_rejects_unrepresentable_values_and_wgsl_uses_schema_offsets() {
    assert!(
        PlanarQuadRecord::pack(1, [0; 3], [0, 1024], 0, false, [0; 4], [0; 4], [0; 4], 0, 0)
            .is_none()
    );
    assert!(
        PlanarQuadRecord::pack(1, [0; 3], [1024; 2], 6, false, [0; 4], [0; 4], [0; 4], 0, 0)
            .is_none()
    );
    assert!(
        PlanarQuadRecord::pack(1, [0; 3], [1024; 2], 0, false, [4; 4], [0; 4], [0; 4], 0, 0)
            .is_none()
    );

    let wgsl = emit_wgsl_unpack_helpers();
    for field in COMPACT_VERTEX_SCHEMA {
        let name = field.name.to_ascii_uppercase();
        assert!(wgsl.contains(&format!("CV_{name}_SHIFT: u32 = {}u", field.shift)));
        assert!(wgsl.contains(&format!("CV_{name}_MASK: u32 = 0x{:08x}u", field.mask())));
    }
    let quad_wgsl = emit_wgsl_quad_pull_unpack();
    for (field, offset, bits) in [
        ("ORIGIN_X_FIXED", 0, 15),
        ("ORIGIN_Y_FIXED", 15, 15),
        ("ORIGIN_Z_FIXED", 30, 15),
        ("SPAN_U_FIXED", 45, 15),
        ("SPAN_V_FIXED", 60, 15),
        ("FACE", 75, 3),
        ("DIAGONAL", 78, 1),
        ("AO", 79, 8),
        ("CORNER_LIGHT", 87, 32),
        ("TINT", 119, 24),
        ("MATERIAL_FLAGS", 143, 8),
    ] {
        assert!(quad_wgsl.contains(&format!("PQ_{field}_OFFSET: u32 = {offset}u")));
        let mask = if bits == 32 {
            u32::MAX as u64
        } else {
            (1u64 << bits) - 1
        };
        assert!(quad_wgsl.contains(&format!("PQ_{field}_MASK: u32 = 0x{mask:x}u")));
    }
}

#[allow(dead_code)]
fn assert_field_spec_is_public(_: BitFieldSpec) {}
