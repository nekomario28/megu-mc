// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

use crate::{
    mesh::{
        encode_fluid, encode_occ, fill_vertical_edges_and_corners_missing, load_interior,
        load_sample_border_plane_into, mesh_loaded_section, missing_encoded, pad_index,
        unpack_light_row, BorderStates, MesherScratch, QuadSink, PAD_EDGE, PAD_SLICE,
    },
    Direction, Sample, SectionView, EDGE, FACE_CELLS,
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
    /// Caller guarantee that every cell is a full-occlusion opaque non-fluid
    /// cube. Lets the culler skip the interior scan; boundary faces are still
    /// checked against the real neighbors.
    pub sealed: bool,
}

struct ColumnBorders<'a> {
    sections: &'a [ColumnSection<'a>],
    index: usize,
    neighbors: [Option<&'a [FacePlane]>; 4],
}

impl BorderStates for ColumnBorders<'_> {
    #[inline(always)]
    fn border_state_id(&self, direction: Direction, x: usize, y: usize, z: usize) -> u32 {
        match direction {
            Direction::Down => self.index.checked_sub(1).map_or(0, |other| {
                self.sections[other].view.blocks[(EDGE - 1) * FACE_CELLS + z * EDGE + x].state_id
            }),
            Direction::Up => self
                .sections
                .get(self.index + 1)
                .map_or(0, |other| other.view.blocks[z * EDGE + x].state_id),
            Direction::North => self.neighbors[0].map_or(0, |faces| {
                faces[self.index][direction.border_index([x, y, z])]
                    .block
                    .state_id
            }),
            Direction::South => self.neighbors[1].map_or(0, |faces| {
                faces[self.index][direction.border_index([x, y, z])]
                    .block
                    .state_id
            }),
            Direction::West => self.neighbors[2].map_or(0, |faces| {
                faces[self.index][direction.border_index([x, y, z])]
                    .block
                    .state_id
            }),
            Direction::East => self.neighbors[3].map_or(0, |faces| {
                faces[self.index][direction.border_index([x, y, z])]
                    .block
                    .state_id
            }),
        }
    }
}

fn fill_vertical_plane(
    section: Option<&ColumnSection<'_>>,
    y: usize,
    py: usize,
    miss_occ: u8,
    miss_light: u8,
    load_fluid: bool,
    scratch: &mut MesherScratch,
) {
    let plane_base = py * PAD_SLICE;
    match section {
        Some(other) => {
            let block_y = y * FACE_CELLS;
            let light_y = y * (FACE_CELLS / 2);
            for z in 0..EDGE {
                let dst = plane_base + (z + 1) * PAD_EDGE + 1;
                if other.uniform_air {
                    scratch.occ[dst..dst + EDGE].fill(0);
                    if load_fluid {
                        scratch.fluid[dst..dst + EDGE].fill(0);
                    }
                } else {
                    let src = &other.view.blocks[block_y + z * EDGE..block_y + z * EDGE + EDGE];
                    for (x, &block) in src.iter().enumerate() {
                        scratch.occ[dst + x] = encode_occ(block);
                    }
                    if load_fluid {
                        for (x, &block) in src.iter().enumerate() {
                            scratch.fluid[dst + x] = encode_fluid(block);
                        }
                    }
                }
                unpack_light_row(
                    other.view.block_light,
                    other.view.sky_light,
                    light_y + z * (EDGE / 2),
                    &mut scratch.light[dst..dst + EDGE],
                );
            }
        }
        None => {
            for z in 0..EDGE {
                let dst = plane_base + (z + 1) * PAD_EDGE + 1;
                scratch.occ[dst..dst + EDGE].fill(miss_occ);
                scratch.light[dst..dst + EDGE].fill(miss_light);
                if load_fluid {
                    scratch.fluid[dst..dst + EDGE].fill(0);
                }
            }
        }
    }
}

fn fill_cardinal_missing(
    dir: Direction,
    miss_occ: u8,
    miss_light: u8,
    load_fluid: bool,
    scratch: &mut MesherScratch,
) {
    match dir {
        Direction::North | Direction::South => {
            let pz = if dir == Direction::North {
                0
            } else {
                PAD_EDGE - 1
            };
            for y in 0..EDGE {
                let dst = (y + 1) * PAD_SLICE + pz * PAD_EDGE + 1;
                scratch.occ[dst..dst + EDGE].fill(miss_occ);
                scratch.light[dst..dst + EDGE].fill(miss_light);
                if load_fluid {
                    scratch.fluid[dst..dst + EDGE].fill(0);
                }
            }
        }
        Direction::West | Direction::East => {
            let px = if dir == Direction::West {
                0
            } else {
                PAD_EDGE - 1
            };
            for y in 0..EDGE {
                let row_base = (y + 1) * PAD_SLICE + px;
                for z in 0..EDGE {
                    let idx = row_base + (z + 1) * PAD_EDGE;
                    scratch.occ[idx] = miss_occ;
                    scratch.light[idx] = miss_light;
                }
                if load_fluid {
                    for z in 0..EDGE {
                        let idx = row_base + (z + 1) * PAD_EDGE;
                        scratch.fluid[idx] = 0;
                    }
                }
            }
        }
        Direction::Down | Direction::Up => unreachable!(),
    }
}

fn load_column_shell(
    sections: &[ColumnSection<'_>],
    index: usize,
    neighbors: [Option<&[FacePlane]>; 4],
    load_fluid: bool,
    reuse_missing_sides: bool,
    scratch: &mut MesherScratch,
) {
    let (miss_occ, miss_light) = missing_encoded(sections[index].view.missing);

    let down_section = index.checked_sub(1).map(|i| &sections[i]);
    let up_section = sections.get(index + 1);
    fill_vertical_plane(
        down_section,
        EDGE - 1,
        0,
        miss_occ,
        miss_light,
        load_fluid,
        scratch,
    );
    fill_vertical_plane(
        up_section,
        0,
        PAD_EDGE - 1,
        miss_occ,
        miss_light,
        load_fluid,
        scratch,
    );

    if reuse_missing_sides {
        return;
    }

    for (side, neighbor) in CARDINALS.into_iter().zip(neighbors) {
        match neighbor {
            Some(faces) => load_sample_border_plane_into(&faces[index], side, load_fluid, scratch),
            None => fill_cardinal_missing(side, miss_occ, miss_light, load_fluid, scratch),
        }
    }

    fill_vertical_edges_and_corners_missing(miss_occ, miss_light, load_fluid, scratch);

    // Horizontal edges at py = 0 (Down) and py = 17 (Up) from cardinal neighbors of adjacent sections.
    for (py, y, other_opt) in [
        (0usize, EDGE - 1, index.checked_sub(1)),
        (
            PAD_EDGE - 1,
            0usize,
            (index + 1 < sections.len()).then_some(index + 1),
        ),
    ] {
        // North (pz = 0) and South (pz = 17): border_index = y * 16 + x
        for (pz, neighbor) in [(0usize, neighbors[0]), (PAD_EDGE - 1, neighbors[1])] {
            let dst = pad_index(1, py, pz);
            match (other_opt, neighbor) {
                (Some(other), Some(faces)) => {
                    let src = &faces[other][y * EDGE..y * EDGE + EDGE];
                    for (x, &s) in src.iter().enumerate() {
                        scratch.occ[dst + x] = encode_occ(s.block);
                        scratch.light[dst + x] = s.light.0;
                    }
                    if load_fluid {
                        for (x, &s) in src.iter().enumerate() {
                            scratch.fluid[dst + x] = encode_fluid(s.block);
                        }
                    }
                }
                _ => {
                    scratch.occ[dst..dst + EDGE].fill(miss_occ);
                    scratch.light[dst..dst + EDGE].fill(miss_light);
                    if load_fluid {
                        scratch.fluid[dst..dst + EDGE].fill(0);
                    }
                }
            }
        }
        // West (px = 0) and East (px = 17): border_index = z * 16 + y
        for (px, neighbor) in [(0usize, neighbors[2]), (PAD_EDGE - 1, neighbors[3])] {
            match (other_opt, neighbor) {
                (Some(other), Some(faces)) => {
                    let plane = &faces[other];
                    for z in 0..EDGE {
                        let idx = pad_index(px, py, z + 1);
                        let s = plane[z * EDGE + y];
                        scratch.occ[idx] = encode_occ(s.block);
                        scratch.light[idx] = s.light.0;
                        if load_fluid {
                            scratch.fluid[idx] = encode_fluid(s.block);
                        }
                    }
                }
                _ => {
                    for z in 0..EDGE {
                        let idx = pad_index(px, py, z + 1);
                        scratch.occ[idx] = miss_occ;
                        scratch.light[idx] = miss_light;
                        if load_fluid {
                            scratch.fluid[idx] = 0;
                        }
                    }
                }
            }
        }
    }
}

pub(crate) fn mesh_column_into<S: QuadSink>(
    sections: &[ColumnSection<'_>],
    neighbors: [Option<&[FacePlane]>; 4],
    scratch: &mut MesherScratch,
    quads: &mut S,
) {
    let no_cardinals = neighbors.iter().all(Option::is_none);
    let mut cached_missing: Option<(crate::MissingNeighbor, bool)> = None;
    for (index, section) in sections.iter().enumerate() {
        if section.uniform_air {
            continue;
        }
        let view = &section.view;
        let miss = view.missing.sample().block;
        let missing_sealed = matches!(view.missing, crate::MissingNeighbor::Opaque);
        // Same skip as `mesh_section_with_scratch`: a section sealed on every
        // side by its neighbors (or holding nothing renderable) cannot emit a
        // quad, so it avoids the interior load, shell fill and greedy pass.
        // A uniform-air neighbor counts as air no matter what its buffers say.
        if crate::cull::emits_nothing(
            view.blocks,
            section.sealed,
            |direction| match direction {
                Direction::Down => index.checked_sub(1).map_or(missing_sealed, |other| {
                    !sections[other].uniform_air
                        && sections[other].view.blocks[(EDGE - 1) * FACE_CELLS..EDGE * FACE_CELLS]
                            .iter()
                            .all(|block| block.occludes(Direction::Up))
                }),
                Direction::Up => sections.get(index + 1).map_or(missing_sealed, |other| {
                    !other.uniform_air
                        && other.view.blocks[..FACE_CELLS]
                            .iter()
                            .all(|block| block.occludes(Direction::Down))
                }),
                cardinal => {
                    // CARDINALS order is North..East = directions 2..=5.
                    neighbors[cardinal as usize - 2].map_or(missing_sealed, |faces| {
                        faces[index]
                            .iter()
                            .all(|sample| sample.block.occludes(cardinal.opposite()))
                    })
                }
            },
            |direction, p| match direction {
                Direction::Down => index.checked_sub(1).map_or(miss, |other| {
                    if sections[other].uniform_air {
                        crate::Block::AIR
                    } else {
                        sections[other].view.blocks[(EDGE - 1) * FACE_CELLS + p[2] * EDGE + p[0]]
                    }
                }),
                Direction::Up => sections.get(index + 1).map_or(miss, |other| {
                    if other.uniform_air {
                        crate::Block::AIR
                    } else {
                        other.view.blocks[p[2] * EDGE + p[0]]
                    }
                }),
                cardinal => neighbors[cardinal as usize - 2]
                    .map_or(miss, |faces| faces[index][cardinal.border_index(p)].block),
            },
        ) {
            continue;
        }
        let scan = load_interior(view.blocks, view.block_light, view.sky_light, scratch);
        if !scan.any_cube && !scan.any_fluid {
            continue;
        }
        let reuse_missing_sides = no_cardinals
            && matches!(
                cached_missing,
                Some((m, fluid_zeroed)) if m == view.missing && (!scan.any_fluid || fluid_zeroed)
            );
        load_column_shell(
            sections,
            index,
            neighbors,
            scan.any_fluid,
            reuse_missing_sides,
            scratch,
        );
        if no_cardinals {
            let fluid_zeroed =
                scan.any_fluid || matches!(cached_missing, Some((m, true)) if m == view.missing);
            cached_missing = Some((view.missing, fluid_zeroed));
        }
        let borders = ColumnBorders {
            sections,
            index,
            neighbors,
        };
        let y_offset = (index * EDGE) as f32;
        mesh_loaded_section(view.blocks, &borders, scan, scratch, y_offset, quads);
    }
}

#[cfg(test)]
pub(crate) fn mesh_column(
    sections: &[ColumnSection<'_>],
    neighbors: [Option<&[FacePlane]>; 4],
) -> Vec<crate::Quad> {
    crate::mesh::with_scratch(|scratch| {
        let mut quads = Vec::new();
        mesh_column_into(sections, neighbors, scratch, &mut quads);
        quads
    })
}
