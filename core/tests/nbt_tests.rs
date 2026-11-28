// SPDX-License-Identifier: LGPL-3.0-or-later
//! Conformance tests for the canonical binary NBT codec (`megu_core::nbt`).
//!
//! The codec is shared by the client kernel and, later, the server's save /
//! template / component paths, so these tests pin the wire bytes rather than
//! only the round trip: an in-memory round trip passes just as happily over a
//! codec that mislabels a tag or writes a name Java would not.

use std::collections::BTreeMap;

use megu_core::nbt::{
    read_named, read_network_nbt, read_unnamed, write_named, write_network_nbt, write_unnamed,
    NbtString, Tag, MAX_NBT_DEPTH, MAX_NBT_TAGS, NBT_DEPTH_ERROR,
};

fn nbt(text: &str) -> NbtString {
    NbtString::text(text)
}

fn compound(fields: &[(&str, Tag)]) -> Tag {
    let mut values = BTreeMap::new();
    for (key, value) in fields {
        values.insert(nbt(key), value.clone());
    }
    Tag::Compound(values)
}

fn units(raw: &[u16]) -> NbtString {
    NbtString(raw.to_vec())
}

/// An anonymous root over an already-encoded payload: tag id then payload.
fn unnamed_doc(id: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![id];
    out.extend_from_slice(payload);
    out
}

/// An anonymous `TAG_String` whose length prefix and bytes are supplied raw, so
/// a test can feed encodings the writer would never produce.
fn string_doc(bytes: &[u8]) -> Vec<u8> {
    let mut out = vec![8u8];
    out.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
    out.extend_from_slice(bytes);
    out
}

/// An anonymous `TAG_List` with a caller-chosen element type and declared
/// length, so the declared count need not match the bytes that follow.
fn list_doc(kind: u8, declared: i32, elements: &[u8]) -> Vec<u8> {
    let mut out = vec![9u8, kind];
    out.extend_from_slice(&declared.to_be_bytes());
    out.extend_from_slice(elements);
    out
}

/// An anonymous compound from raw `(tag id, key, payload)` rows plus the end
/// tag, so a test can build shapes the writer refuses to emit.
fn compound_doc(rows: &[(u8, &[u8], &[u8])]) -> Vec<u8> {
    let mut out = vec![10u8];
    for (id, key, payload) in rows {
        out.push(*id);
        out.extend_from_slice(&(key.len() as u16).to_be_bytes());
        out.extend_from_slice(key);
        out.extend_from_slice(payload);
    }
    out.push(0);
    out
}

/// One value of every tag type, in id order, with shapes that survive the
/// canonicalization the writer applies (an empty list carries no element type).
fn every_value_tag() -> Vec<Tag> {
    vec![
        Tag::Byte(-1),
        Tag::Short(-2),
        Tag::Int(3),
        Tag::Long(-4_000_000_000),
        Tag::Float(1.5f32.to_bits()),
        Tag::Double((-2.25f64).to_bits()),
        Tag::ByteArray(vec![0, 1, 127, 128, 255]),
        Tag::String(units(&[0xD800, 0x0041])),
        Tag::List(3, vec![Tag::Int(1), Tag::Int(-2)]),
        compound(&[
            ("k", Tag::Int(7)),
            ("nested", compound(&[("x", Tag::Byte(1))])),
        ]),
        Tag::IntArray(vec![-1, 0, i32::MAX]),
        Tag::LongArray(vec![i64::MIN, 0, i64::MAX]),
        Tag::List(0, Vec::new()),
    ]
}

#[test]
fn the_java_type_table_is_pinned() {
    let ids: Vec<u8> = [
        Tag::End,
        Tag::Byte(0),
        Tag::Short(0),
        Tag::Int(0),
        Tag::Long(0),
        Tag::Float(0),
        Tag::Double(0),
        Tag::ByteArray(Vec::new()),
        Tag::String(nbt("")),
        Tag::List(0, Vec::new()),
        compound(&[]),
        Tag::IntArray(Vec::new()),
        Tag::LongArray(Vec::new()),
    ]
    .iter()
    .map(Tag::id)
    .collect();
    assert_eq!(ids, (0..=12).collect::<Vec<u8>>());
}

#[test]
fn every_tag_type_round_trips_unnamed_and_named() {
    for tag in every_value_tag() {
        let encoded = write_unnamed(&tag).expect("unnamed write");
        assert_eq!(encoded[0], tag.id(), "root byte is the tag id: {tag:?}");
        assert_eq!(read_unnamed(&encoded).expect("unnamed read"), tag);

        let name = nbt("root");
        let encoded = write_named(&name, &tag).expect("named write");
        assert_eq!(encoded[0], tag.id());
        assert_eq!(read_named(&encoded).expect("named read"), (name, tag));
    }
}

#[test]
fn end_is_a_root_of_its_own_but_never_a_value() {
    // A named root of TAG_End is the two-byte empty document: id 0, no payload.
    assert_eq!(
        write_named(&NbtString::default(), &Tag::End).unwrap(),
        vec![0]
    );
    assert_eq!(read_named(&[0]).unwrap(), (NbtString::default(), Tag::End));
    // The anonymous shape has no way to carry "no value", so both directions
    // refuse it instead of writing a byte the reader must reject.
    assert!(write_unnamed(&Tag::End).is_err());
    assert!(read_unnamed(&[0]).is_err());
    // A compound cannot hold an end value, and a list cannot hold end elements.
    let mut fields = BTreeMap::new();
    fields.insert(nbt("gone"), Tag::End);
    assert!(write_unnamed(&Tag::Compound(fields)).is_err());
    assert_eq!(
        write_unnamed(&Tag::List(1, vec![Tag::Byte(1), Tag::End])),
        Err("invalid NBT list elements")
    );
}

#[test]
fn empty_containers_use_the_canonical_wire_shape() {
    // An empty compound is id 10 then the end tag.
    assert_eq!(write_unnamed(&compound(&[])).unwrap(), vec![10, 0]);
    // An empty list carries element type 0 regardless of the element type the
    // caller asked for: vanilla's writer emits END for a zero-length list.
    assert_eq!(
        write_unnamed(&Tag::List(3, Vec::new())).unwrap(),
        vec![9, 0, 0, 0, 0, 0]
    );
    assert_eq!(
        read_unnamed(&[9, 0, 0, 0, 0, 0]).unwrap(),
        Tag::List(0, Vec::new())
    );
    // A named empty compound: id 10, zero-length name, end tag.
    assert_eq!(
        write_named(&NbtString::default(), &compound(&[])).unwrap(),
        vec![10, 0, 0, 0]
    );
}

#[test]
fn compound_keys_are_written_in_java_utf16_order() {
    // U+10000 is the surrogate pair D800 DC00 and U+E000 is the single unit
    // E000, so Java's UTF-16-unit order puts the astral key first even though
    // code-point order would put it last. A code-point sort here would write
    // the two keys the other way round and still round-trip, so pin the bytes.
    let mut values = BTreeMap::new();
    values.insert(nbt("\u{10000}"), Tag::Byte(1));
    values.insert(nbt("\u{E000}"), Tag::Byte(2));
    assert_eq!(
        write_unnamed(&Tag::Compound(values)).unwrap(),
        vec![
            10, // compound
            1, 0, 6, 0xED, 0xA0, 0x80, 0xED, 0xB0, 0x80, // TAG_Byte, U+10000
            1,    // value
            1, 0, 3, 0xEE, 0x80, 0x80, // TAG_Byte, U+E000
            2,    // value
            0,    // end
        ]
    );
}

#[test]
fn a_duplicate_compound_key_keeps_the_last_value() {
    let doc = compound_doc(&[(1, b"k", &[1]), (1, b"k", &[2])]);
    assert_eq!(
        read_unnamed(&doc).unwrap(),
        compound(&[("k", Tag::Byte(2))])
    );
}

#[test]
fn the_depth_cap_is_fail_closed_on_read_and_write() {
    let nested_bytes = |depth: usize| {
        let mut out = [10, 0, 0].repeat(depth);
        out.resize(out.len() + depth, 0);
        out
    };
    assert!(read_named(&nested_bytes(MAX_NBT_DEPTH)).is_ok());
    assert_eq!(
        read_named(&nested_bytes(MAX_NBT_DEPTH + 1)),
        Err(NBT_DEPTH_ERROR)
    );

    let nested_tag = |depth: usize| {
        let mut tag = compound(&[]);
        for _ in 1..depth {
            tag = compound(&[("x", tag)]);
        }
        tag
    };
    assert!(write_unnamed(&nested_tag(MAX_NBT_DEPTH)).is_ok());
    assert_eq!(
        write_unnamed(&nested_tag(MAX_NBT_DEPTH + 1)),
        Err(NBT_DEPTH_ERROR)
    );
    // The same cap applies to lists, not only compounds: the `TAG_List` reader
    // and writer each carry their own depth check.
    let nested_list = |depth: usize| {
        let mut tag = Tag::List(0, Vec::new());
        for _ in 1..depth {
            tag = Tag::List(9, vec![tag]);
        }
        tag
    };
    let deepest = nested_list(MAX_NBT_DEPTH);
    let encoded = write_unnamed(&deepest).expect("the cap allows its own depth");
    assert_eq!(read_unnamed(&encoded).unwrap(), deepest);
    assert_eq!(
        write_unnamed(&nested_list(MAX_NBT_DEPTH + 1)),
        Err(NBT_DEPTH_ERROR)
    );
    // A list element carries no tag id: its type is the parent's element
    // kind, so each frame is `kind + len`. 511 frames of `kind = 9, len = 1`
    // under the root, then an empty list at depth 511: readable. One more
    // frame pushes the innermost list to depth 512, which the reader refuses.
    let frames = |count: usize| {
        let mut out = vec![9u8];
        out.extend_from_slice(&[9, 0, 0, 0, 1].repeat(count - 1));
        out.extend_from_slice(&[0, 0, 0, 0, 0]);
        out
    };
    assert!(read_unnamed(&frames(MAX_NBT_DEPTH)).is_ok());
    assert_eq!(
        read_unnamed(&frames(MAX_NBT_DEPTH + 1)),
        Err(NBT_DEPTH_ERROR)
    );
}

#[test]
fn a_truncated_document_never_parses() {
    let doc = write_named(
        &nbt("root"),
        &compound(&[
            ("byte", Tag::Byte(7)),
            ("string", Tag::String(nbt("value"))),
            ("list", Tag::List(3, vec![Tag::Int(1), Tag::Int(2)])),
            ("longs", Tag::LongArray(vec![1, -1])),
        ]),
    )
    .unwrap();
    for cut in 0..doc.len() {
        assert!(
            read_named(&doc[..cut]).is_err(),
            "prefix of length {cut} parsed"
        );
    }
    assert!(read_named(&doc).is_ok());
    // Trailing bytes are rejected in every root shape.
    let mut padded = doc.clone();
    padded.push(0);
    assert_eq!(read_named(&padded), Err("trailing bytes after named NBT"));
    let unnamed = write_unnamed(&Tag::Int(5)).unwrap();
    let mut padded = unnamed.clone();
    padded.push(0);
    assert_eq!(
        read_unnamed(&padded),
        Err("trailing bytes after unnamed NBT")
    );
}

#[test]
fn the_tag_budget_is_fail_closed() {
    // One budget unit per tag: the list root plus its elements must fit under
    // MAX_NBT_TAGS, and the element count is the only thing that can exceed it.
    let element_count = MAX_NBT_TAGS - 1;
    let doc = list_doc(1, element_count as i32, &vec![0u8; element_count]);
    assert!(read_unnamed(&doc).is_ok());

    let too_many = MAX_NBT_TAGS;
    let doc = list_doc(1, too_many as i32, &vec![0u8; too_many]);
    assert_eq!(read_unnamed(&doc), Err("NBT exceeds tag count budget"));
}

#[test]
fn collection_lengths_are_bounded_before_any_allocation() {
    // A negative length is never valid, whatever the element size.
    assert_eq!(
        read_unnamed(&unnamed_doc(7, &[0xFF, 0xFF, 0xFF, 0xFF])),
        Err("invalid NBT collection length")
    );
    // A declared length larger than the bytes that follow cannot be honest.
    assert_eq!(
        read_unnamed(&unnamed_doc(7, &[0, 0, 0, 5, 1, 2, 3, 4])),
        Err("invalid NBT collection length")
    );
    assert_eq!(
        read_unnamed(&unnamed_doc(11, &[0, 0, 0, 2, 0, 0, 0, 1])),
        Err("invalid NBT collection length")
    );
    assert_eq!(
        read_unnamed(&unnamed_doc(12, &[0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0])),
        Err("invalid NBT collection length")
    );
    // The same holds for a list whose declared count outruns its elements.
    assert_eq!(
        read_unnamed(&list_doc(3, 2, &[0, 0, 0, 1])),
        Err("truncated NBT")
    );
    // An element type outside the table, and END elements in a non-empty list,
    // are both rejected: the wire has no such value.
    assert_eq!(
        read_unnamed(&list_doc(13, 0, &[])),
        Err("invalid NBT list element type")
    );
    assert_eq!(
        read_unnamed(&list_doc(0, 1, &[0])),
        Err("invalid NBT list element type")
    );
    assert_eq!(read_unnamed(&unnamed_doc(13, &[])), Err("unknown NBT tag"));
    assert_eq!(
        read_unnamed(&compound_doc(&[(13, b"k", &[])])),
        Err("unknown NBT compound tag")
    );
}

#[test]
fn writes_reject_lists_the_wire_cannot_carry() {
    assert_eq!(
        write_unnamed(&Tag::List(1, vec![Tag::Byte(1), Tag::Int(1)])),
        Err("invalid NBT list elements")
    );
    assert_eq!(
        write_unnamed(&Tag::List(13, Vec::new())),
        Err("invalid NBT list elements")
    );
    assert_eq!(
        write_named(&NbtString::default(), &Tag::List(1, vec![Tag::End])),
        Err("invalid NBT list elements")
    );
}

#[test]
fn modified_utf8_preserves_every_java_unit() {
    // NUL is the two-byte overlong form on output, exactly as writeUTF emits.
    assert_eq!(
        write_unnamed(&Tag::String(units(&[0]))).unwrap(),
        vec![8, 0, 2, 0xC0, 0x80]
    );
    // The reader also accepts the single zero byte DataInput.readUTF allows.
    assert_eq!(
        read_unnamed(&string_doc(&[0x00])).unwrap(),
        Tag::String(units(&[0]))
    );
    // Latin-1 and BMP scalars take the two- and three-byte forms.
    assert_eq!(
        write_unnamed(&Tag::String(nbt("\u{00E9}"))).unwrap(),
        vec![8, 0, 2, 0xC3, 0xA9]
    );
    assert_eq!(
        write_unnamed(&Tag::String(nbt("\u{20AC}"))).unwrap(),
        vec![8, 0, 3, 0xE2, 0x82, 0xAC]
    );
    // An astral scalar stays two surrogate units, never four-byte UTF-8.
    assert_eq!(
        write_unnamed(&Tag::String(nbt("\u{1F600}"))).unwrap(),
        vec![8, 0, 6, 0xED, 0xA0, 0xBD, 0xED, 0xB8, 0x80]
    );
    assert_eq!(nbt("\u{1F600}").units(), &[0xD83D, 0xDE00]);
    assert_eq!(nbt("\u{1F600}").to_utf8().unwrap(), "\u{1F600}");
    // A lone surrogate is preserved rather than replaced, and only the fallible
    // conversion refuses it.
    let lone = units(&[0xDC00]);
    let encoded = write_unnamed(&Tag::String(lone.clone())).unwrap();
    assert_eq!(read_unnamed(&encoded).unwrap(), Tag::String(lone.clone()));
    assert!(lone.to_utf8().is_err());
}

#[test]
fn modified_utf8_rejects_what_data_input_rejects() {
    // A four-byte UTF-8 lead, a bad continuation and both truncated forms.
    assert_eq!(
        read_unnamed(&string_doc(&[0xF0, 0x90, 0x80, 0x80])),
        Err("invalid modified UTF-8 leading byte")
    );
    assert_eq!(
        read_unnamed(&string_doc(&[0xC3, 0x28])),
        Err("invalid modified UTF-8 continuation")
    );
    assert_eq!(
        read_unnamed(&string_doc(&[0xE2, 0x82])),
        Err("truncated modified UTF-8")
    );
    assert_eq!(
        read_unnamed(&string_doc(&[0xC3])),
        Err("truncated modified UTF-8")
    );
    assert_eq!(
        read_unnamed(&string_doc(&[0xE2, 0x28, 0xAC])),
        Err("invalid modified UTF-8 continuation")
    );
    // A declared string length past the end of the input is truncation, not a
    // request to read uninitialized bytes.
    assert_eq!(
        read_unnamed(&unnamed_doc(8, &[0, 4, b'a', b'b'])),
        Err("truncated NBT")
    );
}

#[test]
fn modified_utf8_length_is_capped_at_the_u16_prefix() {
    // 21_845 three-byte units are exactly 65_535 bytes and fit the u16 prefix.
    let fits = units(&vec![0x4E00u16; 21_845]);
    let encoded = write_unnamed(&Tag::String(fits.clone())).unwrap();
    assert_eq!(&encoded[1..3], &[0xFF, 0xFF]);
    assert_eq!(read_unnamed(&encoded).unwrap(), Tag::String(fits));

    // One more unit cannot be described by a u16 length, so the write fails
    // closed instead of emitting a truncated prefix.
    let too_long = units(&vec![0x4E00u16; 21_846]);
    assert_eq!(
        write_unnamed(&Tag::String(too_long)),
        Err("modified UTF-8 string exceeds 65535 bytes")
    );
}

#[test]
fn floats_keep_their_bits_and_nans_are_canonical() {
    // Every NaN collapses to the canonical quiet pattern, so a save/load cycle
    // over a document cannot change its bytes or its digest.
    for bits in [f32::NAN.to_bits(), 0x7F80_0001, 0xFFC0_0001] {
        assert_eq!(
            write_unnamed(&Tag::Float(bits)).unwrap(),
            vec![5, 0x7F, 0xC0, 0x00, 0x00]
        );
    }
    for bits in [
        f64::NAN.to_bits(),
        0x7FF0_0000_0000_0001,
        0xFFF8_0000_0000_0001,
    ] {
        assert_eq!(
            write_unnamed(&Tag::Double(bits)).unwrap(),
            vec![6, 0x7F, 0xF8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
        );
    }
    // Negative zero and the infinities are values, not NaNs: their bits pass
    // through untouched.
    for bits in [
        (-0.0f32).to_bits(),
        f32::INFINITY.to_bits(),
        1.0f32.to_bits(),
    ] {
        let encoded = write_unnamed(&Tag::Float(bits)).unwrap();
        assert_eq!(encoded, [vec![5], bits.to_be_bytes().to_vec()].concat());
        assert_eq!(read_unnamed(&encoded).unwrap(), Tag::Float(bits));
    }
    for bits in [
        (-0.0f64).to_bits(),
        f64::NEG_INFINITY.to_bits(),
        (-2.25f64).to_bits(),
    ] {
        let encoded = write_unnamed(&Tag::Double(bits)).unwrap();
        assert_eq!(encoded, [vec![6], bits.to_be_bytes().to_vec()].concat());
        assert_eq!(read_unnamed(&encoded).unwrap(), Tag::Double(bits));
    }
}

#[test]
fn names_round_trip_including_the_empty_and_astral_ones() {
    let tag = Tag::Int(42);
    for name in [NbtString::default(), nbt("root"), nbt("\u{1F600}")] {
        let encoded = write_named(&name, &tag).unwrap();
        let (read_name, read_tag) = read_named(&encoded).unwrap();
        assert_eq!((read_name, read_tag), (name, tag.clone()));
    }
    // The empty name is written as a zero u16 length, not omitted.
    assert_eq!(
        write_named(&NbtString::default(), &tag).unwrap(),
        vec![3, 0, 0, 0, 0, 0, 42]
    );
}

#[test]
fn the_network_cursor_leaves_the_rest_of_the_packet_alone() {
    let tag = compound(&[("level", Tag::Int(1))]);
    let mut wire = write_network_nbt(&tag).unwrap();
    wire.extend_from_slice(&[0xAA, 0xBB]);
    let mut input: &[u8] = &wire;
    assert_eq!(read_network_nbt(&mut input).unwrap(), tag);
    assert_eq!(input, &[0xAA, 0xBB]);

    // The network root is compound-only in both directions: a bare scalar has
    // no place in a registry payload.
    let mut input: &[u8] = &[3, 0, 0, 0, 1];
    assert_eq!(
        read_network_nbt(&mut input),
        Err("network registry NBT root must be a compound")
    );
    assert_eq!(input, &[3, 0, 0, 0, 1], "a refused read consumes nothing");
    assert_eq!(
        write_network_nbt(&Tag::Int(1)),
        Err("network registry NBT root must be a compound")
    );
    assert_eq!(write_network_nbt(&compound(&[])).unwrap(), vec![10, 0]);
}

#[test]
fn accessors_answer_only_for_their_own_tag_type() {
    let tag = compound(&[
        ("int", Tag::Int(-7)),
        ("name", Tag::String(nbt("megu"))),
        ("list", Tag::List(1, vec![Tag::Byte(1), Tag::Byte(2)])),
    ]);
    assert_eq!(tag.get("int").and_then(Tag::int), Some(-7));
    assert_eq!(tag.get("int").and_then(Tag::string), None);
    assert_eq!(
        tag.get("name").and_then(Tag::string).map(NbtString::units),
        Some(&[109, 101, 103, 117][..])
    );
    assert_eq!(
        tag.get("list").and_then(Tag::list),
        Some(&[Tag::Byte(1), Tag::Byte(2)][..])
    );
    assert_eq!(tag.get("missing"), None);
    assert_eq!(tag.get("int").and_then(Tag::list), None);
    assert_eq!(Tag::Int(1).get("int"), None);
    assert_eq!(Tag::Int(1).list(), None);
    // The `NbtString` conveniences keep their Java semantics.
    assert!(NbtString::default().is_empty());
    assert_eq!(NbtString::from("ab").units(), &[0x61, 0x62]);
    assert_eq!(NbtString::from(String::from("ab")), nbt("ab"));
    assert_eq!(NbtString::from(vec![0x61u16]), nbt("a"));
    assert_eq!(Tag::ByteArray(vec![1]).int(), None);
}
