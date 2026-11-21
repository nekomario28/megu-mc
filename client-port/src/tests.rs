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
            },
            ColumnSection {
                view: upper.view(),
                uniform_air: false,
            },
            ColumnSection {
                view: air.view(),
                uniform_air: true,
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
            },
            ColumnSection {
                view: upper.view(),
                uniform_air: false,
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
        mc_spine_client_mesh_column, mc_spine_client_mesh_pack_vertices,
        mc_spine_client_mesh_sort_translucent, MeshColumnInput, MeshColumnSection, PackedVertex,
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
            },
            None => ColumnSection {
                view: air_section.view(),
                uniform_air: true,
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
        unsafe { mc_spine_client_mesh_column(&ow_input, ptr::null_mut(), 0, &mut quad_count) },
        ABI_OK
    );
    let mut ow_buf = vec![MaybeUninit::<Quad>::uninit(); quad_count];
    assert_eq!(
        unsafe {
            mc_spine_client_mesh_column(
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
            mc_spine_client_mesh_pack_vertices(
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
                mc_spine_client_mesh_pack_vertices(
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
                    mc_spine_client_mesh_sort_translucent(
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
            },
            ColumnSection {
                view: make_view(&sec1, missing),
                uniform_air: false,
            },
            ColumnSection {
                view: make_view(&sec2, missing),
                uniform_air: true,
            },
            ColumnSection {
                view: make_view(&sec3, missing),
                uniform_air: false,
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
        },
        ColumnSection {
            view: v1,
            uniform_air: false,
        },
        ColumnSection {
            view: v2,
            uniform_air: false,
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
