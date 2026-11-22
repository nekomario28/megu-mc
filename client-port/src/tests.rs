// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

use crate::*;

const DARK: Light = Light::new(0, 0);
const DAY: Light = Light::new(0, 15);
const STONE: Block = Block::opaque_cube(1);

fn air() -> Section {
    Section::filled(Block::AIR, DAY)
}

fn fluid(kind: u8, level: u8) -> Block {
    let base = if kind == WATER { 80 } else { 96 };
    Block {
        state_id: base + u32::from(level),
        fluid_kind: kind,
        fluid_level: level,
        ..Block::AIR
    }
}

fn face(quads: &[Quad], direction: Direction, state_id: u32) -> &Quad {
    quads
        .iter()
        .find(|q| q.direction == direction && q.state_id == state_id)
        .unwrap()
}

fn normal(q: &Quad) -> [f32; 3] {
    let [a, b, c, _] = q.positions;
    let u: [f32; 3] = std::array::from_fn(|i| b[i] - a[i]);
    let v: [f32; 3] = std::array::from_fn(|i| c[i] - a[i]);
    [
        u[1] * v[2] - u[2] * v[1],
        u[2] * v[0] - u[0] * v[2],
        u[0] * v[1] - u[1] * v[0],
    ]
}

#[test]
fn layout_preserves_states_and_both_light_nibbles() {
    let mut section = air();
    let i = 3 * 256 + 5 * 16 + 6;
    section.blocks[i] = Block::opaque_cube(26_683);
    section.blocks[i + 1] = Block::opaque_cube(700);
    section.block_light[i / 2] = 0xa3;
    section.sky_light[i / 2] = 0xb4;
    assert_eq!(
        section.get(6, 3, 5),
        Sample::new(Block::opaque_cube(26_683), Light::new(3, 4))
    );
    assert_eq!(
        section.get(7, 3, 5),
        Sample::new(Block::opaque_cube(700), Light::new(10, 11))
    );
    assert!(section.set(6, 3, 5, STONE, Light::new(1, 2)));
    assert_eq!(section.block_light[i / 2], 0xa1);
    assert_eq!(section.sky_light[i / 2], 0xb2);
    assert!(!section.set(6, 3, 5, STONE, Light::new(1, 2)));
    assert!(!section.set(16, 0, 0, STONE, DARK));
    let quads = mesh_section(&section.view());
    assert_eq!(
        face(&quads, Direction::Up, 700).face_light,
        Light::new(10, 15)
    );
}

#[test]
fn uniform_cube_reduces_to_six_outward_quads() {
    let section = Section::filled(STONE, DARK);
    let mut view = section.view();
    let quads = mesh_section(&view);
    assert_eq!(quads.len(), 6);
    let normals = [
        [0.0, -1.0, 0.0],
        [0.0, 1.0, 0.0],
        [0.0, 0.0, -1.0],
        [0.0, 0.0, 1.0],
        [-1.0, 0.0, 0.0],
        [1.0, 0.0, 0.0],
    ];
    let mut shell_area = 0.0;
    for (q, n) in quads.iter().zip(normals) {
        assert_eq!(normal(q), n.map(|a| a * 256.0));
        assert_eq!(q.ao, [3; 4]);
        assert_eq!(q.sky_light, [15; 4]);
        assert_eq!(q.indices(), [0, 1, 2, 0, 2, 3]);
        shell_area += normal(q).into_iter().map(f32::abs).sum::<f32>();
    }
    assert_eq!(shell_area, 1536.0);
    assert_eq!(section.blocks.len() * Direction::ALL.len(), 24_576);
    view.missing = MissingNeighbor::Opaque;
    assert!(mesh_section(&view).is_empty());
}

#[test]
fn all_borders_use_world_axes_and_directional_occlusion() {
    let points = [
        [6, 0, 8],
        [6, 15, 8],
        [6, 8, 0],
        [6, 8, 15],
        [0, 6, 8],
        [15, 6, 8],
    ];
    let indices = [8 * 16 + 6; 6];
    for ((direction, [x, y, z]), index) in Direction::ALL.into_iter().zip(points).zip(indices) {
        let mut section = air();
        section.set(x, y, z, STONE, DARK);
        let mut border = [Sample::new(Block::AIR, DAY); FACE_CELLS];
        border[index] = Sample::new(Block::opaque_cube(1), DARK);
        let mut view = section.view();
        view.borders[direction as usize] = Some(&border);
        let quads = mesh_section(&view);
        assert_eq!(quads.len(), 5, "{direction:?}");
        assert!(quads.iter().all(|q| q.direction != direction));

        border[index].block.occlusion = direction.bit();
        let mut view = section.view();
        view.borders[direction as usize] = Some(&border);
        assert_eq!(mesh_section(&view).len(), 6, "{direction:?}");
    }
}

#[test]
fn glass_leaves_and_water_do_not_cull_opaque_faces() {
    let section = Section::filled(STONE, DARK);
    let mut view = section.view();
    let opaque = [Sample::new(STONE, DARK); FACE_CELLS];
    view.borders[Direction::East as usize] = Some(&opaque);
    assert_eq!(mesh_section(&view).len(), 5);
    let glass = Block {
        state_id: 519,
        flags: TRANSLUCENT | SAME_STATE_CULL,
        ..STONE
    };
    let leaves = Block {
        state_id: 238,
        flags: CUTOUT,
        ..STONE
    };
    for neighbor in [glass, leaves, fluid(WATER, 0)] {
        let border = [Sample::new(neighbor, DAY); FACE_CELLS];
        let mut view = section.view();
        view.borders[Direction::East as usize] = Some(&border);
        let quads = mesh_section(&view);
        assert_eq!(quads.len(), 6);
        assert_eq!(normal(face(&quads, Direction::East, 1)), [256.0, 0.0, 0.0]);
    }

    for (block, layer, expected) in [(glass, Layer::Translucent, 6), (leaves, Layer::Cutout, 8)] {
        let mut pair = air();
        pair.set(3, 5, 5, block, DAY);
        pair.set(4, 5, 5, block, DAY);
        let quads = mesh_section(&pair.view());
        assert_eq!(quads.len(), expected);
        assert!(quads.iter().all(|q| q.layer == layer));
    }
}

#[test]
fn face_key_preserves_state_tint_and_directional_light() {
    let mut section = air();
    let red = Block {
        flags: OPAQUE | TINTED,
        tint: 0xff0000,
        ..STONE
    };
    let green = Block {
        tint: 0x00ff00,
        ..red
    };
    for (x, block) in [
        (3, red),
        (4, green),
        (5, green),
        (
            6,
            Block {
                state_id: 2,
                ..green
            },
        ),
    ] {
        section.set(x, 4, 3, block, DARK);
    }
    section.set(5, 5, 3, Block::AIR, Light::new(6, 9));
    let up: Vec<_> = mesh_section(&section.view())
        .into_iter()
        .filter(|q| q.direction == Direction::Up)
        .collect();
    assert_eq!(up.len(), 4);
    assert_eq!(
        up.iter()
            .map(|q| (q.state_id, q.tint, q.face_light))
            .collect::<Vec<_>>(),
        [
            (1, red.tint, DAY),
            (1, green.tint, DAY),
            (1, green.tint, Light::new(6, 9)),
            (2, green.tint, DAY),
        ]
    );
    assert!(up.iter().all(|q| q.flags == QUAD_TINTED));
}

#[test]
fn ambient_occlusion_flips_the_diagonal_and_keeps_smooth_corner_light() {
    let mut section = air();
    section.set(8, 8, 8, STONE, DARK);
    section.set(7, 9, 9, STONE, DARK);
    section.set(9, 9, 7, STONE, DARK);
    section.set(7, 9, 8, Block::AIR, Light::new(8, 11));
    let top = |q: &Quad| q.direction == Direction::Up && q.positions[0] == [8.0, 9.0, 8.0];
    let quads = mesh_section(&section.view());
    let q = quads.iter().find(|q| top(q)).unwrap();
    assert_eq!(q.ao, [3, 2, 3, 2]);
    assert_eq!(q.block_light, [2, 2, 0, 0]);
    assert_eq!(q.sky_light, [14, 10, 15, 11]);
    assert_eq!(q.diagonal, 1);
    assert_eq!(q.indices(), [0, 1, 3, 1, 2, 3]);

    section.set(7, 9, 9, Block::AIR, DAY);
    section.set(9, 9, 7, Block::AIR, DAY);
    section.set(7, 9, 7, STONE, DARK);
    section.set(9, 9, 9, STONE, DARK);
    let quads = mesh_section(&section.view());
    let q = quads.iter().find(|q| top(q)).unwrap();
    assert_eq!(q.ao, [2, 3, 2, 3]);
    assert_eq!(q.diagonal, 0);
}

#[test]
fn greedy_merge_preserves_a_gradient_perpendicular_to_the_merge_axis() {
    let mut section = air();
    for z in 4..=5 {
        section.set(6, 7, z, STONE, DARK);
    }
    for z in 3..=6 {
        section.set(7, 8, z, Block::AIR, Light::new(12, 15));
    }
    // The gradient is in X; both faces share that gradient along Z (U).
    let locate = |q: &Quad| q.direction == Direction::Up && q.positions[0] == [6.0, 8.0, 4.0];
    let quads = mesh_section(&section.view());
    let q = quads.iter().find(|q| locate(q)).unwrap();
    assert_eq!(normal(q), [0.0, 2.0, 0.0]);
    assert_eq!(q.block_light, [0, 0, 6, 6]);
}

#[test]
fn all_fluid_levels_control_surfaces_and_side_heights() {
    for kind in [WATER, LAVA] {
        for level in 0..=15 {
            let mut section = air();
            let block = fluid(kind, level);
            section.set(4, 3, 5, block, DARK);
            let quads = mesh_section(&section.view());
            assert_eq!(quads.len(), 6);
            let top = face(&quads, Direction::Up, block.state_id);
            let expected = match level {
                1..=7 => f32::from(8 - level) / 36.0,
                _ => 80.0 / 117.0,
            };
            assert!(top
                .positions
                .iter()
                .all(|p| (p[1] - 3.0 - expected).abs() < 0.000_001));
            assert_eq!(top.fluid_kind, kind);
            assert_eq!(top.flags & QUAD_FALLING != 0, level >= 8);
            assert_eq!(top.flow, [0.0, 0.0]);
            assert_eq!(
                face(&quads, Direction::East, block.state_id).positions[1][1],
                top.positions[2][1]
            );
        }
    }
}

#[test]
fn opaque_ceiling_only_occludes_a_fluid_top_that_reaches_it() {
    let mut section = air();
    let thin = fluid(WATER, 7);
    section.set(8, 4, 8, thin, DAY);
    section.set(8, 5, 8, STONE, DARK);
    let quads = mesh_section(&section.view());
    let top = face(&quads, Direction::Up, thin.state_id);
    assert!(top.positions.iter().all(|p| p[1] < 5.0));

    for x in [7, 9] {
        for z in [7, 9] {
            section.set(x, 4, z, fluid(WATER, 0), DAY);
            section.set(x, 5, z, fluid(WATER, 0), DAY);
        }
    }
    let quads = mesh_section(&section.view());
    assert!(quads
        .iter()
        .all(|q| q.state_id != thin.state_id || q.direction != Direction::Up));
}

#[test]
fn fluid_continuity_waterlogging_and_submerged_faces() {
    let mut section = air();
    let source = fluid(WATER, 0);
    let thin = fluid(WATER, 7);
    section.set(4, 4, 4, source, DAY);
    section.set(5, 4, 4, thin, DAY);
    let logged = Block {
        state_id: 237,
        flags: CUTOUT | WATERLOGGED | FLUID_TINTED,
        fluid_tint: 0x112233,
        ..Block::AIR
    };
    section.set(4, 5, 4, logged, DAY);
    let quads = mesh_section(&section.view());
    assert!(quads.iter().all(|q| q.state_id != source.state_id
        || ![Direction::Up, Direction::East].contains(&q.direction)));
    let quads: Vec<_> = quads
        .into_iter()
        .filter(|q| q.layer == Layer::Fluid)
        .collect();
    let thin_top = face(&quads, Direction::Up, thin.state_id);
    assert_eq!(thin_top.positions[0][1], 5.0);
    assert_eq!(thin_top.positions[1][1], 5.0);
    assert!(thin_top.positions[2][1] < 5.0 && thin_top.flow[0] > 0.0);
    let logged_top = face(&quads, Direction::Up, logged.state_id);
    assert_eq!(logged_top.tint, 0x112233);
    assert_eq!(logged_top.flags, QUAD_TINTED);
    assert_eq!(logged_top.fluid_kind, WATER);

    section.set(4, 5, 4, fluid(LAVA, 0), DAY);
    let quads = mesh_section(&section.view());
    assert_eq!(
        face(&quads, Direction::Up, source.state_id).fluid_kind,
        WATER
    );
}

#[test]
fn packed_halo_has_unique_slots_for_every_edge_and_corner() {
    let mut visited = [false; EDGE_CORNER_CELLS];
    for x in -1..=16 {
        for y in -1..=16 {
            for z in -1..=16 {
                let expected = [x, y, z]
                    .into_iter()
                    .filter(|&p| p == -1 || p == 16)
                    .count()
                    >= 2;
                let index = edge_corner_index([x, y, z]);
                assert_eq!(index.is_some(), expected);
                if let Some(i) = index {
                    assert!(!std::mem::replace(&mut visited[i], true));
                }
            }
        }
    }
    assert!(visited.into_iter().all(|v| v));
    assert_eq!(edge_corner_index([5, 16, -1]), Some(21));
    assert_eq!(edge_corner_index([16, -1, 16]), Some(197));
}

#[test]
fn cache_coalesces_edits_and_rebuilds_only_dirty_sections() {
    let mut cache = MeshCache::default();
    let main = SectionCoord(-1, 0, 0);
    let east = SectionCoord(0, 0, 0);
    let north = SectionCoord(-1, 0, -1);
    let diagonal = SectionCoord(0, 0, -1);
    let far = SectionCoord(99, 0, 0);
    for id in [main, east, north, diagonal, far] {
        assert!(cache.insert(id, air()));
    }
    assert_eq!(cache.rebuild_dirty(1), [north]);
    assert_eq!(cache.pending(), 4);
    assert_eq!(cache.rebuild_dirty(usize::MAX), [main, diagonal, east, far]);
    assert!(!cache.insert(main, air()));

    assert!(cache.set_cell(main, [4, 4, 4], STONE, DARK));
    assert!(cache.set_cell(main, [4, 4, 4], STONE, Light::new(2, 0)));
    assert!(!cache.set_cell(main, [4, 4, 4], STONE, Light::new(2, 0)));
    assert!(cache.mesh(main).is_none());
    assert_eq!(cache.rebuild_dirty(10), [main]);
    let baseline = cache.mesh(main).unwrap().to_vec();

    for _ in 0..3 {
        assert!(cache.set_cell(main, [15, 4, 8], STONE, DARK));
        assert!(cache.set_cell(main, [15, 4, 8], Block::AIR, DAY));
        assert_eq!(cache.pending(), 2);
        assert_eq!(cache.rebuild_dirty(10), [main, east]);
        assert_eq!(cache.mesh(main).unwrap(), baseline);
    }

    assert!(cache.set_cell(main, [15, 4, 0], Block::AIR, Light::new(5, 15)));
    assert_eq!(cache.rebuild_dirty(10), [north, main, diagonal, east]);
    assert_eq!(cache.mesh(far).unwrap(), &[]);
    assert!(cache.rebuild_dirty(10).is_empty());
}

#[test]
fn cache_loading_and_unloading_changes_border_culling() {
    let west = SectionCoord(0, 0, 0);
    let east = SectionCoord(1, 0, 0);
    let mut cache = MeshCache::default();
    cache.insert(west, Section::filled(STONE, DARK));
    cache.rebuild_dirty(2);
    assert_eq!(cache.mesh(west).unwrap().len(), 6);
    let mut next = air();
    next.set(8, 8, 8, STONE, DARK);
    cache.insert(east, next);
    assert_eq!(cache.rebuild_dirty(2), [east]);

    cache.insert(east, Section::filled(STONE, DARK));
    assert_eq!(cache.rebuild_dirty(2), [west, east]);
    assert!(!cache
        .mesh(west)
        .unwrap()
        .iter()
        .any(|q| q.direction == Direction::East));
    assert!(cache.remove(east));
    assert!(cache.mesh(east).is_none());
    assert_eq!(cache.rebuild_dirty(2), [west]);
    assert_eq!(cache.mesh(west).unwrap().len(), 6);
}

#[test]
fn cache_halo_matches_fluid_height_across_a_four_section_corner() {
    let a = SectionCoord(0, 0, 0);
    let b = SectionCoord(1, 0, 0);
    let c = SectionCoord(0, 0, 1);
    let d = SectionCoord(1, 0, 1);
    let above_d = SectionCoord(1, 1, 1);
    let mut cache = MeshCache::default();
    for (coord, x, z, level) in [(a, 15, 15, 0), (b, 0, 15, 3), (c, 15, 0, 7), (d, 0, 0, 5)] {
        let mut section = air();
        section.set(x, 15, z, fluid(WATER, level), DAY);
        cache.insert(coord, section);
    }
    cache.insert(above_d, air());
    cache.rebuild_dirty(10);
    let height = |cache: &MeshCache, coord, x: f32, z: f32| {
        let q = cache
            .mesh(coord)
            .unwrap()
            .iter()
            .find(|q| q.direction == Direction::Up)
            .unwrap();
        q.positions.iter().find(|p| p[0] == x && p[2] == z).unwrap()[1]
    };
    let h = height(&cache, a, 16.0, 16.0);
    assert!(h > 15.0 && h < 16.0);
    assert_eq!(h, height(&cache, b, 0.0, 16.0));
    assert_eq!(h, height(&cache, c, 16.0, 0.0));
    assert_eq!(h, height(&cache, d, 0.0, 0.0));
    cache.set_cell(above_d, [0, 0, 0], fluid(WATER, 0), DAY);
    assert_eq!(cache.rebuild_dirty(10), [a, c, b, d, above_d]);
    assert_eq!(height(&cache, a, 16.0, 16.0), 16.0);
    assert_eq!(height(&cache, b, 0.0, 16.0), 16.0);
    assert_eq!(height(&cache, c, 16.0, 0.0), 16.0);
    assert!(!cache
        .mesh(d)
        .unwrap()
        .iter()
        .any(|q| q.direction == Direction::Up));
}

#[test]
fn column_stitches_solid_sections_and_preserves_sky_from_known_air() {
    use crate::column::{mesh_column, ColumnSection};

    let mut lower = Section::filled(Block::AIR, Light::new(0, 15));
    lower.set(8, 15, 7, Block::opaque_cube(1), Light::new(0, 0));
    let mut upper = Section::filled(Block::AIR, Light::new(0, 15));
    upper.set(8, 0, 7, Block::opaque_cube(1), Light::new(0, 0));
    upper.set(8, 15, 7, Block::opaque_cube(1), Light::new(3, 0));
    let air = Section::filled(Block::AIR, Light::new(2, 9));

    let quads = mesh_column(
        &[
            ColumnSection {
                view: lower.view(),
                uniform_air: false,
                sealed: false,
            },
            ColumnSection {
                view: upper.view(),
                uniform_air: false,
                sealed: false,
            },
            ColumnSection {
                view: air.view(),
                uniform_air: true,
                sealed: false,
            },
        ],
        [None; 4],
    );
    assert_eq!(quads.len(), 16);
    assert!(!quads.iter().any(|q| {
        matches!(q.direction, Direction::Up | Direction::Down) && q.positions[0][1] == 16.0
    }));
    let top = quads
        .iter()
        .find(|q| q.direction == Direction::Up && q.positions[0][1] == 32.0)
        .unwrap();
    assert_eq!(top.face_light, Light::new(3, 9));
    assert_eq!(top.sky_light, [9; 4]);
}

#[test]
fn column_stitches_water_and_samples_cardinal_halo_across_y() {
    use crate::column::{mesh_column, ColumnSection};

    let water = fluid(WATER, 0);
    let mut lower = air();
    let mut upper = air();
    lower.set(15, 15, 8, water, DAY);
    upper.set(15, 0, 8, water, DAY);

    let mut east = [[Sample::new(Block::AIR, DAY); FACE_CELLS]; 2];
    east[1][Direction::East.border_index([0, 0, 7])] = Sample::new(STONE, DARK);
    let quads = mesh_column(
        &[
            ColumnSection {
                view: lower.view(),
                uniform_air: false,
                sealed: false,
            },
            ColumnSection {
                view: upper.view(),
                uniform_air: false,
                sealed: false,
            },
        ],
        [None, None, None, Some(&east)],
    );
    assert_eq!(quads.len(), 10);
    assert!(!quads.iter().any(|q| {
        matches!(q.direction, Direction::Up | Direction::Down) && q.positions[0][1] == 16.0
    }));
    let lower_side = face(&quads, Direction::East, water.state_id);
    // The lower side reaches the join where the upper water column begins.
    assert_eq!(lower_side.positions.map(|p| p[1]), [15.0, 16.0, 16.0, 15.0]);
    // The stone one section up in the East neighbor shades one top corner.
    assert_eq!(lower_side.ao, [3, 2, 3, 3]);
}

mod reference {
    use crate::column::{ColumnSection, FacePlane, CARDINALS};
    use crate::packed::PackedVertex;
    use crate::*;

    const CORNERS: [(i32, i32); 4] = [(0, 0), (1, 0), (1, 1), (0, 1)];

    #[derive(Clone, Copy, PartialEq, Eq)]
    struct RefShade {
        face: Light,
        ao: [u8; 4],
        corners: [Light; 4],
    }

    impl RefShade {
        fn equal_corners(self, a: usize, b: usize) -> bool {
            self.ao[a] == self.ao[b] && self.corners[a] == self.corners[b]
        }
        fn can_merge_u(self) -> bool {
            self.equal_corners(0, 1) && self.equal_corners(3, 2)
        }
        fn can_merge_v(self) -> bool {
            self.equal_corners(0, 3) && self.equal_corners(1, 2)
        }
        fn diagonal(self) -> u8 {
            u8::from(self.ao[0] + self.ao[2] > self.ao[1] + self.ao[3])
        }
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    struct RefFace {
        state_id: u32,
        tint: u32,
        layer: Layer,
        flags: u8,
        shade: RefShade,
    }

    fn ref_shade(
        view: &SectionView<'_>,
        p: [i32; 3],
        direction: Direction,
        light: Light,
    ) -> RefShade {
        let (u, v) = direction.tangents();
        let outside = direction.offset(p);
        let center = view.sample(outside).light;
        let face = Light::new(center.block().max(light.block()), center.sky());
        let mut result = RefShade {
            face,
            ao: [0; 4],
            corners: [face; 4],
        };
        for (index, (cu, cv)) in CORNERS.into_iter().enumerate() {
            let mut side_u = outside;
            let mut side_v = outside;
            let mut diagonal = outside;
            side_u[u] += cu * 2 - 1;
            side_v[v] += cv * 2 - 1;
            diagonal[u] = side_u[u];
            diagonal[v] = side_v[v];
            let a = view.sample(side_u);
            let b = view.sample(side_v);
            let d = view.sample(diagonal);
            let side = direction.opposite();
            let (ao, diagonal_light) = if a.block.occludes(side) && b.block.occludes(side) {
                (0, face)
            } else {
                let occupied = [a, b, d]
                    .into_iter()
                    .filter(|s| s.block.occludes(side))
                    .count();
                (3 - occupied as u8, d.light)
            };
            let [block_sum, sky_sum] = [face, a.light, b.light, diagonal_light]
                .into_iter()
                .fold([0, 0], |[bl, sk], s| [bl + s.block(), sk + s.sky()]);
            result.ao[index] = ao;
            result.corners[index] =
                Light::new(((block_sum + 2) / 4).max(light.block()), (sky_sum + 2) / 4);
        }
        result
    }

    fn ref_position(slice: usize, u: usize, v: usize, direction: Direction) -> [i32; 3] {
        let (u_axis, v_axis) = direction.tangents();
        let mut p = [0; 3];
        p[direction.axis()] = slice as i32;
        p[u_axis] = u as i32;
        p[v_axis] = v as i32;
        p
    }

    fn ref_rectangle(
        p: [i32; 3],
        direction: Direction,
        width: usize,
        height: usize,
    ) -> [[f32; 3]; 4] {
        let (u, v) = direction.tangents();
        CORNERS.map(|(cu, cv)| {
            let mut p = p;
            p[direction.axis()] += i32::from(direction.step() > 0);
            p[u] += cu * width as i32;
            p[v] += cv * height as i32;
            p.map(|n| n as f32)
        })
    }

    fn ref_fluid_level(block: Block) -> u8 {
        if block.flags & WATERLOGGED != 0 {
            0
        } else {
            block.fluid_level
        }
    }

    fn ref_fluid_corner(view: &SectionView<'_>, x: i32, y: i32, z: i32, kind: u8) -> f32 {
        let mut amount_sum = 0;
        let mut weight_sum = 0;
        for x in x - 1..=x {
            for z in z - 1..=z {
                let block = view.sample([x, y, z]).block;
                if block.fluid() == kind {
                    if view.sample([x, y + 1, z]).block.fluid() == kind {
                        return 1.0;
                    }
                    let level = ref_fluid_level(block);
                    let amount = if level < 8 { 8 - level } else { 8 };
                    let weight = if amount == 8 { 10 } else { 1 };
                    amount_sum += u32::from(amount) * weight;
                    weight_sum += weight;
                } else if block.flags & OPAQUE == 0 || block.occlusion & ALL_FACES != ALL_FACES {
                    weight_sum += 1;
                }
            }
        }
        amount_sum as f32 / (weight_sum * 9) as f32
    }

    pub(super) fn mesh_section_scalar(view: &SectionView<'_>) -> Vec<Quad> {
        let mut quads = Vec::new();
        append_section_scalar(view, &mut quads);
        quads
    }

    fn append_section_scalar(view: &SectionView<'_>, quads: &mut Vec<Quad>) {
        let mut mask = [None; FACE_CELLS];
        for direction in Direction::ALL {
            for slice in 0..EDGE {
                for (index, cell) in mask.iter_mut().enumerate() {
                    let p = ref_position(slice, index % EDGE, index / EDGE, direction);
                    let sample = view.sample(p);
                    let block = sample.block;
                    let layer = match block.flags {
                        flags if flags & OPAQUE != 0 => Some(Layer::Opaque),
                        flags if flags & CUTOUT != 0 => Some(Layer::Cutout),
                        flags if flags & TRANSLUCENT != 0 => Some(Layer::Translucent),
                        _ => None,
                    };
                    *cell = layer.and_then(|layer| {
                        let next = view.sample(direction.offset(p)).block;
                        if next.occludes(direction.opposite())
                            || (block.flags & next.flags & SAME_STATE_CULL != 0
                                && block.state_id == next.state_id)
                        {
                            return None;
                        }
                        let tinted = block.flags & TINTED != 0;
                        Some(RefFace {
                            state_id: block.state_id,
                            tint: if tinted { block.tint } else { 0 },
                            layer,
                            flags: if tinted { QUAD_TINTED } else { 0 },
                            shade: ref_shade(view, p, direction, sample.light),
                        })
                    });
                }
                for v in 0..EDGE {
                    for u in 0..EDGE {
                        let index = v * EDGE + u;
                        let Some(face) = mask[index] else { continue };
                        let merge_u = face.shade.can_merge_u();
                        let merge_v = face.shade.can_merge_v();
                        let equal = |cell: &Option<RefFace>| *cell == Some(face);
                        let width = if merge_u {
                            mask[index..(v + 1) * EDGE]
                                .iter()
                                .take_while(|cell| equal(cell))
                                .count()
                        } else {
                            1
                        };
                        let mut height = 1;
                        while merge_v && v + height < EDGE {
                            let start = index + height * EDGE;
                            if !mask[start..start + width].iter().all(equal) {
                                break;
                            }
                            height += 1;
                        }
                        for row in v..v + height {
                            let start = row * EDGE + u;
                            mask[start..start + width].fill(None);
                        }
                        let origin = ref_position(slice, u, v, direction);
                        quads.push(Quad {
                            positions: ref_rectangle(origin, direction, width, height),
                            direction,
                            state_id: face.state_id,
                            tint: face.tint,
                            flow: [0.0; 2],
                            ao: face.shade.ao,
                            block_light: face.shade.corners.map(Light::block),
                            sky_light: face.shade.corners.map(Light::sky),
                            face_light: face.shade.face,
                            layer: face.layer,
                            diagonal: face.shade.diagonal(),
                            fluid_kind: NO_FLUID,
                            flags: face.flags,
                            reserved: 0,
                        });
                    }
                }
            }
        }
        for (index, block) in view.blocks.iter().enumerate() {
            let kind = block.fluid();
            if kind == NO_FLUID {
                continue;
            }
            let p = [
                (index % EDGE) as i32,
                (index / FACE_CELLS) as i32,
                (index / EDGE % EDGE) as i32,
            ];
            let sample = view.sample(p);
            let mut visible = Direction::ALL.map(|d| {
                let next = view.sample(d.offset(p)).block;
                next.fluid() != kind
                    && !block.occludes(d)
                    && (d == Direction::Up || !next.occludes(d.opposite()))
            });
            if !visible.contains(&true) {
                continue;
            }
            let [x, y, z] = p;
            let h = [
                [
                    ref_fluid_corner(view, x, y, z, kind),
                    ref_fluid_corner(view, x + 1, y, z, kind),
                ],
                [
                    ref_fluid_corner(view, x, y, z + 1, kind),
                    ref_fluid_corner(view, x + 1, y, z + 1, kind),
                ],
            ];
            if h.into_iter().flatten().all(|height| height == 1.0)
                && view
                    .sample(Direction::Up.offset(p))
                    .block
                    .occludes(Direction::Down)
            {
                visible[Direction::Up as usize] = false;
            }
            let flow = [
                ((h[0][0] - h[0][1]) + (h[1][0] - h[1][1])) * 0.5,
                ((h[0][0] - h[1][0]) + (h[0][1] - h[1][1])) * 0.5,
            ];
            for direction in Direction::ALL {
                if !visible[direction as usize] {
                    continue;
                }
                let mut positions = ref_rectangle(p, direction, 1, 1);
                for vertex in &mut positions {
                    if vertex[1] > y as f32 {
                        vertex[1] = y as f32
                            + h[(vertex[2] as i32 - z) as usize][(vertex[0] as i32 - x) as usize];
                    }
                }
                let tinted = block.flags & FLUID_TINTED != 0;
                let s = ref_shade(view, p, direction, sample.light);
                quads.push(Quad {
                    positions,
                    direction,
                    state_id: block.state_id,
                    tint: if tinted { block.fluid_tint } else { 0 },
                    flow,
                    ao: s.ao,
                    block_light: s.corners.map(Light::block),
                    sky_light: s.corners.map(Light::sky),
                    face_light: s.face,
                    layer: Layer::Fluid,
                    diagonal: s.diagonal(),
                    fluid_kind: kind,
                    flags: (if tinted { QUAD_TINTED } else { 0 })
                        | (if ref_fluid_level(*block) >= 8 {
                            QUAD_FALLING
                        } else {
                            0
                        }),
                    reserved: 0,
                });
            }
        }
    }

    pub(super) fn mesh_column_scalar(
        sections: &[ColumnSection<'_>],
        neighbors: [Option<&[FacePlane]>; 4],
    ) -> Vec<Quad> {
        let mut quads = Vec::new();
        for (index, section) in sections.iter().enumerate() {
            if section.uniform_air {
                continue;
            }
            let mut view = section.view;
            for (side, neighbor) in CARDINALS.into_iter().zip(neighbors) {
                view.borders[side as usize] = neighbor.map(|faces| &faces[index]);
            }
            let fallback = view.missing.sample();
            let mut halo = [fallback; EDGE_CORNER_CELLS];
            let mut vertical = [[fallback; FACE_CELLS]; 2];
            for side in [Direction::Down, Direction::Up] {
                let Some(other) = index
                    .checked_add_signed(side.step() as isize)
                    .filter(|&i| i < sections.len())
                else {
                    continue;
                };
                let y = if side == Direction::Down { EDGE - 1 } else { 0 };
                let plane = &mut vertical[side as usize];
                for (i, sample) in plane.iter_mut().enumerate() {
                    *sample = sections[other].view.sample_local(i % EDGE, y, i / EDGE);
                }
                for (horizontal, neighbor) in CARDINALS.into_iter().zip(neighbors) {
                    let Some(faces) = neighbor else { continue };
                    let axis = horizontal.axis();
                    let free = 2 - axis;
                    for coordinate in 0..EDGE {
                        let mut p = [0; 3];
                        p[1] = y;
                        p[free] = coordinate;
                        let sample = faces[other][horizontal.border_index(p)];
                        let mut outside = p.map(|coordinate| coordinate as i32);
                        outside[1] = if side == Direction::Down {
                            -1
                        } else {
                            EDGE as i32
                        };
                        outside[axis] = if horizontal.step() < 0 {
                            -1
                        } else {
                            EDGE as i32
                        };
                        halo[edge_corner_index(outside).expect("vertical cardinal edge")] = sample;
                    }
                }
            }
            view.borders[Direction::Down as usize] = Some(&vertical[0]);
            view.borders[Direction::Up as usize] = Some(&vertical[1]);
            view.edges_and_corners = Some(&halo);
            let start = quads.len();
            append_section_scalar(&view, &mut quads);
            let y_offset = (index * EDGE) as f32;
            for quad in &mut quads[start..] {
                for position in &mut quad.positions {
                    position[1] += y_offset;
                }
            }
        }
        quads
    }

    pub(super) fn pack_quad_scalar(quad: &Quad) -> [PackedVertex; 4] {
        let [width, height] = if quad.layer == Layer::Fluid {
            [1u32; 2]
        } else {
            let (u, v) = quad.direction.tangents();
            [
                (quad.positions[1][u] - quad.positions[0][u]) as u32,
                (quad.positions[3][v] - quad.positions[0][v]) as u32,
            ]
        };
        let attributes = quad.direction as u32
            | ((quad.layer as u32) << 13)
            | (u32::from(quad.fluid_kind) << 15)
            | (u32::from(quad.diagonal) << 17)
            | (u32::from(quad.flags) << 18)
            | (width << 20)
            | (height << 25);
        std::array::from_fn(|corner| {
            let [x, y, z] = quad.positions[corner];
            PackedVertex {
                position: x as u32 | ((z as u32) << 5) | (((y * 1024.0).round() as u32) << 10),
                state_id: quad.state_id,
                attributes: attributes
                    | (u32::from(quad.ao[corner]) << 3)
                    | (u32::from(quad.block_light[corner]) << 5)
                    | (u32::from(quad.sky_light[corner]) << 9)
                    | ((corner as u32) << 30),
                tint: quad.tint,
            }
        })
    }

    pub(super) fn sort_translucent_scalar(quads: &[Quad], camera: [f32; 3]) -> Vec<u32> {
        let mut order: Vec<(f64, u32)> = quads
            .iter()
            .enumerate()
            .filter(|(_, q)| {
                q.layer == Layer::Translucent || (q.layer == Layer::Fluid && q.fluid_kind == WATER)
            })
            .map(|(i, q)| {
                let mut key = 0.0;
                for (axis, cam) in camera.into_iter().enumerate() {
                    let center = q.positions.iter().map(|p| f64::from(p[axis])).sum::<f64>() * 0.25;
                    key += center * (center - 2.0 * f64::from(cam));
                }
                (key, i as u32)
            })
            .collect();
        order.sort_unstable_by(|(l, i), (r, j)| r.partial_cmp(l).unwrap().then(i.cmp(j)));
        order.into_iter().map(|(_, i)| i).collect()
    }

    pub(super) fn hash_quads(quads: &[Quad]) -> u64 {
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        let mut feed = |bytes: &[u8]| {
            for &b in bytes {
                h ^= u64::from(b);
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
        };
        feed(&(quads.len() as u64).to_le_bytes());
        for q in quads {
            for p in q.positions {
                for c in p {
                    feed(&c.to_bits().to_le_bytes());
                }
            }
            feed(&q.state_id.to_le_bytes());
            feed(&q.tint.to_le_bytes());
            for f in q.flow {
                feed(&f.to_bits().to_le_bytes());
            }
            feed(&q.ao);
            feed(&q.block_light);
            feed(&q.sky_light);
            feed(&[
                q.face_light.0,
                q.direction as u8,
                q.layer as u8,
                q.diagonal,
                q.fluid_kind,
                q.flags,
                q.reserved,
            ]);
        }
        h
    }

    pub(super) fn hash_vertices(verts: &[PackedVertex]) -> u64 {
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        let mut feed = |bytes: &[u8]| {
            for &b in bytes {
                h ^= u64::from(b);
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
        };
        feed(&(verts.len() as u64).to_le_bytes());
        for v in verts {
            feed(&v.position.to_le_bytes());
            feed(&v.state_id.to_le_bytes());
            feed(&v.attributes.to_le_bytes());
            feed(&v.tint.to_le_bytes());
        }
        h
    }

    pub(super) fn assert_quads_bit_identical(actual: &[Quad], expected: &[Quad], label: &str) {
        assert_eq!(actual.len(), expected.len(), "{label}: quad count mismatch");
        for (i, (a, e)) in actual.iter().zip(expected).enumerate() {
            assert_eq!(
                a.positions.map(|p| p.map(f32::to_bits)),
                e.positions.map(|p| p.map(f32::to_bits)),
                "{label}: positions mismatch at quad {i}"
            );
            assert_eq!(
                a.flow.map(f32::to_bits),
                e.flow.map(f32::to_bits),
                "{label}: flow mismatch at quad {i}"
            );
            assert_eq!(
                (
                    a.state_id,
                    a.tint,
                    a.ao,
                    a.block_light,
                    a.sky_light,
                    a.face_light,
                    a.direction,
                    a.layer,
                    a.diagonal,
                    a.fluid_kind,
                    a.flags,
                    a.reserved,
                ),
                (
                    e.state_id,
                    e.tint,
                    e.ao,
                    e.block_light,
                    e.sky_light,
                    e.face_light,
                    e.direction,
                    e.layer,
                    e.diagonal,
                    e.fluid_kind,
                    e.flags,
                    e.reserved,
                ),
                "{label}: fields mismatch at quad {i}"
            );
        }
    }
}

struct XorShift64(u64);
impl XorShift64 {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn random_block(&mut self) -> Block {
        let r = self.next();
        match r % 16 {
            0..=4 => Block::AIR,
            5 => Block::opaque_cube(1 + (r as u32 >> 8) % 3),
            6 => Block {
                state_id: 10 + (r as u32 >> 8) % 2,
                tint: 0x00ff_ffff & (r as u32 >> 16),
                flags: OPAQUE | TINTED,
                occlusion: ALL_FACES,
                ..Block::AIR
            },
            7 => Block {
                state_id: 20,
                flags: OPAQUE,
                occlusion: ((r >> 8) as u8) & ALL_FACES,
                ..Block::AIR
            },
            8 => Block {
                state_id: 238,
                tint: 0x228833,
                flags: CUTOUT | if r & 0x100 != 0 { TINTED } else { 0 },
                ..Block::AIR
            },
            9 => Block {
                state_id: 519 + (r as u32 >> 8) % 2,
                flags: TRANSLUCENT | SAME_STATE_CULL,
                ..Block::AIR
            },
            10 => Block {
                state_id: 530,
                flags: TRANSLUCENT,
                ..Block::AIR
            },
            11 => Block {
                state_id: 80 + ((r >> 8) as u32 % 16),
                fluid_kind: WATER,
                fluid_level: (r >> 8) as u8 % 16,
                fluid_tint: 0x3366cc,
                flags: if r & 0x1000 != 0 { FLUID_TINTED } else { 0 },
                ..Block::AIR
            },
            12 => Block {
                state_id: 96 + ((r >> 8) as u32 % 16),
                fluid_kind: LAVA,
                fluid_level: (r >> 8) as u8 % 16,
                ..Block::AIR
            },
            13 => Block {
                state_id: 237,
                tint: 0x445566,
                fluid_tint: 0x112233,
                flags: CUTOUT | WATERLOGGED | TINTED | FLUID_TINTED,
                ..Block::AIR
            },
            14 => Block {
                state_id: 250,
                flags: OPAQUE | CUTOUT | WATERLOGGED,
                occlusion: ((r >> 8) as u8) & ALL_FACES,
                ..Block::AIR
            },
            _ => Block {
                state_id: 260,
                flags: CUTOUT | TRANSLUCENT | SAME_STATE_CULL,
                ..Block::AIR
            },
        }
    }
    fn random_light(&mut self) -> Light {
        let r = self.next();
        Light::new((r & 15) as u8, ((r >> 4) & 15) as u8)
    }
    fn random_section(&mut self) -> Section {
        let mut s = air();
        for y in 0..EDGE {
            for z in 0..EDGE {
                for x in 0..EDGE {
                    let b = self.random_block();
                    let l = self.random_light();
                    s.set(x, y, z, b, l);
                }
            }
        }
        s
    }
    fn random_border(&mut self) -> [Sample; FACE_CELLS] {
        std::array::from_fn(|_| Sample::new(self.random_block(), self.random_light()))
    }
    fn random_halo(&mut self) -> [Sample; EDGE_CORNER_CELLS] {
        std::array::from_fn(|_| Sample::new(self.random_block(), self.random_light()))
    }
}

#[test]
fn parity_against_scalar_reference_on_bench_and_complex_fixtures() {
    use crate::column::{mesh_column, ColumnSection};
    use crate::ffi::{
        megu_client_mesh_column, megu_client_mesh_pack_vertices,
        megu_client_mesh_sort_translucent, MeshColumnInput, MeshColumnSection, PackedVertex,
        ABI_AIR, ABI_COLUMN_UNIFORM_AIR, ABI_OK, ABI_VERSION,
    };
    use std::mem::MaybeUninit;
    use std::ptr;

    // 1. Solid & checkerboard fixtures matching mesh_bench
    let solid = Section::filled(STONE, DARK);
    let mut checkerboard = Section::filled(Block::AIR, DAY);
    for y in 0..EDGE {
        for z in 0..EDGE {
            for x in 0..EDGE {
                if (x + y + z) % 2 == 0 {
                    checkerboard.set(x, y, z, STONE, DARK);
                }
            }
        }
    }
    for (name, section, expected_hash) in [
        ("solid", &solid, 0x2b5a_c837_b128_f678u64),
        ("checkerboard", &checkerboard, 0x52f6_98e8_7339_3203u64),
    ] {
        let view = section.view();
        let actual = mesh_section(&view);
        let expected = reference::mesh_section_scalar(&view);
        reference::assert_quads_bit_identical(&actual, &expected, name);
        assert_eq!(
            reference::hash_quads(&actual),
            expected_hash,
            "{name} baseline hash"
        );
    }

    // 2. Overworld 24-section column fixture matching mesh_bench
    let water = Block {
        state_id: 80,
        fluid_kind: WATER,
        ..Block::AIR
    };
    let ow_sections: Vec<_> = (0..9)
        .map(|index| {
            let mut section = Section::filled(Block::AIR, DAY);
            for y in 0..EDGE {
                let world_y = -64 + (index * EDGE + y) as i32;
                for z in 0..EDGE {
                    for x in 0..EDGE {
                        let surface = 56 + ((x * 13 + z * 7 + x * z) % 14) as i32;
                        if world_y <= surface {
                            section.set(x, y, z, STONE, DARK);
                        } else if world_y <= 62 {
                            section.set(x, y, z, water, Light::new(0, 12));
                        }
                    }
                }
            }
            section
        })
        .collect();
    let dark_bytes = [0u8; LIGHT_BYTES];
    let day_bytes = [255u8; LIGHT_BYTES];
    let ow_col_inputs: [_; 24] = std::array::from_fn(|i| match ow_sections.get(i) {
        Some(s) => MeshColumnSection {
            flags: 0,
            blocks: s.blocks.as_ptr(),
            block_light: s.block_light.as_ptr(),
            sky_light: s.sky_light.as_ptr(),
        },
        None => MeshColumnSection {
            flags: ABI_COLUMN_UNIFORM_AIR,
            blocks: ptr::null(),
            block_light: dark_bytes.as_ptr(),
            sky_light: day_bytes.as_ptr(),
        },
    });
    let air_section = Section::filled(Block::AIR, DAY);
    let ow_rust_sections: Vec<ColumnSection<'_>> = (0..24)
        .map(|i| match ow_sections.get(i) {
            Some(s) => ColumnSection {
                view: s.view(),
                uniform_air: false,
                sealed: false,
            },
            None => ColumnSection {
                view: air_section.view(),
                uniform_air: true,
                sealed: false,
            },
        })
        .collect();
    let ow_expected = reference::mesh_column_scalar(&ow_rust_sections, [None; 4]);
    let ow_input = MeshColumnInput {
        abi_version: ABI_VERSION,
        missing: ABI_AIR,
        sections: ow_col_inputs.as_ptr(),
        section_count: ow_col_inputs.len(),
        borders: [ptr::null(); 4],
    };
    let mut quad_count = 0;
    assert_eq!(
        unsafe { megu_client_mesh_column(&ow_input, ptr::null_mut(), 0, &mut quad_count) },
        ABI_OK
    );
    let mut ow_buf = vec![MaybeUninit::<Quad>::uninit(); quad_count];
    assert_eq!(
        unsafe {
            megu_client_mesh_column(
                &ow_input,
                ow_buf.as_mut_ptr().cast(),
                ow_buf.len(),
                &mut quad_count,
            )
        },
        ABI_OK
    );
    let ow_actual: Vec<Quad> = ow_buf
        .into_iter()
        .take(quad_count)
        .map(|q| unsafe { q.assume_init() })
        .collect();
    reference::assert_quads_bit_identical(&ow_actual, &ow_expected, "overworld_column");

    let mut ow_verts = vec![PackedVertex::default(); ow_actual.len() * 4];
    let mut vert_count = 0;
    assert_eq!(
        unsafe {
            megu_client_mesh_pack_vertices(
                ow_actual.as_ptr(),
                ow_actual.len(),
                ow_verts.as_mut_ptr(),
                ow_verts.len(),
                &mut vert_count,
            )
        },
        ABI_OK
    );
    assert_eq!(vert_count, ow_actual.len() * 4);
    let ow_verts_expected: Vec<PackedVertex> = ow_expected
        .iter()
        .flat_map(reference::pack_quad_scalar)
        .collect();
    assert_eq!(ow_verts, ow_verts_expected, "overworld packed vertices");
    assert_eq!(reference::hash_quads(&ow_actual), 0x1c69_0400_f9a6_e7ee);
    assert_eq!(reference::hash_vertices(&ow_verts), 0x686f_a1b3_fe5e_67fe);

    // 3. Complex mixed random sections with borders, halo, and both MissingNeighbor modes
    let mut rng = XorShift64(0x9e37_79b9_7f4a_7c15);
    let complex_section = rng.random_section();
    let borders: [[Sample; FACE_CELLS]; 6] = std::array::from_fn(|_| rng.random_border());
    let halo = rng.random_halo();

    let expected_section_hashes = [
        (0x0801_7337_53a3_07aeu64, 0xe2a8_948a_ded9_a96au64),
        (0x0801_7337_53a3_07aeu64, 0xe2a8_948a_ded9_a96au64),
        (0x42b9_6aa9_34d7_7aa5u64, 0x7c6a_a2d5_5375_e5f7u64),
        (0x70d4_1c30_7ce3_ba47u64, 0xafdd_fe0f_b287_3070u64),
        (0xaeb4_57db_7235_0d3cu64, 0xae7d_d515_3764_b494u64),
    ];

    for (cfg_idx, (missing, use_borders, use_halo)) in [
        (MissingNeighbor::Air, true, true),
        (MissingNeighbor::Opaque, true, true),
        (MissingNeighbor::Air, false, false),
        (MissingNeighbor::Opaque, false, true),
        (MissingNeighbor::Air, true, false),
    ]
    .into_iter()
    .enumerate()
    {
        let mut view = complex_section.view();
        view.missing = missing;
        if use_borders {
            view.borders = borders.each_ref().map(Some);
        } else {
            view.borders[Direction::Up as usize] = Some(&borders[Direction::Up as usize]);
            view.borders[Direction::West as usize] = Some(&borders[Direction::West as usize]);
        }
        if use_halo {
            view.edges_and_corners = Some(&halo);
        }
        let actual = mesh_section(&view);
        let expected = reference::mesh_section_scalar(&view);
        let label = format!("complex_section_{cfg_idx}");
        reference::assert_quads_bit_identical(&actual, &expected, &label);
        let mut verts = vec![PackedVertex::default(); actual.len() * 4];
        let mut vcount = 0;
        assert_eq!(
            unsafe {
                megu_client_mesh_pack_vertices(
                    actual.as_ptr(),
                    actual.len(),
                    verts.as_mut_ptr(),
                    verts.len(),
                    &mut vcount,
                )
            },
            ABI_OK
        );
        let verts_expected: Vec<PackedVertex> = expected
            .iter()
            .flat_map(reference::pack_quad_scalar)
            .collect();
        assert_eq!(verts, verts_expected, "{label}: packed vertices");

        for cam in [[8.5f32, 7.25, 8.5], [-100.0, 200.0, -50.0], [0.0, 0.0, 0.0]] {
            let expected_sort = reference::sort_translucent_scalar(&expected, cam);
            let mut sort_out = vec![0u32; expected_sort.len()];
            let mut scount = 0;
            assert_eq!(
                unsafe {
                    megu_client_mesh_sort_translucent(
                        actual.as_ptr(),
                        actual.len(),
                        cam[0],
                        cam[1],
                        cam[2],
                        sort_out.as_mut_ptr(),
                        sort_out.len(),
                        &mut scount,
                    )
                },
                ABI_OK
            );
            assert_eq!(scount, expected_sort.len());
            assert_eq!(sort_out, expected_sort, "{label}: sort_translucent {cam:?}");
        }
        let (eqh, evh) = expected_section_hashes[cfg_idx];
        assert_eq!(reference::hash_quads(&actual), eqh, "{label} quad hash");
        assert_eq!(reference::hash_vertices(&verts), evh, "{label} vertex hash");
    }

    // 4. Complex 4-section mixed column with cardinal neighbors, uniform air with non-zero light, both MissingNeighbor modes
    let sec0 = rng.random_section();
    let sec1 = rng.random_section();
    let sec2 = Section::filled(Block::AIR, Light::new(7, 11));
    let sec3 = rng.random_section();
    let north_planes: [[Sample; FACE_CELLS]; 4] = std::array::from_fn(|_| rng.random_border());
    let south_planes: [[Sample; FACE_CELLS]; 4] = std::array::from_fn(|_| rng.random_border());
    let west_planes: [[Sample; FACE_CELLS]; 4] = std::array::from_fn(|_| rng.random_border());
    let east_planes: [[Sample; FACE_CELLS]; 4] = std::array::from_fn(|_| rng.random_border());
    let expected_col_hashes = [0x01e0_737e_e07c_8f41u64, 0xc36a_9baf_fb36_7002u64];
    for (m_idx, missing) in [MissingNeighbor::Air, MissingNeighbor::Opaque]
        .into_iter()
        .enumerate()
    {
        fn make_view(s: &Section, missing: MissingNeighbor) -> SectionView<'_> {
            let mut v = s.view();
            v.missing = missing;
            v
        }
        let col = [
            ColumnSection {
                view: make_view(&sec0, missing),
                uniform_air: false,
                sealed: false,
            },
            ColumnSection {
                view: make_view(&sec1, missing),
                uniform_air: false,
                sealed: false,
            },
            ColumnSection {
                view: make_view(&sec2, missing),
                uniform_air: true,
                sealed: false,
            },
            ColumnSection {
                view: make_view(&sec3, missing),
                uniform_air: false,
                sealed: false,
            },
        ];
        let neighbors = [
            Some(&north_planes[..]),
            Some(&south_planes[..]),
            Some(&west_planes[..]),
            Some(&east_planes[..]),
        ];
        let actual = mesh_column(&col, neighbors);
        let expected = reference::mesh_column_scalar(&col, neighbors);
        let label = format!("complex_column_{m_idx}");
        reference::assert_quads_bit_identical(&actual, &expected, &label);
        assert_eq!(
            reference::hash_quads(&actual),
            expected_col_hashes[m_idx],
            "{label} baseline hash"
        );
    }
}

#[test]
fn parity_on_uniform_slice_and_column_fast_path_edge_cases() {
    use crate::column::{mesh_column, ColumnSection};
    use crate::mesh::transpose_16x16;

    // Exhaustive single-bit + random checks for 16x16 bit-matrix transpose.
    for r in 0..16 {
        for c in 0..16 {
            let mut m = [0u16; 16];
            m[r] = 1u16 << c;
            let t = transpose_16x16(m);
            for (rr, &row) in t.iter().enumerate() {
                let expected = if rr == c { 1u16 << r } else { 0 };
                assert_eq!(row, expected, "transpose single bit ({r}, {c})");
            }
        }
    }
    let mut rng = XorShift64(0x1234_5678_9abc_def0);
    for _ in 0..64 {
        let m: [u16; 16] = std::array::from_fn(|_| rng.next() as u16);
        let t = transpose_16x16(m);
        for (r, &row) in m.iter().enumerate() {
            for (c, &col) in t.iter().enumerate() {
                assert_eq!((row >> c) & 1, (col >> r) & 1);
            }
        }
    }

    // Emissive full solid section (src_bl > out_bl) + single cell light perturbation + single halo corner perturbation.
    let mut emissive = Section::filled(STONE, Light::new(12, 0));
    reference::assert_quads_bit_identical(
        &mesh_section(&emissive.view()),
        &reference::mesh_section_scalar(&emissive.view()),
        "emissive_solid",
    );
    emissive.set(7, 15, 9, STONE, Light::new(3, 0));
    reference::assert_quads_bit_identical(
        &mesh_section(&emissive.view()),
        &reference::mesh_section_scalar(&emissive.view()),
        "emissive_perturbed_cell",
    );

    let solid = Section::filled(STONE, DARK);
    let mut halo = [Sample::new(Block::AIR, DAY); EDGE_CORNER_CELLS];
    halo[edge_corner_index([16, 16, 16]).unwrap()] = Sample::new(STONE, DARK);
    let mut view = solid.view();
    view.edges_and_corners = Some(&halo);
    reference::assert_quads_bit_identical(
        &mesh_section(&view),
        &reference::mesh_section_scalar(&view),
        "solid_perturbed_halo_corner",
    );

    // Full slice with SAME_STATE_CULL matching a border plane.
    let glass = Block {
        state_id: 519,
        flags: TRANSLUCENT | SAME_STATE_CULL,
        ..Block::AIR
    };
    let glass_sec = Section::filled(glass, DAY);
    let glass_border = [Sample::new(glass, DAY); FACE_CELLS];
    let mut gview = glass_sec.view();
    gview.borders[Direction::Up as usize] = Some(&glass_border);
    reference::assert_quads_bit_identical(
        &mesh_section(&gview),
        &reference::mesh_section_scalar(&gview),
        "full_glass_same_state_cull_border",
    );

    // Column with no cardinal neighbors switching missing policy and introducing fluids in a later section.
    let mut water_sec = air();
    water_sec.set(0, 0, 0, fluid(WATER, 0), DAY);
    water_sec.set(15, 15, 15, fluid(LAVA, 4), Light::new(15, 0));
    let mut v0 = solid.view();
    v0.missing = MissingNeighbor::Air;
    let mut v1 = water_sec.view();
    v1.missing = MissingNeighbor::Opaque;
    let mut v2 = solid.view();
    v2.missing = MissingNeighbor::Opaque;
    let col = [
        ColumnSection {
            view: v0,
            uniform_air: false,
            sealed: false,
        },
        ColumnSection {
            view: v1,
            uniform_air: false,
            sealed: false,
        },
        ColumnSection {
            view: v2,
            uniform_air: false,
            sealed: false,
        },
    ];
    reference::assert_quads_bit_identical(
        &mesh_column(&col, [None; 4]),
        &reference::mesh_column_scalar(&col, [None; 4]),
        "column_mixed_missing_and_fluid_reuse",
    );
}

#[test]
fn packed_y_rounding_matches_f32_round_on_halfway_boundaries() {
    let check_y = |y: f32| {
        let fast = (f64::from(y * 1024.0) + 0.5) as u32;
        let orig = (y * 1024.0).round() as u32;
        assert_eq!(fast, orig, "y={y} (bits={:#010x})", y.to_bits());
    };
    check_y(0.0);
    check_y(-0.0);
    check_y(512.0);
    check_y(f32::from_bits(512.0f32.to_bits() - 1));
    for k in 0..=512u32 {
        let half = (k as f32 + 0.5) / 1024.0;
        let bits = half.to_bits();
        for delta in -2i32..=2 {
            let y = f32::from_bits(bits.wrapping_add_signed(delta));
            if (0.0..=512.0).contains(&y) {
                check_y(y);
            }
        }
    }
    for weight in 1..=40u32 {
        for amount in 0..=weight * 8 {
            let h = amount as f32 / (weight * 9) as f32;
            for base_y in [0.0f32, 1.0, 15.0, 62.0, 112.0, 511.0] {
                check_y(base_y + h);
            }
        }
    }
}

#[test]
fn lod_summary_finds_top_block_tint_and_light_per_column() {
    let mut lower = Section::filled(Block::AIR, Light::new(0, 15));
    lower.set(2, 3, 1, STONE, Light::new(4, 9));
    lower.set(0, 15, 0, STONE, DARK);
    let mut upper = air();
    let leaves = Block {
        state_id: 238,
        tint: 0x0022_8833,
        flags: CUTOUT | TINTED,
        ..Block::AIR
    };
    upper.set(2, 3, 1, leaves, Light::new(1, 14));
    let mut out = [LodCell::EMPTY; LOD_FINE * LOD_FINE];
    let views = [lower.view(), upper.view()];
    summarize_column(&views, LOD_FINE, &mut out);

    let cell = out[EDGE + 2];
    assert_eq!(cell.top_y, 16 + 3);
    assert_eq!(cell.state_id, leaves.state_id);
    assert_eq!(cell.tint, leaves.tint);
    assert_eq!(cell.light, Light::new(1, 14).0);

    let low = out[0];
    assert_eq!(low.top_y, 15);
    assert_eq!(low.state_id, STONE.state_id);
    assert_eq!(low.tint, 0);
    assert_eq!(low.light, DARK.0);

    for (i, cell) in out.iter().enumerate() {
        if i != EDGE + 2 && i != 0 {
            assert!(cell.is_empty(), "column {i} should be empty");
        }
    }

    // An untinted fluid counts as the visible top too.
    let mut water_col = air();
    water_col.set(5, 9, 7, fluid(WATER, 0), Light::new(0, 12));
    summarize_column(&[water_col.view()], LOD_FINE, &mut out);
    let cell = out[7 * EDGE + 5];
    assert_eq!(cell.top_y, 9);
    assert_eq!(cell.state_id, 80);
}

#[test]
fn lod_summary_coarse_takes_footprint_max_and_reuses_buffer() {
    let mut section = air();
    // Same 4x4 footprint, different column heights: the tallest wins.
    section.set(1, 2, 1, STONE, DARK);
    let tall = Block::opaque_cube(42);
    section.set(3, 9, 2, tall, DARK);
    section.set(0, 5, 0, STONE, DARK);
    let mut summary = LodSummary::new(LOD_COARSE);
    let cells = summary.update(&[section.view()]).to_vec();
    assert_eq!(summary.edge(), LOD_COARSE);
    assert_eq!(summary.height(), EDGE);
    let best = cells[0];
    assert_eq!(best.top_y, 9);
    assert_eq!(best.state_id, tall.state_id);

    // Reusing the buffer does not reallocate and clears stale tops.
    let ptr = summary.cells().as_ptr();
    let empty = air();
    let cells = summary.update(&[empty.view()]);
    assert_eq!(cells.as_ptr(), ptr);
    assert!(cells.iter().all(|cell| cell.is_empty()));

    // A taller column reports a larger height and saturates only above
    // i16::MAX.
    let views: Vec<_> = (0..24).map(|_| section.view()).collect();
    summary.update(&views);
    assert_eq!(summary.height(), 24 * EDGE);
    assert_eq!(summary.cells()[0].top_y, (23 * EDGE + 9) as i16);
}

#[test]
fn lod_summary_empty_column_and_boundary_cells() {
    let mut out = [LodCell::EMPTY; LOD_FINE * LOD_FINE];
    summarize_column(&[], LOD_FINE, &mut out);
    assert!(out.iter().all(|cell| cell.is_empty()));
    let empty = air();
    summarize_column(&[empty.view()], LOD_COARSE, &mut out);
    assert!(out[..LOD_COARSE * LOD_COARSE]
        .iter()
        .all(|cell| cell.is_empty()));

    // The highest possible cell is reachable.
    let mut section = air();
    section.set(15, 15, 15, STONE, Light::new(15, 0));
    summarize_column(&[section.view()], LOD_FINE, &mut out);
    let cell = out[15 * EDGE + 15];
    assert_eq!(cell.top_y, 15);
    assert_eq!(cell.light, Light::new(15, 0).0);
}

#[test]
fn cull_scan_marks_uniform_air_boundary_faces_and_sealed_sections() {
    let empty = air();
    let vis = SectionVis::scan(&empty.view());
    assert!(vis.uniform_air);
    assert_eq!(vis.boundary_faces, 0);
    assert!(!vis.interior_faces);
    assert!(!vis.emits_any());

    // A uniform cube against air emits on all six outer faces but has no
    // interior geometry.
    let solid = Section::filled(STONE, DARK);
    let vis = SectionVis::scan(&solid.view());
    assert!(!vis.uniform_air);
    assert_eq!(vis.boundary_faces, ALL_FACES);
    assert!(!vis.interior_faces);
    assert!(vis.emits_any());

    // Fully sealed by opaque neighbors: the mesher proves empty too.
    let mut sealed = solid.view();
    sealed.missing = MissingNeighbor::Opaque;
    let vis = SectionVis::scan(&sealed);
    assert_eq!(vis.boundary_faces, 0);
    assert!(!vis.interior_faces);
    assert!(!vis.emits_any());
    assert!(mesh_section(&sealed).is_empty());

    // One open side alone flips the verdict. The meshed quads all face East;
    // the opaque-missing halo keeps them from merging into one.
    let open_border = [Sample::new(Block::AIR, DAY); FACE_CELLS];
    sealed.borders[Direction::East as usize] = Some(&open_border);
    let vis = SectionVis::scan(&sealed);
    assert_eq!(vis.boundary_faces, Direction::East.bit());
    assert!(vis.emits_any());
    let quads = mesh_section(&sealed);
    assert!(!quads.is_empty());
    assert!(quads.iter().all(|q| q.direction == Direction::East));
}

#[test]
fn cull_scan_counts_interior_and_fluid_faces() {
    // A lone cube at the centre emits only interior faces.
    let mut cave = air();
    cave.set(8, 8, 8, STONE, DARK);
    let vis = SectionVis::scan(&cave.view());
    assert_eq!(vis.boundary_faces, 0);
    assert!(vis.interior_faces);
    assert!(vis.visible_towards(0));

    // Water on the top slice emits an Up boundary face; its side and bottom
    // faces count as interior.
    let mut pool = air();
    pool.set(4, 15, 4, fluid(WATER, 0), DAY);
    let vis = SectionVis::scan(&pool.view());
    assert_eq!(vis.boundary_faces, Direction::Up.bit());
    assert!(vis.interior_faces);
    assert!(vis.emits_any());

    // Same-state-culled glass touching an identical border emits nothing on
    // that side.
    let glass = Block {
        state_id: 519,
        flags: TRANSLUCENT | SAME_STATE_CULL,
        ..Block::AIR
    };
    let mut window = air();
    window.set(15, 8, 8, glass, DAY);
    let glass_border = [Sample::new(glass, DAY); FACE_CELLS];
    let mut view = window.view();
    view.borders[Direction::East as usize] = Some(&glass_border);
    let vis = SectionVis::scan(&view);
    // The only outward face is same-state-culled, yet the cube still emits
    // its five interior faces.
    assert_eq!(vis.boundary_faces, 0);
    assert!(vis.interior_faces);
    assert!(vis.emits_any());
}

#[test]
fn cull_facing_mask_and_frustum_decide_visibility() {
    let min = [0.0, 0.0, 0.0];
    assert_eq!(
        box_facing_mask(min, [-1.0, 8.0, 8.0]),
        Direction::West.bit()
    );
    assert_eq!(
        box_facing_mask(min, [20.0, -4.0, 30.0]),
        Direction::East.bit() | Direction::Down.bit() | Direction::South.bit()
    );
    assert_eq!(box_facing_mask(min, [8.0, 8.0, 8.0]), 0);

    // A boundary-only section is invisible from inside or behind a sealed side.
    let solid = Section::filled(STONE, DARK);
    let vis = SectionVis::scan(&solid.view());
    assert!(!vis.visible_towards(0));
    assert!(vis.visible_towards(box_facing_mask(min, [-1.0, 8.0, 8.0])));
    assert!(vis.visible_towards(box_facing_mask(min, [8.0, 20.0, 8.0])));

    // Six-plane frustum: inside half-space dot(n, p) + d >= 0.
    let left = [1.0, 0.0, 0.0, -20.0]; // x >= 20
    assert!(!box_in_frustum(min, &[left]));
    assert!(box_in_frustum([24.0, 0.0, 0.0], &[left]));
    let right = [-1.0, 0.0, 0.0, 40.0]; // x <= 40
    assert!(!box_in_frustum(min, &[left, right]));
    assert!(box_in_frustum([24.0, 0.0, 0.0], &[left, right]));
    // A top plane at y >= 17 clips a box that ends at y = 16.
    assert!(!box_in_frustum(
        [24.0, 0.0, 0.0],
        &[left, right, [0.0, 1.0, 0.0, -17.0]]
    ));
    assert!(box_in_frustum(
        [24.0, 0.0, 0.0],
        &[left, right, [0.0, 1.0, 0.0, -16.0]]
    ));
}

#[test]
fn column_culling_matches_scalar_reference_on_sealed_columns() {
    use crate::column::{mesh_column, ColumnSection};

    let solid = Section::filled(STONE, DARK);
    let mut v_opaque = solid.view();
    v_opaque.missing = MissingNeighbor::Opaque;
    let col: Vec<_> = (0..4)
        .map(|_| ColumnSection {
            view: v_opaque,
            uniform_air: false,
            sealed: false,
        })
        .collect();
    let quads = mesh_column(&col, [None; 4]);
    assert!(quads.is_empty());
    reference::assert_quads_bit_identical(
        &quads,
        &reference::mesh_column_scalar(&col, [None; 4]),
        "sealed_column",
    );

    // An air section on top unseals exactly the Up faces of the sealed
    // section below it: every emitted quad faces Up at y = 16.
    let top_air = air();
    let col2 = [
        ColumnSection {
            view: v_opaque,
            uniform_air: false,
            sealed: false,
        },
        ColumnSection {
            view: top_air.view(),
            uniform_air: false,
            sealed: false,
        },
    ];
    let quads = mesh_column(&col2, [None; 4]);
    reference::assert_quads_bit_identical(
        &quads,
        &reference::mesh_column_scalar(&col2, [None; 4]),
        "sealed_column_open_top",
    );

    // The caller `sealed` hint must not change output: a truthful hint only
    // skips the interior scan, so hinted columns match the unhinted result
    // whether the skip fires or an open side still emits boundary faces.
    let hinted: Vec<_> = col
        .iter()
        .map(|section| ColumnSection {
            view: section.view,
            uniform_air: section.uniform_air,
            sealed: true,
        })
        .collect();
    assert!(mesh_column(&hinted, [None; 4]).is_empty());
    let col3 = [
        ColumnSection {
            view: v_opaque,
            uniform_air: false,
            sealed: true,
        },
        ColumnSection {
            view: top_air.view(),
            uniform_air: false,
            sealed: false,
        },
    ];
    assert_eq!(mesh_column(&col3, [None; 4]), quads);
    assert!(!quads.is_empty());
    assert!(quads.iter().all(|q| q.direction == Direction::Up));
    assert!(quads
        .iter()
        .all(|q| q.positions.iter().all(|p| p[1] == 16.0)));
}

#[test]
fn direction_buckets_group_partitions_and_stays_stable() {
    // Multiple emitters spread across the section produce several quads per
    // direction, so every face bucket ends up non-empty.
    let mut section = air();
    section.set(4, 4, 4, STONE, DAY);
    section.set(10, 11, 12, STONE, DAY);
    section.set(
        7,
        7,
        7,
        Block {
            state_id: 519,
            flags: CUTOUT | TINTED,
            tint: 0x102030,
            ..Block::AIR
        },
        DAY,
    );
    section.set(2, 2, 2, fluid(WATER, 0), DAY);
    let quads = mesh_section(&section.view());
    assert!(quads.len() > DIRECTION_BUCKETS);

    let (grouped, buckets) = DirectionBuckets::group(&quads);
    assert_eq!(grouped.len(), quads.len());
    let mut next = 0;
    for b in 0..DIRECTION_BUCKETS {
        // Ranges tile the whole output with no gaps and contain exactly the
        // input quads of that bucket, in input order.
        assert_eq!(buckets.offset[b], next);
        let range = &grouped[next as usize..(next + buckets.count[b]) as usize];
        let expected: Vec<Quad> = quads
            .iter()
            .filter(|q| DirectionBuckets::index_of(q.direction) == b)
            .copied()
            .collect();
        assert_eq!(range, expected.as_slice(), "bucket {b}");
        next += buckets.count[b];
    }
    assert_eq!(next as usize, quads.len());
    // Mesher output always carries a face direction, so unassigned is empty.
    assert_eq!(buckets.count[DIRECTION_BUCKET_UNASSIGNED], 0);
    assert_eq!(
        buckets.offset[DIRECTION_BUCKET_UNASSIGNED] as usize,
        quads.len()
    );

    // group_into appends and reports absolute offsets into the buffer.
    let mut out = vec![quads[0]];
    let appended = DirectionBuckets::group_into(&quads, &mut out);
    assert_eq!(out.len(), quads.len() + 1);
    for b in 0..DIRECTION_BUCKETS {
        assert_eq!(appended.offset[b], buckets.offset[b] + 1);
        assert_eq!(appended.count[b], buckets.count[b]);
    }
    assert_eq!(&out[1..], grouped.as_slice());
}

#[test]
fn direction_buckets_draw_mask_and_vertex_ranges() {
    // An enclosed camera faces no boundary plane and must draw all buckets.
    assert_eq!(bucket_draw_mask(0), 0x7f);
    // A camera outside on one or more axes draws those face buckets plus the
    // unassigned bucket; the typical corner view skips three directions.
    let facing = Direction::West.bit() | Direction::Up.bit() | Direction::South.bit();
    let mask = bucket_draw_mask(facing);
    assert_eq!(mask, facing | (1 << DIRECTION_BUCKET_UNASSIGNED));
    assert_eq!(mask.count_ones(), 4);
    let enclosed = bucket_draw_mask(box_facing_mask([0.0; 3], [8.0, 8.0, 8.0]));
    assert_eq!(enclosed, 0x7f);
    let west_north = bucket_draw_mask(box_facing_mask([0.0; 3], [-1.0, 8.0, -1.0]));
    assert_eq!(
        west_north,
        Direction::West.bit() | Direction::North.bit() | (1 << DIRECTION_BUCKET_UNASSIGNED)
    );

    let buckets = DirectionBuckets {
        offset: [0, 3, 3, 9, 9, 9, 10],
        count: [0, 3, 0, 6, 0, 1, 0],
    };
    let vr = buckets.vertex_ranges();
    for (b, range) in vr.iter().enumerate() {
        assert_eq!(*range, (buckets.offset[b] * 4, buckets.count[b] * 4));
    }
}

mod lod_ffi {
    use super::*;
    use crate::ffi::{
        megu_client_mesh_summarize_column, MeguLodCell, MeshColumnInput, MeshColumnSection,
        ABI_BUFFER_TOO_SMALL, ABI_INVALID_ARGUMENT, ABI_OK, ABI_OPAQUE, ABI_VERSION,
    };
    use std::ptr;

    fn column_input(
        sections: &[Section],
        flags: impl Fn(usize) -> u32,
    ) -> (Vec<MeshColumnSection>, MeshColumnInput) {
        let inputs: Vec<_> = sections
            .iter()
            .enumerate()
            .map(|(index, section)| {
                let uniform_air = flags(index) == crate::ffi::ABI_COLUMN_UNIFORM_AIR;
                MeshColumnSection {
                    flags: flags(index),
                    blocks: if uniform_air {
                        ptr::null()
                    } else {
                        section.blocks.as_ptr()
                    },
                    block_light: section.block_light.as_ptr(),
                    sky_light: section.sky_light.as_ptr(),
                }
            })
            .collect();
        let input = MeshColumnInput {
            abi_version: ABI_VERSION,
            missing: ABI_OPAQUE,
            sections: inputs.as_ptr(),
            section_count: inputs.len(),
            borders: [ptr::null(); 4],
        };
        (inputs, input)
    }

    // SAFETY: helper keeps `inputs` alive for the duration of each call.
    unsafe fn summarize(
        input: &MeshColumnInput,
        resolution: u32,
        output: *mut MeguLodCell,
        capacity: usize,
        count: &mut usize,
    ) -> i32 {
        unsafe { megu_client_mesh_summarize_column(input, resolution, output, capacity, count) }
    }

    fn clone_input(input: &MeshColumnInput) -> MeshColumnInput {
        MeshColumnInput {
            abi_version: input.abi_version,
            missing: input.missing,
            sections: input.sections,
            section_count: input.section_count,
            borders: input.borders,
        }
    }

    fn fixture() -> (Vec<Section>, MeshColumnInput, Vec<MeshColumnSection>) {
        let mut lower = Section::filled(Block::AIR, Light::new(0, 15));
        lower.set(2, 3, 1, STONE, Light::new(4, 9));
        lower.set(0, 15, 0, STONE, DARK);
        let mut upper = air();
        let leaves = Block {
            state_id: 238,
            tint: 0x0022_8833,
            flags: CUTOUT | TINTED,
            ..Block::AIR
        };
        upper.set(2, 3, 1, leaves, Light::new(1, 14));
        let sections = vec![lower, upper, air()];
        let (inputs, input) = column_input(&sections, |i| {
            if i == 2 {
                crate::ffi::ABI_COLUMN_UNIFORM_AIR
            } else {
                0
            }
        });
        (sections, input, inputs)
    }

    #[test]
    fn lod_ffi_rejects_invalid_arguments() {
        let (_sections, input, _inputs) = fixture();
        let mut count = usize::MAX;
        let mut out = [MeguLodCell::default(); LOD_FINE * LOD_FINE];

        // Null count pointer.
        assert_eq!(
            unsafe {
                megu_client_mesh_summarize_column(
                    &input,
                    LOD_FINE as u32,
                    out.as_mut_ptr(),
                    out.len(),
                    ptr::null_mut(),
                )
            },
            ABI_INVALID_ARGUMENT
        );
        // Null input zeroes the count.
        assert_eq!(
            unsafe {
                megu_client_mesh_summarize_column(
                    ptr::null(),
                    LOD_FINE as u32,
                    out.as_mut_ptr(),
                    out.len(),
                    &mut count,
                )
            },
            ABI_INVALID_ARGUMENT
        );
        assert_eq!(count, 0);
        // Output without capacity, and capacity without output semantics.
        assert_eq!(
            unsafe { summarize(&input, LOD_FINE as u32, ptr::null_mut(), 4, &mut count) },
            ABI_INVALID_ARGUMENT
        );
        // Unsupported resolutions.
        for resolution in [0, 1, 8, 15, 17, u32::MAX] {
            assert_eq!(
                unsafe { summarize(&input, resolution, out.as_mut_ptr(), out.len(), &mut count) },
                ABI_INVALID_ARGUMENT,
                "resolution {resolution}"
            );
        }
        // Mutated header fields.
        let mut bad = clone_input(&input);
        bad.abi_version = ABI_VERSION + 1;
        assert_eq!(
            unsafe {
                summarize(
                    &bad,
                    LOD_FINE as u32,
                    out.as_mut_ptr(),
                    out.len(),
                    &mut count,
                )
            },
            ABI_INVALID_ARGUMENT
        );
        let mut bad = clone_input(&input);
        bad.missing = 7;
        assert_eq!(
            unsafe {
                summarize(
                    &bad,
                    LOD_FINE as u32,
                    out.as_mut_ptr(),
                    out.len(),
                    &mut count,
                )
            },
            ABI_INVALID_ARGUMENT
        );
        let mut bad = clone_input(&input);
        bad.sections = ptr::null();
        assert_eq!(
            unsafe {
                summarize(
                    &bad,
                    LOD_FINE as u32,
                    out.as_mut_ptr(),
                    out.len(),
                    &mut count,
                )
            },
            ABI_INVALID_ARGUMENT
        );
        let mut bad = clone_input(&input);
        bad.section_count = 0;
        assert_eq!(
            unsafe {
                summarize(
                    &bad,
                    LOD_FINE as u32,
                    out.as_mut_ptr(),
                    out.len(),
                    &mut count,
                )
            },
            ABI_INVALID_ARGUMENT
        );
        let mut bad = clone_input(&input);
        bad.section_count = crate::ffi::ABI_MAX_COLUMN_SECTIONS + 1;
        assert_eq!(
            unsafe {
                summarize(
                    &bad,
                    LOD_FINE as u32,
                    out.as_mut_ptr(),
                    out.len(),
                    &mut count,
                )
            },
            ABI_INVALID_ARGUMENT
        );
        assert_eq!(count, 0);
    }

    #[test]
    fn lod_ffi_rejects_bad_section_descriptors() {
        let sections = vec![air(), air()];
        let (mut inputs, input) = column_input(&sections, |_| 0);
        let mut count = 0usize;
        let mut out = [MeguLodCell::default(); LOD_COARSE * LOD_COARSE];

        // Dense flags require a block array.
        inputs[0].blocks = ptr::null();
        assert_eq!(
            unsafe {
                summarize(
                    &input,
                    LOD_COARSE as u32,
                    out.as_mut_ptr(),
                    out.len(),
                    &mut count,
                )
            },
            ABI_INVALID_ARGUMENT
        );
        inputs[0].blocks = sections[0].blocks.as_ptr();
        // Unknown flags and uniform air with a supplied block array fail.
        inputs[0].flags = 99;
        assert_eq!(
            unsafe {
                summarize(
                    &input,
                    LOD_COARSE as u32,
                    out.as_mut_ptr(),
                    out.len(),
                    &mut count,
                )
            },
            ABI_INVALID_ARGUMENT
        );
        inputs[0].flags = crate::ffi::ABI_COLUMN_UNIFORM_AIR;
        assert_eq!(
            unsafe {
                summarize(
                    &input,
                    LOD_COARSE as u32,
                    out.as_mut_ptr(),
                    out.len(),
                    &mut count,
                )
            },
            ABI_INVALID_ARGUMENT
        );
        // Light arrays are always required.
        inputs[0].flags = 0;
        inputs[0].block_light = ptr::null();
        assert_eq!(
            unsafe {
                summarize(
                    &input,
                    LOD_COARSE as u32,
                    out.as_mut_ptr(),
                    out.len(),
                    &mut count,
                )
            },
            ABI_INVALID_ARGUMENT
        );
        assert_eq!(count, 0);
    }

    #[test]
    fn lod_ffi_queries_and_matches_summarize_column() {
        let (sections, input, _inputs) = fixture();
        let views: Vec<_> = sections.iter().map(Section::view).collect();
        let mut count = 0usize;

        for (resolution, edge) in [(LOD_COARSE as u32, LOD_COARSE), (LOD_FINE as u32, LOD_FINE)] {
            let required = edge * edge;
            // NULL output queries the required cell count.
            assert_eq!(
                unsafe { summarize(&input, resolution, ptr::null_mut(), 0, &mut count) },
                ABI_OK
            );
            assert_eq!(count, required);

            // A short buffer reports the requirement and stays untouched.
            let mut short = vec![
                MeguLodCell {
                    state_id: u32::MAX,
                    ..MeguLodCell::default()
                };
                required - 1
            ];
            let sentinel = short[0];
            assert_eq!(
                unsafe {
                    summarize(
                        &input,
                        resolution,
                        short.as_mut_ptr(),
                        short.len(),
                        &mut count,
                    )
                },
                ABI_BUFFER_TOO_SMALL
            );
            assert_eq!(count, required);
            assert_eq!(short[0], sentinel);

            // A full buffer matches summarize_column field for field.
            let mut out = vec![MeguLodCell::default(); required];
            assert_eq!(
                unsafe { summarize(&input, resolution, out.as_mut_ptr(), out.len(), &mut count) },
                ABI_OK
            );
            assert_eq!(count, required);
            let mut expected = vec![LodCell::EMPTY; required];
            summarize_column(&views, edge, &mut expected);
            for (got, want) in out.iter().zip(&expected) {
                assert_eq!(got.state_id, want.state_id);
                assert_eq!(got.tint, want.tint);
                assert_eq!(got.top_y, want.top_y);
                assert_eq!(got.light, want.light);
            }
            if edge == LOD_FINE {
                // Spot-check the known top through the FFI layout.
                let fine_cell = out[LOD_FINE + 2];
                assert_eq!(fine_cell.top_y, EDGE as i16 + 3);
                assert_eq!(fine_cell.state_id, 238);
                assert_eq!(fine_cell.tint, 0x0022_8833);
            }
        }
    }
}

// 26.3-era registries carry block-state ids past the 1.21.1 count (26,684)
// and past u16::MAX. `Block::state_id`, `Quad::state_id`,
// `PackedVertex::state_id` and `LodCell::state_id` are u32 end to end, so
// ids like 30_000, 50_000, 70_000 and u32::MAX survive meshing, culling,
// packing and LOD summarization without truncation.
#[test]
fn high_state_ids_mesh_pack_and_summarize_without_truncation() {
    use crate::packed::{is_packable, pack_quad};

    // Past the vanilla count, past u16::MAX, and at the u32 limit.
    let ids = [30_000, 50_000, 70_000, u32::MAX];
    let mut section = air();
    for (x, &id) in ids.iter().enumerate() {
        section.set(x * 4, 8, 8, Block::opaque_cube(id), DARK);
    }
    let quads = mesh_section(&section.view());
    for &id in &ids {
        let quad = face(&quads, Direction::Up, id);
        assert_eq!(quad.state_id, id);
        assert!(is_packable(quad));
        assert!(pack_quad(quad).iter().all(|v| v.state_id == id));
    }
    assert!(quads.iter().all(|q| ids.contains(&q.state_id)));

    // LOD cells keep the exact id at the top of each column.
    let mut out = [LodCell::EMPTY; LOD_FINE * LOD_FINE];
    summarize_column(&[section.view()], LOD_FINE, &mut out);
    for (x, &id) in ids.iter().enumerate() {
        let cell = out[8 * EDGE + x * 4];
        assert_eq!((cell.state_id, cell.top_y), (id, 8));
    }
}

#[test]
fn same_state_cull_compares_the_full_u32_state_id() {
    // 70_000 is 0x1_1170: only the low 16 bits collide with 4_464 (0x1170).
    // A u16 comparison would cull a face that must be emitted.
    let glass = |state_id| Block {
        state_id,
        flags: TRANSLUCENT | SAME_STATE_CULL,
        ..Block::AIR
    };
    let mut pair = air();
    pair.set(0, 8, 8, glass(70_000), DAY);
    pair.set(1, 8, 8, glass(70_000), DAY);
    let quads = mesh_section(&pair.view());
    // Identical ids share the x = 1 plane and cull it entirely.
    assert!(!quads
        .iter()
        .any(|q| q.positions.iter().all(|p| p[0] == 1.0)));

    pair.set(1, 8, 8, glass(4_464), DAY);
    let quads = mesh_section(&pair.view());
    let shared = |direction, id| {
        quads.iter().any(|q| {
            q.direction == direction
                && q.state_id == id
                && q.positions.iter().all(|p| p[0] == 1.0)
        })
    };
    // Ids equal in their low 16 bits but different in u32 must both emit.
    assert!(shared(Direction::East, 70_000));
    assert!(shared(Direction::West, 4_464));
}

#[test]
fn packed_vertex_y_ceiling_is_1024_blocks() {
    use crate::packed::is_packable;

    let solid = Section::filled(STONE, DAY);
    let quads = mesh_section(&solid.view());
    let top = *quads
        .iter()
        .find(|q| q.direction == Direction::Up)
        .unwrap();
    // Shifting a planar face up keeps it packable just under the 20-bit
    // Y * 1024 field limit, and unrepresentable at it: a quad that cannot
    // round-trip must be rejected, not truncated into the zero bits.
    let mut just_under = top;
    just_under
        .positions
        .iter_mut()
        .for_each(|p| p[1] = 1023.9375);
    assert!(is_packable(&just_under));
    let mut at_ceiling = just_under;
    at_ceiling
        .positions
        .iter_mut()
        .for_each(|p| p[1] = 1024.0);
    assert!(!is_packable(&at_ceiling));

    // Regions fail closed on an unpackable upload instead of silently
    // dropping vertices into the arenas.
    let mut regions = RenderRegions::default();
    assert!(!regions.upload(SectionCoord(0, 0, 0), &[at_ceiling]));
    assert!(regions.region(SectionCoord(0, 0, 0)).is_none());
    assert!(regions.upload(SectionCoord(0, 0, 0), &[just_under]));
    let slab = regions
        .region(SectionCoord(0, 0, 0))
        .unwrap()
        .slab(SectionCoord(0, 0, 0))
        .unwrap();
    assert_eq!((slab.vertices, slab.indices), (4, 6));
}

// 80 sections at sec_y = -32..48 is 1,280 blocks: past the vanilla 24-section
// window and past the old 32-section column bound, spanning both negative and
// positive section Y through a signed coordinate space.
#[test]
fn tall_column_meshes_summarizes_and_traverses_across_negative_sec_y() {
    use crate::column::{mesh_column, ColumnSection};
    use crate::packed::is_packable;

    const SECTIONS: usize = 80;
    const MIN_SEC_Y: i32 = -32;

    // A two-slab stone floor, open air, and a high-id marker capping the
    // column: enough shape for every consumer to prove its path.
    let mut sections: Vec<Section> = (0..SECTIONS).map(|_| air()).collect();
    sections[0] = Section::filled(STONE, DAY);
    sections[1] = Section::filled(STONE, DAY);
    let marker = Block::opaque_cube(70_000);
    sections[SECTIONS - 1].set(8, 15, 8, marker, Light::new(3, 15));
    let coord = |i: usize| SectionCoord(0, MIN_SEC_Y + i as i32, 0);
    let views: Vec<_> = sections.iter().map(Section::view).collect();

    // FaceVisibility is section-local: sealed slabs connect nothing, air and
    // the mostly-empty marker section connect every face pair.
    let graphs: Vec<(SectionCoord, SectionGraph)> = sections
        .iter()
        .enumerate()
        .map(|(i, section)| {
            (
                coord(i),
                SectionGraph {
                    through: FaceVisibility::compute(&section.blocks).bits(),
                    renderable: SectionVis::scan(&section.view()).emits_any(),
                },
            )
        })
        .collect();
    assert_eq!(graphs[0].1, SectionGraph::SOLID);
    assert_eq!(graphs[1].1, SectionGraph::SOLID);
    for &(_, graph) in &graphs[2..SECTIONS - 1] {
        assert_eq!(graph.through, THROUGH_ALL);
        assert!(!graph.renderable);
    }
    assert_eq!(graphs[SECTIONS - 1].1.through, THROUGH_ALL);
    assert!(graphs[SECTIONS - 1].1.renderable);

    // The culler walks signed section coordinates: a camera inside the air
    // band propagates up across the sec_y -1..0 boundary and down into the
    // top stone slab, which occludes everything below it.
    let result = GraphCuller::new(SectionCoord(0, 0, 0), &[]).search(graphs.iter().copied());
    let mut visible: Vec<i32> = result.visible.iter().map(|coord| coord.1).collect();
    visible.sort_unstable();
    assert_eq!(visible, [MIN_SEC_Y + 1, MIN_SEC_Y + SECTIONS as i32 - 1]);
    assert_eq!(result.culled, 1); // only the buried slab at MIN_SEC_Y
    // A camera sealed in the floor sees only its own slab.
    let buried = GraphCuller::new(SectionCoord(0, MIN_SEC_Y, 0), &[]).search(graphs.iter().copied());
    assert_eq!(buried.visible, [SectionCoord(0, MIN_SEC_Y, 0)]);
    assert_eq!(buried.culled, 2);

    // Region partitioning floors negative coords: sec_y -1 lives in the
    // (-4..0) slab, -32 anchors its own slab.
    assert_eq!(
        RenderRegions::origin_of(SectionCoord(0, -1, 0)),
        [0, -4, 0]
    );
    assert_eq!(
        RenderRegions::origin_of(SectionCoord(0, MIN_SEC_Y, 0)),
        [0, MIN_SEC_Y, 0]
    );

    // Mesh each section through the cache and upload at its signed coord;
    // section-local quads always fit the packed format, negative Y included.
    let mut cache = MeshCache::default();
    for (i, section) in sections.iter().enumerate() {
        assert!(cache.insert(coord(i), section.clone()));
    }
    assert_eq!(cache.pending(), SECTIONS);
    cache.rebuild_dirty(usize::MAX);
    let mut regions = RenderRegions::default();
    for i in 0..SECTIONS {
        assert!(regions.upload(coord(i), cache.mesh(coord(i)).unwrap()));
    }
    for sy in MIN_SEC_Y..MIN_SEC_Y + SECTIONS as i32 {
        assert!(regions.region(SectionCoord(0, sy, 0)).is_some(), "sec_y {sy}");
    }
    // A camera under the floor sees one multi-draw command per renderable
    // region: the stone slab pair at sec_y -32 and the marker slab at 44.
    let commands = regions.build_multidraw([8.0, (MIN_SEC_Y * 16 - 8) as f32, 8.0]);
    assert_eq!(
        commands.iter().map(|c| c.region).collect::<Vec<_>>(),
        [[0, MIN_SEC_Y, 0], [0, MIN_SEC_Y + 76, 0]]
    );
    assert_eq!(commands[0].draws.len(), 2);
    assert_eq!(commands[1].draws.len(), 1);
    assert!(commands.iter().all(|c| c.facing == Direction::Down.bit()));

    // The column mesher walks all 80 descriptors bottom to top and keeps
    // column-local Y exact: the marker cap emits at 1,280 blocks, which is
    // precisely why a vertex upload must refuse it.
    let column: Vec<_> = views
        .iter()
        .map(|&view| ColumnSection {
            view,
            uniform_air: false,
            sealed: false,
        })
        .collect();
    let quads = mesh_column(&column, [None; 4]);
    let cap = face(&quads, Direction::Up, 70_000);
    assert_eq!(cap.positions.map(|p| p[1]), [1280.0; 4]);
    assert!(!is_packable(cap));
    // The sealed plane between the two stone slabs never emits.
    assert!(quads.iter().all(|q| {
        !(matches!(q.direction, Direction::Up | Direction::Down)
            && q.positions.iter().all(|p| p[1] == 16.0))
    }));
    assert!(quads.iter().any(|q| {
        q.direction == Direction::Up && q.positions.iter().all(|p| p[1] == 32.0)
    }));
    assert!(quads.iter().any(|q| {
        q.direction == Direction::Down && q.positions.iter().all(|p| p[1] == 0.0)
    }));

    // LOD over the same 80 views: height is the full 1,280 blocks, top_y
    // reaches 1,279 (still far below the i16 field), and the marker keeps
    // its exact u32 id.
    let mut summary = LodSummary::new(LOD_FINE);
    let cells = summary.update(&views).to_vec();
    assert_eq!(summary.height(), SECTIONS * EDGE);
    let top = cells[8 * EDGE + 8];
    assert_eq!((top.top_y, top.state_id, top.light), (1279, 70_000, 0xf3));
    for (i, cell) in cells.iter().enumerate() {
        if i != 8 * EDGE + 8 {
            assert_eq!(
                (cell.top_y, cell.state_id),
                (31, STONE.state_id),
                "column cell {i}"
            );
        }
    }
}
