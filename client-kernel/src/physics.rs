// SPDX-License-Identifier: LGPL-3.0-or-later
use crate::Error;
use megu_core::entity_fluid::{
    fluid_push_scale, update_fluid_height_and_push_with, EntityFluidKind, FluidAabb,
    FluidCell as CoreFluidCell,
};
use std::cell::Cell;

#[allow(clippy::all, dead_code)]
mod owner {
    include!(concat!(env!("OUT_DIR"), "/physics.rs"));

    impl From<super::Aabb> for EntityAabb {
        fn from(aabb: super::Aabb) -> Self {
            let [min_x, min_y, min_z] = aabb.min;
            let [max_x, max_y, max_z] = aabb.max;
            Self {
                min_x,
                min_y,
                min_z,
                max_x,
                max_y,
                max_z,
            }
        }
    }

    pub(super) fn collide(
        aabb: super::Aabb,
        delta: [f64; 3],
        on_ground: bool,
        max_up_step: f32,
        shapes: &[super::Aabb],
    ) -> super::Collision {
        let result = entity_collide(aabb.into(), delta, on_ground, max_up_step, &[], |sweep| {
            let sweep = (sweep.min(), sweep.max());
            shapes
                .iter()
                .map(|shape| (shape.min, shape.max))
                .filter(|&shape| aabb_overlaps(sweep, shape))
                .collect()
        });
        super::Collision {
            delta: result.delta,
            horizontal_collision: result.horizontal_collision,
            vertical_collision: result.vertical_collision,
            vertical_collision_below: result.vertical_collision_below,
            stepped_up: result.stepped_up,
        }
    }
}

impl From<FluidKind> for EntityFluidKind {
    fn from(kind: FluidKind) -> Self {
        match kind {
            FluidKind::Water => Self::Water,
            FluidKind::Lava => Self::Lava,
        }
    }
}

/// The section/registry owner supplies these facts at the snapshot boundary.
impl From<FluidCell> for CoreFluidCell {
    fn from(cell: FluidCell) -> Self {
        Self {
            kind: cell.kind.map(EntityFluidKind::from),
            amount: cell.amount,
            falling: cell.falling,
            blocks_motion: cell.blocks_motion,
            is_ice: cell.is_ice,
            sturdy_faces: cell.sturdy_faces,
        }
    }
}

impl From<Aabb> for FluidAabb {
    fn from(aabb: Aabb) -> Self {
        let [min_x, min_y, min_z] = aabb.min;
        let [max_x, max_y, max_z] = aabb.max;
        Self {
            min_x,
            min_y,
            min_z,
            max_x,
            max_y,
            max_z,
        }
    }
}

/// World-space bounds in XYZ order.
#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(C)]
pub struct Aabb {
    pub min: [f64; 3],
    pub max: [f64; 3],
}

impl Aabb {
    fn valid(self) -> bool {
        (0..3).all(|i| {
            self.min[i].is_finite() && self.max[i].is_finite() && self.min[i] < self.max[i]
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Collision {
    pub delta: [f64; 3],
    pub horizontal_collision: bool,
    pub vertical_collision: bool,
    pub vertical_collision_below: bool,
    pub stepped_up: bool,
}

/// Resolve a sweep against the ordered world-space shape table. A positive step height
/// permits a step-up on horizontal collision while grounded or clipping downward.
pub fn collide(
    aabb: Aabb,
    delta: [f64; 3],
    on_ground: bool,
    max_up_step: f32,
    shapes: &[Aabb],
) -> Result<Collision, Error> {
    if !aabb.valid()
        || !max_up_step.is_finite()
        || max_up_step < 0.0
        || !delta.into_iter().all(f64::is_finite)
        || !shapes.iter().all(|shape| shape.valid())
    {
        return Err(Error::InvalidArgument);
    }
    // The server uses squared displacement to select and compare sweeps.
    let [x, y, z] = delta;
    if !(x * x + y * y + z * z).is_finite() {
        return Err(Error::OutOfRange);
    }
    let result = owner::collide(aabb, delta, on_ground, max_up_step, shapes);
    if !result.delta.into_iter().all(f64::is_finite) {
        return Err(Error::OutOfRange);
    }
    Ok(result)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FluidKind {
    Water,
    Lava,
}

/// Per-cell block/registry facts used by the server's flow and immersion calculations.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FluidCell {
    pub kind: Option<FluidKind>,
    pub amount: u8,
    pub falling: bool,
    pub blocks_motion: bool,
    pub is_ice: bool,
    pub sturdy_faces: u8,
}

impl FluidCell {
    fn valid(self) -> bool {
        let valid_fluid = if self.kind.is_some() {
            (1..=8).contains(&self.amount)
        } else {
            self.amount == 0 && !self.falling
        };
        valid_fluid && self.sturdy_faces <= 0x3f
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FluidQuery {
    pub kind: FluidKind,
    pub ultra_warm: bool,
    pub pushed_by_fluid: bool,
    pub is_player: bool,
    pub velocity: [f64; 3],
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FluidImpulse {
    pub in_fluid: bool,
    pub max_height: f64,
    pub push: [f64; 3],
}

/// Sample geometry and fluid facts independently of any world or registry instance.
pub fn fluid_impulse(
    aabb: Aabb,
    query: FluidQuery,
    mut lookup: impl FnMut(i32, i32, i32) -> Result<FluidCell, Error>,
) -> Result<FluidImpulse, Error> {
    if !aabb.valid() || !query.velocity.into_iter().all(f64::is_finite) {
        return Err(Error::InvalidArgument);
    }
    // Leave one cell of headroom for neighbor queries and bound the server's i32 counter.
    let mut volume = 1u64;
    for i in 0..3 {
        let lo = aabb.min[i].floor();
        let hi = aabb.max[i].ceil();
        if lo <= f64::from(i32::MIN) || hi >= f64::from(i32::MAX) {
            return Err(Error::OutOfRange);
        }
        volume = volume.saturating_mul((hi - lo) as u64);
    }
    if volume > i32::MAX as u64 {
        return Err(Error::OutOfRange);
    }

    let error = Cell::new(None);
    let lookup = std::cell::RefCell::new(&mut lookup);
    let kind = EntityFluidKind::from(query.kind);
    let result = update_fluid_height_and_push_with(
        FluidAabb::from(aabb),
        kind,
        fluid_push_scale(kind, query.ultra_warm),
        query.pushed_by_fluid,
        query.is_player,
        query.velocity,
        &|x, y, z| {
            if error.get().is_some() {
                return CoreFluidCell::default();
            }
            match lookup.borrow_mut()(x, y, z) {
                Ok(cell) if cell.valid() => CoreFluidCell::from(cell),
                result => {
                    error.set(Some(result.err().unwrap_or(Error::InvalidArgument)));
                    CoreFluidCell::default()
                }
            }
        },
    );
    if let Some(error) = error.get() {
        return Err(error);
    }
    Ok(FluidImpulse {
        in_fluid: result.in_fluid,
        max_height: result.max_height,
        push: result.push,
    })
}
