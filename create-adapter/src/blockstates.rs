// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Deterministic multi-property `BlockState` expansion and Save v2 canonical
//! name palette codec for Create 1.21.1 blocks.

use std::collections::{BTreeMap, HashMap};

/// Default first dynamic state ID, matching `mc-spine` S2 custom block space.
pub const DEFAULT_BASE_STATE_ID: u32 = 26_684;

/// Functional role of a Create block in the native simulation adapter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CreateBlockRole {
    Shaft,
    Cogwheel,
    LargeCogwheel,
    Gearbox,
    Clutch,
    Gearshift,
    Generator { driven_rpm: i32 },
    StressConsumer,
    Belt,
    Transport,
    Processor,
    FluidPipe,
    FluidPump,
    FluidTank,
    ContraptionAnchor,
    Sensor,
}

/// One Minecraft/NeoForge block state property (e.g. `axis` -> `["x", "y", "z"]`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PropertyDef {
    pub name: String,
    pub values: Vec<String>,
}

impl PropertyDef {
    pub fn new(name: impl Into<String>, values: &[&str]) -> Self {
        Self {
            name: name.into(),
            values: values.iter().map(|v| (*v).to_string()).collect(),
        }
    }
}

/// Definition of a custom block prior to Cartesian `BlockState` expansion.
#[derive(Clone, Debug, PartialEq)]
pub struct BlockDef {
    pub id: String,
    pub properties: Vec<PropertyDef>,
    /// Stress unit impact per RPM (SU / RPM).
    pub stress_impact: f64,
    /// Stress unit capacity per RPM (SU / RPM) for generators.
    pub stress_capacity: f64,
    pub role: CreateBlockRole,
}

/// One expanded concrete block state in the registry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockStateEntry {
    pub state_id: u32,
    pub block_index: usize,
    pub block_id: String,
    /// Property key/value pairs in definition order.
    pub properties: Vec<(String, String)>,
    /// Canonical Save v2 string: `"namespace:path[k1=v1,k2=v2]"` (sorted by key),
    /// or `"namespace:path"` when property-less.
    pub canonical: String,
}

impl BlockStateEntry {
    pub fn property(&self, name: &str) -> Option<&str> {
        self.properties
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegistryError {
    DuplicateBlock(String),
    EmptyPropertyValues { block: String, property: String },
    DuplicatePropertyName { block: String, property: String },
    InvalidBlockId(String),
    StateIdOverflow,
}

/// Frozen table of multi-property custom blocks and expanded state IDs.
#[derive(Clone, Debug)]
pub struct CreateBlockRegistry {
    base_state_id: u32,
    blocks: Vec<BlockDef>,
    block_by_id: HashMap<String, usize>,
    block_default_state: Vec<u32>,
    states: Vec<BlockStateEntry>,
    state_by_canonical: HashMap<String, u32>,
}

impl CreateBlockRegistry {
    /// Build a frozen registry from block definitions. Blocks are sorted
    /// canonically by `(namespace, path)` so registration order never perturbs
    /// runtime IDs, and properties expand in NeoForge Cartesian product order.
    pub fn build(base_state_id: u32, mut defs: Vec<BlockDef>) -> Result<Self, RegistryError> {
        for def in &defs {
            if !is_valid_resource_location(&def.id) {
                return Err(RegistryError::InvalidBlockId(def.id.clone()));
            }
            let mut seen_props = BTreeMap::new();
            for prop in &def.properties {
                if prop.values.is_empty() {
                    return Err(RegistryError::EmptyPropertyValues {
                        block: def.id.clone(),
                        property: prop.name.clone(),
                    });
                }
                if seen_props.insert(prop.name.clone(), ()).is_some() {
                    return Err(RegistryError::DuplicatePropertyName {
                        block: def.id.clone(),
                        property: prop.name.clone(),
                    });
                }
            }
        }

        defs.sort_by(|a, b| split_rl(&a.id).cmp(&split_rl(&b.id)));

        let mut block_by_id = HashMap::with_capacity(defs.len());
        let mut block_default_state = Vec::with_capacity(defs.len());
        let mut states = Vec::new();
        let mut state_by_canonical = HashMap::new();
        let mut next_id = base_state_id;

        for (block_index, def) in defs.iter().enumerate() {
            if block_by_id.insert(def.id.clone(), block_index).is_some() {
                return Err(RegistryError::DuplicateBlock(def.id.clone()));
            }
            block_default_state.push(next_id);

            let mut combos: Vec<Vec<(String, String)>> = vec![Vec::new()];
            for prop in &def.properties {
                let mut next = Vec::with_capacity(combos.len() * prop.values.len());
                for prefix in &combos {
                    for val in &prop.values {
                        let mut row = prefix.clone();
                        row.push((prop.name.clone(), val.clone()));
                        next.push(row);
                    }
                }
                combos = next;
            }

            for props in combos {
                let state_id = next_id;
                next_id = next_id
                    .checked_add(1)
                    .ok_or(RegistryError::StateIdOverflow)?;
                let canonical = format_canonical(&def.id, &props);
                state_by_canonical.insert(canonical.clone(), state_id);
                states.push(BlockStateEntry {
                    state_id,
                    block_index,
                    block_id: def.id.clone(),
                    properties: props,
                    canonical,
                });
            }
        }

        Ok(Self {
            base_state_id,
            blocks: defs,
            block_by_id,
            block_default_state,
            states,
            state_by_canonical,
        })
    }

    /// Build the default core Create 1.21.1 block catalog (23 blocks).
    pub fn core_create(base_state_id: u32) -> Self {
        Self::build(base_state_id, core_create_block_defs())
            .expect("built-in Create catalog must be valid")
    }

    pub fn base_state_id(&self) -> u32 {
        self.base_state_id
    }

    pub fn block_count(&self) -> usize {
        self.blocks.len()
    }

    pub fn state_count(&self) -> usize {
        self.states.len()
    }

    pub fn state(&self, state_id: u32) -> Option<&BlockStateEntry> {
        let offset = state_id.checked_sub(self.base_state_id)? as usize;
        self.states.get(offset)
    }

    pub fn block_def(&self, block_id: &str) -> Option<&BlockDef> {
        self.block_by_id.get(block_id).map(|&idx| &self.blocks[idx])
    }

    pub fn block_def_for_state(&self, state_id: u32) -> Option<&BlockDef> {
        let entry = self.state(state_id)?;
        Some(&self.blocks[entry.block_index])
    }

    pub fn default_state(&self, block_id: &str) -> Option<u32> {
        self.block_by_id
            .get(block_id)
            .map(|&idx| self.block_default_state[idx])
    }

    /// Encode a runtime `state_id` as its Save v2 canonical string.
    pub fn encode_canonical(&self, state_id: u32) -> Option<&str> {
        self.state(state_id).map(|entry| entry.canonical.as_str())
    }

    /// Resolve a Save v2 canonical string (accepting properties in any order)
    /// back to the current runtime `state_id`.
    pub fn decode_canonical(&self, text: &str) -> Option<u32> {
        if let Some(&id) = self.state_by_canonical.get(text) {
            return Some(id);
        }
        // Fallback: normalize unsorted property order in input string.
        let (block_id, rest) = text.split_once('[')?;
        let inner = rest.strip_suffix(']')?;
        let mut pairs = Vec::new();
        for part in inner.split(',') {
            let (k, v) = part.split_once('=')?;
            pairs.push((k.trim().to_string(), v.trim().to_string()));
        }
        let normalized = format_canonical(block_id.trim(), &pairs);
        self.state_by_canonical.get(&normalized).copied()
    }

    /// Return the state ID obtained by setting one property to `value`.
    pub fn with_property(&self, state_id: u32, prop: &str, value: &str) -> Option<u32> {
        let entry = self.state(state_id)?;
        let mut updated = entry.properties.clone();
        let slot = updated.iter_mut().find(|(k, _)| k == prop)?;
        slot.1 = value.to_string();
        let canonical = format_canonical(&entry.block_id, &updated);
        self.state_by_canonical.get(&canonical).copied()
    }

    /// Return the state ID obtained by cycling one property (e.g. Create wrench
    /// rotating `axis` `x -> y -> z -> x`).
    pub fn cycle_property(&self, state_id: u32, prop: &str) -> Option<u32> {
        let entry = self.state(state_id)?;
        let def = &self.blocks[entry.block_index];
        let pdef = def.properties.iter().find(|p| p.name == prop)?;
        let current = entry.property(prop)?;
        let pos = pdef.values.iter().position(|v| v == current)?;
        let next = &pdef.values[(pos + 1) % pdef.values.len()];
        self.with_property(state_id, prop, next)
    }
}

fn split_rl(id: &str) -> (&str, &str) {
    id.split_once(':').unwrap_or(("minecraft", id))
}

fn is_valid_resource_location(id: &str) -> bool {
    let Some((ns, path)) = id.split_once(':') else {
        return false;
    };
    !ns.is_empty()
        && !path.is_empty()
        && ns.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-' || b == b'.'
        })
        && path.bytes().all(|b| {
            b.is_ascii_lowercase()
                || b.is_ascii_digit()
                || b == b'_'
                || b == b'-'
                || b == b'.'
                || b == b'/'
        })
}

fn format_canonical(block_id: &str, properties: &[(String, String)]) -> String {
    if properties.is_empty() {
        return block_id.to_string();
    }
    let mut sorted: Vec<(&str, &str)> = properties
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    sorted.sort_by_key(|(k, _)| *k);
    let mut out = String::from(block_id);
    out.push('[');
    for (idx, (k, v)) in sorted.into_iter().enumerate() {
        if idx > 0 {
            out.push(',');
        }
        out.push_str(k);
        out.push('=');
        out.push_str(v);
    }
    out.push(']');
    out
}

/// Built-in Create 1.21.1 core block catalog (23 blocks covering kinetics,
/// belts/chutes, processing, fluids, and contraption anchors).
pub fn core_create_block_defs() -> Vec<BlockDef> {
    let axis = || PropertyDef::new("axis", &["x", "y", "z"]);
    let facing_all =
        || PropertyDef::new("facing", &["north", "south", "east", "west", "up", "down"]);
    let facing_horiz = || PropertyDef::new("facing", &["north", "south", "east", "west"]);
    let waterlogged = || PropertyDef::new("waterlogged", &["false", "true"]);
    let powered = || PropertyDef::new("powered", &["false", "true"]);

    vec![
        BlockDef {
            id: "create:shaft".into(),
            properties: vec![axis(), waterlogged()],
            stress_impact: 0.0,
            stress_capacity: 0.0,
            role: CreateBlockRole::Shaft,
        },
        BlockDef {
            id: "create:cogwheel".into(),
            properties: vec![axis(), waterlogged()],
            stress_impact: 0.0,
            stress_capacity: 0.0,
            role: CreateBlockRole::Cogwheel,
        },
        BlockDef {
            id: "create:large_cogwheel".into(),
            properties: vec![axis(), waterlogged()],
            stress_impact: 0.0,
            stress_capacity: 0.0,
            role: CreateBlockRole::LargeCogwheel,
        },
        BlockDef {
            id: "create:gearbox".into(),
            properties: vec![axis()],
            stress_impact: 0.0,
            stress_capacity: 0.0,
            role: CreateBlockRole::Gearbox,
        },
        BlockDef {
            id: "create:clutch".into(),
            properties: vec![axis(), powered()],
            stress_impact: 0.0,
            stress_capacity: 0.0,
            role: CreateBlockRole::Clutch,
        },
        BlockDef {
            id: "create:gearshift".into(),
            properties: vec![axis(), powered()],
            stress_impact: 0.0,
            stress_capacity: 0.0,
            role: CreateBlockRole::Gearshift,
        },
        BlockDef {
            id: "create:water_wheel".into(),
            properties: vec![axis()],
            stress_impact: 0.0,
            stress_capacity: 32.0,
            role: CreateBlockRole::Generator { driven_rpm: 8 },
        },
        BlockDef {
            id: "create:large_water_wheel".into(),
            properties: vec![axis()],
            stress_impact: 0.0,
            stress_capacity: 128.0,
            role: CreateBlockRole::Generator { driven_rpm: 4 },
        },
        BlockDef {
            id: "create:windmill_bearing".into(),
            properties: vec![facing_all()],
            stress_impact: 0.0,
            stress_capacity: 512.0,
            role: CreateBlockRole::Generator { driven_rpm: 16 },
        },
        BlockDef {
            id: "create:mechanical_belt".into(),
            properties: vec![
                facing_horiz(),
                PropertyDef::new("part", &["start", "middle", "end", "pulley"]),
                PropertyDef::new("slope", &["horizontal", "upward", "downward", "vertical"]),
                waterlogged(),
            ],
            stress_impact: 1.0,
            stress_capacity: 0.0,
            role: CreateBlockRole::Belt,
        },
        BlockDef {
            id: "create:depot".into(),
            properties: vec![waterlogged()],
            stress_impact: 0.0,
            stress_capacity: 0.0,
            role: CreateBlockRole::Transport,
        },
        BlockDef {
            id: "create:chute".into(),
            properties: vec![
                facing_horiz(),
                PropertyDef::new("shape", &["normal", "window", "intersection"]),
                waterlogged(),
            ],
            stress_impact: 0.0,
            stress_capacity: 0.0,
            role: CreateBlockRole::Transport,
        },
        BlockDef {
            id: "create:andesite_funnel".into(),
            properties: vec![
                facing_all(),
                PropertyDef::new("extracting", &["false", "true"]),
                powered(),
            ],
            stress_impact: 0.0,
            stress_capacity: 0.0,
            role: CreateBlockRole::Transport,
        },
        BlockDef {
            id: "create:mechanical_press".into(),
            properties: vec![facing_horiz()],
            stress_impact: 8.0,
            stress_capacity: 0.0,
            role: CreateBlockRole::Processor,
        },
        BlockDef {
            id: "create:mechanical_mixer".into(),
            properties: vec![],
            stress_impact: 4.0,
            stress_capacity: 0.0,
            role: CreateBlockRole::Processor,
        },
        BlockDef {
            id: "create:basin".into(),
            properties: vec![PropertyDef::new(
                "facing",
                &["down", "north", "south", "east", "west"],
            )],
            stress_impact: 0.0,
            stress_capacity: 0.0,
            role: CreateBlockRole::Processor,
        },
        BlockDef {
            id: "create:millstone".into(),
            properties: vec![],
            stress_impact: 4.0,
            stress_capacity: 0.0,
            role: CreateBlockRole::Processor,
        },
        BlockDef {
            id: "create:fluid_pipe".into(),
            properties: vec![
                PropertyDef::new("north", &["false", "true"]),
                PropertyDef::new("south", &["false", "true"]),
                PropertyDef::new("east", &["false", "true"]),
                PropertyDef::new("west", &["false", "true"]),
                PropertyDef::new("up", &["false", "true"]),
                PropertyDef::new("down", &["false", "true"]),
                waterlogged(),
            ],
            stress_impact: 0.0,
            stress_capacity: 0.0,
            role: CreateBlockRole::FluidPipe,
        },
        BlockDef {
            id: "create:mechanical_pump".into(),
            properties: vec![facing_all(), waterlogged()],
            stress_impact: 4.0,
            stress_capacity: 0.0,
            role: CreateBlockRole::FluidPump,
        },
        BlockDef {
            id: "create:fluid_tank".into(),
            properties: vec![
                PropertyDef::new("top", &["false", "true"]),
                PropertyDef::new("bottom", &["false", "true"]),
                PropertyDef::new(
                    "shape",
                    &[
                        "plain",
                        "window",
                        "window_nw",
                        "window_sw",
                        "window_ne",
                        "window_se",
                    ],
                ),
            ],
            stress_impact: 0.0,
            stress_capacity: 0.0,
            role: CreateBlockRole::FluidTank,
        },
        BlockDef {
            id: "create:spout".into(),
            properties: vec![],
            stress_impact: 0.0,
            stress_capacity: 0.0,
            role: CreateBlockRole::Processor,
        },
        BlockDef {
            id: "create:mechanical_bearing".into(),
            properties: vec![facing_all()],
            stress_impact: 4.0,
            stress_capacity: 0.0,
            role: CreateBlockRole::ContraptionAnchor,
        },
        BlockDef {
            id: "create:stressometer".into(),
            properties: vec![facing_horiz(), axis()],
            stress_impact: 0.0,
            stress_capacity: 0.0,
            role: CreateBlockRole::Sensor,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cartesian_expansion_and_save_v2_remapping_survive_reorder() {
        let base = CreateBlockRegistry::core_create(DEFAULT_BASE_STATE_ID);
        assert_eq!(base.block_count(), 23);
        // Shaft has 3 axes * 2 waterlogged = 6 states.
        let shaft_default = base.default_state("create:shaft").unwrap();
        let entry = base.state(shaft_default).unwrap();
        assert_eq!(entry.canonical, "create:shaft[axis=x,waterlogged=false]");
        let y_shaft = base.cycle_property(shaft_default, "axis").unwrap();
        assert_eq!(
            base.encode_canonical(y_shaft),
            Some("create:shaft[axis=y,waterlogged=false]")
        );
        let z_shaft = base.cycle_property(y_shaft, "axis").unwrap();
        assert_eq!(
            base.encode_canonical(z_shaft),
            Some("create:shaft[axis=z,waterlogged=false]")
        );
        assert_eq!(base.cycle_property(z_shaft, "axis"), Some(shaft_default));

        // Unsorted property text decodes cleanly too.
        assert_eq!(
            base.decode_canonical("create:shaft[waterlogged=false,axis=y]"),
            Some(y_shaft)
        );

        // Insert a brand-new addon block that sorts before create:shaft;
        // runtime state_id shifts, but Save v2 canonical string remaps cleanly!
        let mut extended_defs = core_create_block_defs();
        extended_defs.push(BlockDef {
            id: "create:aa_addon_axle".into(),
            properties: vec![PropertyDef::new("axis", &["x", "y", "z"])],
            stress_impact: 0.0,
            stress_capacity: 0.0,
            role: CreateBlockRole::Shaft,
        });
        let extended = CreateBlockRegistry::build(DEFAULT_BASE_STATE_ID, extended_defs).unwrap();
        let saved = base.encode_canonical(y_shaft).unwrap();
        let remapped = extended.decode_canonical(saved).unwrap();
        assert_ne!(remapped, y_shaft);
        assert_eq!(extended.encode_canonical(remapped), Some(saved));
    }
}
