// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later
//! Dense, precomputed per-state light properties and tick-local edit coalescing.
//!
//! The kernel's scalar light engine seals a shared face when the *union* of
//! its four quadrants is full. A six-bit whole-face mask cannot encode that
//! union for two partial blocks (for example, opposite half slabs). The packed
//! properties are two bytes per state, with a separate four-byte-per-state
//! quadrant cache to preserve the exact scalar transmission predicate.

use std::collections::BTreeMap;

use crate::{
    light::face_occlusion_from_octants, BlockFacts, Error, LightEdit, LightFact, Registry,
};

pub type FaceMask = u8;

pub const DOWN: FaceMask = 1 << 0;
pub const UP: FaceMask = 1 << 1;
pub const NORTH: FaceMask = 1 << 2;
pub const SOUTH: FaceMask = 1 << 3;
pub const WEST: FaceMask = 1 << 4;
pub const EAST: FaceMask = 1 << 5;
pub const IS_OPAQUE_FULL_CUBE: FaceMask = 1 << 6;
pub const BLOCKS_SKYLIGHT: FaceMask = 1 << 7;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Direction {
    Down = 0,
    Up = 1,
    North = 2,
    South = 3,
    West = 4,
    East = 5,
}

impl Direction {
    pub const ALL: [Self; 6] = [
        Self::Down,
        Self::Up,
        Self::North,
        Self::South,
        Self::West,
        Self::East,
    ];

    pub const fn bit(self) -> FaceMask {
        1 << self as u8
    }

    pub const fn opposite(self) -> Self {
        Self::ALL[self as usize ^ 1]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HorizontalFacing {
    North,
    East,
    South,
    West,
}

/// Simplified shapes for constructing tables without a loaded registry.
/// `Partial` has no specified geometry and therefore covers no full quadrants.
/// Use [`PrebakedLightTable::from_registry`] for exact asset-derived shapes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockShapeKind {
    Air,
    FullOpaque,
    Transparent,
    BottomSlab,
    TopSlab,
    Stair {
        facing: HorizontalFacing,
        half_top: bool,
    },
    Leaves,
    WaterloggedOpaque,
    Partial,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockLightState {
    pub shape: BlockShapeKind,
    pub emission: u8,
    pub opacity: u8,
}

impl BlockLightState {
    pub const fn new(shape: BlockShapeKind, emission: u8, opacity: u8) -> Self {
        Self {
            shape,
            emission,
            opacity,
        }
    }
}

/// Byte 0 has the six fully occluded faces and the opaque/skylight flags.
/// Byte 1 has emission in bits 0..3 and opacity in bits 4..7.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(transparent)]
pub struct PackedLightProps(u16);

impl PackedLightProps {
    pub const AIR: Self = Self(0);

    pub const fn new(
        faces: FaceMask,
        opaque_full_cube: bool,
        blocks_skylight: bool,
        emission: u8,
        opacity: u8,
    ) -> Self {
        let flags = (faces & 0x3f)
            | if opaque_full_cube {
                IS_OPAQUE_FULL_CUBE
            } else {
                0
            }
            | if blocks_skylight { BLOCKS_SKYLIGHT } else { 0 };
        let emission = if emission > 15 { 15 } else { emission };
        let opacity = if opacity > 15 { 15 } else { opacity };
        Self((flags as u16) | (((emission | (opacity << 4)) as u16) << 8))
    }

    pub const fn as_bytes(self) -> [u8; 2] {
        self.0.to_le_bytes()
    }

    pub const fn face_mask(self) -> FaceMask {
        self.0 as u8 & 0x3f
    }

    pub const fn is_opaque_full_cube(self) -> bool {
        self.0 as u8 & IS_OPAQUE_FULL_CUBE != 0
    }

    pub const fn blocks_skylight(self) -> bool {
        self.0 as u8 & BLOCKS_SKYLIGHT != 0
    }

    pub const fn emission(self) -> u8 {
        (self.0 >> 8) as u8 & 0x0f
    }

    pub const fn opacity(self) -> u8 {
        (self.0 >> 12) as u8
    }
}

/// Six half-block quadrant masks, four bits per face in `Direction` order.
#[derive(Clone, Copy, Debug, Default)]
struct PackedQuadrants(u32);

impl PackedQuadrants {
    fn from_lighting_order(faces: [u8; 6]) -> Self {
        // LightFact's source order is W, E, N, S, D, U.
        let ordered = [faces[4], faces[5], faces[2], faces[3], faces[0], faces[1]];
        let mut packed = 0;
        for (index, face) in ordered.into_iter().enumerate() {
            packed |= u32::from(face & 0x0f) << (index * 4);
        }
        Self(packed)
    }

    const fn face(self, direction: Direction) -> u8 {
        ((self.0 >> (direction as u8 * 4)) & 0x0f) as u8
    }

    fn full_face_mask(self) -> FaceMask {
        Direction::ALL.into_iter().fold(0, |mask, direction| {
            mask | if self.face(direction) == 15 {
                direction.bit()
            } else {
                0
            }
        })
    }
}

/// Immutable state-ID-indexed lookup for directional light transmission.
#[derive(Clone, Debug, Default)]
pub struct PrebakedLightTable {
    states: Vec<PackedLightProps>,
    quadrants: Vec<PackedQuadrants>,
}

impl PrebakedLightTable {
    /// Precompute from the same per-state face quadrants used by `LightEngine`.
    /// Call once immediately after opening a `Registry` for its version pack.
    pub fn from_registry(registry: &Registry) -> Self {
        let mut table = Self::with_capacity(registry.state_count());
        for state in 0..registry.state_count() {
            // All IDs in 0..state_count() are present in a validated registry.
            let facts = registry.facts(state as u32).expect("registry state exists");
            table.push(
                facts.face_occlusion,
                facts.luminance,
                facts.opacity,
                facts.is_solid_render != 0,
            );
        }
        table
    }

    /// Build a dense table in state-ID order from simple shape descriptions.
    pub fn from_shapes(states: &[BlockLightState]) -> Self {
        let mut table = Self::with_capacity(states.len());
        for state in states {
            let octants = match state.shape {
                BlockShapeKind::Air
                | BlockShapeKind::Transparent
                | BlockShapeKind::Leaves
                | BlockShapeKind::Partial => 0,
                BlockShapeKind::FullOpaque | BlockShapeKind::WaterloggedOpaque => 0xff,
                BlockShapeKind::BottomSlab => 0x0f,
                BlockShapeKind::TopSlab => 0xf0,
                BlockShapeKind::Stair { facing, half_top } => {
                    let back = match facing {
                        HorizontalFacing::North => 0b0011,
                        HorizontalFacing::East => 0b1010,
                        HorizontalFacing::South => 0b1100,
                        HorizontalFacing::West => 0b0101,
                    };
                    if half_top {
                        0xf0 | back
                    } else {
                        0x0f | (back << 4)
                    }
                }
            };
            let is_full = matches!(
                state.shape,
                BlockShapeKind::FullOpaque | BlockShapeKind::WaterloggedOpaque
            );
            table.push(
                face_occlusion_from_octants(octants),
                state.emission,
                state.opacity,
                is_full,
            );
        }
        table
    }

    fn with_capacity(count: usize) -> Self {
        Self {
            states: Vec::with_capacity(count),
            quadrants: Vec::with_capacity(count),
        }
    }

    fn push(&mut self, faces: [u8; 6], emission: u8, opacity: u8, is_full: bool) {
        let quadrants = PackedQuadrants::from_lighting_order(faces);
        let face_mask = quadrants.full_face_mask();
        self.states.push(PackedLightProps::new(
            face_mask,
            is_full,
            opacity > 0 || face_mask != 0,
            emission,
            opacity,
        ));
        self.quadrants.push(quadrants);
    }

    pub fn len(&self) -> usize {
        self.states.len()
    }

    pub fn is_empty(&self) -> bool {
        self.states.is_empty()
    }

    /// Out-of-bounds IDs are transparent air, including the quadrant cache.
    pub fn get(&self, state_id: u32) -> PackedLightProps {
        self.states
            .get(state_id as usize)
            .copied()
            .unwrap_or(PackedLightProps::AIR)
    }

    /// The scalar engine's exact seal predicate, including complementary
    /// partial faces. Two quadrant-cache reads and two face-mask extracts
    /// suffice, with no shape allocation or shape-object traversal.
    #[inline]
    pub fn light_passes_between(&self, from_state: u32, to_state: u32, dir: Direction) -> bool {
        let from_quads = self
            .quadrants
            .get(from_state as usize)
            .copied()
            .unwrap_or_default();
        let to_quads = self
            .quadrants
            .get(to_state as usize)
            .copied()
            .unwrap_or_default();
        (from_quads.face(dir) | to_quads.face(dir.opposite())) != 0x0f
    }
}

/// A final block-state write to one world-space block position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueuedLightEdit {
    pub x: i32,
    pub y: i32,
    pub z: i32,
    pub state_id: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChunkLightBatch {
    pub chunk: (i32, i32),
    pub edits: Vec<QueuedLightEdit>,
}

impl ChunkLightBatch {
    /// Adapt the coalesced final states to the existing incremental light API.
    pub fn to_light_edits(&self, registry: &Registry) -> Result<Vec<LightEdit>, Error> {
        self.edits
            .iter()
            .map(|edit| {
                let BlockFacts {
                    opacity,
                    luminance,
                    face_occlusion,
                    ..
                } = registry.facts(edit.state_id)?;
                Ok(LightEdit {
                    x: edit.x,
                    y: edit.y,
                    z: edit.z,
                    fact: LightFact {
                        opacity,
                        emission: luminance,
                        occlusion: face_occlusion,
                    },
                })
            })
            .collect()
    }
}

/// Tick-local, last-write-wins aggregation by chunk and block position.
/// `drain` emits each chunk once, with deterministic chunk/position ordering.
#[derive(Clone, Debug, Default)]
pub struct CoalescedLightQueue {
    chunks: BTreeMap<(i32, i32), BTreeMap<(i32, i32, i32), u32>>,
    unique_positions: usize,
}

impl CoalescedLightQueue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, edit: QueuedLightEdit) {
        let chunk = (edit.x.div_euclid(16), edit.z.div_euclid(16));
        if self
            .chunks
            .entry(chunk)
            .or_default()
            .insert((edit.x, edit.y, edit.z), edit.state_id)
            .is_none()
        {
            self.unique_positions += 1;
        }
    }

    pub fn len(&self) -> usize {
        self.unique_positions
    }

    pub fn is_empty(&self) -> bool {
        self.unique_positions == 0
    }

    pub fn chunk_count(&self) -> usize {
        self.chunks.len()
    }

    pub fn drain(&mut self) -> Vec<ChunkLightBatch> {
        self.unique_positions = 0;
        std::mem::take(&mut self.chunks)
            .into_iter()
            .map(|(chunk, edits)| ChunkLightBatch {
                chunk,
                edits: edits
                    .into_iter()
                    .map(|((x, y, z), state_id)| QueuedLightEdit { x, y, z, state_id })
                    .collect(),
            })
            .collect()
    }
}
