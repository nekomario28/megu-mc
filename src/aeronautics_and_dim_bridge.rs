// Production runtime bridge: Create: Aeronautics 6-DOF sub-worlds, universal
// Create addon discovery, and dimension-adding mods wired into mc-spine's live
// subsystems.
//
// `include!`d into `main.rs` under `#[cfg(feature = "create-offload")]`, in the
// same flat root-scope style as `mod_offload_bridge.rs`, so the layers module
// graph is untouched: with the feature off nothing here is compiled or linked.
//
// Surfaces wired today:
//   * `AeronauticsRuntimeBridge` — a fleet of `RigidBodySubWorld` airships keyed
//     by `subworld_id`. `aeronautics_bridge_tick` steps every active ship (its
//     embedded live `CreateFactory` propeller/kinetic network included) and
//     grants each attached passenger their exact `passenger_world_delta`
//     through `ac_note_carried_motion`, so pilots and passengers on flying,
//     pitching, yawing airships are never rubber-banded or false-flagged by
//     NoFall.
//   * `aeronautics_bridge_save_bytes` / `aeronautics_bridge_load_bytes` — a
//     deterministic multi-airship Save v2 blob wrapping `AERO` v1 snapshots,
//     restored byte-for-byte.
//   * `aeronautics_bridge_discover_addon_jar` — zero-JVM discovery of arbitrary
//     Create addon JARs via `discover_create_addon_profile`; discovered
//     `#c:ores`/`#c:chests` hidden blocks flow into anti-xray through
//     `register_hidden_block_names`.
//   * `DimensionModRuntimeBridge` — `load_dimension_mod_entries` forwards pure
//     data-driven dimension mods straight into Track 2's
//     `worldgen::register_datapack_dimensions` (live `DimId`s), folds ore/chest
//     tag members into anti-xray, and installs a `DimensionEnvProfile` per
//     `DimId` (`dim_gravity_multiplier`, `dim_supports_underground_occlusion`).
//     `try_mod_portal_teleport` turns a validated `PortalTransition` into a
//     `TeleportIntent` and grants `ac_expect_teleport` + `ac_grant_grace`, so
//     stepping through an Aether / Undergarden / Ad Astra portal can never
//     false-flag movement checks.

use create_adapter::addon_profile::{
    builtin_create_addon_profiles, create_registry_with_addons,
    discover_create_addon_profile, CreateAddonProfile,
};
use create_adapter::aeronautics::{
    decode_aeronautics_snapshot, encode_aeronautics_snapshot, RigidBodySubWorld,
};
use mod_adapter_core::data_loader as mod_data;
use mod_adapter_core::dimension_mod as dim_mod;

/// Multi-airship Save v2 bridge codec: `"MSAE"` + version byte, then VarInt
/// ship count and `(u64 id, u32 len, AERO snapshot)` records in `subworld_id`
/// order — deterministic like every other save codec in this file's family.
const ADB_SAVE_MAGIC: [u8; 4] = *b"MSAE";
const ADB_SAVE_VERSION: u8 = 1;
/// Sanity bounds for the multi-ship blob; a real fleet is nowhere near these.
const ADB_MAX_SHIPS: usize = 4096;
const ADB_MAX_SHIP_BYTES: usize = 1 << 24;

/// Fold block names into the anti-xray hidden set the way
/// `register_hidden_tags_from_dir` does for datapack tags: vanilla ids resolve
/// through `breg()` into runtime state ids, custom mod ids through the offload
/// bridge's mod registries. Returns how many state ids were newly hidden;
/// names no registry knows yet are the caller's record to keep (mod ids whose
/// blocks register later still get hidden when they land).
#[allow(dead_code)]
fn register_hidden_block_names(names: impl IntoIterator<Item = impl AsRef<str>>) -> usize {
    let wanted: HashSet<String> = names
        .into_iter()
        .map(|name| ns_rl(name.as_ref()))
        .collect();
    if wanted.is_empty() {
        return 0;
    }
    let mut state_ids: Vec<u32> = Vec::new();
    if let Some(reg) = breg() {
        for st in 0..reg.state_count() as u32 {
            if wanted.contains(reg.name_of(st)) {
                state_ids.push(st);
            }
        }
    }
    if let Ok(offload) = mod_offload_bridge().lock() {
        for name in &wanted {
            state_ids.extend(offload.mod_state_ids_for_block(name));
        }
    }
    let mut added = 0usize;
    {
        let mut extra = hidden_extra().lock().unwrap_or_else(|e| e.into_inner());
        for st in state_ids {
            if extra.insert(st) {
                added += 1;
            }
        }
    }
    if added > 0 {
        HIDDEN_EXTRA_USED.store(true, Ordering::Relaxed);
    }
    added
}

// ---------------------------------------------------------------------------
// Aeronautics 6-DOF airship runtime bridge
// ---------------------------------------------------------------------------

/// Everything the aeronautics side of the bridge owns at runtime: the shared
/// `create:*` + addon block registry every sub-world's embedded
/// [`CreateFactory`] is built from, the live ship fleet, passenger
/// attachments, and addon profiles discovered from JAR scans.
#[allow(dead_code)]
struct AeronauticsRuntimeBridge {
    /// `create:*` + every loaded addon profile's defs, built lazily so an idle
    /// bridge carries no registry. Rebuilt whenever addon discovery adds a
    /// profile so later-assembled ships see the new blocks.
    registry: Option<CreateBlockRegistry>,
    /// Loaded addon profiles (builtins seeded at construction, discovered
    /// profiles folded in by `discover_addon_jar`).
    addons: Vec<CreateAddonProfile>,
    /// Live `RigidBodySubWorld` airships keyed by `subworld_id`.
    airships: BTreeMap<u64, RigidBodySubWorld>,
    /// `eid -> (subworld_id, local_offset)`; `local_offset` is the rider's
    /// local position measured from the ship's center of mass, exactly what
    /// `passenger_world_delta` consumes.
    passengers: BTreeMap<i32, (u64, [f64; 3])>,
    next_subworld_id: u64,
    /// Custom ore/chest ids discovery fed to anti-xray (they may predate any
    /// registered state id).
    mod_hidden_names: BTreeSet<String>,
    /// Ticks actually stepped — an empty bridge must never tick.
    ticks_stepped: u64,
}

#[allow(dead_code)]
impl AeronauticsRuntimeBridge {
    fn new() -> Self {
        Self {
            registry: None,
            addons: builtin_create_addon_profiles(),
            airships: BTreeMap::new(),
            passengers: BTreeMap::new(),
            next_subworld_id: 1,
            mod_hidden_names: BTreeSet::new(),
            ticks_stepped: 0,
        }
    }

    /// Build the shared `create:*` + addon registry on first use. A duplicate
    /// block id across two addon profiles fails closed — a half-built registry
    /// must never back a live ship.
    fn ensure_registry(&mut self) -> Result<(), String> {
        if self.registry.is_none() {
            self.registry = Some(
                create_registry_with_addons(DEFAULT_BASE_STATE_ID, &self.addons)
                    .map_err(|e| format!("aeronautics registry: {e:?}"))?,
            );
        }
        Ok(())
    }

    /// Fold a discovered/builtin addon profile in (re-discovery of the same
    /// `mod_id` replaces the old entry) and invalidate the shared registry.
    fn add_addon(&mut self, profile: CreateAddonProfile) {
        match self.addons.iter_mut().find(|a| a.mod_id == profile.mod_id) {
            Some(slot) => *slot = profile,
            None => self.addons.push(profile),
        }
        self.registry = None;
    }

    /// Assemble an airship from sparse local cells (canonical blockstate
    /// strings) under `subworld_id`. `addon_mod_id` picks the propulsion
    /// profile; `None` uses the first loaded profile that ships one
    /// (Create: Aeronautics on a default bridge), matching
    /// `PropulsionSpec::aeronautics_default` when no propulsion addon exists.
    fn assemble_airship(
        &mut self,
        subworld_id: u64,
        parent_dim: u32,
        cells: BTreeMap<[i32; 3], String>,
        addon_mod_id: Option<&str>,
        block_mass: f64,
    ) -> Result<(), String> {
        self.ensure_registry()?;
        let addon = match addon_mod_id {
            Some(id) => self.addons.iter().find(|a| a.mod_id == id),
            None => self.addons.iter().find(|a| a.propulsion.is_some()),
        };
        let sub = RigidBodySubWorld::assemble(
            subworld_id,
            parent_dim,
            cells,
            self.registry.as_ref().expect("ensure_registry just ran"),
            addon,
            block_mass,
        )
        .map_err(|e| format!("aeronautics assemble {subworld_id}: {e:?}"))?;
        self.add_airship(sub);
        Ok(())
    }

    /// Register an externally assembled sub-world under its own `sub.id`.
    fn add_airship(&mut self, sub: RigidBodySubWorld) {
        self.next_subworld_id = self.next_subworld_id.max(sub.id + 1);
        self.airships.insert(sub.id, sub);
    }

    /// Attach player `eid` to airship `subworld_id` at local `local_offset`.
    /// Every tick grants the exact rigid-carry delta into anti-cheat.
    fn attach_passenger(&mut self, eid: i32, subworld_id: u64, local_offset: [f64; 3]) -> bool {
        if !self.airships.contains_key(&subworld_id) {
            return false;
        }
        self.passengers.insert(eid, (subworld_id, local_offset));
        true
    }

    /// Player left the airship (dismount, disassembly, logout).
    fn detach_passenger(&mut self, eid: i32) -> bool {
        self.passengers.remove(&eid).is_some()
    }

    /// The exact world displacement the last step applied to passenger `eid`'s
    /// attached point — the same value `step` just granted to anti-cheat.
    fn passenger_delta(&self, eid: i32, dt: f64) -> Option<[f64; 3]> {
        let (ship, offset) = self.passengers.get(&eid)?;
        Some(self.airships.get(ship)?.passenger_world_delta(*offset, dt))
    }

    /// Anything worth ticking: live ships or attached passengers.
    fn has_work(&self) -> bool {
        !self.airships.is_empty() || !self.passengers.is_empty()
    }

    /// Advance every airship by `dt` seconds (0.05 at 20 TPS): each
    /// `RigidBodySubWorld` steps its embedded `CreateFactory` kinetic network,
    /// propeller thrust, buoyancy and Euler dynamics, then every attached
    /// passenger gets their exact `passenger_world_delta` granted through
    /// `ac_note_carried_motion`.
    fn step(&mut self, dt: f64, gravity: f64, air_density: f64) {
        self.ticks_stepped += 1;
        for sub in self.airships.values_mut() {
            sub.step(dt, gravity, air_density);
        }
        for (eid, (ship, offset)) in &self.passengers {
            let Some(sub) = self.airships.get(ship) else {
                continue;
            };
            ac_note_carried_motion(*eid, sub.passenger_world_delta(*offset, dt));
        }
    }

    /// Deterministic Save v2 blob: every airship's `AERO` v1 snapshot in
    /// `subworld_id` order.
    fn save_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(64 + self.airships.len() * 256);
        out.extend_from_slice(&ADB_SAVE_MAGIC);
        out.push(ADB_SAVE_VERSION);
        mod_wire::write_varint(&mut out, self.airships.len() as u32);
        for (id, sub) in &self.airships {
            out.extend_from_slice(&id.to_be_bytes());
            let snap = encode_aeronautics_snapshot(sub);
            out.extend_from_slice(&(snap.len() as u32).to_be_bytes());
            out.extend_from_slice(&snap);
        }
        out
    }

    /// Restore a `save_bytes` blob: replaces the whole fleet, decoding every
    /// `AERO` v1 snapshot against the current shared registry so reordered
    /// runtime ids remap cleanly — and commits all-or-nothing so a corrupt
    /// blob can never leave a half-restored fleet.
    fn load_bytes(&mut self, bytes: &[u8]) -> Result<(), String> {
        let err = |m: &str| format!("aeronautics save: {m}");
        if bytes.len() < 5 || bytes[..4] != ADB_SAVE_MAGIC {
            return Err(err("bad magic"));
        }
        if bytes[4] != ADB_SAVE_VERSION {
            return Err(err("unsupported version"));
        }
        let mut cursor = 5usize;
        let count = mod_wire::read_varint(bytes, &mut cursor)
            .ok_or_else(|| err("truncated ship count"))? as usize;
        if count > ADB_MAX_SHIPS {
            return Err(err("ship count out of range"));
        }
        self.ensure_registry()?;
        let registry = self.registry.as_ref().expect("ensure_registry just ran");
        let mut restored: BTreeMap<u64, RigidBodySubWorld> = BTreeMap::new();
        for _ in 0..count {
            let id = adb_r_u64(bytes, &mut cursor).ok_or_else(|| err("truncated ship id"))?;
            let len =
                adb_r_u32(bytes, &mut cursor).ok_or_else(|| err("truncated ship len"))? as usize;
            if len > ADB_MAX_SHIP_BYTES {
                return Err(err("ship snapshot out of range"));
            }
            let snap = bytes
                .get(cursor..cursor + len)
                .ok_or_else(|| err("truncated ship snapshot"))?;
            cursor += len;
            let sub = decode_aeronautics_snapshot(snap, registry)
                .map_err(|e| err(&format!("corrupt airship {id}: {e:?}")))?;
            if restored.insert(id, sub).is_some() {
                return Err(err("duplicate ship id"));
            }
        }
        if cursor != bytes.len() {
            return Err(err("trailing bytes"));
        }
        self.next_subworld_id = self
            .next_subworld_id
            .max(restored.keys().next_back().map(|id| id + 1).unwrap_or(1));
        self.airships = restored;
        self.passengers.clear();
        Ok(())
    }

    /// Zero-JVM discovery of an arbitrary Create addon JAR: the `(path, bytes)`
    /// entries are wrapped as a stored ZIP for `load_from_stored_zip`, every
    /// discovered addon namespace yields a `CreateAddonProfile`, and each
    /// profile's `#c:ores`/`#c:chests` hidden blocks flow into anti-xray via
    /// `register_hidden_block_names`. Returns how many state ids were newly
    /// hidden; discovered profiles are kept so later airship assemblies see
    /// the addon's blocks and propulsion model.
    fn discover_addon_jar<I, S, B>(&mut self, entries: I) -> Result<usize, String>
    where
        I: IntoIterator<Item = (S, B)>,
        S: AsRef<str>,
        B: AsRef<[u8]>,
    {
        let entries: Vec<(String, Vec<u8>)> = entries
            .into_iter()
            .map(|(p, b)| (p.as_ref().to_string(), b.as_ref().to_vec()))
            .collect();
        let zip = adb_stored_zip(&entries);
        let mut ids = mod_data::ItemFluidIdTable::new();
        let data = mod_data::load_from_stored_zip(&zip, &mut ids)
            .map_err(|e| format!("addon jar scan: {e:?}"))?;
        let mut hidden: BTreeSet<String> = BTreeSet::new();
        for ns in adb_addon_namespaces(&data) {
            let profile = discover_create_addon_profile(&ns, &data);
            if profile.block_defs().is_empty() && profile.hidden_blocks.is_empty() {
                continue;
            }
            for name in &profile.hidden_blocks {
                hidden.insert(ns_rl(name));
                self.mod_hidden_names.insert(ns_rl(name));
            }
            self.add_addon(profile);
        }
        Ok(register_hidden_block_names(hidden.iter().map(String::as_str)))
    }
}

// ---------------------------------------------------------------------------
// Dimension-adding mod runtime bridge
// ---------------------------------------------------------------------------

/// What one `load_dimension_mod_entries` pass changed.
#[derive(Clone, Debug, Default)]
#[allow(dead_code)]
struct DimModLoadReport {
    /// Dimension entry ids discovered (`data/<ns>/dimension/*.json`).
    discovered_dims: Vec<String>,
    /// Custom dimensions now live as Track 2 `DimId`s (0 when the mod needs a
    /// Java `ChunkGenerator`, i.e. is not pure data-driven).
    track2_dims_registered: usize,
    /// `DimensionModData::is_pure_data_driven` for the scanned mod.
    pure_data_driven: bool,
    /// Block state ids newly hidden in anti-xray.
    hidden_states_added: usize,
    /// `data/*` paths that matched a dimension-mod slot but failed to parse.
    skipped_malformed: Vec<String>,
}

/// The dimension-mod side of the bridge: a declarative `PortalRegistry`, the
/// `dimension_type`/`dimension` tables discovered from JAR scans (with builtin
/// reference profiles as defaults), one `DimensionEnvProfile` per live
/// `DimId`, and the anti-xray record of custom ore/chest ids.
#[allow(dead_code)]
struct DimensionModRuntimeBridge {
    /// Ordered declarative portal rules: every builtin profile's rules plus
    /// any rules registered for custom dims.
    portals: dim_mod::PortalRegistry,
    /// `dimension_type` id -> descriptor (builtin defaults; discovered JSON
    /// wins when it disagrees, per the mod-adapter contract).
    dim_types: BTreeMap<String, dim_mod::DimTypeDescriptor>,
    /// `dimension` entry id -> declared `type` id.
    dim_to_type: BTreeMap<String, String>,
    /// Live env profile per resolved `DimId`.
    env_by_dim: BTreeMap<DimId, dim_mod::DimensionEnvProfile>,
    /// Env profile per canonical dim id (builtin-known or discovered).
    env_by_name: BTreeMap<String, dim_mod::DimensionEnvProfile>,
    /// Canonical dim id -> live `DimId` for dims this bridge registered.
    registered_dims: BTreeMap<String, DimId>,
    /// Custom ore/chest ids fed to anti-xray (they may predate any registered
    /// state id).
    mod_hidden_names: BTreeSet<String>,
}

#[allow(dead_code)]
impl DimensionModRuntimeBridge {
    fn new() -> Self {
        let mut bridge = Self {
            portals: dim_mod::PortalRegistry::with_builtin_rules(),
            dim_types: BTreeMap::new(),
            dim_to_type: BTreeMap::new(),
            env_by_dim: BTreeMap::new(),
            env_by_name: BTreeMap::new(),
            registered_dims: BTreeMap::new(),
            mod_hidden_names: BTreeSet::new(),
        };
        // Builtin reference profiles seed the coordinate-scale / arrival-band
        // and env tables so well-known dims answer correctly even before any
        // JAR scan; discovered JSON overwrites these entries on load.
        for profile in dim_mod::builtin_dimension_mod_profiles() {
            for t in profile.dimension_types {
                bridge.dim_types.insert(t.id.clone(), t);
            }
            for d in profile.dimensions {
                bridge.dim_to_type.insert(d.id.clone(), d.type_id);
            }
            for e in profile.env_profiles {
                bridge.env_by_name.insert(e.dimension_id.clone(), e);
            }
        }
        bridge
    }

    /// Register an extra portal rule (a mod manifest, a datapack, an operator
    /// config). The transition detector names rules by `rule.name`.
    fn register_portal_rule(&mut self, rule: dim_mod::PortalRule) {
        self.portals.register(rule);
    }

    /// The `dimension_type` descriptor for a canonical dim key: the type its
    /// `dimension/` entry names, or a descriptor stored under the dim's own id.
    fn type_desc_for_key(&self, dim_key: &str) -> Option<dim_mod::DimTypeDescriptor> {
        self.dim_to_type
            .get(dim_key)
            .and_then(|tid| self.dim_types.get(tid))
            .or_else(|| self.dim_types.get(dim_key))
            .cloned()
    }

    /// The `dimension_type` descriptor backing a live `DimId`: discovered JSON
    /// first, then the registered `DimensionSpec` (for dims registered outside
    /// this bridge), then vanilla's three pinned answers.
    fn type_desc_for_dim(&self, dim: DimId) -> Option<dim_mod::DimTypeDescriptor> {
        let key = dim.dimension_id();
        if let Some(desc) = self.type_desc_for_key(key) {
            return Some(desc);
        }
        if let Some(spec) = worldgen::dimension_spec(dim) {
            let mut desc = dim_mod::DimTypeDescriptor::defaults(key, spec.min_y, spec.height);
            desc.coordinate_scale = spec.coordinate_scale;
            desc.has_skylight = spec.has_skylight;
            desc.has_ceiling = spec.has_ceiling;
            desc.ultrawarm = spec.ultrawarm;
            desc.natural = spec.natural;
            desc.bed_works = spec.bed_works;
            desc.respawn_anchor_works = spec.respawn_anchor_works;
            desc.ambient_light = f64::from(spec.ambient_light);
            desc.effects = spec.effects.clone();
            return Some(desc);
        }
        let desc = match dim {
            DimId::Overworld => {
                dim_mod::DimTypeDescriptor::defaults("minecraft:overworld", -64, 384)
            }
            DimId::Nether => {
                let mut t =
                    dim_mod::DimTypeDescriptor::defaults("minecraft:the_nether", 0, 256);
                t.coordinate_scale = 8.0;
                t.has_skylight = false;
                t.has_ceiling = true;
                t.ultrawarm = true;
                t.bed_works = false;
                t.respawn_anchor_works = true;
                t.effects = "minecraft:the_nether".into();
                t
            }
            DimId::End => {
                let mut t = dim_mod::DimTypeDescriptor::defaults("minecraft:the_end", 0, 256);
                t.has_skylight = false;
                t.bed_works = false;
                t.effects = "minecraft:the_end".into();
                t
            }
            _ => return None,
        };
        Some(desc)
    }

    /// `dimension_type.coordinate_scale` for a live `DimId` (8.0 for the
    /// Nether, 1.0 when nothing declares otherwise).
    fn coordinate_scale(&self, dim: DimId) -> f64 {
        self.type_desc_for_dim(dim)
            .map(|t| t.coordinate_scale)
            .unwrap_or(1.0)
    }

    /// Effective `generic.gravity` multiplier for `dim` (Moon 0.165, Mars
    /// 0.38, orbit 0.05; 1.0 when the dimension declares no override).
    fn dim_gravity_multiplier(&self, dim: DimId) -> f64 {
        self.env_by_dim
            .get(&dim)
            .or_else(|| self.env_by_name.get(dim.dimension_id()))
            .map(|env| env.gravity_multiplier)
            .unwrap_or(1.0)
    }

    /// Whether the 2-lane Underground Occlusion anti-xray lane applies in
    /// `dim` (`has_skylight && !has_ceiling`; ceilinged dimensions hide
    /// uniformly anyway, so the lane adds nothing there).
    fn dim_supports_underground_occlusion(&self, dim: DimId) -> bool {
        if let Some(env) = self
            .env_by_dim
            .get(&dim)
            .or_else(|| self.env_by_name.get(dim.dimension_id()))
        {
            return env.supports_underground_occlusion;
        }
        let (skylight, ceiling) = match worldgen::dimension_spec(dim) {
            Some(spec) => (spec.has_skylight, spec.has_ceiling),
            None => match dim {
                DimId::Overworld => (true, false),
                DimId::Nether => (false, true),
                DimId::End => (false, false),
                _ => return false,
            },
        };
        skylight && !ceiling
    }

    /// Scan `(path, bytes)` entries of a dimension-adding mod JAR/datapack:
    /// forwards pure data-driven dimensions into Track 2's
    /// `register_datapack_dimensions` as live `DimId`s, folds ore/chest tag
    /// members into anti-xray, installs `DimensionEnvProfile`s, and keeps the
    /// `dimension_type`/`dimension` tables for portal math. Mods carrying a
    /// custom Java `ChunkGenerator` still get env/anti-xray coverage while the
    /// dimension itself routes to the Tier 2/3 fallback.
    fn load_dimension_mod_entries<I, S, B>(
        &mut self,
        entries: I,
    ) -> Result<DimModLoadReport, String>
    where
        I: IntoIterator<Item = (S, B)>,
        S: AsRef<str>,
        B: AsRef<[u8]>,
    {
        let entries: Vec<(String, Vec<u8>)> = entries
            .into_iter()
            .map(|(p, b)| (p.as_ref().to_string(), b.as_ref().to_vec()))
            .collect();
        let data = dim_mod::discover_dimension_mod_entries(
            entries.iter().map(|(p, b)| (p.as_str(), b.as_slice())),
        )
        .map_err(|e| format!("dimension mod scan: {e:?}"))?;

        let mut report = DimModLoadReport {
            discovered_dims: data.dimensions.iter().map(|d| d.id.clone()).collect(),
            pure_data_driven: data.is_pure_data_driven(),
            skipped_malformed: data.skipped_malformed.clone(),
            ..DimModLoadReport::default()
        };

        // Track 2 live dims: the same raw `dimension_type`/`dimension` JSON
        // the datapack scan reads, forwarded untouched — the mod's dimensions
        // become real `DimId`s immediately.
        if data.is_dimension_mod() && data.is_pure_data_driven() {
            let files: Vec<(String, String)> = entries
                .iter()
                .filter(|(p, _)| adb_is_dimension_json(p))
                .filter_map(|(p, b)| {
                    std::str::from_utf8(b).ok().map(|s| (p.clone(), s.to_string()))
                })
                .collect();
            if !files.is_empty() {
                let sources = [worldgen::DatapackDimensionSource::Entries(files)];
                report.track2_dims_registered =
                    worldgen::register_datapack_dimensions(&sources, name_to_state)?;
            }
        }

        // Anti-xray: `#c:ores`/`#forge:ores`/`#c:chests` tag members, exactly
        // like the datapack tag walker folds them in.
        let hidden = data.anti_xray_hidden_blocks();
        for name in &hidden {
            self.mod_hidden_names.insert(ns_rl(name));
        }
        report.hidden_states_added =
            register_hidden_block_names(hidden.iter().map(String::as_str));

        // Discovered `dimension_type`/`dimension` tables win over builtins.
        for t in &data.dimension_types {
            self.dim_types.insert(t.id.clone(), t.clone());
        }
        for d in &data.dimensions {
            self.dim_to_type.insert(d.id.clone(), d.type_id.clone());
        }

        // Environment profiles: a builtin profile's physics overrides apply
        // when its mod's dims were discovered; every other dim derives a safe
        // default from its own `dimension_type`.
        let discovered: HashSet<&str> =
            data.dimensions.iter().map(|d| d.id.as_str()).collect();
        for profile in dim_mod::builtin_dimension_mod_profiles() {
            if profile
                .dimensions
                .iter()
                .any(|d| discovered.contains(d.id.as_str()))
            {
                for env in profile.env_profiles {
                    self.env_by_name.insert(env.dimension_id.clone(), env);
                }
            }
        }
        for d in &data.dimensions {
            let env = self.env_by_name.get(&d.id).cloned().or_else(|| {
                self.type_desc_for_key(&d.id)
                    .map(|t| dim_mod::DimensionEnvProfile::derive(&t, 1.0, false))
            });
            if let Some(dim) = DimId::parse_dimension_id(&d.id) {
                self.registered_dims.insert(d.id.clone(), dim);
                if let Some(env) = &env {
                    self.env_by_dim.insert(dim, env.clone());
                }
            }
            if let Some(env) = env {
                self.env_by_name.insert(d.id.clone(), env);
            }
        }
        Ok(report)
    }

    /// Convert a validated `PortalTransition` into the `TeleportIntent` the
    /// server should execute, then grant `eid` `ac_expect_teleport` +
    /// `ac_grant_grace` for the rule's grace window so the portal hop cannot
    /// false-flag movement checks. `None` when the transition names an unknown
    /// rule, targets a different dimension than `target_dim`, or the target
    /// has no known `dimension_type`.
    fn try_mod_portal_teleport(
        &self,
        eid: i32,
        source_dim: DimId,
        target_dim: DimId,
        source_pos: [f64; 3],
        trigger_result: &dim_mod::PortalTransition,
    ) -> Option<dim_mod::TeleportIntent> {
        let rule = self.portals.find(&trigger_result.rule_name)?;
        if ns_rl(&trigger_result.target_dim) != target_dim.dimension_id() {
            return None;
        }
        let target_type = self.type_desc_for_dim(target_dim)?;
        let intent = dim_mod::build_teleport_intent(
            trigger_result,
            source_pos,
            self.coordinate_scale(source_dim),
            &target_type,
            rule.teleport_grace_ticks,
        );
        ac_expect_teleport(eid);
        ac_grant_grace(
            eid,
            Duration::from_millis(50 * u64::from(intent.expect_teleport_grace_ticks)),
        );
        Some(intent)
    }
}

// ---------- shared helpers ----------

/// `data/<ns>/dimension_type/*.json` or `data/<ns>/dimension/*.json` — the raw
/// entries Track 2's `register_datapack_dimensions` consumes.
fn adb_is_dimension_json(path: &str) -> bool {
    path.ends_with(".json")
        && path.starts_with("data/")
        && (path.contains("/dimension_type/") || path.contains("/dimension/"))
}

/// Addon namespaces a JAR scan should try `discover_create_addon_profile` on:
/// every namespace owning blockstates, tag files, tag members, or recipe
/// types, minus the conventional shared namespaces (`c`, `forge`, ...) that
/// hold tag registries rather than block inventories, and `create` itself
/// (already in the core registry — re-deriving it would collide).
fn adb_addon_namespaces(data: &mod_data::LoadedModData) -> BTreeSet<String> {
    const SHARED: [&str; 6] = ["minecraft", "c", "forge", "neoforge", "fabric", "create"];
    let mut out = BTreeSet::new();
    let mut push_ns = |id: &str| {
        if let Some(ns) = id.split(':').next() {
            if !SHARED.contains(&ns) {
                out.insert(ns.to_string());
            }
        }
    };
    for id in data.blockstates.keys() {
        push_ns(id);
    }
    for (tag, members) in &data.tags {
        push_ns(tag);
        for member in members {
            push_ns(member.strip_prefix('#').unwrap_or(member));
        }
    }
    for recipe in &data.recipes {
        push_ns(&recipe.recipe_type);
    }
    out
}

/// Wrap `(path, bytes)` JAR entries as a stored (uncompressed) ZIP the
/// `load_from_stored_zip` scanner accepts verbatim — local file headers only;
/// the scanner stops at the first non-entry signature so no central directory
/// is needed.
fn adb_stored_zip(entries: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, data) in entries {
        if !name.ends_with(".json")
            || !(name.starts_with("data/") || name.starts_with("assets/"))
            || name.len() > u16::MAX as usize
            || data.len() > u32::MAX as usize
        {
            continue;
        }
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes()); // local file header
        out.extend_from_slice(&20u16.to_le_bytes()); // version needed
        out.extend_from_slice(&0u16.to_le_bytes()); // flags
        out.extend_from_slice(&0u16.to_le_bytes()); // method 0 = stored
        out.extend_from_slice(&0u16.to_le_bytes()); // mod time
        out.extend_from_slice(&0u16.to_le_bytes()); // mod date
        out.extend_from_slice(&0u32.to_le_bytes()); // crc32 (scanner ignores)
        out.extend_from_slice(&(data.len() as u32).to_le_bytes()); // comp size
        out.extend_from_slice(&(data.len() as u32).to_le_bytes()); // uncomp size
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // extra len
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);
    }
    out
}

fn adb_r_u64(bytes: &[u8], cursor: &mut usize) -> Option<u64> {
    let s = bytes.get(*cursor..*cursor + 8)?;
    *cursor += 8;
    Some(u64::from_be_bytes(s.try_into().ok()?))
}

fn adb_r_u32(bytes: &[u8], cursor: &mut usize) -> Option<u32> {
    let s = bytes.get(*cursor..*cursor + 4)?;
    *cursor += 4;
    Some(u32::from_be_bytes(s.try_into().ok()?))
}

// ---------- global bridges + entry points ----------

/// The process-wide aeronautics bridge.
fn aeronautics_bridge() -> &'static Mutex<AeronauticsRuntimeBridge> {
    static BRIDGE: OnceLock<Mutex<AeronauticsRuntimeBridge>> = OnceLock::new();
    BRIDGE.get_or_init(|| Mutex::new(AeronauticsRuntimeBridge::new()))
}

/// The process-wide dimension-mod bridge.
fn dimension_mod_bridge() -> &'static Mutex<DimensionModRuntimeBridge> {
    static BRIDGE: OnceLock<Mutex<DimensionModRuntimeBridge>> = OnceLock::new();
    BRIDGE.get_or_init(|| Mutex::new(DimensionModRuntimeBridge::new()))
}

/// One step for every active airship (`dt` seconds, 0.05 at 20 TPS): each
/// `RigidBodySubWorld` ticks its live embedded `CreateFactory` propeller/
/// kinetic network, then every attached passenger's exact world delta is
/// granted to anti-cheat via `ac_note_carried_motion`. No-op on an empty fleet.
#[allow(dead_code)]
fn aeronautics_bridge_tick(dt: f64, gravity: f64, air_density: f64) {
    let Ok(mut bridge) = aeronautics_bridge().lock() else {
        return;
    };
    if !bridge.has_work() {
        return;
    }
    bridge.step(dt, gravity, air_density);
}

/// Save v2 bytes for the whole airship fleet (`AERO` v1 snapshots inside the
/// deterministic `MSAE` wrapper).
#[allow(dead_code)]
fn aeronautics_bridge_save_bytes() -> Vec<u8> {
    aeronautics_bridge()
        .lock()
        .map(|b| b.save_bytes())
        .unwrap_or_default()
}

/// Restore `aeronautics_bridge_save_bytes`; `false` on lock failure or a
/// corrupt buffer (the existing fleet is left untouched either way).
#[allow(dead_code)]
fn aeronautics_bridge_load_bytes(bytes: &[u8]) -> bool {
    aeronautics_bridge()
        .lock()
        .map(|mut b| b.load_bytes(bytes).is_ok())
        .unwrap_or(false)
}

/// Discover an arbitrary Create addon JAR's profiles and forward its `#c:ores`
/// /`#c:chests` hidden blocks into anti-xray. Returns the number of state ids
/// newly hidden.
#[allow(dead_code)]
fn aeronautics_bridge_discover_addon_jar<I, S, B>(entries: I) -> Result<usize, String>
where
    I: IntoIterator<Item = (S, B)>,
    S: AsRef<str>,
    B: AsRef<[u8]>,
{
    aeronautics_bridge()
        .lock()
        .map_err(|_| "aeronautics bridge lock poisoned".to_string())
        .and_then(|mut b| b.discover_addon_jar(entries))
}

/// Scan a dimension-adding mod's `(path, bytes)` entries: live Track 2
/// `DimId`s for pure data-driven dims, anti-xray hidden blocks, and per-dim
/// `DimensionEnvProfile`s.
#[allow(dead_code)]
fn dimension_mod_load_entries<I, S, B>(entries: I) -> Result<DimModLoadReport, String>
where
    I: IntoIterator<Item = (S, B)>,
    S: AsRef<str>,
    B: AsRef<[u8]>,
{
    dimension_mod_bridge()
        .lock()
        .map_err(|_| "dimension mod bridge lock poisoned".to_string())
        .and_then(|mut b| b.load_dimension_mod_entries(entries))
}

/// Portal-hop hook: produce the `TeleportIntent` for a validated transition
/// and grant `eid` clean-arrival anti-cheat treatment (`ac_expect_teleport` +
/// grace for the rule's window).
#[allow(dead_code)]
fn dimension_mod_try_portal_teleport(
    eid: i32,
    source_dim: DimId,
    target_dim: DimId,
    source_pos: [f64; 3],
    trigger_result: &dim_mod::PortalTransition,
) -> Option<dim_mod::TeleportIntent> {
    dimension_mod_bridge().lock().ok()?.try_mod_portal_teleport(
        eid,
        source_dim,
        target_dim,
        source_pos,
        trigger_result,
    )
}

/// `generic.gravity` multiplier for `dim` (1.0 when nothing overrides it).
#[allow(dead_code)]
fn dim_gravity_multiplier(dim: DimId) -> f64 {
    dimension_mod_bridge()
        .lock()
        .map(|b| b.dim_gravity_multiplier(dim))
        .unwrap_or(1.0)
}

/// Whether the 2-lane Underground Occlusion anti-xray lane applies in `dim`.
#[allow(dead_code)]
fn dim_supports_underground_occlusion(dim: DimId) -> bool {
    dimension_mod_bridge()
        .lock()
        .map(|b| b.dim_supports_underground_occlusion(dim))
        .unwrap_or(false)
}

#[cfg(all(test, feature = "create-offload"))]
mod aeronautics_and_dim_bridge_tests {
    use super::*;
    use create_adapter::aeronautics::{DEFAULT_BLOCK_MASS, SEA_LEVEL_AIR_DENSITY};
    use create_adapter::factory::DimBlockPos;

    /// Several tests here reprogram the process-global bridges; serialize them.
    /// A panicking sibling test must not cascade poison into the next one, so
    /// both guards recover the inner value.
    static ADB_LOCK: Mutex<()> = Mutex::new(());

    fn adb_lock() -> std::sync::MutexGuard<'static, AeronauticsRuntimeBridge> {
        aeronautics_bridge().lock().unwrap_or_else(|e| e.into_inner())
    }

    fn dim_lock() -> std::sync::MutexGuard<'static, DimensionModRuntimeBridge> {
        dimension_mod_bridge().lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Synthetic `Client` (same fixture style as `mod_offload_bridge`'s tests)
    /// — a loopback socket, survival game mode, and an unregistered dimension
    /// slot so `block_at_dim`/`ac_server_supported` read cheap all-air terrain.
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

    fn canonical(reg: &CreateBlockRegistry, id: &str) -> String {
        reg.encode_canonical(reg.default_state(id).unwrap())
            .unwrap()
            .to_string()
    }

    fn canonical_with(reg: &CreateBlockRegistry, id: &str, prop: &str, val: &str) -> String {
        let st = reg
            .with_property(reg.default_state(id).unwrap(), prop, val)
            .unwrap();
        reg.encode_canonical(st).unwrap().to_string()
    }

    /// The proven flying-ship fixture: `propeller_engine[axis=x]` drives
    /// `create:shaft[axis=x]` into `propeller_bearing[facing=east]` (a live
    /// 64-RPM kinetic chain pushing +X), under a single balloon envelope.
    fn powered_airship_cells(reg: &CreateBlockRegistry) -> BTreeMap<[i32; 3], String> {
        let mut cells = BTreeMap::new();
        cells.insert(
            [0, 0, 0],
            canonical_with(reg, "createaeronautics:propeller_engine", "axis", "x"),
        );
        cells.insert([1, 0, 0], canonical_with(reg, "create:shaft", "axis", "x"));
        cells.insert(
            [2, 0, 0],
            canonical_with(reg, "createaeronautics:propeller_bearing", "facing", "east"),
        );
        cells.insert(
            [0, 1, 0],
            canonical(reg, "createaeronautics:balloon_envelope"),
        );
        cells
    }

    #[test]
    fn flying_airship_pilot_carry_passes_anticheat_move_and_fall_checks() {
        let _serial = ADB_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        *adb_lock() = AeronauticsRuntimeBridge::new();
        let dim = DimId::from_raw(9);
        let pilot_eid = 988_001;
        let hack_eid = 988_002;
        let pilot = mk_client("adb_pilot", pilot_eid, dim, (0.5, 80.0, 0.5));
        let hack = mk_client("adb_speed_hacker", hack_eid, dim, (0.5, 80.0, 200.5));
        for c in [&pilot, &hack] {
            ac_test_set_player_preset(&c.name, AcPreset::Standard);
        }

        // Assemble a real 6-DOF airship with a live kinetic propeller network
        // and attach the pilot at a local offset above the deck.
        {
            let mut b = adb_lock();
            let cells = {
                b.ensure_registry().unwrap();
                powered_airship_cells(b.registry.as_ref().unwrap())
            };
            b.assemble_airship(1, dim.raw(), cells, Some("createaeronautics"), DEFAULT_BLOCK_MASS)
                .unwrap();
            let ship = b.airships.get_mut(&1).unwrap();
            assert_eq!(ship.propellers.len(), 2, "engine + bearing both thrust");
            ship.pos = [0.0, 100.0, 0.0];
            assert!(b.attach_passenger(pilot_eid, 1, [0.0, 1.5, 0.0]));
        }

        // Baseline small legal moves: installs the per-eid anti-cheat entries
        // the carry grants then land in.
        for c in [&pilot, &hack] {
            let v = ac_check_move(c, (0.5, 80.0, 0.5), (0.6, 80.0, 0.5), false);
            assert!(matches!(v, AcMoveVerdict::Accept { .. }));
        }

        // 40 sustained flown ticks: the live propeller network spins up to
        // 64 RPM, the ship accelerates far beyond vanilla's ~0.56 blocks/tick
        // sprint-jump allowance and sinks slightly (one balloon < hull mass) —
        // every carried move, up AND down, must accept with the on-ground
        // claim intact and zero Movement/NoFall flags.
        let mut pos = (0.5, 80.0, 0.5);
        let mut saw_overspeed = false;
        let mut saw_descent = false;
        for _ in 0..40 {
            aeronautics_bridge_tick(0.05, 9.81, SEA_LEVEL_AIR_DENSITY);
            let delta = adb_lock()
                .passenger_delta(pilot_eid, 0.05)
                .expect("pilot is attached");
            if delta[0].hypot(delta[2]) > 0.6 || delta[1] > 0.6 {
                saw_overspeed = true;
            }
            if delta[1] < -0.05 {
                saw_descent = true;
            }
            let to = (pos.0 + delta[0], pos.1 + delta[1], pos.2 + delta[2]);
            let v = ac_check_move(&pilot, pos, to, true);
            assert!(
                matches!(v, AcMoveVerdict::Accept { ground: true }),
                "carried airship move must accept with ground intact, got {v:?} delta {delta:?}"
            );
            pos = to;
        }
        assert!(saw_overspeed, "ship must exceed vanilla move allowance to prove the carry path");
        assert!(saw_descent, "one balloon must not lift the hull — descent exercises the NoFall leg");
        assert_eq!(ac_flag_count(pilot_eid, AcCheckId::Movement), 0);
        assert_eq!(ac_flag_count(pilot_eid, AcCheckId::NoFall), 0);
        // The embedded factory is still live: the propeller node solves at the
        // engine's 64 RPM after 40 ticks.
        {
            let b = adb_lock();
            let ship = b.airships.get(&1).unwrap();
            let sol = ship
                .factory()
                .kinetic_solution(DimBlockPos::new(dim.raw() as i32, 2, 0, 0))
                .unwrap();
            assert!((sol.speed - 64.0).abs() < 1e-9, "propeller RPM: {sol:?}");
        }

        // The same sustained speed without a carry grant still trips the check.
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
    fn dimension_mod_jar_auto_registers_live_track2_dim_and_portal_teleport_passes_anticheat()
    {
        let _serial = ADB_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        *dim_lock() = DimensionModRuntimeBridge::new();
        ac_test_set_anti_xray(Some(true));

        // A synthetic data-driven dimension mod: one fully custom dimension
        // (coordinate_scale 4.0 to exercise portal scaling), plus the builtin-
        // profile dims for Ad Astra's Moon and the Undergarden, and a shared
        // `#c:ores` tag declaring a mod ore plus a vanilla block the hidden
        // set does not already cover.
        let entries: Vec<(String, Vec<u8>)> = vec![
            (
                "data/testdim/dimension_type/crystal_caves.json".to_string(),
                br#"{"min_y":-64,"height":384,"coordinate_scale":4.0,"has_skylight":true,"has_ceiling":false,"natural":true,"effects":"minecraft:overworld"}"#.to_vec(),
            ),
            (
                "data/testdim/dimension/crystal_caves.json".to_string(),
                br#"{"type":"testdim:crystal_caves","generator":{"type":"minecraft:noise","settings":"minecraft:overworld"}}"#.to_vec(),
            ),
            (
                "data/ad_astra/dimension_type/moon.json".to_string(),
                br#"{"min_y":-64,"height":384,"has_skylight":true,"has_ceiling":false,"bed_works":false,"respawn_anchor_works":false,"effects":"minecraft:the_end"}"#.to_vec(),
            ),
            (
                "data/ad_astra/dimension/moon.json".to_string(),
                br#"{"type":"ad_astra:moon","generator":{"type":"minecraft:noise","settings":"minecraft:overworld"}}"#.to_vec(),
            ),
            (
                "data/undergarden/dimension_type/undergarden.json".to_string(),
                br#"{"min_y":0,"height":256,"has_skylight":false,"has_ceiling":true,"bed_works":false,"respawn_anchor_works":true,"ambient_light":0.1,"effects":"minecraft:the_nether"}"#.to_vec(),
            ),
            (
                "data/undergarden/dimension/undergarden.json".to_string(),
                br#"{"type":"undergarden:undergarden","generator":{"type":"minecraft:noise","settings":"minecraft:overworld"}}"#.to_vec(),
            ),
            (
                "data/c/tags/block/ores.json".to_string(),
                br#"{"values":["testdim:crystal_ore","minecraft:calcite"]}"#.to_vec(),
            ),
        ];

        let report = dimension_mod_load_entries(entries).unwrap();
        assert!(report.pure_data_driven);
        assert_eq!(report.track2_dims_registered, 3);
        assert!(report.skipped_malformed.is_empty());
        assert!(report
            .discovered_dims
            .iter()
            .any(|d| d == "testdim:crystal_caves"));

        // The mod's dimensions are live Track 2 `DimId`s now.
        let crystal = DimId::parse_dimension_id("testdim:crystal_caves")
            .expect("testdim:crystal_caves registered");
        let moon = DimId::parse_dimension_id("ad_astra:moon").expect("ad_astra:moon registered");
        let undergarden =
            DimId::parse_dimension_id("undergarden:undergarden").expect("undergarden registered");
        for dim in [crystal, moon, undergarden] {
            assert!(dim.raw() >= 3, "custom dim must own a dynamic slot: {dim:?}");
        }
        assert_eq!(crystal.dimension_id(), "testdim:crystal_caves");

        // Environment profiles: the builtin Ad Astra physics override lands on
        // the Moon; discovered types derive sane defaults elsewhere.
        assert!((dim_gravity_multiplier(moon) - 0.165).abs() < 1e-9);
        assert!((dim_gravity_multiplier(crystal) - 1.0).abs() < 1e-9);
        assert!(dim_supports_underground_occlusion(crystal));
        assert!(!dim_supports_underground_occlusion(undergarden));

        // Anti-xray: the mod ore is recorded, and the vanilla tag member that
        // was not already hidden is now in the hidden set.
        {
            let b = dim_lock();
            assert!(b.mod_hidden_names.contains("testdim:crystal_ore"));
            assert!(b.mod_hidden_names.contains("minecraft:calcite"));
        }
        let calcite = name_to_state("calcite").expect("vanilla block");
        assert!(is_hidden(calcite), "#c:ores member must be hidden from xray");

        // A custom VerticalFrame portal (the Aether pattern: glowstone frame
        // + water_bucket igniter) registered on the custom dim.
        {
            let mut b = dim_lock();
            b.register_portal_rule(dim_mod::PortalRule::new(
                "testdim:crystal_portal",
                dim_mod::PortalTrigger::VerticalFrame {
                    frame_block: "minecraft:glowstone".to_string(),
                    min_width: 2,
                    min_height: 3,
                    max_width: 8,
                    max_height: 8,
                    igniter_item: "minecraft:water_bucket".to_string(),
                    portal_block: "testdim:crystal_portal".to_string(),
                },
                "testdim:crystal_caves",
            ));
        }
        let rule = {
            let b = dim_lock();
            b.portals.find("testdim:crystal_portal").unwrap().clone()
        };
        // A 2x3 open interior in a glowstone ring, detected in a flat test world.
        let mut world: BTreeMap<(i32, i32, i32), String> = BTreeMap::new();
        for y in 69..=73 {
            for x in 9..=12 {
                world.insert((x, y, 20), "minecraft:glowstone".to_string());
            }
        }
        for y in 70..=72 {
            for x in 10..=11 {
                world.insert((x, y, 20), "minecraft:air".to_string());
            }
        }
        let block_at = |x: i32, y: i32, z: i32| {
            Some(
                world
                    .get(&(x, y, z))
                    .cloned()
                    .unwrap_or_else(|| "minecraft:air".to_string()),
            )
        };
        let transition =
            dim_mod::detect_vertical_portal(&rule, [10, 70, 20], &block_at)
                .expect("glowstone frame must validate");
        assert_eq!(transition.target_dim, "testdim:crystal_caves");
        assert_eq!(transition.cell_count, 6);

        // The traveller steps through far from the origin: scale 1.0 -> 4.0
        // maps the 3000-block journey through `ac_expect_teleport` + grace.
        let traveller_eid = 988_010;
        let control_eid = 988_011;
        let traveller = mk_client("adb_traveller", traveller_eid, DimId::Overworld, (0.5, 80.0, 0.5));
        let control = mk_client("adb_control", control_eid, DimId::Overworld, (0.5, 80.0, 0.5));
        for c in [&traveller, &control] {
            ac_test_set_player_preset(&c.name, AcPreset::Standard);
            let v = ac_check_move(c, (0.5, 80.0, 0.5), (0.6, 80.0, 0.5), false);
            assert!(matches!(v, AcMoveVerdict::Accept { .. }));
        }
        let source_pos = [4000.5, 70.0, -2000.5];
        let intent = {
            let b = dim_lock();
            b.try_mod_portal_teleport(
                traveller_eid,
                DimId::Overworld,
                crystal,
                source_pos,
                &transition,
            )
            .expect("validated transition must produce a teleport intent")
        };
        assert_eq!(intent.target_dim, "testdim:crystal_caves");
        assert_eq!(intent.expect_teleport_grace_ticks, 20);
        // x/z scale by 1/4; y clamps inside the target's [min_y+4, top-8] band.
        assert!((intent.target_pos[0] - 1000.125).abs() < 1e-9);
        assert!((intent.target_pos[1] - 70.0).abs() < 1e-9);
        assert!((intent.target_pos[2] - (-500.125)).abs() < 1e-9);

        let from = (source_pos[0], source_pos[1], source_pos[2]);
        let to = (intent.target_pos[0], intent.target_pos[1], intent.target_pos[2]);
        let v = ac_check_move(&traveller, from, to, false);
        assert!(
            matches!(v, AcMoveVerdict::Accept { .. }),
            "granted portal hop must accept, got {v:?}"
        );
        assert_eq!(ac_flag_count(traveller_eid, AcCheckId::Movement), 0);
        assert_eq!(ac_flag_count(traveller_eid, AcCheckId::NoFall), 0);

        // The identical hop without the portal grant is a plain blink: flagged.
        let v = ac_check_move(&control, from, to, false);
        assert!(matches!(v, AcMoveVerdict::Setback), "ungranted portal hop must flag");
        assert!(ac_flag_count(control_eid, AcCheckId::Movement) > 0);

        ac_test_set_anti_xray(None);
    }

    #[test]
    fn aeronautics_multi_ship_save_load_roundtrip_and_unknown_addon_discovery() {
        let _serial = ADB_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        *adb_lock() = AeronauticsRuntimeBridge::new();

        // Two airships: a powered flyer and an unpowered deck raft.
        {
            let mut b = adb_lock();
            b.ensure_registry().unwrap();
            let reg = b.registry.as_ref().unwrap().clone();
            let cells = powered_airship_cells(&reg);
            b.assemble_airship(7, 0, cells, Some("createaeronautics"), DEFAULT_BLOCK_MASS)
                .unwrap();
            b.airships.get_mut(&7).unwrap().pos = [3.0, 90.0, -4.0];

            let mut raft = BTreeMap::new();
            raft.insert([0, 0, 0], canonical(&reg, "createaeronautics:airship_assembler"));
            raft.insert([1, 0, 0], canonical(&reg, "createaeronautics:airship_assembler"));
            raft.insert([0, 1, 0], canonical(&reg, "createaeronautics:balloon_envelope"));
            raft.insert([1, 1, 0], canonical(&reg, "createaeronautics:balloon_envelope"));
            b.assemble_airship(9, 0, raft, Some("createaeronautics"), DEFAULT_BLOCK_MASS)
                .unwrap();
            b.airships.get_mut(&9).unwrap().pos = [-10.0, 120.0, 6.0];
        }
        for _ in 0..10 {
            aeronautics_bridge_tick(0.05, 9.81, SEA_LEVEL_AIR_DENSITY);
        }

        // Deterministic multi-ship blob: save twice, byte-identical.
        let blob1 = aeronautics_bridge_save_bytes();
        let blob2 = aeronautics_bridge_save_bytes();
        assert_eq!(blob1, blob2);
        assert_eq!(&blob1[..5], b"MSAE\x01");

        // Restore into a fresh bridge: byte-for-byte identical re-save.
        *adb_lock() = AeronauticsRuntimeBridge::new();
        assert!(aeronautics_bridge_load_bytes(&blob1));
        let reblob = aeronautics_bridge_save_bytes();
        assert_eq!(reblob, blob1, "save -> load -> save must round-trip exactly");
        // The restored fleet keeps flying (the powered ship's live network
        // survived the codec).
        let before = adb_lock().airships[&7].pos;
        for _ in 0..5 {
            aeronautics_bridge_tick(0.05, 9.81, SEA_LEVEL_AIR_DENSITY);
        }
        let after = adb_lock().airships[&7].pos;
        assert!(after[0] > before[0], "restored ship must keep +X thrust: {before:?} -> {after:?}");

        // Corrupt blobs fail closed and leave the fleet untouched.
        let moved_blob = aeronautics_bridge_save_bytes();
        assert!(!aeronautics_bridge_load_bytes(&blob1[..blob1.len() - 3]));
        let mut padded = moved_blob.clone();
        padded.push(0);
        assert!(!aeronautics_bridge_load_bytes(&padded));
        assert!(aeronautics_bridge_save_bytes() == moved_blob);

        // Unknown Create addon JAR: blockstates confirm the blocks, the `c:ores`
        // tag declares an ore, a chests tag a chest, and the name heuristic
        // classifies a propeller -> a propulsion profile is discovered.
        let jar: Vec<(String, Vec<u8>)> = vec![
            (
                "assets/mysteryaddon/blockstates/flux_ore.json".to_string(),
                br#"{"variants":{"":{}}}"#.to_vec(),
            ),
            (
                "assets/mysteryaddon/blockstates/flux_chest.json".to_string(),
                br#"{"variants":{"":{}}}"#.to_vec(),
            ),
            (
                "assets/mysteryaddon/blockstates/flux_propeller.json".to_string(),
                br#"{"variants":{"facing=east":{},"facing=west":{}}}"#.to_vec(),
            ),
            (
                "data/mysteryaddon/tags/block/ores.json".to_string(),
                br#"{"values":["mysteryaddon:flux_ore"]}"#.to_vec(),
            ),
            (
                "data/mysteryaddon/tags/block/chests.json".to_string(),
                br#"{"values":["mysteryaddon:flux_chest"]}"#.to_vec(),
            ),
        ];
        let hidden = aeronautics_bridge_discover_addon_jar(jar).unwrap();
        // No registry knows `mysteryaddon:*` state ids yet, so nothing new is
        // hidden — but the names are recorded for when the addon registers.
        assert_eq!(hidden, 0);
        let mut b = adb_lock();
        assert!(b.mod_hidden_names.contains("mysteryaddon:flux_ore"));
        assert!(b.mod_hidden_names.contains("mysteryaddon:flux_chest"));
        let profile = b
            .addons
            .iter()
            .find(|a| a.mod_id == "mysteryaddon")
            .expect("discovered profile must be kept");
        assert!(
            profile.propulsion.is_some(),
            "flux_propeller must classify as propulsion"
        );
        assert!(profile.hidden_blocks.contains(&"mysteryaddon:flux_ore".to_string()));
        // The rebuilt shared registry now knows the addon's blocks.
        b.ensure_registry().unwrap();
        let reg = b.registry.as_ref().expect("registry rebuilt after discovery");
        assert!(reg.default_state("mysteryaddon:flux_propeller").is_some());
        assert!(reg.default_state("mysteryaddon:flux_ore").is_some());
    }
}
