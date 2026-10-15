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
                    _ => if i & 1 == 0 { MIN_Y - 1 } else { MIN_Y + 416 },
                };
            }
            let mut spill = HashMap::new();
            // Include both nibbles, negative coordinates, chunk/vertical edges,
            // and levels outside the producer's range to pin the masking order.
            for x in -1i32..=16 { for z in -1i32..=16 {
                for y in (MIN_Y - 1..=MIN_Y + 416).step_by(7)
                    .chain([MIN_Y, MIN_Y + 415, MIN_Y + 416]) {
                    spill.insert((cx * 16 + x, y, cz * 16 + z),
                        (x * 3 + y + z * 7).rem_euclid(32) as u8);
                }
            }}
            for values in [&spill, &HashMap::new()] {
                let actual = sky_light_sections_with_spill(cx, cz, &floors, values);
                assert_eq!(actual.len(), 26);
                for (section, bytes) in actual.iter().enumerate() {
                    let sy = LIGHT_MIN_Y + section as i32 * 16;
                    let expected = pack_light_section(sy, |lx, y, lz| {
                        let base: u8 = if y > floors[(lz << 4) | lx] { 15 } else { 0 };
                        base.max(values.get(&(cx * 16 + lx as i32, y, cz * 16 + lz as i32))
                            .copied().unwrap_or(0))
                    });
                    for (i, (a, b)) in bytes.iter().zip(expected.iter()).enumerate() {
                        assert_eq!(a, b, "chunk {cx},{cz} profile {profile} section {section} byte {i}");
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
    assert_eq!(LIGHT_MIN_Y, MIN_Y - 16, "the first light section is minSection - 1");
    let mut floors = [MIN_Y + 5; 256];
    let secs = sky_light_sections_from(0, 0, &floors);
    assert_eq!(secs.len(), 26);
    let nib = |y: i32, lx: usize, lz: usize| -> u8 {
        let sec = ((y - LIGHT_MIN_Y) / 16) as usize;
        let sy = (y - LIGHT_MIN_Y - sec as i32 * 16) as usize;
        let b = secs[sec][sy * 128 + lz * 8 + (lx >> 1)];
        if lx % 2 == 0 { b & 0x0f } else { b >> 4 }
    };
    assert_eq!(nib(MIN_Y + 5, 0, 0), 0, "at the floor is dark");
    assert_eq!(nib(MIN_Y + 6, 0, 0), 15, "above the floor is lit");
    assert_eq!(nib(MIN_Y - 1, 0, 0), 0, "the extra section below the world is dark");
    assert_eq!(nib(LIGHT_MIN_Y, 0, 0), 0, "and its bottom is dark too");
}
