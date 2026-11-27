// SPDX-License-Identifier: LGPL-3.0-or-later

use megu_core::{bits, hash, varint};

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

/// Classic per-byte IEEE 802.3 reference (polynomial `0xEDB8_8320`),
/// implemented here — not via `hash::CRC32_IEEE_TABLE` — so the test
/// pins the public CRC against the definition, not against itself.
fn crc32_reference(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for (i, slot) in table.iter_mut().enumerate() {
        let mut crc = i as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                0xedb8_8320 ^ (crc >> 1)
            } else {
                crc >> 1
            };
        }
        *slot = crc;
    }
    let mut state = !0u32;
    for &byte in data {
        state = table[((state ^ u32::from(byte)) & 0xff) as usize] ^ (state >> 8);
    }
    !state
}

#[test]
fn crc32_slice_by_8_is_bit_exact_against_reference() {
    // Deterministic byte pattern with no period aligned to the 8-byte
    // slicing stride.
    let mut buf = Vec::with_capacity(65_536);
    let mut state = 0x9e37_79b9u32;
    while buf.len() < 65_536 {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        buf.push((state >> 16) as u8);
    }

    // Every length across several slice-by-8 block boundaries, plus a
    // spread covering the whole 0..64KiB contract.
    let mut lengths: Vec<usize> = (0..=520).collect();
    lengths.extend([1_000, 4_095, 4_096, 4_097, 8_191, 16_384, 32_767, 65_535, 65_536]);
    for &len in &lengths {
        let data = &buf[..len];
        assert_eq!(
            hash::crc32_ieee(data),
            crc32_reference(data),
            "crc32_ieee differs from per-byte reference at len {len}"
        );
        // Incremental update across a split must equal the one-shot hash
        // at every split point class (inside a block, on the boundary,
        // mid-tail).
        for &split in &[0usize, 1, 7, 8, 9, len / 2, len.saturating_sub(1), len] {
            let split = split.min(len);
            let first = hash::crc32_ieee(&data[..split]);
            assert_eq!(
                hash::crc32_ieee_update(first, &data[split..]),
                crc32_reference(data),
                "chunked update differs at len {len} split {split}"
            );
        }
    }
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
