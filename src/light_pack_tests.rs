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
                    let sy = MIN_Y + section as i32 * 16;
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
