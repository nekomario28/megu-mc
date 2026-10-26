//! Independent dense formula for sparse sky-light composition, not a world oracle.
use super::*;

#[test]
fn sparse_sky_composition_matches_every_dense_nibble() {
    for (cx, cz) in [(0, 0), (-2, 1)] {
        for profile in 0..3 {
            let mut floors = [0; 256];
            for (i, floor) in floors.iter_mut().enumerate() {
                *floor = match profile {
                    0 => 12,
                    1 => MIN_Y + (i as i32 * 13).rem_euclid(416),
                    _ => {
                        if i & 1 == 0 {
                            MIN_Y - 1
                        } else {
                            MIN_Y + 416
                        }
                    }
                };
            }
            let mut spill = HashMap::new();
            // Include both nibbles, negative coordinates, chunk/vertical edges,
            // and levels outside the producer's range to pin the masking order.
            for x in -1i32..=16 {
                for z in -1i32..=16 {
                    for y in (MIN_Y - 1..=MIN_Y + 416).step_by(7).chain([
                        MIN_Y,
                        MIN_Y + 415,
                        MIN_Y + 416,
                    ]) {
                        spill.insert(
                            (cx * 16 + x, y, cz * 16 + z),
                            (x * 3 + y + z * 7).rem_euclid(32) as u8,
                        );
                    }
                }
            }
            for values in [&spill, &HashMap::new()] {
                let actual = sky_light_sections_with_spill(cx, cz, &floors, values);
                assert_eq!(actual.len(), 26);
                for (section, bytes) in actual.iter().enumerate() {
                    let sy = LIGHT_MIN_Y + section as i32 * 16;
                    let expected = pack_light_section(sy, |lx, y, lz| {
                        let base: u8 = if y > floors[(lz << 4) | lx] { 15 } else { 0 };
                        base.max(
                            values
                                .get(&(cx * 16 + lx as i32, y, cz * 16 + lz as i32))
                                .copied()
                                .unwrap_or(0),
                        )
                    });
                    for (i, (a, b)) in bytes.iter().zip(expected.iter()).enumerate() {
                        assert_eq!(
                            a, b,
                            "chunk {cx},{cz} profile {profile} section {section} byte {i}"
                        );
                    }
                }
            }
        }
    }
}

/// The light array at index i must belong to light section `LIGHT_MIN_Y/16 + i`, where
/// `LIGHT_MIN_Y` is one section below the world. Vanilla
/// `LevelLightEngine.getMinLightSection()` is `getMinSection() - 1` and the packet sends
/// `getSectionsCount() + 2` arrays from there; anchoring at `MIN_Y` shifts every array up one.
#[test]
fn light_sections_start_one_below_the_world() {
    assert_eq!(
        LIGHT_MIN_Y,
        MIN_Y - 16,
        "the first light section is minSection - 1"
    );
    let floors = [MIN_Y + 5; 256];
    let secs = sky_light_sections_from(0, 0, &floors);
    assert_eq!(secs.len(), 26);
    let nib = |y: i32, lx: usize, lz: usize| -> u8 {
        let sec = ((y - LIGHT_MIN_Y) / 16) as usize;
        let sy = (y - LIGHT_MIN_Y - sec as i32 * 16) as usize;
        let b = secs[sec][sy * 128 + lz * 8 + (lx >> 1)];
        if lx.is_multiple_of(2) {
            b & 0x0f
        } else {
            b >> 4
        }
    };
    assert_eq!(nib(MIN_Y + 5, 0, 0), 0, "at the floor is dark");
    assert_eq!(nib(MIN_Y + 6, 0, 0), 15, "above the floor is lit");
    assert_eq!(
        nib(MIN_Y - 1, 0, 0),
        0,
        "the extra section below the world is dark"
    );
    assert_eq!(nib(LIGHT_MIN_Y, 0, 0), 0, "and its bottom is dark too");
}

/// A generated light source (lava, glowstone) is not in the overlay, so the block-light pass
/// must take it from the chunk's recorded generated sources.
#[test]
fn block_light_uses_generated_sources() {
    crate::test_support::require_vanilla_data();
    let _g = world_guard();
    let mut gen = GenSources::new();
    gen.insert((0, 0), vec![((5, 70, 7), 15)]);
    let secs = block_light_sections(0, 0, &gen);
    let nib = |y: i32, lx: usize, lz: usize| -> u8 {
        let sec = ((y - LIGHT_MIN_Y) / 16) as usize;
        let sy = (y - LIGHT_MIN_Y - sec as i32 * 16) as usize;
        let b = secs[sec][sy * 128 + lz * 8 + (lx >> 1)];
        if lx.is_multiple_of(2) {
            b & 0x0f
        } else {
            b >> 4
        }
    };
    assert_eq!(nib(70, 5, 7), 15, "a generated emitter must light");
    assert_eq!(nib(70, 6, 7), 14, "and spread by one");
}

/// A generated emitter in the next chunk must light this one across the border: the light pass
/// may not assume every source is inside the chunk being lit.
#[test]
fn block_light_sees_a_neighbours_generated_source() {
    crate::test_support::require_vanilla_data();
    let _g = world_guard();
    let mut gen = GenSources::new();
    gen.insert((1, 0), vec![((20, 70, 7), 15)]); // next chunk, five blocks past the border
    let secs = block_light_sections(0, 0, &gen);
    let nib = |y: i32, lx: usize, lz: usize| -> u8 {
        let sec = ((y - LIGHT_MIN_Y) / 16) as usize;
        let sy = (y - LIGHT_MIN_Y - sec as i32 * 16) as usize;
        let b = secs[sec][sy * 128 + lz * 8 + (lx >> 1)];
        if lx.is_multiple_of(2) {
            b & 0x0f
        } else {
            b >> 4
        }
    };
    assert_eq!(
        nib(70, 15, 7),
        10,
        "a neighbour's emitter lights this chunk's edge"
    );
    assert_eq!(nib(70, 14, 7), 9, "and falls off with distance");
}

/// A newly generated chunk names an already-generated neighbour for a light update, and a chunk
/// with no emitters names none; a neighbour not generated yet is left to its own call.
#[test]
fn a_source_marks_an_already_generated_neighbour_for_a_light_update() {
    let mut gen = GenSources::new();
    gen.insert((0, 0), vec![((15, 70, 7), 15)]);
    gen.insert((1, 0), vec![]);
    let dirty = light_dirty_for(&gen, 0, 0);
    assert!(
        dirty.contains(&(1, 0)),
        "an emitter at the border reaches the next chunk"
    );
    assert!(
        !dirty.contains(&(0, 1)),
        "a neighbour not generated yet is not updated now"
    );
    assert!(
        light_dirty_for(&gen, 1, 0).is_empty(),
        "a chunk with no emitters changes no light"
    );
}

/// A light update carries only block light: no sky mask, no sky arrays, and every zero block
/// section goes in the empty mask. The client leaves sky unchanged for an unmasked section, so
/// omitting it is what keeps the update small without dimming the world.
#[test]
fn update_light_packet_sends_block_light_only() {
    crate::test_support::require_vanilla_data();
    let _g = world_guard();
    let mut gen = GenSources::new();
    gen.insert((0, 0), vec![((5, 70, 7), 15)]);
    let bytes = update_light_packet(0, 0, &gen);
    let mut r = &bytes[..];
    assert_eq!(read_varint(&mut r), Some(0), "chunk x");
    assert_eq!(read_varint(&mut r), Some(0), "chunk z");
    assert_eq!(read_varint(&mut r), Some(0), "no sky mask");
    assert_eq!(read_varint(&mut r), Some(1), "block mask is one long");
    let block_mask = u64::from_be_bytes(r[..8].try_into().unwrap());
    r = &r[8..];
    assert_ne!(block_mask, 0, "the chunk has lit sections");
    assert_eq!(read_varint(&mut r), Some(0), "no empty sky mask");
    assert_eq!(read_varint(&mut r), Some(1), "empty block mask is one long");
    let empty_mask = u64::from_be_bytes(r[..8].try_into().unwrap());
    r = &r[8..];
    assert_eq!(
        block_mask & empty_mask,
        0,
        "a section is lit or empty, never both"
    );
    assert_eq!(
        block_mask | empty_mask,
        (1 << 26) - 1,
        "all 26 sections are accounted for"
    );
    assert_eq!(read_varint(&mut r), Some(0), "no sky arrays");
    let count = read_varint(&mut r).unwrap() as usize;
    assert_eq!(
        count,
        block_mask.count_ones() as usize,
        "one array per lit section"
    );
    for _ in 0..count {
        assert_eq!(read_varint(&mut r), Some(2048));
        r = &r[2048..];
    }
    assert!(r.is_empty(), "packet fully consumed");
}

/// A generated emitter the player has since removed must stop lighting. The cached generated
/// source is skipped when the overlay now holds that cell, so the live overlay decides emission.
#[test]
fn a_removed_generated_emitter_stops_lighting() {
    crate::test_support::require_vanilla_data();
    let _g = world_guard();
    let old_world = world().lock().unwrap().clone();
    let mut gen = GenSources::new();
    gen.insert((0, 0), vec![((5, 70, 7), 15)]);
    let nib = |secs: &[[u8; 2048]], y: i32, lx: usize, lz: usize| -> u8 {
        let sec = ((y - LIGHT_MIN_Y) / 16) as usize;
        let sy = (y - LIGHT_MIN_Y - sec as i32 * 16) as usize;
        let b = secs[sec][sy * 128 + lz * 8 + (lx >> 1)];
        if lx.is_multiple_of(2) {
            b & 0x0f
        } else {
            b >> 4
        }
    };
    let lit = block_light_sections(0, 0, &gen);
    assert_eq!(nib(&lit, 70, 5, 7), 15, "the generated emitter lights");
    // The player breaks it: an explicit air override at the cell, as set_block records.
    world()
        .lock()
        .unwrap()
        .entry((0, 0))
        .or_default()
        .insert(local_idx(5, 70, 7), 0);
    let dark = block_light_sections(0, 0, &gen);
    assert_eq!(
        nib(&dark, 70, 5, 7),
        0,
        "a removed generated emitter must not light again"
    );
    // Removing the edit re-exposes the cached generated emitter, which must light again.
    world()
        .lock()
        .unwrap()
        .entry((0, 0))
        .or_default()
        .remove(&local_idx(5, 70, 7));
    let back = block_light_sections(0, 0, &gen);
    assert_eq!(
        nib(&back, 70, 5, 7),
        15,
        "removing the overlay re-exposes the generated emitter"
    );
    *world().lock().unwrap() = old_world;
}
