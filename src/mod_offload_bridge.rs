// Production bridge between megu and the local mod-adapter crates.
//
// `include!`d into `main.rs` under `#[cfg(feature = "create-offload")]`, in the same
// flat root-scope style as `anticheat.rs`/`play_protocol.rs`, so the layers module
// graph is untouched: with the feature off nothing here is compiled or linked.
//
// Surfaces wired today:
//   * `mod_offload_tick` — one step per server tick from `game_tick`, driving
//     `CreateFactory` + `UniversalModRuntime` region budgets and translating
//     belt/contraption carry into exact `ac_note_carried_motion` grants so
//     anti-cheat never rubber-bands or false-flags fall damage on riders.
//   * `mod_offload_sync_antixray_tags` — folds `#c:ores`/`#forge:ores`/`#c:chests`/
//     `#forge:chests` (and name-heuristic) mod blocks into `hidden_extra()`, the
//     same anti-xray hidden set `refresh_hidden_tags_from_datapacks` feeds.
//     The sync runs automatically inside `enable_create`/`load_profiles`/
//     `declare_tag`, so loaded mod blocks protect against x-ray with no
//     manual second call; the free function stays for explicit re-syncs.
//   * `mod_offload_config_payloads` — deterministic Configuration-phase `0x07`
//     registry data and `neoforge:frozen_registry*` custom payloads, emitted only
//     while custom mod blocks are active so vanilla clients see nothing new.
//   * `mod_offload_save_bytes` / `mod_offload_load_bytes` — Save v2 persistence
//     for placed Create blocks and live contraptions, encoded in create-adapter's
//     deterministic binary wire style (BE scalars + VarInt counts + MC strings).

use create_adapter::blockstates::{core_create_registry, CreateBlockRegistry};
use create_adapter::contraption_adapter::{
    is_immovable_block_id, ActorCell, ActorTickReport, ActorWorldEdit, ContraptionActor,
    ContraptionActorKind, ContraptionError, ContraptionKind, ContraptionSnapshot,
    StagedActorEditBatch,
    ContraptionStorage, ContraptionWorldQuery, CropCell, LiveContraption, SuperGlueVolume,
    WorldBlockEdit, WorldItemDrop, DEFAULT_MAX_CONTRAPTION_BLOCKS,
};
use create_adapter::factory::{CreateFactory, DimBlockPos as ModBlockPos, KineticUpdateTag};
use create_adapter::offload_bridge::{
    AssemblyStress, OffloadBridge, OffloadBudget, OffloadError,
};
use create_adapter::rail_network::{
    ChunkKey as RailChunkKey, EdgeId as RailEdgeId, NodeId as RailNodeId,
    TrainReplicationPayload, TransitEvent as RailTransitEvent,
};
use create_adapter::neoforge_wire as create_wire;
use mod_adapter_core::blockstates::DEFAULT_BASE_STATE_ID;
use mod_adapter_core::neoforge_wire as mod_wire;
use mod_adapter_core::profile as mod_profile;
use mod_adapter_core::profile::{ModOffloadProfile, UniversalModRuntime};

/// Protocol 767 Configuration clientbound Custom Payload packet id.
#[allow(dead_code)]
const CONFIG_CUSTOM_PAYLOAD_ID: u8 = 0x01;
/// Per-region work/time budget for one `mod_offload` tick step. The kernels are
/// sleeping schedulers: a region with nothing queued returns immediately, so the
/// bound only bites on genuinely busy modded factories.
const MOD_OFFLOAD_REGION_STEPS: usize = 1_024;
const MOD_OFFLOAD_REGION_US: u64 = 500;
/// Total block-visit work units one `mod_offload` tick may spend across
/// factory regions and contraption assemblies — a single shared budget so a
/// runaway modded factory can never starve the actor passes below it.
const MOD_OFFLOAD_MAX_WORK_UNITS: usize = 16_384;
/// Default contraption partition workers for the offload engine. The
/// deterministic fold is bit-identical for every worker count — this only
/// changes where the motion work executes, never what it produces.
const MOD_OFFLOAD_DEFAULT_WORKERS: usize = 4;

/// Save v2 bridge codec: `"MSMO"` + version byte, then create-adapter-style
/// deterministic fields (BE scalars, VarInt counts, MC strings).
#[allow(dead_code)]
const MOD_OFFLOAD_SAVE_MAGIC: [u8; 4] = *b"MSMO";
#[allow(dead_code)]
const MOD_OFFLOAD_SAVE_VERSION: u8 = 2;

/// A player riding an assembled contraption: `anchor` is their world-space point
/// rigidly attached to the contraption frame, updated every stepped tick.
#[derive(Clone, Copy, Debug)]
struct ContraptionRider {
    contraption: u32,
    anchor: [f64; 3],
}

/// Staged actor batch plus the world edits derived from each staged entry.
/// `None` means the edit could not resolve to a concrete world write and is
/// counted as failed during inventory commit.
#[derive(Clone, Debug)]
struct PendingActorBatch {
    contraption: u32,
    batch: StagedActorEditBatch,
}

/// Everything the feature-gated mod offload owns at runtime. Constructed empty;
/// `has_work`/`mod_blocks_active` stay false until `MEGU_MOD_OFFLOAD` (or a
/// test) opts pieces in, keeping untouched vanilla connections and worlds at
/// literally zero bridge cost.
#[allow(dead_code)]
struct ModOffloadBridge {
    /// Parallel offload engine: owns the flagship `CreateFactory` (built on
    /// `enable_create`, so an empty bridge carries no registry) and the
    /// region-partitioned contraption assemblies its parallel scheduler steps.
    offload: OffloadBridge,
    /// Declarative universal runtime, present iff profiles loaded cleanly.
    universal: Option<UniversalModRuntime>,
    /// Profiles `load_profiles` accepted (kept for config payload parity).
    profiles: Vec<ModOffloadProfile>,
    /// Cached `state_count` of the Create registry so `universal` state IDs can
    /// reserve space deterministically even before `create` exists.
    create_state_count: Option<u32>,
    /// Create blocks this bridge placed, so Save v2 can rebuild the kinetic
    /// network: `pos -> state_id` in the *current* runtime's IDs; persisted as
    /// canonical strings so a reordered registry remaps cleanly.
    placed_create: BTreeMap<ModBlockPos, u32>,
    /// Stress spec per registered assembly, preserved across the
    /// unregister/re-register cycle every mutable contraption access performs.
    assembly_stress: BTreeMap<u32, AssemblyStress>,
    next_contraption_id: u32,
    contraption_riders: BTreeMap<i32, ContraptionRider>,
    /// eid -> exact per-tick carry velocity while riding a belt/conveyor.
    belt_riders: BTreeMap<i32, [f64; 3]>,
    /// Loaded `#tag` -> member block ids (mod or vanilla), e.g. from mod jars.
    tag_members: BTreeMap<String, BTreeSet<String>>,
    /// Custom (non-vanilla) state IDs the anti-xray sync decided to hide.
    mod_state_ids: BTreeSet<u32>,
    /// Partition workers handed to `OffloadBudget::with_workers`; the fold is
    /// bit-identical for every count.
    offload_workers: usize,
    /// Ticks actually stepped — the empty bridge must never tick (test evidence).
    ticks_stepped: u64,
    /// Actor world edits committed through the CAS write path — live proof the
    /// returned `WorldBlockEdit`s are applied, never discarded.
    world_block_edits_applied: u64,
    /// Coalesced kinetic UpdateTag deltas emitted since boot.
    kinetic_updates_emitted: u64,
    /// Ticks that produced no step output at all.
    idle_ticks_skipped: u64,
    /// Actor batches awaiting external world application. Production commits
    /// these only after the process-wide bridge mutex has been released.
    pending_actor_batches: Vec<PendingActorBatch>,
    /// Committed overflow drops not yet drained by the entity/wire layer.
    committed_actor_drops: Vec<WorldItemDrop>,
}

/// Deterministic summary of one [`ModOffloadBridge::step_tick`]: every output
/// the engine produced this tick folded into one value so nothing the factory,
/// the universal runtime, or the contraption actors computed is silently
/// discarded.
#[allow(dead_code)]
#[derive(Clone, Debug, Default)]
struct BridgeTickOutput {
    /// Factory regions that produced step output this tick.
    factory_regions: usize,
    /// Factory batch records exported this tick (S5-frame payloads).
    factory_records: usize,
    /// Universal-runtime regions that produced step output this tick.
    universal_regions: usize,
    /// Coalesced kinetic RPM/stress UpdateTag deltas drained after the step,
    /// in canonical `DimBlockPos` order.
    kinetic_updates: Vec<(ModBlockPos, KineticUpdateTag)>,
    /// Assemblies the budget scan actually stepped this tick.
    assemblies_stepped: usize,
    /// Eligible assemblies deferred by the work-unit budget (retried later).
    assemblies_deferred: usize,
    /// Assemblies frozen by overstress, halt latch, or over-budget cost.
    assemblies_stopped: usize,
    /// Work units the engine consumed this tick.
    work_units_used: usize,
    /// Actor edits committed into the live world this tick.
    world_block_edits: Vec<WorldBlockEdit>,
    /// Parallel grouped view of actor world edits, including unresolved
    /// entries as `None`; used only for transactional external application.
    actor_edit_groups: Vec<Vec<Option<WorldBlockEdit>>>,
    /// Items ejected by full onboard storage — reported, never silently lost.
    item_drops: Vec<WorldItemDrop>,
    /// Rail signal/station/render lifecycle events emitted this tick.
    rail_events: Vec<RailTransitEvent>,
    /// Orbital transfer commits drained this tick (single-owner handoff).
    orbital_transfer_commits: Vec<create_adapter::orbital::OrbitalTransferCommit>,
    /// The engine's deterministic post-tick digest.
    digest: u64,
    /// Fail-closed step error, if the engine refused the tick.
    error: Option<String>,
}

#[allow(dead_code)]
impl BridgeTickOutput {
    /// Whether this tick produced any observable work at all.
    fn did_any_work(&self) -> bool {
        self.factory_regions > 0
            || self.factory_records > 0
            || self.universal_regions > 0
            || self.assemblies_stepped > 0
            || !self.kinetic_updates.is_empty()
            || !self.world_block_edits.is_empty()
            || !self.item_drops.is_empty()
            || !self.rail_events.is_empty()
            || !self.orbital_transfer_commits.is_empty()
    }
}

/// Zero-stress spec for assemblies registered without an explicit budget.
fn mob_zero_stress() -> AssemblyStress {
    AssemblyStress {
        capacity: 0.0,
        consumption: 0.0,
    }
}

/// Map an offload-engine registration failure onto the public contraption
/// error surface — fail closed, never a half-registered assembly.
fn mob_offload_err(err: OffloadError) -> ContraptionError {
    match err {
        OffloadError::AssemblyTooLarge { count, limit, .. } => {
            ContraptionError::TooLarge { count, limit }
        }
        other => ContraptionError::CorruptSnapshot(format!("offload register: {other:?}")),
    }
}

#[allow(dead_code)]
impl ModOffloadBridge {
    fn new() -> Self {
        Self {
            offload: OffloadBridge::new(),
            universal: None,
            profiles: Vec::new(),
            create_state_count: None,
            placed_create: BTreeMap::new(),
            assembly_stress: BTreeMap::new(),
            next_contraption_id: 1,
            contraption_riders: BTreeMap::new(),
            belt_riders: BTreeMap::new(),
            tag_members: BTreeMap::new(),
            mod_state_ids: BTreeSet::new(),
            offload_workers: MOD_OFFLOAD_DEFAULT_WORKERS,
            ticks_stepped: 0,
            world_block_edits_applied: 0,
            kinetic_updates_emitted: 0,
            idle_ticks_skipped: 0,
            pending_actor_batches: Vec::new(),
            committed_actor_drops: Vec::new(),
        }
    }

    /// `MEGU_MOD_OFFLOAD` (comma list): `create` arms the flagship adapter;
    /// `universal`/`all` loads the built-in reference profiles; `ae2`, `mekanism`
    /// and `pipez` load single reference profiles. Absent/empty means a fully
    /// vanilla server with zero mod-offload surface.
    fn from_env() -> Self {
        let mut bridge = Self::new();
        let Ok(spec) = std::env::var("MEGU_MOD_OFFLOAD") else {
            return bridge;
        };
        let mut profiles: Vec<ModOffloadProfile> = Vec::new();
        for token in spec.split(',').map(str::trim).filter(|t| !t.is_empty()) {
            match token {
                "create" => bridge.enable_create(),
                "universal" | "all" => profiles.extend(mod_profile::builtin_reference_profiles()),
                "ae2" => profiles.push(mod_profile::ae2_class_profile()),
                "mekanism" => profiles.push(mod_profile::mekanism_class_profile()),
                "pipez" => profiles.push(mod_profile::pipez_class_profile()),
                other => eprintln!("[mod-offload] ignoring unknown MEGU_MOD_OFFLOAD token {other:?}"),
            }
        }
        if !profiles.is_empty() {
            if let Err(err) = bridge.load_profiles(profiles) {
                eprintln!("[mod-offload] reference profiles failed to build: {err:?}");
            }
        }
        bridge
    }

    fn create_registry(&self) -> Option<&CreateBlockRegistry> {
        self.offload.factory().map(CreateFactory::registry)
    }

    /// Build the flagship Create registry + factory if not already present.
    /// Assemblies only register after the factory arms the engine, so the swap
    /// below can never drop a live contraption — the drain is belt-and-braces
    /// in case a future caller ever violates that invariant.
    fn enable_create(&mut self) {
        if self.offload.factory().is_some() {
            return;
        }
        let parked: Vec<LiveContraption> = self
            .offload
            .assembly_ids()
            .collect::<Vec<_>>()
            .into_iter()
            .filter_map(|id| self.offload.unregister_contraption(id))
            .collect();
        self.offload = OffloadBridge::with_factory(CreateFactory::new(
            core_create_registry(DEFAULT_BASE_STATE_ID),
        ));
        for contraption in parked {
            let stress = self
                .assembly_stress
                .get(&contraption.id)
                .copied()
                .unwrap_or_else(mob_zero_stress);
            let _ = self.offload.register_contraption(contraption, stress);
        }
        // Freshly armed mod block space may already match the anti-xray
        // tag/name rules declared so far — fold it in immediately.
        self.sync_antixray_tags();
    }

    /// Run `f` on the live assembly `id`, preserving its stress budget across
    /// the unregister/re-register cycle the engine requires for `&mut` access.
    /// `None` when `id` is unknown.
    fn with_contraption<R>(
        &mut self,
        id: u32,
        f: impl FnOnce(&mut LiveContraption) -> R,
    ) -> Option<R> {
        let mut contraption = self.offload.unregister_contraption(id)?;
        let out = f(&mut contraption);
        let stress = self
            .assembly_stress
            .get(&id)
            .copied()
            .unwrap_or_else(mob_zero_stress);
        let _ = self.offload.register_contraption(contraption, stress);
        Some(out)
    }

    /// State ID space after the (always reserved) Create block states, so the
    /// universal runtime's IDs are deterministic regardless of enable order.
    fn universal_base(&mut self) -> u32 {
        let count = match self.create_registry() {
            Some(reg) => reg.state_count() as u32,
            None => *self
                .create_state_count
                .get_or_insert_with(|| core_create_registry(DEFAULT_BASE_STATE_ID).state_count() as u32),
        };
        DEFAULT_BASE_STATE_ID + count
    }

    /// Load declarative universal profiles; the runtime is only installed when
    /// every profile builds, so a bad manifest can never leave a half-loaded
    /// adapter visible to config payloads.
    fn load_profiles(
        &mut self,
        profiles: Vec<ModOffloadProfile>,
    ) -> Result<(), mod_adapter_core::blockstates::RegistryError> {
        let base = self.universal_base();
        let runtime = UniversalModRuntime::from_profiles(base, &profiles)?;
        self.universal = Some(runtime);
        self.profiles = profiles;
        // Loaded mod blocks may sit under already-declared `#c:ores`/
        // `#forge:chests` tags or match the ore/chest/barrel name heuristic:
        // hide them against x-ray now, not on some later manual sync.
        self.sync_antixray_tags();
        Ok(())
    }

    /// Anything worth ticking or syncing. The empty bridge is a zero-cost no-op.
    fn has_work(&self) -> bool {
        self.offload.factory().is_some()
            || self.universal.is_some()
            || self.offload.assembly_count() > 0
            || self.offload.orbital_ship_count() > 0
            || !self.offload.rail_network().trains.is_empty()
            || !self.contraption_riders.is_empty()
            || !self.belt_riders.is_empty()
    }

    /// Custom mod block space is live: config-phase payloads exist only now.
    fn mod_blocks_active(&self) -> bool {
        self.offload.factory().is_some() || self.universal.is_some()
    }

    /// State Save v2 must carry: placed Create blocks and live contraptions.
    /// Riders, tag caches and the universal runtime are re-derived at boot or
    /// next tick, so an otherwise-empty bridge still emits zero trailer bytes.
    fn has_persistent_state(&self) -> bool {
        !self.placed_create.is_empty()
            || self.offload.assembly_count() > 0
            || !self.offload.rail_network().trains.is_empty()
            || !self.offload.rail_network().graph.edges.is_empty()
    }

    /// Place a Create block through the bridge so Save v2 can rebuild the same
    /// kinetic network after a reload. The returned UpdateTags wake exactly the
    /// regions whose topology changed; untouched regions stay fingerprint-gated.
    fn place_create_block(
        &mut self,
        pos: ModBlockPos,
        state_id: u32,
    ) -> Vec<KineticUpdateTag> {
        self.enable_create();
        let factory = self.offload.factory_mut().expect("enable_create just ran");
        self.placed_create.insert(pos, state_id);
        factory.place_block(pos, state_id)
    }

    fn remove_create_block(&mut self, pos: ModBlockPos) -> Vec<KineticUpdateTag> {
        self.placed_create.remove(&pos);
        match self.offload.factory_mut() {
            Some(factory) => factory.remove_block(pos),
            None => Vec::new(),
        }
    }

    /// Assemble a world region into a live contraption; the returned world edits
    /// are for the caller to commit (remove the swept blocks).
    fn assemble_contraption(
        &mut self,
        dim: DimId,
        anchor: [i32; 3],
        seed: [i32; 3],
        attached_blocks: &BTreeMap<[i32; 3], u32>,
        kind: ContraptionKind,
    ) -> Result<(u32, Vec<WorldBlockEdit>), ContraptionError> {
        self.enable_create();
        let id = self.next_contraption_id;
        self.next_contraption_id += 1;
        let (contraption, edits) = LiveContraption::assemble_from_world(
            id,
            dim.raw() as i32,
            anchor,
            seed,
            attached_blocks,
            kind,
            DEFAULT_MAX_CONTRAPTION_BLOCKS,
        )?;
        self.offload
            .register_contraption(contraption, mob_zero_stress())
            .map_err(mob_offload_err)?;
        Ok((id, edits))
    }

    /// Assemble only blocks joined through Super Glue or a radial sticky
    /// block. The input's canonical names provide the adapter predicates.
    #[allow(clippy::too_many_arguments)]
    fn assemble_contraption_with_glue(
        &mut self,
        dim: DimId,
        anchor: [i32; 3],
        seed: [i32; 3],
        world_blocks: &BTreeMap<[i32; 3], (u32, String)>,
        glue: &SuperGlueVolume,
        kind: ContraptionKind,
    ) -> Result<(u32, Vec<WorldBlockEdit>), ContraptionError> {
        let mut classes = BTreeMap::<u32, (bool, bool)>::new();
        let mut states = BTreeMap::new();
        for (pos, (state_id, block_id)) in world_blocks {
            states.insert(*pos, *state_id);
            let sticky = matches!(
                block_id.as_str(),
                "create:linear_chassis"
                    | "create:secondary_linear_chassis"
                    | "create:radial_chassis"
                    | "minecraft:slime_block"
                    | "minecraft:honey_block"
            );
            let immovable = is_immovable_block_id(block_id);
            let class = classes.entry(*state_id).or_default();
            class.0 |= sticky;
            class.1 |= immovable;
        }
        let id = self.next_contraption_id;
        let next_id = id.checked_add(1).ok_or_else(|| {
            ContraptionError::CorruptSnapshot("contraption id space exhausted".to_string())
        })?;
        let (contraption, edits) = LiveContraption::assemble_with_glue(
            id,
            dim.raw() as i32,
            anchor,
            seed,
            &states,
            glue,
            |state| classes.get(&state).is_some_and(|class| class.0),
            |state| classes.get(&state).is_some_and(|class| class.1),
            kind,
            DEFAULT_MAX_CONTRAPTION_BLOCKS,
        )?;
        self.enable_create();
        self.next_contraption_id = next_id;
        self.offload
            .register_contraption(contraption, mob_zero_stress())
            .map_err(mob_offload_err)?;
        Ok((id, edits))
    }

    /// Mount an onboard actor (drill, harvester, deployer...) at a local cell
    /// of the live assembly `id`. Returns false when `id` is unknown.
    fn mount_contraption_actor(&mut self, id: u32, actor: ContraptionActor) -> bool {
        self.with_contraption(id, |contraption| contraption.actors.push(actor))
            .is_some()
    }

    /// Advance mounted Create actors and return their proposed world edits.
    fn step_contraption_actors(
        &mut self,
        id: u32,
        query: &dyn ContraptionWorldQuery,
    ) -> Option<ActorTickReport> {
        self.with_contraption(id, |contraption| contraption.step_actors(query))
    }

    /// Unmount a contraption only when every placement target is available.
    /// An unknown id returns an empty conflict list and preserves the bridge;
    /// a conflict puts the contraption back exactly where the scan found it.
    fn disassemble_contraption_checked<F>(
        &mut self,
        id: u32,
        is_occupied: F,
    ) -> Result<Vec<WorldBlockEdit>, Vec<ModBlockPos>>
    where
        F: Fn(ModBlockPos) -> bool,
    {
        let Some(contraption) = self.offload.unregister_contraption(id) else {
            return Err(Vec::new());
        };
        match contraption.disassemble_to_world_checked(is_occupied) {
            Ok(edits) => {
                self.assembly_stress.remove(&id);
                self.contraption_riders
                    .retain(|_, rider| rider.contraption != id);
                Ok(edits)
            }
            Err(conflicts) => {
                let stress = self
                    .assembly_stress
                    .get(&id)
                    .copied()
                    .unwrap_or_else(mob_zero_stress);
                let _ = self.offload.register_contraption(contraption, stress);
                Err(conflicts)
            }
        }
    }

    /// Attach player `eid` to a live contraption at world `anchor`. Each tick the
    /// rigid pose delta is granted to anti-cheat exactly.
    fn note_contraption_rider(&mut self, eid: i32, contraption: u32, anchor: [f64; 3]) -> bool {
        if self.offload.assembly(contraption).is_none() {
            return false;
        }
        self.contraption_riders
            .insert(eid, ContraptionRider { contraption, anchor });
        true
    }

    /// Attach player `eid` to a belt/conveyor carrying them `delta` per tick.
    fn note_belt_rider(&mut self, eid: i32, delta: [f64; 3]) {
        self.belt_riders.insert(eid, delta);
    }

    /// Player left every modded carrier (dismount, disassembly, logout).
    fn stop_carry(&mut self, eid: i32) {
        self.contraption_riders.remove(&eid);
        self.belt_riders.remove(&eid);
    }

    /// Declare a mod block tag's members, e.g. parsed from a loaded jar's
    /// `data/<ns>/tags/block/*.json`. `#` prefixes are normalized away.
    fn declare_tag(&mut self, tag: &str, members: impl IntoIterator<Item = impl Into<String>>) {
        let tag = tag.strip_prefix('#').unwrap_or(tag);
        let entry = self.tag_members.entry(ns_rl(tag)).or_default();
        for member in members {
            let name: String = member.into();
            entry.insert(ns_rl(&name));
        }
        // A freshly declared `#c:ores`/`#forge:chests`-family tag may name
        // already-loaded mod blocks — re-fold the hidden set right away.
        self.sync_antixray_tags();
    }

    /// Every loaded custom mod block id across both adapter tiers.
    fn mod_block_ids(&self) -> Vec<String> {
        let mut ids = Vec::new();
        if let Some(reg) = self.create_registry() {
            ids.extend(reg.blocks().iter().map(|b| b.id.clone()));
        }
        if let Some(uni) = &self.universal {
            ids.extend(uni.registry().blocks().iter().map(|b| b.id.clone()));
        }
        ids
    }

    /// All expanded runtime state IDs of loaded mod block `name`.
    fn mod_state_ids_for_block(&self, name: &str) -> Vec<u32> {
        let mut out = Vec::new();
        if let Some(reg) = self.create_registry() {
            out.extend(
                reg.states()
                    .iter()
                    .filter(|e| e.block_id == name)
                    .map(|e| e.state_id),
            );
        }
        if let Some(uni) = &self.universal {
            out.extend(
                uni.registry()
                    .states()
                    .iter()
                    .filter(|e| e.block_id == name)
                    .map(|e| e.state_id),
            );
        }
        out
    }

    /// Fold loaded mod ore/chest blocks into the anti-xray hidden set: members of
    /// the conventional `#c:ores` / `#forge:ores` / `#c:chests` / `#forge:chests`
    /// (and `*:barrels`) tag roots, plus loaded blocks whose names pattern-match
    /// (`*_ore`, `*chest*`, `*barrel*`, covering e.g. `create:zinc_ore` and
    /// `create:deepslate_zinc_ore` when a profile/addon declares them). Vanilla
    /// names resolve through `breg()`; custom mod state IDs are tracked in the
    /// bridge registry — both land in `hidden_extra()` for `is_hidden`.
    fn sync_antixray_tags(&mut self) -> usize {
        let mut names: BTreeSet<String> = BTreeSet::new();
        for (tag, members) in &self.tag_members {
            if mod_hidden_root_tag(tag) {
                names.extend(members.iter().cloned());
            }
        }
        for id in self.mod_block_ids() {
            let path = rl_path(&id);
            if path.ends_with("_ore") || path.contains("chest") || path.contains("barrel") {
                names.insert(id);
            }
        }
        if names.is_empty() {
            return 0;
        }
        let mut added = 0usize;
        {
            let mut extra = hidden_extra().lock().unwrap_or_else(|e| e.into_inner());
            if let Some(reg) = breg() {
                for st in 0..reg.state_count() as u32 {
                    if names.contains(reg.name_of(st)) && extra.insert(st) {
                        added += 1;
                    }
                }
            }
            for name in &names {
                for st in self.mod_state_ids_for_block(name) {
                    self.mod_state_ids.insert(st);
                    if extra.insert(st) {
                        added += 1;
                    }
                }
            }
        }
        if added > 0 {
            HIDDEN_EXTRA_USED.store(true, Ordering::Relaxed);
        }
        added
    }

    /// Deterministic Configuration-phase packets: one `0x07` Registry Data frame
    /// covering every loaded mod block name (in the same create-then-universal
    /// canonical order the adapters' own wire test uses), then each active mod
    /// registry's `neoforge:frozen_registry*` frames as `0x01` custom payloads.
    /// Empty while no mod offload profile is loaded — vanilla joins are
    /// bit-identical to a build without the feature.
    fn config_payloads(&self) -> Vec<(u8, Vec<u8>)> {
        if !self.mod_blocks_active() {
            return Vec::new();
        }
        let mut block_names = Vec::new();
        if let Some(reg) = self.create_registry() {
            block_names.extend(reg.blocks().iter().map(|b| b.id.clone()));
        }
        if let Some(uni) = &self.universal {
            block_names.extend(uni.registry().blocks().iter().map(|b| b.id.clone()));
        }
        let mut out = Vec::new();
        if let Ok(body) = mod_wire::build_config_registry_data_0x07("minecraft:block", &block_names)
        {
            out.push((mod_wire::CONFIG_REGISTRY_DATA_PACKET_ID, body));
        }
        let push_frames = |out: &mut Vec<(u8, Vec<u8>)>, frames: Vec<(String, Vec<u8>)>| {
            for (channel, payload) in frames {
                let mut body = Vec::with_capacity(1 + 32 + payload.len());
                body.push(CONFIG_CUSTOM_PAYLOAD_ID);
                mod_wire::write_mc_string(&mut body, &channel);
                body.extend_from_slice(&payload);
                out.push((CONFIG_CUSTOM_PAYLOAD_ID, body));
            }
        };
        if let Some(reg) = self.create_registry() {
            push_frames(&mut out, create_wire::build_neoforge_frozen_registry_frames(reg));
        }
        if let Some(uni) = &self.universal {
            push_frames(
                &mut out,
                create_wire::build_neoforge_frozen_registry_frames(uni.registry()),
            );
        }
        out
    }

    /// Dimension-aware rail edit/lifecycle facade forwarded to the offload
    /// engine. Events are drained through `step_tick`; these helpers mutate
    /// topology only through the network's occupancy-safe methods.
    fn rail_add_node(&mut self, dim: String, pos: [f64; 3]) -> RailNodeId {
        self.offload
            .rail_add_node(create_adapter::rail_network::DimId::new(dim), pos)
    }

    fn rail_add_edge(
        &mut self,
        from: RailNodeId,
        to: RailNodeId,
        gauge: create_adapter::rail_network::TrackGauge,
        control: [[f64; 3]; 4],
    ) -> Result<RailEdgeId, create_adapter::rail_network::RailError> {
        self.offload.rail_add_edge(from, to, gauge, control)
    }

    fn rail_remove_edge(&mut self, edge: RailEdgeId) -> bool {
        self.offload.rail_remove_edge(edge)
    }

    fn rail_chunk_loaded(&mut self, dim: String, cx: i32, cz: i32) {
        self.offload.rail_chunk_loaded(RailChunkKey::new(
            create_adapter::rail_network::DimId::new(dim),
            cx,
            cz,
        ));
    }

    fn rail_chunk_unloaded(&mut self, dim: String, cx: i32, cz: i32) {
        self.offload.rail_chunk_unloaded(&RailChunkKey::new(
            create_adapter::rail_network::DimId::new(dim),
            cx,
            cz,
        ));
    }

    fn rail_replication_payloads(&self) -> Vec<TrainReplicationPayload> {
        self.offload.rail_replication_payloads()
    }

    /// Advance the whole bridge by one server tick and return every output the
    /// engine produced: parallel factory region + contraption motion step, the
    /// coalesced kinetic UpdateTag deltas, the universal runtime step, actor
    /// world edits committed into the live world through the CAS path, and the
    /// exact carry grants into anti-cheat.
    fn step_tick(&mut self, tick: u64) -> BridgeTickOutput {
        self.step_tick_impl(tick, None, false)
    }

    /// Actor-query injection keeps the bridge integration test hermetic when
    /// the generated vanilla registry is absent from a clean-room checkout.
    /// Injected-query tests commit inline for backwards compatibility; the
    /// production tick uses the staged lock-free path.
    fn step_tick_with_actor_query(
        &mut self,
        tick: u64,
        actor_query: Option<&dyn ContraptionWorldQuery>,
    ) -> BridgeTickOutput {
        self.step_tick_impl(tick, actor_query, true)
    }

    fn step_tick_impl(
        &mut self,
        tick: u64,
        actor_query: Option<&dyn ContraptionWorldQuery>,
        commit_actors_inline: bool,
    ) -> BridgeTickOutput {
        self.ticks_stepped += 1;
        let mut out = BridgeTickOutput::default();

        // One deterministic engine step: factory regions and contraption
        // assemblies partitioned across `offload_workers`, folded in canonical
        // order — bit-identical for every worker count.
        let mut budget =
            OffloadBudget::new(MOD_OFFLOAD_MAX_WORK_UNITS).with_workers(self.offload_workers);
        budget.factory_region_steps = MOD_OFFLOAD_REGION_STEPS;
        budget.factory_region_micros = Some(MOD_OFFLOAD_REGION_US);
        match self.offload.step_tick(tick, budget) {
            Ok(report) => {
                out.factory_regions = report.factory_regions;
                out.factory_records = report.factory_records;
                out.work_units_used = report.work_units_used;
                out.assemblies_stepped = report.stepped;
                out.assemblies_deferred = report.deferred;
                out.assemblies_stopped =
                    report.overstressed + report.halted + report.over_budget;
                out.digest = report.digest;
            }
            Err(err) => {
                self.idle_ticks_skipped += 1;
                out.error = Some(format!("offload step refused: {err:?}"));
                return out;
            }
        }
        out.rail_events = self.offload.drain_rail_events();
        out.orbital_transfer_commits = self.offload.drain_orbital_transfer_commits();
        if let Some(universal) = &mut self.universal {
            out.universal_regions = universal
                .step_tick(tick, MOD_OFFLOAD_REGION_STEPS, Some(MOD_OFFLOAD_REGION_US))
                .len();
        }
        // Kinetic deltas are real output, not noise: drain the coalesced
        // UpdateTag batch so RPM/stress changes reach the wire.
        if let Some(factory) = self.offload.factory_mut() {
            factory.drain_update_batch_into(&mut out.kinetic_updates);
        }
        self.kinetic_updates_emitted += out.kinetic_updates.len() as u64;

        // Actor pass: stage edits and inventory deltas while the bridge is
        // locked, but do not mutate the world here. Production (`commit_inline
        // == false`) leaves inventory uncommitted until `mod_offload_tick`
        // releases the process-wide bridge mutex, applies the world edits, and
        // reacquires the bridge to commit exactly the successful deltas.
        self.pending_actor_batches.clear();
        for id in self.offload.assembly_ids().collect::<Vec<_>>() {
            let Some((dim, batch)) = self.with_contraption(id, |contraption| {
                let dim = contraption.dim;
                let batch = match actor_query {
                    Some(query) => contraption.step_actors_staged(query),
                    None => contraption.step_actors_staged(&LiveModWorldQuery { dim }),
                };
                (dim, batch)
            }) else {
                continue;
            };
            let _ = dim;
            let world_edits: Vec<Option<WorldBlockEdit>> = batch
                .edits
                .iter()
                .map(|staged| mob_actor_world_edit(&staged.edit))
                .collect();
            if commit_actors_inline {
                let successes: Vec<bool> = world_edits
                    .iter()
                    .map(|edit| edit.as_ref().is_some_and(mob_commit_world_block_edit))
                    .collect();
                let commit = self.with_contraption(id, |contraption| {
                    contraption.commit_staged_actor_inventory(&batch, &successes)
                });
                if let Some(commit) = commit {
                    self.committed_actor_drops.extend(commit.drops);
                }
                self.world_block_edits_applied += successes.iter().filter(|&&ok| ok).count() as u64;
            } else {
                self.pending_actor_batches.push(PendingActorBatch {
                    contraption: id,
                    batch,
                });
                out.actor_edit_groups.push(world_edits.clone());
            }
            for edit in world_edits.iter().flatten() {
                out.world_block_edits.push(edit.clone());
            }
        }

        // Exact rigid-carry deltas: `passenger_delta` is the displacement the
        // contraption just applied to the rider's attached point; belts carry a
        // fixed per-tick velocity. Both go through `ac_note_carried_motion`, so
        // the move is granted exactly and NoFall treats the rider as supported —
        // no rubber-banding, no false fall damage, real teleports still flag.
        for (eid, rider) in self.contraption_riders.iter_mut() {
            let Some(contraption) = self.offload.assembly(rider.contraption) else {
                continue;
            };
            let Some(delta) = contraption.passenger_delta(rider.anchor) else {
                continue;
            };
            for (axis, d) in rider.anchor.iter_mut().zip(delta) {
                *axis += d;
            }
            ac_note_carried_motion(*eid, delta);
        }
        for (&eid, &delta) in &self.belt_riders {
            ac_note_carried_motion(eid, delta);
        }

        if !out.did_any_work() {
            self.idle_ticks_skipped += 1;
        }
        out
    }

    /// Commit staged actor inventory after world edits were applied outside
    /// the bridge mutex. `success_groups` uses the same batch/edit ordering as
    /// `actor_edit_groups` returned by the tick. Overflow drops are retained
    /// for `drain_committed_actor_drops` — never silently destroyed.
    fn finalize_staged_actor_batches(&mut self, success_groups: &[Vec<bool>]) -> Vec<WorldItemDrop> {
        let pending = std::mem::take(&mut self.pending_actor_batches);
        let mut drops = Vec::new();
        for (index, staged) in pending.into_iter().enumerate() {
            let empty = Vec::new();
            let successes = success_groups.get(index).unwrap_or(&empty);
            let report = self.with_contraption(staged.contraption, |contraption| {
                contraption.commit_staged_actor_inventory(&staged.batch, successes)
            });
            if let Some(report) = report {
                drops.extend(report.drops);
            }
        }
        self.committed_actor_drops.extend(drops.iter().cloned());
        drops
    }

    /// Drain item drops whose inventory effects have already committed.
    fn drain_committed_actor_drops(&mut self) -> Vec<WorldItemDrop> {
        std::mem::take(&mut self.committed_actor_drops)
    }

    /// Transactional contraption entry helper for bearings/pistons/glue
    /// workflows: assemble through the normal checked path and return the
    /// caller-visible world edits without hiding actor drops or partial
    /// failures behind a fire-and-forget interaction result.
    fn interact_contraption_trigger(
        &mut self,
        dim: DimId,
        anchor: [i32; 3],
        seed: [i32; 3],
        attached_blocks: &BTreeMap<[i32; 3], u32>,
        kind: ContraptionKind,
    ) -> Result<(u32, Vec<WorldBlockEdit>), ContraptionError> {
        self.assemble_contraption(dim, anchor, seed, attached_blocks, kind)
    }

    /// Deterministic Save v2 binary snapshot: placed Create blocks as canonical
    /// blockstate strings (registry-reorder safe) plus live contraptions in
    /// create-adapter's name-based snapshot form.
    fn save_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(256);
        out.extend_from_slice(&MOD_OFFLOAD_SAVE_MAGIC);
        out.push(MOD_OFFLOAD_SAVE_VERSION);

        mod_wire::write_varint(&mut out, self.placed_create.len() as u32);
        for (pos, state_id) in &self.placed_create {
            mob_w_i32(&mut out, pos.dim);
            mob_w_i32(&mut out, pos.x);
            mob_w_i32(&mut out, pos.y);
            mob_w_i32(&mut out, pos.z);
            let canonical = self
                .create_registry()
                .and_then(|reg| reg.encode_canonical(*state_id))
                .unwrap_or("");
            mod_wire::write_mc_string(&mut out, canonical);
        }

        let snapshots: Vec<ContraptionSnapshot> = self
            .create_registry()
            .map(|reg| {
                self.offload
                    .assembly_ids()
                    .filter_map(|id| self.offload.assembly(id))
                    .map(|c| c.snapshot(reg))
                    .collect()
            })
            .unwrap_or_default();
        mod_wire::write_varint(&mut out, snapshots.len() as u32);
        for snap in &snapshots {
            mob_w_snapshot(&mut out, snap);
        }
        let rail = self.offload.rail_save_bytes();
        mod_wire::write_varint(&mut out, rail.len() as u32);
        out.extend_from_slice(&rail);
        out
    }

    /// Restore a `save_bytes` snapshot: replaces placed Create blocks and live
    /// contraptions. Canonical strings decode against the current registry, so
    /// reordered runtime IDs remap exactly like Save v2 promises.
    fn load_bytes(&mut self, bytes: &[u8]) -> Result<(), String> {
        let err = |m: &str| format!("mod-offload save: {m}");
        if bytes.len() < 5 || bytes[..4] != MOD_OFFLOAD_SAVE_MAGIC {
            return Err(err("bad magic"));
        }
        if bytes[4] != MOD_OFFLOAD_SAVE_VERSION && bytes[4] != 1 {
            return Err(err("unsupported version"));
        }
        let mut cursor = 5usize;

        let placed_count = mod_wire::read_varint(bytes, &mut cursor)
            .ok_or_else(|| err("truncated placed count"))? as usize;
        if placed_count > MAX_BATCH_PLACED {
            return Err(err("placed count out of range"));
        }
        self.enable_create();
        let mut pending = Vec::with_capacity(placed_count);
        for _ in 0..placed_count {
            let dim = mob_r_i32(bytes, &mut cursor).ok_or_else(|| err("truncated pos"))?;
            let x = mob_r_i32(bytes, &mut cursor).ok_or_else(|| err("truncated pos"))?;
            let y = mob_r_i32(bytes, &mut cursor).ok_or_else(|| err("truncated pos"))?;
            let z = mob_r_i32(bytes, &mut cursor).ok_or_else(|| err("truncated pos"))?;
            let canonical = mod_wire::read_mc_string(
                bytes,
                &mut cursor,
                mod_wire::MAX_REGISTRY_DATA_NAME_LEN,
            )
            .ok_or_else(|| err("truncated canonical state"))?;
            let state_id = self
                .create_registry()
                .and_then(|reg| reg.decode_canonical(&canonical))
                .ok_or_else(|| err("unknown canonical state"))?;
            pending.push((ModBlockPos::new(dim, x, y, z), state_id));
        }

        let contraption_count = mod_wire::read_varint(bytes, &mut cursor)
            .ok_or_else(|| err("truncated contraption count"))? as usize;
        if contraption_count > DEFAULT_MAX_CONTRAPTION_BLOCKS {
            return Err(err("contraption count out of range"));
        }
        let mut restored = Vec::with_capacity(contraption_count);
        for _ in 0..contraption_count {
            let snap = mob_r_snapshot(bytes, &mut cursor).ok_or_else(|| err("truncated snapshot"))?;
            let registry = self
                .create_registry()
                .ok_or_else(|| err("create registry unavailable"))?;
            restored.push(
                LiveContraption::restore(&snap, registry)
                    .map_err(|e| err(&format!("corrupt contraption: {e:?}")))?,
            );
        }
        let mut rail_snapshot = None;
        if bytes[4] == MOD_OFFLOAD_SAVE_VERSION {
            let rail_len = mod_wire::read_varint(bytes, &mut cursor)
                .ok_or_else(|| err("truncated rail snapshot len"))? as usize;
            let rail_bytes = bytes
                .get(cursor..cursor.saturating_add(rail_len))
                .ok_or_else(|| err("truncated rail snapshot"))?;
            cursor = cursor.saturating_add(rail_len);
            rail_snapshot = Some(
                create_adapter::rail_network::decode_snapshot(rail_bytes)
                    .map_err(|e| err(&format!("corrupt rail snapshot: {e:?}")))?,
            );
        }
        if cursor != bytes.len() {
            return Err(err("trailing bytes"));
        }

        // Decode fully succeeded: commit in one shot so a corrupt save can never
        // leave a half-restored kinetic network.
        self.placed_create.clear();
        for (pos, state_id) in pending {
            self.place_create_block(pos, state_id);
        }
        for id in self.offload.assembly_ids().collect::<Vec<_>>() {
            let _ = self.offload.unregister_contraption(id);
        }
        self.assembly_stress.clear();
        let mut max_id = 0u32;
        for c in restored {
            max_id = max_id.max(c.id);
            let _ = self.offload.register_contraption(c, mob_zero_stress());
        }
        self.next_contraption_id = self.next_contraption_id.max(max_id + 1);
        if let Some(rail) = rail_snapshot {
            self.offload.rail_load_bytes(&create_adapter::rail_network::encode_snapshot(&rail))
                .map_err(|e| err(&format!("rail restore failed: {e:?}")))?;
        }
        Ok(())
    }
}

/// Sanity bound for a placed-block section; a real factory is nowhere near this.
#[allow(dead_code)]
const MAX_BATCH_PLACED: usize = 1 << 20;

/// `name` -> always-namespaced resource location (`"chest"` -> `"minecraft:chest"`).
#[allow(dead_code)]
fn ns_rl(name: &str) -> String {
    if name.contains(':') {
        name.to_string()
    } else {
        format!("minecraft:{name}")
    }
}

/// Live-world view handed to `LiveContraption::step_actors`: reads real chunk
/// cells through `block_at_dim` and resolves names, hardness and crop ages
/// from the vanilla registry. A cell the registry cannot name — an out-of-range
/// mod state, an absent registry — reports as air, a fail-closed skip rather
/// than a guess.
#[allow(dead_code)]
struct LiveModWorldQuery {
    dim: i32,
}

#[allow(dead_code)]
impl ContraptionWorldQuery for LiveModWorldQuery {
    fn cell_at(&self, pos: [i32; 3]) -> ActorCell {
        let Ok(raw) = u32::try_from(self.dim) else {
            return ActorCell::air();
        };
        let st = block_at_dim(DimId::from_raw(raw), pos[0], pos[1], pos[2]);
        let Some(reg) = breg() else {
            return ActorCell::air();
        };
        if (st as usize) >= reg.state_count() || reg.is_air(st) {
            return ActorCell::air();
        }
        let name = reg.name_of(st).to_string();
        let mut cell = ActorCell::block(
            name.clone(),
            !is_immovable_block_id(&name) && reg.hardness_of(st) >= 0.0,
        );
        cell.crop = mob_crop_cell(reg, st, &name);
        cell
    }

    fn place_state_for(&self, item_id: &str) -> Option<u32> {
        name_to_state(item_id)
    }
}

/// Map a live crop state onto the harvester's `CropCell`: growth stage from the
/// real `age` property plus the drop table a Create harvester yields. Returns
/// `None` for non-crops — the harvester skips the cell entirely then.
#[allow(dead_code)]
fn mob_crop_cell(reg: &BReg, st: u32, name: &str) -> Option<CropCell> {
    let (max_age, drop_item, drop_count): (u8, &str, u32) = match name {
        "minecraft:wheat" => (7, "minecraft:wheat", 1),
        "minecraft:carrots" => (7, "minecraft:carrot", 2),
        "minecraft:potatoes" => (7, "minecraft:potato", 2),
        "minecraft:beetroots" => (3, "minecraft:beetroot", 1),
        "minecraft:nether_wart" => (3, "minecraft:nether_wart", 2),
        "minecraft:cocoa" => (2, "minecraft:cocoa_beans", 2),
        _ => return None,
    };
    let age: u8 = reg
        .properties_of(st)?
        .into_iter()
        .find(|(prop, _)| prop == "age")?
        .1
        .parse()
        .ok()?;
    Some(CropCell {
        age,
        max_age,
        drop_item: drop_item.to_string(),
        drop_count,
    })
}

/// Translate one reported `ActorWorldEdit` into the world-space `WorldBlockEdit`
/// the commit queue applies. `ResetCrop` re-resolves the live crop to its age-0
/// state; an edit that cannot resolve a target state is dropped, never guessed.
#[allow(dead_code)]
fn mob_actor_world_edit(edit: &ActorWorldEdit) -> Option<WorldBlockEdit> {
    match edit {
        ActorWorldEdit::BreakBlock { pos } => Some(WorldBlockEdit {
            pos: *pos,
            state_id: None,
        }),
        ActorWorldEdit::PlaceBlock { pos, state_id, .. } => Some(WorldBlockEdit {
            pos: *pos,
            state_id: Some(*state_id),
        }),
        ActorWorldEdit::ResetCrop { pos } => {
            let raw = u32::try_from(pos.dim).ok()?;
            let st = block_at_dim(DimId::from_raw(raw), pos.x, pos.y, pos.z);
            let reset = breg().map(|reg| reg.with_prop(st, "age", "0"))?;
            (reset != st).then_some(WorldBlockEdit {
                pos: *pos,
                state_id: Some(reset),
            })
        }
    }
}

/// Commit one bridge-produced world edit through the same CAS write path the
/// server uses, preserving dimension awareness and every side effect (lighting,
/// anti-xray, block entities, redstone, persistence notes). Fails closed on
/// out-of-bounds cells, immovable targets, and cells another writer already
/// raced past; returns whether the edit landed.
#[allow(dead_code)]
fn mob_commit_world_block_edit(edit: &WorldBlockEdit) -> bool {
    let Ok(raw) = u32::try_from(edit.pos.dim) else {
        return false;
    };
    let dim = DimId::from_raw(raw);
    let (x, y, z) = (edit.pos.x, edit.pos.y, edit.pos.z);
    if y < MIN_Y || y >= MIN_Y + 384 {
        return false;
    }
    let target = edit.state_id.unwrap_or(0);
    let old = block_at_dim(dim, x, y, z);
    if old == target {
        return false;
    }
    if let Some(reg) = breg() {
        if (old as usize) < reg.state_count() && is_immovable_block_id(reg.name_of(old)) {
            return false;
        }
    }
    set_block_if_dim(dim, x, y, z, target, Some(old))
}

/// Path component of a resource location (`"create:zinc_ore"` -> `"zinc_ore"`).
#[allow(dead_code)]
fn rl_path(id: &str) -> &str {
    id.split_once(':').map(|(_, p)| p).unwrap_or(id)
}

/// Conventional tags whose members anti-xray should hide (mirrors the predicate
/// `register_hidden_tags_from_dir` uses for datapack tags).
#[allow(dead_code)]
fn mod_hidden_root_tag(tag: &str) -> bool {
    matches!(
        tag,
        "c:ores" | "forge:ores" | "c:chests" | "forge:chests" | "c:barrels" | "forge:barrels"
    ) || tag.starts_with("c:ores/")
        || tag.starts_with("forge:ores/")
        || tag.ends_with(":ores")
}

// ---------- deterministic binary codec (create-adapter wire style) ----------

#[allow(dead_code)]
fn mob_w_i32(out: &mut Vec<u8>, v: i32) {
    out.extend_from_slice(&v.to_be_bytes());
}

#[allow(dead_code)]
fn mob_w_f32(out: &mut Vec<u8>, v: f32) {
    out.extend_from_slice(&v.to_be_bytes());
}

#[allow(dead_code)]
fn mob_w_f64(out: &mut Vec<u8>, v: f64) {
    out.extend_from_slice(&v.to_be_bytes());
}

#[allow(dead_code)]
fn mob_r_i32(bytes: &[u8], cursor: &mut usize) -> Option<i32> {
    let s = bytes.get(*cursor..*cursor + 4)?;
    *cursor += 4;
    Some(i32::from_be_bytes(s.try_into().ok()?))
}

#[allow(dead_code)]
fn mob_r_f32(bytes: &[u8], cursor: &mut usize) -> Option<f32> {
    let s = bytes.get(*cursor..*cursor + 4)?;
    *cursor += 4;
    Some(f32::from_be_bytes(s.try_into().ok()?))
}

#[allow(dead_code)]
fn mob_r_f64(bytes: &[u8], cursor: &mut usize) -> Option<f64> {
    let s = bytes.get(*cursor..*cursor + 8)?;
    *cursor += 8;
    Some(f64::from_be_bytes(s.try_into().ok()?))
}

#[allow(dead_code)]
fn mob_w_snapshot(out: &mut Vec<u8>, snap: &ContraptionSnapshot) {
    out.extend_from_slice(&snap.id.to_be_bytes());
    mob_w_i32(out, snap.dim);
    for v in snap.anchor {
        mob_w_i32(out, v);
    }
    match snap.kind {
        ContraptionKind::Bearing {
            angular_deg_per_tick,
        } => {
            out.push(0);
            mob_w_f32(out, angular_deg_per_tick);
        }
        ContraptionKind::Linear { velocity_per_tick } => {
            out.push(1);
            for v in velocity_per_tick {
                mob_w_f64(out, v);
            }
        }
        ContraptionKind::BoundedLinear {
            velocity_per_tick,
            min_offset,
            max_offset,
            reverse_at_bounds,
        } => {
            out.push(2);
            for v in velocity_per_tick {
                mob_w_f64(out, v);
            }
            for v in min_offset {
                mob_w_f64(out, v);
            }
            for v in max_offset {
                mob_w_f64(out, v);
            }
            out.push(u8::from(reverse_at_bounds));
        }
        ContraptionKind::Minecart {
            speed_per_tick,
            heading_yaw,
        } => {
            out.push(3);
            mob_w_f64(out, speed_per_tick);
            mob_w_f64(out, heading_yaw);
        }
    }
    mob_w_f32(out, snap.yaw_deg);
    mob_w_f32(out, snap.pitch_deg);
    for v in snap.translation {
        mob_w_f64(out, v);
    }
    mod_wire::write_varint(out, snap.cells.len() as u32);
    for (local, name) in &snap.cells {
        for v in *local {
            mob_w_i32(out, v);
        }
        mod_wire::write_mc_string(out, name);
    }
    mod_wire::write_varint(out, snap.storage.capacity_slots() as u32);
    mod_wire::write_varint(out, snap.storage.used_slots() as u32);
    for (item_id, count) in snap.storage.slots() {
        mod_wire::write_mc_string(out, item_id);
        out.extend_from_slice(&count.to_be_bytes());
    }
    mod_wire::write_varint(out, snap.actors.len() as u32);
    for actor in &snap.actors {
        for v in actor.local {
            mob_w_i32(out, v);
        }
        for v in actor.facing {
            mob_w_i32(out, v);
        }
        match &actor.kind {
            ContraptionActorKind::MechanicalDrill => out.push(0),
            ContraptionActorKind::MechanicalSaw => out.push(1),
            ContraptionActorKind::MechanicalHarvester => out.push(2),
            ContraptionActorKind::MechanicalPlough => out.push(3),
            ContraptionActorKind::MechanicalDeployer { filter_item: None } => out.push(4),
            ContraptionActorKind::MechanicalDeployer {
                filter_item: Some(item),
            } => {
                out.push(5);
                mod_wire::write_mc_string(out, item);
            }
            ContraptionActorKind::PortableStorageInterface => out.push(6),
        }
    }
}

#[allow(dead_code)]
fn mob_r_snapshot(bytes: &[u8], cursor: &mut usize) -> Option<ContraptionSnapshot> {
    let id = u32::from_be_bytes(bytes.get(*cursor..*cursor + 4)?.try_into().ok()?);
    *cursor += 4;
    let dim = mob_r_i32(bytes, cursor)?;
    let mut anchor = [0; 3];
    for v in &mut anchor {
        *v = mob_r_i32(bytes, cursor)?;
    }
    let kind = match *bytes.get(*cursor)? {
        0 => {
            *cursor += 1;
            ContraptionKind::Bearing {
                angular_deg_per_tick: mob_r_f32(bytes, cursor)?,
            }
        }
        1 => {
            *cursor += 1;
            let mut velocity_per_tick = [0.0; 3];
            for v in &mut velocity_per_tick {
                *v = mob_r_f64(bytes, cursor)?;
            }
            ContraptionKind::Linear { velocity_per_tick }
        }
        2 => {
            *cursor += 1;
            let mut velocity_per_tick = [0.0; 3];
            for v in &mut velocity_per_tick {
                *v = mob_r_f64(bytes, cursor)?;
            }
            let mut min_offset = [0.0; 3];
            for v in &mut min_offset {
                *v = mob_r_f64(bytes, cursor)?;
            }
            let mut max_offset = [0.0; 3];
            for v in &mut max_offset {
                *v = mob_r_f64(bytes, cursor)?;
            }
            let rev_byte = *bytes.get(*cursor)?;
            *cursor += 1;
            let reverse_at_bounds = match rev_byte {
                0 => false,
                1 => true,
                _ => return None,
            };
            ContraptionKind::BoundedLinear {
                velocity_per_tick,
                min_offset,
                max_offset,
                reverse_at_bounds,
            }
        }
        3 => {
            *cursor += 1;
            let speed_per_tick = mob_r_f64(bytes, cursor)?;
            let heading_yaw = mob_r_f64(bytes, cursor)?;
            ContraptionKind::Minecart {
                speed_per_tick,
                heading_yaw,
            }
        }
        _ => return None,
    };
    kind.validate().ok()?;
    let yaw_deg = mob_r_f32(bytes, cursor)?;
    let pitch_deg = mob_r_f32(bytes, cursor)?;
    let mut translation = [0.0; 3];
    for v in &mut translation {
        *v = mob_r_f64(bytes, cursor)?;
    }
    let cell_count = mod_wire::read_varint(bytes, cursor)? as usize;
    if cell_count > DEFAULT_MAX_CONTRAPTION_BLOCKS {
        return None;
    }
    let mut cells = Vec::with_capacity(cell_count);
    for _ in 0..cell_count {
        let mut local = [0; 3];
        for v in &mut local {
            *v = mob_r_i32(bytes, cursor)?;
        }
        let name = mod_wire::read_mc_string(bytes, cursor, mod_wire::MAX_REGISTRY_DATA_NAME_LEN)?;
        cells.push((local, name));
    }
    let capacity_slots = mod_wire::read_varint(bytes, cursor)? as usize;
    if capacity_slots > DEFAULT_MAX_CONTRAPTION_BLOCKS {
        return None;
    }
    let used_slots = mod_wire::read_varint(bytes, cursor)? as usize;
    if used_slots > capacity_slots {
        return None;
    }
    let mut storage = ContraptionStorage::new(capacity_slots);
    for _ in 0..used_slots {
        let item_id =
            mod_wire::read_mc_string(bytes, cursor, mod_wire::MAX_REGISTRY_DATA_NAME_LEN)?;
        let count_bytes = bytes.get(*cursor..*cursor + 8)?;
        *cursor += 8;
        let mut remaining = u64::from_be_bytes(count_bytes.try_into().ok()?);
        if remaining == 0 {
            return None;
        }
        while remaining > 0 {
            let chunk = remaining.min(u64::from(u32::MAX)) as u32;
            if storage.insert_item(&item_id, chunk) != 0 {
                return None;
            }
            remaining -= u64::from(chunk);
        }
    }
    let actor_count = mod_wire::read_varint(bytes, cursor)? as usize;
    if actor_count > DEFAULT_MAX_CONTRAPTION_BLOCKS {
        return None;
    }
    let mut actors = Vec::with_capacity(actor_count);
    for _ in 0..actor_count {
        let mut local = [0; 3];
        for v in &mut local {
            *v = mob_r_i32(bytes, cursor)?;
        }
        let mut facing = [0; 3];
        for v in &mut facing {
            *v = mob_r_i32(bytes, cursor)?;
        }
        let tag = *bytes.get(*cursor)?;
        *cursor += 1;
        let actor_kind = match tag {
            0 => ContraptionActorKind::MechanicalDrill,
            1 => ContraptionActorKind::MechanicalSaw,
            2 => ContraptionActorKind::MechanicalHarvester,
            3 => ContraptionActorKind::MechanicalPlough,
            4 => ContraptionActorKind::MechanicalDeployer { filter_item: None },
            5 => {
                let item =
                    mod_wire::read_mc_string(bytes, cursor, mod_wire::MAX_REGISTRY_DATA_NAME_LEN)?;
                ContraptionActorKind::MechanicalDeployer {
                    filter_item: Some(item),
                }
            }
            6 => ContraptionActorKind::PortableStorageInterface,
            _ => return None,
        };
        actors.push(ContraptionActor {
            local,
            facing,
            kind: actor_kind,
        });
    }
    Some(ContraptionSnapshot {
        id,
        dim,
        anchor,
        kind,
        yaw_deg,
        pitch_deg,
        translation,
        cells,
        storage,
        actors,
    })
}

// ---------- global bridge + tick hook ----------

/// The process-wide bridge, lazily armed from `MEGU_MOD_OFFLOAD`.
fn mod_offload_bridge() -> &'static Mutex<ModOffloadBridge> {
    static BRIDGE: OnceLock<Mutex<ModOffloadBridge>> = OnceLock::new();
    BRIDGE.get_or_init(|| Mutex::new(ModOffloadBridge::from_env()))
}

/// Per-tick entry wired from `game_tick` as `run("mod_offload", mod_offload_tick)`.
fn mod_offload_tick() {
    let tick = TICK_IDX.load(Ordering::Relaxed);
    let (edit_groups, had_work) = {
        let Ok(mut bridge) = mod_offload_bridge().lock() else {
            return;
        };
        if !bridge.has_work() {
            return;
        }
        let out = bridge.step_tick(tick);
        { let had_work = out.did_any_work(); (out.actor_edit_groups, had_work) }
    };
    // The process-wide offload bridge mutex is released here, before any live
    // world write. World application must never call back into a held bridge.
    let mut success_groups = Vec::with_capacity(edit_groups.len());
    for group in &edit_groups {
        success_groups.push(
            group
                .iter()
                .map(|edit| edit.as_ref().is_some_and(mob_commit_world_block_edit))
                .collect::<Vec<bool>>(),
        );
    }
    if let Ok(mut bridge) = mod_offload_bridge().lock() {
        let applied: u64 = success_groups
            .iter()
            .flat_map(|group| group.iter())
            .filter(|&&ok| ok)
            .count() as u64;
        bridge.world_block_edits_applied =
            bridge.world_block_edits_applied.saturating_add(applied);
        bridge.finalize_staged_actor_batches(&success_groups);
        if !had_work && applied == 0 {
            bridge.idle_ticks_skipped = bridge.idle_ticks_skipped.saturating_add(1);
        }
    }
}

/// Configuration-phase packets a joining NeoForge client needs for the custom
/// block space; empty on a vanilla server.
#[allow(dead_code)]
fn mod_offload_config_payloads() -> Vec<(u8, Vec<u8>)> {
    mod_offload_bridge()
        .lock()
        .map(|b| b.config_payloads())
        .unwrap_or_default()
}

/// Forward loaded mod ore/chest blocks into the anti-xray hidden set.
/// Returns how many state IDs were newly hidden.
#[allow(dead_code)]
fn mod_offload_sync_antixray_tags() -> usize {
    mod_offload_bridge()
        .lock()
        .map(|mut b| b.sync_antixray_tags())
        .unwrap_or(0)
}

/// Whether the bridge holds state Save v2 must persist — the `MOF1` trailer is
/// emitted only then, keeping untouched vanilla saves byte-identical.
#[allow(dead_code)]
fn mod_offload_has_state() -> bool {
    mod_offload_bridge()
        .lock()
        .map(|b| b.has_persistent_state())
        .unwrap_or(false)
}

/// Save v2 bytes for the whole offload state (placed blocks + contraptions).
#[allow(dead_code)]
fn mod_offload_save_bytes() -> Vec<u8> {
    mod_offload_bridge()
        .lock()
        .map(|b| b.save_bytes())
        .unwrap_or_default()
}

/// Restore `mod_offload_save_bytes`; `false` on lock failure or a corrupt buffer
/// (the existing state is left untouched either way).
#[allow(dead_code)]
fn mod_offload_load_bytes(bytes: &[u8]) -> bool {
    mod_offload_bridge()
        .lock()
        .map(|mut b| b.load_bytes(bytes).is_ok())
        .unwrap_or(false)
}

#[cfg(all(test, feature = "create-offload"))]
mod mod_offload_bridge_tests {
    use super::*;
    use create_adapter::contraption_adapter::{ActorCell, ActorWorldEdit, SuperGlueBox};

    /// Several tests here reprogram the process-global bridge; serialize them.
    static MOB_LOCK: Mutex<()> = Mutex::new(());

    fn fresh_bridge() {
        *mod_offload_bridge().lock().unwrap_or_else(|e| e.into_inner()) = ModOffloadBridge::new();
    }

    #[test]
    fn mod_offload_bridge_glue_actors_and_checked_disassembly() {
        struct StoneAt([i32; 3]);
        impl ContraptionWorldQuery for StoneAt {
            fn cell_at(&self, pos: [i32; 3]) -> ActorCell {
                if pos == self.0 {
                    ActorCell::block("minecraft:stone", true)
                } else {
                    ActorCell::air()
                }
            }
        }

        let mut bridge = ModOffloadBridge::new();
        let dim = DimId::from_raw(6);
        let seed = [10, 64, 10];
        let attached = [11, 64, 10];
        let unglued = [12, 64, 10];
        let mut world = BTreeMap::from([
            (seed, (100, "minecraft:stone".to_string())),
            (attached, (101, "minecraft:oak_planks".to_string())),
            (unglued, (102, "minecraft:stone".to_string())),
        ]);
        let glue = SuperGlueVolume::from_boxes(vec![SuperGlueBox::new(seed, attached)]);
        let driver = ContraptionKind::Linear {
            velocity_per_tick: [0.0; 3],
        };

        world.insert(attached, (101, "minecraft:bedrock".to_string()));
        assert_eq!(
            bridge.assemble_contraption_with_glue(dim, seed, seed, &world, &glue, driver),
            Err(ContraptionError::ImmovableBlock { pos: attached })
        );
        assert_eq!(bridge.next_contraption_id, 1);
        assert_eq!(bridge.offload.assembly_count(), 0);
        world.insert(attached, (101, "minecraft:oak_planks".to_string()));

        let (id, removals) = bridge
            .assemble_contraption_with_glue(dim, seed, seed, &world, &glue, driver)
            .unwrap();
        assert_eq!(id, 1);
        assert_eq!(removals.len(), 2);
        assert!(removals.iter().all(|edit| edit.state_id.is_none()));
        assert!(!removals.iter().any(|edit| edit.pos.x == unglued[0]));

        assert!(bridge.mount_contraption_actor(
            id,
            ContraptionActor {
                local: [1, 0, 0],
                facing: [1, 0, 0],
                kind: ContraptionActorKind::MechanicalDrill,
            },
        ));
        let report = bridge.step_contraption_actors(id, &StoneAt(unglued)).unwrap();
        assert_eq!(report.edits, vec![ActorWorldEdit::BreakBlock {
            pos: ModBlockPos::new(dim.raw() as i32, unglued[0], unglued[1], unglued[2])
        }]);
        assert!(bridge.step_contraption_actors(id + 1, &StoneAt(unglued)).is_none());

        assert!(bridge.note_contraption_rider(900, id, [10.0, 64.0, 10.0]));
        let conflict = ModBlockPos::new(dim.raw() as i32, attached[0], attached[1], attached[2]);
        assert_eq!(
            bridge.disassemble_contraption_checked(id, |pos| pos == conflict),
            Err(vec![conflict])
        );
        assert!(bridge.offload.assembly(id).is_some());
        assert!(bridge.contraption_riders.contains_key(&900));

        let edits = bridge.disassemble_contraption_checked(id, |_| false).unwrap();
        assert_eq!(edits.len(), 2);
        assert_eq!(
            edits.iter().filter_map(|edit| edit.state_id).collect::<Vec<_>>(),
            vec![100, 101]
        );
        assert!(bridge.offload.assembly(id).is_none());
        assert!(!bridge.contraption_riders.contains_key(&900));
        assert_eq!(bridge.disassemble_contraption_checked(id, |_| false), Err(Vec::new()));
    }

    /// Synthetic `Client` (same fixture style as `entity_fluid`'s tests) — a
    /// loopback socket, survival game mode, and an unregistered dimension slot so
    /// `block_at_dim`/`ac_server_supported` read cheap all-air terrain.
    fn mk_client(name: &str, eid: i32, dim: DimId, pos: (f64, f64, f64)) -> Arc<Client> {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stream = TcpStream::connect(addr).unwrap();
        let _accepted = listener.accept().unwrap();
        Arc::new(Client {
            eid,
            uuid: [7u8; 16],
            name: name.to_string(),
            player_attack_damage_id: None,
            pos: Mutex::new((pos.0, pos.1, pos.2, 0.0, 0.0, true)),
            teleport: Mutex::new(Default::default()),
            hp: Mutex::new(20.0),
            sock: Mutex::new(stream),
            inv: Mutex::new(HashMap::new()),
            cursor: Mutex::new((0, 0, Vec::new())),
            drag: Mutex::new((-1, Vec::new())),
            open_chest: Mutex::new(None),
            chest_sync: Mutex::new(None),
            open_furnace: Mutex::new(None),
            craft: Mutex::new(HashMap::new()),
            craft9: Mutex::new(HashMap::new()),
            open_table: Mutex::new(false),
            open_merchant: Mutex::new(false),
            trade_sel: Mutex::new(-1),
            merchant_in: Mutex::new([(0, 0, Vec::new()), (0, 0, Vec::new())]),
            bow_draw: Mutex::new(None),
            dig: Mutex::new(None),
            eat_start: Mutex::new(None),
            blocking: Mutex::new(false),
            sprinting: Mutex::new(false),
            sneaking: Mutex::new(false),
            last_melee: Mutex::new(Instant::now() - Duration::from_secs(10)),
            fall_fly: Mutex::new(false),
            invulnerable: Mutex::new(0),
            last_hurt: Mutex::new(0.0),
            shield_cd: Mutex::new(Instant::now() - Duration::from_secs(10)),
            absorb: Mutex::new(0.0),
            lod_v1_enabled: AtomicBool::new(false),
            underground_revealed: AtomicBool::new(false),
            sleeping: Mutex::new(None),
            inv_state_id: AtomicI32::new(0),
            generic_kill_pending: AtomicBool::new(false),
            use_on_resend: Mutex::new(None),
            pearl_cd: Mutex::new(Instant::now() - Duration::from_secs(10)),
            xp: Mutex::new(0),
            skin: Mutex::new(None),
            seen: Mutex::new(HashSet::new()),
            chunks: Mutex::new(HashSet::new()),
            chunk_ack: Mutex::new(ChunkFeedback::new(1)),
            gen_sources: Mutex::new(GenSources::default()),
            gm: Mutex::new(0),
            fall_from: Mutex::new(pos.1),
            food: Mutex::new(20),
            air: Mutex::new(server_ticks(AIR_SUPPLY_GT) as i32),
            armor_pts: Mutex::new(0),
            sat: Mutex::new(5.0),
            walk_acc: Mutex::new(0.0),
            lastp: Mutex::new(pos),
            spawn_pt: Mutex::new(None),
            dim: Mutex::new(dim),
            spawn_dim: Mutex::new(dim),
            view: Mutex::new(2),
            in_portal: Mutex::new(false),
            portal_at: Mutex::new(None),
            held: Mutex::new(0),
            effs: Mutex::new(Vec::new()),
        })
    }

    /// Vanilla sprint-jump allowance is ~0.564 blocks/tick; every carry below
    /// exceeds it, so a missing grant would flag on the first move.
    #[test]
    fn mod_offload_belt_and_contraption_carry_prevents_ac_rubberband() {
        let _serial = MOB_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        fresh_bridge();
        let dim = DimId::from_raw(9);
        let belt_eid = 977_001;
        let lift_eid = 977_002;
        let hack_eid = 977_003;
        let belt = mk_client("mob_belt_rider", belt_eid, dim, (0.5, 80.0, 0.5));
        let lift = mk_client("mob_lift_rider", lift_eid, dim, (102.5, 65.0, 100.5));
        let hack = mk_client("mob_speed_hacker", hack_eid, dim, (0.5, 80.0, 200.5));
        for c in [&belt, &lift, &hack] {
            ac_test_set_player_preset(&c.name, AcPreset::Standard);
        }

        // A 3-block bearing arm at (100,64,100), one rider at the arm tip, and a
        // belt rider carried at 1.2 blocks/tick — over twice the walk allowance.
        {
            let mut b = mod_offload_bridge().lock().unwrap_or_else(|e| e.into_inner());
            b.enable_create();
            let shaft = b
                .create_registry()
                .unwrap()
                .default_state("create:shaft")
                .unwrap();
            let mut world = BTreeMap::new();
            world.insert([100, 64, 100], shaft);
            world.insert([101, 64, 100], shaft);
            world.insert([102, 64, 100], shaft);
            let (cid, edits) = b
                .assemble_contraption(
                    dim,
                    [100, 64, 100],
                    [100, 64, 100],
                    &world,
                    ContraptionKind::Bearing {
                        angular_deg_per_tick: 30.0,
                    },
                )
                .unwrap();
            assert_eq!(edits.len(), 3);
            assert!(b.note_contraption_rider(lift_eid, cid, [102.5, 65.0, 100.5]));
            b.note_belt_rider(belt_eid, [1.2, 0.0, 0.0]);
        }

        // Baseline small legal moves: installs the per-eid anti-cheat entries the
        // carry grant then lands in.
        for c in [&belt, &lift, &hack] {
            let v = ac_check_move(c, (0.5, 80.0, 0.5), (0.6, 80.0, 0.5), false);
            assert!(matches!(v, AcMoveVerdict::Accept { .. }));
        }

        // Three sustained carried ticks: each grants the exact delta, every fed
        // move must be accepted with the on-ground claim intact.
        let mut belt_pos = (0.5, 80.0, 0.5);
        let mut lift_from = [102.5, 65.0, 100.5];
        for _ in 0..3 {
            mod_offload_tick();

            let belt_to = (belt_pos.0 + 1.2, belt_pos.1, belt_pos.2);
            let v = ac_check_move(&belt, belt_pos, belt_to, true);
            assert!(
                matches!(v, AcMoveVerdict::Accept { ground: true }),
                "carried belt move must accept with ground intact, got {v:?}"
            );
            belt_pos = belt_to;

            let lift_anchor = mod_offload_bridge().lock().unwrap_or_else(|e| e.into_inner()).contraption_riders
                [&lift_eid]
                .anchor;
            let v = ac_check_move(
                &lift,
                (lift_from[0], lift_from[1], lift_from[2]),
                (lift_anchor[0], lift_anchor[1], lift_anchor[2]),
                true,
            );
            assert!(
                matches!(v, AcMoveVerdict::Accept { ground: true }),
                "carried contraption move must accept with ground intact, got {v:?}"
            );
            lift_from = lift_anchor;
        }
        for eid in [belt_eid, lift_eid] {
            assert_eq!(ac_flag_count(eid, AcCheckId::Movement), 0, "eid {eid}");
            assert_eq!(ac_flag_count(eid, AcCheckId::NoFall), 0, "eid {eid}");
        }

        // The same distance WITHOUT a carry grant still trips the speed check:
        // an uncarried 8-block/tick teleport drains the 40-tick burst credit and
        // gets a setback within a few packets.
        let mut flagged = false;
        let mut hx = 0.5;
        for _ in 0..8 {
            let to = hx + 8.0;
            if matches!(
                ac_check_move(&hack, (hx, 80.0, 200.5), (to, 80.0, 200.5), false),
                AcMoveVerdict::Setback
            ) {
                flagged = true;
            }
            hx = to;
        }
        assert!(flagged, "uncarried speed hack must rubber-band");
        assert!(ac_flag_count(hack_eid, AcCheckId::Movement) > 0);
    }

    #[test]
    fn mod_offload_config_wire_roundtrips_create_and_universal_profiles() {
        let _serial = MOB_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        {
            let mut b = mod_offload_bridge().lock().unwrap_or_else(|e| e.into_inner());
            *b = ModOffloadBridge::new();
            b.enable_create();
            b.load_profiles(mod_profile::builtin_reference_profiles())
                .unwrap();
        }
        let payloads = mod_offload_config_payloads();

        // First frame: the merged `minecraft:block` registry data.
        let (pid, body) = &payloads[0];
        assert_eq!(*pid, mod_wire::CONFIG_REGISTRY_DATA_PACKET_ID);
        let (registry, entries) = mod_wire::decode_config_registry_data_0x07(body).unwrap();
        assert_eq!(registry, "minecraft:block");
        let expected_names: Vec<String> = {
            let b = mod_offload_bridge().lock().unwrap_or_else(|e| e.into_inner());
            let mut names: Vec<String> = b
                .create_registry()
                .unwrap()
                .blocks()
                .iter()
                .map(|d| d.id.clone())
                .collect();
            names.extend(b.universal.as_ref().unwrap().registry().blocks().iter().map(|d| d.id.clone()));
            names
        };
        assert_eq!(entries, expected_names);

        // Then one (start, snapshot, completed) frame triple per active registry.
        let channels = [
            create_wire::CHANNEL_FROZEN_REGISTRY_SYNC_START,
            create_wire::CHANNEL_FROZEN_REGISTRY,
            create_wire::CHANNEL_FROZEN_REGISTRY_SYNC_COMPLETED,
        ];
        let frames = &payloads[1..];
        assert_eq!(frames.len(), 6);
        fn states_of<R>(
            reg: &mod_adapter_core::blockstates::BlockRegistry<R>,
        ) -> (usize, Vec<(u32, String)>) {
            (
                reg.state_count(),
                reg.states()
                    .iter()
                    .map(|e| (e.state_id, e.canonical.clone()))
                    .collect(),
            )
        }
        let registries: Vec<(usize, Vec<(u32, String)>)> = {
            let b = mod_offload_bridge().lock().unwrap_or_else(|e| e.into_inner());
            vec![
                states_of(b.create_registry().unwrap()),
                states_of(b.universal.as_ref().unwrap().registry()),
            ]
        };
        for (i, (pid, body)) in frames.iter().enumerate() {
            assert_eq!(*pid, CONFIG_CUSTOM_PAYLOAD_ID);
            let mut cursor = 1usize;
            let channel =
                mod_wire::read_mc_string(body, &mut cursor, mod_wire::MAX_REGISTRY_DATA_NAME_LEN)
                    .unwrap();
            assert_eq!(channel, channels[i % 3]);
            if i % 3 != 1 {
                continue;
            }
            // The frozen-registry snapshot round-trips every expanded state.
            let snap = &body[cursor..];
            let mut sc = 0usize;
            assert_eq!(
                mod_wire::read_mc_string(snap, &mut sc, 128).as_deref(),
                Some("minecraft:block")
            );
            let count = mod_wire::read_varint(snap, &mut sc).unwrap() as usize;
            let (expected_count, expected_states) = &registries[i / 3];
            assert_eq!(count, *expected_count);
            let mut decoded = Vec::with_capacity(count);
            for _ in 0..count {
                let state_id = mod_wire::read_varint(snap, &mut sc).unwrap();
                let canonical =
                    mod_wire::read_mc_string(snap, &mut sc, mod_wire::MAX_REGISTRY_DATA_NAME_LEN)
                        .unwrap();
                decoded.push((state_id, canonical));
            }
            assert_eq!(sc, snap.len());
            assert_eq!(&decoded, expected_states);
        }

        // Anti-xray tag sync is wired into the load path: `load_profiles` and
        // `declare_tag` fold tag members and name-heuristic blocks into the
        // hidden set themselves — no manual `sync_antixray_tags` call needed.
        {
            let mut b = mod_offload_bridge().lock().unwrap_or_else(|e| e.into_inner());
            // `quartz_ore` matches the `*_ore` name heuristic, so profile load
            // alone hides it; `loot_vault` shares no ore/chest/barrel
            // substring, so only its `#forge:chests` tag membership can hide it.
            let custom = ModOffloadProfile::from_json(
                br#"{"mod_id":"testmod","blocks":[{"id":"testmod:quartz_ore","role":"decorative"},{"id":"testmod:loot_vault","role":"decorative"}]}"#,
            )
            .unwrap();
            b.load_profiles(vec![custom]).unwrap();
            let quartz_states = b.mod_state_ids_for_block("testmod:quartz_ore");
            assert!(!quartz_states.is_empty());
            assert!(quartz_states.iter().all(|st| b.mod_state_ids.contains(st)));
            let vault_states = b.mod_state_ids_for_block("testmod:loot_vault");
            assert!(!vault_states.is_empty());
            assert!(
                vault_states.iter().all(|st| !b.mod_state_ids.contains(st)),
                "loot_vault must not be hidden before its tag is declared"
            );
            {
                let extra = hidden_extra().lock().unwrap_or_else(|e| e.into_inner());
                for st in &quartz_states {
                    assert!(extra.contains(st));
                }
            }
            b.declare_tag("forge:chests", ["testmod:loot_vault", "minecraft:chest"]);
            b.declare_tag("c:ores", ["minecraft:gold_ore"]);
            assert!(vault_states.iter().all(|st| b.mod_state_ids.contains(st)));
            {
                let extra = hidden_extra().lock().unwrap_or_else(|e| e.into_inner());
                for st in quartz_states.iter().chain(&vault_states) {
                    assert!(extra.contains(st));
                }
                if let Some(reg) = breg() {
                    for st in 0..reg.state_count() as u32 {
                        if matches!(reg.name_of(st), "minecraft:chest" | "minecraft:gold_ore") {
                            assert!(extra.contains(&st));
                        }
                    }
                }
            }
            assert!(HIDDEN_EXTRA_USED.load(Ordering::Relaxed));
            // The entrypoint stays available for explicit re-syncs and is
            // idempotent once the load path already folded everything in.
            assert_eq!(b.sync_antixray_tags(), 0);
        }
    }

    #[test]
    fn mod_offload_empty_bridge_emits_zero_vanilla_packets() {
        let _serial = MOB_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        fresh_bridge();
        assert!(mod_offload_config_payloads().is_empty());
        assert!(!mod_offload_bridge().lock().unwrap_or_else(|e| e.into_inner()).has_work());
        mod_offload_tick();
        let b = mod_offload_bridge().lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(b.ticks_stepped, 0, "empty bridge must be a no-op, not ticked");
    }

    #[test]
    fn mod_offload_save_load_roundtrips_contraption_state() {
        let _serial = MOB_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut src = ModOffloadBridge::new();
        src.enable_create();
        let wheel_x = src
            .create_registry()
            .unwrap()
            .with_property(
                src.create_registry()
                    .unwrap()
                    .default_state("create:water_wheel")
                    .unwrap(),
                "axis",
                "x",
            )
            .unwrap();
        let shaft_x = src
            .create_registry()
            .unwrap()
            .with_property(
                src.create_registry()
                    .unwrap()
                    .default_state("create:shaft")
                    .unwrap(),
                "axis",
                "x",
            )
            .unwrap();
        let p_wheel = ModBlockPos::new(0, 0, 64, 0);
        let p_shaft = ModBlockPos::new(0, 1, 64, 0);
        src.place_create_block(p_wheel, wheel_x);
        src.place_create_block(p_shaft, shaft_x);
        let sol = src
            .offload
            .factory()
            .unwrap()
            .kinetic_solution(p_shaft)
            .unwrap();
        assert!(
            sol.speed.abs() > 0.0,
            "water wheel should drive the shaft (kinetic network running)"
        );

        let shaft = src
            .create_registry()
            .unwrap()
            .default_state("create:shaft")
            .unwrap();
        let mut world = BTreeMap::new();
        world.insert([10, 64, 10], shaft);
        world.insert([11, 64, 10], shaft);
        world.insert([12, 64, 10], shaft);
        let (cid, _) = src
            .assemble_contraption(
                DimId::Overworld,
                [10, 64, 10],
                [10, 64, 10],
                &world,
                ContraptionKind::Bearing {
                    angular_deg_per_tick: 15.0,
                },
            )
            .unwrap();
        src.step_tick(1);
        src.step_tick(2);

        let bytes = src.save_bytes();
        assert_eq!(src.save_bytes(), bytes, "save must be byte-deterministic");

        let mut dst = ModOffloadBridge::new();
        dst.load_bytes(&bytes).unwrap();
        // The kinetic network rebuilt: wheel + shaft re-placed and driving.
        let sol = dst
            .offload
            .factory()
            .unwrap()
            .kinetic_solution(p_shaft)
            .unwrap();
        assert!(sol.speed.abs() > 0.0);
        // The contraption came back with pose, kind and blocks intact.
        let restored = dst.offload.assembly(cid).unwrap();
        assert_eq!(restored.block_count(), 3);
        assert!((restored.yaw_deg - 30.0).abs() < 1e-6);
        let snap_src = src
            .offload
            .assembly(cid)
            .unwrap()
            .snapshot(src.create_registry().unwrap());
        let snap_dst = restored.snapshot(dst.create_registry().unwrap());
        assert_eq!(snap_src, snap_dst);
        // And the loaded bridge serializes back to identical bytes.
        assert_eq!(dst.save_bytes(), bytes);
    }

    #[test]
    fn mega_w3_offload_bridge_applies_contraption_world_edits_and_steps_parallel_deterministically()
    {
        struct StoneTargets(Vec<[i32; 3]>);
        impl ContraptionWorldQuery for StoneTargets {
            fn cell_at(&self, pos: [i32; 3]) -> ActorCell {
                if self.0.contains(&pos) {
                    ActorCell::block("minecraft:stone", true)
                } else {
                    ActorCell::air()
                }
            }
        }

        let _w = world_guard();
        let reg = core_create_registry(DEFAULT_BASE_STATE_ID);
        let shaft = reg.default_state("create:shaft").unwrap();
        let wheel = reg.default_state("create:water_wheel").unwrap();
        let stone = name_to_state("minecraft:stone").unwrap();
        let dim = DimId::Overworld;

        // Three drill contraptions in far-apart regions; each drill's swept
        // cell sits one block -X of its anchor and holds real stone written
        // into the live world through the CAS path.
        let anchors = [
            [5_000, 200, 5_000],
            [20_000, 200, 20_000],
            [-15_000, 200, -15_000],
        ];
        let targets: Vec<[i32; 3]> =
            anchors.iter().map(|a| [a[0] - 1, a[1], a[2]]).collect();
        let actor_query = StoneTargets(targets.clone());
        let plant = |targets: &[[i32; 3]]| {
            for &t in targets {
                let old = block_at_dim(dim, t[0], t[1], t[2]);
                assert!(set_block_if_dim(dim, t[0], t[1], t[2], stone, Some(old)));
            }
        };
        plant(&targets);

        let build = |workers: usize| {
            let mut bridge = ModOffloadBridge::new();
            bridge.offload_workers = workers;
            // A placed Create block exercises the factory tier too.
            bridge.place_create_block(ModBlockPos::new(0, 6_000, 64, 6_000), wheel);
            for anchor in anchors {
                let mut blocks = BTreeMap::new();
                blocks.insert(anchor, shaft);
                let (cid, _) = bridge
                    .assemble_contraption(
                        dim,
                        anchor,
                        anchor,
                        &blocks,
                        ContraptionKind::Linear {
                            velocity_per_tick: [0.0, 0.0, 0.0],
                        },
                    )
                    .unwrap();
                assert!(bridge.mount_contraption_actor(
                    cid,
                    ContraptionActor {
                        local: [0, 0, 0],
                        facing: [-1, 0, 0],
                        kind: ContraptionActorKind::MechanicalDrill,
                    },
                ));
            }
            bridge
        };

        // Serial pass: the drill edits must reach the live world — nothing is
        // discarded any more.
        let mut serial = build(1);
        let out = serial.step_tick_with_actor_query(1, Some(&actor_query));
        assert!(out.error.is_none(), "offload step refused: {:?}", out.error);
        assert_eq!(out.world_block_edits.len(), 3);
        assert_eq!(serial.world_block_edits_applied, 3);
        for &t in &targets {
            assert_eq!(
                block_at_dim(dim, t[0], t[1], t[2]),
                0,
                "drill must have broken the stone at {t:?}"
            );
        }

        // Restore the broken cells, then the parallel pass must produce the
        // exact same edit stream and digest.
        plant(&targets);
        let mut parallel = build(8);
        let par_out = parallel.step_tick_with_actor_query(1, Some(&actor_query));
        assert!(par_out.error.is_none(), "offload step refused: {:?}", par_out.error);
        assert_eq!(par_out.world_block_edits, out.world_block_edits);
        assert_eq!(par_out.digest, out.digest);
        assert_eq!(parallel.offload.state_digest(), serial.offload.state_digest());
        assert_eq!(parallel.save_bytes(), serial.save_bytes());
        assert_eq!(
            parallel.world_block_edits_applied,
            serial.world_block_edits_applied
        );
        for &t in &targets {
            assert_eq!(block_at_dim(dim, t[0], t[1], t[2]), 0);
        }
    }

    #[test]
    fn staged_actor_finalize_is_failure_atomic_and_surfaces_overflow_once() {
        struct StoneAt;
        impl ContraptionWorldQuery for StoneAt {
            fn cell_at(&self, pos: [i32; 3]) -> ActorCell {
                if pos == [11, 64, 10] {
                    ActorCell::block("minecraft:stone", true)
                } else {
                    ActorCell::air()
                }
            }
        }

        let mut bridge = ModOffloadBridge::new();
        let dim = DimId::from_raw(6);
        let anchor = [10, 64, 10];
        let shaft = bridge
            .create_registry()
            .map(|_| 0)
            .unwrap_or(0);
        let _ = shaft;
        let registry = core_create_registry(DEFAULT_BASE_STATE_ID);
        let shaft = registry.default_state("create:shaft").unwrap();
        let mut blocks = BTreeMap::new();
        blocks.insert(anchor, shaft);
        let (cid, _) = bridge
            .assemble_contraption(
                dim,
                anchor,
                anchor,
                &blocks,
                ContraptionKind::Bearing { angular_deg_per_tick: 0.0 },
            )
            .unwrap();
        assert!(bridge.mount_contraption_actor(
            cid,
            ContraptionActor {
                local: [0, 0, 0],
                facing: [1, 0, 0],
                kind: ContraptionActorKind::MechanicalDrill,
            },
        ));
        let batch = bridge
            .with_contraption(cid, |contraption| {
                contraption.storage = ContraptionStorage::new(0);
                contraption.step_actors_staged(&StoneAt)
            })
            .unwrap();
        assert_eq!(batch.edits.len(), 1);

        bridge.pending_actor_batches.push(PendingActorBatch {
            contraption: cid,
            batch: batch.clone(),
        });
        assert!(bridge.finalize_staged_actor_batches(&[vec![false]]).is_empty());
        assert!(bridge.pending_actor_batches.is_empty());
        assert!(bridge.drain_committed_actor_drops().is_empty());

        bridge.pending_actor_batches.push(PendingActorBatch {
            contraption: cid,
            batch,
        });
        let drops = bridge.finalize_staged_actor_batches(&[vec![true]]);
        assert_eq!(drops.len(), 1);
        assert_eq!(drops[0].item_id, "minecraft:stone");
        assert_eq!(drops[0].count, 1);
        assert_eq!(bridge.drain_committed_actor_drops(), drops);
        assert!(bridge.drain_committed_actor_drops().is_empty());
    }

}
