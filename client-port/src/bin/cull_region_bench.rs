// SPDX-License-Identifier: LGPL-3.0-or-later

//! S2 `GraphCuller` + S4 `RenderRegion` perf budget bench. Run:
//! `cargo run --release --manifest-path client-port/Cargo.toml --bin cull_region_bench`
//!
//! S2 measures the direction-monotonic BFS over a realistic multi-chunk
//! scene (16x16 chunk columns, 128 blocks tall) that mixes a solid
//! underground, an open cave/room network reachable through a surface
//! ravine, sealed rooms that stay occluded, and a half-space frustum that
//! rejects a corner slab of the world before traversal.
//!
//! S4 measures the shared-arena upload/eviction/multidraw cycle across a
//! 12x4x12-section workload that straddles four 8x4x8 regions, reporting
//! the deterministic command/draw counts and live arena occupancy.

use std::hint::black_box;
use std::time::Instant;

use megu_client_mesh::{
    mesh_section, section_index, Block, FaceVisibility, GraphCuller, Light, RenderRegions, Section,
    SectionCoord, SectionGraph, EDGE, FACE_CELLS, SECTION_CELLS, THROUGH_ALL,
};

// --- S2 scene ---------------------------------------------------------------

const SCENE_X: i32 = 16;
const SCENE_Y: i32 = 8;
const SCENE_Z: i32 = 16;
/// Sections below this height are underground (stone/caves); at it is the
/// open surface layer; above it is uniform air.
const SURFACE_Y: i32 = 4;

/// Cave network joined to the surface: two crossing tunnels at y=2, a
/// ravine shaft up through the surface layer, and one side room.
fn is_cave(x: i32, y: i32, z: i32) -> bool {
    // Main tunnel, east-west.
    (y == 2 && (7..=8).contains(&z) && (2..=13).contains(&x))
        // Branch tunnel, north-south.
        || (y == 2 && (7..=8).contains(&x) && (2..=13).contains(&z))
        // Ravine shaft connecting the tunnels to the surface layer.
        || ((7..=8).contains(&x) && (3..=SURFACE_Y).contains(&y) && (7..=8).contains(&z))
        // Room off the branch tunnel (overlaps it at x=7).
        || ((4..=7).contains(&x) && (1..=2).contains(&y) && (9..=12).contains(&z))
}

/// Fully enclosed air pockets: never reached by sight, so the solid shell
/// around them is pure occlusion-culled load.
fn is_sealed_room(x: i32, y: i32, z: i32) -> bool {
    ((11..=12).contains(&x) && (0..=1).contains(&y) && (3..=4).contains(&z))
        || ((2..=3).contains(&x) && (0..=1).contains(&y) && (12..=13).contains(&z))
}

/// Grass-over-stone surface: real `FaceVisibility` for a section whose
/// bottom half is opaque cube and whose top half is air. Sight connects
/// every face except Down, which the stone floor seals.
fn surface_graph() -> SectionGraph {
    let stone = Block::opaque_cube(1);
    let mut blocks = [Block::AIR; SECTION_CELLS];
    for y in 0..EDGE / 2 {
        for z in 0..EDGE {
            for x in 0..EDGE {
                blocks[section_index(x, y, z)] = stone;
            }
        }
    }
    SectionGraph {
        through: FaceVisibility::compute(&blocks).bits(),
        renderable: true,
    }
}

fn scene() -> Vec<(SectionCoord, SectionGraph)> {
    let surface = surface_graph();
    let mut map = Vec::with_capacity((SCENE_X * SCENE_Y * SCENE_Z) as usize);
    for x in 0..SCENE_X {
        for y in 0..SCENE_Y {
            for z in 0..SCENE_Z {
                let graph = if is_sealed_room(x, y, z) {
                    SectionGraph::AIR
                } else if is_cave(x, y, z) {
                    // Mostly air but emits wall geometry.
                    SectionGraph {
                        through: THROUGH_ALL,
                        renderable: true,
                    }
                } else if y > SURFACE_Y {
                    SectionGraph::AIR
                } else if y == SURFACE_Y {
                    surface
                } else {
                    SectionGraph::SOLID
                };
                map.push((SectionCoord(x, y, z), graph));
            }
        }
    }
    map
}

/// Axis-aligned half-space frustum: keeps the x<=216, z<=184 slab, culling
/// the far corner of the scene before the search can traverse into it.
fn frustum() -> [[f32; 4]; 6] {
    [
        [-1.0, 0.0, 0.0, 216.0],
        [0.0, 0.0, -1.0, 184.0],
        [1.0, 0.0, 0.0, 4096.0],
        [0.0, 1.0, 0.0, 4096.0],
        [0.0, 0.0, 1.0, 4096.0],
        [0.0, -1.0, 0.0, 4096.0],
    ]
}

// --- S4 workload ------------------------------------------------------------

const S4_X: i32 = 12;
const S4_Y: i32 = 4;
const S4_Z: i32 = 12;

fn s4_coords() -> Vec<SectionCoord> {
    let mut coords = Vec::with_capacity((S4_X * S4_Y * S4_Z) as usize);
    for x in 0..S4_X {
        for y in 0..S4_Y {
            for z in 0..S4_Z {
                coords.push(SectionCoord(x, y, z));
            }
        }
    }
    coords
}

/// Deterministic subset unloaded and re-uploaded every iteration so the
/// arena free list is exercised, not just append-only growth.
fn evicted(coord: SectionCoord) -> bool {
    (coord.0 + 2 * coord.2 + coord.1) % 7 == 0
}

/// Sparse deterministic stone scatter inside an air section.
fn sparse_section() -> Section {
    let stone = Block::opaque_cube(1);
    let mut section = Section::filled(Block::AIR, Light::new(0, 15));
    for index in (0..SECTION_CELLS).step_by(37) {
        let x = index % EDGE;
        let z = (index / EDGE) % EDGE;
        let y = index / FACE_CELLS;
        section.set(x, y, z, stone, Light::new(0, 0));
    }
    section
}

// --- driver -----------------------------------------------------------------

fn median_us(samples: &mut [f64]) -> f64 {
    samples.sort_by(f64::total_cmp);
    samples[samples.len() / 2]
}

fn main() {
    let runs: usize = std::env::args()
        .nth(1)
        .map_or(200, |n| n.parse().expect("positive run count"));
    assert!(runs > 0, "run count must be positive");

    // --- S2 GraphCuller ---
    let scene = scene();
    let renderable = scene.iter().filter(|(_, g)| g.renderable).count();
    let camera = SectionCoord(7, SURFACE_Y + 2, 7);
    let planes = frustum();
    let mut s2_us = Vec::with_capacity(runs);
    let (mut visible, mut culled) = (0usize, 0usize);
    for _ in 0..runs {
        let culler = GraphCuller::new(camera, &planes);
        let start = Instant::now();
        let result = culler.search(black_box(scene.iter().copied()));
        s2_us.push(start.elapsed().as_secs_f64() * 1e6);
        visible = result.visible.len();
        culled = result.culled;
        black_box(&result.visible);
    }
    let s2_median = median_us(&mut s2_us);
    assert_eq!((visible, culled), (363, 901));
    assert_eq!(visible + culled, renderable);
    println!(
        "S2 scene: {renderable} renderable of {} sections ({SCENE_X}x{SCENE_Y}x{SCENE_Z}, tunnels + sealed rooms)",
        scene.len()
    );
    println!(
        "s2 search: {visible} visible, {culled} culled, {s2_median:.2} us/iter ({runs} iters)"
    );

    // --- S4 RenderRegion ---
    let stone = Block::opaque_cube(1);
    let solid_quads = mesh_section(&Section::filled(stone, Light::new(0, 0)).view());
    let sparse_quads = mesh_section(&sparse_section().view());
    let coords = s4_coords();
    let camera_block = [
        S4_X as f32 * 8.0,
        (S4_Y + 2) as f32 * 16.0,
        S4_Z as f32 * 8.0,
    ];
    let evict: Vec<_> = coords.iter().copied().filter(|&c| evicted(c)).collect();
    let mut regions = RenderRegions::default();
    let mut s4_upload_us = Vec::with_capacity(runs);
    let mut s4_draw_us = Vec::with_capacity(runs);
    let (mut commands, mut draws) = (0usize, 0usize);
    for _ in 0..runs {
        let start = Instant::now();
        for &coord in &coords {
            let quads = if (coord.0 + coord.1 + coord.2) % 3 == 0 {
                &sparse_quads
            } else {
                &solid_quads
            };
            assert!(regions.upload(coord, quads));
        }
        for &coord in &evict {
            assert!(regions.remove(coord));
        }
        s4_upload_us.push(start.elapsed().as_secs_f64() * 1e6);
        let start = Instant::now();
        let batch = regions.build_multidraw(black_box(camera_block));
        s4_draw_us.push(start.elapsed().as_secs_f64() * 1e6);
        commands = batch.len();
        draws = batch.iter().map(|command| command.draws.len()).sum();
        black_box(&batch);
    }
    let s4_upload_median = median_us(&mut s4_upload_us);
    let s4_draw_median = median_us(&mut s4_draw_us);
    // Arena occupancy after the final upload+evict pass.
    let mut vertex_words = 0usize;
    let mut index_words = 0usize;
    let mut origins: Vec<[i32; 3]> = coords
        .iter()
        .map(|&c| RenderRegions::origin_of(c))
        .collect();
    origins.sort();
    origins.dedup();
    for origin in &origins {
        let stats = regions
            .region(SectionCoord(origin[0], origin[1], origin[2]))
            .expect("region still loaded")
            .stats();
        vertex_words += stats.vertex_words;
        index_words += stats.index_words;
    }
    let arena_bytes = vertex_words * 16 + index_words * 4;
    let loaded = coords.len() - evict.len();
    assert_eq!((commands, draws), (16, 495));
    assert_eq!(draws, loaded);
    assert_eq!((vertex_words, index_words), (520704, 781056));
    println!(
        "S4 workload: {} sections across {} regions, {} evicted/iter \
         ({} solid + {} sparse fixture quads)",
        coords.len(),
        origins.len(),
        evict.len(),
        solid_quads.len(),
        sparse_quads.len()
    );
    println!("s4 upload+evict: {s4_upload_median:.2} us/iter ({runs} iters)");
    println!("s4 multidraw: {s4_draw_median:.2} us/iter, {commands} commands, {draws} draws");
    println!(
        "s4 arenas: {vertex_words} vertex words + {index_words} index words \
         ({arena_bytes} bytes active over {loaded} loaded sections)"
    );
}
