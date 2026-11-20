// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

use std::hint::black_box;
use std::time::Instant;

use mc_spine_client_mesh::{mesh_section, Block, Light, Section, EDGE, SECTION_CELLS};

fn main() {
    let runs: usize = std::env::args()
        .nth(1)
        .map_or(200, |n| n.parse().expect("positive run count"));
    assert!(runs > 0, "run count must be positive");
    let stone = Block::opaque_cube(1);
    let solid = Section::filled(stone, Light::new(0, 0));
    let mut checkerboard = Section::filled(Block::AIR, Light::new(0, 15));
    for y in 0..EDGE {
        for z in 0..EDGE {
            for x in 0..EDGE {
                if (x + y + z) % 2 == 0 {
                    checkerboard.set(x, y, z, stone, Light::new(0, 0));
                }
            }
        }
    }
    println!(
        "uniform cube: {} raw faces, {} outer faces before merging",
        SECTION_CELLS * 6,
        EDGE * EDGE * 6
    );
    for (name, section, expected) in [
        ("solid", solid, 6),
        ("checkerboard", checkerboard, SECTION_CELLS * 3),
    ] {
        let view = section.view();
        assert_eq!(mesh_section(&view).len(), expected);
        let start = Instant::now();
        for _ in 0..runs {
            black_box(mesh_section(black_box(&view)));
        }
        println!(
            "{name}: {expected} quads, {runs} sections, {:.2} us/section",
            start.elapsed().as_secs_f64() * 1e6 / runs as f64
        );
    }
}
