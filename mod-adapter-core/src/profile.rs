// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Declarative universal mod offload profile (`ModOffloadProfile`) and generic
//! region-isolated runtime (`UniversalModRuntime`) allowing ANY mod's pipes,
//! cables, tanks, and recipe machines to run natively on `mod-offload` with
//! zero per-mod Rust code.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use mod_offload::kinetic_graph::{KineticNode, NetworkStatus, Solution};
use mod_offload::recipe_processor::{Drive, Recipe, Stack};
use mod_offload::region_scheduler::{
    BatchReport, RegionBudget, RegionKey, RegionReport, RegionScheduler, MAX_BATCH_BYTES,
    MAX_BATCH_RECORDS,
};

use crate::blockstates::{BlockDef, BlockRegistry, PropertyDef, RegistryError, UniversalBlockRole};
use crate::data_loader::{parse_json, DataLoadError, JsonValue};

/// World coordinate with arbitrary dynamic dimension ID (`0=Overworld`, `3+=custom`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DimBlockPos {
    pub dim: i32,
    pub x: i32,
    pub y: i32,
    pub z: i32,
}

impl DimBlockPos {
    pub const fn new(dim: i32, x: i32, y: i32, z: i32) -> Self {
        Self { dim, x, y, z }
    }

    pub fn region_key(self) -> RegionKey {
        RegionKey::for_block(self.dim, self.x, self.z)
    }
}

/// Declarative JSON offload profile describing any mod's blocks and how they
/// map onto `mod_offload`'s 6 generic simulation kernels.
#[derive(Clone, Debug)]
pub struct ModOffloadProfile {
    pub mod_id: String,
    pub blocks: Vec<BlockDef<UniversalBlockRole>>,
    pub transport_delay_ticks: BTreeMap<String, u64>,
    pub tank_capacity_mb: BTreeMap<String, u32>,
    pub processor_recipe_types: BTreeMap<String, Vec<String>>,
}

impl ModOffloadProfile {
    /// Parse a declarative offload profile JSON document.
    pub fn from_json(bytes: &[u8]) -> Result<Self, DataLoadError> {
        let root = parse_json(bytes)?;
        let obj = root
            .as_object()
            .ok_or_else(|| DataLoadError::Json("profile root must be object".into()))?;
        let mod_id = obj
            .get("mod_id")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| DataLoadError::Json("missing string 'mod_id'".into()))?
            .to_string();

        let mut blocks = Vec::new();
        let mut transport_delay_ticks = BTreeMap::new();
        let mut tank_capacity_mb = BTreeMap::new();
        let mut processor_recipe_types = BTreeMap::new();

        if let Some(arr) = obj.get("blocks").and_then(JsonValue::as_array) {
            for item in arr {
                let bobj = item
                    .as_object()
                    .ok_or_else(|| DataLoadError::Json("block entry must be object".into()))?;
                let id = bobj
                    .get("id")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| DataLoadError::Json("block missing 'id'".into()))?
                    .to_string();
                let role = parse_role(
                    bobj.get("role")
                        .and_then(JsonValue::as_str)
                        .unwrap_or("decorative"),
                );
                let load_factor = bobj.get("load").and_then(JsonValue::as_f64).unwrap_or(0.0);
                let capacity_factor = bobj
                    .get("capacity")
                    .and_then(JsonValue::as_f64)
                    .unwrap_or(0.0);
                let mut properties = Vec::new();
                if let Some(pobj) = bobj.get("properties").and_then(JsonValue::as_object) {
                    for (pname, pvals) in pobj {
                        if let Some(vals) = pvals.as_array() {
                            let parsed_vals: Vec<String> = vals
                                .iter()
                                .filter_map(JsonValue::as_str)
                                .map(str::to_string)
                                .collect();
                            if !parsed_vals.is_empty() {
                                properties.push(PropertyDef {
                                    name: pname.clone(),
                                    values: parsed_vals,
                                });
                            }
                        }
                    }
                }
                if let Some(delay) = bobj
                    .get("transport_delay_ticks")
                    .and_then(JsonValue::as_u64)
                {
                    transport_delay_ticks.insert(id.clone(), delay.max(1));
                }
                if let Some(cap) = bobj
                    .get("tank_capacity_mb")
                    .and_then(JsonValue::as_u64)
                    .and_then(|v| u32::try_from(v).ok())
                {
                    tank_capacity_mb.insert(id.clone(), cap.max(1));
                }
                if let Some(rtypes) = bobj.get("recipe_types").and_then(JsonValue::as_array) {
                    let list: Vec<String> = rtypes
                        .iter()
                        .filter_map(JsonValue::as_str)
                        .map(str::to_string)
                        .collect();
                    if !list.is_empty() {
                        processor_recipe_types.insert(id.clone(), list);
                    }
                }
                blocks.push(BlockDef {
                    id,
                    properties,
                    load_factor,
                    capacity_factor,
                    role,
                });
            }
        }

        Ok(Self {
            mod_id,
            blocks,
            transport_delay_ticks,
            tank_capacity_mb,
            processor_recipe_types,
        })
    }
}

fn parse_role(text: &str) -> UniversalBlockRole {
    match text {
        "transport_pipe" => UniversalBlockRole::TransportPipe,
        "fluid_pipe" => UniversalBlockRole::FluidPipe,
        "fluid_tank" => UniversalBlockRole::FluidTank,
        "fluid_pump" => UniversalBlockRole::FluidPump,
        "network_cable" => UniversalBlockRole::NetworkCable,
        "network_generator" => UniversalBlockRole::NetworkGenerator,
        "network_consumer" => UniversalBlockRole::NetworkConsumer,
        "processor" => UniversalBlockRole::Processor,
        "contraption_anchor" => UniversalBlockRole::ContraptionAnchor,
        _ => UniversalBlockRole::Decorative,
    }
}

/// Generic multi-mod native runtime executing blocks from ANY number of
/// [`ModOffloadProfile`] manifests on `mod-offload`'s `RegionScheduler`.
pub struct UniversalModRuntime {
    registry: BlockRegistry<UniversalBlockRole>,
    scheduler: RegionScheduler,
    active_regions: BTreeSet<RegionKey>,
    transport_delays: BTreeMap<String, u64>,
    tank_capacities: BTreeMap<String, u32>,
    next_id: u32,
    network_nodes: BTreeMap<DimBlockPos, u32>,
    transport_nodes: BTreeMap<DimBlockPos, u32>,
    tank_nodes: BTreeMap<DimBlockPos, u32>,
    processor_nodes: BTreeMap<DimBlockPos, u32>,
    scratch: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct UniversalStepOutput {
    pub region: RegionKey,
    pub report: RegionReport,
    pub batch: BatchReport,
}

impl UniversalModRuntime {
    pub fn from_profiles(
        base_state_id: u32,
        profiles: &[ModOffloadProfile],
    ) -> Result<Self, RegistryError> {
        let mut all_blocks = Vec::new();
        let mut transport_delays = BTreeMap::new();
        let mut tank_capacities = BTreeMap::new();
        for p in profiles {
            all_blocks.extend(p.blocks.iter().cloned());
            for (k, &v) in &p.transport_delay_ticks {
                transport_delays.insert(k.clone(), v);
            }
            for (k, &v) in &p.tank_capacity_mb {
                tank_capacities.insert(k.clone(), v);
            }
        }
        let registry = BlockRegistry::build(base_state_id, all_blocks)?;
        Ok(Self {
            registry,
            scheduler: RegionScheduler::default(),
            active_regions: BTreeSet::new(),
            transport_delays,
            tank_capacities,
            next_id: 1,
            network_nodes: BTreeMap::new(),
            transport_nodes: BTreeMap::new(),
            tank_nodes: BTreeMap::new(),
            processor_nodes: BTreeMap::new(),
            scratch: vec![0u8; MAX_BATCH_BYTES],
        })
    }

    pub fn registry(&self) -> &BlockRegistry<UniversalBlockRole> {
        &self.registry
    }

    pub fn scheduler(&self) -> &RegionScheduler {
        &self.scheduler
    }

    /// Place any profile-declared block at `pos`, automatically attaching it
    /// to its 6-connected same-region network (cable/fluid/transport).
    pub fn place_block(&mut self, pos: DimBlockPos, state_id: u32, recipes: Vec<Recipe>) -> bool {
        let Some(def) = self.registry.block_def_for_state(state_id).cloned() else {
            return false;
        };
        let key = pos.region_key();
        self.active_regions.insert(key);
        let id = self.next_id;
        self.next_id += 1;

        match def.role {
            UniversalBlockRole::NetworkCable
            | UniversalBlockRole::NetworkGenerator
            | UniversalBlockRole::NetworkConsumer => {
                let driven =
                    matches!(def.role, UniversalBlockRole::NetworkGenerator).then_some(1.0);
                let node = KineticNode {
                    capacity: def.capacity_factor,
                    load: def.load_factor,
                    driven_speed: driven,
                };
                let region = self.scheduler.region_mut(key);
                let _ = region.kinetic.add_node(id, node);
                self.network_nodes.insert(pos, id);
                for npos in six_neighbors(pos) {
                    if npos.region_key() == key {
                        if let Some(&nid) = self.network_nodes.get(&npos) {
                            let _ = region.kinetic.connect(id, nid, 1.0);
                        }
                    }
                }
            }
            UniversalBlockRole::TransportPipe => {
                let delay = self.transport_delays.get(&def.id).copied().unwrap_or(4);
                let region = self.scheduler.region_mut(key);
                let _ = region.add_machine(id, 8, delay);
                self.transport_nodes.insert(pos, id);
            }
            UniversalBlockRole::FluidTank
            | UniversalBlockRole::FluidPipe
            | UniversalBlockRole::FluidPump => {
                let cap = self.tank_capacities.get(&def.id).copied().unwrap_or(4000);
                let head = if def.role == UniversalBlockRole::FluidPump {
                    1000
                } else {
                    0
                };
                let region = self.scheduler.region_mut(key);
                let _ = region.add_tank(id, cap, pos.y);
                self.tank_nodes.insert(pos, id);
                for npos in six_neighbors(pos) {
                    if npos.region_key() == key {
                        if let Some(&nid) = self.tank_nodes.get(&npos) {
                            let _ = region.set_fluid_link(id, nid, Some(head));
                        }
                    }
                }
            }
            UniversalBlockRole::Processor => {
                let region = self.scheduler.region_mut(key);
                let _ = region.add_processor(
                    id,
                    64,
                    64,
                    recipes,
                    Drive::Mechanical {
                        rpm: 16,
                        overstressed: false,
                    },
                );
                self.processor_nodes.insert(pos, id);
            }
            UniversalBlockRole::Decorative | UniversalBlockRole::ContraptionAnchor => {}
        }
        true
    }

    pub fn network_solution(&self, pos: DimBlockPos) -> Option<Solution> {
        let &id = self.network_nodes.get(&pos)?;
        self.scheduler
            .region(pos.region_key())?
            .kinetic
            .solution(id)
    }

    pub fn network_running(&self, pos: DimBlockPos) -> bool {
        self.network_solution(pos)
            .is_some_and(|s| s.status == NetworkStatus::Running)
    }

    pub fn insert_processor_input(&mut self, pos: DimBlockPos, stack: Stack) -> Option<u32> {
        let &id = self.processor_nodes.get(&pos)?;
        self.scheduler
            .region_mut(pos.region_key())
            .insert_ingredient(id, stack)
            .ok()
    }

    pub fn extract_processor_output(&mut self, pos: DimBlockPos, stack: Stack) -> Option<u32> {
        let &id = self.processor_nodes.get(&pos)?;
        self.scheduler
            .region_mut(pos.region_key())
            .extract_product(id, stack)
            .ok()
    }

    pub fn step_tick(
        &mut self,
        tick: u64,
        max_steps_per_region: usize,
        max_us_per_region: Option<u64>,
    ) -> Vec<UniversalStepOutput> {
        let budget = RegionBudget {
            max_steps: max_steps_per_region,
            max_time: max_us_per_region.map(Duration::from_micros),
        };
        let mut outs = Vec::new();
        for &key in &self.active_regions {
            let region = self.scheduler.region_mut(key);
            if let Ok(report) = region.advance_to(tick, budget) {
                if let Ok(batch) = region.export_into(&mut self.scratch, MAX_BATCH_RECORDS) {
                    if report.processed > 0 || batch.records > 0 {
                        outs.push(UniversalStepOutput {
                            region: key,
                            report,
                            batch,
                        });
                    }
                }
            }
        }
        outs
    }
}

fn six_neighbors(pos: DimBlockPos) -> [DimBlockPos; 6] {
    [
        DimBlockPos::new(pos.dim, pos.x + 1, pos.y, pos.z),
        DimBlockPos::new(pos.dim, pos.x - 1, pos.y, pos.z),
        DimBlockPos::new(pos.dim, pos.x, pos.y + 1, pos.z),
        DimBlockPos::new(pos.dim, pos.x, pos.y - 1, pos.z),
        DimBlockPos::new(pos.dim, pos.x, pos.y, pos.z + 1),
        DimBlockPos::new(pos.dim, pos.x, pos.y, pos.z - 1),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blockstates::DEFAULT_BASE_STATE_ID;
    use crate::data_loader::{parse_mod_recipe_json, ItemFluidIdTable};
    use mod_offload::recipe_processor::{Resource, ResourceKind};

    #[test]
    fn declarative_profile_accelerates_arbitrary_tech_mod_without_mod_specific_rust() {
        let profile_json = br#"{
          "mod_id": "mekanism",
          "blocks": [
            {"id": "mekanism:heat_generator", "role": "network_generator", "capacity": 200.0},
            {"id": "mekanism:universal_cable", "role": "network_cable"},
            {"id": "mekanism:crusher", "role": "processor", "recipe_types": ["mekanism:crushing"]}
          ]
        }"#;
        let profile = ModOffloadProfile::from_json(profile_json).unwrap();
        let mut rt = UniversalModRuntime::from_profiles(DEFAULT_BASE_STATE_ID, &[profile]).unwrap();

        let gen = rt
            .registry()
            .default_state("mekanism:heat_generator")
            .unwrap();
        let cable = rt
            .registry()
            .default_state("mekanism:universal_cable")
            .unwrap();
        let crusher = rt.registry().default_state("mekanism:crusher").unwrap();

        let mut ids = ItemFluidIdTable::new();
        let recipe = parse_mod_recipe_json(
            "mekanism:crushing/bio_fuel",
            1,
            br#"{"type":"mekanism:crushing","input":{"item":"minecraft:wheat"},"output":{"id":"mekanism:bio_fuel","count":2},"processing_time":5}"#,
            &mut ids,
        )
        .unwrap()
        .unwrap();

        rt.place_block(DimBlockPos::new(0, 0, 64, 0), gen, Vec::new());
        rt.place_block(DimBlockPos::new(0, 1, 64, 0), cable, Vec::new());
        assert!(rt.network_running(DimBlockPos::new(0, 1, 64, 0)));

        let p_crush = DimBlockPos::new(0, 2, 64, 0);
        rt.place_block(p_crush, crusher, vec![recipe.recipe]);
        rt.insert_processor_input(
            p_crush,
            Stack {
                resource: Resource {
                    kind: ResourceKind::Item,
                    id: ids.item_id("minecraft:wheat").unwrap(),
                },
                amount: 1,
            },
        );
        for tick in 1..=6 {
            rt.step_tick(tick, 128, None);
        }
        let produced = rt
            .extract_processor_output(
                p_crush,
                Stack {
                    resource: Resource {
                        kind: ResourceKind::Item,
                        id: ids.item_id("mekanism:bio_fuel").unwrap(),
                    },
                    amount: 2,
                },
            )
            .unwrap();
        assert_eq!(produced, 2);
    }
}
