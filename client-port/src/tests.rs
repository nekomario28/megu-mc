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
