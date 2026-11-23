// SPDX-License-Identifier: LGPL-3.0-or-later
//! Adversarial wire-decoder regressions for the client NBT and chunk paths.

use megu_client::protocol::{decode_chunk_packet, write_varint, NbtValue, WireReader};

fn be_i32(value: i32) -> [u8; 4] {
    value.to_be_bytes()
}

fn varint(value: i32) -> Vec<u8> {
    let mut out = Vec::new();
    write_varint(&mut out, value);
    out
}

fn end_list(len: i32) -> Vec<u8> {
    let mut out = vec![9, 0];
    out.extend_from_slice(&be_i32(len));
    out
}

fn malicious_end_list_compound() -> Vec<u8> {
    let mut out = vec![10];
    for index in 0..60 {
        out.push(9);
        let name = if index == 0 { "aa" } else { "a" };
        out.extend_from_slice(&(name.len() as u16).to_be_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&end_list(1_048_576)[1..]);
    }
    out.push(0);
    assert_eq!(out.len(), 543);
    out
}

#[test]
fn zero_byte_end_list_compound_fails_fast() {
    let packet = malicious_end_list_compound();
    assert_eq!(packet.len(), 543);
    let err = WireReader::new(&packet)
        .read_nbt()
        .expect_err("TAG_End list bomb must be rejected");
    assert!(err.contains("TAG_End"), "{err}");
}

#[test]
fn end_element_list_requires_zero_length() {
    let hostile = end_list(1);
    let mut reader = WireReader::new(&hostile);
    assert!(reader.read_nbt().is_err());

    let value = WireReader::new(&end_list(0))
        .read_nbt()
        .expect("empty TAG_End list is valid");
    assert_eq!(value, NbtValue::List(Vec::new()));
}

#[test]
fn oversized_list_length_fails_before_element_walk() {
    let mut packet = vec![9, 1];
    packet.extend_from_slice(&be_i32(1_000_000));
    packet.extend_from_slice(&[0; 4]);
    assert_eq!(packet.len(), 10);
    let err = WireReader::new(&packet)
        .read_nbt()
        .expect_err("oversized list length must be rejected");
    assert!(err.contains("NBT list length"), "{err}");
}

#[test]
fn inflated_typed_array_lengths_fail_on_tiny_buffers() {
    for tag in [7, 11, 12] {
        let mut packet = vec![tag];
        packet.extend_from_slice(&be_i32(1_000_000));
        let err = WireReader::new(&packet)
            .read_nbt()
            .expect_err("inflated array length must be rejected");
        assert!(err.contains("requires"), "tag {tag}: {err}");
    }
}

#[test]
fn aggregate_nbt_tag_budget_rejects_fanout_bomb() {
    let mut packet = vec![9, 1];
    packet.extend_from_slice(&be_i32(300_000));
    packet.extend_from_slice(&[0; 300_000]);
    let err = WireReader::new(&packet)
        .read_nbt()
        .expect_err("tag fan-out must exhaust the aggregate budget");
    assert!(err.contains("budget"), "{err}");
}

fn chunk_body_with_section_payload(payload: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&be_i32(0));
    body.extend_from_slice(&be_i32(0));
    body.extend_from_slice(&[10, 0]); // empty heightmaps compound
    body.extend(varint(payload.len() as i32));
    body.extend_from_slice(payload);
    body
}

#[test]
fn inflated_chunk_section_count_and_data_length_fail_closed() {
    let tiny = chunk_body_with_section_payload(&[0]);

    let err = decode_chunk_packet(&tiny, 4096, 64)
        .expect_err("4096 sections must exceed the world-section cap");
    assert!(err.contains("section count"), "{err}");

    let err = decode_chunk_packet(&tiny, 256, 64)
        .expect_err("256 sections cannot fit in a one-byte section payload");
    assert!(err.contains("minimum"), "{err}");
}
