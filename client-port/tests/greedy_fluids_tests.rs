// SPDX-License-Identifier: LGPL-3.0-or-later

//! 2D greedy merging of flat fluid +/-Y faces.
//!
//! A naive per-cell oracle defines the exact set of exposed unit faces on
//! the top and bottom of a fluid volume. Every emitted fluid quad is
//! expanded back into the unit cells it covers on the face plane; the
//! merged output must cover exactly the oracle set — same exposed surface
//! area, no missing cells, no overlaps — while collapsing identical flat
//! sheets into single quads.
//!
//! Every fixture fills the whole section with the fluid and continues it
//! past the sides via water border planes plus an edge/corner halo, so the
//! y=15 surface's corner heights are uniform across the rim — exactly like
//! an ocean spanning multiple sections.

use std::collections::BTreeSet;

use megu_client_mesh::ffi::*;
use megu_client_mesh::{
    edge_corner_index, mesh_section, Block, Direction, Layer, Light, Quad, Sample, Section,
    SectionView, EDGE, EDGE_CORNER_CELLS, FACE_CELLS, FLUID_TINTED, LAVA, QUAD_TINTED, WATER,
};

const SIDES: [Direction; 4] = [
    Direction::North,
    Direction::South,
    Direction::West,
    Direction::East,
];

const WATER_TOP: usize = EDGE - 1;

fn water(state_id: u32, level: u8) -> Block {
    Block {
        state_id,
        fluid_kind: WATER,
        fluid_level: level,
        ..Block::AIR
    }
}

fn lava(state_id: u32) -> Block {
    Block {
        state_id,
        fluid_kind: LAVA,
        fluid_level: 0,
        ..Block::AIR
    }
}

fn tinted_water(state_id: u32, tint: u32) -> Block {
    Block {
        flags: FLUID_TINTED,
        fluid_tint: tint,
        ..water(state_id, 0)
    }
}

/// Edge/corner halo continuing the fluid through every in-range y row and
/// leaving the rows above/below the section as air — the same surface the
/// neighboring sections' y=16 air would provide.
fn fluid_halo(block: Block) -> [Sample; EDGE_CORNER_CELLS] {
    let mut halo = [Sample::new(Block::AIR, Light::new(0, 15)); EDGE_CORNER_CELLS];
    for x in -1..=EDGE as i32 {
        for y in -1..=EDGE as i32 {
            for z in -1..=EDGE as i32 {
                if let Some(index) = edge_corner_index([x, y, z]) {
                    if (0..EDGE as i32).contains(&y) {
                        halo[index] = Sample::new(block, Light::new(0, 15));
                    }
                }
            }
        }
    }
    halo
}

/// View of a fluid-filled section continued past the sides. `cull`
/// directions get an opaque border plane (hiding that face entirely);
/// every other border stays at the `MissingNeighbor::Air` default, so a
/// fluid face facing it is exposed.
fn ocean_view<'a>(
    section: &'a Section,
    planes: &'a mut [[Sample; FACE_CELLS]; 6],
    halo: &'a mut [Sample; EDGE_CORNER_CELLS],
    border: Block,
    cull: &[Direction],
) -> SectionView<'a> {
    *halo = fluid_halo(border);
    let light = Light::new(0, 15);
    let side = [Sample::new(border, light); FACE_CELLS];
    for d in SIDES {
        planes[d as usize] = side;
    }
    for &d in cull {
        planes[d as usize] = [Sample::new(Block::opaque_cube(9), light); FACE_CELLS];
    }
    let mut view = section.view();
    view.edges_and_corners = Some(halo);
    for d in SIDES {
        view.borders[d as usize] = Some(&planes[d as usize]);
    }
    for &d in cull {
        view.borders[d as usize] = Some(&planes[d as usize]);
    }
    view
}

/// Unit-face identity on a fluid +/-Y face plane: (x, z) cell of the
/// section slice the face covers.
type FaceKey = (i32, i32);

/// Expand a fluid top/bottom quad into the unit cells it covers.
fn fluid_coverage(quad: &Quad) -> Vec<FaceKey> {
    assert!(
        quad.direction == Direction::Down || quad.direction == Direction::Up,
        "coverage helper only handles +/-Y faces"
    );
    let bounds = |a: usize| {
        quad.positions
            .iter()
            .map(|c| c[a] as i32)
            .fold((i32::MAX, i32::MIN), |(lo, hi), v| (lo.min(v), hi.max(v)))
    };
    let (x0, x1) = bounds(0);
    let (z0, z1) = bounds(2);
    let mut cells = Vec::with_capacity(((x1 - x0) * (z1 - z0)) as usize);
    for x in x0..x1 {
        for z in z0..z1 {
            cells.push((x, z));
        }
    }
    cells
}

/// Assert the emitted fluid quads for `direction` cover exactly
/// `expected` unit cells — no gaps and no double-covered cells — and
/// return how many quads produced that coverage.
fn assert_coverage(
    quads: &[Quad],
    direction: Direction,
    expected: &BTreeSet<FaceKey>,
    label: &str,
) -> usize {
    let mut covered = BTreeSet::new();
    let mut covered_count = 0usize;
    let mut quad_count = 0usize;
    for quad in quads {
        if quad.layer != Layer::Fluid || quad.direction != direction {
            continue;
        }
        quad_count += 1;
        let cells = fluid_coverage(quad);
        covered_count += cells.len();
        covered.extend(cells);
    }
    assert_eq!(
        covered_count,
        covered.len(),
        "{label}: merged fluid quads overlap ({} cells covered twice)",
        covered_count - covered.len()
    );
    let missing: Vec<_> = expected.difference(&covered).collect();
    let extra: Vec<_> = covered.difference(expected).collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "{label}: coverage mismatch: {missing:?} missing, {extra:?} extra"
    );
    quad_count
}

fn full_slice() -> BTreeSet<FaceKey> {
    (0..EDGE as i32)
        .flat_map(|x| (0..EDGE as i32).map(move |z| (x, z)))
        .collect()
}

fn test_planes() -> [[Sample; FACE_CELLS]; 6] {
    [[Sample::new(Block::AIR, Light::new(0, 15)); FACE_CELLS]; 6]
}

fn test_halo() -> [Sample; EDGE_CORNER_CELLS] {
    [Sample::new(Block::AIR, Light::new(0, 15)); EDGE_CORNER_CELLS]
}

#[test]
fn flat_ocean_top_merges_256_cells_to_one_quad() {
    // Full water section + continued borders: only the 256 top unit faces
    // are exposed, and they must collapse into a single greedy quad.
    let section = Section::filled(water(80, 0), Light::new(0, 15));
    let mut planes = test_planes();
    let mut halo = test_halo();
    let view = ocean_view(&section, &mut planes, &mut halo, water(80, 0), &[Direction::Down]);
    let quads = mesh_section(&view);
    assert_eq!(quads.len(), 1, "expected one merged quad, got {quads:?}");
    let quad = &quads[0];
    assert_eq!(quad.direction, Direction::Up);
    assert_eq!(quad.layer, Layer::Fluid);
    assert_eq!(quad.fluid_kind, WATER);
    // All four corners share one height: the quad is flat.
    let ys: BTreeSet<_> = quad.positions.iter().map(|p| p[1].to_bits()).collect();
    assert_eq!(ys.len(), 1, "merged top is not coplanar: {quad:?}");
    assert_coverage(&quads, Direction::Up, &full_slice(), "flat_ocean");
}

#[test]
fn flat_ocean_bottom_also_merges() {
    // Same ocean open to air on both ends: the 256 exposed undersides
    // merge to one quad just like the 256 tops.
    let section = Section::filled(water(80, 0), Light::new(0, 15));
    let mut planes = test_planes();
    let mut halo = test_halo();
    let view = ocean_view(&section, &mut planes, &mut halo, water(80, 0), &[]);
    let quads = mesh_section(&view);
    assert_eq!(quads.len(), 2, "expected merged top + bottom, got {quads:?}");
    assert_eq!(assert_coverage(&quads, Direction::Up, &full_slice(), "up"), 1);
    assert_eq!(
        assert_coverage(&quads, Direction::Down, &full_slice(), "down"),
        1
    );
}

#[test]
fn light_boundary_splits_merged_top() {
    // Same still water everywhere, but the top layer's left half carries
    // block light: the face light differs, so the sheet must split.
    let mut section = Section::filled(water(80, 0), Light::new(0, 15));
    for z in 0..EDGE {
        for x in 0..EDGE {
            let light = if x < 8 { 5 } else { 0 };
            let _ = section.set(x, WATER_TOP, z, water(80, 0), Light::new(light, 15));
        }
    }
    let mut planes = test_planes();
    let mut halo = test_halo();
    let view = ocean_view(&section, &mut planes, &mut halo, water(80, 0), &[Direction::Down]);
    let quads = mesh_section(&view);
    let up_quads = assert_coverage(&quads, Direction::Up, &full_slice(), "light_split");
    assert!(
        up_quads >= 2,
        "top must split across the light boundary, got {up_quads}"
    );
}

#[test]
fn tint_and_state_boundaries_split_merged_top() {
    // Left half tinted + state 80, right half tinted differently + state
    // 81: material and state differences must both prevent merging.
    let mut section = Section::filled(water(80, 0), Light::new(0, 15));
    for z in 0..EDGE {
        for x in 0..EDGE {
            let block = if x < 8 {
                tinted_water(80, 0x113355)
            } else {
                tinted_water(81, 0x224466)
            };
            let _ = section.set(x, WATER_TOP, z, block, Light::new(0, 15));
        }
    }
    let mut planes = test_planes();
    let mut halo = test_halo();
    let view = ocean_view(&section, &mut planes, &mut halo, water(80, 0), &[Direction::Down]);
    let quads = mesh_section(&view);
    let up_quads = assert_coverage(&quads, Direction::Up, &full_slice(), "tint_split");
    assert_eq!(up_quads, 2, "expected two 8x16 sheets, got {up_quads}");
    for quad in quads.iter().filter(|q| q.direction == Direction::Up) {
        let west = quad.positions.iter().all(|p| p[0] <= 8.0);
        assert_eq!(
            quad.tint,
            if west { 0x113355 } else { 0x224466 },
            "tint leaked across the merge boundary"
        );
        assert!(quad.flags & QUAD_TINTED != 0);
    }
}

#[test]
fn differing_flat_levels_never_merge() {
    // Two flat pools at different source levels on the surface layer:
    // the top must split at the height boundary (coverage stays exact).
    let mut section = Section::filled(water(80, 0), Light::new(0, 15));
    for z in 0..EDGE {
        for x in 0..EDGE {
            let _ = section.set(
                x,
                WATER_TOP,
                z,
                water(80, if x < 8 { 0 } else { 4 }),
                Light::new(0, 15),
            );
        }
    }
    let mut planes = test_planes();
    let mut halo = test_halo();
    let view = ocean_view(&section, &mut planes, &mut halo, water(80, 0), &[Direction::Down]);
    let quads = mesh_section(&view);
    let up_quads = assert_coverage(&quads, Direction::Up, &full_slice(), "level_split");
    assert!(
        up_quads >= 2,
        "top must split across the height boundary, got {up_quads}"
    );
}

#[test]
fn flowing_gradient_emits_unmerged_sloped_faces() {
    // A west-to-east level ramp on the surface: sloped tops are not flat,
    // so the mesher must keep mostly per-cell faces instead of merging.
    let mut section = Section::filled(water(80, 0), Light::new(0, 15));
    for z in 0..EDGE {
        for x in 0..EDGE {
            let _ = section.set(
                x,
                WATER_TOP,
                z,
                water(80, (7 - x.min(14) / 2) as u8),
                Light::new(0, 15),
            );
        }
    }
    let mut planes = test_planes();
    let mut halo = test_halo();
    let view = ocean_view(&section, &mut planes, &mut halo, water(80, 0), &[Direction::Down]);
    let quads = mesh_section(&view);
    let up_quads = assert_coverage(&quads, Direction::Up, &full_slice(), "flowing");
    assert!(
        up_quads > EDGE,
        "sloped top must not collapse to merged sheets: {up_quads}"
    );
    // Non-flat quads stay unit quads; only genuinely flat runs may merge.
    for quad in quads.iter().filter(|q| q.direction == Direction::Up) {
        let ys: BTreeSet<_> = quad.positions.iter().map(|p| p[1].to_bits()).collect();
        if ys.len() > 1 {
            assert_eq!(
                fluid_coverage(quad).len(),
                1,
                "non-flat top quad covers more than one cell: {quad:?}"
            );
        }
    }
}

#[test]
fn lava_ocean_merges_the_same_way() {
    let section = Section::filled(lava(90), Light::new(0, 15));
    let mut planes = test_planes();
    let mut halo = test_halo();
    let view = ocean_view(&section, &mut planes, &mut halo, lava(90), &[Direction::Down]);
    let quads = mesh_section(&view);
    assert_eq!(quads.len(), 1, "lava ocean should merge identically: {quads:?}");
    assert_eq!(quads[0].fluid_kind, LAVA);
    assert_coverage(&quads, Direction::Up, &full_slice(), "lava");
}

#[test]
fn merged_fluid_quad_packs_real_width_and_height() {
    // The packed vertex attributes must carry the merged 16x16 extents so
    // the shader tiles UVs across the whole sheet.
    let section = Section::filled(water(80, 0), Light::new(0, 15));
    let mut planes = test_planes();
    let mut halo = test_halo();
    let view = ocean_view(&section, &mut planes, &mut halo, water(80, 0), &[Direction::Down]);
    let quads = mesh_section(&view);
    assert_eq!(quads.len(), 1);
    let quad = quads[0];
    let mut output = [PackedVertex::default(); 4];
    let mut count = 0usize;
    // SAFETY: one valid quad and four disjoint writable vertex slots.
    assert_eq!(
        unsafe {
            megu_client_mesh_pack_vertices(
                &quad,
                1,
                output.as_mut_ptr(),
                output.len(),
                &mut count,
            )
        },
        ABI_OK
    );
    assert_eq!(count, 4);
    for vertex in output {
        assert_eq!((vertex.attributes >> 20) & 31, 16, "packed width");
        assert_eq!((vertex.attributes >> 25) & 31, 16, "packed height");
        assert_eq!((vertex.attributes >> 15) & 3, u32::from(WATER));
    }
}
