// SPDX-License-Identifier: LGPL-3.0-or-later
//! Behaviour pinning for [`megu_core::paletted`] — the canonical protocol-767
//! compact section the client kernel used to splice out of the server source
//! tree at build time.
//!
//! Every assertion goes through the production API. The expected byte streams
//! are written from the protocol definition (bit-width byte, VarInt palette,
//! VarInt word count, big-endian non-straddling words), never from the
//! implementation's own helpers.

use megu_core::paletted::{is_air_state, PalettedSection, SectionLightClass};

const CELLS: usize = 4096;

fn never(_: u32) -> bool {
    false
}

fn pack(cells: &[u32; CELLS]) -> PalettedSection {
    PalettedSection::pack(cells, &never, &never)
}

/// Cells cycling through `distinct` states: `state = (i * 7 % distinct) * 3`.
fn mixed(distinct: usize) -> [u32; CELLS] {
    std::array::from_fn(|i| ((i * 7) % distinct) as u32 * 3)
}

fn unpack(section: &PalettedSection) -> [u32; CELLS] {
    let mut cells = [0; CELLS];
    section.unpack_into(&mut cells);
    cells
}

/// VarInt encoder written from the protocol definition, independent of
/// `megu_core::varint`.
fn push_varint(out: &mut Vec<u8>, value: u32) {
    let mut rest = value;
    while rest >= 0x80 {
        out.push((rest as u8 & 0x7f) | 0x80);
        rest >>= 7;
    }
    out.push(rest as u8);
}

#[test]
fn air_state_ids_are_the_pinned_vanilla_ones() {
    for state in [0u32, 12_958, 12_959] {
        assert!(is_air_state(state), "state {state} must count as air");
    }
    for state in [1u32, 12_957, 12_960, 26_683] {
        assert!(!is_air_state(state), "state {state} must not count as air");
    }
    assert_eq!(PalettedSection::single(0, false, false).non_air(), 0);
    assert_eq!(PalettedSection::single(12_959, false, false).non_air(), 0);
    assert_eq!(PalettedSection::single(1, false, false).non_air(), 4096);
}

#[test]
fn uniform_section_is_zero_bit_wire_and_copy_free_views() {
    let section = PalettedSection::single(1, false, false);
    assert_eq!(section.palette_bits(), 0);
    assert_eq!(section.uniform_state(), Some(1));
    assert_eq!(section.is_uniform(), Some(1));
    assert_eq!(section.non_air(), 4096);
    assert!(section.is_clean_wire());
    assert_eq!(
        section.approx_bytes(),
        std::mem::size_of::<PalettedSection>()
    );
    assert_eq!((section.get(0), section.get(4095)), (1, 1));
    assert_eq!(unpack(&section), [1; CELLS]);
    assert!(!section.any_palette_entry(|state| state == 2));
    assert!(section.any_palette_entry(|state| state == 1));

    let mut wire = Vec::new();
    section.write_block_states_wire(&mut wire);
    assert_eq!(wire, [0, 1, 0]);

    // A two-byte state VarInt, and the air accounting of the cave-air pin.
    let cave_air = PalettedSection::single(12_958, false, false);
    assert_eq!(cave_air.non_air(), 0);
    let mut wire = Vec::new();
    cave_air.write_block_states_wire(&mut wire);
    assert_eq!(wire, [0, 0x9e, 0x65, 0]);
}

#[test]
fn uniform_cells_collapse_to_the_single_variant() {
    let cells = [7; CELLS];
    let section = pack(&cells);
    assert_eq!(section.palette_bits(), 0);
    assert_eq!(section.uniform_state(), Some(7));
    let mut wire = Vec::new();
    section.write_block_states_wire(&mut wire);
    assert_eq!(wire, [0, 7, 0]);
    assert_eq!(unpack(&section), cells);
}

#[test]
fn two_state_pack_is_four_bit_nibbles_in_big_endian_words() {
    // Even cells hold state 5 (palette index 0), odd cells state 7 (index 1).
    let cells: [u32; CELLS] = std::array::from_fn(|i| if i % 2 == 0 { 5 } else { 7 });
    let section = pack(&cells);
    assert_eq!(section.palette_bits(), 4);
    assert_eq!(section.uniform_state(), None);
    assert!(section.is_uniform().is_none());
    assert_eq!(section.non_air(), 4096);
    assert_eq!(unpack(&section), cells);

    let mut wire = Vec::new();
    section.write_block_states_wire(&mut wire);
    let mut expected = vec![4u8];
    push_varint(&mut expected, 2);
    push_varint(&mut expected, 5);
    push_varint(&mut expected, 7);
    push_varint(&mut expected, 256);
    // 16 cells per word: nibble 0 is palette index 0, nibble 1 is index 1, so
    // every word is 0x1010_1010_1010_1010 and no cell is ever split.
    expected.extend(std::iter::repeat_n(0x10u8, 256 * 8));
    assert_eq!(wire, expected);
    assert_eq!(&wire[wire.len() - 8..], [0x10u8; 8].as_slice());
}

#[test]
fn five_bit_pack_zero_pads_the_tail_word() {
    let cells = mixed(17);
    let section = pack(&cells);
    assert_eq!(section.palette_bits(), 5);
    assert_eq!(unpack(&section), cells);

    let mut wire = Vec::new();
    section.write_block_states_wire(&mut wire);
    assert_eq!(wire[0], 5);
    // 12 cells per word: 4096 = 341 * 12 + 4, so the 342nd word carries only
    // four cells (20 bits) and the remaining 44 bits must stay zero.
    let words = 4096usize.div_ceil(12);
    assert_eq!(words, 342);
    let tail = &wire[wire.len() - 8..];
    let tail = u64::from_be_bytes(tail.try_into().expect("tail word"));
    assert_eq!(tail >> 20, 0, "tail padding must be zero");
}

#[test]
fn fifteen_bit_pack_has_no_local_palette_and_keeps_cell_values() {
    let cells = mixed(257);
    let section = pack(&cells);
    assert_eq!(section.palette_bits(), 15);
    assert_eq!(unpack(&section), cells);

    let mut wire = Vec::new();
    section.write_block_states_wire(&mut wire);
    assert_eq!(wire[0], 15);
    let mut header = Vec::new();
    push_varint(&mut header, 1024);
    assert_eq!(&wire[1..1 + header.len()], header.as_slice());
    assert_eq!(wire.len(), 1 + header.len() + 1024 * 8);

    // Four 15-bit cells per word, least significant cell first, no local
    // palette to map through.
    let first = u64::from(cells[0])
        | u64::from(cells[1]) << 15
        | u64::from(cells[2]) << 30
        | u64::from(cells[3]) << 45;
    let start = 1 + header.len();
    let word = u64::from_be_bytes((&wire[start..start + 8]).try_into().expect("first word"));
    assert_eq!(word, first);
    let last = u64::from(cells[4092])
        | u64::from(cells[4093]) << 15
        | u64::from(cells[4094]) << 30
        | u64::from(cells[4095]) << 45;
    let word = u64::from_be_bytes((&wire[wire.len() - 8..]).try_into().expect("last word"));
    assert_eq!(word, last);
}

#[test]
fn set_never_reclaims_a_palette_entry_and_reports_unclean_wire() {
    let cells: [u32; CELLS] = std::array::from_fn(|i| if i % 2 == 0 { 5 } else { 7 });
    let mut section = pack(&cells);
    assert!(section.is_clean_wire());
    section.set(1, 9, &never, &never);
    assert!(!section.is_clean_wire());
    assert_eq!(section.get(1), 9);
    // The stale entry survives, so the written bytes are no longer canonical.
    assert!(section.any_palette_entry(|state| state == 7));
    let mut stale = Vec::new();
    section.write_block_states_wire(&mut stale);

    let mut edited = cells;
    edited[1] = 9;
    let mut canonical = Vec::new();
    pack(&edited).write_block_states_wire(&mut canonical);
    assert_ne!(stale, canonical);
    // Same cells, so the encoded width agrees even though the bytes do not.
    assert_eq!(stale[0], canonical[0]);
    assert_eq!(unpack(&section), edited);
}

#[test]
fn set_promotes_every_indirect_width_then_the_direct_form() {
    let mut section = PalettedSection::single(0, false, false);
    // Palette index 0 is the initial state 0; each written state adds one.
    for state in 1..=256u32 {
        section.set(state as usize - 1, state, &never, &never);
        assert_eq!(section.get(state as usize - 1), state);
        let entries = state as usize + 1;
        let expected = match entries {
            0..=16 => 4,
            17..=32 => 5,
            33..=64 => 6,
            65..=128 => 7,
            129..=256 => 8,
            _ => 15,
        };
        assert_eq!(
            section.palette_bits(),
            expected,
            "width after {entries} palette entries"
        );
    }
    assert_eq!(section.palette_bits(), 15);
    let cells = unpack(&section);
    assert_eq!(cells[0], 1);
    assert_eq!(cells[255], 256);
    assert_eq!(cells[256], 0);
    assert_eq!(section.non_air(), 256);
}

#[test]
fn indirect_section_edited_to_uniform_matches_the_single_variant_bytes() {
    let mut section = PalettedSection::single(5, false, false);
    section.set(0, 7, &never, &never);
    assert_eq!(section.is_uniform(), None);
    assert_eq!(section.uniform_state(), None);

    section.set(0, 5, &never, &never);
    // The palette still holds the unused 7 entry, but every cell is 5 again.
    assert_eq!(section.is_uniform(), Some(5));
    assert_eq!(section.uniform_state(), None);
    assert!(section.any_palette_entry(|state| state == 7));
    let mut wire = Vec::new();
    section.write_block_states_wire(&mut wire);
    assert_eq!(wire[0], 4);
    assert_eq!(unpack(&section), [5; CELLS]);
}

#[test]
fn hidden_and_palette_queries_answer_different_questions() {
    let hidden = |state: u32| state == 7;
    let mut cells = [5; CELLS];
    cells[0] = 7;
    let mut section = PalettedSection::pack(&cells, &never, &hidden);
    assert!(section.has_hidden());
    assert!(section.any_palette_entry(|state| state == 7));

    // Clearing the only hidden cell drops the flag, while the stale palette
    // entry keeps answering the palette query.
    section.set(0, 5, &never, &hidden);
    assert!(!section.has_hidden());
    assert!(section.any_palette_entry(|state| state == 7));

    let mut additive = PalettedSection::pack(&cells, &never, &hidden);
    additive.set(1, 9, &never, &hidden);
    assert!(additive.has_hidden());
    assert!(additive.any_palette_entry(|state| state == 9));
}

#[test]
fn light_class_tracks_the_opacity_predicate_and_air_counts() {
    let opaque = |state: u32| state != 5;
    let mixed_cells: [u32; CELLS] = std::array::from_fn(|i| if i % 2 == 0 { 1 } else { 5 });
    assert_eq!(
        PalettedSection::pack(&mixed_cells, &opaque, &never).light_class(),
        SectionLightClass::Mixed
    );
    let all_opaque = PalettedSection::pack(&[1; CELLS], &opaque, &never);
    assert_eq!(all_opaque.light_class(), SectionLightClass::Opaque);
    assert_eq!(
        PalettedSection::pack(&[0; CELLS], &opaque, &never).light_class(),
        SectionLightClass::Empty
    );
    // Editing a non-opaque cell back to opaque recomputes across the cells.
    let mut section = PalettedSection::pack(&mixed_cells, &opaque, &never);
    for cell in (0..CELLS).step_by(2) {
        section.set(cell, 5, &opaque, &never);
    }
    assert_eq!(section.light_class(), SectionLightClass::Mixed);
    section.set(0, 1, &opaque, &never);
    assert_eq!(section.light_class(), SectionLightClass::Mixed);
}

#[test]
fn copy_y_slice_copies_exactly_the_requested_rows() {
    let cells = mixed(33);
    let section = pack(&cells);

    let mut one = vec![0u32; 256];
    section.copy_y_slice(0, 1, &mut one);
    assert_eq!(one[..], cells[..256]);
    let mut last = vec![0u32; 256];
    section.copy_y_slice(15, 1, &mut last);
    assert_eq!(last[..], cells[15 * 256..]);
    let mut all = vec![0u32; CELLS];
    section.copy_y_slice(0, 16, &mut all);
    assert_eq!(all[..], cells[..]);
    // Zero rows is a no-op even for an empty destination.
    let mut none: [u32; 0] = [];
    section.copy_y_slice(0, 0, &mut none);
}

#[test]
#[should_panic(expected = "copy_y_slice destination too small")]
fn copy_y_slice_rejects_a_short_destination() {
    pack(&mixed(17)).copy_y_slice(0, 2, &mut [0u32; 256]);
}

#[test]
fn approx_bytes_grows_with_the_representation() {
    let single = PalettedSection::single(1, false, false);
    let indirect = pack(&mixed(17));
    let direct = pack(&mixed(257));
    assert_eq!(
        single.approx_bytes(),
        std::mem::size_of::<PalettedSection>()
    );
    assert!(indirect.approx_bytes() > single.approx_bytes());
    assert!(direct.approx_bytes() > indirect.approx_bytes());
}
