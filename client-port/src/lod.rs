// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Coarse LOD summaries for chunk columns past the active
//! mesh distance: per-column top height, block and tint at a 4x4 or 16x16 grid.

use crate::section::{Block, SectionView, EDGE, FACE_CELLS, FLUID_TINTED, TINTED};

/// Cells per side for the two supported summaries: one cell per 4x4 block
/// footprint, or one cell per block column.
pub const LOD_COARSE: usize = 4;
pub const LOD_FINE: usize = EDGE;

/// One coarse cell: the highest non-air block found in its column footprint.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LodCell {
    /// Global palette state id at `top_y` (0 when the footprint is empty).
    pub state_id: u32,
    /// `Block::tint` for `TINTED` tops, `Block::fluid_tint` for `FLUID_TINTED`
    /// tops, else 0.
    pub tint: u32,
    /// Height of the top non-air block in column-local units
    /// (`section_index * 16 + y`); -1 when the footprint holds nothing.
    pub top_y: i16,
    /// Packed light at `top_y` (block nibble | sky nibble << 4).
    pub light: u8,
    reserved: [u8; 5],
}

impl LodCell {
    pub const EMPTY: Self = Self {
        state_id: 0,
        tint: 0,
        top_y: -1,
        light: 0,
        reserved: [0; 5],
    };

    pub fn is_empty(self) -> bool {
        self.top_y < 0
    }
}

#[inline(always)]
fn lod_tint(block: Block) -> u32 {
    if block.flags & TINTED != 0 {
        block.tint
    } else if block.flags & FLUID_TINTED != 0 {
        block.fluid_tint
    } else {
        0
    }
}

/// Compact LOD summary over a whole chunk column (`views` bottom to top).
/// `edge` is the cells-per-side granularity: `LOD_COARSE` (4) or `LOD_FINE`
/// (16). Writes `edge * edge` cells into `out` in x-fastest, z-major order
/// matching `section_index`'s X/Z layout. Performs no heap allocation and
/// stops scanning sections once every column has found its top.
pub fn summarize_column(views: &[SectionView<'_>], edge: usize, out: &mut [LodCell]) {
    assert!(matches!(edge, LOD_COARSE | LOD_FINE));
    assert!(out.len() >= edge * edge);
    let mut tops = [LodCell::EMPTY; FACE_CELLS];
    let mut found = [0u64; FACE_CELLS / 64];
    for (si, view) in views.iter().enumerate().rev() {
        if found.iter().all(|&word| word == u64::MAX) {
            break;
        }
        for z in 0..EDGE {
            for x in 0..EDGE {
                let col = z * EDGE + x;
                if found[col / 64] >> (col % 64) & 1 != 0 {
                    continue;
                }
                let mut cell = LodCell::EMPTY;
                for y in (0..EDGE).rev() {
                    let index = y * FACE_CELLS + col;
                    let block = view.blocks[index];
                    if block.renderable() {
                        cell = LodCell {
                            state_id: block.state_id,
                            tint: lod_tint(block),
                            top_y: (si * EDGE + y).min(i16::MAX as usize) as i16,
                            light: view.sample_local(x, y, z).light.0,
                            reserved: [0; 5],
                        };
                        break;
                    }
                }
                tops[col] = cell;
                // Only a found top marks the column done; empty columns keep
                // scanning deeper sections.
                if !cell.is_empty() {
                    found[col / 64] |= 1 << (col % 64);
                }
            }
        }
    }
    let foot = EDGE / edge;
    for cz in 0..edge {
        for cx in 0..edge {
            let mut best = LodCell::EMPTY;
            for bz in 0..foot {
                for bx in 0..foot {
                    let cell = tops[(cz * foot + bz) * EDGE + cx * foot + bx];
                    if cell.top_y > best.top_y {
                        best = cell;
                    }
                }
            }
            out[cz * edge + cx] = best;
        }
    }
}

/// Owning buffer for `summarize_column`; `update` reuses its allocation, so
/// regenerating the summary of a chunk column is heap-churn free.
pub struct LodSummary {
    cells: Vec<LodCell>,
    edge: usize,
    height: usize,
}

impl LodSummary {
    pub fn new(edge: usize) -> Self {
        assert!(matches!(edge, LOD_COARSE | LOD_FINE));
        Self {
            cells: vec![LodCell::EMPTY; edge * edge],
            edge,
            height: 0,
        }
    }

    pub fn edge(&self) -> usize {
        self.edge
    }

    /// Column height in blocks covered by the last `update`.
    pub fn height(&self) -> usize {
        self.height
    }

    pub fn cells(&self) -> &[LodCell] {
        &self.cells
    }

    /// Regenerate the summary in place. No allocation while `edge` is
    /// unchanged and `top_y` saturates at `i16::MAX` for very tall columns.
    pub fn update(&mut self, views: &[SectionView<'_>]) -> &[LodCell] {
        self.height = views.len() * EDGE;
        summarize_column(views, self.edge, &mut self.cells);
        &self.cells
    }
}
