// SPDX-License-Identifier: LGPL-3.0-or-later
//! Ordered draw emission: front-to-back opaque draws for Early-Z,
//! back-to-front transparent draws for blending, and zero-allocation
//! scratch reuse across frames.

use megu_client_mesh::draw_order::{DrawOrder, DrawOrderScratch, MultidrawScratch};
use megu_client_mesh::{
    aabb_distance_sq, depth_key, mesh_section, Block, Light, RenderRegions, Section, SectionCoord,
};

fn stone_section_quads() -> Vec<megu_client_mesh::Quad> {
    let section = Section::filled(Block::opaque_cube(1), Light::new(0, 15));
    mesh_section(&section.view())
}

#[test]
fn depth_key_is_monotone_and_scrubs_bad_input() {
    assert!(depth_key(1.0) < depth_key(2.0));
    assert!(depth_key(0.0) == 0);
    assert!(depth_key(1_000_000.0) < u32::MAX);
    // Corrupt inputs pin to "farthest" instead of poisoning the sort.
    assert_eq!(depth_key(f32::NAN), u32::MAX);
    assert_eq!(depth_key(f32::INFINITY), u32::MAX);
    assert_eq!(depth_key(f32::NEG_INFINITY), u32::MAX);
    assert_eq!(depth_key(-1.0), u32::MAX);
}

#[test]
fn aabb_distance_sq_is_nearest_point_distance() {
    // Camera inside the box: zero.
    assert_eq!(
        aabb_distance_sq([0.0; 3], [16.0; 3], [8.0, 8.0, 8.0]),
        0.0
    );
    // Camera 8 blocks west, inside Y/Z range: 8^2.
    assert_eq!(
        aabb_distance_sq([0.0; 3], [16.0; 3], [-8.0, 8.0, 8.0]),
        64.0
    );
    // Two axes out: 8^2 + 8^2.
    assert_eq!(
        aabb_distance_sq([0.0; 3], [16.0; 3], [-8.0, -8.0, 8.0]),
        128.0
    );
}

#[test]
fn draw_order_scratch_sorts_opaque_near_to_far_transparent_far_to_near() {
    let mut scratch = DrawOrderScratch::<u32>::new();
    // (distance², payload id)
    for &(dist, id) in &[(10_000.0f32, 0u32), (100.0, 1), (2_500.0, 2), (1.0, 3)] {
        scratch.push_opaque(dist, id);
        scratch.push_transparent(dist, id);
    }
    scratch.finish();

    let opaque_ids: Vec<u32> = scratch.opaque().iter().map(|k| k.draw).collect();
    assert_eq!(opaque_ids, vec![3, 1, 2, 0], "opaque must run near-to-far");
    let transparent_ids: Vec<u32> = scratch.transparent().iter().map(|k| k.draw).collect();
    assert_eq!(
        transparent_ids,
        vec![0, 2, 1, 3],
        "transparent must run far-to-near"
    );
    // Keys stay attached so callers can inspect or re-key.
    assert!(scratch
        .opaque()
        .windows(2)
        .all(|w| w[0].depth_key <= w[1].depth_key));
    assert!(scratch
        .transparent()
        .windows(2)
        .all(|w| w[0].depth_key >= w[1].depth_key));
}

#[test]
fn draw_order_scratch_reuses_capacity_with_zero_growth_after_warmup() {
    let mut scratch = DrawOrderScratch::<u32>::new();
    // Two warmup frames grow the lists to working size.
    for _ in 0..2 {
        for i in 0..64 {
            scratch.push_opaque(i as f32, i);
            scratch.push_transparent(i as f32, i);
        }
        scratch.finish();
        scratch.clear();
    }
    let events_after_warmup = scratch.growth_events();
    let opaque_cap = scratch.opaque_capacity();
    let transparent_cap = scratch.transparent_capacity();
    assert!(opaque_cap >= 64 && transparent_cap >= 64);

    for _ in 0..8 {
        for i in 0..64 {
            scratch.push_opaque(i as f32, i);
            scratch.push_transparent(i as f32, i);
        }
        scratch.finish();
        scratch.clear();
    }
    assert_eq!(
        scratch.growth_events(),
        events_after_warmup,
        "steady-state frames must not grow any buffer"
    );
    assert_eq!(scratch.opaque_capacity(), opaque_cap);
    assert_eq!(scratch.transparent_capacity(), transparent_cap);
}

fn upload_test_regions() -> RenderRegions {
    let quads = stone_section_quads();
    let mut regions = RenderRegions::default();
    // Sections x=0..8 fill region [0,0,0]; x=8..12 open region [8,0,0].
    for x in 0..12 {
        assert!(regions.upload(SectionCoord(x, 0, 0), &quads));
    }
    regions
}

#[test]
fn multidraw_into_orders_near_commands_and_draws_first() {
    let regions = upload_test_regions();
    // Camera west of everything, inside the section Y/Z slab.
    let camera = [-8.0, 8.0, 8.0];
    let mut scratch = MultidrawScratch::new();

    regions.build_multidraw_into(camera, DrawOrder::FrontToBack, &mut scratch);
    let commands = scratch.commands();
    assert_eq!(commands.len(), 2, "one facing bucket per region");
    // Region [0,0,0] holds the nearest sections and must submit first.
    assert_eq!(commands[0].region, [0, 0, 0]);
    assert_eq!(commands[1].region, [8, 0, 0]);
    // Inside the near command, draws run nearest-x first (base_vertex
    // tracks upload order here: x * quads_per_section * 4 vertices).
    let quads_per_section = stone_section_quads().len() as i32;
    let base_vertices: Vec<i32> = commands[0].draws.iter().map(|d| d.base_vertex).collect();
    assert_eq!(
        base_vertices,
        (0..8).map(|x| x * quads_per_section * 4).collect::<Vec<i32>>()
    );

    // Back-to-front flips both the command stream and the draws inside it.
    regions.build_multidraw_into(camera, DrawOrder::BackToFront, &mut scratch);
    let commands = scratch.commands();
    assert_eq!(commands[0].region, [8, 0, 0]);
    assert_eq!(commands[1].region, [0, 0, 0]);
    let base_vertices: Vec<i32> = commands[1].draws.iter().map(|d| d.base_vertex).collect();
    assert_eq!(
        base_vertices,
        (0..8)
            .rev()
            .map(|x| x * quads_per_section * 4)
            .collect::<Vec<i32>>()
    );
}

#[test]
fn multidraw_scratch_reuses_every_buffer_with_zero_growth_after_warmup() {
    let regions = upload_test_regions();
    let camera = [-8.0, 8.0, 8.0];
    let mut scratch = MultidrawScratch::new();

    // Warmup: the first two frames grow every pool to working size.
    for _ in 0..2 {
        regions.build_multidraw_into(camera, DrawOrder::FrontToBack, &mut scratch);
    }
    let events_after_warmup = scratch.growth_events();
    let baseline: Vec<_> = scratch.commands().to_vec();

    for _ in 0..6 {
        regions.build_multidraw_into(camera, DrawOrder::FrontToBack, &mut scratch);
    }
    assert_eq!(
        scratch.growth_events(),
        events_after_warmup,
        "steady-state multidraw builds must perform zero heap growth"
    );
    // Recycled buffers still emit the identical command stream.
    assert_eq!(scratch.commands(), baseline.as_slice());
}
