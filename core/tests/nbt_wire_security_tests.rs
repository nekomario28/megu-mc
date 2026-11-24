// SPDX-License-Identifier: LGPL-3.0-or-later
//! Security and conformance tests for the canonical NBT wire skipper.

use std::time::{Duration, Instant};

use megu_core::nbt_wire::{
    skip_named_nbt, skip_nbt_payload, skip_network_nbt, NbtWireError, NbtWireLimits,
    NBT_WIRE_DEFAULT_MAX_DEPTH, NBT_WIRE_DEFAULT_MAX_TAGS, NBT_WIRE_HARD_MAX_DEPTH,
};

fn default_limits() -> NbtWireLimits {
    NbtWireLimits::default()
}

fn push_nbt_string(out: &mut Vec<u8>, bytes: &[u8]) {
    let len = u16::try_from(bytes.len()).expect("test string fits u16");
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(bytes);
}

fn push_named(out: &mut Vec<u8>, tag: u8, name: &[u8], payload: &[u8]) {
    out.push(tag);
    push_nbt_string(out, name);
    out.extend_from_slice(payload);
}

fn list_payload(elem_tag: u8, elements: &[Vec<u8>]) -> Vec<u8> {
    let mut out = vec![elem_tag];
    let len = i32::try_from(elements.len()).expect("test list fits i32");
    out.extend_from_slice(&len.to_be_bytes());
    for element in elements {
        out.extend_from_slice(element);
    }
    out
}

fn array_payload(width: usize, count: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let len = i32::try_from(count).expect("test array fits i32");
    out.extend_from_slice(&len.to_be_bytes());
    out.resize(4 + count * width, 0);
    out
}

/// Minimal valid payload for each tag id `0..=12`.
fn minimal_payload(tag: u8) -> Vec<u8> {
    match tag {
        0 => Vec::new(),
        1 => vec![0],
        2 => vec![0; 2],
        3 | 5 => vec![0; 4],
        4 | 6 => vec![0; 8],
        7 | 11 | 12 => vec![0; 4],
        8 => vec![0; 2],
        9 => vec![0; 5],
        10 => vec![0],
        _ => unreachable!("minimal payload only covers valid tag ids"),
    }
}

/// A compound exercising every non-End tag type, nested compounds/lists and
/// both UTF-8 and Java MUTF-8 string bytes.
fn sample_compound_payload() -> Vec<u8> {
    let mut out = Vec::new();

    push_named(&mut out, 1, b"byte", &[0x7f]);
    push_named(&mut out, 2, b"short", &(-2i16).to_be_bytes());
    push_named(&mut out, 3, b"int", &42i32.to_be_bytes());
    push_named(&mut out, 4, b"long", &(-9i64).to_be_bytes());
    push_named(&mut out, 5, b"float", &1.5f32.to_be_bytes());
    push_named(&mut out, 6, b"double", &(-2.5f64).to_be_bytes());

    let mut byte_array = 3i32.to_be_bytes().to_vec();
    byte_array.extend_from_slice(&[1, 2, 3]);
    push_named(&mut out, 7, b"byte_array", &byte_array);

    let mut string = Vec::new();
    push_nbt_string(&mut string, "héllo 日本語".as_bytes());
    push_named(&mut out, 8, b"utf8", &string);

    let mut mutf8 = Vec::new();
    // MUTF-8 NUL (C0 80) followed by the CESU-8 surrogate pair for U+1F600.
    push_nbt_string(
        &mut mutf8,
        &[0xC0, 0x80, 0xED, 0xA0, 0x80, 0xED, 0xB8, 0x80],
    );
    push_named(&mut out, 8, b"mutf8", &mutf8);

    push_named(&mut out, 9, b"empty_list", &[0, 0, 0, 0, 0]);

    let int_elements = vec![
        1i32.to_be_bytes().to_vec(),
        2i32.to_be_bytes().to_vec(),
        3i32.to_be_bytes().to_vec(),
    ];
    push_named(&mut out, 9, b"ints", &list_payload(3, &int_elements));

    let mut nested = Vec::new();
    push_named(&mut nested, 8, b"inside", &{
        let mut s = Vec::new();
        push_nbt_string(&mut s, b"value");
        s
    });
    let nested_element = {
        let mut c = nested;
        c.push(0);
        c
    };
    push_named(
        &mut out,
        9,
        b"compounds",
        &list_payload(10, &[nested_element, vec![0]]),
    );

    push_named(&mut out, 10, b"child", &[0]);
    push_named(&mut out, 11, b"int_array", &array_payload(4, 2));
    push_named(&mut out, 12, b"long_array", &array_payload(8, 2));

    out.push(0); // TAG_End
    out
}

fn network_compound() -> Vec<u8> {
    let mut wire = vec![10u8];
    wire.extend_from_slice(&sample_compound_payload());
    wire
}

fn named_compound(name: &[u8]) -> Vec<u8> {
    let mut wire = vec![10u8];
    push_nbt_string(&mut wire, name);
    wire.extend_from_slice(&sample_compound_payload());
    wire
}

fn nested_compounds(children: usize) -> Vec<u8> {
    let mut wire = vec![10u8];
    for _ in 0..children {
        wire.extend_from_slice(&[10, 0, 0]);
    }
    wire.extend(std::iter::repeat_n(0u8, children + 1));
    wire
}

// ---------------------------------------------------------------------------
// Conformance: every tag type, both wire shapes, exact consumed length
// ---------------------------------------------------------------------------

#[test]
fn every_tag_id_works_in_payload_network_and_named_shapes() {
    for tag in 1..=12u8 {
        let payload = minimal_payload(tag);

        assert_eq!(
            skip_nbt_payload(tag, &payload, default_limits()),
            Ok(payload.len()),
            "payload shape for tag {tag}"
        );

        let mut network = vec![tag];
        network.extend_from_slice(&payload);
        assert_eq!(
            skip_network_nbt(&network, default_limits()),
            Ok(network.len()),
            "anonymous/network shape for tag {tag}"
        );

        let mut named = vec![tag];
        push_nbt_string(&mut named, b"root");
        named.extend_from_slice(&payload);
        assert_eq!(
            skip_named_nbt(&named, default_limits()),
            Ok(named.len()),
            "named shape for tag {tag}"
        );
    }

    // TAG_End has an empty payload but is never a root value.
    assert_eq!(skip_nbt_payload(0, &[], default_limits()), Ok(0));
    assert_eq!(
        skip_network_nbt(&[0], default_limits()),
        Err(NbtWireError::InvalidTagId(0))
    );
    assert_eq!(
        skip_named_nbt(&[0], default_limits()),
        Err(NbtWireError::InvalidTagId(0))
    );
}

#[test]
fn network_compound_skips_all_tags_and_preserves_trailing_bytes() {
    let wire = network_compound();
    assert_eq!(
        skip_network_nbt(&wire, default_limits()),
        Ok(wire.len()),
        "all 13 tag types and nested containers"
    );

    let mut with_trailing = wire.clone();
    with_trailing.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
    let consumed = skip_network_nbt(&with_trailing, default_limits()).expect("valid prefix");
    assert_eq!(consumed, wire.len());
    assert_eq!(&with_trailing[consumed..], &[0xDE, 0xAD, 0xBE, 0xEF]);
}

#[test]
fn named_compound_with_utf8_root_name_skips_exact_length() {
    let name = "root-é-日本語".as_bytes();
    let wire = named_compound(name);
    assert_eq!(skip_named_nbt(&wire, default_limits()), Ok(wire.len()));

    let mut empty_name = vec![10u8];
    push_nbt_string(&mut empty_name, b"");
    empty_name.push(0);
    assert_eq!(
        skip_named_nbt(&empty_name, default_limits()),
        Ok(empty_name.len())
    );
}

#[test]
fn empty_tag_end_element_list_is_accepted() {
    let payload = [0u8, 0, 0, 0, 0];
    assert_eq!(
        skip_nbt_payload(9, &payload, default_limits()),
        Ok(payload.len())
    );

    let mut wire = vec![10u8];
    push_named(&mut wire, 9, b"empty", &payload);
    wire.push(0);
    assert_eq!(skip_network_nbt(&wire, default_limits()), Ok(wire.len()));
}

#[test]
fn utf8_and_modified_utf8_strings_are_opaque_bytes() {
    let mut utf8 = Vec::new();
    push_nbt_string(&mut utf8, "日本語".as_bytes());
    assert_eq!(skip_nbt_payload(8, &utf8, default_limits()), Ok(utf8.len()));

    let mut mutf8 = Vec::new();
    push_nbt_string(&mut mutf8, &[0xC0, 0x80, 0xED, 0xA0, 0x80]);
    assert_eq!(
        skip_nbt_payload(8, &mutf8, default_limits()),
        Ok(mutf8.len())
    );
}

// ---------------------------------------------------------------------------
// Security: DoS payloads, lengths, depth, budget, truncation
// ---------------------------------------------------------------------------

#[test]
fn empty_element_list_dos_is_rejected_in_microseconds() {
    // 543-byte TAG_List(TAG_End, 1_048_576) payload nested in a compound.
    // The guard must fire on the header, before any element loop runs.
    let mut wire = vec![10u8, 9, 0, 0, 0];
    wire.extend_from_slice(&1_048_576i32.to_be_bytes());
    wire.resize(543, 0);
    assert_eq!(wire.len(), 543);

    let start = Instant::now();
    let error = skip_network_nbt(&wire, default_limits()).expect_err("DoS list must fail");
    let elapsed = start.elapsed();

    assert_eq!(
        error,
        NbtWireError::InvalidEmptyElementList { length: 1_048_576 }
    );
    assert!(
        elapsed < Duration::from_millis(1),
        "DoS payload should fail before any element work, took {elapsed:?}"
    );
}

#[test]
fn negative_lengths_are_rejected_for_lists_and_all_arrays() {
    for tag in [7u8, 9, 11, 12] {
        let mut payload = Vec::new();
        if tag == 9 {
            payload.push(3); // element tag does not matter; length is checked first
        }
        payload.extend_from_slice(&(-1i32).to_be_bytes());
        assert_eq!(
            skip_nbt_payload(tag, &payload, default_limits()),
            Err(NbtWireError::NegativeLength { tag }),
            "negative length for tag {tag}"
        );
    }
}

#[test]
fn declared_lengths_past_the_input_are_rejected_before_advancing() {
    // String: declares 5 bytes, only 2 remain.
    assert_eq!(
        skip_nbt_payload(8, &[0, 5, 1, 2], default_limits()),
        Err(NbtWireError::LengthExceedsRemaining {
            tag: 8,
            length: 5,
            remaining: 2
        })
    );

    // Byte array: declares 4 bytes, only 2 remain.
    assert_eq!(
        skip_nbt_payload(7, &[0, 0, 0, 4, 1, 2], default_limits()),
        Err(NbtWireError::LengthExceedsRemaining {
            tag: 7,
            length: 4,
            remaining: 2
        })
    );

    // List: declares 5 elements, only 2 bytes remain.
    let mut list = vec![1u8];
    list.extend_from_slice(&5i32.to_be_bytes());
    list.extend_from_slice(&[0, 0]);
    assert_eq!(
        skip_nbt_payload(9, &list, default_limits()),
        Err(NbtWireError::LengthExceedsRemaining {
            tag: 9,
            length: 5,
            remaining: 2
        })
    );

    // Compound child name: declares 3 bytes, only 1 remains.
    assert_eq!(
        skip_nbt_payload(10, &[1, 0, 3, b'a'], default_limits()),
        Err(NbtWireError::LengthExceedsRemaining {
            tag: 8,
            length: 3,
            remaining: 1
        })
    );
}

#[test]
fn typed_array_byte_cap_is_enforced() {
    let byte_array = {
        let mut payload = 16i32.to_be_bytes().to_vec();
        payload.resize(20, 0);
        payload
    };
    let capped = NbtWireLimits {
        max_array_bytes: 15,
        ..default_limits()
    };
    assert_eq!(
        skip_nbt_payload(7, &byte_array, capped),
        Err(NbtWireError::ArrayTooLarge { bytes: 16, max: 15 })
    );

    let int_array = array_payload(4, 4);
    assert_eq!(
        skip_nbt_payload(11, &int_array, capped),
        Err(NbtWireError::ArrayTooLarge { bytes: 16, max: 15 })
    );

    let long_array = array_payload(8, 2);
    assert_eq!(
        skip_nbt_payload(12, &long_array, capped),
        Err(NbtWireError::ArrayTooLarge { bytes: 16, max: 15 })
    );
}

#[test]
fn list_length_cap_is_enforced() {
    let payload = list_payload(1, &[vec![0], vec![0], vec![0]]);
    let capped = NbtWireLimits {
        max_list_len: 2,
        ..default_limits()
    };
    assert_eq!(
        skip_nbt_payload(9, &payload, capped),
        Err(NbtWireError::ListTooLong { length: 3, max: 2 })
    );
}

#[test]
fn depth_ceiling_is_enforced_and_hard_capped() {
    let wire = nested_compounds(2);
    let allowed = NbtWireLimits {
        max_depth: 3,
        ..default_limits()
    };
    assert_eq!(
        skip_network_nbt(&wire, allowed),
        Ok(wire.len()),
        "root + two nested compounds is depth 3"
    );

    let shallow = NbtWireLimits {
        max_depth: 2,
        ..default_limits()
    };
    assert_eq!(
        skip_network_nbt(&wire, shallow),
        Err(NbtWireError::DepthExceeded { depth: 3, max: 2 })
    );

    // Even an absurd caller limit cannot exceed the fixed frame stack.
    let too_deep = nested_compounds(NBT_WIRE_HARD_MAX_DEPTH);
    let unbounded = NbtWireLimits {
        max_depth: usize::MAX,
        ..default_limits()
    };
    assert_eq!(
        skip_network_nbt(&too_deep, unbounded),
        Err(NbtWireError::DepthExceeded {
            depth: NBT_WIRE_HARD_MAX_DEPTH + 1,
            max: NBT_WIRE_HARD_MAX_DEPTH
        })
    );

    let deepest_allowed = nested_compounds(NBT_WIRE_DEFAULT_MAX_DEPTH - 1);
    assert_eq!(
        skip_network_nbt(&deepest_allowed, default_limits()),
        Ok(deepest_allowed.len())
    );
}

#[test]
fn aggregate_tag_budget_bounds_branching_and_packed_work() {
    let mut wire = vec![10u8];
    push_named(&mut wire, 1, b"a", &[0]);
    push_named(&mut wire, 1, b"b", &[0]);
    wire.push(0);

    // root + a + b = 3 value tags.
    assert_eq!(
        skip_network_nbt(
            &wire,
            NbtWireLimits {
                max_tags: 2,
                ..default_limits()
            }
        ),
        Err(NbtWireError::TagBudgetExceeded { max: 2 })
    );
    assert_eq!(
        skip_network_nbt(
            &wire,
            NbtWireLimits {
                max_tags: 3,
                ..default_limits()
            }
        ),
        Ok(wire.len())
    );

    // root + list field + four packed byte elements = 6 value tags.
    let mut packed = vec![10u8];
    let elements = vec![vec![0u8], vec![0], vec![0], vec![0]];
    push_named(&mut packed, 9, b"l", &list_payload(1, &elements));
    packed.push(0);
    assert_eq!(
        skip_network_nbt(
            &packed,
            NbtWireLimits {
                max_tags: 3,
                ..default_limits()
            }
        ),
        Err(NbtWireError::TagBudgetExceeded { max: 3 })
    );
    assert_eq!(
        skip_network_nbt(
            &packed,
            NbtWireLimits {
                max_tags: 6,
                ..default_limits()
            }
        ),
        Ok(packed.len())
    );
}

#[test]
fn exhaustive_truncation_sweep_rejects_every_short_prefix() {
    let wire = network_compound();
    for cut in 0..wire.len() {
        assert!(
            skip_network_nbt(&wire[..cut], default_limits()).is_err(),
            "network prefix of {cut}/{} bytes must fail",
            wire.len()
        );
    }
    assert_eq!(skip_network_nbt(&wire, default_limits()), Ok(wire.len()));

    let named = named_compound(b"root");
    for cut in 0..named.len() {
        assert!(
            skip_named_nbt(&named[..cut], default_limits()).is_err(),
            "named prefix of {cut}/{} bytes must fail",
            named.len()
        );
    }
    assert_eq!(skip_named_nbt(&named, default_limits()), Ok(named.len()));
}

#[test]
fn i32_max_lengths_are_rejected_without_arithmetic_overflow() {
    for tag in [7u8, 11, 12] {
        let width = match tag {
            7 => 1,
            11 => 4,
            _ => 8,
        };
        let mut payload = i32::MAX.to_be_bytes().to_vec();
        payload.resize(4 + width * 2, 0);
        assert_eq!(
            skip_nbt_payload(tag, &payload, default_limits()),
            Err(NbtWireError::LengthExceedsRemaining {
                tag,
                length: i32::MAX as usize,
                remaining: width * 2
            }),
            "i32::MAX x {width} bytes for tag {tag}"
        );
    }

    // TAG_LongArray with the full 8-byte payload after i32::MAX elements: the
    // declared count alone still exceeds the remaining bytes.
    let mut long_array = i32::MAX.to_be_bytes().to_vec();
    long_array.extend_from_slice(&[0u8; 8]);
    assert_eq!(
        skip_nbt_payload(12, &long_array, default_limits()),
        Err(NbtWireError::LengthExceedsRemaining {
            tag: 12,
            length: i32::MAX as usize,
            remaining: 8
        })
    );

    // A list length at i32::MAX hits the list cap first at default limits;
    // with the cap raised it still fails the remaining-bytes guard.
    let mut huge_list = vec![1u8];
    huge_list.extend_from_slice(&i32::MAX.to_be_bytes());
    assert_eq!(
        skip_nbt_payload(9, &huge_list, default_limits()),
        Err(NbtWireError::ListTooLong {
            length: i32::MAX as usize,
            max: 65_536
        })
    );
    assert_eq!(
        skip_nbt_payload(
            9,
            &huge_list,
            NbtWireLimits {
                max_list_len: usize::MAX,
                ..default_limits()
            }
        ),
        Err(NbtWireError::LengthExceedsRemaining {
            tag: 9,
            length: i32::MAX as usize,
            remaining: 0
        })
    );
}

#[test]
fn invalid_tag_ids_and_truncated_headers_fail_closed() {
    assert_eq!(
        skip_network_nbt(&[], default_limits()),
        Err(NbtWireError::UnexpectedEof {
            needed: 1,
            remaining: 0
        })
    );
    assert_eq!(
        skip_named_nbt(&[10], default_limits()),
        Err(NbtWireError::UnexpectedEof {
            needed: 2,
            remaining: 0
        })
    );
    assert_eq!(
        skip_network_nbt(&[13], default_limits()),
        Err(NbtWireError::InvalidTagId(13))
    );
    assert_eq!(
        skip_nbt_payload(13, &[], default_limits()),
        Err(NbtWireError::InvalidTagId(13))
    );
    assert_eq!(
        skip_nbt_payload(10, &[13], default_limits()),
        Err(NbtWireError::InvalidTagId(13))
    );
    assert_eq!(
        skip_nbt_payload(9, &[13, 0, 0, 0, 0], default_limits()),
        Err(NbtWireError::InvalidTagId(13))
    );
    assert_eq!(
        skip_named_nbt(&[10, 0, 5, b'a'], default_limits()),
        Err(NbtWireError::LengthExceedsRemaining {
            tag: 8,
            length: 5,
            remaining: 1
        })
    );
}

#[test]
fn default_limits_are_the_hardened_contract_values() {
    let limits = NbtWireLimits::new();
    assert_eq!(limits, NbtWireLimits::default());
    assert_eq!(limits.max_depth, 512);
    assert_eq!(limits.max_tags, 65_536);
    assert_eq!(limits.max_list_len, 65_536);
    assert_eq!(limits.max_array_bytes, 4 * 1024 * 1024);
    assert_eq!(NBT_WIRE_DEFAULT_MAX_TAGS, 65_536);
    assert_eq!(limits.effective_max_depth(), NBT_WIRE_HARD_MAX_DEPTH);
    assert_eq!(
        NbtWireLimits {
            max_depth: NBT_WIRE_HARD_MAX_DEPTH + 100,
            ..limits
        }
        .effective_max_depth(),
        NBT_WIRE_HARD_MAX_DEPTH
    );
}
