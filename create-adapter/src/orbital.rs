// SPDX-License-Identifier: LGPL-3.0-or-later

//! Orbital & planetary dimension transitions for 6-DOF aeronautics
//! sub-worlds: connects [`crate::aeronautics::RigidBodySubWorld`] flight to
//! the shared [`mod_adapter_core::dimension_mod`] environment and teleport
//! machinery.
//!
//! An [`OrbitalTransitionRouter`] holds the dimension table (canonical ids,
//! coordinate scales, safe arrival bands and [`AtmospherePhysics`] profiles),
//! the declarative [`OrbitalTransitionRule`] set (altitude ceilings, descent
//! floors, world-space entry zones), the passenger roster and the transition
//! cooldown. [`OrbitalTransitionRouter::step_with_dimension_transitions`]
//! steps a ship under its current dimension's atmosphere, then evaluates the
//! rules against the post-step world center of mass: an airship climbing past
//! `Y >= 560` in the Overworld transfers atomically — horizontal momentum,
//! orientation, mounts and every onboard passenger included — into a
//! microgravity space dimension, and a hull sinking below an orbital floor
//! drops into a planet dimension under that planet's `gravity_scale`.
//!
//! Everything is deterministic and clean-room: rule order is registration
//! order, all physics runs through the existing sanitized integrator, and
//! `AERO2` snapshots stay byte-exact across crossings because `atmosphere`
//! is re-derived from the restored `parent_dim` on the next routed step.

use std::collections::BTreeMap;

use mod_adapter_core::dimension_mod as dim_mod;
use mod_adapter_core::dimension_mod::{DimTypeDescriptor, TeleportIntent};

use crate::aeronautics::{AtmospherePhysics, RigidBodySubWorld};

/// Base gravitational acceleration (m/s²) `gravity_scale` multiplies, matching
/// the value every existing `step(dt, 9.81, ..)` call site passes.
pub const SURFACE_GRAVITY: f64 = 9.81;

/// Ticks a ship waits after one dimension transition before any rule may
/// fire for it again — two seconds at 20 TPS. A hull hovering exactly on a
/// trigger boundary rides the cooldown instead of ping-ponging between
/// dimensions every tick.
pub const DEFAULT_TRANSITION_COOLDOWN_TICKS: u64 = 40;

// ---------------------------------------------------------------------------
// Triggers and rules
// ---------------------------------------------------------------------------

/// A world-space condition evaluated against a ship's center of mass.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum OrbitalTrigger {
    /// CoM `y >= ceiling_y`: atmospheric ceiling crossings (rocket ascent,
    /// `Y >= 560` into orbit).
    AscendCeiling { ceiling_y: f64 },
    /// CoM `y <= floor_y`: descent through an orbital floor into a
    /// planetary dimension (re-entry corridor).
    DescendFloor { floor_y: f64 },
    /// CoM inside the inclusive world-space box `[min, max]`: arbitrary
    /// orbital entry zones (e.g. a transfer window over a planet).
    InsideZone { min: [f64; 3], max: [f64; 3] },
}

impl OrbitalTrigger {
    /// Whether the trigger fires on a center of mass at `pos`. Non-finite
    /// components can never satisfy a bound.
    pub fn matches(&self, pos: [f64; 3]) -> bool {
        match *self {
            Self::AscendCeiling { ceiling_y } => pos[1] >= ceiling_y,
            Self::DescendFloor { floor_y } => pos[1] <= floor_y,
            Self::InsideZone { min, max } => {
                (0..3).all(|i| pos[i] >= min[i] && pos[i] <= max[i])
            }
        }
    }
}

/// One declarative transition rule: when a ship whose `parent_dim` is
/// `from_dim` meets `trigger`, ship and riders move to `dest_dim` in one
/// atomic step. Rule order is registration order; the first match wins.
#[derive(Clone, Debug, PartialEq)]
pub struct OrbitalTransitionRule {
    pub name: String,
    /// The `parent_dim` this trigger is evaluated in.
    pub from_dim: u32,
    pub trigger: OrbitalTrigger,
    /// Destination `parent_dim` after the transfer.
    pub dest_dim: u32,
    /// Fixed arrival altitude in the destination dimension. `None` keeps the
    /// ship's `y`, clamped inside the destination's safe band exactly the
    /// way [`dim_mod::compute_portal_destination`] clamps portal arrivals.
    pub spawn_y: Option<f64>,
    /// Anti-cheat movement grace carried into every passenger
    /// [`TeleportIntent`] (`ac_expect_teleport` window) so onboard players
    /// can never be false-flagged during orbital transit.
    pub teleport_grace_ticks: u32,
}

impl OrbitalTransitionRule {
    /// A rule with the shared default teleport grace and a coordinate-scale
    /// destination (`spawn_y: None`).
    pub fn new(
        name: impl Into<String>,
        from_dim: u32,
        trigger: OrbitalTrigger,
        dest_dim: u32,
    ) -> Self {
        Self {
            name: name.into(),
            from_dim,
            trigger,
            dest_dim,
            spawn_y: None,
            teleport_grace_ticks: dim_mod::DEFAULT_TELEPORT_GRACE_TICKS,
        }
    }

    /// Arrive at a fixed altitude in the destination dimension.
    pub fn with_spawn_y(mut self, spawn_y: f64) -> Self {
        self.spawn_y = Some(spawn_y);
        self
    }

    /// Override the per-passenger anti-cheat grace window.
    pub fn with_grace_ticks(mut self, grace_ticks: u32) -> Self {
        self.teleport_grace_ticks = grace_ticks;
        self
    }
}

// ---------------------------------------------------------------------------
// Dimension table
// ---------------------------------------------------------------------------

/// A dimension the router routes ships through: canonical id (carried into
/// [`TeleportIntent::target_dim`]), coordinate scale, safe arrival band and
/// the [`AtmospherePhysics`] installed on every ship flying there.
#[derive(Clone, Debug, PartialEq)]
pub struct OrbitalDimension {
    /// Canonical `"namespace:path"` id (`"minecraft:overworld"`,
    /// `"ad_astra:moon"`, a discovered dimension-mod id, ...).
    pub id: String,
    /// `dimension_type.coordinate_scale` (Overworld/End `1.0`, Nether `8.0`).
    pub coordinate_scale: f64,
    /// `dimension_type.min_y` — lower bound of the safe arrival band.
    pub min_y: i32,
    /// `dimension_type.height` — with `min_y`, bounds the safe band.
    pub height: i32,
    /// Environment physics installed on ships while in this dimension.
    pub atmosphere: AtmospherePhysics,
}

impl OrbitalDimension {
    /// A dimension with Overworld-shaped bounds (`min_y -64`, `height 384`,
    /// `coordinate_scale 1.0`) under `atmosphere`.
    pub fn new(id: impl Into<String>, atmosphere: AtmospherePhysics) -> Self {
        Self {
            id: id.into(),
            coordinate_scale: 1.0,
            min_y: -64,
            height: 384,
            atmosphere,
        }
    }

    /// Derive a registered dimension from a shared `dimension_type`
    /// descriptor plus the environment ships should fly in.
    pub fn from_descriptor(desc: &DimTypeDescriptor, atmosphere: AtmospherePhysics) -> Self {
        Self {
            id: desc.id.clone(),
            coordinate_scale: desc.coordinate_scale,
            min_y: desc.min_y,
            height: desc.height,
            atmosphere,
        }
    }

    /// The `dimension_type` descriptor used for coordinate-scale and
    /// safe-band math — identical inputs to the ones the shared
    /// [`dim_mod::compute_portal_destination`] consumes.
    fn to_descriptor(&self) -> DimTypeDescriptor {
        let mut desc = DimTypeDescriptor::defaults(&self.id, self.min_y, self.height);
        desc.coordinate_scale = self.coordinate_scale;
        desc
    }
}

// ---------------------------------------------------------------------------
// Transition records
// ---------------------------------------------------------------------------

/// A rider rigidly attached to a ship at `local_offset`, measured from the
/// ship's center of mass — the same input `passenger_world_delta` consumes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShipPassengerAttachment {
    pub ship_id: u64,
    pub local_offset: [f64; 3],
}

/// One passenger carried through a [`ShipDimensionTransition`]: the entity
/// id, the rigid local offset that survives the crossing unchanged, the
/// arrival world position, and the [`TeleportIntent`] the server executes
/// (with anti-cheat grace).
#[derive(Clone, Debug, PartialEq)]
pub struct ShipPassengerTransition {
    pub entity_id: i32,
    pub local_offset: [f64; 3],
    /// World position of the rider immediately after the transition.
    pub new_pos: [f64; 3],
    pub intent: TeleportIntent,
}

/// An atomic ship + passengers dimension crossing emitted by
/// [`OrbitalTransitionRouter::step_with_dimension_transitions`].
#[derive(Clone, Debug, PartialEq)]
pub struct ShipDimensionTransition {
    pub ship_id: u64,
    /// Name of the [`OrbitalTransitionRule`] that fired.
    pub rule_name: String,
    pub from_dim: u32,
    pub to_dim: u32,
    /// Canonical destination id as written into every passenger
    /// `TeleportIntent` (`"dim:<n>"` fallback for unregistered dims).
    pub to_dim_id: String,
    /// World pose the ship holds in the destination dimension.
    pub new_pos: [f64; 3],
    pub new_quat: [f64; 4],
    /// Linear velocity preserved across the crossing — horizontal momentum
    /// and climb rate ride through untouched.
    pub vel: [f64; 3],
    /// Router tick the transition fired on.
    pub tick: u64,
    /// Every onboard passenger's carry-through record, `entity_id` order.
    pub passengers: Vec<ShipPassengerTransition>,
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

/// Atomic dimension-transition router for a fleet of
/// [`RigidBodySubWorld`]s: dimension table + declarative rules + passenger
/// roster + cooldown hysteresis.
#[derive(Clone, Debug)]
pub struct OrbitalTransitionRouter {
    /// Declarative rules in registration order; first match wins per step.
    rules: Vec<OrbitalTransitionRule>,
    /// `parent_dim` -> dimension descriptor + atmosphere.
    dims: BTreeMap<u32, OrbitalDimension>,
    /// `entity_id` -> rigid attachment. Mirrors the production bridge's
    /// `eid -> (subworld_id, local_offset)` roster.
    passengers: BTreeMap<i32, ShipPassengerAttachment>,
    /// Hysteresis: minimum ticks between two transitions of the same ship.
    cooldown_ticks: u64,
    /// `ship_id` -> router tick of its last transition.
    last_transition: BTreeMap<u64, u64>,
    /// Router clock: one per `step_with_dimension_transitions` call.
    tick: u64,
    /// Base gravitational acceleration the per-dimension `gravity_scale`
    /// multiplies (`SURFACE_GRAVITY` = 9.81 by default).
    base_gravity: f64,
}

impl OrbitalTransitionRouter {
    /// A router preloaded with the three vanilla dimensions (`0` Overworld,
    /// `1` Nether at `coordinate_scale 8.0`, `2` End), all under
    /// [`AtmospherePhysics::OVERWORLD`]. Custom orbital/planet dimensions and
    /// rules are registered on top.
    pub fn new() -> Self {
        let mut router = Self {
            rules: Vec::new(),
            dims: BTreeMap::new(),
            passengers: BTreeMap::new(),
            cooldown_ticks: DEFAULT_TRANSITION_COOLDOWN_TICKS,
            last_transition: BTreeMap::new(),
            tick: 0,
            base_gravity: SURFACE_GRAVITY,
        };
        router.register_dimension(0, {
            let mut d = OrbitalDimension::new("minecraft:overworld", AtmospherePhysics::OVERWORLD);
            d.min_y = -64;
            d.height = 384;
            d
        });
        router.register_dimension(1, {
            let mut d = OrbitalDimension::new("minecraft:the_nether", AtmospherePhysics::OVERWORLD);
            d.min_y = 0;
            d.height = 256;
            d.coordinate_scale = 8.0;
            d
        });
        router.register_dimension(2, {
            let mut d = OrbitalDimension::new("minecraft:the_end", AtmospherePhysics::OVERWORLD);
            d.min_y = 0;
            d.height = 256;
            d
        });
        router
    }

    /// Register (or replace) the descriptor + atmosphere for `dim`.
    pub fn register_dimension(&mut self, dim: u32, descriptor: OrbitalDimension) {
        self.dims.insert(dim, descriptor);
    }

    /// Ensure a live dimension spec is routable, deriving its canonical id,
    /// coordinate scale and safe arrival band from the shared descriptor.
    /// Existing explicit registrations take precedence.
    pub fn ensure_dimension_from_spec(
        &mut self,
        dim: u32,
        descriptor: &DimTypeDescriptor,
        atmosphere: AtmospherePhysics,
    ) -> &OrbitalDimension {
        self.dims
            .entry(dim)
            .or_insert_with(|| OrbitalDimension::from_descriptor(descriptor, atmosphere))
    }

    /// The registered descriptor for `dim`, if any.
    pub fn dimension(&self, dim: u32) -> Option<&OrbitalDimension> {
        self.dims.get(&dim)
    }

    /// The [`AtmospherePhysics`] ships step under in `dim`;
    /// [`AtmospherePhysics::OVERWORLD`] for dimensions the router does not
    /// know (the bit-identical legacy physics).
    pub fn atmosphere_for(&self, dim: u32) -> AtmospherePhysics {
        self.dims
            .get(&dim)
            .map(|d| d.atmosphere)
            .unwrap_or(AtmospherePhysics::OVERWORLD)
    }

    /// Canonical id for `dim` as written into `TeleportIntent::target_dim`;
    /// `"dim:<n>"` when the destination was never registered.
    pub fn dim_id_for(&self, dim: u32) -> String {
        self.dims
            .get(&dim)
            .map(|d| d.id.clone())
            .unwrap_or_else(|| format!("dim:{dim}"))
    }

    /// Register a transition rule; rules evaluate in registration order.
    pub fn register_rule(&mut self, rule: OrbitalTransitionRule) {
        self.rules.push(rule);
    }

    /// The declarative rule set, in evaluation order.
    pub fn rules(&self) -> &[OrbitalTransitionRule] {
        &self.rules
    }

    /// Hysteresis window between a ship's transitions.
    pub fn cooldown_ticks(&self) -> u64 {
        self.cooldown_ticks
    }

    pub fn set_cooldown_ticks(&mut self, ticks: u64) {
        self.cooldown_ticks = ticks;
    }

    /// Base gravitational acceleration `gravity_scale` multiplies.
    pub fn base_gravity(&self) -> f64 {
        self.base_gravity
    }

    pub fn set_base_gravity(&mut self, base_gravity: f64) {
        self.base_gravity = base_gravity;
    }

    /// Router clock: routed steps taken so far.
    pub fn tick(&self) -> u64 {
        self.tick
    }

    /// Attach rider `entity_id` to `ship_id` at a local offset measured from
    /// the ship's center of mass.
    pub fn attach_passenger(
        &mut self,
        entity_id: i32,
        ship_id: u64,
        local_offset: [f64; 3],
    ) -> bool {
        self.passengers.insert(
            entity_id,
            ShipPassengerAttachment {
                ship_id,
                local_offset,
            },
        );
        true
    }

    /// Detach rider `entity_id` (dismount, disassembly, logout).
    pub fn detach_passenger(&mut self, entity_id: i32) -> bool {
        self.passengers.remove(&entity_id).is_some()
    }

    /// Every attachment for `ship_id`, in `entity_id` order.
    pub fn passengers_of(&self, ship_id: u64) -> Vec<(i32, ShipPassengerAttachment)> {
        self.passengers
            .iter()
            .filter(|(_, att)| att.ship_id == ship_id)
            .map(|(&eid, &att)| (eid, att))
            .collect()
    }

    /// Whether `ship_id` is inside its post-transition hysteresis window.
    pub fn in_cooldown(&self, ship_id: u64) -> bool {
        self.last_transition
            .get(&ship_id)
            .is_some_and(|t| self.tick.saturating_sub(*t) <= self.cooldown_ticks)
    }

    /// Step `ship` one routed tick: install the atmosphere of its current
    /// dimension, integrate under that profile, then evaluate the rule set
    /// against the post-step center of mass. A matching rule transfers the
    /// ship and every attached passenger atomically — `parent_dim`, world
    /// pose and `AtmospherePhysics` switch, horizontal momentum, orientation
    /// and mounts ride through — and the emitted
    /// [`ShipDimensionTransition`] carries one [`TeleportIntent`] per rider
    /// with the rule's anti-cheat grace.
    ///
    /// Ships inside their cooldown window keep stepping normally but no
    /// rule may fire for them, so a hull parked on a trigger boundary can
    /// never ping-pong between dimensions.
    pub fn step_with_dimension_transitions(
        &mut self,
        ship: &mut RigidBodySubWorld,
        dt: f64,
    ) -> Option<ShipDimensionTransition> {
        self.tick = self.tick.saturating_add(1);
        // The atmosphere of the dimension the ship currently flies in —
        // also re-derives the profile after a snapshot reload restored only
        // `parent_dim`, keeping AERO2 continuity byte-exact.
        ship.set_atmosphere(self.atmosphere_for(ship.parent_dim));
        ship.step_with_atmosphere(dt, self.base_gravity);
        if self.in_cooldown(ship.id) {
            return None;
        }
        let rule = self
            .rules
            .iter()
            .find(|r| r.from_dim == ship.parent_dim && r.trigger.matches(ship.pos))?
            .clone();
        Some(self.execute_transition(ship, &rule))
    }

    /// Apply `rule` to `ship`: coordinate-scale the horizontal position,
    /// resolve the arrival altitude, switch `parent_dim`/`pos`/`atmosphere`,
    /// re-key the embedded kinetic factory via
    /// [`RigidBodySubWorld::transfer_to_dimension`], then emit every rider's
    /// [`TeleportIntent`]. Runs only when the ship is outside cooldown.
    fn execute_transition(
        &mut self,
        ship: &mut RigidBodySubWorld,
        rule: &OrbitalTransitionRule,
    ) -> ShipDimensionTransition {
        let from_dim = ship.parent_dim;
        let dest = self.dims.get(&rule.dest_dim).cloned().unwrap_or_else(|| {
            OrbitalDimension::new(format!("dim:{}", rule.dest_dim), AtmospherePhysics::OVERWORLD)
        });
        let source_scale = self
            .dims
            .get(&from_dim)
            .map(|d| d.coordinate_scale)
            .unwrap_or(1.0);
        let desc = dest.to_descriptor();
        let scaled = dim_mod::compute_portal_destination(ship.pos, source_scale, &desc);
        let new_pos = match rule.spawn_y {
            Some(y) if y.is_finite() => [scaled[0], y, scaled[2]],
            _ => scaled,
        };

        ship.transfer_to_dimension(rule.dest_dim, new_pos);
        ship.set_atmosphere(dest.atmosphere);
        self.last_transition.insert(ship.id, self.tick);

        let mut passengers = Vec::new();
        for (entity_id, att) in &self.passengers {
            if att.ship_id != ship.id {
                continue;
            }
            let com = ship.center_of_mass;
            let world_pos = ship.local_to_world([
                com[0] + att.local_offset[0],
                com[1] + att.local_offset[1],
                com[2] + att.local_offset[2],
            ]);
            passengers.push(ShipPassengerTransition {
                entity_id: *entity_id,
                local_offset: att.local_offset,
                new_pos: world_pos,
                intent: TeleportIntent {
                    target_dim: dest.id.clone(),
                    target_pos: world_pos,
                    expect_teleport_grace_ticks: rule.teleport_grace_ticks,
                },
            });
        }

        ShipDimensionTransition {
            ship_id: ship.id,
            rule_name: rule.name.clone(),
            from_dim,
            to_dim: rule.dest_dim,
            to_dim_id: dest.id.clone(),
            new_pos: ship.pos,
            new_quat: ship.quat,
            vel: ship.vel,
            tick: self.tick,
            passengers,
        }
    }
}

impl Default for OrbitalTransitionRouter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::addon_profile::{createaeronautics_profile, create_registry_with_addons};
    use crate::aeronautics::{encode_aeronautics_snapshot, SEA_LEVEL_AIR_DENSITY};
    use crate::blockstates::{CreateBlockRegistry, DEFAULT_BASE_STATE_ID};
    use std::collections::BTreeMap;

    fn aero_registry() -> (crate::addon_profile::CreateAddonProfile, CreateBlockRegistry) {
        let addon = createaeronautics_profile();
        let reg =
            create_registry_with_addons(DEFAULT_BASE_STATE_ID, std::slice::from_ref(&addon))
                .unwrap();
        (addon, reg)
    }

    fn canonical(reg: &CreateBlockRegistry, id: &str) -> String {
        reg.encode_canonical(reg.default_state(id).unwrap())
            .unwrap()
            .to_string()
    }

    /// A simple two-block hull: assembler deck + one envelope.
    fn hull(reg: &CreateBlockRegistry) -> BTreeMap<[i32; 3], String> {
        let mut cells = BTreeMap::new();
        cells.insert([0, 0, 0], canonical(reg, "createaeronautics:airship_assembler"));
        cells.insert([0, 1, 0], canonical(reg, "createaeronautics:balloon_envelope"));
        cells
    }

    #[test]
    fn triggers_match_only_in_bounds() {
        let ceiling = OrbitalTrigger::AscendCeiling { ceiling_y: 560.0 };
        assert!(ceiling.matches([0.0, 560.0, 0.0]));
        assert!(ceiling.matches([0.0, 700.0, 0.0]));
        assert!(!ceiling.matches([0.0, 559.999, 0.0]));
        assert!(!ceiling.matches([0.0, f64::NAN, 0.0]));

        let floor = OrbitalTrigger::DescendFloor { floor_y: 64.0 };
        assert!(floor.matches([0.0, 64.0, 0.0]));
        assert!(floor.matches([0.0, -30.0, 0.0]));
        assert!(!floor.matches([0.0, 64.001, 0.0]));

        let zone = OrbitalTrigger::InsideZone {
            min: [-10.0, 80.0, -10.0],
            max: [10.0, 120.0, 10.0],
        };
        assert!(zone.matches([0.0, 100.0, 0.0]));
        assert!(zone.matches([10.0, 80.0, -10.0]));
        assert!(!zone.matches([11.0, 100.0, 0.0]));
        assert!(!zone.matches([0.0, 121.0, 0.0]));
    }

    #[test]
    fn overworld_atmosphere_is_bit_identical_to_legacy_step() {
        let (addon, reg) = aero_registry();
        let mut a =
            RigidBodySubWorld::assemble(1, 0, hull(&reg), &reg, Some(&addon), 100.0).unwrap();
        let mut b =
            RigidBodySubWorld::assemble(1, 0, hull(&reg), &reg, Some(&addon), 100.0).unwrap();
        a.pos = [3.0, 200.0, -8.0];
        b.pos = a.pos;
        a.vel = [1.5, -0.25, 0.75];
        b.vel = a.vel;
        assert_eq!(b.atmosphere(), AtmospherePhysics::OVERWORLD);
        for _ in 0..100 {
            a.step(0.05, 9.81, SEA_LEVEL_AIR_DENSITY);
            b.step_with_atmosphere(0.05, 9.81);
        }
        assert_eq!(
            encode_aeronautics_snapshot(&a),
            encode_aeronautics_snapshot(&b)
        );
    }

    #[test]
    fn microgravity_coasts_without_gravity_drop_and_scales_planet_gravity() {
        let (addon, reg) = aero_registry();
        // Space: no gravity, no drag — velocity carries exactly.
        let mut space =
            RigidBodySubWorld::assemble(1, 9, hull(&reg), &reg, Some(&addon), 100.0).unwrap();
        space.set_atmosphere(AtmospherePhysics::MICROGRAVITY);
        space.pos = [0.0, 500.0, 0.0];
        space.vel = [4.0, 6.0, -2.0];
        for _ in 0..40 {
            space.step_with_atmosphere(0.05, 9.81);
        }
        assert_eq!(space.vel, [4.0, 6.0, -2.0]);
        assert!((space.pos[0] - 4.0 * 0.05 * 40.0).abs() < 1e-9);
        assert!((space.pos[1] - (500.0 + 6.0 * 0.05 * 40.0)).abs() < 1e-9);

        // Moon: exactly 0.165 of surface gravity accelerates the fall.
        let mut moon =
            RigidBodySubWorld::assemble(2, 10, hull(&reg), &reg, Some(&addon), 100.0).unwrap();
        moon.set_atmosphere(AtmospherePhysics::MOON);
        moon.pos = [0.0, 200.0, 0.0];
        moon.step_with_atmosphere(0.05, 9.81);
        let expect = -(9.81 * 0.165) * 0.05;
        assert!(
            (moon.vel[1] - expect).abs() < 1e-12,
            "moon fall: vel={:?} expect={expect}",
            moon.vel
        );
    }

    #[test]
    fn buoyancy_gate_keeps_drag_but_drops_lift() {
        let (addon, reg) = aero_registry();
        // Thin-atmosphere planet with buoyancy off: the envelope hull still
        // feels drag sideways but gets no lift.
        let mut thin =
            RigidBodySubWorld::assemble(3, 11, hull(&reg), &reg, Some(&addon), 100.0).unwrap();
        let mut atmo = AtmospherePhysics::low_gravity(0.38, 0.2);
        atmo.enable_buoyancy = false;
        thin.set_atmosphere(atmo);
        thin.pos = [0.0, 150.0, 0.0];
        thin.vel = [10.0, 0.0, 0.0];
        thin.step_with_atmosphere(0.05, 9.81);
        // Lateral drag (air_density 0.2) slows x; gravity pulls y down with
        // no envelope lift.
        assert!(thin.vel[0] < 10.0 && thin.vel[0] > 0.0);
        assert!(thin.vel[1] < 0.0);

        // And the vacuum_damping stabilizer bleeds speed in zero-atmosphere.
        let mut drift =
            RigidBodySubWorld::assemble(4, 9, hull(&reg), &reg, Some(&addon), 100.0).unwrap();
        let mut space = AtmospherePhysics::MICROGRAVITY;
        space.vacuum_damping = 50.0;
        drift.set_atmosphere(space);
        drift.pos = [0.0, 400.0, 0.0];
        drift.vel = [8.0, 0.0, 0.0];
        for _ in 0..20 {
            drift.step_with_atmosphere(0.05, 9.81);
        }
        assert!(drift.vel[0] < 8.0 && drift.vel[0] > 0.0, "damping: {:?}", drift.vel);
    }

    #[test]
    fn transfer_to_dimension_rekeys_kinetics_and_preserves_state() {
        let (addon, reg) = aero_registry();
        let mut cells = BTreeMap::new();
        cells.insert([0, 0, 0], canonical(&reg, "createaeronautics:airship_assembler"));
        let mut sub =
            RigidBodySubWorld::assemble(5, 0, cells, &reg, Some(&addon), 100.0).unwrap();
        sub.quat = [0.0, 0.0, 0.0, 1.0];
        sub.vel = [3.0, 1.0, -2.0];
        sub.omega = [0.0, 0.5, 0.0];
        sub.transfer_to_dimension(9, [40.0, 120.0, -7.0]);
        assert_eq!(sub.parent_dim, 9);
        assert_eq!(sub.pos, [40.0, 120.0, -7.0]);
        assert_eq!(sub.vel, [3.0, 1.0, -2.0]);
        assert_eq!(sub.omega, [0.0, 0.5, 0.0]);
        // Non-finite arrival positions sanitize, never corrupt.
        sub.transfer_to_dimension(10, [f64::NAN, 64.0, f64::INFINITY]);
        assert_eq!(sub.pos, [0.0, 64.0, 0.0]);
    }

    #[test]
    fn cooldown_blocks_boundary_ping_pong() {
        let mut router = OrbitalTransitionRouter::new();
        router.register_dimension(
            9,
            OrbitalDimension::new("test:space", AtmospherePhysics::MICROGRAVITY),
        );
        // Pathological pair: dim0 ceiling sends to 9 at exactly y=560, where
        // dim9's own ceiling rule would fire back immediately.
        router.register_rule(
            OrbitalTransitionRule::new(
                "test:to_space",
                0,
                OrbitalTrigger::AscendCeiling { ceiling_y: 560.0 },
                9,
            )
            .with_spawn_y(560.0),
        );
        router.register_rule(
            OrbitalTransitionRule::new(
                "test:to_over",
                9,
                OrbitalTrigger::AscendCeiling { ceiling_y: 560.0 },
                0,
            )
            .with_spawn_y(560.0),
        );

        let (mut addon, reg) = aero_registry();
        // Retune lift so the 200 kg hull holds exact buoyancy equilibrium at
        // the 560 boundary (stock taper ends lift at 320).
        addon.propulsion = Some(crate::addon_profile::PropulsionSpec {
            lift_per_buoyancy_block: 200.0 * 9.81,
            lift_full_altitude: 600.0,
            max_lift_altitude: 1200.0,
            ..crate::addon_profile::PropulsionSpec::aeronautics_default()
        });
        let mut sub =
            RigidBodySubWorld::assemble(7, 0, hull(&reg), &reg, Some(&addon), 100.0).unwrap();
        sub.pos = [0.0, 560.0, 0.0];

        // Tick 1: first step fires `to_space`; the ship then hovers at 560 in
        // dim 9 for the whole cooldown without a single return transition.
        let first = router.step_with_dimension_transitions(&mut sub, 0.05).unwrap();
        assert_eq!((first.from_dim, first.to_dim), (0, 9));
        assert!(router.in_cooldown(7));
        for _ in 0..DEFAULT_TRANSITION_COOLDOWN_TICKS {
            assert!(router.step_with_dimension_transitions(&mut sub, 0.05).is_none());
            assert_eq!(sub.parent_dim, 9);
        }
        // After the window the boundary rule may fire once — exactly once.
        let mut fired = 0;
        for _ in 0..10 {
            if router.step_with_dimension_transitions(&mut sub, 0.05).is_some() {
                fired += 1;
            }
        }
        assert_eq!(fired, 1);
    }

    #[test]
    fn dynamically_registered_dimension_routes_with_descriptor_physics() {
        let desc = DimTypeDescriptor::defaults("modded:sky", -64, 512);
        let mut router = OrbitalTransitionRouter::new();
        let atmosphere = AtmospherePhysics::low_gravity(0.38, 0.06);
        let dimension = router.ensure_dimension_from_spec(3, &desc, atmosphere);
        assert_eq!(dimension.id, "modded:sky");
        assert_eq!((dimension.min_y, dimension.height), (-64, 512));
        assert_eq!(dimension.atmosphere, atmosphere);
        assert_eq!(router.atmosphere_for(3), atmosphere);

        router.register_rule(
            OrbitalTransitionRule::new(
                "overworld_to_sky",
                0,
                OrbitalTrigger::AscendCeiling { ceiling_y: 90.0 },
                3,
            )
            .with_spawn_y(180.0),
        );
        let (addon, reg) = aero_registry();
        let mut sub = RigidBodySubWorld::assemble(30, 0, hull(&reg), &reg, Some(&addon), 100.0)
            .unwrap();
        sub.pos = [0.0, 100.0, 0.0];
        let transition = router
            .step_with_dimension_transitions(&mut sub, 0.05)
            .expect("registered custom dimension route fires");
        assert_eq!(transition.to_dim, 3);
        assert_eq!(transition.to_dim_id, "modded:sky");
        assert_eq!(sub.parent_dim, 3);
    }
}
