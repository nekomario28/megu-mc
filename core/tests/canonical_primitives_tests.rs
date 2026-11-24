// SPDX-License-Identifier: LGPL-3.0-or-later

use megu_core::{bits, geom, hash, varint};

#[test]
fn crc32_and_fnv_vectors_and_chunking() {
    assert_eq!(hash::crc32_ieee(b"123456789"), 0xCBF4_3926);
    let first = hash::crc32_ieee(b"1234");
    assert_eq!(
        hash::crc32_ieee_update(first, b"56789"),
        hash::crc32_ieee(b"123456789")
    );
    assert_eq!(
        hash::crc32_ieee_update(0, b"123456789"),
        hash::crc32_ieee(b"123456789")
    );

    assert_eq!(hash::fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
    assert_eq!(hash::fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
    assert_eq!(hash::fnv1a64_str("hello"), 0xa430_d846_80aa_bd0b);
    let mut incremental = hash::Fnv1a64::new();
    incremental.update(b"he");
    incremental.update_str("llo");
    assert_eq!(incremental.finish(), hash::fnv1a64(b"hello"));
}

#[test]
fn protocol_varint_boundaries_and_rejection() {
    for value in [i32::MIN, -1, 0, 1, 127, 128, 16_383, 16_384, i32::MAX] {
        let (encoded, len) = varint::encode_varint_i32(value);
        assert_eq!(len, varint::varint_i32_len(value));
        assert_eq!(varint::read_varint_i32(&encoded[..len]), Ok((value, len)));
        let mut out = Vec::new();
        varint::write_varint_i32(value, &mut out);
        assert_eq!(out, encoded[..len]);
    }
    for value in [i64::MIN, -1, 0, 127, 128, i64::MAX] {
        let (encoded, len) = varint::encode_varlong_i64(value);
        assert_eq!(len, varint::varlong_i64_len(value));
        assert_eq!(varint::read_varlong_i64(&encoded[..len]), Ok((value, len)));
        let mut out = Vec::new();
        varint::write_varlong_i64(value, &mut out);
        assert_eq!(out, encoded[..len]);
    }
    assert_eq!(
        varint::read_varint_i32(&[0x80]),
        Err(varint::VarIntError::UnexpectedEof)
    );
    assert_eq!(
        varint::read_varint_i32(&[0x80, 0]),
        Err(varint::VarIntError::TooLong)
    );
    assert_eq!(
        varint::read_varint_i32(&[0xff, 0xff, 0xff, 0xff, 0x10]),
        Err(varint::VarIntError::TooLong)
    );
    assert_eq!(
        varint::read_varlong_i64(&[0x80]),
        Err(varint::VarIntError::UnexpectedEof)
    );
    assert_eq!(
        varint::read_varlong_i64(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 2]),
        Err(varint::VarIntError::TooLong)
    );
}

#[test]
fn minecraft_string_caps_and_utf8_validation() {
    let mut encoded = Vec::new();
    varint::write_mc_string("A🦀", &mut encoded);
    assert_eq!(
        varint::read_mc_string(&encoded, 5),
        Ok(("A🦀", encoded.len()))
    );
    assert_eq!(
        varint::read_mc_string(&encoded, 4),
        Err(varint::VarIntError::StringTooLong { len: 5, max: 4 })
    );
    assert_eq!(
        varint::read_mc_string_utf16_capped(&encoded, 3),
        Ok(("A🦀", encoded.len()))
    );
    assert_eq!(
        varint::read_mc_string_utf16_capped(&encoded, 2),
        Err(varint::VarIntError::StringTooLong { len: 3, max: 2 })
    );
    assert_eq!(
        varint::read_mc_string(&[0xff, 0xff, 0xff, 0xff, 0x0f], 100),
        Err(varint::VarIntError::NegativeLength(-1))
    );
    assert_eq!(
        varint::read_mc_string(&[1, 0xff], 100),
        Err(varint::VarIntError::InvalidUtf8)
    );
}

#[test]
fn bit_storage_roundtrips_all_widths_and_matches_simd() {
    for bits_per_entry in 1..=16 {
        let mask = (1u32 << bits_per_entry) - 1;
        let values: Vec<u16> = (0..137)
            .map(|i| ((i as u32 * 31 + 7) & mask) as u16)
            .collect();
        let packed = bits::pack_from_u16(&values, bits_per_entry);
        assert_eq!(packed.len(), bits::long_count(bits_per_entry, values.len()));
        let mut unpacked = vec![0; values.len()];
        bits::unpack_to_u16(&packed, bits_per_entry, values.len(), &mut unpacked);
        assert_eq!(unpacked, values, "width {bits_per_entry}");
        for (index, &value) in values.iter().enumerate() {
            assert_eq!(
                bits::get(&packed, bits_per_entry, index),
                Some(u32::from(value))
            );
        }
    }
    let values: Vec<u16> = (0..256).map(|i| (i % 16) as u16).collect();
    let packed = bits::pack_from_u16(&values, 4);
    let mut out = vec![0; values.len()];
    bits::unpack_to_u16(&packed, 4, values.len(), &mut out);
    let mut simd = vec![0; values.len()];
    megu_core::simd_voxel::unpack_palette_4bit_to_u16(
        &packed,
        &(0u16..16).collect::<Vec<_>>(),
        &mut simd,
    );
    assert_eq!(out, values);
    assert_eq!(out, simd);
}

#[test]
fn light_nibbles_and_adaptive_merge_match_scalar() {
    let mut light = bits::LightArray::new_zero();
    light.set_idx(0, 0x1f);
    light.set_idx(1, 0x2e);
    assert_eq!(light.as_bytes()[0], 0xef);
    assert_eq!(light.get_idx(0), 15);
    assert_eq!(light.get_idx(1), 14);
    assert_eq!(bits::section_index(3, 2, 1), (2 << 8) | (1 << 4) | 3);
    let mut other = bits::LightArray::new_zero();
    for i in 0..bits::LIGHT_NIBBLES {
        other.set_idx(i, (i % 16) as u8);
    }
    let mut merged = light;
    merged.max_merge_from(&other);
    let expected = megu_core::hw_adaptive_engine::max_nibble_arrays_tiered(
        light.as_bytes(),
        other.as_bytes(),
        megu_core::hw_adaptive_engine::SimdKernelTier::Scalar,
        false,
    );
    assert_eq!(merged.as_bytes(), &expected);
}

#[test]
fn frustum_aabb_test_matches_simd_culling() {
    let matrix = [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ];
    let frustum = geom::Frustum::from_view_proj(&matrix);
    let bounds = geom::Aabb3f {
        min: [-1.0, -1.0, -1.0],
        max: [1.0, 1.0, 1.0],
    };
    assert!(bounds.contains_point([0.0, 0.0, 0.0]));
    assert!(bounds.intersects(&geom::Aabb3f {
        min: [1.0, 0.0, 0.0],
        max: [2.0, 1.0, 1.0],
    }));
    assert_eq!(bounds.center(), [0.0; 3]);
    assert_eq!(bounds.extents(), [1.0; 3]);
    let bounds_d = geom::Aabb3d {
        min: [0.0; 3],
        max: [1.0; 3],
    };
    assert_eq!(bounds_d.translated([1.0; 3]).min, [1.0; 3]);
    assert_eq!(bounds_d.expanded(1.0).min, [-1.0; 3]);
    assert_eq!(bounds_d.union(&bounds_d.translated([1.0; 3])).max, [2.0; 3]);
    let mins: Vec<[f32; 3]> = (0..80)
        .map(|i| [i as f32 * 0.1 - 4.0, -0.5, -0.5])
        .collect();
    let maxs: Vec<[f32; 3]> = mins.iter().map(|p| [p[0] + 0.75, 0.5, 0.5]).collect();
    let expected: Vec<bool> = mins
        .iter()
        .zip(&maxs)
        .map(|(min, max)| frustum.contains_aabb_pvertex(*min, *max))
        .collect();
    let (min_x, min_y, min_z): (Vec<_>, Vec<_>, Vec<_>) = mins
        .iter()
        .map(|p| (p[0], p[1], p[2]))
        .collect::<Vec<_>>()
        .into_iter()
        .fold((vec![], vec![], vec![]), |(mut x, mut y, mut z), p| {
            x.push(p.0);
            y.push(p.1);
            z.push(p.2);
            (x, y, z)
        });
    let (max_x, max_y, max_z): (Vec<_>, Vec<_>, Vec<_>) = maxs
        .iter()
        .map(|p| (p[0], p[1], p[2]))
        .collect::<Vec<_>>()
        .into_iter()
        .fold((vec![], vec![], vec![]), |(mut x, mut y, mut z), p| {
            x.push(p.0);
            y.push(p.1);
            z.push(p.2);
            (x, y, z)
        });
    let mut words = [0u64; 2];
    megu_core::simd_voxel::batch_aabb_frustum_cull_soa(
        &min_x,
        &min_y,
        &min_z,
        &max_x,
        &max_y,
        &max_z,
        &frustum.to_raw_planes(),
        &mut words,
    );
    for (i, visible) in expected.into_iter().enumerate() {
        assert_eq!(visible, words[i / 64] & (1 << (i % 64)) != 0);
    }
}

#[test]
fn zlib_stored_roundtrip_checksum_and_bomb_cap() {
    assert_eq!(megu_core::zip_archive::adler32(b"Wikipedia"), 0x11E6_0398);
    let payload = b"stored zlib payload".repeat(100);
    let encoded = megu_core::zip_archive::zlib_compress_stored(&payload);
    assert_eq!(
        megu_core::zip_archive::zlib_decompress_capped(&encoded, payload.len()).unwrap(),
        payload.clone()
    );
    assert!(megu_core::zip_archive::zlib_decompress_capped(&encoded, payload.len() - 1).is_err());
    assert_eq!(
        megu_core::zip_archive::zlib_decompress_capped(
            &megu_core::zip_archive::zlib_compress_stored(&[]),
            0
        )
        .unwrap(),
        Vec::<u8>::new()
    );
    let mut bad_checksum = encoded;
    let last = bad_checksum.len() - 1;
    bad_checksum[last] ^= 1;
    assert!(matches!(
        megu_core::zip_archive::zlib_decompress_capped(&bad_checksum, payload.len()),
        Err(megu_core::zip_archive::ZipError::Malformed(
            "zlib Adler-32 mismatch"
        ))
    ));
    let bomb = megu_core::zip_archive::zlib_compress_stored(&vec![b'x'; 70_000]);
    assert_eq!(
        megu_core::zip_archive::zlib_decompress_capped(&bomb, 70_000).unwrap(),
        vec![b'x'; 70_000]
    );
    assert!(matches!(
        megu_core::zip_archive::zlib_decompress_capped(&bomb, 1024),
        Err(megu_core::zip_archive::ZipError::OutputCapExceeded)
    ));
    assert!(matches!(
        megu_core::zip_archive::zlib_decompress_capped(&[0, 0, 0, 0, 0, 0], 10),
        Err(megu_core::zip_archive::ZipError::Malformed(
            "invalid zlib header"
        ))
    ));
    assert!(matches!(
        megu_core::zip_archive::zlib_decompress_capped(&[0x78, 0x20, 0, 0, 0, 0], 10),
        Err(megu_core::zip_archive::ZipError::Malformed(
            "zlib preset dictionary is unsupported"
        ))
    ));
}
