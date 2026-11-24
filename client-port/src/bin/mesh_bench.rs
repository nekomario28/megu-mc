// SPDX-License-Identifier: LGPL-3.0-or-later

use std::hint::black_box;
use std::mem::MaybeUninit;
use std::ptr;
use std::time::Instant;

use megu_client_mesh::{
    ffi::{
        megu_client_mesh_column, megu_client_mesh_pack_vertices, MeshColumnInput,
        MeshColumnSection, PackedVertex, ABI_AIR, ABI_COLUMN_UNIFORM_AIR, ABI_OK, ABI_VERSION,
    },
    mesh_section, Block, Light, Quad, Section, EDGE, LIGHT_BYTES, SECTION_CELLS, WATER,
};

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
    let mut uniform_varying_light = Section::filled(stone, Light::new(0, 0));
    for y in 0..EDGE {
        for z in 0..EDGE {
            for x in 0..EDGE {
                let block_light = ((x + z) % 16) as u8;
                let sky_light = ((y + x) % 16) as u8;
                uniform_varying_light.set(x, y, z, stone, Light::new(block_light, sky_light));
            }
        }
    }
    println!(
        "uniform cube: {} raw faces, {} outer faces before merging",
        SECTION_CELLS * 6,
        EDGE * EDGE * 6
    );
    let mut verified_cases = 0;
    for (name, section, expected) in [
        ("uniform solid", solid, Some(6)),
        ("uniform mixed-light", uniform_varying_light, None),
        ("mixed checkerboard", checkerboard, Some(SECTION_CELLS * 3)),
    ] {
        let view = section.view();
        let baseline = mesh_section(&view);
        let quad_count = baseline.len();
        if let Some(expected) = expected {
            assert_eq!(quad_count, expected);
        }
        verified_cases += 1;
        drop(baseline);
        let start = Instant::now();
        for _ in 0..runs {
            let output = mesh_section(black_box(&view));
            assert_eq!(output.len(), quad_count);
            black_box(output);
        }
        println!(
            "{name}: {quad_count} quads, {runs} sections, {:.2} us/section",
            start.elapsed().as_secs_f64() * 1e6 / runs as f64
        );
    }
    println!("fixture quad-count verifications: {verified_cases}/3");
    overworld_column(runs);
}

fn overworld_column(runs: usize) {
    const MIN_Y: i32 = -64;
    const WATER_Y: i32 = 62;
    const DENSE_SECTIONS: usize = 9;
    const OVERWORLD_SECTIONS: usize = 24;
    let stone = Block::opaque_cube(1);
    let water = Block {
        state_id: 80,
        fluid_kind: WATER,
        ..Block::AIR
    };
    let sections: Vec<_> = (0..DENSE_SECTIONS)
        .map(|index| {
            let mut section = Section::filled(Block::AIR, Light::new(0, 15));
            for y in 0..EDGE {
                let world_y = MIN_Y + (index * EDGE + y) as i32;
                for z in 0..EDGE {
                    for x in 0..EDGE {
                        let surface = 56 + ((x * 13 + z * 7 + x * z) % 14) as i32;
                        if world_y <= surface {
                            section.set(x, y, z, stone, Light::new(0, 0));
                        } else if world_y <= WATER_Y {
                            section.set(x, y, z, water, Light::new(0, 12));
                        }
                    }
                }
            }
            section
        })
        .collect();
    let dark = [0; LIGHT_BYTES];
    let daylight = [255; LIGHT_BYTES];
    let inputs: [_; OVERWORLD_SECTIONS] = std::array::from_fn(|i| match sections.get(i) {
        Some(section) => MeshColumnSection {
            flags: 0,
            blocks: section.blocks.as_ptr(),
            block_light: section.block_light.as_ptr(),
            sky_light: section.sky_light.as_ptr(),
        },
        None => MeshColumnSection {
            flags: ABI_COLUMN_UNIFORM_AIR,
            blocks: ptr::null(),
            block_light: dark.as_ptr(),
            sky_light: daylight.as_ptr(),
        },
    });
    let input = MeshColumnInput {
        abi_version: ABI_VERSION,
        missing: ABI_AIR,
        sections: inputs.as_ptr(),
        section_count: inputs.len(),
        borders: [ptr::null(); 4],
    };
    let mut quad_count = 0;
    // SAFETY: inputs and count are live and have the lengths in the ABI.
    assert_eq!(
        unsafe { megu_client_mesh_column(&input, ptr::null_mut(), 0, &mut quad_count) },
        ABI_OK
    );
    let mut quads = vec![MaybeUninit::<Quad>::uninit(); quad_count];
    let mut vertices = vec![PackedVertex::default(); quad_count * 4];
    let mut vertex_count = 0;
    let start = Instant::now();
    for _ in 0..runs {
        // SAFETY: the mesh writes at most the allocated capacity. Pack reads only
        // the initialized prefix on success. All output buffers are disjoint.
        unsafe {
            assert_eq!(
                megu_client_mesh_column(
                    black_box(&input),
                    quads.as_mut_ptr().cast(),
                    quads.len(),
                    &mut quad_count,
                ),
                ABI_OK
            );
            assert_eq!(
                megu_client_mesh_pack_vertices(
                    quads.as_ptr().cast(),
                    quad_count,
                    vertices.as_mut_ptr(),
                    vertices.len(),
                    &mut vertex_count,
                ),
                ABI_OK
            );
        }
        black_box(&vertices);
    }
    println!(
        "Overworld -64..319: {quad_count} quads, {vertex_count} vertices, \
         {runs} columns, {:.2} us/column (mesh + pack)",
        start.elapsed().as_secs_f64() * 1e6 / runs as f64
    );
}
