// SPDX-License-Identifier: LGPL-3.0-or-later
//! Deterministic voxel-shape and entity selection rays for protocol 767 clients.

use crate::physics::Aabb;

pub const SURVIVAL_BLOCK_REACH: f64 = 4.5;
pub const CREATIVE_BLOCK_REACH: f64 = 5.0;
pub const SURVIVAL_ENTITY_REACH: f64 = 3.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FluidClipMode {
    None,
    SourceOnly,
    Any,
}

/// Protocol 767 face order for `use_item_on` and `player_action`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum BlockHitFace {
    Down = 0,
    Up = 1,
    North = 2,
    South = 3,
    West = 4,
    East = 5,
}

/// Per-voxel selection shapes. `boxes` use local block coordinates (0..1).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RayBlockShapes {
    pub boxes: Vec<Aabb>,
    pub is_fluid: bool,
    pub is_fluid_source: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RayHit {
    pub block_pos: [i32; 3],
    pub face: BlockHitFace,
    pub hit_pos: [f64; 3],
    pub cursor_in_block: [f32; 3],
    pub distance: f64,
    pub inside: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EntityRayHit {
    pub entity_id: i32,
    pub hit_pos: [f64; 3],
    pub distance: f64,
}

#[derive(Clone, Copy)]
struct Intersection {
    distance: f64,
    face: BlockHitFace,
    inside: bool,
}

fn ray_direction(origin: [f64; 3], direction: [f64; 3], max_distance: f64) -> Option<[f64; 3]> {
    if !origin.into_iter().all(f64::is_finite)
        || !direction.into_iter().all(f64::is_finite)
        || !max_distance.is_finite()
        || max_distance <= 0.0
    {
        return None;
    }

    let scale = direction.into_iter().map(f64::abs).fold(0.0, f64::max);
    if scale == 0.0 {
        return None;
    }
    let scaled = direction.map(|axis| axis / scale);
    let length = scaled[0].hypot(scaled[1]).hypot(scaled[2]);
    Some(scaled.map(|axis| axis / length))
}

fn valid_bounds(bounds: Aabb) -> bool {
    (0..3).all(|i| {
        bounds.min[i].is_finite() && bounds.max[i].is_finite() && bounds.min[i] < bounds.max[i]
    })
}

fn opposite_ray_face(direction: [f64; 3]) -> BlockHitFace {
    let [x, y, z] = direction.map(f64::abs);
    if x >= y && x >= z {
        if direction[0] > 0.0 {
            BlockHitFace::West
        } else {
            BlockHitFace::East
        }
    } else if y >= z {
        if direction[1] > 0.0 {
            BlockHitFace::Down
        } else {
            BlockHitFace::Up
        }
    } else if direction[2] > 0.0 {
        BlockHitFace::North
    } else {
        BlockHitFace::South
    }
}

/// Intersect an infinite ray against a world-space AABB using per-axis slabs.
fn intersect_box(
    origin: [f64; 3],
    direction: [f64; 3],
    bounds: Aabb,
    max_distance: f64,
) -> Option<Intersection> {
    if !valid_bounds(bounds) {
        return None;
    }

    let mut enter = f64::NEG_INFINITY;
    let mut exit = f64::INFINITY;
    let mut face = opposite_ray_face(direction);
    for axis in 0..3 {
        let speed = direction[axis];
        if speed == 0.0 {
            if origin[axis] < bounds.min[axis] || origin[axis] > bounds.max[axis] {
                return None;
            }
            continue;
        }

        let (near, far, entry_face) = if speed > 0.0 {
            (
                (bounds.min[axis] - origin[axis]) / speed,
                (bounds.max[axis] - origin[axis]) / speed,
                [BlockHitFace::West, BlockHitFace::Down, BlockHitFace::North][axis],
            )
        } else {
            (
                (bounds.max[axis] - origin[axis]) / speed,
                (bounds.min[axis] - origin[axis]) / speed,
                [BlockHitFace::East, BlockHitFace::Up, BlockHitFace::South][axis],
            )
        };
        if near > enter {
            enter = near;
            face = entry_face;
        }
        exit = exit.min(far);
        if enter > exit {
            return None;
        }
    }

    if exit < 0.0 || enter > max_distance || (enter < 0.0 && exit <= 0.0) {
        return None;
    }

    let inside =
        (0..3).all(|axis| origin[axis] > bounds.min[axis] && origin[axis] < bounds.max[axis]);
    Some(Intersection {
        distance: enter.max(0.0),
        face: if enter < 0.0 {
            opposite_ray_face(direction)
        } else {
            face
        },
        inside,
    })
}

fn hit_position(origin: [f64; 3], direction: [f64; 3], distance: f64) -> [f64; 3] {
    std::array::from_fn(|axis| direction[axis].mul_add(distance, origin[axis]))
}

/// Trace local block selection boxes and optionally water/lava surfaces.
///
/// The sampler is called in voxel-traversal order, including the origin cell.
/// A cell's shape boxes are translated from local to world coordinates here.
pub fn raycast_blocks(
    origin: [f64; 3],
    direction: [f64; 3],
    max_distance: f64,
    fluid_mode: FluidClipMode,
    mut sample: impl FnMut(i32, i32, i32) -> RayBlockShapes,
) -> Option<RayHit> {
    let direction = ray_direction(origin, direction, max_distance)?;
    let cell = origin.map(|coordinate| coordinate.floor());
    if !cell
        .iter()
        .all(|&coordinate| coordinate >= f64::from(i32::MIN) && coordinate <= f64::from(i32::MAX))
    {
        return None;
    }
    let mut voxel = cell.map(|coordinate| coordinate as i32);
    let step = direction.map(|axis| i32::from(axis > 0.0) - i32::from(axis < 0.0));
    let delta = direction.map(|axis| {
        if axis == 0.0 {
            f64::INFINITY
        } else {
            1.0 / axis.abs()
        }
    });
    let mut next: [f64; 3] = std::array::from_fn(|axis| match step[axis] {
        1 => (cell[axis] + 1.0 - origin[axis]) * delta[axis],
        -1 => (origin[axis] - cell[axis]) * delta[axis],
        _ => f64::INFINITY,
    });
    // Three axis crossings per unit of travel (plus boundary/corner headroom).
    let max_steps = (max_distance.ceil() as usize)
        .saturating_add(4)
        .saturating_mul(4);
    let mut best: Option<RayHit> = None;

    for _ in 0..max_steps {
        let shapes = sample(voxel[0], voxel[1], voxel[2]);
        let mut candidates = shapes.boxes;
        if matches!(fluid_mode, FluidClipMode::Any) && shapes.is_fluid
            || matches!(fluid_mode, FluidClipMode::SourceOnly) && shapes.is_fluid_source
        {
            candidates.push(Aabb {
                min: [0.0, 0.0, 0.0],
                max: [1.0, 0.888_888_9, 1.0],
            });
        }

        for shape in candidates {
            if !valid_bounds(shape) {
                continue;
            }
            let world = Aabb {
                min: std::array::from_fn(|axis| shape.min[axis] + f64::from(voxel[axis])),
                max: std::array::from_fn(|axis| shape.max[axis] + f64::from(voxel[axis])),
            };
            if let Some(hit) = intersect_box(origin, direction, world, max_distance) {
                if best.is_some_and(|current| current.distance <= hit.distance) {
                    continue;
                }
                let hit_pos = hit_position(origin, direction, hit.distance);
                best = Some(RayHit {
                    block_pos: voxel,
                    face: hit.face,
                    hit_pos,
                    cursor_in_block: std::array::from_fn(|axis| {
                        (hit_pos[axis] - f64::from(voxel[axis])).clamp(0.0, 1.0) as f32
                    }),
                    distance: hit.distance,
                    inside: hit.inside,
                });
            }
        }

        // Vanilla's strict axis comparisons resolve simultaneous crossings Z, Y, X.
        let axis = if next[0] < next[1] && next[0] < next[2] {
            0
        } else if next[1] < next[2] {
            1
        } else {
            2
        };
        if next[axis] > max_distance || best.is_some_and(|hit| hit.distance < next[axis]) {
            return best;
        }
        if !next[axis].is_finite() || step[axis] == 0 {
            return best;
        }
        let Some(next_voxel) = voxel[axis].checked_add(step[axis]) else {
            return best;
        };
        voxel[axis] = next_voxel;
        next[axis] += delta[axis];
    }
    best
}

/// Find the nearest entity intersected strictly before the supplied reach.
/// Equal-distance candidates are resolved by the lowest entity ID.
pub fn raycast_entities(
    origin: [f64; 3],
    direction: [f64; 3],
    max_distance: f64,
    entities: &[(i32, Aabb)],
) -> Option<EntityRayHit> {
    let direction = ray_direction(origin, direction, max_distance)?;
    let mut best: Option<EntityRayHit> = None;
    for &(entity_id, bounds) in entities {
        let Some(hit) = intersect_box(origin, direction, bounds, max_distance) else {
            continue;
        };
        if hit.distance >= max_distance
            || best.is_some_and(|current| {
                (hit.distance, entity_id) >= (current.distance, current.entity_id)
            })
        {
            continue;
        }
        best = Some(EntityRayHit {
            entity_id,
            hit_pos: hit_position(origin, direction, hit.distance),
            distance: hit.distance,
        });
    }
    best
}
