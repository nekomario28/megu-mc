// SPDX-License-Identifier: LGPL-3.0-or-later

//! Geometric-equivalence tests for the bitboard greedy section mesher.
//!
//! A naive per-voxel oracle defines the exact set of visible unit faces.
//! Every emitted non-fluid quad is expanded back into the unit faces it
//! covers, and the two sets must match exactly: no missing faces, no extra
//! faces, and no overlapping merged quads.

use std::collections::BTreeSet;

use megu_client_mesh::{
    mesh_section, section_index, Block, Direction, Layer, Light, MissingNeighbor, Quad, Sample,
    SectionView, ALL_FACES, CUTOUT, EDGE, FACE_CELLS, LIGHT_BYTES, OPAQUE,
    SAME_STATE_CULL, SECTION_CELLS, TRANSLUCENT, TINTED, WATER, WATERLOGGED,
};

const DIR_AXIS: [usize; 6] = [1, 1, 2, 2, 0, 0];
const DIR_STEP: [i32; 6] = [-1, 1, -1, 1, -1, 1];

/// Unit face identity shared by the oracle and the quad decoder:
/// (direction, face-plane coordinate, in-plane cell u, in-plane cell v) with
/// the in-plane axes ordered by ascending world-axis index.
type FaceKey = (u8, i32, i32, i32);

fn is_cube(b: Block) -> bool {
    b.flags & (OPAQUE | CUTOUT | TRANSLUCENT) != 0
}

fn occludes(b: Block, d: Direction) -> bool {
    b.flags & OPAQUE != 0 && b.occlusion & d.bit() != 0
}

fn in_plane_axes(axis: usize) -> (usize, usize) {
    match axis {
        0 => (1, 2),
        1 => (0, 2),
        _ => (0, 1),
    }
}

/// Naive branchy face check: for every cube cell and every direction, the
/// face is visible iff the neighbor does not opaquely occlude the opposing
/// side and the pair is not same-state culled.
fn naive_unit_faces(view: &SectionView<'_>) -> BTreeSet<FaceKey> {
    let mut faces = BTreeSet::new();
    for y in 0..EDGE {
        for z in 0..EDGE {
            for x in 0..EDGE {
                let cell = view.blocks[section_index(x, y, z)];
                if !is_cube(cell) {
                    continue;
                }
                for d in Direction::ALL {
                    let axis = DIR_AXIS[d as usize];
                    let step = DIR_STEP[d as usize];
                    let mut np = [x as i32, y as i32, z as i32];
                    np[axis] += step;
                    let nb = view.sample(np).block;
                    if occludes(nb, d.opposite()) {
                        continue;
                    }
                    if cell.flags & SAME_STATE_CULL != 0
                        && nb.flags & SAME_STATE_CULL != 0
                        && nb.state_id == cell.state_id
                    {
                        continue;
                    }
                    let plane = if step > 0 { np[axis] } else { np[axis] + 1 };
                    let (a1, a2) = in_plane_axes(axis);
                    let p = [x as i32, y as i32, z as i32];
                    faces.insert((d as u8, plane, p[a1], p[a2]));
                }
            }
        }
    }
    faces
}

/// Expand an emitted quad into the unit faces it covers. Non-fluid quads are
/// axis-aligned integer rectangles on the face plane.
fn quad_coverage(quad: &Quad) -> Vec<FaceKey> {
    let axis = DIR_AXIS[quad.direction as usize];
    let (a1, a2) = in_plane_axes(axis);
    let plane = quad.positions[0][axis];
    for corner in &quad.positions {
        assert_eq!(
            corner[axis], plane,
            "quad is not planar on its face axis: {quad:?}"
        );
        for (i, c) in corner.iter().enumerate() {
            assert_eq!(
                c.fract(),
                0.0,
                "cube quad corner {i} is not integral: {corner:?}"
            );
        }
    }
    let bounds = |a: usize| {
        quad.positions
            .iter()
            .map(|c| c[a] as i32)
            .fold((i32::MAX, i32::MIN), |(lo, hi), v| (lo.min(v), hi.max(v)))
    };
    let (u0, u1) = bounds(a1);
    let (v0, v1) = bounds(a2);
    let mut faces = Vec::with_capacity(((u1 - u0) * (v1 - v0)) as usize);
    for u in u0..u1 {
        for v in v0..v1 {
            faces.push((quad.direction as u8, plane as i32, u, v));
        }
    }
    faces
}

/// The meshed section must cover exactly the oracle's face set: equal sets
/// and no duplicate coverage (which would mean overlapping merged quads).
fn assert_matches_naive(view: &SectionView<'_>, label: &str) -> Vec<Quad> {
    let quads = mesh_section(view);
    let expected = naive_unit_faces(view);
    let mut covered = BTreeSet::new();
    let mut covered_count = 0usize;
    for quad in &quads {
        if quad.layer == Layer::Fluid {
            continue;
        }
        let faces = quad_coverage(quad);
        covered_count += faces.len();
        covered.extend(faces);
    }
    assert_eq!(
        covered_count,
        covered.len(),
        "{label}: merged quads overlap ({} unit faces covered twice)",
        covered_count - covered.len()
    );
    let missing: Vec<_> = expected.difference(&covered).collect();
    let extra: Vec<_> = covered.difference(&expected).collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "{label}: face-set mismatch: {missing:?} missing, {extra:?} extra"
    );
    quads
}

struct Fixture {
    blocks: Box<[Block; SECTION_CELLS]>,
    block_light: Box<[u8; LIGHT_BYTES]>,
    sky_light: Box<[u8; LIGHT_BYTES]>,
    borders: [Option<Box<[Sample; FACE_CELLS]>>; 6],
    missing: MissingNeighbor,
}

impl Fixture {
    fn new() -> Self {
        Self {
            blocks: Box::new([Block::AIR; SECTION_CELLS]),
            block_light: Box::new([0; LIGHT_BYTES]),
            sky_light: Box::new([0xff; LIGHT_BYTES]),
            borders: [None, None, None, None, None, None],
            missing: MissingNeighbor::Air,
        }
    }

    fn set_nibble(buf: &mut [u8; LIGHT_BYTES], index: usize, level: u8) {
        let shift = (index & 1) * 4;
        let byte = &mut buf[index / 2];
        *byte = (*byte & !(15 << shift)) | (level << shift);
    }

    fn set(&mut self, x: usize, y: usize, z: usize, block: Block, light: Light) {
        let index = section_index(x, y, z);
        self.blocks[index] = block;
        Self::set_nibble(&mut self.block_light, index, light.block());
        Self::set_nibble(&mut self.sky_light, index, light.sky());
    }

    fn set_blocks_where(&mut self, block: Block, mut pred: impl FnMut(usize, usize, usize) -> bool) {
        for y in 0..EDGE {
            for z in 0..EDGE {
                for x in 0..EDGE {
                    if pred(x, y, z) {
                        self.set(x, y, z, block, Light::new(0, 15));
                    }
                }
            }
        }
    }

    fn uniform_border(&mut self, d: Direction, block: Block) {
        self.borders[d as usize] =
            Some(Box::new([Sample::new(block, Light::new(0, 15)); FACE_CELLS]));
    }

    /// Side border plane that is opaque only along `y == 0`, so it culls the
    /// bottom slab's side faces without occluding the top/bottom AO rings.
    fn slab_side_border(&mut self, d: Direction) {
        let mut plane = Box::new([Sample::new(Block::AIR, Light::new(0, 15)); FACE_CELLS]);
        for (i, sample) in plane.iter_mut().enumerate() {
            let y = match d {
                Direction::West | Direction::East => i % EDGE,
                _ => i / EDGE,
            };
            if y == 0 {
                *sample = Sample::new(Block::opaque_cube(3), Light::new(0, 15));
            }
        }
        self.borders[d as usize] = Some(plane);
    }

    fn view(&self) -> SectionView<'_> {
        SectionView {
            blocks: &self.blocks,
            block_light: &self.block_light,
            sky_light: &self.sky_light,
            borders: std::array::from_fn(|i| self.borders[i].as_deref()),
            edges_and_corners: None,
            missing: self.missing,
        }
    }
}

#[test]
fn air_only_section_emits_zero_quads() {
    let fixture = Fixture::new();
    assert!(mesh_section(&fixture.view()).is_empty());
}

#[test]
fn fully_buried_solid_section_emits_zero_quads() {
    let mut fixture = Fixture::new();
    fixture.missing = MissingNeighbor::Opaque;
    fixture.set_blocks_where(Block::opaque_cube(1), |_, _, _| true);
    assert!(mesh_section(&fixture.view()).is_empty());
}

#[test]
fn solid_section_collapses_to_six_quads() {
    let mut fixture = Fixture::new();
    fixture.set_blocks_where(Block::opaque_cube(1), |_, _, _| true);
    let quads = assert_matches_naive(&fixture.view(), "solid");
    assert_eq!(quads.len(), 6);
    let dirs: BTreeSet<u8> = quads.iter().map(|q| q.direction as u8).collect();
    assert_eq!(dirs.len(), 6);
}

#[test]
fn flat_slab_merges_to_two_quads() {
    // A 16x16 stone layer at y = 0. Opaque side borders cull the four
    // side-strip faces, leaving only the top and bottom 16x16 sheets —
    // two greedy quads instead of 512 unit faces.
    let mut fixture = Fixture::new();
    fixture.set_blocks_where(Block::opaque_cube(2), |_, y, _| y == 0);
    for d in [
        Direction::North,
        Direction::South,
        Direction::West,
        Direction::East,
    ] {
        fixture.slab_side_border(d);
    }
    let quads = assert_matches_naive(&fixture.view(), "flat_slab");
    assert_eq!(quads.len(), 2);
    let mut dirs: Vec<u8> = quads.iter().map(|q| q.direction as u8).collect();
    dirs.sort_unstable();
    assert_eq!(dirs, [Direction::Down as u8, Direction::Up as u8]);
}

#[test]
fn flat_slab_open_sides_merge_to_six_quads() {
    // Same slab against air borders: each of the four exposed side strips
    // also merges into a single quad.
    let mut fixture = Fixture::new();
    fixture.set_blocks_where(Block::opaque_cube(2), |_, y, _| y == 0);
    let quads = assert_matches_naive(&fixture.view(), "flat_slab_open");
    assert_eq!(quads.len(), 6);
}

#[test]
fn checkerboard_matches_naive_oracle() {
    let mut fixture = Fixture::new();
    fixture.set_blocks_where(Block::opaque_cube(4), |x, y, z| (x + y + z) % 2 == 0);
    let quads = assert_matches_naive(&fixture.view(), "checkerboard");
    // Every cube cell is isolated: 2048 cells x 6 faces, nothing merges.
    assert_eq!(quads.len(), 2048 * 6);
}

#[test]
fn hollow_box_matches_naive_oracle() {
    let mut fixture = Fixture::new();
    fixture.set_blocks_where(Block::opaque_cube(5), |x, y, z| {
        x == 0 || x == 15 || y == 0 || y == 15 || z == 0 || z == 15
    });
    let quads = assert_matches_naive(&fixture.view(), "hollow_box");
    // Six outer 16x16 sheets merge fully; the six inner 14x14 sheets split
    // where wall occlusion changes the AO ring, leaving 54 inner quads.
    assert_eq!(quads.len(), 60);
}

#[test]
fn caves_match_naive_oracle() {
    let mut fixture = Fixture::new();
    // Deterministic cave-ish density: ~57% solid with connected air pockets.
    fixture.set_blocks_where(Block::opaque_cube(6), |x, y, z| {
        (x * 31 + y * 17 + z * 7 + x * z) % 7 < 4
    });
    assert_matches_naive(&fixture.view(), "caves");
}

#[test]
fn neighbor_borders_cull_and_match_naive_oracle() {
    // West half solid: with an opaque east border plane the x=15 faces are
    // culled; with air neighbors they are exposed. Both must match the
    // oracle, which goes through the same `SectionView::sample` path.
    let mut fixture = Fixture::new();
    fixture.set_blocks_where(Block::opaque_cube(7), |x, _, _| x >= 8);
    fixture.uniform_border(Direction::East, Block::opaque_cube(8));
    let quads = assert_matches_naive(&fixture.view(), "bordered");
    assert!(
        quads
            .iter()
            .all(|q| !(q.direction == Direction::East
                && q.positions.iter().all(|c| c[0] == 16.0))),
        "east faces at the opaque border must be culled"
    );
    assert_matches_naive(&fixture.view(), "bordered");

    let mut open = Fixture::new();
    open.set_blocks_where(Block::opaque_cube(7), |x, _, _| x >= 8);
    let open_quads = assert_matches_naive(&open.view(), "open");
    assert!(open_quads.len() > quads.len());
}

#[test]
fn same_state_cull_suppresses_shared_translucent_faces() {
    let glass = |state: u32| Block {
        state_id: state,
        flags: TRANSLUCENT | SAME_STATE_CULL,
        ..Block::AIR
    };
    let mut fixture = Fixture::new();
    fixture.set(4, 4, 4, glass(519), Light::new(0, 15));
    fixture.set(5, 4, 4, glass(519), Light::new(0, 15));
    fixture.set(7, 4, 4, glass(519), Light::new(0, 15));
    fixture.set(8, 4, 4, glass(520), Light::new(0, 15));
    let quads = assert_matches_naive(&fixture.view(), "same_state_cull");
    let covered: BTreeSet<FaceKey> = quads
        .iter()
        .filter(|q| q.layer != Layer::Fluid)
        .flat_map(quad_coverage)
        .collect();
    // Shared faces between the two same-state cells are culled ...
    assert!(!covered.contains(&(Direction::East as u8, 5, 4, 4)));
    assert!(!covered.contains(&(Direction::West as u8, 5, 4, 4)));
    // ... but the state change re-exposes both sides.
    assert!(covered.contains(&(Direction::East as u8, 8, 4, 4)));
    assert!(covered.contains(&(Direction::West as u8, 8, 4, 4)));
}

#[test]
fn mixed_materials_match_naive_oracle() {
    let mut fixture = Fixture::new();
    let glass = |state: u32| Block {
        state_id: state,
        flags: TRANSLUCENT | SAME_STATE_CULL,
        ..Block::AIR
    };
    let leaves = Block {
        state_id: 238,
        tint: 0x228833,
        flags: CUTOUT | TINTED,
        ..Block::AIR
    };
    let partial = Block {
        state_id: 20,
        flags: OPAQUE,
        occlusion: ALL_FACES & !Direction::Up.bit(),
        ..Block::AIR
    };
    let waterlogged = Block {
        state_id: 42,
        flags: OPAQUE | WATERLOGGED,
        occlusion: ALL_FACES,
        ..Block::AIR
    };
    let water = Block {
        fluid_kind: WATER,
        fluid_level: 0,
        ..Block::AIR
    };
    // Stone platform, glass wall (same-state runs + a state seam), a leaves
    // patch, a partially-occluding block with an exposed underside above it,
    // a waterlogged stone and a pool of real fluid.
    fixture.set_blocks_where(Block::opaque_cube(1), |_, y, _| y == 0);
    for x in 0..EDGE {
        for y in 1..5 {
            fixture.set(x, y, 8, glass(519), Light::new(0, 15));
            fixture.set(x, y, 9, glass(if x < 8 { 519 } else { 520 }), Light::new(0, 15));
        }
    }
    fixture.set_blocks_where(leaves, |x, y, z| (6..10).contains(&x) && y == 6 && (6..10).contains(&z));
    fixture.set(3, 2, 3, partial, Light::new(0, 15));
    fixture.set(3, 3, 3, Block::opaque_cube(9), Light::new(0, 15));
    fixture.set(12, 1, 12, waterlogged, Light::new(0, 15));
    for x in 12..15 {
        for z in 2..5 {
            fixture.set(x, 1, z, water, Light::new(0, 15));
        }
    }
    let quads = assert_matches_naive(&fixture.view(), "mixed");
    assert!(
        quads.iter().any(|q| q.layer == Layer::Fluid),
        "water cells must emit fluid quads"
    );
    assert!(
        quads.iter().any(|q| q.layer == Layer::Translucent),
        "glass cells must emit translucent quads"
    );
    assert!(
        quads.iter().any(|q| q.layer == Layer::Cutout),
        "leaves cells must emit cutout quads"
    );
}

#[test]
fn light_differences_prevent_greedy_merging() {
    // Flat slab with two light levels on the top surface: the greedy merge
    // must split at the light boundary instead of producing one quad.
    let mut fixture = Fixture::new();
    for x in 0..EDGE {
        for z in 0..EDGE {
            let light = if x < 8 { 15 } else { 0 };
            fixture.set(x, 8, z, Block::opaque_cube(1), Light::new(light, light));
        }
    }
    for d in [
        Direction::North,
        Direction::South,
        Direction::West,
        Direction::East,
    ] {
        fixture.uniform_border(d, Block::opaque_cube(3));
    }
    let view = fixture.view();
    let quads = assert_matches_naive(&view, "light_split");
    let up_quads = quads
        .iter()
        .filter(|q| q.direction == Direction::Up)
        .count();
    assert!(
        up_quads >= 2,
        "top face must split across the light boundary, got {up_quads}"
    );
}

#[test]
fn ao_differences_prevent_greedy_merging() {
    // A single bump block occludes the sky-side neighbors of adjacent top
    // faces, giving them different AO than the open rest of the slab: the
    // merge must break even though block and light are uniform.
    let mut fixture = Fixture::new();
    fixture.set_blocks_where(Block::opaque_cube(1), |_, y, _| y == 0);
    fixture.set(4, 1, 4, Block::opaque_cube(1), Light::new(0, 15));
    for d in [
        Direction::North,
        Direction::South,
        Direction::West,
        Direction::East,
    ] {
        fixture.uniform_border(d, Block::opaque_cube(3));
    }
    let quads = assert_matches_naive(&fixture.view(), "ao_split");
    let up_quads = quads
        .iter()
        .filter(|q| q.direction == Direction::Up)
        .count();
    assert!(
        up_quads >= 2,
        "AO boundary must stop the greedy merge, got {up_quads} up quads"
    );
}
