// SPDX-License-Identifier: LGPL-3.0-or-later

use megu_core::block_delta::{
    diff_sections, region_merkle_root, section_merkle_leaf, transfer_conserved, BlockEdit,
    ConservationError, ConservationPort, DeltaCoreError, SectionBlockIndex, SectionDelta,
    SectionDeltaTxn,
};
use megu_core::coords::BlockPos;
use megu_core::crypto::hex_encode;
use megu_core::random::LegacyRandom;
use megu_core::wire_envelope::{
    decode_envelope, encode_envelope, BoundedCursor, BoundedWriter, EnvelopeError, EnvelopeSpec,
};

const SPEC: EnvelopeSpec = EnvelopeSpec {
    magic: *b"TEST",
    schema_version: 0x0102,
    max_payload_bytes: 128,
};

#[test]
fn envelope_matches_wire_fixture_and_borrows_payload() {
    let payload = [0x00, 0x80, 0xff];
    let fixture = [
        b'T', b'E', b'S', b'T', 0x01, 0x02, 0, 0, 0, 3, 0x00, 0x80, 0xff, 0x6e, 0x4e, 0x8d, 0x84,
    ];
    assert_eq!(encode_envelope(SPEC, &payload).unwrap(), fixture);
    let decoded = decode_envelope(SPEC, &fixture).unwrap();
    assert_eq!(decoded, payload);
    assert_eq!(decoded.as_ptr(), fixture[10..].as_ptr());
}

#[test]
fn envelope_rejects_truncation_corruption_and_invalid_lengths() {
    let mut rng = LegacyRandom::new(0x7f2d_381a);
    for len in 0..=SPEC.max_payload_bytes {
        let payload: Vec<_> = (0..len).map(|_| rng.next_int() as u8).collect();
        let frame = encode_envelope(SPEC, &payload).unwrap();
        assert_eq!(decode_envelope(SPEC, &frame).unwrap(), payload);
        for cut in 0..frame.len() {
            assert_eq!(
                decode_envelope(SPEC, &frame[..cut]),
                Err(EnvelopeError::Truncated)
            );
        }
        let mut corrupt = frame.clone();
        for offset in 0..frame.len() {
            for bit in 0..8 {
                corrupt[offset] ^= 1 << bit;
                assert!(
                    decode_envelope(SPEC, &corrupt).is_err(),
                    "bit {bit} in byte {offset} was accepted"
                );
                corrupt[offset] ^= 1 << bit;
            }
        }
        corrupt.push(0);
        assert_eq!(
            decode_envelope(SPEC, &corrupt),
            Err(EnvelopeError::TrailingBytes)
        );

        let noise: Vec<_> = (0..len).map(|_| rng.next_int() as u8).collect();
        assert!(decode_envelope(SPEC, &noise).is_err());
    }

    let mut frame = encode_envelope(SPEC, &[1, 2, 3]).unwrap();
    let older = EnvelopeSpec {
        schema_version: 1,
        ..SPEC
    };
    assert_eq!(
        decode_envelope(older, &frame),
        Err(EnvelopeError::SchemaVersionMismatch {
            expected: 1,
            actual: SPEC.schema_version,
        })
    );
    frame[10] ^= 1;
    assert_eq!(
        decode_envelope(SPEC, &frame),
        Err(EnvelopeError::CrcMismatch)
    );
    for claimed in [0, 2, 4, 128, 129, u32::MAX] {
        frame[6..10].copy_from_slice(&claimed.to_be_bytes());
        let expected = match claimed {
            0 | 2 => EnvelopeError::TrailingBytes,
            4 | 128 => EnvelopeError::Truncated,
            _ => EnvelopeError::LimitExceeded {
                limit: SPEC.max_payload_bytes,
            },
        };
        assert_eq!(decode_envelope(SPEC, &frame), Err(expected));
    }
    assert_eq!(
        encode_envelope(SPEC, &[0; 129]),
        Err(EnvelopeError::LimitExceeded { limit: 128 })
    );
}

#[test]
fn cursor_and_writer_use_big_endian_and_keep_bounds_on_error() {
    let mut writer = BoundedWriter::new(26);
    writer.write_u8(0x80).unwrap();
    writer.write_u16(0x1234).unwrap();
    writer.write_u32(0x89ab_cdef).unwrap();
    writer.write_i32(-2).unwrap();
    writer.write_u64(0x0123_4567_89ab_cdef).unwrap();
    assert_eq!(
        writer.write_str("水", 2),
        Err(EnvelopeError::LimitExceeded { limit: 2 })
    );
    writer.write_str("水", 3).unwrap();
    assert_eq!(
        writer.write_bytes(&[0]),
        Err(EnvelopeError::LimitExceeded { limit: 26 })
    );
    let bytes = writer.into_inner();
    assert_eq!(
        bytes,
        [
            0x80, 0x12, 0x34, 0x89, 0xab, 0xcd, 0xef, 0xff, 0xff, 0xff, 0xfe, 0x01, 0x23, 0x45,
            0x67, 0x89, 0xab, 0xcd, 0xef, 0, 0, 0, 3, 0xe6, 0xb0, 0xb4,
        ]
    );

    let mut cursor = BoundedCursor::new(&bytes);
    assert_eq!(cursor.finish_empty(), Err(EnvelopeError::TrailingBytes));
    assert_eq!(cursor.read_bytes(usize::MAX), Err(EnvelopeError::Truncated));
    assert_eq!(cursor.read_u8().unwrap(), 0x80);
    assert_eq!(cursor.read_u16().unwrap(), 0x1234);
    assert_eq!(cursor.read_u32().unwrap(), 0x89ab_cdef);
    assert_eq!(cursor.read_i32().unwrap(), -2);
    assert_eq!(cursor.read_u64().unwrap(), 0x0123_4567_89ab_cdef);
    assert_eq!(cursor.read_u64(), Err(EnvelopeError::Truncated));
    assert_eq!(
        cursor.read_str(2),
        Err(EnvelopeError::LimitExceeded { limit: 2 })
    );
    assert_eq!(cursor.read_str(3).unwrap(), "水");
    cursor.finish_empty().unwrap();

    let mut writer = BoundedWriter::new(4);
    assert_eq!(
        writer.write_str("a", 1),
        Err(EnvelopeError::LimitExceeded { limit: 4 })
    );
    writer.write_u32(1).unwrap();
    assert_eq!(writer.as_slice(), [0, 0, 0, 1]);

    let invalid = [0, 0, 0, 1, 0xff];
    let mut cursor = BoundedCursor::new(&invalid[..4]);
    assert_eq!(cursor.read_str(1), Err(EnvelopeError::Truncated));
    assert_eq!(cursor.read_u32().unwrap(), 1);
    let mut cursor = BoundedCursor::new(&invalid);
    assert_eq!(cursor.read_str(1), Err(EnvelopeError::InvalidUtf8));
    assert_eq!(cursor.read_bytes(5).unwrap(), invalid);
}

#[test]
fn net_no_op_cancels_when_section_deltas_compose() {
    let a = [0; 4096];
    let mut b = a;
    b[40] = 7;
    b[70] = 2;
    let mut c = b;
    c[40] = 0;
    c[70] = 3;
    c[4095] = 1;
    let composed = SectionDelta::compose(&diff_sections(&a, &b), &diff_sections(&b, &c)).unwrap();
    assert_eq!(composed, diff_sections(&a, &c));
    let mut actual = a;
    composed.apply_to_section(&mut actual).unwrap();
    assert_eq!(actual, c);
    composed.invert().apply_to_section(&mut actual).unwrap();
    assert_eq!(actual, a);
}

#[test]
fn section_index_round_trips_every_coordinate_and_rejects_invalid_inputs() {
    let mut raw = 0;
    for y in 0..16 {
        for z in 0..16 {
            for x in 0..16 {
                let idx = SectionBlockIndex::from_xyz(x, y, z).unwrap();
                assert_eq!(idx, SectionBlockIndex::from_raw(raw).unwrap());
                assert_eq!(idx.xyz(), (x, y, z));
                assert_eq!(
                    idx.as_usize(),
                    BlockPos::new(x.into(), y.into(), z.into()).local_index()
                );
                raw += 1;
            }
        }
    }
    for raw in 4096..=u16::MAX {
        assert_eq!(
            SectionBlockIndex::from_raw(raw),
            Err(DeltaCoreError::IndexOutOfRange { raw })
        );
    }
    for n in 16..=u8::MAX {
        for (x, y, z) in [(n, 0, 0), (0, n, 0), (0, 0, n)] {
            assert_eq!(
                SectionBlockIndex::from_xyz(x, y, z),
                Err(DeltaCoreError::CoordinatesOutOfRange { x, y, z })
            );
        }
    }
}

#[test]
fn section_delta_is_associative_for_a_compatible_timeline() {
    let a = std::array::from_fn(|i| i as u32 % 17);
    let b = std::array::from_fn(|i| if i % 2 == 0 { 12 } else { a[i] });
    let c = std::array::from_fn(|i| if i % 3 == 0 { 7 } else { b[i] });
    let d = std::array::from_fn(|i| if i % 5 == 0 { a[i] } else { c[i] });
    let ab = diff_sections(&a, &b);
    let bc = diff_sections(&b, &c);
    let cd = diff_sections(&c, &d);
    let ac = SectionDelta::compose(&ab, &bc).unwrap();
    let bd = SectionDelta::compose(&bc, &cd).unwrap();
    let ad = diff_sections(&a, &d);

    assert_eq!(SectionDelta::compose(&ac, &cd).unwrap(), ad);
    assert_eq!(SectionDelta::compose(&ab, &bd).unwrap(), ad);
    let identity = SectionDelta::compose(&ad, &ad.invert()).unwrap();
    assert!(identity.is_empty());
    assert_eq!(SectionDelta::compose(&identity, &ad).unwrap(), ad);
    assert_eq!(SectionDelta::compose(&ad, &identity).unwrap(), ad);
    let mut actual = a;
    ad.apply_to_section(&mut actual).unwrap();
    assert_eq!(actual, d);
    ad.invert().apply_to_section(&mut actual).unwrap();
    assert_eq!(actual, a);
}

fn edit(raw: u16, prev_state: u32, next_state: u32) -> BlockEdit {
    BlockEdit {
        idx: SectionBlockIndex::from_raw(raw).unwrap(),
        prev_state,
        next_state,
    }
}

#[test]
fn delta_normalizes_an_edit_stream_and_rejects_discontinuity() {
    let edits = [
        edit(4095, 0, 1),
        edit(2, 0, 3),
        edit(1, 3, 3),
        edit(4095, 1, 0),
        edit(2, 3, 4),
        edit(3, 7, 8),
    ];
    assert_eq!(
        SectionDelta::from_edits(edits).unwrap().edits(),
        &[edit(2, 0, 4), edit(3, 7, 8)]
    );
    let wrong = edit(4095, 7, 8);
    assert_eq!(
        SectionDelta::from_edits(edits.into_iter().chain([wrong])),
        Err(DeltaCoreError::StateMismatch {
            idx: wrong.idx,
            expected: 7,
            actual: 0,
        })
    );
}

#[test]
fn delta_mismatch_prevents_every_write() {
    let first = SectionDelta::from_edits([edit(1, 0, 4), edit(4095, 2, 3)]).unwrap();
    let last = edit(4095, 2, 3).idx;
    let original = [0; 4096];
    let mut actual = original;
    assert_eq!(
        first.apply_to_section(&mut actual),
        Err(DeltaCoreError::StateMismatch {
            idx: last,
            expected: 2,
            actual: 0,
        })
    );
    assert_eq!(actual, original);

    let next = SectionDelta::from_edits([edit(4095, 5, 7)]).unwrap();
    assert_eq!(
        SectionDelta::compose(&first, &next),
        Err(DeltaCoreError::StateMismatch {
            idx: last,
            expected: 5,
            actual: 3,
        })
    );
}

#[test]
fn merkle_hashes_pin_state_order_child_order_and_domain() {
    let mut section = [0; 4096];
    section[0] = 0x0123_4567;
    section[4095] = 0x89ab_cdef;
    let leaf = section_merkle_leaf(&section);
    assert_eq!(
        hex_encode(&leaf),
        "29468dae55c1143f222c95fa991a37c5d1ab68c9b1a514497d3fff2e18e9cdcc"
    );
    let other = [0x42; 32];
    let root = region_merkle_root(b"chunk", &[leaf, other]);
    assert_eq!(
        hex_encode(&root),
        "60d86429662cf30c145b0045c4e4bef1a986214fa5a18c5231d3ae2def070af8"
    );
    assert_ne!(root, region_merkle_root(b"chunk", &[other, leaf]));
    assert_ne!(root, region_merkle_root(b"region", &[leaf, other]));
    let mut longer_domain = [0; 33];
    longer_domain[0] = b'a';
    assert_ne!(
        region_merkle_root(b"a", &[[0; 32]]),
        region_merkle_root(&longer_domain, &[])
    );
    let row = [root; 32 * 32];
    let full_region = region_merkle_root(b"region", &row);
    let mut changed = row;
    changed[1023] = leaf;
    assert_ne!(full_region, region_merkle_root(b"region", &changed));
}

#[test]
fn transaction_rolls_back_on_drop_and_certifies_a_commit() {
    let original = [10; 4096];
    let mut section = original;
    let first = edit(123, 10, 42);
    let second = edit(2, 10, 99);
    {
        let mut txn = SectionDeltaTxn::new(&mut section);
        txn.stage_edit(first).unwrap();
        txn.stage_edit(second).unwrap();
        assert!(txn.stage_edit(first).is_err());
    }
    assert_eq!(section, original);

    let mut txn = SectionDeltaTxn::new(&mut section);
    txn.stage_edit(first).unwrap();
    txn.stage_edit(second).unwrap();
    txn.stage_edit(edit(123, 42, 10)).unwrap();
    txn.stage_edit(edit(2, 99, 75)).unwrap();
    txn.stage_edit(edit(0, 10, 30)).unwrap();
    let committed = txn.commit();
    assert_eq!(
        committed.delta().edits(),
        &[edit(0, 10, 30), edit(2, 10, 75)]
    );
    let mut expected = original;
    expected[0] = 30;
    expected[2] = 75;
    assert_eq!(section, expected);
    assert_eq!(committed.leaf_hash(), section_merkle_leaf(&section));
}

struct Tank {
    amount: u64,
    capacity: u64,
    extract_extra: u64,
    insert_extra: u64,
}

impl Tank {
    fn new(amount: u64, capacity: u64) -> Self {
        Self {
            amount,
            capacity,
            extract_extra: 0,
            insert_extra: 0,
        }
    }
}

impl ConservationPort<&str> for Tank {
    fn simulate_extract(&self, key: &&str, requested: u64) -> u64 {
        self.total_amount(key).min(requested)
    }

    fn commit_extract(&mut self, key: &&str, amount: u64) {
        assert_eq!(*key, "water");
        assert!(amount > 0);
        self.amount -= amount + self.extract_extra;
    }

    fn simulate_insert(&self, key: &&str, offered: u64) -> u64 {
        if *key == "water" {
            (self.capacity - self.amount).min(offered)
        } else {
            0
        }
    }

    fn commit_insert(&mut self, key: &&str, amount: u64) {
        assert_eq!(*key, "water");
        assert!(amount > 0);
        self.amount += amount + self.insert_extra;
    }

    fn total_amount(&self, key: &&str) -> u64 {
        if *key == "water" {
            self.amount
        } else {
            0
        }
    }
}

#[test]
fn transfer_reports_a_port_that_creates_resources() {
    let mut from = Tank::new(40, 80);
    let mut to = Tank::new(0, 80);
    to.insert_extra = 1;
    assert_eq!(
        transfer_conserved(&mut from, &mut to, &"water", 10),
        Err(ConservationError::ConservationViolation)
    );
}

#[test]
fn transfer_moves_only_the_mutually_supported_amount() {
    let mut from = Tank::new(40, 80);
    let mut to = Tank::new(50, 65);
    assert_eq!(transfer_conserved(&mut from, &mut to, &"water", 10), Ok(10));
    assert_eq!((from.amount, to.amount), (30, 60));
    assert_eq!(transfer_conserved(&mut from, &mut to, &"water", 20), Ok(5));
    assert_eq!((from.amount, to.amount), (25, 65));
    assert_eq!(transfer_conserved(&mut from, &mut to, &"water", 20), Ok(0));
    assert_eq!((from.amount, to.amount), (25, 65));

    let mut empty = Tank::new(0, 30);
    assert_eq!(
        transfer_conserved(&mut from, &mut empty, &"water", 0),
        Ok(0)
    );
    assert_eq!(
        transfer_conserved(&mut from, &mut empty, &"lava", 10),
        Ok(0)
    );
    assert_eq!(
        transfer_conserved(&mut empty, &mut from, &"water", 1),
        Ok(0)
    );
    assert_eq!((from.amount, empty.amount), (25, 0));
    assert_eq!(
        transfer_conserved(&mut from, &mut empty, &"water", 30),
        Ok(25)
    );
    assert_eq!((from.amount, empty.amount), (0, 25));
}

#[test]
fn transfer_checks_exact_amount_and_supports_wide_totals() {
    let mut from = Tank::new(u64::MAX, u64::MAX);
    let mut to = Tank::new(u64::MAX - 2, u64::MAX);
    assert_eq!(transfer_conserved(&mut from, &mut to, &"water", 6), Ok(2));
    assert_eq!((from.amount, to.amount), (u64::MAX - 2, u64::MAX));

    let mut from = Tank::new(40, 80);
    let mut to = Tank::new(0, 80);
    from.extract_extra = 1;
    to.insert_extra = 1;
    assert_eq!(
        transfer_conserved(&mut from, &mut to, &"water", 10),
        Err(ConservationError::ConservationViolation)
    );
    assert_eq!(from.amount + to.amount, 40);
}
