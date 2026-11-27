// SPDX-License-Identifier: LGPL-3.0-or-later
//! Material-independent templates for common axis-aligned block shapes.
//!
//! A registry owns each structural state once. Texture sprites, biome tint,
//! and emissive properties are supplied at instantiation time so geometry can
//! be reused across block families.

use std::collections::BTreeMap;

use crate::dir_basis::{CubeOrientation, DIR_BASIS};

const FACE_ORDER: [usize; 6] = [0, 1, 2, 3, 4, 5];
const CANONICAL_TEMPLATE_COUNT: usize = 1 + 2 + 40 + 16 + 162 + 16 + 32;
const VANILLA_STATE_MODEL_COUNT: usize = 1 + 2 + 40 + 16 + 324 + 16 + 64;

/// One palette entry for a geometry material slot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MaterialSlotBinding {
    pub texture_sprite_id: u32,
    pub biome_tint_index: Option<u8>,
    pub emissive: bool,
}

/// Texture and tint palette passed independently of the shape state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MaterialBinding {
    pub slots: Vec<MaterialSlotBinding>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StairShape {
    Straight,
    InnerLeft,
    InnerRight,
    OuterLeft,
    OuterRight,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WallSide {
    None,
    Low,
    Tall,
}

/// Structural block state. Horizontal direction bits use N, E, S, W in bits
/// 0 through 3; `facing_nesw` uses the same indices.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ShapeArchetype {
    FullCube,
    Slab {
        top: bool,
    },
    Stair {
        shape: StairShape,
        half_top: bool,
        facing_nesw: u8,
    },
    Fence {
        connections_nesw: u8,
    },
    Wall {
        up_post: bool,
        sides_nesw: [WallSide; 4],
    },
    Trapdoor {
        open: bool,
        half_top: bool,
        facing_nesw: u8,
    },
    Door {
        open: bool,
        hinge_right: bool,
        upper: bool,
        facing_nesw: u8,
    },
}

/// Axis-aligned rectangle on one outward-facing plane. UVs are in 0..16 units.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TemplateQuad {
    pub min: [f32; 3],
    pub max: [f32; 3],
    pub face: usize,
    pub material_slot: u8,
    pub uv: [f32; 4],
    pub cull_face: Option<usize>,
}

/// A canonical geometry template, independent of all texture data.
#[derive(Clone, Debug, PartialEq)]
pub struct ShapeTemplate {
    pub id: u64,
    pub archetype: ShapeArchetype,
    pub quads: Vec<TemplateQuad>,
}

/// One template quad with its resolved material slot binding.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InstantiatedQuad {
    pub quad: TemplateQuad,
    pub binding: MaterialSlotBinding,
}

/// Estimated geometry model counts with and without material-family sharing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TemplateSavingsReport {
    pub canonical_template_count: usize,
    pub naive_materialized_state_model_count: usize,
    pub unique_materials: usize,
    pub reduction_percent: u8,
}

/// Lazily builds and interns structural block-state geometry.
#[derive(Clone, Debug, Default)]
pub struct ShapeTemplateRegistry {
    templates: BTreeMap<ShapeArchetype, ShapeTemplate>,
    next_id: u64,
}

impl ShapeTemplateRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn template(&mut self, archetype: ShapeArchetype) -> &ShapeTemplate {
        if !self.templates.contains_key(&archetype) {
            let id = self.next_id;
            self.next_id += 1;
            let quads = build_quads(archetype);
            self.templates.insert(
                archetype,
                ShapeTemplate {
                    id,
                    archetype,
                    quads,
                },
            );
        }
        self.templates.get(&archetype).expect("inserted template")
    }

    pub fn template_id(&mut self, archetype: ShapeArchetype) -> u64 {
        self.template(archetype).id
    }

    pub fn instantiate(
        &mut self,
        archetype: ShapeArchetype,
        material_binding: &MaterialBinding,
    ) -> Vec<InstantiatedQuad> {
        self.template(archetype)
            .quads
            .iter()
            .map(|quad| InstantiatedQuad {
                quad: *quad,
                binding: material_binding
                    .slots
                    .get(quad.material_slot as usize)
                    .copied()
                    .unwrap_or_default(),
            })
            .collect()
    }

    pub fn templates(&self) -> impl Iterator<Item = &ShapeTemplate> {
        self.templates.values()
    }

    pub fn memory_savings_report(&self, unique_materials: usize) -> TemplateSavingsReport {
        let naive = VANILLA_STATE_MODEL_COUNT.saturating_mul(unique_materials);
        let reduction = if naive == 0 {
            0
        } else {
            naive
                .saturating_sub(CANONICAL_TEMPLATE_COUNT)
                .saturating_mul(100)
                .checked_div(naive)
                .unwrap_or(0)
        };
        TemplateSavingsReport {
            canonical_template_count: CANONICAL_TEMPLATE_COUNT,
            naive_materialized_state_model_count: naive,
            unique_materials,
            reduction_percent: reduction.min(100) as u8,
        }
    }
}

#[derive(Clone, Copy)]
struct Box3 {
    min: [f32; 3],
    max: [f32; 3],
}

fn box3(x0: f32, y0: f32, z0: f32, x1: f32, y1: f32, z1: f32) -> Box3 {
    Box3 {
        min: [x0, y0, z0],
        max: [x1, y1, z1],
    }
}

fn yaw(turns: u8) -> CubeOrientation {
    let cw = CubeOrientation {
        matrix: [[0, 0, -1], [0, 1, 0], [1, 0, 0]],
    };
    (0..turns % 4).fold(CubeOrientation::IDENTITY, |rotation, _| {
        cw.compose(rotation)
    })
}

fn rotate_box(b: Box3, rotation: CubeOrientation) -> Box3 {
    let mut min = [f32::INFINITY; 3];
    let mut max = [f32::NEG_INFINITY; 3];
    for x in [b.min[0], b.max[0]] {
        for y in [b.min[1], b.max[1]] {
            for z in [b.min[2], b.max[2]] {
                let centered = [x - 0.5, y - 0.5, z - 0.5];
                for axis in 0..3 {
                    let value = 0.5
                        + (rotation.matrix[axis][0] as f32 * centered[0]
                            + rotation.matrix[axis][1] as f32 * centered[1]
                            + rotation.matrix[axis][2] as f32 * centered[2]);
                    min[axis] = min[axis].min(value);
                    max[axis] = max[axis].max(value);
                }
            }
        }
    }
    Box3 { min, max }
}

fn mask_has(mask: u8, direction: usize) -> bool {
    mask & (1 << (direction % 4)) != 0
}

fn geometry(archetype: ShapeArchetype) -> Vec<Box3> {
    let mut boxes = Vec::new();
    let mut push = |b: Box3, rotation: CubeOrientation| boxes.push(rotate_box(b, rotation));
    match archetype {
        ShapeArchetype::FullCube => push(
            box3(0.0, 0.0, 0.0, 1.0, 1.0, 1.0),
            CubeOrientation::IDENTITY,
        ),
        ShapeArchetype::Slab { top } => {
            let rotation = if top {
                CubeOrientation {
                    matrix: [[1, 0, 0], [0, -1, 0], [0, 0, -1]],
                }
            } else {
                CubeOrientation::IDENTITY
            };
            push(box3(0.0, 0.0, 0.0, 1.0, 0.5, 1.0), rotation);
        }
        ShapeArchetype::Stair {
            shape,
            half_top,
            facing_nesw,
        } => {
            let flip = if half_top {
                CubeOrientation {
                    matrix: [[1, 0, 0], [0, -1, 0], [0, 0, -1]],
                }
            } else {
                CubeOrientation::IDENTITY
            };
            let rotation = yaw(facing_nesw).compose(flip);
            push(box3(0.0, 0.0, 0.0, 1.0, 0.5, 1.0), rotation);
            match shape {
                StairShape::Straight => push(box3(0.0, 0.5, 0.0, 1.0, 1.0, 0.5), rotation),
                StairShape::InnerLeft => {
                    push(box3(0.0, 0.5, 0.0, 0.5, 1.0, 0.5), rotation);
                    push(box3(0.5, 0.5, 0.0, 1.0, 1.0, 1.0), rotation);
                }
                StairShape::InnerRight => {
                    push(box3(0.5, 0.5, 0.0, 1.0, 1.0, 0.5), rotation);
                    push(box3(0.0, 0.5, 0.0, 0.5, 1.0, 1.0), rotation);
                }
                StairShape::OuterLeft => push(box3(0.0, 0.5, 0.0, 0.5, 1.0, 0.5), rotation),
                StairShape::OuterRight => push(box3(0.5, 0.5, 0.0, 1.0, 1.0, 0.5), rotation),
            }
        }
        ShapeArchetype::Fence { connections_nesw } => {
            push(
                box3(0.375, 0.0, 0.375, 0.625, 1.0, 0.625),
                CubeOrientation::IDENTITY,
            );
            for dir in 0..4 {
                if mask_has(connections_nesw, dir) {
                    let arm = box3(0.4375, 0.375, 0.0, 0.5625, 0.9375, 0.5);
                    push(arm, yaw(dir as u8));
                }
            }
        }
        ShapeArchetype::Wall {
            up_post,
            sides_nesw,
        } => {
            if up_post || sides_nesw.iter().all(|side| *side == WallSide::None) {
                push(
                    box3(0.25, 0.0, 0.25, 0.75, 1.0, 0.75),
                    CubeOrientation::IDENTITY,
                );
            }
            for (dir, side) in sides_nesw.iter().copied().enumerate() {
                if side != WallSide::None {
                    let height = if side == WallSide::Tall { 1.0 } else { 0.8125 };
                    push(box3(0.3125, 0.0, 0.0, 0.6875, height, 0.5), yaw(dir as u8));
                }
            }
        }
        ShapeArchetype::Trapdoor {
            open,
            half_top,
            facing_nesw,
        } => {
            let (b, rotation) = if open {
                (box3(0.0, 0.0, 0.0, 1.0, 1.0, 0.1875), yaw(facing_nesw))
            } else if half_top {
                (
                    box3(0.0, 0.8125, 0.0, 1.0, 1.0, 1.0),
                    CubeOrientation::IDENTITY,
                )
            } else {
                (
                    box3(0.0, 0.0, 0.0, 1.0, 0.1875, 1.0),
                    CubeOrientation::IDENTITY,
                )
            };
            push(b, rotation);
        }
        ShapeArchetype::Door {
            open,
            hinge_right,
            upper: _,
            facing_nesw,
        } => {
            let offset = if open {
                if hinge_right {
                    0.8125
                } else {
                    0.0
                }
            } else {
                0.0
            };
            let b = box3(0.0, 0.0, offset, 1.0, 1.0, offset + 0.1875);
            push(b, yaw(facing_nesw));
        }
    }
    boxes
}

fn build_quads(archetype: ShapeArchetype) -> Vec<TemplateQuad> {
    let boxes = geometry(archetype);
    let mut quads = Vec::new();
    for (box_index, b) in boxes.iter().copied().enumerate() {
        for face in FACE_ORDER {
            let basis = DIR_BASIS[face];
            let plane_axis = if basis.n[0] != 0 {
                0
            } else if basis.n[1] != 0 {
                1
            } else {
                2
            };
            let outward_max = basis.n[plane_axis] > 0;
            let plane = if outward_max {
                b.max[plane_axis]
            } else {
                b.min[plane_axis]
            };
            let cull_face = if (plane - if outward_max { 1.0 } else { 0.0 }).abs() < 1e-6 {
                Some(face)
            } else {
                None
            };
            let Some(u_axis) = basis
                .u_axis
                .iter()
                .position(|component| *component != 0)
            else {
                continue;
            };
            let Some(v_axis) = basis
                .v_axis
                .iter()
                .position(|component| *component != 0)
            else {
                continue;
            };
            let mut u_edges = vec![b.min[u_axis], b.max[u_axis]];
            let mut v_edges = vec![b.min[v_axis], b.max[v_axis]];
            for (other_index, other) in boxes.iter().copied().enumerate() {
                if other_index == box_index {
                    continue;
                }
                for edge in [other.min[u_axis], other.max[u_axis]] {
                    if edge > b.min[u_axis] && edge < b.max[u_axis] {
                        u_edges.push(edge);
                    }
                }
                for edge in [other.min[v_axis], other.max[v_axis]] {
                    if edge > b.min[v_axis] && edge < b.max[v_axis] {
                        v_edges.push(edge);
                    }
                }
            }
            u_edges.sort_by(f32::total_cmp);
            v_edges.sort_by(f32::total_cmp);
            u_edges.dedup_by(|left, right| (*left - *right).abs() < 1e-6);
            v_edges.dedup_by(|left, right| (*left - *right).abs() < 1e-6);
            for u in u_edges.windows(2) {
                for v in v_edges.windows(2) {
                    let mut min = [0.0; 3];
                    let mut max = [0.0; 3];
                    min[plane_axis] = plane;
                    max[plane_axis] = plane;
                    min[u_axis] = u[0];
                    max[u_axis] = u[1];
                    min[v_axis] = v[0];
                    max[v_axis] = v[1];
                    let mut probe = [0.0; 3];
                    probe[plane_axis] = plane + basis.n[plane_axis] as f32 * 1e-4;
                    probe[u_axis] = (u[0] + u[1]) * 0.5;
                    probe[v_axis] = (v[0] + v[1]) * 0.5;
                    let hidden = boxes.iter().enumerate().any(|(other_index, other)| {
                        other_index != box_index
                            && (0..3).all(|axis| {
                                probe[axis] > other.min[axis] + 1e-6
                                    && probe[axis] < other.max[axis] - 1e-6
                            })
                    });
                    if hidden {
                        continue;
                    }
                    let origin = basis.origin.map(f32::from);
                    let uv = [
                        (min[u_axis] - origin[u_axis]) * basis.u_axis[u_axis] as f32 * 16.0,
                        (min[v_axis] - origin[v_axis]) * basis.v_axis[v_axis] as f32 * 16.0,
                        (max[u_axis] - origin[u_axis]) * basis.u_axis[u_axis] as f32 * 16.0,
                        (max[v_axis] - origin[v_axis]) * basis.v_axis[v_axis] as f32 * 16.0,
                    ];
                    quads.push(TemplateQuad {
                        min,
                        max,
                        face,
                        material_slot: 0,
                        uv,
                        cull_face,
                    });
                }
            }
        }
    }
    quads
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn savings_report_accounts_for_material_families() {
        let report = ShapeTemplateRegistry::new().memory_savings_report(12);
        assert!(report.reduction_percent > 90);
        assert_eq!(report.naive_materialized_state_model_count, 463 * 12);
    }
}
