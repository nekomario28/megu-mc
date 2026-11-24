// SPDX-License-Identifier: LGPL-3.0-or-later

//! Section visibility masks and emission proofs: a six-bit face mask plus
//! cheap "emits nothing" proofs that let sealed and uniform-air sections skip
//! the greedy mesher entirely.

use crate::section::{
    section_index, Block, Direction, SectionView, ALL_FACES, CUTOUT, EDGE, FACE_CELLS, NO_FLUID,
    OPAQUE, SAME_STATE_CULL, SECTION_CELLS, TRANSLUCENT,
};

/// Flags that mark a block as a renderable cube, matching the mesher's
/// `CUBE_FLAGS` test.
const RENDER_FLAGS: u8 = OPAQUE | CUTOUT | TRANSLUCENT;

/// Cell-index deltas along each `Direction` inside the 16^3 block array.
const STEP: [isize; 6] = [
    -(FACE_CELLS as isize), // Down
    FACE_CELLS as isize,    // Up
    -(EDGE as isize),       // North
    EDGE as isize,          // South
    -1,                     // West
    1,                      // East
];

/// Conservative mirror of the mesher's face rules for one
/// `block -> direction -> neighbor` triple. The cube branch matches
/// `greedy_slice` exactly, including same-state culling; the fluid branch drops
/// the same-kind merge rule, so it may over-report but never culls a real face.
#[inline(always)]
fn face_can_emit(block: Block, neighbor: Block, direction: Direction) -> bool {
    let neighbor_occludes = neighbor.occludes(direction.opposite());
    if block.flags & RENDER_FLAGS != 0
        && !neighbor_occludes
        && !(block.flags & neighbor.flags & SAME_STATE_CULL != 0
            && block.state_id == neighbor.state_id)
    {
        return true;
    }
    block.fluid() != NO_FLUID && !neighbor_occludes && !block.occludes(direction)
}

#[inline(always)]
fn on_boundary(direction: Direction, p: [usize; 3]) -> bool {
    match direction {
        Direction::Down => p[1] == 0,
        Direction::Up => p[1] == EDGE - 1,
        Direction::North => p[2] == 0,
        Direction::South => p[2] == EDGE - 1,
        Direction::West => p[0] == 0,
        Direction::East => p[0] == EDGE - 1,
    }
}

/// Six-bit boundary face mask plus interior/uniform summary for one section.
/// Reported faces are a superset of what the mesher would emit: a set bit can
/// still produce no quad, but a cleared bit or flag is a proof, not a guess.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SectionVis {
    /// One `Direction::bit` per side: set when the boundary slice can emit an
    /// outward face through the section's neighbor plane.
    pub boundary_faces: u8,
    /// Any emitted face that does not lie on the section boundary: inward
    /// faces of shell cells and faces between interior cells alike.
    pub interior_faces: bool,
    /// No renderable cell anywhere in the section.
    pub uniform_air: bool,
}

impl SectionVis {
    /// Scan every cell once against the neighbor planes in `view`. Border cells
    /// read `view.borders`; missing planes fall back to `view.missing`.
    pub fn scan(view: &SectionView<'_>) -> Self {
        Self::scan_blocks(view.blocks, |direction, p| {
            view.borders[direction as usize].map_or_else(
                || view.missing.sample().block,
                |border| border[direction.border_index(p)].block,
            )
        })
    }

    /// Same scan against caller-supplied neighbor blocks. `neighbor` is asked
    /// for the block outside `direction` at boundary position `p` only.
    pub(crate) fn scan_blocks(
        blocks: &[Block; SECTION_CELLS],
        mut neighbor: impl FnMut(Direction, [usize; 3]) -> Block,
    ) -> Self {
        let mut vis = SectionVis::default();
        let mut any_renderable = false;
        for y in 0..EDGE {
            for z in 0..EDGE {
                for x in 0..EDGE {
                    let p = [x, y, z];
                    let index = y * FACE_CELLS + z * EDGE + x;
                    let block = blocks[index];
                    if !block.renderable() {
                        continue;
                    }
                    any_renderable = true;
                    for direction in Direction::ALL {
                        let boundary = on_boundary(direction, p);
                        let next = if boundary {
                            neighbor(direction, p)
                        } else {
                            // Off the boundary on this axis, so index + STEP is
                            // always inside the array.
                            blocks[(index as isize + STEP[direction as usize]) as usize]
                        };
                        if face_can_emit(block, next, direction) {
                            if boundary {
                                vis.boundary_faces |= direction.bit();
                            } else {
                                vis.interior_faces = true;
                            }
                        }
                    }
                    // Saturated: every flag is decided and a live face already
                    // proves uniform_air is false.
                    if vis.interior_faces && vis.boundary_faces == ALL_FACES {
                        return vis;
                    }
                }
            }
        }
        vis.uniform_air = !any_renderable;
        vis
    }

    /// Whether the section can emit any quad at all. A `false` result is a
    /// proof the greedy mesher would produce an empty mesh.
    pub fn emits_any(self) -> bool {
        self.interior_faces || self.boundary_faces != 0
    }

    /// Whether emitted geometry can be seen from the half-spaces in `facing`
    /// (see `box_facing_mask`). Interior faces count as visible from every
    /// side: a hole in a facing wall can expose them.
    pub fn visible_towards(self, facing: u8) -> bool {
        self.interior_faces || self.boundary_faces & facing != 0
    }
}

/// A cell that occludes every direction and carries no fluid. Between two such
/// cells no face can exist, so a section built only of them emits nothing once
/// its sides are sealed too.
#[inline(always)]
fn seals(block: Block) -> bool {
    block.flags & OPAQUE != 0
        && block.occlusion & ALL_FACES == ALL_FACES
        && block.fluid() == NO_FLUID
}

/// Section-local position of face-plane cell `face` (0..FACE_CELLS) on the
/// `direction` boundary; the inverse of `Direction::border_index`.
#[inline(always)]
fn boundary_pos(direction: Direction, face: usize) -> [usize; 3] {
    let edge = (direction as usize & 1) * (EDGE - 1);
    match direction.axis() {
        0 => [edge, face % EDGE, face / EDGE],
        1 => [face % EDGE, edge, face / EDGE],
        _ => [face % EDGE, face / EDGE, edge],
    }
}

/// Cheap sufficient proof that a section produces zero quads, for the meshing
/// hot path. Returns true only for a uniform-air section or a section where
/// every cell is a full-occlusion cube *and* every side is sealed by the
/// neighbor plane. `sealed_hint` is a caller guarantee that every cell is a
/// full-occlusion cube already, which skips the interior flag pass entirely.
/// `side_sealed` reports whether all 256 positions outside a side occlude
/// toward the section; `face_block` fetches one of those blocks and is only
/// asked on sides that are not sealed. Open sections exit on the first live
/// boundary face; skipped sections cost one flag pass plus the six
/// `side_sealed` checks, or just the side checks with `sealed_hint`.
pub(crate) fn emits_nothing(
    blocks: &[Block; SECTION_CELLS],
    sealed_hint: bool,
    mut side_sealed: impl FnMut(Direction) -> bool,
    mut face_block: impl FnMut(Direction, [usize; 3]) -> Block,
) -> bool {
    let mut all_sides_sealed = true;
    for direction in Direction::ALL {
        if side_sealed(direction) {
            continue;
        }
        all_sides_sealed = false;
        for face in 0..FACE_CELLS {
            let p = boundary_pos(direction, face);
            let cell = blocks[section_index(p[0], p[1], p[2])];
            if face_can_emit(cell, face_block(direction, p), direction) {
                return false;
            }
        }
    }
    if sealed_hint {
        return true;
    }
    let mut any_renderable = false;
    let mut all_sealing = true;
    for &block in blocks.iter() {
        any_renderable |= block.renderable();
        all_sealing &= seals(block);
        if any_renderable && !all_sealing {
            return false;
        }
    }
    !any_renderable || (all_sealing && all_sides_sealed)
}

/// Bits for the sides of a section-sized box that face `camera`. `min` is the
/// lowest box corner; the box spans `min ..= min + EDGE` per axis. A camera
/// inside or on a side plane faces no boundary plane along that axis, so an
/// enclosed camera gets mask 0 and relies on `interior_faces`.
pub fn box_facing_mask(min: [f32; 3], camera: [f32; 3]) -> u8 {
    const SIDES: [(Direction, Direction); 3] = [
        (Direction::West, Direction::East),
        (Direction::Down, Direction::Up),
        (Direction::North, Direction::South),
    ];
    let mut mask = 0;
    for (axis, (low, high)) in SIDES.into_iter().enumerate() {
        if camera[axis] < min[axis] {
            mask |= low.bit();
        } else if camera[axis] > min[axis] + EDGE as f32 {
            mask |= high.bit();
        }
    }
    mask
}

/// Conservative six-plane test: `false` only when the section box lies fully
/// outside at least one plane. Each plane is `[nx, ny, nz, d]` with the inside
/// half-space `dot(n, p) + d >= 0` and is tested against the corner furthest
/// along its normal, so an unculled box may still touch the frustum edge.
pub fn box_in_frustum(min: [f32; 3], planes: &[[f32; 4]]) -> bool {
    planes.iter().all(|&[nx, ny, nz, d]| {
        let p = [
            if nx >= 0.0 {
                min[0] + EDGE as f32
            } else {
                min[0]
            },
            if ny >= 0.0 {
                min[1] + EDGE as f32
            } else {
                min[1]
            },
            if nz >= 0.0 {
                min[2] + EDGE as f32
            } else {
                min[2]
            },
        ];
        nx * p[0] + ny * p[1] + nz * p[2] + d >= 0.0
    })
}
