// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
use crate::Error;
use std::collections::BTreeSet;

// The server's lighting source is imported verbatim and extended only with
// accessors for the facts it keeps private.
#[allow(dead_code, clippy::all)]
pub(crate) mod owner {
    include!("../../src/lighting.rs");

    impl FaceOcclusion {
        pub(crate) fn from_quadrants(quadrants: [u8; 6]) -> Self {
            Self(quadrants)
        }

        pub(crate) fn quadrants(self) -> [u8; 6] {
            self.0
        }
    }

    impl IncrementalLighting {
        pub(crate) fn is_loaded(&self, chunk: Chunk) -> bool {
            self.chunks.contains_key(&chunk)
        }

        pub(crate) fn window(&self) -> (i32, i32) {
            (self.min_y, self.height)
        }
    }
}

pub use owner::{ChunkLightUpdate, LightKind, LightUpdate};

/// Bytes in one packed 16x16x16 nibble array.
pub const LIGHT_SECTION_BYTES: usize = 2048;

/// Face order is West, East, North, South, Down, Up. Each mask holds the
/// four covered half-block quadrants of that face.
pub fn face_occlusion_from_octants(octants: u8) -> [u8; 6] {
    owner::FaceOcclusion::from_octants(octants).quadrants()
}

/// Light facts for one block cell.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LightFact {
    pub opacity: u8,
    pub emission: u8,
    pub occlusion: [u8; 6],
}

impl LightFact {
    fn block(self) -> Result<owner::LightBlock, Error> {
        if self.opacity > 15 || self.emission > 15 || self.occlusion.iter().any(|&q| q > 0xf) {
            return Err(Error::InvalidArgument);
        }
        Ok(owner::LightBlock::with_occlusion(
            self.opacity,
            self.emission,
            owner::FaceOcclusion::from_quadrants(self.occlusion),
        ))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LightEdit {
    pub x: i32,
    pub y: i32,
    pub z: i32,
    pub fact: LightFact,
}

/// A loaded window of chunk columns lit by the server's incremental engine.
pub struct LightEngine {
    inner: owner::IncrementalLighting,
}

/// Chunk coordinates keep every neighbor offset within `i32` block space.
const CHUNK_LIMIT: i32 = 1 << 26;

impl LightEngine {
    pub fn new(min_y: i32, height: u32, has_skylight: bool) -> Result<Self, Error> {
        let height = i32::try_from(height).map_err(|_| Error::OutOfRange)?;
        if min_y % 16 != 0
            || height <= 0
            || height % 16 != 0
            || height / 16 + 2 > 64
            || min_y.checked_add(height).is_none()
        {
            return Err(Error::InvalidArgument);
        }
        Ok(Self {
            inner: owner::IncrementalLighting::from_chunks(min_y, height, has_skylight, []),
        })
    }

    /// Number of sections per layer, including one guard section below and above.
    pub fn section_count(&self) -> usize {
        (self.inner.window().1 / 16 + 2) as usize
    }

    pub fn cells_per_chunk(&self) -> usize {
        self.inner.window().1 as usize * 256
    }

    fn check_chunk(cx: i32, cz: i32) -> Result<(), Error> {
        if cx.abs() < CHUNK_LIMIT && cz.abs() < CHUNK_LIMIT {
            Ok(())
        } else {
            Err(Error::OutOfRange)
        }
    }

    /// Replace one complete column. Cells are ordered y, z, x from the window's bottom.
    pub fn load_chunk(&mut self, cx: i32, cz: i32, cells: &[LightFact]) -> Result<(), Error> {
        Self::check_chunk(cx, cz)?;
        if cells.len() != self.cells_per_chunk() {
            return Err(Error::OutOfRange);
        }
        let min_y = self.inner.window().0;
        let cells = cells
            .iter()
            .enumerate()
            .map(|(i, fact)| {
                let position = owner::Position::new(
                    cx * 16 + (i & 15) as i32,
                    min_y + (i >> 8) as i32,
                    cz * 16 + ((i >> 4) & 15) as i32,
                );
                Ok((position, fact.block()?))
            })
            .collect::<Result<Vec<_>, Error>>()?;
        self.inner.load_chunk(cx, cz, cells);
        Ok(())
    }

    /// Apply one or more edits atomically and report each changed neighbor section.
    pub fn apply_edits(&mut self, edits: &[LightEdit]) -> Result<LightUpdate, Error> {
        if edits.is_empty() {
            return Err(Error::InvalidArgument);
        }
        let (min_y, height) = self.inner.window();
        let mut changes = Vec::with_capacity(edits.len());
        let mut targets = BTreeSet::new();
        for edit in edits {
            let position = owner::Position::new(edit.x, edit.y, edit.z);
            let (cx, cz) = (edit.x.div_euclid(16), edit.z.div_euclid(16));
            if !(min_y..min_y + height).contains(&edit.y) || !self.inner.is_loaded((cx, cz)) {
                return Err(Error::OutOfRange);
            }
            changes.push((position, edit.fact.block()?));
            for (dx, dz) in (-1..=1).flat_map(|dx| (-1..=1).map(move |dz| (dx, dz))) {
                if self.inner.is_loaded((cx + dx, cz + dz)) {
                    targets.insert((cx + dx, cz + dz));
                }
            }
        }
        Ok(self.inner.edit_batch(changes, targets))
    }

    /// Copy a section nibble array. Index zero starts one section below the window.
    pub fn section(
        &self,
        cx: i32,
        cz: i32,
        kind: LightKind,
        index: usize,
    ) -> Result<[u8; LIGHT_SECTION_BYTES], Error> {
        if !self.inner.is_loaded((cx, cz)) || index >= self.section_count() {
            return Err(Error::OutOfRange);
        }
        Ok(self.inner.sections((cx, cz), kind)[index])
    }
}
