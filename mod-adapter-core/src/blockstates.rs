// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Universal deterministic multi-property `BlockState` expansion and Save v2
//! canonical `"namespace:block[k1=v1,k2=v2]"` palette codec for ANY mod.

use std::collections::{BTreeMap, HashMap};

/// Default first dynamic state ID, matching `megu` S2 custom block space.
pub const DEFAULT_BASE_STATE_ID: u32 = 26_684;

/// Generic role tag for any mod block declared through `ModOffloadProfile`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UniversalBlockRole {
    #[default]
    Decorative,
    TransportPipe,
    FluidPipe,
    FluidTank,
    FluidPump,
    NetworkCable,
    NetworkGenerator,
    NetworkConsumer,
    Processor,
    ContraptionAnchor,
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
pub struct BlockDef<R = UniversalBlockRole> {
    pub id: String,
    pub properties: Vec<PropertyDef>,
    /// Generic load coefficient (e.g. SU/RPM, FE/tick, channel cost).
    pub load_factor: f64,
    /// Generic capacity coefficient (e.g. SU/RPM, FE/tick, channel supply).
    pub capacity_factor: f64,
    pub role: R,
}

/// One expanded concrete block state in the frozen registry.
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

/// Frozen multi-mod table of multi-property blocks and expanded state IDs.
#[derive(Clone, Debug)]
pub struct BlockRegistry<R = UniversalBlockRole> {
    base_state_id: u32,
    blocks: Vec<BlockDef<R>>,
    block_by_id: HashMap<String, usize>,
    block_default_state: Vec<u32>,
    states: Vec<BlockStateEntry>,
    state_by_canonical: HashMap<String, u32>,
}

impl<R> BlockRegistry<R> {
    /// Build a frozen registry from block definitions across any namespaces.
    /// Blocks sort canonically by `(namespace, path)` so load order never
    /// perturbs runtime IDs, and properties expand in NeoForge Cartesian order.
    pub fn build(base_state_id: u32, mut defs: Vec<BlockDef<R>>) -> Result<Self, RegistryError> {
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

    pub fn base_state_id(&self) -> u32 {
        self.base_state_id
    }

    pub fn block_count(&self) -> usize {
        self.blocks.len()
    }

    pub fn state_count(&self) -> usize {
        self.states.len()
    }

    pub fn blocks(&self) -> &[BlockDef<R>] {
        &self.blocks
    }

    pub fn states(&self) -> &[BlockStateEntry] {
        &self.states
    }

    pub fn state(&self, state_id: u32) -> Option<&BlockStateEntry> {
        let offset = state_id.checked_sub(self.base_state_id)? as usize;
        self.states.get(offset)
    }

    pub fn block_def(&self, block_id: &str) -> Option<&BlockDef<R>> {
        self.block_by_id.get(block_id).map(|&idx| &self.blocks[idx])
    }

    pub fn block_def_for_state(&self, state_id: u32) -> Option<&BlockDef<R>> {
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

    /// Return the state ID obtained by cycling one property to its next value.
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

pub fn split_rl(id: &str) -> (&str, &str) {
    id.split_once(':').unwrap_or(("minecraft", id))
}

pub fn is_valid_resource_location(id: &str) -> bool {
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

pub fn format_canonical(block_id: &str, properties: &[(String, String)]) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multi_namespace_blocks_sort_deterministically_and_remap_by_name() {
        let defs = vec![
            BlockDef {
                id: "mekanism:basic_cable".into(),
                properties: vec![PropertyDef::new("tier", &["basic", "advanced"])],
                load_factor: 0.0,
                capacity_factor: 1000.0,
                role: UniversalBlockRole::NetworkCable,
            },
            BlockDef {
                id: "ae2:fluix_cable".into(),
                properties: vec![PropertyDef::new("waterlogged", &["false", "true"])],
                load_factor: 1.0,
                capacity_factor: 8.0,
                role: UniversalBlockRole::NetworkCable,
            },
        ];
        let reg = BlockRegistry::build(DEFAULT_BASE_STATE_ID, defs).unwrap();
        assert_eq!(reg.block_count(), 2);
        assert_eq!(reg.state_count(), 4);
        let mek_adv = reg
            .decode_canonical("mekanism:basic_cable[tier=advanced]")
            .unwrap();
        assert_eq!(
            reg.encode_canonical(mek_adv),
            Some("mekanism:basic_cable[tier=advanced]")
        );
    }
}
