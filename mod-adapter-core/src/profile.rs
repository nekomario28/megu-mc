// SPDX-License-Identifier: LGPL-3.0-or-later

//! Declarative universal mod offload profile (`ModOffloadProfile`) and generic
//! region-isolated runtime (`UniversalModRuntime`) allowing ANY mod's pipes,
//! cables, tanks, and recipe machines to run natively on `mod-offload` with
//! zero per-mod Rust code.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use mod_offload::fluid_network::TankState;
use mod_offload::kinetic_graph::{KineticNode, NetworkStatus, Solution};
use mod_offload::recipe_processor::{Drive, Recipe, Stack};
use mod_offload::region_scheduler::{
    BatchReport, RegionBudget, RegionKey, RegionReport, RegionScheduler, MAX_BATCH_BYTES,
    MAX_BATCH_RECORDS,
};

use crate::blockstates::{BlockDef, BlockRegistry, PropertyDef, RegistryError, UniversalBlockRole};
use crate::capability_bus::{
    BlockCapabilityBus, BlockCoord, EnergyTransferHandler, FluidTransferHandler,
    ItemTransferHandler, SimpleEnergyBuffer, SimpleFluidTank, SimpleSlotInventory,
};
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

    /// Fold another profile's blocks and tuning tables into this one.
    /// Blocks with an `id` already present are left untouched (first writer
    /// wins), so callers control precedence by merge order. Used to compose
    /// addon offload halves (e.g. every `CreateAddonProfile::offload`) into a
    /// single runtime manifest without hand-merging JSON.
    pub fn extend(&mut self, other: &ModOffloadProfile) {
        for def in &other.blocks {
            if !self.blocks.iter().any(|b| b.id == def.id) {
                self.blocks.push(def.clone());
            }
        }
        for (k, &v) in &other.transport_delay_ticks {
            self.transport_delay_ticks.entry(k.clone()).or_insert(v);
        }
        for (k, &v) in &other.tank_capacity_mb {
            self.tank_capacity_mb.entry(k.clone()).or_insert(v);
        }
        for (k, v) in &other.processor_recipe_types {
            self.processor_recipe_types
                .entry(k.clone())
                .or_insert_with(|| v.clone());
        }
    }

    /// Generic [`crate::capability_bus`] endpoints `block_id` exposes
    /// in-world under this profile: `FluidTank`/`FluidPipe`/`FluidPump`
    /// yield a [`SimpleFluidTank`] sized by `tank_capacity_mb`
    /// ([`DEFAULT_PROFILE_TANK_MB`] when undeclared), `TransportPipe` /
    /// `Processor` yield a [`SimpleSlotInventory`], and the
    /// `NetworkCable`/`NetworkGenerator`/`NetworkConsumer` family yields
    /// a [`SimpleEnergyBuffer`]. `Decorative` and `ContraptionAnchor`
    /// blocks expose nothing. Adapters register the returned endpoints at
    /// any [`BlockCoord`] — zero mod-specific wiring.
    pub fn capability_endpoints_for(&self, block_id: &str) -> ProfileCapabilityEndpoints {
        let mut out = ProfileCapabilityEndpoints::default();
        let Some(role) = self
            .blocks
            .iter()
            .find(|b| b.id == block_id)
            .map(|b| b.role)
        else {
            return out;
        };
        match role {
            UniversalBlockRole::FluidTank
            | UniversalBlockRole::FluidPipe
            | UniversalBlockRole::FluidPump => {
                let capacity = self
                    .tank_capacity_mb
                    .get(block_id)
                    .copied()
                    .unwrap_or(DEFAULT_PROFILE_TANK_MB);
                out.fluid = Some(Box::new(SimpleFluidTank::new(u64::from(capacity))));
            }
            UniversalBlockRole::TransportPipe | UniversalBlockRole::Processor => {
                out.item = Some(Box::new(SimpleSlotInventory::new(
                    PROFILE_INVENTORY_SLOTS,
                    PROFILE_MAX_STACK,
                )));
            }
            UniversalBlockRole::NetworkCable
            | UniversalBlockRole::NetworkGenerator
            | UniversalBlockRole::NetworkConsumer => {
                out.energy = Some(Box::new(SimpleEnergyBuffer::new(DEFAULT_PROFILE_ENERGY_FE)));
            }
            UniversalBlockRole::Decorative | UniversalBlockRole::ContraptionAnchor => {}
        }
        out
    }

    /// `true` when `block_id`'s declared role exposes at least one
    /// capability-bus endpoint — the cheap pre-check adapters run per
    /// block write before paying for [`Self::capability_endpoints_for`]'s
    /// endpoint objects. Decorative and contraption-anchor blocks answer
    /// `false`, matching the empty-endpoints contract.
    pub fn declares_capability_block(&self, block_id: &str) -> bool {
        self.blocks.iter().any(|b| {
            b.id == block_id
                && !matches!(
                    b.role,
                    UniversalBlockRole::Decorative | UniversalBlockRole::ContraptionAnchor
                )
        })
    }
}

/// Tank size used for profile fluid blocks without an explicit
/// `tank_capacity_mb` (the Mekanism basic tank's 16 000 mB).
pub const DEFAULT_PROFILE_TANK_MB: u32 = 16_000;
/// FE capacity of a generic profile energy endpoint.
pub const DEFAULT_PROFILE_ENERGY_FE: u64 = 10_000;
/// Slot count of a generic profile item endpoint (chest-like).
pub const PROFILE_INVENTORY_SLOTS: usize = 9;
/// Per-slot stack cap of a generic profile item endpoint.
pub const PROFILE_MAX_STACK: u32 = 64;

/// Generic capability-bus endpoints a profile block exposes in-world —
/// at most one of each kind, matching what a real block entity can
/// register under NeoForge `BlockCapabilities`.
#[derive(Default)]
pub struct ProfileCapabilityEndpoints {
    /// Item handler, when the role exposes one.
    pub item: Option<Box<dyn ItemTransferHandler + Send>>,
    /// Fluid handler, when the role exposes one.
    pub fluid: Option<Box<dyn FluidTransferHandler + Send>>,
    /// Energy handler, when the role exposes one.
    pub energy: Option<Box<dyn EnergyTransferHandler + Send>>,
}

impl ProfileCapabilityEndpoints {
    /// `true` when the block exposes no capabilities.
    pub fn is_empty(&self) -> bool {
        self.item.is_none() && self.fluid.is_none() && self.energy.is_none()
    }

    /// Register every present endpoint at `coord` on `bus`.
    pub fn register_into(self, bus: &mut BlockCapabilityBus, coord: &BlockCoord) {
        if let Some(handler) = self.item {
            bus.register_item_handler(coord.clone(), handler);
        }
        if let Some(handler) = self.fluid {
            bus.register_fluid_handler(coord.clone(), handler);
        }
        if let Some(handler) = self.energy {
            bus.register_energy_handler(coord.clone(), handler);
        }
    }
}

// ---------------------------------------------------------------------------
// Built-in reference declarative profiles (AE2 / Mekanism / Pipez-class mods)
// ---------------------------------------------------------------------------

/// Sample declarative profile for an AE2-class channel network mod:
/// a powered controller, lossy fluix cables, a channel-consuming storage
/// drive, and an inscriber-driven molecular assembler.
pub const AE2_CLASS_PROFILE_JSON: &[u8] = br#"{
  "mod_id": "ae2",
  "blocks": [
    {"id": "ae2:controller", "role": "network_generator", "capacity": 32.0},
    {"id": "ae2:fluix_cable", "role": "network_cable", "load": 0.5,
     "properties": {"waterlogged": ["false", "true"]}},
    {"id": "ae2:drive", "role": "network_consumer", "load": 2.0},
    {"id": "ae2:molecular_assembler", "role": "processor", "load": 1.0,
     "recipe_types": ["ae2:inscriber"]}
  ]
}"#;

/// Sample declarative profile for a Mekanism-class tech mod: a heat generator,
/// tiered universal cables, a crusher processor, and a pressurized
/// pipe/tank/pump fluid network.
pub const MEKANISM_CLASS_PROFILE_JSON: &[u8] = br#"{
  "mod_id": "mekanism",
  "blocks": [
    {"id": "mekanism:heat_generator", "role": "network_generator", "capacity": 200.0},
    {"id": "mekanism:universal_cable", "role": "network_cable",
     "properties": {"tier": ["basic", "advanced", "elite", "ultimate"]}},
    {"id": "mekanism:crusher", "role": "processor",
     "recipe_types": ["mekanism:crushing"]},
    {"id": "mekanism:mechanical_pipe", "role": "fluid_pipe", "tank_capacity_mb": 1000},
    {"id": "mekanism:basic_fluid_tank", "role": "fluid_tank", "tank_capacity_mb": 16000},
    {"id": "mekanism:electric_pump", "role": "fluid_pump", "tank_capacity_mb": 1000}
  ]
}"#;

/// Sample declarative profile for a Pipez-class logistics mod: sleeping
/// item pipes with a per-hop transfer delay plus small-buffer fluid pipes.
pub const PIPEZ_CLASS_PROFILE_JSON: &[u8] = br#"{
  "mod_id": "pipez",
  "blocks": [
    {"id": "pipez:item_pipe", "role": "transport_pipe", "transport_delay_ticks": 2,
     "properties": {
       "north": ["false", "true"], "south": ["false", "true"],
       "east": ["false", "true"], "west": ["false", "true"],
       "up": ["false", "true"], "down": ["false", "true"]
     }},
    {"id": "pipez:fluid_pipe", "role": "fluid_pipe", "tank_capacity_mb": 500}
  ]
}"#;

/// Parse the built-in AE2-class channel network profile.
pub fn ae2_class_profile() -> ModOffloadProfile {
    builtin_profile(AE2_CLASS_PROFILE_JSON)
}

/// Parse the built-in Mekanism-class power/fluid tech profile.
pub fn mekanism_class_profile() -> ModOffloadProfile {
    builtin_profile(MEKANISM_CLASS_PROFILE_JSON)
}

/// Parse the built-in Pipez-class sleeping pipe profile.
pub fn pipez_class_profile() -> ModOffloadProfile {
    builtin_profile(PIPEZ_CLASS_PROFILE_JSON)
}

/// All built-in reference profiles in deterministic `(namespace)` order.
pub fn builtin_reference_profiles() -> Vec<ModOffloadProfile> {
    vec![
        ae2_class_profile(),
        mekanism_class_profile(),
        pipez_class_profile(),
    ]
}

fn builtin_profile(json: &[u8]) -> ModOffloadProfile {
    ModOffloadProfile::from_json(json).expect("checked-in reference profile must parse")
}

/// Generic power-network profile for any `mod_id`: a `{mod}:power_generator`,
/// `{mod}:power_cable` and a speed-driven `{mod}:crusher` processor, mirroring
/// Mekanism/Thermal-class tech mods without any per-mod Rust code.
pub fn generic_power_profile(mod_id: &str) -> ModOffloadProfile {
    let crusher = format!("{mod_id}:crusher");
    ModOffloadProfile {
        mod_id: mod_id.to_string(),
        blocks: vec![
            profile_block(
                format!("{mod_id}:power_generator"),
                0.0,
                200.0,
                UniversalBlockRole::NetworkGenerator,
            ),
            profile_block(
                format!("{mod_id}:power_cable"),
                0.0,
                0.0,
                UniversalBlockRole::NetworkCable,
            ),
            profile_block(crusher.clone(), 1.0, 0.0, UniversalBlockRole::Processor),
        ],
        transport_delay_ticks: BTreeMap::new(),
        tank_capacity_mb: BTreeMap::new(),
        processor_recipe_types: BTreeMap::from([(crusher, vec![format!("{mod_id}:crushing")])]),
    }
}

/// Generic sleeping item-pipe chain profile for any `mod_id` (Pipez /
/// PrettyPipes-class): one `{mod}:item_pipe` transport node with a 2-tick
/// per-hop delay.
pub fn generic_item_pipe_profile(mod_id: &str) -> ModOffloadProfile {
    let pipe = format!("{mod_id}:item_pipe");
    ModOffloadProfile {
        mod_id: mod_id.to_string(),
        blocks: vec![profile_block(
            pipe.clone(),
            0.0,
            0.0,
            UniversalBlockRole::TransportPipe,
        )],
        transport_delay_ticks: BTreeMap::from([(pipe, 2)]),
        tank_capacity_mb: BTreeMap::new(),
        processor_recipe_types: BTreeMap::new(),
    }
}

/// Generic pressurized fluid network profile for any `mod_id`: a
/// `{mod}:fluid_tank` reservoir, `{mod}:fluid_pipe` segments, and a
/// `{mod}:pump` head source.
pub fn generic_fluid_profile(mod_id: &str) -> ModOffloadProfile {
    let tank = format!("{mod_id}:fluid_tank");
    let pipe = format!("{mod_id}:fluid_pipe");
    ModOffloadProfile {
        mod_id: mod_id.to_string(),
        blocks: vec![
            profile_block(tank.clone(), 0.0, 0.0, UniversalBlockRole::FluidTank),
            profile_block(pipe.clone(), 0.0, 0.0, UniversalBlockRole::FluidPipe),
            profile_block(
                format!("{mod_id}:pump"),
                0.0,
                0.0,
                UniversalBlockRole::FluidPump,
            ),
        ],
        transport_delay_ticks: BTreeMap::new(),
        tank_capacity_mb: BTreeMap::from([(tank, 16_000), (pipe, 1_000)]),
        processor_recipe_types: BTreeMap::new(),
    }
}

fn profile_block(
    id: String,
    load: f64,
    capacity: f64,
    role: UniversalBlockRole,
) -> BlockDef<UniversalBlockRole> {
    BlockDef {
        id,
        properties: Vec::new(),
        load_factor: load,
        capacity_factor: capacity,
        role,
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

    /// Link the transport pipe at `pos` downstream into the pipe at `next`.
    /// Both positions must hold `TransportPipe` nodes in the same region.
    pub fn link_transport(&mut self, pos: DimBlockPos, next: DimBlockPos) -> bool {
        if pos.region_key() != next.region_key() {
            return false;
        }
        let (Some(&from), Some(&to)) = (
            self.transport_nodes.get(&pos),
            self.transport_nodes.get(&next),
        ) else {
            return false;
        };
        self.scheduler
            .region_mut(pos.region_key())
            .set_downstream(from, Some(to))
            .is_ok()
    }

    /// Queue an item into the sleeping transport pipe at `pos`.
    pub fn insert_transport_item(&mut self, pos: DimBlockPos, item_id: u32) -> bool {
        let Some(&id) = self.transport_nodes.get(&pos) else {
            return false;
        };
        self.scheduler
            .region_mut(pos.region_key())
            .insert_item(id, item_id)
            .is_ok()
    }

    /// Take the oldest item collected in the transport pipe's local output FIFO.
    pub fn extract_transport_output(&mut self, pos: DimBlockPos) -> Option<u32> {
        let &id = self.transport_nodes.get(&pos)?;
        self.scheduler
            .region_mut(pos.region_key())
            .extract_output(id)
            .ok()
            .flatten()
    }

    /// Insert `amount_mb` of `fluid_id` into the tank/pipe/pump node at `pos`.
    pub fn insert_tank_fluid(
        &mut self,
        pos: DimBlockPos,
        fluid_id: u32,
        amount_mb: u32,
    ) -> Option<u32> {
        let &id = self.tank_nodes.get(&pos)?;
        self.scheduler
            .region_mut(pos.region_key())
            .insert_fluid(id, fluid_id, amount_mb)
            .ok()
    }

    /// Current `(fluid, amount_mb)` state of the tank/pipe/pump node at `pos`.
    pub fn tank_state(&self, pos: DimBlockPos) -> Option<TankState> {
        let &id = self.tank_nodes.get(&pos)?;
        self.scheduler.region(pos.region_key())?.fluid().tank(id)
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

    #[test]
    fn builtin_reference_profiles_parse_and_drive_sleeping_and_fluid_chains() {
        let profiles = builtin_reference_profiles();
        assert_eq!(
            profiles
                .iter()
                .map(|p| p.mod_id.as_str())
                .collect::<Vec<_>>(),
            ["ae2", "mekanism", "pipez"]
        );
        let mut rt = UniversalModRuntime::from_profiles(DEFAULT_BASE_STATE_ID, &profiles).unwrap();
        let mut ids = ItemFluidIdTable::new();
        let wheat = ids.intern_item("minecraft:wheat");
        let water = ids.intern_fluid("minecraft:water");

        let pipe_a = DimBlockPos::new(3, 0, 64, 0);
        let pipe_b = DimBlockPos::new(3, 1, 64, 0);
        let item_pipe = rt.registry().default_state("pipez:item_pipe").unwrap();
        assert!(rt.place_block(pipe_a, item_pipe, Vec::new()));
        assert!(rt.place_block(pipe_b, item_pipe, Vec::new()));
        assert!(rt.link_transport(pipe_a, pipe_b));
        assert!(rt.insert_transport_item(pipe_a, wheat));

        let tank_a = DimBlockPos::new(3, 0, 64, 4);
        let tank_b = DimBlockPos::new(3, 1, 64, 4);
        let tank = rt
            .registry()
            .default_state("mekanism:basic_fluid_tank")
            .unwrap();
        assert!(rt.place_block(tank_a, tank, Vec::new()));
        assert!(rt.place_block(tank_b, tank, Vec::new()));
        assert_eq!(rt.insert_tank_fluid(tank_a, water, 4000), Some(4000));

        let mut records = 0usize;
        for tick in 1..=6 {
            for out in rt.step_tick(tick, 128, None) {
                records += out.batch.records;
            }
        }
        assert!(records >= 4);
        assert_eq!(rt.extract_transport_output(pipe_b), Some(wheat));
        assert_eq!(rt.tank_state(tank_a).unwrap().amount, 2000);
        assert_eq!(rt.tank_state(tank_b).unwrap().amount, 2000);
    }

    #[test]
    fn generic_pattern_profiles_compose_for_arbitrary_mod_ids() {
        let profiles = vec![
            generic_power_profile("thermalexp"),
            generic_item_pipe_profile("prettypipes"),
            generic_fluid_profile("fluidics"),
        ];
        assert!(ModOffloadProfile::from_json(AE2_CLASS_PROFILE_JSON).is_ok());
        assert!(ModOffloadProfile::from_json(MEKANISM_CLASS_PROFILE_JSON).is_ok());
        assert!(ModOffloadProfile::from_json(PIPEZ_CLASS_PROFILE_JSON).is_ok());
        let mut rt = UniversalModRuntime::from_profiles(DEFAULT_BASE_STATE_ID, &profiles).unwrap();
        let gen = rt
            .registry()
            .default_state("thermalexp:power_generator")
            .unwrap();
        let cable = rt
            .registry()
            .default_state("thermalexp:power_cable")
            .unwrap();
        rt.place_block(DimBlockPos::new(0, 0, 64, 0), gen, Vec::new());
        rt.place_block(DimBlockPos::new(0, 1, 64, 0), cable, Vec::new());
        assert!(rt.network_running(DimBlockPos::new(0, 1, 64, 0)));
    }
}
