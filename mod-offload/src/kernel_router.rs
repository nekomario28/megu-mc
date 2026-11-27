// SPDX-License-Identifier: LGPL-3.0-or-later

//! Structural routing from mod feature descriptions into shared simulation kernels.

use std::collections::BTreeMap;

use crate::{
    combat_motion::{ActionSpec, ActiveAction, Hand},
    contraption::{BlockCluster, Contraption, Pose},
    fluid_network::FluidNetwork,
    kinetic_graph::{KineticGraph, KineticNode},
    lazy_catchup::{catch_up_furnace, CatchUpLimits, FurnaceCatchUpState},
    me_network::{MeChannelGraph, MeNodeKind},
    recipe_processor::{Drive, Recipe, RecipeError, RecipeProcessor, Stack},
    sleeping_transport::SleepingTransport,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum GraphDomain {
    Kinetic,
    Fluid,
    StorageMe,
    Energy,
    Mana,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum UniversalKernelKind {
    GraphNetwork(GraphDomain),
    DueProcessMachine,
    Subworld6Dof,
    ConservationTransfer,
    TrackGraph,
    TimelineAction,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModFeatureDescriptor {
    pub feature_id: String,
    pub has_network_ports: bool,
    pub network_domain: Option<GraphDomain>,
    pub processing_recipes: bool,
    pub sleep_on_idle: bool,
    pub subworld_assembly: bool,
    pub resource_ports: bool,
    pub track_node: bool,
    pub action_timeline: bool,
    pub unsupported_custom_hooks: Vec<String>,
}

impl ModFeatureDescriptor {
    pub fn new(feature_id: impl Into<String>) -> Self {
        Self {
            feature_id: feature_id.into(),
            has_network_ports: false,
            network_domain: None,
            processing_recipes: false,
            sleep_on_idle: false,
            subworld_assembly: false,
            resource_ports: false,
            track_node: false,
            action_timeline: false,
            unsupported_custom_hooks: Vec::new(),
        }
    }
}

/// Declarative structural profile; names are labels only and never affect routing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeatureProfile {
    pub feature_id: String,
    pub network_domain: Option<GraphDomain>,
    pub processing_recipes: bool,
    pub sleep_on_idle: bool,
    pub subworld_assembly: bool,
    pub resource_ports: bool,
    pub track_node: bool,
    pub action_timeline: bool,
}

impl FeatureProfile {
    pub fn lower(&self) -> ModFeatureDescriptor {
        let mut d = ModFeatureDescriptor::new(self.feature_id.clone());
        d.has_network_ports = self.network_domain.is_some();
        d.network_domain = self.network_domain;
        d.processing_recipes = self.processing_recipes;
        d.sleep_on_idle = self.sleep_on_idle;
        d.subworld_assembly = self.subworld_assembly;
        d.resource_ports = self.resource_ports;
        d.track_node = self.track_node;
        d.action_timeline = self.action_timeline;
        d
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeclarativeProfile {
    pub features: Vec<FeatureProfile>,
}
impl DeclarativeProfile {
    pub fn lower(&self) -> Vec<ModFeatureDescriptor> {
        self.features.iter().map(FeatureProfile::lower).collect()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RouteHandle(pub u32);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoutedBinding {
    pub mod_id: String,
    pub feature_id: String,
    pub handle: RouteHandle,
    pub target: UniversalKernelKind,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnroutedFeature {
    pub mod_id: String,
    pub feature_id: String,
    pub reason: String,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KernelRouteReport {
    pub routed: Vec<RoutedBinding>,
    pub unrouted_features: Vec<UnroutedFeature>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RoutingBudget {
    pub max_features_per_mod: usize,
    pub max_total_routes: usize,
    pub max_nodes_per_tick: usize,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouterError {
    BudgetExceeded,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RouterTickStats {
    pub sleeping: usize,
    pub woken: usize,
    pub routed_nodes_ticked: usize,
    pub budget_exhausted: bool,
}

#[derive(Default)]
struct Runtime {
    transport: Option<SleepingTransport>,
    recipes: Option<RecipeProcessor>,
    kinetic: Option<KineticGraph>,
    kinetic_dirty: bool,
    fluid: Option<FluidNetwork>,
    me: Option<MeChannelGraph>,
    me_dirty: bool,
    contraptions: Vec<Contraption>,
    actions: Vec<ActiveAction>,
    furnaces: Vec<(FurnaceCatchUpState, u64)>,
}

fn furnace_can_progress(state: &FurnaceCatchUpState) -> bool {
    state.current_burn_remaining > 0
        || (state.input_count > 0
            && state.output_count < state.output_max_stack
            && state.fuel_count > 0
            && state.fuel_burn_ticks_each > 0)
}

#[derive(Default)]
pub struct ModKernelRouter {
    routes: BTreeMap<RouteHandle, RoutedBinding>,
    by_feature: BTreeMap<(String, String), RouteHandle>,
    runtimes: BTreeMap<String, Runtime>,
    next_handle: u32,
}

fn targets(d: &ModFeatureDescriptor) -> Vec<UniversalKernelKind> {
    let mut v = Vec::new();
    if d.has_network_ports {
        if let Some(domain) = d.network_domain {
            v.push(UniversalKernelKind::GraphNetwork(domain));
        }
    }
    if d.processing_recipes || d.sleep_on_idle {
        v.push(UniversalKernelKind::DueProcessMachine);
    }
    if d.subworld_assembly {
        v.push(UniversalKernelKind::Subworld6Dof);
    }
    if d.resource_ports {
        v.push(UniversalKernelKind::ConservationTransfer);
    }
    if d.track_node {
        v.push(UniversalKernelKind::TrackGraph);
    }
    if d.action_timeline {
        v.push(UniversalKernelKind::TimelineAction);
    }
    v
}

impl ModKernelRouter {
    pub fn register_mod_ir(
        &mut self,
        mod_id: &str,
        descriptors: Vec<ModFeatureDescriptor>,
        budget: RoutingBudget,
    ) -> Result<KernelRouteReport, RouterError> {
        let count = descriptors.len();
        if count > budget.max_features_per_mod
            || self
                .routes
                .len()
                .saturating_add(descriptors.iter().map(|d| targets(d).len()).sum::<usize>())
                > budget.max_total_routes
        {
            return Err(RouterError::BudgetExceeded);
        }
        let mut report = KernelRouteReport::default();
        for d in descriptors {
            if !d.unsupported_custom_hooks.is_empty() {
                report.unrouted_features.push(UnroutedFeature {
                    mod_id: mod_id.into(),
                    feature_id: d.feature_id,
                    reason: format!(
                        "unsupported custom hooks: {}",
                        d.unsupported_custom_hooks.join(", ")
                    ),
                });
                continue;
            }
            let ts = targets(&d);
            if ts.is_empty() {
                report.unrouted_features.push(UnroutedFeature {
                    mod_id: mod_id.into(),
                    feature_id: d.feature_id,
                    reason: "no supported structural traits".into(),
                });
                continue;
            }
            for target in ts {
                let key = (mod_id.to_owned(), d.feature_id.clone());
                let Some(next) = self.next_handle.checked_add(1) else {
                    return Err(RouterError::BudgetExceeded);
                };
                let handle = RouteHandle(self.next_handle);
                self.next_handle = next;
                let binding = RoutedBinding {
                    mod_id: mod_id.into(),
                    feature_id: d.feature_id.clone(),
                    handle,
                    target,
                };
                self.routes.insert(handle, binding.clone());
                self.by_feature.insert(key, handle);
                report.routed.push(binding);
            }
        }
        report.routed.sort_by(|a, b| {
            (&a.mod_id, &a.feature_id, a.target).cmp(&(&b.mod_id, &b.feature_id, b.target))
        });
        report
            .unrouted_features
            .sort_by(|a, b| (&a.mod_id, &a.feature_id).cmp(&(&b.mod_id, &b.feature_id)));
        Ok(report)
    }

    pub fn binding(&self, handle: RouteHandle) -> Option<&RoutedBinding> {
        self.routes.get(&handle)
    }
    pub fn handle(&self, mod_id: &str, feature_id: &str) -> Option<RouteHandle> {
        self.by_feature
            .get(&(mod_id.into(), feature_id.into()))
            .copied()
    }

    /// Advance bounded registered runtime work. Scheduled transport is advanced once
    /// per mod; dormant machines, clean graph topologies, idle actions, and stalled
    /// furnaces consume zero per-tick node budget.
    pub fn tick_routed_kernels(&mut self, now_tick: u64, budget: RoutingBudget) -> RouterTickStats {
        let mut stats = RouterTickStats::default();
        if self.runtimes.is_empty() {
            return stats;
        }
        if budget.max_nodes_per_tick == 0 {
            stats.budget_exhausted = true;
            return stats;
        }
        for runtime in self.runtimes.values_mut() {
            if stats.routed_nodes_ticked >= budget.max_nodes_per_tick {
                stats.budget_exhausted = true;
                break;
            }
            if let Some(t) = runtime.transport.as_mut() {
                let pending = t.next_wake_tick();
                if pending.is_some_and(|due| due <= now_tick) {
                    if let Ok(report) = t.advance_to(
                        now_tick,
                        budget.max_nodes_per_tick - stats.routed_nodes_ticked,
                    ) {
                        stats.woken += report.processed;
                        stats.routed_nodes_ticked += report.processed;
                    }
                } else {
                    stats.sleeping += 1;
                }
            }
            if let Some(g) = runtime.kinetic.as_ref() {
                if runtime.kinetic_dirty {
                    stats.routed_nodes_ticked = stats.routed_nodes_ticked.saturating_add(
                        g.tick().min(
                            budget
                                .max_nodes_per_tick
                                .saturating_sub(stats.routed_nodes_ticked),
                        ),
                    );
                    runtime.kinetic_dirty = false;
                } else {
                    stats.sleeping += 1;
                }
            }
            if let Some(f) = runtime.fluid.as_mut() {
                if stats.routed_nodes_ticked < budget.max_nodes_per_tick {
                    if f.step().is_some() {
                        stats.routed_nodes_ticked += 1;
                    } else {
                        stats.sleeping += 1;
                    }
                }
            }
            if let Some(m) = runtime.me.as_mut() {
                if runtime.me_dirty {
                    if stats.routed_nodes_ticked < budget.max_nodes_per_tick {
                        let allocation = m.solve_channels();
                        runtime.me_dirty = false;
                        if !allocation.online_devices.is_empty() || !allocation.conflicts.is_empty()
                        {
                            stats.routed_nodes_ticked += 1;
                        }
                    }
                } else {
                    stats.sleeping += 1;
                }
            }
            if let Some(r) = runtime.recipes.as_mut() {
                if stats.routed_nodes_ticked < budget.max_nodes_per_tick {
                    if r.next_due().is_some_and(|due| due <= now_tick) {
                        if r.step().is_some() {
                            stats.routed_nodes_ticked += 1;
                            stats.woken += 1;
                        }
                    } else {
                        stats.sleeping += 1;
                    }
                }
            }
            for action in &mut runtime.actions {
                if action.phase() == crate::combat_motion::ActionPhase::Idle {
                    stats.sleeping += 1;
                    continue;
                }
                if stats.routed_nodes_ticked >= budget.max_nodes_per_tick {
                    stats.budget_exhausted = true;
                    break;
                }
                let _ = action.advance_to(now_tick);
                stats.routed_nodes_ticked += 1;
            }
            if !runtime.contraptions.is_empty()
                && stats.routed_nodes_ticked < budget.max_nodes_per_tick
            {
                stats.routed_nodes_ticked += runtime
                    .contraptions
                    .len()
                    .min(budget.max_nodes_per_tick - stats.routed_nodes_ticked);
            }
            for (state, last_tick) in &mut runtime.furnaces {
                if !furnace_can_progress(state) {
                    *last_tick = (*last_tick).max(now_tick);
                    stats.sleeping += 1;
                    continue;
                }
                if *last_tick < now_tick && stats.routed_nodes_ticked < budget.max_nodes_per_tick {
                    let delta = now_tick - *last_tick;
                    *state = catch_up_furnace(
                        &CatchUpLimits {
                            enabled: true,
                            max_delta_ticks: delta,
                        },
                        state,
                        delta,
                    )
                    .state;
                    *last_tick = now_tick;
                    stats.routed_nodes_ticked += 1;
                    stats.woken += 1;
                }
            }
        }
        if stats.routed_nodes_ticked >= budget.max_nodes_per_tick {
            stats.budget_exhausted = true;
        }
        stats
    }

    /// Minimal explicit kernel attachment helpers used by adapters after structural routing.
    pub fn attach_sleeping_transport(&mut self, mod_id: &str, id: u32, delay: u64) -> bool {
        let runtime = self.runtimes.entry(mod_id.into()).or_default();
        let transport = runtime
            .transport
            .get_or_insert_with(SleepingTransport::default);
        transport.add_machine(id, 1, delay).is_ok()
    }
    pub fn insert_transport_item(&mut self, mod_id: &str, id: u32, item: u32) -> bool {
        self.runtimes
            .get_mut(mod_id)
            .and_then(|r| r.transport.as_mut())
            .is_some_and(|t| t.insert(id, item).is_ok())
    }
    pub fn attach_recipe_machine(
        &mut self,
        mod_id: &str,
        id: u32,
        input_capacity: u32,
        output_capacity: u32,
        recipes: Vec<Recipe>,
        drive: Drive,
    ) -> Result<(), RecipeError> {
        self.runtimes
            .entry(mod_id.into())
            .or_default()
            .recipes
            .get_or_insert_with(RecipeProcessor::default)
            .add_machine(id, input_capacity, output_capacity, recipes, drive)
    }
    pub fn insert_recipe_input(
        &mut self,
        mod_id: &str,
        id: u32,
        stack: Stack,
    ) -> Result<u32, RecipeError> {
        self.runtimes
            .get_mut(mod_id)
            .and_then(|r| r.recipes.as_mut())
            .ok_or(RecipeError::UnknownMachine)?
            .insert(id, stack)
    }
    pub fn attach_kinetic_node(&mut self, mod_id: &str, id: u32, node: KineticNode) -> bool {
        let runtime = self.runtimes.entry(mod_id.into()).or_default();
        let added = runtime
            .kinetic
            .get_or_insert_with(KineticGraph::default)
            .add_node(id, node)
            .is_ok();
        if added {
            runtime.kinetic_dirty = true;
        }
        added
    }
    pub fn attach_fluid_tank(&mut self, mod_id: &str, id: u32, capacity: u32) -> bool {
        self.runtimes
            .entry(mod_id.into())
            .or_default()
            .fluid
            .get_or_insert_with(FluidNetwork::default)
            .add_tank(id, capacity, 0)
            .is_ok()
    }
    pub fn attach_me_node(&mut self, mod_id: &str, id: u32, kind: MeNodeKind) -> bool {
        let runtime = self.runtimes.entry(mod_id.into()).or_default();
        let added = runtime
            .me
            .get_or_insert_with(MeChannelGraph::default)
            .add_node(id, kind)
            .is_ok();
        if added {
            runtime.me_dirty = true;
        }
        added
    }
    pub fn attach_contraption(&mut self, mod_id: &str, cluster: BlockCluster, pose: Pose) {
        self.runtimes
            .entry(mod_id.into())
            .or_default()
            .contraptions
            .push(Contraption::new(cluster, pose));
    }
    pub fn attach_action(&mut self, mod_id: &str, spec: ActionSpec, start: u64) -> bool {
        if let Ok(action) = ActiveAction::start(spec, 1.0, start, Hand::MainHand) {
            self.runtimes
                .entry(mod_id.into())
                .or_default()
                .actions
                .push(action);
            true
        } else {
            false
        }
    }
    pub fn attach_furnace_catchup(
        &mut self,
        mod_id: &str,
        state: FurnaceCatchUpState,
        now_tick: u64,
    ) {
        self.runtimes
            .entry(mod_id.into())
            .or_default()
            .furnaces
            .push((state, now_tick));
    }
}
