// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! LOD generation speed and section-culling savings. Run:
//! `cargo run --release --manifest-path client-port/Cargo.toml --bin lod_bench`

use std::hint::black_box;
use std::mem::MaybeUninit;
use std::ptr;
use std::time::Instant;

use mc_spine_client_mesh::{
    ffi::{
        mc_spine_client_mesh_column, MeshColumnInput, MeshColumnSection, ABI_AIR,
        ABI_COLUMN_SEALED_SOLID, ABI_OK, ABI_OPAQUE, ABI_VERSION,
    },
    mesh_section, summarize_column, Block, Light, LodCell, LodSummary, MissingNeighbor, Quad,
    Section, SectionVis, EDGE, LOD_COARSE, LOD_FINE, WATER,
};

const MIN_Y: i32 = -64;
const WATER_Y: i32 = 62;
const DENSE_SECTIONS: usize = 9;
const OVERWORLD_SECTIONS: usize = 24;

fn overworld() -> Vec<Section> {
    let stone = Block::opaque_cube(1);
    let water = Block {
        state_id: 80,
        fluid_kind: WATER,
        ..Block::AIR
    };
    let mut sections: Vec<_> = (0..DENSE_SECTIONS)
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
    while sections.len() < OVERWORLD_SECTIONS {
        sections.push(Section::filled(Block::AIR, Light::new(0, 15)));
    }
    sections
}

fn column_input(sections: &[Section], missing: u32) -> (Vec<MeshColumnSection>, MeshColumnInput) {
    column_input_flags(sections, missing, |_| 0)
}

fn column_input_flags(
    sections: &[Section],
    missing: u32,
    flags: impl Fn(usize) -> u32,
) -> (Vec<MeshColumnSection>, MeshColumnInput) {
    let inputs: Vec<_> = sections
        .iter()
        .enumerate()
        .map(|(index, section)| MeshColumnSection {
            flags: flags(index),
            blocks: section.blocks.as_ptr(),
            block_light: section.block_light.as_ptr(),
            sky_light: section.sky_light.as_ptr(),
        })
        .collect();
    let input = MeshColumnInput {
        abi_version: ABI_VERSION,
        missing,
        sections: inputs.as_ptr(),
        section_count: inputs.len(),
        borders: [ptr::null(); 4],
    };
    (inputs, input)
}

fn time_us(runs: usize, mut f: impl FnMut()) -> f64 {
    let start = Instant::now();
    for _ in 0..runs {
        f();
    }
    start.elapsed().as_secs_f64() * 1e6 / runs as f64
}

fn main() {
    let runs: usize = std::env::args()
        .nth(1)
        .map_or(200, |n| n.parse().expect("positive run count"));
    assert!(runs > 0, "run count must be positive");

    // --- LOD summaries over the overworld fixture (24 sections, 384 blocks). ---
    let world = overworld();
    let views: Vec<_> = world.iter().map(Section::view).collect();
    let mut summary = LodSummary::new(LOD_COARSE);
    black_box(summary.update(&views));
    assert_eq!(summary.height(), OVERWORLD_SECTIONS * EDGE);
    let us = time_us(runs, || {
        black_box(summary.update(black_box(&views)));
    });
    println!(
        "lod 4x4: {us:.2} us/column ({} cells, {runs} runs)",
        summary.cells().len()
    );
    let mut fine = [LodCell::EMPTY; LOD_FINE * LOD_FINE];
    summarize_column(&views, LOD_FINE, &mut fine);
    black_box(&fine);
    let us = time_us(runs, || {
        summarize_column(black_box(&views), LOD_FINE, &mut fine);
    });
    println!(
        "lod 16x16: {us:.2} us/column ({} cells, {runs} runs)",
        fine.len()
    );
    let water_top = fine[15 * EDGE + 8];
    println!(
        "  max top_y={}, example cell top_y={} state={}",
        fine.iter().map(|c| c.top_y).max().unwrap_or(-1),
        water_top.top_y,
        water_top.state_id
    );

    // --- Section culling: the sealed-column skip versus a full mesh. ---
    let solid = Section::filled(Block::opaque_cube(1), Light::new(0, 0));
    let open_view = solid.view();
    let mut sealed_view = solid.view();
    sealed_view.missing = MissingNeighbor::Opaque;
    assert!(!SectionVis::scan(&sealed_view).emits_any());
    assert!(mesh_section(&sealed_view).is_empty());
    assert_eq!(mesh_section(&open_view).len(), 6);

    let us = time_us(runs, || {
        black_box(mesh_section(black_box(&sealed_view)));
    });
    println!("mesh sealed section (culled): {us:.2} us");
    let us = time_us(runs, || {
        black_box(SectionVis::scan(black_box(&sealed_view)));
    });
    println!("SectionVis::scan sealed section (mask only): {us:.2} us");
    let us = time_us(runs, || {
        black_box(mesh_section(black_box(&open_view)));
    });
    println!("mesh open solid section (no cull possible): {us:.2} us");

    // Column level: 24 sealed sections against opaque missing vs the open
    // overworld column through the same FFI path.
    let solids: Vec<_> = (0..OVERWORLD_SECTIONS)
        .map(|_| Section::filled(Block::opaque_cube(1), Light::new(0, 0)))
        .collect();
    let (_sealed_inputs, sealed_input) =
        column_input_flags(&solids, ABI_OPAQUE, |_| ABI_COLUMN_SEALED_SOLID);
    let mut quad_count = usize::MAX;
    // SAFETY: input buffers live for the call; count is disjoint.
    assert_eq!(
        unsafe { mc_spine_client_mesh_column(&sealed_input, ptr::null_mut(), 0, &mut quad_count) },
        ABI_OK
    );
    assert_eq!(quad_count, 0);
    let culled_us = time_us(runs, || unsafe {
        assert_eq!(
            mc_spine_client_mesh_column(
                black_box(&sealed_input),
                ptr::null_mut(),
                0,
                &mut quad_count
            ),
            ABI_OK
        );
    });
    println!("mesh 24 sealed-hinted sections (culled): {culled_us:.2} us/column");

    // Same data without the caller hint: the interior scan still proves the
    // sections silent, just less cheaply.
    let (_scan_inputs, scan_input) = column_input(&solids, ABI_OPAQUE);
    assert_eq!(
        unsafe { mc_spine_client_mesh_column(&scan_input, ptr::null_mut(), 0, &mut quad_count) },
        ABI_OK
    );
    assert_eq!(quad_count, 0);
    let scanned_us = time_us(runs, || unsafe {
        assert_eq!(
            mc_spine_client_mesh_column(
                black_box(&scan_input),
                ptr::null_mut(),
                0,
                &mut quad_count
            ),
            ABI_OK
        );
    });
    println!("mesh 24 sealed sections (scanned): {scanned_us:.2} us/column");

    // Same sections with open (air) missing sides: nothing can be skipped, so
    // this is the full-pipeline cost for the same data shape.
    let (_open_inputs, open_input) = column_input(&solids, ABI_AIR);
    assert_eq!(
        unsafe { mc_spine_client_mesh_column(&open_input, ptr::null_mut(), 0, &mut quad_count) },
        ABI_OK
    );
    let open_quads = quad_count;
    let mut open_buf = vec![MaybeUninit::<Quad>::uninit(); quad_count];
    let full_us = time_us(runs, || {
        unsafe {
            assert_eq!(
                mc_spine_client_mesh_column(
                    black_box(&open_input),
                    open_buf.as_mut_ptr().cast(),
                    open_buf.len(),
                    &mut quad_count
                ),
                ABI_OK
            );
        }
        black_box(&open_buf);
    });
    println!(
        "mesh 24 unsealed solid sections (full pipeline): {full_us:.2} us/column ({open_quads} quads)"
    );
    if culled_us > 0.0 && full_us > culled_us {
        println!(
            "  culling saves {:.1}% of column time on sealed content",
            100.0 * (1.0 - culled_us / full_us)
        );
    }

    let (_world_inputs, world_input) = column_input(&world, ABI_OPAQUE);
    // SAFETY: as above.
    assert_eq!(
        unsafe { mc_spine_client_mesh_column(&world_input, ptr::null_mut(), 0, &mut quad_count) },
        ABI_OK
    );
    let mut quads = vec![MaybeUninit::<Quad>::uninit(); quad_count];
    let us = time_us(runs, || {
        unsafe {
            assert_eq!(
                mc_spine_client_mesh_column(
                    black_box(&world_input),
                    quads.as_mut_ptr().cast(),
                    quads.len(),
                    &mut quad_count
                ),
                ABI_OK
            );
        }
        black_box(&quads);
    });
    println!("mesh overworld column: {us:.2} us/column ({quad_count} quads, reference)");
}
