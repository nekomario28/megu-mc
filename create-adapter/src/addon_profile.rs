// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Universal Create addon extensibility: a declarative [`CreateAddonProfile`]
//! describing any Create companion mod's kinetic sources, consumers,
//! connectors, transport/fluid blocks, and 6-DOF propulsion blocks, plus
//! zero-JVM JAR/datapack auto-discovery so unknown addons offload without
//! hand-authored Rust code.
//!
//! Every profile carries two synchronized views of the same addon:
//! - `block_defs()`: flagship [`CreateBlockRole`] definitions that merge into
//!   the `CreateFactory` block registry next to the core `create:*` catalog.
//! - `offload`: a [`ModOffloadProfile`] mirror feeding the universal runtime
//!   (`UniversalModRuntime`) so transport/fluid/network pieces also work on
//!   the generic tier and in 4-tier fallback classification.

use std::collections::{BTreeMap, BTreeSet};

use mod_adapter_core::blockstates::{
    BlockDef, BlockRegistry, PropertyDef, RegistryError, UniversalBlockRole,
};
use mod_adapter_core::data_loader::{parse_json, DataLoadError, JsonValue, LoadedModData};
use mod_adapter_core::profile::ModOffloadProfile;

use crate::blockstates::{core_create_block_defs, CreateBlockRegistry, CreateBlockRole};
use crate::factory::Axis;

/// Kinetic connector variants understood by `CreateFactory` topology rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectorKind {
    Shaft,
    Cogwheel,
    LargeCogwheel,
    Gearbox,
    Clutch,
    Gearshift,
}

impl ConnectorKind {
    fn role(self) -> CreateBlockRole {
        match self {
            Self::Shaft => CreateBlockRole::Shaft,
            Self::Cogwheel => CreateBlockRole::Cogwheel,
            Self::LargeCogwheel => CreateBlockRole::LargeCogwheel,
            Self::Gearbox => CreateBlockRole::Gearbox,
            Self::Clutch => CreateBlockRole::Clutch,
            Self::Gearshift => CreateBlockRole::Gearshift,
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        match name {
            "shaft" => Some(Self::Shaft),
            "cogwheel" => Some(Self::Cogwheel),
            "large_cogwheel" => Some(Self::LargeCogwheel),
            "gearbox" => Some(Self::Gearbox),
            "clutch" => Some(Self::Clutch),
            "gearshift" => Some(Self::Gearshift),
            _ => None,
        }
    }
}

/// Addon block injecting rotation into a kinetic network (motors, engines,
/// energisers, diesel generators). `stress_capacity` is the SU produced at
/// `base_rpm`; `axis` is the block's declared default rotation axis used when
/// the blockstate carries an `axis`/`facing` property.
#[derive(Clone, Debug, PartialEq)]
pub struct KineticSourceSpec {
    pub id: String,
    pub base_rpm: i32,
    pub stress_capacity: f64,
    pub axis: Axis,
}

/// Addon block drawing stress from a kinetic network. `min_rpm` gates
/// speed-dependent behaviour (e.g. propellers only develop thrust at or above
/// this RPM, mills refuse work below it).
#[derive(Clone, Debug, PartialEq)]
pub struct KineticConsumerSpec {
    pub id: String,
    pub stress_impact: f64,
    pub min_rpm: f64,
}

/// Addon kinetic connector block (shafts, cogwheels, gearboxes, clutches,
/// gearshifts, chain drives) participating in `CreateFactory` link rules.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectorSpec {
    pub id: String,
    pub kind: ConnectorKind,
}

/// 6-DOF propulsion model for aeronautics-class addons. Force/torque gains
/// are tuned so [`crate::aeronautics::RigidBodySubWorld`] stays bounded and
/// deterministic at the fixed 20 TPS step.
#[derive(Clone, Debug, PartialEq)]
pub struct PropulsionSpec {
    /// Newtons of thrust per solved RPM per propeller mount.
    pub thrust_per_rpm: f64,
    /// Newtons of lift per buoyancy/envelope block at full-lift altitude.
    pub lift_per_buoyancy_block: f64,
    /// Quadratic aerodynamic drag coefficient (`F = -k * ρ * |v| * v`).
    pub drag_coeff: f64,
    /// Aerodynamic angular damping coefficient (`τ = -k * ρ * ω`).
    pub angular_drag_coeff: f64,
    /// Rudder torque gain (Nm per m/s of sensed local lateral airflow).
    pub rudder_torque: f64,
    /// Altitude (world Y) below which buoyancy lift is undiminished.
    pub lift_full_altitude: f64,
    /// Altitude (world Y) at and above which buoyancy lift reaches zero.
    pub max_lift_altitude: f64,
    /// Kinetic blocks whose mounts produce thrust (propeller bearings,
    /// thrusters); each must also appear in `kinetic_sources` or
    /// `kinetic_consumers` to spin, or it is registered with a default
    /// 8 SU/RPM consumer load.
    pub propeller_blocks: Vec<String>,
    /// Blocks contributing lift (`balloon_envelope`, `levitation_core`).
    pub buoyancy_blocks: Vec<String>,
    /// Blocks contributing control torque (`rudder`, helm blocks).
    pub rudder_blocks: Vec<String>,
}

impl PropulsionSpec {
    /// Reference tuning used by the built-in Create: Aeronautics profile and
    /// as the fallback for discovered aeronautics-class addons.
    pub fn aeronautics_default() -> Self {
        Self {
            thrust_per_rpm: 150.0,
            lift_per_buoyancy_block: 1200.0,
            drag_coeff: 2.0,
            angular_drag_coeff: 80.0,
            rudder_torque: 60.0,
            lift_full_altitude: 200.0,
            max_lift_altitude: 320.0,
            propeller_blocks: Vec::new(),
            buoyancy_blocks: Vec::new(),
            rudder_blocks: Vec::new(),
        }
    }
}

/// Declarative Create addon profile: Create-specific component rules layered
/// on a [`ModOffloadProfile`] base so one manifest drives both the flagship
/// `create-adapter` registry and the universal `mod-adapter-core` runtime.
#[derive(Clone, Debug)]
pub struct CreateAddonProfile {
    pub mod_id: String,
    /// Universal-tier mirror of this addon (transport pipes, fluid
    /// pipes/tanks/pumps, processors, kinetic network nodes, decorative
    /// blocks). `finalize` keeps it synchronized with the Create-side lists.
    pub offload: ModOffloadProfile,
    pub kinetic_sources: Vec<KineticSourceSpec>,
    pub kinetic_consumers: Vec<KineticConsumerSpec>,
    pub connectors: Vec<ConnectorSpec>,
    /// Blocks that anchor a contraption / airship assembly
    /// (`airship_assembler`, bearings).
    pub assembly_anchors: Vec<String>,
    /// Aeronautics propulsion model; `None` for non-flying addons.
    pub propulsion: Option<PropulsionSpec>,
    /// Additional purely-decorative blocks (controls, cosmetic blocks).
    pub decorative_blocks: Vec<String>,
    /// Ore/chest/barrel blocks folded into the anti-xray hidden set.
    pub hidden_blocks: Vec<String>,
    /// Explicit or discovered blockstate property tables (`id -> props`).
    /// Overrides the axis/facing defaults in `block_defs()`.
    pub block_properties: BTreeMap<String, Vec<PropertyDef>>,
}

impl CreateAddonProfile {
    /// Empty programmatic profile for `mod_id`; populate fields then call
    /// [`Self::finalize`] to mirror Create-side blocks into `offload`.
    pub fn new(mod_id: impl Into<String>) -> Self {
        let mod_id = mod_id.into();
        Self {
            mod_id: mod_id.clone(),
            offload: ModOffloadProfile {
                mod_id,
                blocks: Vec::new(),
                transport_delay_ticks: BTreeMap::new(),
                tank_capacity_mb: BTreeMap::new(),
                processor_recipe_types: BTreeMap::new(),
            },
            kinetic_sources: Vec::new(),
            kinetic_consumers: Vec::new(),
            connectors: Vec::new(),
            assembly_anchors: Vec::new(),
            propulsion: None,
            decorative_blocks: Vec::new(),
            hidden_blocks: Vec::new(),
            block_properties: BTreeMap::new(),
        }
    }

    /// Parse a declarative Create addon profile JSON document. The universal
    /// `blocks` list shares [`ModOffloadProfile::from_json`]'s schema;
    /// Create-side sections (`kinetic_sources`, `kinetic_consumers`,
    /// `connectors`, `assembly_anchors`, `decorative_blocks`, `hidden_blocks`,
    /// `block_properties`, `propulsion`) extend it.
    pub fn from_json(bytes: &[u8]) -> Result<Self, DataLoadError> {
        let offload = ModOffloadProfile::from_json(bytes)?;
        let root = parse_json(bytes)?;
        let obj = root
            .as_object()
            .ok_or_else(|| DataLoadError::Json("addon profile root must be object".into()))?;
        let mut profile = Self::new(offload.mod_id.clone());
        profile.offload = offload;

        if let Some(arr) = obj.get("kinetic_sources").and_then(JsonValue::as_array) {
            for item in arr {
                let o = json_obj(item, "kinetic_sources entry")?;
                let id = json_str(o, "id")?.to_string();
                profile.kinetic_sources.push(KineticSourceSpec {
                    id,
                    base_rpm: json_f64(o, "rpm").unwrap_or(32.0) as i32,
                    stress_capacity: json_f64(o, "capacity").unwrap_or(512.0),
                    axis: axis_from_name(
                        o.get("axis").and_then(JsonValue::as_str).unwrap_or("y"),
                    ),
                });
            }
        }
        if let Some(arr) = obj
            .get("kinetic_consumers")
            .and_then(JsonValue::as_array)
        {
            for item in arr {
                let o = json_obj(item, "kinetic_consumers entry")?;
                profile.kinetic_consumers.push(KineticConsumerSpec {
                    id: json_str(o, "id")?.to_string(),
                    stress_impact: json_f64(o, "impact").unwrap_or(4.0),
                    min_rpm: json_f64(o, "min_rpm").unwrap_or(0.0),
                });
            }
        }
        if let Some(arr) = obj.get("connectors").and_then(JsonValue::as_array) {
            for item in arr {
                let o = json_obj(item, "connectors entry")?;
                let kind = ConnectorKind::from_name(json_str(o, "kind")?)
                    .ok_or_else(|| DataLoadError::Json("unknown connector kind".into()))?;
                profile.connectors.push(ConnectorSpec {
                    id: json_str(o, "id")?.to_string(),
                    kind,
                });
            }
        }
        for (key, out) in [
            ("assembly_anchors", &mut profile.assembly_anchors),
            ("decorative_blocks", &mut profile.decorative_blocks),
            ("hidden_blocks", &mut profile.hidden_blocks),
        ] {
            if let Some(arr) = obj.get(key).and_then(JsonValue::as_array) {
                out.extend(arr.iter().filter_map(|v| v.as_str().map(str::to_string)));
            }
        }
        if let Some(props) = obj.get("block_properties").and_then(JsonValue::as_object) {
            for (id, ptable) in props {
                if let Some(pobj) = ptable.as_object() {
                    let defs: Vec<PropertyDef> = pobj
                        .iter()
                        .filter_map(|(name, vals)| {
                            let values: Vec<String> = vals
                                .as_array()?
                                .iter()
                                .filter_map(|v| v.as_str().map(str::to_string))
                                .collect();
                            (!values.is_empty()).then(|| PropertyDef {
                                name: name.clone(),
                                values,
                            })
                        })
                        .collect();
                    profile.block_properties.insert(id.clone(), defs);
                }
            }
        }
        if let Some(pobj) = obj.get("propulsion").and_then(JsonValue::as_object) {
            let mut spec = PropulsionSpec::aeronautics_default();
            for (key, slot) in [
                ("thrust_per_rpm", &mut spec.thrust_per_rpm),
                ("lift_per_block", &mut spec.lift_per_buoyancy_block),
                ("drag", &mut spec.drag_coeff),
                ("angular_drag", &mut spec.angular_drag_coeff),
                ("rudder_torque", &mut spec.rudder_torque),
                ("lift_full_altitude", &mut spec.lift_full_altitude),
                ("max_lift_altitude", &mut spec.max_lift_altitude),
            ] {
                if let Some(v) = pobj.get(key).and_then(JsonValue::as_f64) {
                    *slot = v;
                }
            }
            for (key, out) in [
                ("propeller_blocks", &mut spec.propeller_blocks),
                ("buoyancy_blocks", &mut spec.buoyancy_blocks),
                ("rudder_blocks", &mut spec.rudder_blocks),
            ] {
                if let Some(arr) = pobj.get(key).and_then(JsonValue::as_array) {
                    out.extend(arr.iter().filter_map(|v| v.as_str().map(str::to_string)));
                }
            }
            profile.propulsion = Some(spec);
        }
        profile.finalize();
        Ok(profile)
    }

    /// Flagship [`CreateBlockRole`] definitions for every Create-side block in
    /// this profile, ready to merge into a [`CreateBlockRegistry`].
    /// First declaration wins on duplicate ids (sources > connectors >
    /// consumers > offload mirrors > mounts > anchors > decorative).
    pub fn block_defs(&self) -> Vec<BlockDef<CreateBlockRole>> {
        let axis_props: &dyn Fn() -> Vec<PropertyDef> =
            &|| vec![PropertyDef::new("axis", &["x", "y", "z"])];
        let axis_powered_props: &dyn Fn() -> Vec<PropertyDef> = &|| {
            vec![
                PropertyDef::new("axis", &["x", "y", "z"]),
                PropertyDef::new("powered", &["false", "true"]),
            ]
        };
        let facing_props: &dyn Fn() -> Vec<PropertyDef> = &|| {
            vec![PropertyDef::new(
                "facing",
                &["north", "south", "east", "west", "up", "down"],
            )]
        };
        let empty_props: &dyn Fn() -> Vec<PropertyDef> = &Vec::new;

        let mut out: Vec<BlockDef<CreateBlockRole>> = Vec::new();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut push = |def: BlockDef<CreateBlockRole>, seen: &mut BTreeSet<String>| {
            if seen.insert(def.id.clone()) {
                out.push(def);
            }
        };

        let propeller_set: BTreeSet<&str> = self
            .propulsion
            .iter()
            .flat_map(|p| p.propeller_blocks.iter().map(String::as_str))
            .collect();

        for s in &self.kinetic_sources {
            push(
                BlockDef {
                    id: s.id.clone(),
                    properties: self.props_for(&s.id, &axis_props),
                    load_factor: 0.0,
                    capacity_factor: s.stress_capacity,
                    role: CreateBlockRole::Generator {
                        driven_rpm: s.base_rpm,
                    },
                },
                &mut seen,
            );
        }
        for c in &self.connectors {
            let powered = matches!(c.kind, ConnectorKind::Clutch | ConnectorKind::Gearshift);
            push(
                BlockDef {
                    id: c.id.clone(),
                    properties: self.props_for(
                        &c.id,
                        if powered {
                            axis_powered_props
                        } else {
                            axis_props
                        },
                    ),
                    load_factor: 0.0,
                    capacity_factor: 0.0,
                    role: c.kind.role(),
                },
                &mut seen,
            );
        }
        for c in &self.kinetic_consumers {
            let default_props = if propeller_set.contains(c.id.as_str()) {
                facing_props
            } else {
                axis_props
            };
            push(
                BlockDef {
                    id: c.id.clone(),
                    properties: self.props_for(&c.id, default_props),
                    load_factor: c.stress_impact,
                    capacity_factor: 0.0,
                    role: if self.offload.processor_recipe_types.contains_key(&c.id) {
                        CreateBlockRole::Processor
                    } else {
                        CreateBlockRole::StressConsumer
                    },
                },
                &mut seen,
            );
        }
        for b in &self.offload.blocks {
            if seen.contains(&b.id) {
                continue;
            }
            let role = match b.role {
                UniversalBlockRole::TransportPipe => CreateBlockRole::Transport,
                UniversalBlockRole::FluidPipe => CreateBlockRole::FluidPipe,
                UniversalBlockRole::FluidTank => CreateBlockRole::FluidTank,
                UniversalBlockRole::FluidPump => CreateBlockRole::FluidPump,
                UniversalBlockRole::Processor => CreateBlockRole::Processor,
                UniversalBlockRole::ContraptionAnchor => CreateBlockRole::ContraptionAnchor,
                _ => continue, // network roles stay universal-tier only
            };
            let def = BlockDef {
                id: b.id.clone(),
                properties: self.props_for(&b.id, &|| b.properties.clone()),
                load_factor: b.load_factor,
                capacity_factor: b.capacity_factor,
                role,
            };
            push(def, &mut seen);
        }
        if let Some(p) = &self.propulsion {
            for id in &p.propeller_blocks {
                push(
                    BlockDef {
                        id: id.clone(),
                        properties: self.props_for(id, facing_props),
                        load_factor: 8.0,
                        capacity_factor: 0.0,
                        role: CreateBlockRole::StressConsumer,
                    },
                    &mut seen,
                );
            }
            for id in p.buoyancy_blocks.iter().chain(&p.rudder_blocks) {
                push(
                    BlockDef {
                        id: id.clone(),
                        properties: self.props_for(id, empty_props),
                        load_factor: 0.0,
                        capacity_factor: 0.0,
                        role: CreateBlockRole::Decorative,
                    },
                    &mut seen,
                );
            }
        }
        for id in self
            .assembly_anchors
            .iter()
            .chain(&self.decorative_blocks)
            .chain(&self.hidden_blocks)
        {
            push(
                BlockDef {
                    id: id.clone(),
                    properties: self.props_for(id, empty_props),
                    load_factor: 0.0,
                    capacity_factor: 0.0,
                    role: if self.assembly_anchors.contains(id) {
                        CreateBlockRole::ContraptionAnchor
                    } else {
                        CreateBlockRole::Decorative
                    },
                },
                &mut seen,
            );
        }
        out
    }

    /// Keep `offload` synchronized: every Create-side block gains a
    /// universal-tier mirror (kinetic sources -> network generators, consumers
    /// -> network consumers/processors, connectors -> network cables,
    /// transport/fluid blocks -> their kernel roles). Explicit `offload`
    /// entries always win.
    pub fn finalize(&mut self) {
        let sources = self.kinetic_sources.clone();
        for s in sources {
            mirror_block(
                &mut self.offload.blocks,
                &s.id,
                0.0,
                s.stress_capacity,
                UniversalBlockRole::NetworkGenerator,
            );
        }
        for c in self.kinetic_consumers.clone() {
            let role = if self.offload.processor_recipe_types.contains_key(&c.id) {
                UniversalBlockRole::Processor
            } else {
                UniversalBlockRole::NetworkConsumer
            };
            mirror_block(&mut self.offload.blocks, &c.id, c.stress_impact, 0.0, role);
        }
        for c in self.connectors.clone() {
            mirror_block(
                &mut self.offload.blocks,
                &c.id,
                0.0,
                0.0,
                UniversalBlockRole::NetworkCable,
            );
        }
        for id in self.assembly_anchors.clone() {
            mirror_block(
                &mut self.offload.blocks,
                &id,
                0.0,
                0.0,
                UniversalBlockRole::ContraptionAnchor,
            );
        }
        for id in self.decorative_blocks.clone().into_iter().chain(self.hidden_blocks.clone()) {
            mirror_block(
                &mut self.offload.blocks,
                &id,
                0.0,
                0.0,
                UniversalBlockRole::Decorative,
            );
        }
        if let Some(p) = &self.propulsion {
            for (ids, role) in [
                (&p.propeller_blocks, UniversalBlockRole::NetworkConsumer),
                (&p.buoyancy_blocks, UniversalBlockRole::Decorative),
                (&p.rudder_blocks, UniversalBlockRole::Decorative),
            ] {
                for id in ids.clone() {
                    mirror_block(&mut self.offload.blocks, &id, 0.0, 0.0, role);
                }
            }
        }
    }

    fn props_for(
        &self,
        id: &str,
        default: &dyn Fn() -> Vec<PropertyDef>,
    ) -> Vec<PropertyDef> {
        self.block_properties
            .get(id)
            .cloned()
            .unwrap_or_else(default)
    }
}

/// Append a universal-tier mirror block unless `id` is already present.
fn mirror_block(
    blocks: &mut Vec<BlockDef<UniversalBlockRole>>,
    id: &str,
    load: f64,
    capacity: f64,
    role: UniversalBlockRole,
) {
    if !blocks.iter().any(|b| b.id == id) {
        blocks.push(BlockDef {
            id: id.to_string(),
            properties: Vec::new(),
            load_factor: load,
            capacity_factor: capacity,
            role,
        });
    }
}

// ---------------------------------------------------------------------------
// Zero-JVM JAR / datapack auto-discovery
// ---------------------------------------------------------------------------

/// How a discovered addon block maps onto the simulation tiers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DiscoveredClass {
    Source,
    Consumer,
    Processor,
    Connector(ConnectorKind),
    Belt,
    Transport,
    FluidPipe,
    FluidPump,
    FluidTank,
    Propeller,
    Buoyancy,
    Rudder,
    Anchor,
    Decorative,
}

/// Auto-discover a [`CreateAddonProfile`] for `mod_id` from data already
/// extracted by [`mod_adapter_core::data_loader`]: `data/<ns>/tags/block/*.json`
/// (and `tags/blocks/*.json`) member lists classify blocks by tag and name
/// heuristics, `assets/<ns>/blockstates/*.json` supplies real property tables,
/// and `data/<ns>/recipes/*.json` recipe types attach to discovered processors.
/// Unknown Create addons (Steam 'n' Rails, Crafts & Additions, Diesel
/// Generators, Big Cannons, Clockwork, ...) thereby register their blocks,
/// ores/chests, and mechanical/transport components without hand-authored Rust.
pub fn discover_create_addon_profile(mod_id: &str, data: &LoadedModData) -> CreateAddonProfile {
    let mut profile = CreateAddonProfile::new(mod_id);
    profile.block_properties = data
        .blockstates
        .iter()
        .filter(|(id, _)| id.starts_with(&format!("{mod_id}:")))
        .map(|(id, props)| (id.clone(), props.clone()))
        .collect();

    let mut classes: BTreeMap<String, DiscoveredClass> = BTreeMap::new();
    // Seed from every blockstate file under this namespace.
    for id in profile.block_properties.keys() {
        classes.insert(id.clone(), classify_path(id_path(id)));
    }
    // Fold in tag members: `data/<ns>/tags/block/*.json` and `tags/blocks/*`.
    for (tag, members) in &data.tags {
        let path = id_path(tag);
        let is_block_tag = path.starts_with("block/") || path.starts_with("blocks/");
        if !is_block_tag {
            continue;
        }
        let tag_class = classify_path(&path[6 + usize::from(path.starts_with("blocks/"))..]);
        for member in members {
            let member = member.strip_prefix('#').unwrap_or(member);
            if !member.starts_with(&format!("{mod_id}:")) {
                continue;
            }
            let member_class = classify_path(id_path(member));
            // A classifying tag (e.g. `<ns>:propellers`) is curated addon
            // intent and beats the bare name heuristic; a generic tag name
            // falls back to the member's own name class.
            let chosen = if tag_class != DiscoveredClass::Decorative {
                tag_class
            } else {
                member_class
            };
            classes.insert(member.to_string(), chosen);
        }
    }

    let mut spec = PropulsionSpec::aeronautics_default();
    for (id, class) in classes {
        match class {
            DiscoveredClass::Source => {
                profile.kinetic_sources.push(KineticSourceSpec {
                    id,
                    base_rpm: 64,
                    stress_capacity: 512.0,
                    axis: Axis::Y,
                });
            }
            DiscoveredClass::Consumer => profile.kinetic_consumers.push(KineticConsumerSpec {
                id,
                stress_impact: 4.0,
                min_rpm: 1.0,
            }),
            DiscoveredClass::Processor => {
                let recipe_types: Vec<String> = data
                    .recipes
                    .iter()
                    .filter(|r| {
                        r.recipe_type.starts_with(&format!("{mod_id}:"))
                            && recipe_type_matches_block(id_path(&r.recipe_type), id_path(&id))
                    })
                    .map(|r| r.recipe_type.clone())
                    .collect();
                profile.kinetic_consumers.push(KineticConsumerSpec {
                    id: id.clone(),
                    stress_impact: 4.0,
                    min_rpm: 1.0,
                });
                if !recipe_types.is_empty() {
                    profile
                        .offload
                        .processor_recipe_types
                        .insert(id.clone(), recipe_types);
                }
                profile.offload.blocks.push(BlockDef {
                    id: id.clone(),
                    properties: Vec::new(),
                    load_factor: 4.0,
                    capacity_factor: 0.0,
                    role: UniversalBlockRole::Processor,
                });
            }
            DiscoveredClass::Connector(kind) => profile.connectors.push(ConnectorSpec { id, kind }),
            DiscoveredClass::Belt => {
                profile.offload.blocks.push(BlockDef {
                    id: id.clone(),
                    properties: Vec::new(),
                    load_factor: 1.0,
                    capacity_factor: 0.0,
                    role: UniversalBlockRole::TransportPipe,
                });
                profile.offload.transport_delay_ticks.insert(id, 1);
            }
            DiscoveredClass::Transport => {
                profile.offload.blocks.push(BlockDef {
                    id: id.clone(),
                    properties: Vec::new(),
                    load_factor: 0.0,
                    capacity_factor: 0.0,
                    role: UniversalBlockRole::TransportPipe,
                });
                profile.offload.transport_delay_ticks.insert(id, 4);
            }
            DiscoveredClass::FluidPipe => {
                profile.offload.blocks.push(BlockDef {
                    id: id.clone(),
                    properties: Vec::new(),
                    load_factor: 0.0,
                    capacity_factor: 0.0,
                    role: UniversalBlockRole::FluidPipe,
                });
                profile.offload.tank_capacity_mb.insert(id, 1_000);
            }
            DiscoveredClass::FluidPump => {
                profile.offload.blocks.push(BlockDef {
                    id: id.clone(),
                    properties: Vec::new(),
                    load_factor: 4.0,
                    capacity_factor: 0.0,
                    role: UniversalBlockRole::FluidPump,
                });
                profile.offload.tank_capacity_mb.insert(id, 1_000);
            }
            DiscoveredClass::FluidTank => {
                profile.offload.blocks.push(BlockDef {
                    id: id.clone(),
                    properties: Vec::new(),
                    load_factor: 0.0,
                    capacity_factor: 0.0,
                    role: UniversalBlockRole::FluidTank,
                });
                profile.offload.tank_capacity_mb.insert(id, 8_000);
            }
            DiscoveredClass::Propeller => {
                profile.kinetic_consumers.push(KineticConsumerSpec {
                    id: id.clone(),
                    stress_impact: 8.0,
                    min_rpm: 4.0,
                });
                spec.propeller_blocks.push(id);
            }
            DiscoveredClass::Buoyancy => spec.buoyancy_blocks.push(id),
            DiscoveredClass::Rudder => spec.rudder_blocks.push(id),
            DiscoveredClass::Anchor => profile.assembly_anchors.push(id),
            DiscoveredClass::Decorative => profile.decorative_blocks.push(id),
        }
    }

    if !spec.propeller_blocks.is_empty()
        || !spec.buoyancy_blocks.is_empty()
        || !spec.rudder_blocks.is_empty()
    {
        profile.propulsion = Some(spec);
    }

    // Anti-xray hidden blocks: tag roots and name patterns mirror the bridge's
    // `mod_hidden_root_tag` / name-match rules.
    let mut hidden: BTreeSet<String> = BTreeSet::new();
    for (tag, members) in &data.tags {
        if !hidden_root_tag(tag) {
            continue;
        }
        for member in members {
            let member = member.strip_prefix('#').unwrap_or(member);
            if member.starts_with(&format!("{mod_id}:")) {
                hidden.insert(member.to_string());
            }
        }
    }
    for id in profile.block_properties.keys() {
        let path = id_path(id);
        if path.ends_with("_ore") || path.contains("chest") || path.contains("barrel") {
            hidden.insert(id.clone());
        }
    }
    profile.hidden_blocks = hidden.into_iter().collect();

    profile.finalize();
    profile
}

fn id_path(id: &str) -> &str {
    id.split_once(':').map(|(_, p)| p).unwrap_or(id)
}

/// Match an addon recipe type path to its processing block path by Create's
/// `block -> "<stem>ing"` convention (`rolling_mill`/`rolling`, `grinder`/
/// `grinding`, `mixer`/`mixing`, `press`/`pressing`, `crusher`/`crushing`).
fn recipe_type_matches_block(recipe_path: &str, block_path: &str) -> bool {
    if block_path.starts_with(recipe_path) || recipe_path.starts_with(block_path) {
        return true;
    }
    let stem = recipe_path.strip_suffix("ing").unwrap_or(recipe_path);
    !stem.is_empty() && (block_path.starts_with(stem) || stem.starts_with(block_path))
}

/// Name-heuristic classifier for unknown addon block paths. Order matters:
/// compound names (`propeller_bearing`, `large_cogwheel`, `gearbox`) are
/// checked before their generic substrings.
fn classify_path(path: &str) -> DiscoveredClass {
    let has = |needle: &str| path.contains(needle);
    if has("propeller") || has("thruster") {
        DiscoveredClass::Propeller
    } else if has("balloon") || has("envelope") || has("blimp") || has("levitat") || has("float") {
        DiscoveredClass::Buoyancy
    } else if has("rudder") || has("helm") {
        DiscoveredClass::Rudder
    } else if has("assembler") || has("bearing") {
        DiscoveredClass::Anchor
    } else if has("gearbox") {
        DiscoveredClass::Connector(ConnectorKind::Gearbox)
    } else if has("gearshift") {
        DiscoveredClass::Connector(ConnectorKind::Gearshift)
    } else if has("clutch") {
        DiscoveredClass::Connector(ConnectorKind::Clutch)
    } else if has("large_cogwheel") || has("large_cog") {
        DiscoveredClass::Connector(ConnectorKind::LargeCogwheel)
    } else if has("cogwheel") || has("cog") || has("gear") {
        DiscoveredClass::Connector(ConnectorKind::Cogwheel)
    } else if has("shaft") || has("axle") || has("driveshaft") {
        DiscoveredClass::Connector(ConnectorKind::Shaft)
    } else if has("motor") || has("engine") || has("energiser") || has("flywheel") {
        DiscoveredClass::Source
    } else if has("pump") {
        DiscoveredClass::FluidPump
    } else if has("pipe") {
        DiscoveredClass::FluidPipe
    } else if has("tank") || has("reservoir") {
        DiscoveredClass::FluidTank
    } else if has("belt") {
        DiscoveredClass::Belt
    } else if has("chute") || has("funnel") || has("depot") || has("hopper") {
        DiscoveredClass::Transport
    } else if has("crusher") || has("mill") || has("mixer") || has("press") || has("grinder")
        || has("saw") || has("roller")
    {
        DiscoveredClass::Processor
    } else if has("alternator") || has("coil") || has("fan") || has("brushes") {
        DiscoveredClass::Consumer
    } else {
        DiscoveredClass::Decorative
    }
}

/// Ore/chest tag roots the bridge folds into the anti-xray hidden set
/// (`#c:ores`, `#forge:ores`, `#c:chests`, `#forge:chests`, `*:barrels`).
fn hidden_root_tag(tag: &str) -> bool {
    let path = id_path(tag);
    let leaf = path.rsplit('/').next().unwrap_or(path);
    (path.starts_with("block/") || path.starts_with("blocks/"))
        && matches!(leaf, "ores" | "chests" | "barrels")
}

// ---------------------------------------------------------------------------
// Registry / factory integration
// ---------------------------------------------------------------------------

/// Build the flagship Create block registry with addon definitions merged in
/// after the core `create:*` catalog. Duplicate block ids fail closed via
/// [`RegistryError::DuplicateBlock`].
pub fn create_registry_with_addons(
    base_state_id: u32,
    addons: &[CreateAddonProfile],
) -> Result<CreateBlockRegistry, RegistryError> {
    let mut defs = core_create_block_defs();
    for addon in addons {
        defs.extend(addon.block_defs());
    }
    BlockRegistry::build(base_state_id, defs)
}

/// Merge every addon's universal `offload` half into one
/// [`ModOffloadProfile`] (`mod_id` = `"create_addons"`) for
/// [`mod_adapter_core::profile::UniversalModRuntime`].
pub fn merged_offload_profile(addons: &[CreateAddonProfile]) -> ModOffloadProfile {
    let mut merged = ModOffloadProfile {
        mod_id: "create_addons".to_string(),
        blocks: Vec::new(),
        transport_delay_ticks: BTreeMap::new(),
        tank_capacity_mb: BTreeMap::new(),
        processor_recipe_types: BTreeMap::new(),
    };
    for addon in addons {
        merged.extend(&addon.offload);
    }
    merged
}

fn json_obj<'a>(
    value: &'a JsonValue,
    what: &str,
) -> Result<&'a BTreeMap<String, JsonValue>, DataLoadError> {
    value
        .as_object()
        .ok_or_else(|| DataLoadError::Json(format!("{what} must be object")))
}

fn json_str<'a>(
    obj: &'a BTreeMap<String, JsonValue>,
    key: &str,
) -> Result<&'a str, DataLoadError> {
    obj.get(key)
        .and_then(JsonValue::as_str)
        .ok_or_else(|| DataLoadError::Json(format!("missing string '{key}'")))
}

fn json_f64(obj: &BTreeMap<String, JsonValue>, key: &str) -> Option<f64> {
    obj.get(key).and_then(JsonValue::as_f64)
}

fn axis_from_name(name: &str) -> Axis {
    Axis::from_name(name).unwrap_or(Axis::Y)
}

// ---------------------------------------------------------------------------
// Built-in reference profiles
// ---------------------------------------------------------------------------

/// Create: Aeronautics (`createaeronautics`): airship assembly anchor,
/// propeller bearings/thrusters, balloon envelopes, and rudder/helm control.
pub fn createaeronautics_profile() -> CreateAddonProfile {
    let mut p = CreateAddonProfile::new("createaeronautics");
    p.assembly_anchors = vec!["createaeronautics:airship_assembler".into()];
    p.kinetic_sources = vec![KineticSourceSpec {
        id: "createaeronautics:propeller_engine".into(),
        base_rpm: 64,
        stress_capacity: 2048.0,
        axis: Axis::X,
    }];
    p.kinetic_consumers = vec![
        KineticConsumerSpec {
            id: "createaeronautics:propeller_bearing".into(),
            stress_impact: 8.0,
            min_rpm: 4.0,
        },
        KineticConsumerSpec {
            id: "createaeronautics:gyroscopic_propeller_bearing".into(),
            stress_impact: 8.0,
            min_rpm: 1.0,
        },
        KineticConsumerSpec {
            id: "createaeronautics:torsion_spring".into(),
            stress_impact: 4.0,
            min_rpm: 0.0,
        },
        KineticConsumerSpec {
            id: "createaeronautics:stationary_potato_cannon".into(),
            stress_impact: 2.0,
            min_rpm: 0.0,
        },
    ];
    p.decorative_blocks = vec!["createaeronautics:compass_table".into()];
    let mut spec = PropulsionSpec::aeronautics_default();
    spec.propeller_blocks = vec![
        "createaeronautics:propeller_bearing".into(),
        "createaeronautics:gyroscopic_propeller_bearing".into(),
        "createaeronautics:propeller_engine".into(),
    ];
    spec.buoyancy_blocks = vec![
        "createaeronautics:balloon_envelope".into(),
        "createaeronautics:levitation_core".into(),
    ];
    spec.rudder_blocks = vec!["createaeronautics:rudder".into()];
    p.propulsion = Some(spec);
    p.block_properties = BTreeMap::from([
        (
            "createaeronautics:propeller_bearing".to_string(),
            vec![PropertyDef::new(
                "facing",
                &["north", "south", "east", "west", "up", "down"],
            )],
        ),
        (
            "createaeronautics:propeller_engine".to_string(),
            vec![PropertyDef::new("axis", &["x", "y", "z"])],
        ),
    ]);
    p.finalize();
    p
}

/// Create: Steam 'n' Rails (`railways`): trackside signals, couplers and
/// conductor blocks (all non-kinetic, decorative on the flagship tier).
pub fn railways_profile() -> CreateAddonProfile {
    let mut p = CreateAddonProfile::new("railways");
    p.decorative_blocks = vec![
        "railways:semaphore".into(),
        "railways:track_coupler".into(),
        "railways:conductor_whistle".into(),
        "railways:yellow_conductor_cap".into(),
    ];
    p.offload.blocks.push(BlockDef {
        id: "railways:portable_fuel_interface".into(),
        properties: Vec::new(),
        load_factor: 0.0,
        capacity_factor: 0.0,
        role: UniversalBlockRole::FluidTank,
    });
    p.offload
        .tank_capacity_mb
        .insert("railways:portable_fuel_interface".into(), 8_000);
    p.finalize();
    p
}

/// Create: Crafts & Additions (`createaddition`): electric motor source,
/// alternator/tesla coil consumers, rolling mill processor, connectors.
pub fn createaddition_profile() -> CreateAddonProfile {
    let mut p = CreateAddonProfile::new("createaddition");
    p.kinetic_sources = vec![KineticSourceSpec {
        id: "createaddition:electric_motor".into(),
        base_rpm: 96,
        stress_capacity: 4096.0,
        axis: Axis::Y,
    }];
    p.kinetic_consumers = vec![
        KineticConsumerSpec {
            id: "createaddition:alternator".into(),
            stress_impact: 8.0,
            min_rpm: 16.0,
        },
        KineticConsumerSpec {
            id: "createaddition:tesla_coil".into(),
            stress_impact: 16.0,
            min_rpm: 1.0,
        },
        KineticConsumerSpec {
            id: "createaddition:rolling_mill".into(),
            stress_impact: 4.0,
            min_rpm: 1.0,
        },
    ];
    p.decorative_blocks = vec![
        "createaddition:connector".into(),
        "createaddition:accumulator".into(),
        "createaddition:modular_accumulator".into(),
        "createaddition:digital_adapter".into(),
    ];
    p.offload
        .processor_recipe_types
        .insert("createaddition:rolling_mill".into(), vec!["createaddition:rolling".into()]);
    p.finalize();
    p
}

/// Create: New Age (`create_new_age`): tiered energiser sources and
/// generator-coil / carbon-brush consumers.
pub fn create_new_age_profile() -> CreateAddonProfile {
    let mut p = CreateAddonProfile::new("create_new_age");
    p.kinetic_sources = vec![
        KineticSourceSpec {
            id: "create_new_age:energiser_t1".into(),
            base_rpm: 64,
            stress_capacity: 2048.0,
            axis: Axis::Y,
        },
        KineticSourceSpec {
            id: "create_new_age:energiser_t2".into(),
            base_rpm: 128,
            stress_capacity: 8192.0,
            axis: Axis::Y,
        },
        KineticSourceSpec {
            id: "create_new_age:energiser_t3".into(),
            base_rpm: 256,
            stress_capacity: 32_768.0,
            axis: Axis::Y,
        },
    ];
    p.kinetic_consumers = vec![
        KineticConsumerSpec {
            id: "create_new_age:generator_coil".into(),
            stress_impact: 8.0,
            min_rpm: 1.0,
        },
        KineticConsumerSpec {
            id: "create_new_age:carbon_brushes".into(),
            stress_impact: 2.0,
            min_rpm: 1.0,
        },
    ];
    p.decorative_blocks = vec!["create_new_age:magnetite_block".into()];
    p.finalize();
    p
}

/// All built-in Create addon reference profiles in deterministic order.
pub fn builtin_create_addon_profiles() -> Vec<CreateAddonProfile> {
    vec![
        createaeronautics_profile(),
        createaddition_profile(),
        create_new_age_profile(),
        railways_profile(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data_loader::ItemFluidIdTable;
    use mod_adapter_core::data_loader::load_from_stored_zip;

    fn build_stored_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (name, data) in entries {
            out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
            out.extend_from_slice(&20u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(data);
        }
        out
    }

    #[test]
    fn builtin_profiles_merge_into_create_registry_and_offload() {
        let addons = builtin_create_addon_profiles();
        assert_eq!(
            addons.iter().map(|a| a.mod_id.as_str()).collect::<Vec<_>>(),
            ["createaeronautics", "createaddition", "create_new_age", "railways"]
        );
        let reg = create_registry_with_addons(
            crate::blockstates::DEFAULT_BASE_STATE_ID,
            &addons,
        )
        .unwrap();
        for id in [
            "createaeronautics:airship_assembler",
            "createaeronautics:propeller_bearing",
            "createaeronautics:balloon_envelope",
            "createaeronautics:rudder",
            "railways:semaphore",
            "createaddition:electric_motor",
            "createaddition:alternator",
            "create_new_age:energiser_t1",
            "create_new_age:magnetite_block",
        ] {
            assert!(reg.default_state(id).is_some(), "missing {id}");
        }
        // Universal mirror exposes the same addon to the generic tier.
        let merged = merged_offload_profile(&addons);
        let ids: BTreeSet<&str> = merged.blocks.iter().map(|b| b.id.as_str()).collect();
        assert!(ids.contains("createaddition:electric_motor"));
        assert!(ids.contains("createaeronautics:propeller_bearing"));
        assert_eq!(
            merged.processor_recipe_types["createaddition:rolling_mill"],
            vec!["createaddition:rolling".to_string()]
        );
    }

    #[test]
    fn json_profile_round_trips_create_specific_sections() {
        let profile = CreateAddonProfile::from_json(br#"{
          "mod_id": "democaddon",
          "blocks": [
            {"id": "democaddon:copper_pipe", "role": "fluid_pipe", "tank_capacity_mb": 500}
          ],
          "kinetic_sources": [{"id": "democaddon:motor", "rpm": 128, "capacity": 1024.0, "axis": "z"}],
          "kinetic_consumers": [{"id": "democaddon:mixer", "impact": 6.0, "min_rpm": 8.0}],
          "connectors": [{"id": "democaddon:chain_drive", "kind": "shaft"}],
          "assembly_anchors": ["democaddon:assembler"],
          "hidden_blocks": ["democaddon:tin_ore"],
          "propulsion": {
            "thrust_per_rpm": 42.0,
            "propeller_blocks": ["democaddon:fan_prop"],
            "buoyancy_blocks": ["democaddon:gas_cell"],
            "rudder_blocks": ["democaddon:tail_fin"]
          }
        }"#)
        .unwrap();
        assert_eq!(profile.mod_id, "democaddon");
        assert_eq!(profile.kinetic_sources[0].base_rpm, 128);
        assert_eq!(profile.kinetic_consumers[0].min_rpm, 8.0);
        assert_eq!(profile.connectors[0].kind, ConnectorKind::Shaft);
        let prop = profile.propulsion.as_ref().unwrap();
        assert_eq!(prop.thrust_per_rpm, 42.0);
        assert_eq!(prop.buoyancy_blocks, ["democaddon:gas_cell"]);
        // Offload mirror gained every Create-side block.
        let mirror: BTreeSet<&str> = profile.offload.blocks.iter().map(|b| b.id.as_str()).collect();
        for id in [
            "democaddon:copper_pipe",
            "democaddon:motor",
            "democaddon:mixer",
            "democaddon:chain_drive",
            "democaddon:assembler",
            "democaddon:tin_ore",
            "democaddon:fan_prop",
        ] {
            assert!(mirror.contains(id), "mirror missing {id}");
        }
    }

    #[test]
    fn unknown_addon_jar_autodiscovers_kinetics_transport_and_hidden_blocks() {
        // Synthetic unknown addon "skyaeronaut": tags + blockstates + recipes.
        let jar = build_stored_zip(&[
            (
                "data/skyaeronaut/tags/block/propellers.json",
                br#"{"values": ["skyaeronaut:rotor_bearing"]}"#,
            ),
            (
                "data/skyaeronaut/tags/blocks/buoyant.json",
                br#"{"values": ["skyaeronaut:gas_envelope"]}"#,
            ),
            (
                "data/c/tags/block/ores.json",
                br#"{"values": ["skyaeronaut:sky_iron_ore", "othermod:ore"]}"#,
            ),
            (
                "assets/skyaeronaut/blockstates/rotor_bearing.json",
                br#"{"variants": {"facing=north": {}, "facing=up": {}, "facing=down": {}, "facing=east": {}, "facing=west": {}, "facing=south": {}}}"#,
            ),
            (
                "assets/skyaeronaut/blockstates/driveshaft.json",
                br#"{"variants": {"axis=x": {}, "axis=y": {}, "axis=z": {}}}"#,
            ),
            (
                "assets/skyaeronaut/blockstates/aero_motor.json",
                br#"{"variants": {"axis=x": {}, "axis=y": {}, "axis=z": {}}}"#,
            ),
            (
                "assets/skyaeronaut/blockstates/fluid_tank.json",
                br#"{"variants": {"": {}}}"#,
            ),
            (
                "assets/skyaeronaut/blockstates/grinder.json",
                br#"{"variants": {"": {}}}"#,
            ),
            (
                "data/skyaeronaut/recipes/grinding/dust.json",
                br#"{"type": "skyaeronaut:grinding", "ingredients": [{"item": "minecraft:iron_ore"}], "results": [{"id": "skyaeronaut:iron_dust", "count": 2}]}"#,
            ),
        ]);
        let mut ids = ItemFluidIdTable::new();
        let data = load_from_stored_zip(&jar, &mut ids).unwrap();
        let profile = discover_create_addon_profile("skyaeronaut", &data);

        // Tag-driven propeller + blockstate-driven shaft/motor/tank/grinder.
        let propulsion = profile
            .propulsion
            .as_ref()
            .expect("propeller tag -> propulsion spec");
        assert_eq!(propulsion.propeller_blocks, ["skyaeronaut:rotor_bearing"]);
        assert_eq!(propulsion.buoyancy_blocks, ["skyaeronaut:gas_envelope"]);
        assert!(profile
            .connectors
            .iter()
            .any(|c| c.id == "skyaeronaut:driveshaft" && c.kind == ConnectorKind::Shaft));
        assert!(profile
            .kinetic_sources
            .iter()
            .any(|s| s.id == "skyaeronaut:aero_motor"));
        assert!(profile
            .offload
            .blocks
            .iter()
            .any(|b| b.id == "skyaeronaut:fluid_tank" && b.role == UniversalBlockRole::FluidTank));
        // Grinder matched its `<ns>:grinding` recipe type.
        assert_eq!(
            profile.offload.processor_recipe_types["skyaeronaut:grinder"],
            vec!["skyaeronaut:grinding".to_string()]
        );
        // Ore folded into hidden set; othermod ore ignored.
        assert_eq!(profile.hidden_blocks, ["skyaeronaut:sky_iron_ore"]);
        // Discovered blockstate properties drive registry expansion.
        assert_eq!(
            profile.block_properties["skyaeronaut:rotor_bearing"],
            vec![PropertyDef::new(
                "facing",
                &["down", "east", "north", "south", "up", "west"]
            )]
        );
        // Registry accepts the discovered blocks and expands real properties.
        let reg = create_registry_with_addons(
            crate::blockstates::DEFAULT_BASE_STATE_ID,
            &[profile],
        )
        .unwrap();
        let rotor = reg.default_state("skyaeronaut:rotor_bearing").unwrap();
        assert_eq!(
            reg.encode_canonical(rotor),
            Some("skyaeronaut:rotor_bearing[facing=down]")
        );
        assert!(reg.default_state("skyaeronaut:driveshaft").is_some());
    }
}
