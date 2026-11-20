// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

use crate::{
    edge_corner_index, mesh::append_section, Direction, Quad, Sample, SectionView, EDGE,
    EDGE_CORNER_CELLS, FACE_CELLS,
};

pub(crate) type FacePlane = [Sample; FACE_CELLS];
pub(crate) const CARDINALS: [Direction; 4] = [
    Direction::North,
    Direction::South,
    Direction::West,
    Direction::East,
];

pub(crate) struct ColumnSection<'a> {
    pub view: SectionView<'a>,
    pub uniform_air: bool,
}

pub(crate) fn mesh_column(
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

            // Cardinal planes from the adjacent section supply the X/Y and Y/Z edges.
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
        append_section(&view, &mut quads);
        let y_offset = (index * EDGE) as f32;
        for quad in &mut quads[start..] {
            for position in &mut quad.positions {
                position[1] += y_offset;
            }
        }
    }
    quads
}
