// SPDX-License-Identifier: LGPL-3.0-or-later
//! Minecraft 1.21.1 projectile motion and first contact along a tick segment.
use std::array::from_fn;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProjectileKind {
    Arrow,
    SpectralArrow,
    Trident,
    Snowball,
    Egg,
    EnderPearl,
    SplashPotion,
    LingeringPotion,
    ExperienceBottle,
    Fireball,
    SmallFireball,
    WitherSkull,
    DangerousWitherSkull,
    WindCharge,
    FishingBobber,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProjectileSpec {
    pub gravity: f64,
    pub air_drag: f64,
    pub water_drag: f64,
    pub hit_margin: f64,
}

impl ProjectileKind {
    pub const fn spec(self) -> ProjectileSpec {
        use ProjectileKind::*;
        let (gravity, air_drag, water_drag) = match self {
            Arrow | SpectralArrow => (0.05, 0.99, 0.60),
            Trident => (0.05, 0.99, 0.99),
            Snowball | Egg | EnderPearl => (0.03, 0.99, 0.80),
            SplashPotion | LingeringPotion => (0.05, 0.99, 0.80),
            ExperienceBottle => (0.07, 0.99, 0.80),
            Fireball | SmallFireball | WitherSkull => (0.0, 0.95, 0.80),
            DangerousWitherSkull => (0.0, 0.73, 0.80),
            WindCharge => (0.0, 1.0, 1.0),
            FishingBobber => (0.03, 0.92, 0.92),
        };
        ProjectileSpec {
            gravity,
            air_drag,
            water_drag,
            hit_margin: 0.30,
        }
    }
}

/// Protocol 767 entity type in the Minecraft 1.21.1 registry.
pub const fn projectile_wire_entity_type(kind: ProjectileKind) -> i64 {
    use ProjectileKind::*;
    match kind {
        Arrow => 4,
        Egg => 28,
        EnderPearl => 32,
        ExperienceBottle => 37,
        Fireball => 62,
        SplashPotion | LingeringPotion => 82,
        SmallFireball => 94,
        Snowball => 97,
        SpectralArrow => 99,
        Trident => 109,
        WindCharge => 117,
        WitherSkull | DangerousWitherSkull => 121,
        FishingBobber => 129,
    }
}

pub type Point = [f64; 3];
pub type Bounds = (Point, Point);

pub fn step_projectile_kinematics(
    pos: Point,
    vel: Point,
    in_water: bool,
    spec: ProjectileSpec,
) -> (Point, Point) {
    let drag = if in_water {
        spec.water_drag
    } else {
        spec.air_drag
    };
    let mut next = vel.map(|v| v * drag);
    next[1] -= spec.gravity;
    (from_fn(|i| pos[i] + vel[i]), next)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Contact {
    Block { cell: [i32; 3], normal: [i8; 3] },
    Entity(i32),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Impact {
    pub t: f64,
    pub point: Point,
    pub contact: Contact,
}

#[derive(Default, Debug)]
pub struct FlightContacts {
    pub age_ticks: u32,
    pub left_owner: bool,
    pub pierced: Vec<i32>,
}

// Slab entry in units of the original segment. A start inside is an
// immediate hit; a segment starting on a face and travelling away is free.
fn clip(pos: Point, vel: Point, (min, max): Bounds, limit: f64) -> Option<(f64, [i8; 3])> {
    let mut entry = f64::NEG_INFINITY;
    let mut exit = f64::INFINITY;
    let mut normal = [0; 3];
    for i in 0..3 {
        if vel[i] == 0.0 {
            if pos[i] < min[i] || pos[i] > max[i] {
                return None;
            }
            continue;
        }
        let a = (min[i] - pos[i]) / vel[i];
        let b = (max[i] - pos[i]) / vel[i];
        if a.min(b) > entry {
            entry = a.min(b);
            normal = [0; 3];
            normal[i] = if vel[i] > 0.0 { -1 } else { 1 };
        }
        exit = exit.min(a.max(b));
    }
    (exit > 0.0 && entry <= exit && entry <= limit).then_some((entry.max(0.0), normal))
}

/// Traverse voxel boundaries in increasing segment time. The world adapter
/// supplies local collision boxes for each visited cell and same-world targets
/// with their uninflated world-space bounds. A block wins exact-distance ties.
pub fn sweep_projectile_segment<I>(
    pos: Point,
    vel: Point,
    spec: ProjectileSpec,
    shooter: i32,
    flight: &mut FlightContacts,
    mut shapes: impl FnMut([i32; 3]) -> I,
    entities: impl IntoIterator<Item = (i32, Bounds)>,
) -> Option<Impact>
where
    I: IntoIterator<Item = Bounds>,
{
    if !pos.into_iter().chain(vel).all(f64::is_finite) {
        return None;
    }
    let mut cell = pos.map(|v| v.floor() as i32);
    let step: [i32; 3] = vel.map(|v| {
        if v > 0.0 {
            1
        } else if v < 0.0 {
            -1
        } else {
            0
        }
    });
    let stride = vel.map(|v| 1.0 / v.abs());
    let mut boundary: Point = from_fn(|i| {
        if step[i] == 0 {
            return f64::INFINITY;
        }
        (f64::from(cell[i]) + f64::from(step[i] > 0) - pos[i]) / vel[i]
    });
    let mut nearest = None;
    let mut limit = 1.0;
    loop {
        let local = from_fn(|i| pos[i] - f64::from(cell[i]));
        for bounds in shapes(cell) {
            if let Some((t, normal)) = clip(local, vel, bounds, limit) {
                limit = t;
                nearest = Some(Contact::Block { cell, normal });
            }
        }
        let next = boundary.into_iter().fold(f64::INFINITY, f64::min);
        if next > limit {
            break;
        }
        for i in 0..3 {
            if boundary[i] == next {
                cell[i] = cell[i].checked_add(step[i])?;
                boundary[i] += stride[i];
            }
        }
    }

    let mut touching_owner = false;
    for (id, (min, max)) in entities {
        let bounds = (
            min.map(|v| v - spec.hit_margin),
            max.map(|v| v + spec.hit_margin),
        );
        if id == shooter {
            touching_owner = (0..3).all(|i| pos[i] >= bounds.0[i] && pos[i] <= bounds.1[i]);
            if flight.age_ticks < 2 || (!flight.left_owner && touching_owner) {
                continue;
            }
        }
        if flight.pierced.contains(&id) {
            continue;
        }
        if let Some((t, _)) = clip(pos, vel, bounds, limit) {
            if nearest.is_none()
                || t < limit
                || matches!(nearest, Some(Contact::Entity(other)) if t == limit && id < other)
            {
                limit = t;
                nearest = Some(Contact::Entity(id));
            }
        }
    }
    flight.left_owner |= !touching_owner;
    nearest.map(|contact| Impact {
        t: limit,
        point: from_fn(|i| pos[i] + vel[i] * limit),
        contact,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_entity_precedes_wall_regardless_of_iteration_order() {
        let entities = [
            (3, ([5.0, 0., 0.], [6., 1., 1.])),
            (9, ([2., 0., 0.], [3., 1., 1.])),
        ];
        for targets in [entities, [entities[1], entities[0]]] {
            let hit = sweep_projectile_segment(
                [0., 0.5, 0.5],
                [10., 0., 0.],
                ProjectileKind::Arrow.spec(),
                -1,
                &mut FlightContacts::default(),
                |cell| (cell[0] == 4).then_some(([0.; 3], [1.; 3])),
                targets,
            )
            .unwrap();
            assert_eq!(hit.contact, Contact::Entity(9));
            assert!((hit.point[0] - 1.7).abs() < 1e-12);
        }
    }

    #[test]
    fn negative_direction_partial_block_and_occlusion() {
        let hit = sweep_projectile_segment(
            [5.5, 0.25, 0.5],
            [-10., 0., 0.],
            ProjectileKind::Egg.spec(),
            -1,
            &mut FlightContacts::default(),
            |c| (c[0] == -2).then_some(([0.; 3], [0.5; 3])),
            [(42, ([-4.; 3], [-3., 1., 1.]))],
        )
        .unwrap();
        assert_eq!(
            hit.contact,
            Contact::Block {
                cell: [-2, 0, 0],
                normal: [1, 0, 0]
            }
        );
        assert_eq!(hit.point, [-1.5, 0.25, 0.5]);
    }

    #[test]
    fn owner_grace_pierce_exclusions_and_water_drag() {
        let spec = ProjectileKind::Arrow.spec();
        let mut flight = FlightContacts {
            age_ticks: 9,
            pierced: vec![2],
            ..Default::default()
        };
        let targets = [(1, ([-0.3; 3], [0.3; 3])), (2, ([1.; 3], [2.; 3]))];
        let mut sweep = |start, velocity| {
            sweep_projectile_segment(start, velocity, spec, 1, &mut flight, |_| None, targets)
        };
        assert!(sweep([0.; 3], [3.; 3]).is_none());
        assert_eq!(
            sweep([3.; 3], [-3.; 3]).unwrap().contact,
            Contact::Entity(1)
        );
        assert_eq!(
            step_projectile_kinematics([1.; 3], [2.; 3], true, spec),
            ([3.; 3], [1.2, 1.15, 1.2])
        );
    }
}
