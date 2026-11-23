// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! `CompactVoxelVertex` (12B) upload coverage for `client-gpu`: byte-stride
//! proof of the terrain vertex layout, bit-exact `PackedVertex` roundtrips
//! through the production staging path, the region-arena section-word
//! encoding, and the retained-CPU telemetry surface.

use megu_client_gpu::renderer::{
    compact_arena_representable, compact_arena_words, region_section_word, stage_compact_verts,
    TERRAIN_VERTEX_LAYOUT,
};
use megu_client_mesh::compact_vertex::CompactVoxelVertex;
use megu_client_mesh::ffi::PackedVertex;
use megu_client_mesh::SectionCoord;

/// One representable quad: position bits x/z/y-fixed in section-local
/// range, direction+AO+light+layer attributes, extents in the top byte.
fn quad(tag: u32) -> [PackedVertex; 4] {
    let base = PackedVertex {
        position: (tag & 0xf) | (((tag * 3) & 0xf) << 5) | (((tag * 977) & 0x3fff) << 10),
        state_id: (tag * 4099) & 0x7ffff,
        attributes: (tag & 0x3ffff) | (1 << 20) | (1 << 25),
        tint: (tag * 16807) & 0x00ff_ffff,
    };
    [base, base, base, base]
}

#[test]
fn terrain_vertex_layout_is_the_12_byte_compact_stream() {
    assert_eq!(std::mem::size_of::<CompactVoxelVertex>(), 12);
    assert_eq!(std::mem::size_of::<PackedVertex>(), 16);
    assert_eq!(
        TERRAIN_VERTEX_LAYOUT.array_stride, 12,
        "the terrain pipelines must fetch the 12-byte compact stream"
    );
    assert_eq!(TERRAIN_VERTEX_LAYOUT.attributes.len(), 3);
    assert!(
        TERRAIN_VERTEX_LAYOUT
            .attributes
            .iter()
            .all(|attr| attr.format == wgpu::VertexFormat::Uint32),
        "compact words are three u32 attributes"
    );
}

#[test]
fn staged_compact_bytes_roundtrip_representable_vertices_bit_exact() {
    let verts: Vec<PackedVertex> = (1..64u32).flat_map(quad).collect();
    let mut bytes = Vec::new();
    let bad = stage_compact_verts(&mut bytes, &verts);
    assert_eq!(bad, 0, "fixture quads are all representable");
    assert_eq!(bytes.len(), verts.len() * 12);

    for (vertex, record) in verts.iter().zip(bytes.as_chunks::<12>().0) {
        let compact = CompactVoxelVertex {
            word0: u32::from_ne_bytes(record[0..4].try_into().unwrap()),
            word1: u32::from_ne_bytes(record[4..8].try_into().unwrap()),
            word2: u32::from_ne_bytes(record[8..12].try_into().unwrap()),
        };
        let back = compact.to_packed();
        assert_eq!(back.position, vertex.position, "position bits lost");
        assert_eq!(back.state_id, vertex.state_id, "state id lost");
        assert_eq!(back.attributes, vertex.attributes, "attribute bits lost");
        assert_eq!(back.tint, vertex.tint, "tint lost");
    }
}

#[test]
fn unrepresentable_quads_stage_as_degenerate_zeros_and_count() {
    // y_fixed beyond COMPACT_VERTEX_MAX_Y_FIXED fails the domain check.
    let mut bad_quad = quad(3);
    bad_quad[0].position |= 0x3fff_ffff << 10;
    let verts: Vec<PackedVertex> = quad(1)
        .into_iter()
        .chain(bad_quad)
        .chain(quad(2))
        .collect();
    let mut bytes = Vec::new();
    assert_eq!(stage_compact_verts(&mut bytes, &verts), 1);
    assert_eq!(bytes.len(), verts.len() * 12);
    assert_eq!(
        &bytes[48..96],
        &[0u8; 48],
        "unrepresentable quad must stage as four zeroed vertices"
    );
}

#[test]
fn region_arena_words_carry_the_section_offset_in_compact_attr_bits() {
    // Origin (-8, 0, -8) -> coord (-7, 2, -7): sx=1, sy=2, sz=1
    // -> section byte = 1 | (2<<3) | (1<<5) = 0b0011_0001.
    let origin = [-8, 0, -8];
    let coord = SectionCoord(-7, 2, -7);
    let word = region_section_word(origin, coord);
    assert_eq!(word >> 24, 0x31);

    let vertex = quad(7)[0];
    assert!(compact_arena_representable(&vertex));
    let [w0, w1, _w2] = compact_arena_words(&vertex, word);

    // Replay the WGSL region decode: attr bits 20-27 -> attributes 24-31.
    let pos_index = w0 & 0x7f_ffff;
    let attr = (w0 >> 23) | ((w1 & 0x1f_ffff) << 9);
    let attributes = (attr & 0xf_ffff) | (((attr >> 20) & 0xff) << 24);
    assert_eq!(attributes >> 24, 0x31, "section word must lift to bits 24-31");
    assert_eq!(
        attributes & 0xf_ffff,
        vertex.attributes & 0xf_ffff,
        "low attribute bits must survive"
    );

    // The position index decodes back to the same packed position word.
    let y_fixed = pos_index / 289;
    let rem = pos_index % 289;
    let (x, z) = (rem % 17, rem / 17);
    assert_eq!(
        x | (z << 5) | (y_fixed << 10),
        vertex.position,
        "region stream must keep the packed position word"
    );
}

#[test]
fn unrepresentable_region_quad_is_rejected_not_truncated() {
    let mut vertex = quad(2)[0];
    vertex.state_id = u32::MAX;
    assert!(!compact_arena_representable(&vertex));
}
