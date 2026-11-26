// Production runtime bridge: Create: Aeronautics 6-DOF sub-worlds, universal
// Create addon discovery, and dimension-adding mods wired into megu's live
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
    builtin_create_addon_profiles, create_registry_with_addons, discover_create_addon_profile,
    CreateAddonProfile, PropulsionSpec,
};
use create_adapter::aeronautics::{
    decode_aeronautics_snapshot, encode_aeronautics_snapshot, BuiltinContactSolver,
    RigidBodySubWorld, SubWorldHelmInput, SubWorldSyncFrame, SubWorldSyncKind,
    DEFAULT_BLOCK_MASS, SEA_LEVEL_AIR_DENSITY,
};
use create_adapter::orbital::{OrbitalDimension, OrbitalTransitionRouter};
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
    let wanted: HashSet<String> = names.into_iter().map(|name| ns_rl(name.as_ref())).collect();
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
    } else {
        // Clean-room exports omit the generated full state registry. Keep
        // anti-xray useful for vanilla blocks whose canonical default state
        // IDs are still available through the checked-in name table. Modded
        // names remain recorded by the owning bridge until their registry is
        // available; `name_to_state` returns None for those names.
        state_ids.extend(
            wanted
                .iter()
                .filter(|name| name.starts_with("minecraft:"))
                .filter_map(|name| name_to_state(name)),
        );
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

/// Authoritative, expiring helm-seat assignment. The lease is deliberately
/// small and wire-independent: `player_id` is the entity id the helm packet
/// arrived from, `dim_id` is the raw dimension the seat was granted in, and
/// `revision` is the ship assembly revision (not the pose revision, which
/// changes every tick). Reassembly, dimension transfer, timeout, detach, or
/// disconnect invalidates the lease and neutralizes latched controls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HelmSeatLease {
    player_id: u32,
    ship_id: u64,
    dim_id: u32,
    revision: u32,
    expires_tick: u64,
}

/// Rider attachment mode. `Seat` is rigidly fixed to the helm/passenger seat;
/// `Walking` is still ship-carried but may have its local deck offset updated
/// by authoritative player movement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RiderAttachmentKind {
    Seat,
    Walking,
}

/// World-space cell participating in atomic ship assembly. `pos` is the
/// source world cell; `local` is derived from `pos - origin` by the transfer
/// helper. `block_entity` is optional opaque serialized state.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ShipAssemblyCell {
    pos: [i32; 3],
    state_id: u32,
    block_id: String,
    block_entity: Option<Vec<u8>>,
}

/// Collision-checked world write produced by atomic disassembly. Callers
/// apply all writes in one externally synchronized transaction; because the
/// plan is produced only after every target passed the occupancy/protection
/// callback, a rejected plan leaves the ship fully intact.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ShipDisassemblyWrite {
    pos: [i32; 3],
    state_id: u32,
    block_id: String,
    block_entity: Option<Vec<u8>>,
}

/// Result of a committed atomic disassembly: the world writes plus riders
/// who were detached because their ship ceased to exist.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ShipDisassemblyPlan {
    ship_id: u64,
    dim_id: u32,
    writes: Vec<ShipDisassemblyWrite>,
    detached_riders: Vec<i32>,
}

/// Catalog admission summary for Create addons: supported recipes are merged
/// across every enabled profile while unknown serializers are explicit.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct AddonCatalogReport {
    admitted_mods: Vec<String>,
    recipe_serializers: Vec<String>,
    unsupported_serializers: Vec<String>,
    behavior_blocks: Vec<String>,
}

fn adb_processing_recipe_serializer(
    recipe: &create_adapter::addon_profile::AddonProcessingRecipe,
) -> String {
    recipe.serializer_id().to_string()
}

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
    /// Encoded S2C frames queued by assembly and simulation ticks.
    sync_frames: Vec<(u32, Vec<u8>)>,
    /// Monotonic per-ship revisions for client pose deltas.
    revisions: BTreeMap<u64, u32>,
    /// Assembly revisions checked by helm leases. Unlike pose revisions,
    /// these change only when a ship is assembled/reassembled.
    assembly_revisions: BTreeMap<u64, u32>,
    /// Absolute server tick clock supplied by `aeronautics_tick_once_for_server`.
    server_tick: u64,
    /// Player entity id -> authoritative expiring helm lease.
    helm_leases: BTreeMap<u32, HelmSeatLease>,
    /// `(player_id, ship_id)` pairs permitted to execute disassembly.
    disassembly_permits: BTreeSet<(u32, u64)>,
    /// Rider attachment modes keyed by entity id.
    rider_kinds: BTreeMap<i32, RiderAttachmentKind>,
    /// Last committed start/end rigid poses per ship: `(start_pos,start_quat,end_pos,end_quat)`.
    #[allow(clippy::type_complexity)]
    committed_poses: BTreeMap<u64, ([f64; 3], [f64; 4], [f64; 3], [f64; 4])>,
    /// Serialized block-entity payloads riding each ship's local voxels.
    ship_block_entities: BTreeMap<u64, BTreeMap<[i32; 3], Vec<u8>>>,
    /// Late-join/dimension-change client tracking: client id -> raw dimension.
    tracked_clients: BTreeMap<u32, u32>,
    /// Unsupported addon recipe serializers explicitly reported by catalog admission.
    unsupported_addon_serializers: BTreeSet<String>,
    /// Per-dimension atmosphere and transition rules for the active fleet.
    orbital_router: OrbitalTransitionRouter,
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
            sync_frames: Vec::new(),
            revisions: BTreeMap::new(),
            assembly_revisions: BTreeMap::new(),
            server_tick: 0,
            helm_leases: BTreeMap::new(),
            disassembly_permits: BTreeSet::new(),
            rider_kinds: BTreeMap::new(),
            committed_poses: BTreeMap::new(),
            ship_block_entities: BTreeMap::new(),
            tracked_clients: BTreeMap::new(),
            unsupported_addon_serializers: BTreeSet::new(),
            orbital_router: OrbitalTransitionRouter::new(),
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

    /// Merge every enabled addon's propulsion/control catalog into one
    /// synthetic profile for ship assembly. Aero metallurgy, power, and custom
    /// propulsion addons frequently contribute disjoint propeller/lift/helm
    /// block lists; picking the first profile would silently disable the
    /// others. Numeric aerodynamic coefficients come from the first declaring
    /// profile, while block ownership lists are unioned deterministically.
    fn combined_propulsion_profile(&self) -> Option<CreateAddonProfile> {
        let mut iter = self.addons.iter().filter(|a| a.propulsion.is_some());
        let mut combined = iter.next()?.clone();
        let base = combined.propulsion.as_mut().expect("filtered Some");
        for addon in iter {
            let Some(prop) = &addon.propulsion else {
                continue;
            };
            for (src, dst) in [
                (&prop.propeller_blocks, &mut base.propeller_blocks),
                (&prop.buoyancy_blocks, &mut base.buoyancy_blocks),
                (&prop.rudder_blocks, &mut base.rudder_blocks),
            ] {
                for id in src {
                    if !dst.contains(id) {
                        dst.push(id.clone());
                    }
                }
            }
        }
        // Kinetic min-RPM behavior is also additive across profiles.
        for addon in &self.addons {
            if addon.mod_id == combined.mod_id {
                continue;
            }
            for spec in &addon.kinetic_consumers {
                if !combined.kinetic_consumers.iter().any(|s| s.id == spec.id) {
                    combined.kinetic_consumers.push(spec.clone());
                }
            }
            for spec in &addon.kinetic_sources {
                if !combined.kinetic_sources.iter().any(|s| s.id == spec.id) {
                    combined.kinetic_sources.push(spec.clone());
                }
            }
            combined.processing_recipes.extend(addon.processing_recipes.clone());
        }
        combined.mod_id = "megu:combined_propulsion".into();
        Some(combined)
    }

    /// Admit a catalog of already-discovered addon profiles into this runtime.
    /// Profiles are merged rather than winner-take-all; their processing
    /// recipe serializer ids are cataloged and unknown serializers are
    /// explicitly surfaced as unsupported instead of disappearing.
    fn register_admitted_addon_catalog(
        &mut self,
        profiles: Vec<CreateAddonProfile>,
    ) -> AddonCatalogReport {
        let mut report = AddonCatalogReport::default();
        for profile in profiles {
            if !report.admitted_mods.iter().any(|id| id == &profile.mod_id) {
                report.admitted_mods.push(profile.mod_id.clone());
            }
            for recipe in &profile.processing_recipes {
                let serializer = adb_processing_recipe_serializer(recipe);
                if create_adapter::data_loader::CreateRecipeKind::from_type_str(&serializer)
                    .is_some()
                {
                    if !report.recipe_serializers.contains(&serializer) {
                        report.recipe_serializers.push(serializer);
                    }
                } else if self.unsupported_addon_serializers.insert(serializer.clone())
                    && !report.unsupported_serializers.contains(&serializer)
                {
                    report.unsupported_serializers.push(serializer);
                }
            }
            for id in profile
                .kinetic_sources
                .iter()
                .map(|s| &s.id)
                .chain(profile.kinetic_consumers.iter().map(|s| &s.id))
                .chain(profile.connectors.iter().map(|s| &s.id))
                .chain(profile.assembly_anchors.iter())
                .chain(profile.decorative_blocks.iter())
            {
                if !report.behavior_blocks.contains(id) {
                    report.behavior_blocks.push(id.clone());
                }
            }
            self.add_addon(profile);
        }
        report.recipe_serializers.sort();
        report.unsupported_serializers.sort();
        report.behavior_blocks.sort();
        report
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
        let combined = self.combined_propulsion_profile();
        let addon = match addon_mod_id {
            Some(id) => self
                .addons
                .iter()
                .find(|a| a.mod_id == id)
                .or(combined.as_ref()),
            None => combined.as_ref(),
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
        self.revisions.insert(subworld_id, 0);
        let next_revision = self
            .assembly_revisions
            .get(&subworld_id)
            .copied()
            .unwrap_or(0)
            .saturating_add(1);
        self.assembly_revisions.insert(subworld_id, next_revision);
        self.ship_block_entities.entry(subworld_id).or_default();
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
        self.attach_rider(eid, subworld_id, local_offset, true)
    }

    /// Real seat/walking rider attachment. The offset is measured from the
    /// ship center of mass, must be finite and bounded, and the ship must
    /// exist. Seated riders are fixed; walking riders use the same rigid
    /// carry but may update their local offset through `move_walking_rider`.
    fn attach_rider(
        &mut self,
        eid: i32,
        subworld_id: u64,
        local_offset: [f64; 3],
        seated: bool,
    ) -> bool {
        if !self.airships.contains_key(&subworld_id)
            || !local_offset.iter().all(|v| v.is_finite())
            || local_offset.iter().any(|v| v.abs() > 256.0)
        {
            return false;
        }
        self.passengers.insert(eid, (subworld_id, local_offset));
        self.rider_kinds.insert(
            eid,
            if seated {
                RiderAttachmentKind::Seat
            } else {
                RiderAttachmentKind::Walking
            },
        );
        true
    }

    /// Update a walking rider's local deck offset. Seated riders reject the
    /// move because their authoritative position belongs to the seat.
    fn move_walking_rider(&mut self, eid: i32, local_offset: [f64; 3]) -> bool {
        if self.rider_kinds.get(&eid) != Some(&RiderAttachmentKind::Walking)
            || !local_offset.iter().all(|v| v.is_finite())
            || local_offset.iter().any(|v| v.abs() > 256.0)
        {
            return false;
        }
        let Some((_, offset)) = self.passengers.get_mut(&eid) else {
            return false;
        };
        *offset = local_offset;
        true
    }

    /// Player left the airship (dismount, disassembly, logout).
    fn detach_passenger(&mut self, eid: i32) -> bool {
        self.detach_rider(eid)
    }

    /// Detach a seat or walking rider; helm authority is revoked as well when
    /// the same entity held it, so a departing pilot cannot retain control.
    fn detach_rider(&mut self, eid: i32) -> bool {
        self.rider_kinds.remove(&eid);
        if let Ok(player) = u32::try_from(eid) {
            self.detach_helm_player(player);
        }
        self.passengers.remove(&eid).is_some()
    }

    /// The exact world displacement the last step applied to passenger `eid`'s
    /// attached point — the same value `step` just granted to anti-cheat.
    fn passenger_delta(&self, eid: i32, dt: f64) -> Option<[f64; 3]> {
        if let Some(delta) = self.committed_passenger_delta(eid) {
            return Some(delta);
        }
        let (ship, offset) = self.passengers.get(&eid)?;
        Some(self.airships.get(ship)?.passenger_world_delta(*offset, dt))
    }

    /// Anything worth ticking: live ships. Passenger entries without a
    /// ship cannot produce motion or sync work, so an empty fleet is a
    /// strict zero-work dormant state even if a stale attachment remains.
    fn has_work(&self) -> bool {
        !self.airships.is_empty()
    }

    /// Advance every airship by `dt` seconds (0.05 at 20 TPS): each
    /// `RigidBodySubWorld` steps its embedded `CreateFactory` kinetic network,
    /// propeller thrust, buoyancy and Euler dynamics. The orbital router uses
    /// the bridge's absolute server tick, so transfer cooldowns are fleet-size
    /// independent. Rider motion is derived from committed start/end poses.
    fn step(&mut self, dt: f64, gravity: f64, air_density: f64) {
        self.ticks_stepped += 1;
        self.expire_helm_leases();
        self.orbital_router.set_base_gravity(gravity);
        let server_tick = self.server_tick.max(self.ticks_stepped);
        let ship_ids: Vec<u64> = self.airships.keys().copied().collect();
        for ship_id in ship_ids {
            let Some(mut sub) = self.airships.remove(&ship_id) else {
                continue;
            };
            if self.orbital_router.dimension(sub.parent_dim).is_none() {
                self.orbital_router.register_dimension(
                    sub.parent_dim,
                    OrbitalDimension::new(
                        DimId::from_raw(sub.parent_dim).dimension_id(),
                        create_adapter::aeronautics::AtmospherePhysics {
                            air_density,
                            ..create_adapter::aeronautics::AtmospherePhysics::OVERWORLD
                        },
                    ),
                );
            }
            let start = (sub.pos, sub.quat);
            let parent_dim = sub.parent_dim;
            let terrain = |cell: [i32; 3]| {
                cell[1] >= MIN_Y
                    && cell[1] < MIN_Y + 384
                    && block_at_dim(
                        DimId::from_raw(parent_dim),
                        cell[0],
                        cell[1],
                        cell[2],
                    ) != 0
            };
            let mut backend = BuiltinContactSolver::default();
            {
                let mut peers: Vec<&mut RigidBodySubWorld> =
                    self.airships.values_mut().collect();
                let _ = self
                    .orbital_router
                    .step_with_dimension_transitions_contacts_at(
                        &mut sub,
                        dt,
                        server_tick,
                        &mut backend,
                        &terrain,
                        &mut peers,
                    );
            }
            self.committed_poses
                .insert(sub.id, (start.0, start.1, sub.pos, sub.quat));
            self.airships.insert(sub.id, sub);
        }
        let mut pose_frames = Vec::new();
        for (id, sub) in &self.airships {
            let revision = self.revisions.entry(*id).or_default();
            *revision = revision.saturating_add(1);
            if let Ok(record) = adb_ship_sw6d_record(sub, u64::from(*revision), false, None) {
                if let Ok(bytes) = adb_encode_sw6d_frame(ADB_SW6D_MODE_POSE, &[record]) {
                    pose_frames.push((sub.parent_dim, bytes));
                }
            }
        }
        self.sync_frames.extend(pose_frames);
        for (eid, (ship, offset)) in &self.passengers {
            let Some(&(from_pos, from_quat, to_pos, to_quat)) = self.committed_poses.get(ship)
            else {
                continue;
            };
            ac_note_carried_motion(
                *eid,
                RigidBodySubWorld::passenger_world_delta_exact(
                    from_pos, from_quat, to_pos, to_quat, *offset,
                ),
            );
        }
    }

    /// Advance the bridge to an authoritative absolute server tick. This is
    /// the tick entry `main.rs` wiring should call; legacy dt-only callers
    /// fall back to the local stepped count.
    fn tick_once_for_server(&mut self, server_tick: u64) {
        self.server_tick = self.server_tick.max(server_tick);
        if self.has_work() {
            self.step(0.05, 9.81, SEA_LEVEL_AIR_DENSITY);
        } else {
            self.expire_helm_leases();
        }
    }

    /// Grant or refresh an authoritative helm seat for `ttl_ticks`.
    fn assign_helm_seat(
        &mut self,
        player_id: u32,
        ship_id: u64,
        ttl_ticks: u64,
    ) -> Result<HelmSeatLease, String> {
        let ship = self
            .airships
            .get(&ship_id)
            .ok_or_else(|| "unknown helm ship".to_string())?;
        if ttl_ticks == 0 {
            return Err("helm lease ttl must be positive".into());
        }
        let lease = HelmSeatLease {
            player_id,
            ship_id,
            dim_id: ship.parent_dim,
            revision: self.assembly_revisions.get(&ship_id).copied().unwrap_or(0),
            expires_tick: self.server_tick.saturating_add(ttl_ticks),
        };
        self.helm_leases.insert(player_id, lease);
        Ok(lease)
    }

    /// Permit one seated player to execute the destructive disassembly path.
    fn permit_ship_disassembly(&mut self, player_id: u32, ship_id: u64) {
        self.disassembly_permits.insert((player_id, ship_id));
    }

    /// Detach/logout cleanup: remove helm authority and immediately zero any
    /// latched actuator state so a departing player cannot keep flying a ship.
#[allow(dead_code)]
    fn detach_helm_player(&mut self, player_id: u32) -> bool {
        let lease = self.helm_leases.remove(&player_id);
        if let Some(lease) = lease {
            self.disassembly_permits.remove(&(player_id, lease.ship_id));
            self.neutralize_helm(lease.ship_id);
            true
        } else {
            false
        }
    }

    /// Zero the helm actuators of `ship_id`, ignoring unknown ships.
    fn neutralize_helm(&mut self, ship_id: u64) {
        if let Some(ship) = self.airships.get_mut(&ship_id) {
            let neutral = SubWorldHelmInput {
                subworld_id: ship_id,
                throttle: 0.0,
                pitch: 0.0,
                yaw: 0.0,
                roll: 0.0,
                buoyancy_delta: 0.0,
                disassemble: false,
            };
            let _ = ship.apply_helm_input(&neutral);
        }
    }

    /// Expire timed-out seats and neutralize their ships exactly once.
    fn expire_helm_leases(&mut self) {
        let expired: Vec<(u32, u64)> = self
            .helm_leases
            .iter()
            .filter(|(_, lease)| self.server_tick >= lease.expires_tick)
            .map(|(&player, lease)| (player, lease.ship_id))
            .collect();
        for (player, ship) in expired {
            self.helm_leases.remove(&player);
            self.disassembly_permits.remove(&(player, ship));
            self.neutralize_helm(ship);
        }
    }

    /// Validate identity, ship, dimension, assembly revision, lease expiry,
    /// and destructive-command permission before applying one helm packet.
    fn validate_helm_input(
        &mut self,
        player_id: u32,
        input: &SubWorldHelmInput,
    ) -> Result<(), String> {
        let lease = self
            .helm_leases
            .get(&player_id)
            .copied()
            .ok_or_else(|| "player does not hold the helm seat".to_string())?;
        if lease.player_id != player_id || lease.ship_id != input.subworld_id {
            return Err("helm packet identity does not match the seated lease".into());
        }
        if self.server_tick >= lease.expires_tick {
            self.detach_helm_player(player_id);
            return Err("helm seat lease expired".into());
        }
        let ship = self
            .airships
            .get(&input.subworld_id)
            .ok_or_else(|| "unknown subworld".to_string())?;
        if ship.parent_dim != lease.dim_id {
            self.detach_helm_player(player_id);
            return Err("helm ship changed dimension; lease invalid".into());
        }
        if self.assembly_revisions.get(&input.subworld_id).copied().unwrap_or(0)
            != lease.revision
        {
            self.detach_helm_player(player_id);
            return Err("helm ship assembly revision mismatch".into());
        }
        if input.disassemble
            && !self
                .disassembly_permits
                .contains(&(player_id, input.subworld_id))
        {
            return Err("player is not permitted to disassemble this ship".into());
        }
        Ok(())
    }

    /// Exact rider delta from the last committed start-to-end pose. This
    /// deliberately avoids post-step forward projection: the granted movement
    /// is the movement the rigid body actually committed.
    fn committed_passenger_delta(&self, eid: i32) -> Option<[f64; 3]> {
        let (ship, offset) = self.passengers.get(&eid)?;
        let &(from_pos, from_quat, to_pos, to_quat) = self.committed_poses.get(ship)?;
        Some(RigidBodySubWorld::passenger_world_delta_exact(
            from_pos, from_quat, to_pos, to_quat, *offset,
        ))
    }

    /// Atomically assemble a connected ship hull and every block-entity
    /// payload. The world-side removals must be committed by the caller as
    /// part of the same external transaction; this helper is total preflight
    /// plus subworld construction, so malformed/disconnected input never
    /// produces a half-assembled ship or dropped block entities.
    fn assemble_ship_atomic(
        &mut self,
        subworld_id: u64,
        parent_dim: u32,
        origin: [i32; 3],
        cells: &[ShipAssemblyCell],
        addon_mod_id: Option<&str>,
        block_mass: f64,
    ) -> Result<(), String> {
        if cells.is_empty() {
            return Err("ship assembly must contain at least one block".into());
        }
        use std::collections::VecDeque;
        let mut local_cells = BTreeMap::new();
        let mut entities = BTreeMap::new();
        let mut positions = BTreeSet::new();
        for cell in cells {
            if !cell.block_id.contains(':') {
                return Err(format!("invalid ship block id {:?}", cell.block_id));
            }
            let local = [
                cell.pos[0].checked_sub(origin[0]).ok_or("ship local coordinate overflow")?,
                cell.pos[1].checked_sub(origin[1]).ok_or("ship local coordinate overflow")?,
                cell.pos[2].checked_sub(origin[2]).ok_or("ship local coordinate overflow")?,
            ];
            if local
                .iter()
                .any(|v| *v < i32::from(i16::MIN) || *v > i32::from(i16::MAX))
            {
                return Err("ship block lies outside the wire coordinate range".into());
            }
            if !positions.insert(cell.pos) || local_cells.contains_key(&local) {
                return Err("duplicate ship assembly cell".into());
            }
            local_cells.insert(local, cell.block_id.clone());
            if let Some(payload) = &cell.block_entity {
                entities.insert(local, payload.clone());
            }
        }
        // Connectivity preflight: face-connected 6-neighbor flood fill over
        // local cells. Air pockets are fine; disjoint assemblies are not.
        let first = *positions.iter().next().expect("nonempty checked");
        let mut seen = BTreeSet::from([first]);
        let mut queue = VecDeque::from([first]);
        while let Some(pos) = queue.pop_front() {
            for axis in 0..3 {
                for sign in [-1i32, 1] {
                    let mut next = pos;
                    next[axis] = next[axis].saturating_add(sign);
                    if positions.contains(&next) && seen.insert(next) {
                        queue.push_back(next);
                    }
                }
            }
        }
        if seen.len() != positions.len() {
            return Err("ship assembly cells are not face-connected".into());
        }
        self.assemble_airship(
            subworld_id,
            parent_dim,
            local_cells,
            addon_mod_id,
            block_mass,
        )?;
        self.ship_block_entities.insert(subworld_id, entities);
        if let Some(ship) = self.airships.get_mut(&subworld_id) {
            ship.pos = [
                f64::from(origin[0]) + ship.center_of_mass[0],
                f64::from(origin[1]) + ship.center_of_mass[1],
                f64::from(origin[2]) + ship.center_of_mass[2],
            ];
            self.committed_poses
                .insert(subworld_id, (ship.pos, ship.quat, ship.pos, ship.quat));
        }
        Ok(())
    }

    /// Produce a collision-checked atomic disassembly plan. Every current
    /// hull cell is transformed by the ship's committed rigid pose (rotation
    /// snapped to the nearest quarter turn for grid write-back) and every
    /// carried block entity follows its cell. `cell_available` must encode
    /// both obstruction and protection policy. If any target fails, the ship,
    /// riders, leases, and block entities remain untouched; otherwise the
    /// ship is removed exactly once and the complete write plan returned.
    fn disassemble_ship_atomic(
        &mut self,
        subworld_id: u64,
        mut cell_available: impl FnMut([i32; 3], &str) -> bool,
    ) -> Result<ShipDisassemblyPlan, String> {
        let ship = self
            .airships
            .get(&subworld_id)
            .ok_or_else(|| "unknown subworld".to_string())?;
        let registry = self
            .registry
            .as_ref()
            .ok_or_else(|| "aeronautics registry unavailable".to_string())?;
        // Extract yaw for nearest-quarter grid snapping. Roll/pitch-only
        // components intentionally do not select a different grid rotation.
        let [qx, qy, qz, qw] = ship.quat;
        let yaw = (2.0 * (qw * qy + qx * qz))
            .atan2(1.0 - 2.0 * (qy * qy + qx * qx));
        let quarter = (yaw.to_degrees() / 90.0).round() as i32;
        let rot = quarter.rem_euclid(4);
        let base = [
            (ship.pos[0] - ship.center_of_mass[0]).round() as i32,
            (ship.pos[1] - ship.center_of_mass[1]).round() as i32,
            (ship.pos[2] - ship.center_of_mass[2]).round() as i32,
        ];
        let entities = self
            .ship_block_entities
            .get(&subworld_id)
            .cloned()
            .unwrap_or_default();
        let mut writes = Vec::with_capacity(ship.blocks.len());
        let mut targets = BTreeSet::new();
        for (&[lx, ly, lz], block_id) in &ship.blocks {
            let (rx, rz) = match rot {
                0 => (lx, lz),
                1 => (lz, -lx),
                2 => (-lx, -lz),
                3 => (-lz, lx),
                _ => unreachable!(),
            };
            let pos = [
                base[0].checked_add(rx).ok_or("disassembly target overflow")?,
                base[1].checked_add(ly).ok_or("disassembly target overflow")?,
                base[2].checked_add(rz).ok_or("disassembly target overflow")?,
            ];
            if !targets.insert(pos) {
                return Err("rotated disassembly writes overlap".into());
            }
            if !cell_available(pos, block_id) {
                return Err(format!("disassembly target {pos:?} is obstructed or protected"));
            }
            let state_id = registry.decode_canonical(block_id).unwrap_or(0);
            writes.push(ShipDisassemblyWrite {
                pos,
                state_id,
                block_id: block_id.clone(),
                block_entity: entities.get(&[lx, ly, lz]).cloned(),
            });
        }

        // Only after total preflight succeeds do we mutate ownership.
        let ship = self.airships.remove(&subworld_id).expect("checked above");
        let detached_riders: Vec<i32> = self
            .passengers
            .iter()
            .filter(|(_, (ship_id, _))| *ship_id == subworld_id)
            .map(|(&eid, _)| eid)
            .collect();
        self.passengers
            .retain(|_, (ship_id, _)| *ship_id != subworld_id);
        self.rider_kinds.retain(|eid, _| self.passengers.contains_key(eid));
        self.helm_leases.retain(|player, lease| {
            if lease.ship_id == subworld_id {
                self.disassembly_permits.remove(&(*player, subworld_id));
                false
            } else {
                true
            }
        });
        self.revisions.remove(&subworld_id);
        self.committed_poses.remove(&subworld_id);
        self.ship_block_entities.remove(&subworld_id);
        let frame = SubWorldSyncFrame {
            kind: SubWorldSyncKind::Despawn,
            subworld_id,
            dim_id: DimId::from_raw(ship.parent_dim).dimension_id().to_owned(),
            pos: ship.pos,
            quat: ship.quat.map(|v| v as f32),
            vel: [0.0; 3],
            ang_vel: [0.0; 3],
            revision: self.revisions.get(&subworld_id).copied().unwrap_or(0),
            voxels: Vec::new(),
        }
        .encode()?;
        self.sync_frames.push((ship.parent_dim, frame));
        Ok(ShipDisassemblyPlan {
            ship_id: subworld_id,
            dim_id: ship.parent_dim,
            writes,
            detached_riders,
        })
    }

    /// Encode a complete revisioned SW6D v2 baseline for every ship in
    /// `dim` — hull voxels, pivot, numeric dimension id, pose and velocities
    /// included. `None` means either no ships are present or validation failed
    /// closed.
    fn sw6d_baseline_frame(&self, dim: u32) -> Option<Vec<u8>> {
        let mut records = Vec::new();
        for ship in self.airships.values().filter(|ship| ship.parent_dim == dim) {
            let revision = self.revisions.get(&ship.id).copied().unwrap_or(0);
            records.push(
                adb_ship_sw6d_record(
                    ship,
                    u64::from(revision),
                    true,
                    self.registry.as_ref(),
                )
                .ok()?,
            );
        }
        if records.is_empty() {
            return None;
        }
        adb_encode_sw6d_frame(ADB_SW6D_MODE_BASELINE, &records).ok()
    }

    /// Record a client join/dimension change and return the baseline frame
    /// that must precede any incremental pose delta for that client.
    fn client_join_baseline(&mut self, client_id: u32, dim: u32) -> Vec<Vec<u8>> {
        self.tracked_clients.insert(client_id, dim);
        self.sw6d_baseline_frame(dim).into_iter().collect()
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
        self.rider_kinds.clear();
        self.committed_poses = self
            .airships
            .values()
            .map(|ship| (ship.id, (ship.pos, ship.quat, ship.pos, ship.quat)))
            .collect();
        // Restore is a complete fleet re-baseline, not a sequence of pose
        // deltas: clients must receive full hull+pivot+dimension records first.
        for dim in self
            .airships
            .values()
            .map(|ship| ship.parent_dim)
            .collect::<BTreeSet<_>>()
        {
            if let Some(frame) = self.sw6d_baseline_frame(dim) {
                self.sync_frames.push((dim, frame));
            }
        }
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
        let raw_jars: Vec<(String, Vec<u8>)> = entries
            .iter()
            .filter(|(path, bytes)| path.ends_with(".jar") && bytes.starts_with(b"PK\x03\x04"))
            .cloned()
            .collect();
        let zip = adb_stored_zip(&entries);
        let mut data = if zip.is_empty() {
            mod_data::LoadedModData::default()
        } else {
            let bundle =
                mod_adapter_core::auto_adapter::extract_mod_data(&zip, "aeronautics entries")?;
            mod_data::LoadedModData {
                recipes: bundle.recipes,
                tags: bundle.tags,
                blockstates: bundle.blockstates,
                skipped_compressed_zip_entries: 0,
            }
        };
        for (path, bytes) in raw_jars {
            let bundle = mod_adapter_core::auto_adapter::extract_mod_data(&bytes, &path)?;
            data.recipes.extend(bundle.recipes);
            data.tags.extend(bundle.tags);
            data.blockstates.extend(bundle.blockstates);
        }
        let mut hidden: BTreeSet<String> = BTreeSet::new();
        for recipe in &data.recipes {
            if create_adapter::data_loader::CreateRecipeKind::from_type_str(&recipe.recipe_type)
                .is_none()
            {
                self.unsupported_addon_serializers
                    .insert(recipe.recipe_type.clone());
            }
        }
        let namespaces = adb_addon_namespaces(&data);
        for ns in namespaces {
            let mut profile = discover_create_addon_profile(&ns, &data);
            // Aeronautics 1.3.3 splits its real blocks across three bundled
            // namespaces. Preserve those identities in propulsion metadata
            // even when their path names do not match the generic heuristic.
            let mut propulsion = profile
                .propulsion
                .take()
                .unwrap_or_else(PropulsionSpec::aeronautics_default);
            for id in data
                .blockstates
                .keys()
                .filter(|id| id.starts_with(&format!("{ns}:")))
            {
                let path = id.split_once(':').map(|(_, path)| path).unwrap_or(id);
                if adb_is_real_propeller(id, path) {
                    propulsion.propeller_blocks.push(id.clone());
                }
                if adb_is_real_lift(id, path) {
                    propulsion.buoyancy_blocks.push(id.clone());
                }
                if adb_is_real_control(id, path) {
                    propulsion.rudder_blocks.push(id.clone());
                }
            }
            if !propulsion.propeller_blocks.is_empty()
                || !propulsion.buoyancy_blocks.is_empty()
                || !propulsion.rudder_blocks.is_empty()
            {
                profile.propulsion = Some(propulsion);
            }
            if profile.block_defs().is_empty() && profile.hidden_blocks.is_empty() {
                continue;
            }
            for name in &profile.hidden_blocks {
                hidden.insert(ns_rl(name));
                self.mod_hidden_names.insert(ns_rl(name));
            }
            let report = self.register_admitted_addon_catalog(vec![profile]);
            for serializer in report.unsupported_serializers {
                self.unsupported_addon_serializers.insert(serializer);
            }
        }
        Ok(register_hidden_block_names(
            hidden.iter().map(String::as_str),
        ))
    }
}

fn adb_is_real_propeller(id: &str, path: &str) -> bool {
    id == "aeronautics:andesite_propeller"
        || id == "aeronautics:gyroscopic_propeller_bearing"
        || (id.starts_with("simulated:") && path.contains("portable_engine"))
        || (id.starts_with("simulated:") && path.contains("propeller"))
        || path.contains("thruster")
}

fn adb_is_real_lift(id: &str, path: &str) -> bool {
    id == "aeronautics:adjustable_burner"
        || (id.starts_with("aeronautics:") && (path.contains("envelope") || path.contains("sail")))
        || (id.starts_with("simulated:") && path.contains("sail"))
        || path.contains("balloon")
}

fn adb_is_real_control(id: &str, path: &str) -> bool {
    (id.starts_with("simulated:") && path.contains("handle"))
        || id == "simulated:altitude_sensor"
        || id == "simulated:analog_transmission"
        || id == "offroad:wheel_mount"
        || id == "offroad:rockcutting_wheel"
        || id == "offroad:borehead_bearing"
        || path == "helm"
        || path == "cockpit"
        || path.contains("rudder")
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
                let mut t = dim_mod::DimTypeDescriptor::defaults("minecraft:the_nether", 0, 256);
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

    /// Build a Create orbital descriptor from the same live dimension type and
    /// environment profile used by portal and gravity resolution.
    fn orbital_dimension(&self, dim: DimId) -> Option<create_adapter::orbital::OrbitalDimension> {
        let desc = self.type_desc_for_dim(dim)?;
        let env = self
            .env_by_dim
            .get(&dim)
            .or_else(|| self.env_by_name.get(dim.dimension_id()))
            .cloned()
            .unwrap_or_else(|| dim_mod::DimensionEnvProfile::derive(&desc, 1.0, false));
        let atmosphere = create_adapter::aeronautics::AtmospherePhysics::from_env_profile(
            &env,
            create_adapter::aeronautics::SEA_LEVEL_AIR_DENSITY,
        );
        Some(create_adapter::orbital::OrbitalDimension::from_descriptor(
            &desc, atmosphere,
        ))
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
                    std::str::from_utf8(b)
                        .ok()
                        .map(|s| (p.clone(), s.to_string()))
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
        report.hidden_states_added = register_hidden_block_names(hidden.iter().map(String::as_str));

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
        let discovered: HashSet<&str> = data.dimensions.iter().map(|d| d.id.as_str()).collect();
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

    /// Environment profiles of every *discovered* dimension, in canonical
    /// `registered_dims` order — the state `AER1` carries. Builtin profiles are
    /// skipped: they re-derive at boot like `dim_types` and the portal table.
    fn persistent_env_records(&self) -> Vec<dim_mod::DimensionEnvProfile> {
        self.registered_dims
            .iter()
            .filter_map(|(name, dim)| {
                self.env_by_dim
                    .get(dim)
                    .or_else(|| self.env_by_name.get(name))
                    .cloned()
            })
            .collect()
    }

    /// Reinstall `AER1` environment records: profiles whose dimension still
    /// exists relink their `DimId`/`registered_dims` entry; profiles for a dim
    /// whose JAR vanished still answer through `env_by_name` so gravity and
    /// occlusion queries keep their last-known values.
    fn restore_env_records(&mut self, records: Vec<dim_mod::DimensionEnvProfile>) {
        for env in records {
            if let Some(dim) = DimId::parse_dimension_id(&env.dimension_id) {
                self.registered_dims.insert(env.dimension_id.clone(), dim);
                self.env_by_dim.insert(dim, env.clone());
            }
            self.env_by_name.insert(env.dimension_id.clone(), env);
        }
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

/// Wrap `(path, bytes)` JAR entries as a complete stored (uncompressed) ZIP.
fn adb_stored_zip(entries: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    let mut entry_count = 0u16;
    for (name, data) in entries {
        if !name.ends_with(".json")
            || !(name.starts_with("data/") || name.starts_with("assets/"))
            || name.len() > u16::MAX as usize
            || data.len() > u32::MAX as usize
        {
            continue;
        }
        let offset = out.len() as u32;
        let crc = megu_core::hash::crc32_ieee(data);
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes()); // local file header
        out.extend_from_slice(&20u16.to_le_bytes()); // version needed
        out.extend_from_slice(&0u16.to_le_bytes()); // flags
        out.extend_from_slice(&0u16.to_le_bytes()); // method 0 = stored
        out.extend_from_slice(&0u16.to_le_bytes()); // mod time
        out.extend_from_slice(&0u16.to_le_bytes()); // mod date
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes()); // comp size
        out.extend_from_slice(&(data.len() as u32).to_le_bytes()); // uncomp size
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // extra len
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);

        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u32.to_le_bytes());
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
        entry_count = entry_count.saturating_add(1);
    }
    let central_offset = out.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&entry_count.to_le_bytes());
    out.extend_from_slice(&entry_count.to_le_bytes());
    out.extend_from_slice(&(central.len() as u32).to_le_bytes());
    out.extend_from_slice(&central_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
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

// ---------- canonical SW6D v2 frame encoding ----------

const ADB_SW6D_MAGIC: [u8; 4] = *b"SW6D";
const ADB_SW6D_VERSION: u8 = 2;
const ADB_SW6D_MODE_BASELINE: u8 = 0;
const ADB_SW6D_MODE_SPAWN: u8 = 1;
const ADB_SW6D_MODE_POSE: u8 = 2;
const ADB_SW6D_MODE_DESPAWN: u8 = 3;

#[derive(Clone, Debug)]
struct AdbSw6dRecord {
    subworld_id: u64,
    dimension_id: u32,
    revision: u64,
    pos: [f64; 3],
    quat: [f32; 4],
    vel: [f32; 3],
    ang_vel: [f32; 3],
    pivot: [f32; 3],
    flags: u8,
    voxels: Vec<([i16; 3], u32)>,
}

fn adb_encode_sw6d_frame(mode: u8, records: &[AdbSw6dRecord]) -> Result<Vec<u8>, String> {
    if records.len() > 4096 || records.len() > u16::MAX as usize {
        return Err("SW6D record count exceeds limit".into());
    }
    if mode != ADB_SW6D_MODE_BASELINE && records.len() != 1 {
        return Err("SW6D delta frames carry exactly one record".into());
    }
    let allow_voxels = matches!(mode, ADB_SW6D_MODE_BASELINE | ADB_SW6D_MODE_SPAWN);
    for record in records {
        if !allow_voxels && !record.voxels.is_empty() {
            return Err("SW6D pose/despawn records cannot carry voxels".into());
        }
        if record.voxels.len() > create_adapter::aeronautics::MAX_SUBWORLD_VOXELS {
            return Err("SW6D voxel payload exceeds limit".into());
        }
        if !record.pos.iter().all(|v| v.is_finite())
            || !record.quat.iter().all(|v| v.is_finite())
            || !record.vel.iter().all(|v| v.is_finite())
            || !record.ang_vel.iter().all(|v| v.is_finite())
            || !record.pivot.iter().all(|v| v.is_finite())
        {
            return Err("SW6D record contains non-finite pose values".into());
        }
    }
    let voxel_len: usize = records.iter().map(|r| r.voxels.len() * 10).sum();
    let mut out = Vec::with_capacity(8 + records.len() * 93 + voxel_len);
    out.extend_from_slice(&ADB_SW6D_MAGIC);
    out.push(ADB_SW6D_VERSION);
    out.push(mode);
    out.extend_from_slice(&(records.len() as u16).to_be_bytes());
    for record in records {
        out.extend_from_slice(&record.subworld_id.to_be_bytes());
        out.extend_from_slice(&record.dimension_id.to_be_bytes());
        out.extend_from_slice(&record.revision.to_be_bytes());
        for value in record.pos {
            out.extend_from_slice(&value.to_be_bytes());
        }
        for value in record.quat {
            out.extend_from_slice(&value.to_be_bytes());
        }
        for value in record.vel {
            out.extend_from_slice(&value.to_be_bytes());
        }
        for value in record.ang_vel {
            out.extend_from_slice(&value.to_be_bytes());
        }
        for value in record.pivot {
            out.extend_from_slice(&value.to_be_bytes());
        }
        out.push(record.flags);
        out.extend_from_slice(&(record.voxels.len() as u32).to_be_bytes());
        for (offset, state) in &record.voxels {
            for value in offset {
                out.extend_from_slice(&value.to_be_bytes());
            }
            out.extend_from_slice(&state.to_be_bytes());
        }
    }
    Ok(out)
}

fn adb_ship_sw6d_record(
    ship: &RigidBodySubWorld,
    revision: u64,
    include_voxels: bool,
    registry: Option<&CreateBlockRegistry>,
) -> Result<AdbSw6dRecord, String> {
    let mut voxels = Vec::new();
    if include_voxels {
        for (&[x, y, z], canonical) in &ship.blocks {
            if x < i32::from(i16::MIN)
                || x > i32::from(i16::MAX)
                || y < i32::from(i16::MIN)
                || y > i32::from(i16::MAX)
                || z < i32::from(i16::MIN)
                || z > i32::from(i16::MAX)
            {
                return Err("ship block lies outside the SW6D voxel range".into());
            }
            let state = registry
                .and_then(|reg| reg.decode_canonical(canonical))
                .unwrap_or(0);
            voxels.push(([x as i16, y as i16, z as i16], state));
        }
    }
    Ok(AdbSw6dRecord {
        subworld_id: ship.id,
        dimension_id: ship.parent_dim,
        revision,
        pos: ship.pos,
        quat: ship.quat.map(|v| v as f32),
        vel: ship.vel.map(|v| v as f32),
        ang_vel: ship.omega.map(|v| v as f32),
        pivot: ship.center_of_mass.map(|v| v as f32),
        flags: 0,
        voxels,
    })
}

// ---------- global bridges + entry points ----------
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

/// Ensure a live vanilla or custom dimension is registered with the Create
/// orbital router using its shared dimension and environment descriptors.
#[cfg(feature = "create-offload")]
#[allow(dead_code)]
fn dimension_mod_ensure_orbital_dimension(dim: DimId) -> bool {
    let descriptor = dimension_mod_bridge()
        .lock()
        .ok()
        .and_then(|bridge| bridge.orbital_dimension(dim));
    let Some(descriptor) = descriptor else {
        return false;
    };
    if let Ok(mut bridge) = aeronautics_bridge().lock() {
        bridge
            .orbital_router
            .register_dimension(dim.raw(), descriptor.clone());
    }
    mod_offload_bridge()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .offload
        .register_orbital_dimension(dim.raw(), descriptor);
    true
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

/// Assemble a live airship from world-space block coordinates and queue its
/// complete S2C spawn snapshot. Every input block is a `(position,state_id,id)` tuple.
fn aeronautics_bridge_assemble_ship(
    dim: DimId,
    origin: [i32; 3],
    blocks: &[([i32; 3], u32, &str)],
) -> Result<u64, String> {
    if blocks.is_empty() || blocks.len() > create_adapter::aeronautics::MAX_SUBWORLD_VOXELS {
        return Err("ship assembly must contain 1..=65536 blocks".into());
    }
    let mut cells = BTreeMap::new();
    let mut voxels = Vec::with_capacity(blocks.len());
    for (world, state, name) in blocks {
        let local = [
            world[0] - origin[0],
            world[1] - origin[1],
            world[2] - origin[2],
        ];
        if local
            .iter()
            .any(|v| *v < i32::from(i16::MIN) || *v > i32::from(i16::MAX))
        {
            return Err("ship block lies outside the wire coordinate range".into());
        }
        if !name.contains(':') || name.len() > 32_767 {
            return Err(format!("invalid block id: {name}"));
        }
        if cells.insert(local, (*name).to_owned()).is_some() {
            return Err("duplicate ship block position".into());
        }
        voxels.push(([local[0] as i16, local[1] as i16, local[2] as i16], *state));
    }
    let mut bridge = aeronautics_bridge()
        .lock()
        .map_err(|_| "aeronautics bridge lock poisoned".to_string())?;
    bridge.ensure_registry()?;
    let id = bridge.next_subworld_id.max(1);
    bridge.assemble_airship(id, dim.raw(), cells, None, DEFAULT_BLOCK_MASS)?;
    let Some(sub) = bridge.airships.get_mut(&id) else {
        return Err("assembled ship was not registered".into());
    };
    // `pos` is the *center of mass* in world space, not the local origin:
    // `local_to_world([lx + 0.5, ly + 0.5, lz + 0.5])` must equal
    // `[world + 0.5]` for every assembled block, so the COM sits at
    // `origin + com`, not at `origin` — on asymmetric hulls the two differ
    // by whole blocks and the old code silently displaced the ship.
    sub.pos = [
        f64::from(origin[0]) + sub.center_of_mass[0],
        f64::from(origin[1]) + sub.center_of_mass[1],
        f64::from(origin[2]) + sub.center_of_mass[2],
    ];
    let frame = adb_encode_sw6d_frame(
        ADB_SW6D_MODE_SPAWN,
        &[AdbSw6dRecord {
            subworld_id: id,
            dimension_id: dim.raw(),
            revision: 0,
            pos: sub.pos,
            quat: sub.quat.map(|v| v as f32),
            vel: [0.0; 3],
            ang_vel: [0.0; 3],
            pivot: sub.center_of_mass.map(|v| v as f32),
            flags: 0,
            voxels,
        }],
    )?;
    bridge.sync_frames.push((dim.raw(), frame));
    Ok(id)
}

/// Apply a player helm packet to the addressed live ship. Player identity is
/// authoritative: only an unexpired seated lease in the ship's current
/// dimension and assembly revision may actuate the helm, and destructive
/// disassembly requires its separate permission bit.
fn aeronautics_bridge_apply_helm_input(
    player_eid: u32,
    input: &SubWorldHelmInput,
) -> Result<(), String> {
    input.encode()?;
    let mut bridge = aeronautics_bridge()
        .lock()
        .map_err(|_| "aeronautics bridge lock poisoned".to_string())?;
    bridge.validate_helm_input(player_eid, input)?;
    if input.disassemble {
        let dim_raw = bridge
            .airships
            .get(&input.subworld_id)
            .map(|ship| ship.parent_dim)
            .ok_or_else(|| "unknown subworld".to_string())?;
        let plan = bridge.disassemble_ship_atomic(input.subworld_id, |pos, _| {
            pos[1] >= MIN_Y
                && pos[1] < MIN_Y + 384
                && block_at_dim(DimId::from_raw(dim_raw), pos[0], pos[1], pos[2]) == 0
        })?;
        let revision = 1u64;
        let frame = adb_encode_sw6d_frame(
            ADB_SW6D_MODE_DESPAWN,
            &[AdbSw6dRecord {
                subworld_id: input.subworld_id,
                dimension_id: plan.dim_id,
                revision,
                pos: [0.0; 3],
                quat: [0.0, 0.0, 0.0, 1.0],
                vel: [0.0; 3],
                ang_vel: [0.0; 3],
                pivot: [0.0; 3],
                flags: 0,
                voxels: Vec::new(),
            }],
        )?;
        bridge.sync_frames.push((plan.dim_id, frame));
        drop(bridge);
        // World mutation happens only after the bridge mutex has been
        // released; the plan carries every block entity payload so contents
        // are never silently discarded.
        for write in &plan.writes {
            let edit = create_adapter::contraption_adapter::WorldBlockEdit {
                pos: create_adapter::factory::DimBlockPos::new(
                    plan.dim_id as i32,
                    write.pos[0],
                    write.pos[1],
                    write.pos[2],
                ),
                state_id: Some(write.state_id),
            };
            let _ = mob_commit_world_block_edit(&edit);
        }
        return Ok(());
    }
    let mut governed = *input;
    let sub = bridge
        .airships
        .get_mut(&input.subworld_id)
        .ok_or_else(|| "unknown subworld".to_string())?;
    // Authority follows installed hardware: no propeller mount means no
    // throttle actuator, and no control/helm/rudder mount means no torque.
    if sub.propellers.is_empty() {
        governed.throttle = 0.0;
    }
    if sub.rudders.is_empty() {
        governed.pitch = 0.0;
        governed.yaw = 0.0;
        governed.roll = 0.0;
    }
    sub.apply_helm_input(&governed)
}

/// Grant or refresh an authoritative helm seat. Returns the lease revision
/// and expiry tick for callers that surface seat state to clients.
#[allow(dead_code)]
fn aeronautics_bridge_assign_helm_seat(
    player_eid: u32,
    ship_id: u64,
    ttl_ticks: u64,
) -> Result<(u32, u64), String> {
    let mut bridge = aeronautics_bridge()
        .lock()
        .map_err(|_| "aeronautics bridge lock poisoned".to_string())?;
    bridge
        .assign_helm_seat(player_eid, ship_id, ttl_ticks)
        .map(|lease| (lease.revision, lease.expires_tick))
}

/// Permit one seated player to execute ship disassembly.
#[allow(dead_code)]
fn aeronautics_bridge_permit_ship_disassembly(player_eid: u32, ship_id: u64) -> bool {
    aeronautics_bridge()
        .lock()
        .map(|mut bridge| {
            if bridge.helm_leases.contains_key(&player_eid) {
                bridge.permit_ship_disassembly(player_eid, ship_id);
                true
            } else {
                false
            }
        })
        .unwrap_or(false)
}

/// Disconnect/dismount cleanup: revoke the helm lease and neutralize any
/// latched controls on the ship the player used to control.
#[allow(dead_code)]
fn detach_helm_player(player_id: u32) -> bool {
    aeronautics_bridge()
        .lock()
        .map(|mut bridge| bridge.detach_helm_player(player_id))
        .unwrap_or(false)
}

/// Late-join/dimension-change entry point: record the client's dimension and
/// return the revisioned full SW6D v2 baseline that must precede pose deltas.
#[allow(dead_code)]
fn aeronautics_client_join_baseline(client_id: u32, dim_id: DimId) -> Vec<Vec<u8>> {
    aeronautics_bridge()
        .lock()
        .map(|mut bridge| bridge.client_join_baseline(client_id, dim_id.raw()))
        .unwrap_or_default()
}

/// Absolute-server-tick entry point ready for `main.rs` tick wiring.
fn aeronautics_tick_once_for_server(server_tick: u64) {
    if let Ok(mut bridge) = aeronautics_bridge().lock() {
        bridge.tick_once_for_server(server_tick);
    }
}

/// Advance active airships and return queued `(dimension, encoded frame)` S2C updates.
fn aeronautics_bridge_step_and_drain_sync_frames(dt_secs: f32) -> Vec<(DimId, Vec<u8>)> {
    if !dt_secs.is_finite() || dt_secs <= 0.0 {
        return Vec::new();
    }
    aeronautics_bridge_tick(f64::from(dt_secs), 9.81, SEA_LEVEL_AIR_DENSITY);
    let Ok(mut bridge) = aeronautics_bridge().lock() else {
        return Vec::new();
    };
    std::mem::take(&mut bridge.sync_frames)
        .into_iter()
        .map(|(dim, bytes)| (DimId::from_raw(dim), bytes))
        .collect()
}

fn aeronautics_bridge_ship_summaries() -> Vec<(u64, DimId, [f64; 3], [f64; 3])> {
    aeronautics_bridge()
        .lock()
        .map(|bridge| {
            bridge
                .airships
                .values()
                .map(|ship| {
                    (
                        ship.id,
                        DimId::from_raw(ship.parent_dim),
                        ship.pos,
                        ship.vel,
                    )
                })
                .collect()
        })
        .unwrap_or_default()
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

/// Whether the aeronautics + dimension-mod bridges hold state Save v2 must
/// persist: a live airship, or at least one discovered (non-builtin) dimension
/// environment. Both empty means zero `AER1` bytes — vanilla saves untouched.
fn aeronautics_bridge_has_state() -> bool {
    let ships = aeronautics_bridge()
        .lock()
        .map(|b| !b.airships.is_empty())
        .unwrap_or(false);
    ships
        || dimension_mod_bridge()
            .lock()
            .map(|b| !b.registered_dims.is_empty())
            .unwrap_or(false)
}

/// Inner body of the `AER1` trailer: the `MSAE` fleet blob plus the
/// environment profiles of every discovered dimension. Dimension/type/portal
/// registrations re-derive from the dimension JARs at boot; what a restart
/// cannot re-derive — the live fleet and the discovered env overrides — is
/// what rides the trailer.
fn aeronautics_state_save_bytes() -> Vec<u8> {
    let msae = aeronautics_bridge_save_bytes();
    let records = dimension_mod_bridge()
        .lock()
        .map(|b| b.persistent_env_records())
        .unwrap_or_default();
    let mut out = Vec::with_capacity(msae.len() + 8 + records.len() * 96);
    mod_wire::write_varint(&mut out, msae.len() as u32);
    out.extend_from_slice(&msae);
    mod_wire::write_varint(&mut out, records.len() as u32);
    for env in &records {
        mod_wire::write_mc_string(&mut out, &env.dimension_id);
        out.extend_from_slice(&env.gravity_multiplier.to_be_bytes());
        out.push(env.requires_oxygen as u8);
        out.push(env.has_skylight as u8);
        out.push(env.has_ceiling as u8);
        out.push(env.supports_underground_occlusion as u8);
        mod_wire::write_mc_string(&mut out, &env.sky_effects);
    }
    out
}

/// Restore `aeronautics_state_save_bytes`; `false` on a corrupt buffer or a
/// lock failure — the fleet and env tables are left untouched either way.
fn aeronautics_state_load_bytes(bytes: &[u8]) -> bool {
    /// Bound on the embedded `MSAE` blob and the env-record count — real
    /// trailers are nowhere near either.
    const AER1_MAX_MSAE: usize = 1 << 26;
    const AER1_MAX_DIMS: usize = 4_096;
    let mut cursor = 0usize;
    let Some(msae_len) = mod_wire::read_varint(bytes, &mut cursor)
        .and_then(|n| usize::try_from(n).ok())
        .filter(|&n| n <= AER1_MAX_MSAE)
    else {
        return false;
    };
    let Some(msae) = bytes.get(cursor..cursor + msae_len) else {
        return false;
    };
    cursor += msae_len;
    let Some(env_count) = mod_wire::read_varint(bytes, &mut cursor)
        .and_then(|n| usize::try_from(n).ok())
        .filter(|&n| n <= AER1_MAX_DIMS)
    else {
        return false;
    };
    let mut records = Vec::with_capacity(env_count);
    for _ in 0..env_count {
        let Some(dimension_id) =
            mod_wire::read_mc_string(bytes, &mut cursor, mod_wire::MAX_REGISTRY_DATA_NAME_LEN)
        else {
            return false;
        };
        let Some(gravity_raw) = bytes.get(cursor..cursor + 8) else {
            return false;
        };
        cursor += 8;
        let gravity_multiplier = f64::from_be_bytes(gravity_raw.try_into().unwrap_or_default());
        if !gravity_multiplier.is_finite() {
            return false;
        }
        let Some(&[requires_oxygen, has_skylight, has_ceiling, occlusion]) =
            bytes.get(cursor..cursor + 4)
        else {
            return false;
        };
        cursor += 4;
        let Some(sky_effects) =
            mod_wire::read_mc_string(bytes, &mut cursor, mod_wire::MAX_REGISTRY_DATA_NAME_LEN)
        else {
            return false;
        };
        records.push(dim_mod::DimensionEnvProfile {
            dimension_id,
            gravity_multiplier,
            requires_oxygen: requires_oxygen != 0,
            has_skylight: has_skylight != 0,
            has_ceiling: has_ceiling != 0,
            supports_underground_occlusion: occlusion != 0,
            sky_effects,
        });
    }
    if cursor != bytes.len() {
        return false;
    }
    if !aeronautics_bridge_load_bytes(msae) {
        return false;
    }
    dimension_mod_bridge()
        .lock()
        .map(|mut b| b.restore_env_records(records))
        .is_ok()
}

/// Discover an arbitrary Create addon JAR's profiles and forward its `#c:ores`
/// /`#c:chests` hidden blocks into anti-xray. Returns the number of state ids
/// newly hidden.
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
    let report = dimension_mod_bridge()
        .lock()
        .map_err(|_| "dimension mod bridge lock poisoned".to_string())
        .and_then(|mut b| b.load_dimension_mod_entries(entries))?;
    #[cfg(feature = "create-offload")]
    for dim in DimId::all_registered()
        .into_iter()
        .filter(|dim| !dim.is_vanilla())
    {
        let _ = dimension_mod_ensure_orbital_dimension(dim);
    }
    Ok(report)
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
    use create_adapter::orbital::{OrbitalTransitionRule, OrbitalTrigger};

    /// Several tests here reprogram the process-global bridges; serialize them.
    /// A panicking sibling test must not cascade poison into the next one, so
    /// both guards recover the inner value.
    static ADB_LOCK: Mutex<()> = Mutex::new(());

    fn adb_lock() -> std::sync::MutexGuard<'static, AeronauticsRuntimeBridge> {
        aeronautics_bridge()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn dim_lock() -> std::sync::MutexGuard<'static, DimensionModRuntimeBridge> {
        dimension_mod_bridge()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn empty_fleet_with_stale_passenger_is_zero_work() {
        let _serial = ADB_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        *adb_lock() = AeronauticsRuntimeBridge::new();
        adb_lock().passengers.insert(77, (999, [0.0; 3]));
        aeronautics_bridge_tick(0.05, 9.81, SEA_LEVEL_AIR_DENSITY);
        assert_eq!(
            adb_lock().ticks_stepped,
            0,
            "a passenger whose ship no longer exists must not keep the fleet awake"
        );
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

    fn test_stored_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, data) in entries {
            let offset = out.len() as u32;
            let crc = megu_core::hash::crc32_ieee(data);
            out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
            out.extend_from_slice(&20u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&crc.to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(data);

            central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&crc.to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u32.to_le_bytes());
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let central_offset = out.len() as u32;
        out.extend_from_slice(&central);
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(central.len() as u32).to_le_bytes());
        out.extend_from_slice(&central_offset.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
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
            b.assemble_airship(
                1,
                dim.raw(),
                cells,
                Some("createaeronautics"),
                DEFAULT_BLOCK_MASS,
            )
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
        assert!(
            saw_overspeed,
            "ship must exceed vanilla move allowance to prove the carry path"
        );
        assert!(
            saw_descent,
            "one balloon must not lift the hull — descent exercises the NoFall leg"
        );
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
    fn dimension_mod_jar_auto_registers_live_track2_dim_and_portal_teleport_passes_anticheat() {
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
            assert!(
                dim.raw() >= 3,
                "custom dim must own a dynamic slot: {dim:?}"
            );
        }
        assert_eq!(crystal.dimension_id(), "testdim:crystal_caves");

        let orbital = dim_lock()
            .orbital_dimension(crystal)
            .expect("custom dimension derives a Create orbital descriptor");
        assert_eq!(orbital.id, "testdim:crystal_caves");
        assert_eq!((orbital.min_y, orbital.height), (-64, 384));
        assert_eq!(orbital.coordinate_scale, 4.0);
        assert_eq!(orbital.atmosphere.gravity_scale, 1.0);
        let offload_bridge = mod_offload_bridge()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let live_orbital = offload_bridge
            .offload
            .orbital_router()
            .dimension(crystal.raw())
            .expect("dimension load registers custom orbital route");
        assert_eq!(live_orbital.id, "testdim:crystal_caves");

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
        assert!(
            is_hidden(calcite),
            "#c:ores member must be hidden from xray"
        );

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
        let transition = dim_mod::detect_vertical_portal(&rule, [10, 70, 20], &block_at)
            .expect("glowstone frame must validate");
        assert_eq!(transition.target_dim, "testdim:crystal_caves");
        assert_eq!(transition.cell_count, 6);

        // The traveller steps through far from the origin: scale 1.0 -> 4.0
        // maps the 3000-block journey through `ac_expect_teleport` + grace.
        let traveller_eid = 988_010;
        let control_eid = 988_011;
        let traveller = mk_client(
            "adb_traveller",
            traveller_eid,
            DimId::Overworld,
            (0.5, 80.0, 0.5),
        );
        let control = mk_client(
            "adb_control",
            control_eid,
            DimId::Overworld,
            (0.5, 80.0, 0.5),
        );
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
        let to = (
            intent.target_pos[0],
            intent.target_pos[1],
            intent.target_pos[2],
        );
        let v = ac_check_move(&traveller, from, to, false);
        assert!(
            matches!(v, AcMoveVerdict::Accept { .. }),
            "granted portal hop must accept, got {v:?}"
        );
        assert_eq!(ac_flag_count(traveller_eid, AcCheckId::Movement), 0);
        assert_eq!(ac_flag_count(traveller_eid, AcCheckId::NoFall), 0);

        // The identical hop without the portal grant is a plain blink: flagged.
        let v = ac_check_move(&control, from, to, false);
        assert!(
            matches!(v, AcMoveVerdict::Setback),
            "ungranted portal hop must flag"
        );
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
            raft.insert(
                [0, 0, 0],
                canonical(&reg, "createaeronautics:airship_assembler"),
            );
            raft.insert(
                [1, 0, 0],
                canonical(&reg, "createaeronautics:airship_assembler"),
            );
            raft.insert(
                [0, 1, 0],
                canonical(&reg, "createaeronautics:balloon_envelope"),
            );
            raft.insert(
                [1, 1, 0],
                canonical(&reg, "createaeronautics:balloon_envelope"),
            );
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
        assert_eq!(
            reblob, blob1,
            "save -> load -> save must round-trip exactly"
        );
        // The restored fleet keeps flying (the powered ship's live network
        // survived the codec).
        let before = adb_lock().airships[&7].pos;
        for _ in 0..5 {
            aeronautics_bridge_tick(0.05, 9.81, SEA_LEVEL_AIR_DENSITY);
        }
        let after = adb_lock().airships[&7].pos;
        assert!(
            after[0] > before[0],
            "restored ship must keep +X thrust: {before:?} -> {after:?}"
        );

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
        assert!(profile
            .hidden_blocks
            .contains(&"mysteryaddon:flux_ore".to_string()));
        // The rebuilt shared registry now knows the addon's blocks.
        b.ensure_registry().unwrap();
        let reg = b
            .registry
            .as_ref()
            .expect("registry rebuilt after discovery");
        assert!(reg.default_state("mysteryaddon:flux_propeller").is_some());
        assert!(reg.default_state("mysteryaddon:flux_ore").is_some());
    }

    #[test]
    fn bundled_aeronautics_jarjar_discovers_all_three_real_namespaces() {
        let _serial = ADB_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        *adb_lock() = AeronauticsRuntimeBridge::new();
        let aero = test_stored_zip(&[(
            "assets/aeronautics/blockstates/andesite_propeller.json",
            br#"{"variants":{"":{"model":"aeronautics:block/andesite_propeller"}}}"#,
        )]);
        let simulated = test_stored_zip(&[(
            "assets/simulated/blockstates/black_portable_engine.json",
            br#"{"variants":{"":{"model":"simulated:block/black_portable_engine"}}}"#,
        )]);
        let offroad = test_stored_zip(&[(
            "assets/offroad/blockstates/wheel_mount.json",
            br#"{"variants":{"":{"model":"offroad:block/wheel_mount"}}}"#,
        )]);
        let bundled = test_stored_zip(&[
            ("META-INF/jarjar/aeronautics.jar", aero.as_slice()),
            ("META-INF/jarjar/simulated.jar", simulated.as_slice()),
            ("META-INF/jarjar/offroad.jar", offroad.as_slice()),
        ]);
        {
            let mut bridge = adb_lock();
            bridge
                .discover_addon_jar([("create-aeronautics-bundled.jar", bundled.as_slice())])
                .unwrap();
            for namespace in ["aeronautics", "simulated", "offroad"] {
                assert!(
                    bridge
                        .addons
                        .iter()
                        .any(|profile| profile.mod_id == namespace),
                    "missing {namespace}"
                );
            }
            let aero_profile = bridge
                .addons
                .iter()
                .find(|p| p.mod_id == "aeronautics")
                .unwrap();
            assert!(aero_profile
                .propulsion
                .as_ref()
                .unwrap()
                .propeller_blocks
                .contains(&"aeronautics:andesite_propeller".to_string()));
            let simulated_profile = bridge
                .addons
                .iter()
                .find(|p| p.mod_id == "simulated")
                .unwrap();
            assert!(simulated_profile
                .propulsion
                .as_ref()
                .unwrap()
                .propeller_blocks
                .contains(&"simulated:black_portable_engine".to_string()));
            let offroad_profile = bridge
                .addons
                .iter()
                .find(|p| p.mod_id == "offroad")
                .unwrap();
            assert!(offroad_profile
                .propulsion
                .as_ref()
                .unwrap()
                .rudder_blocks
                .contains(&"offroad:wheel_mount".to_string()));
        }
    }

    #[test]
    fn live_world_assembly_helm_flight_and_sync_drain() {
        let _serial = ADB_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        *adb_lock() = AeronauticsRuntimeBridge::new();
        let blocks = [
            ([10, 100, 10], 1, "aeronautics:andesite_propeller"),
            ([11, 100, 10], 2, "aeronautics:red_envelope"),
            ([10, 100, 11], 3, "aeronautics:rudder"),
        ];
        let id =
            aeronautics_bridge_assemble_ship(DimId::Overworld, [10, 100, 10], &blocks).unwrap();
        aeronautics_bridge()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .orbital_router
            .register_rule(OrbitalTransitionRule::new(
                "test-ascent",
                DimId::Overworld.raw(),
                OrbitalTrigger::AscendCeiling { ceiling_y: 100.1 },
                2,
            ));
        let spawn_frames = aeronautics_bridge_step_and_drain_sync_frames(0.05);
        assert!(spawn_frames
            .iter()
            .any(|(_, bytes)| SubWorldSyncFrame::decode(bytes)
                .is_ok_and(|f| f.kind == SubWorldSyncKind::FullSpawn)));
        let input = SubWorldHelmInput {
            subworld_id: id,
            throttle: 1.0,
            pitch: 0.25,
            yaw: -0.2,
            roll: 0.1,
            buoyancy_delta: 1.0,
            disassemble: false,
        };
        assert!(aeronautics_bridge_apply_helm_input(91, &input).is_err());
        aeronautics_bridge_assign_helm_seat(91, id, 200).unwrap();
        aeronautics_bridge_apply_helm_input(91, &input).unwrap();
        for _ in 0..20 {
            let frames = aeronautics_bridge_step_and_drain_sync_frames(0.05);
            assert!(frames
                .iter()
                .any(|(_, bytes)| SubWorldSyncFrame::decode(bytes)
                    .is_ok_and(|f| f.kind == SubWorldSyncKind::PoseDelta)));
        }
        {
            let bridge = adb_lock();
            let ship = bridge
                .airships
                .get(&id)
                .expect("live ship remains registered");
            assert!(
                ship.pos[1] > 100.0,
                "helium control should increase altitude: {:?}",
                ship.pos
            );
            assert!(
                ship.vel[0] > 0.0,
                "throttle should increase forward velocity: {:?}",
                ship.vel
            );
            assert!(
                ship.omega.iter().any(|v| v.abs() > 0.0),
                "steering should rotate the ship"
            );
            assert_eq!(
                ship.parent_dim, 2,
                "configured orbital ceiling should transfer the ship"
            );
        }
        let despawn = SubWorldHelmInput {
            disassemble: true,
            throttle: 0.0,
            pitch: 0.0,
            yaw: 0.0,
            roll: 0.0,
            buoyancy_delta: 0.0,
            ..input
        };
        // The orbital transfer invalidated the original dimension-bound lease;
        // re-seat the pilot in the destination dimension before permitting
        // the destructive disassembly command.
        aeronautics_bridge_assign_helm_seat(91, id, 200).unwrap();
        assert!(aeronautics_bridge_permit_ship_disassembly(91, id));
        aeronautics_bridge_apply_helm_input(91, &despawn).unwrap();
        let frames = aeronautics_bridge_step_and_drain_sync_frames(0.05);
        assert!(frames
            .iter()
            .any(|(_, bytes)| SubWorldSyncFrame::decode(bytes)
                .is_ok_and(|f| f.kind == SubWorldSyncKind::Despawn)));
    }

    #[test]
    fn assembly_pos_is_world_com_so_voxels_stay_cell_exact_on_asymmetric_hulls() {
        let _serial = ADB_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        *adb_lock() = AeronauticsRuntimeBridge::new();
        // Heavily asymmetric hull: a dense obsidian keel four cells out on
        // +x, a wool fin on +y — the mass-weighted COM sits far off the
        // origin cell.
        let blocks = [
            ([10, 100, 10], 1u32, "minecraft:stone"),
            ([11, 100, 10], 2u32, "minecraft:stone"),
            ([14, 100, 10], 3u32, "minecraft:obsidian"),
            ([10, 101, 10], 4u32, "minecraft:white_wool"),
        ];
        let origin = [10, 100, 10];
        let id = aeronautics_bridge_assemble_ship(DimId::Overworld, origin, &blocks).unwrap();
        let bridge = adb_lock();
        let sub = bridge.airships.get(&id).expect("ship registered");
        // Every voxel's world-space cell center must equal its source
        // world cell to <1e-12 — the bug this guards against placed `pos`
        // at `origin` instead of `origin + com`, displacing the whole ship.
        for (world, _state, _name) in &blocks {
            let local = [
                world[0] - origin[0],
                world[1] - origin[1],
                world[2] - origin[2],
            ];
            let w = sub.local_to_world([
                local[0] as f64 + 0.5,
                local[1] as f64 + 0.5,
                local[2] as f64 + 0.5,
            ]);
            for i in 0..3 {
                assert!(
                    (w[i] - (world[i] as f64 + 0.5)).abs() < 1e-12,
                    "voxel {world:?} displaced on assembly: {w:?}"
                );
            }
        }
        // Sanity: the COM is genuinely off-origin (the test isn't trivially
        // passing because com ≈ [0.5; 3]).
        assert!(
            (sub.center_of_mass[0] - 0.5).abs() > 0.5,
            "expected an off-origin COM, got {:?}",
            sub.center_of_mass
        );
    }

    fn audit_cells(pos: [i32; 3], block_entity: bool) -> Vec<ShipAssemblyCell> {
        vec![
            ShipAssemblyCell {
                pos,
                state_id: 1,
                block_id: "aeronautics:andesite_propeller".to_string(),
                block_entity: block_entity.then(|| b"chest-contents".to_vec()),
            },
            ShipAssemblyCell {
                pos: [pos[0] + 1, pos[1], pos[2]],
                state_id: 2,
                block_id: "aeronautics:red_envelope".to_string(),
                block_entity: None,
            },
        ]
    }

    #[test]
    fn helm_lease_identity_expiry_disconnect_and_disassembly_permission() {
        let mut bridge = AeronauticsRuntimeBridge::new();
        bridge
            .assemble_ship_atomic(42, 0, [0, 64, 0], &audit_cells([0, 64, 0], false), None, 100.0)
            .unwrap();
        let input = SubWorldHelmInput {
            subworld_id: 42,
            throttle: 1.0,
            pitch: 0.0,
            yaw: 0.0,
            roll: 0.0,
            buoyancy_delta: 0.0,
            disassemble: false,
        };
        assert!(bridge.validate_helm_input(91, &input).is_err());
        bridge.assign_helm_seat(91, 42, 10).unwrap();
        bridge.validate_helm_input(91, &input).unwrap();
        // Destructive input is separately permission-gated even for the
        // authorized seated pilot.
        let mut destructive = input;
        destructive.disassemble = true;
        assert!(bridge.validate_helm_input(91, &destructive).is_err());
        bridge.permit_ship_disassembly(91, 42);
        bridge.validate_helm_input(91, &destructive).unwrap();

        // Latch thrust, then expire the lease: validation fails and the ship
        // is neutralized instead of continuing under stale input.
        bridge
            .airships
            .get_mut(&42)
            .unwrap()
            .apply_helm_input(&input)
            .unwrap();
        bridge.server_tick = 10;
        assert!(bridge.validate_helm_input(91, &input).is_err());
        assert!(bridge.helm_leases.is_empty());
        bridge.step(0.05, 9.81, SEA_LEVEL_AIR_DENSITY);
        assert_eq!(
            bridge.airships[&42].vel[0], 0.0,
            "expiry must neutralize latched throttle before the next step"
        );

        // Disconnect cleanup has the same revoke-and-neutralize behavior.
        bridge.assign_helm_seat(91, 42, 100).unwrap();
        bridge
            .airships
            .get_mut(&42)
            .unwrap()
            .apply_helm_input(&input)
            .unwrap();
        assert!(bridge.detach_helm_player(91));
        assert!(!bridge.helm_leases.contains_key(&91));
        bridge.airships.get_mut(&42).unwrap().vel = [0.0; 3];
        bridge.step(0.05, 9.81, SEA_LEVEL_AIR_DENSITY);
        assert_eq!(
            bridge.airships[&42].vel[0], 0.0,
            "disconnect must neutralize latched throttle before the next step"
        );
    }

    #[test]
    fn atomic_ship_transfer_preserves_block_entities_and_rolls_back_blockage() {
        let mut bridge = AeronauticsRuntimeBridge::new();
        let cells = audit_cells([10, 64, 10], true);
        bridge
            .assemble_ship_atomic(77, 0, [10, 64, 10], &cells, None, 100.0)
            .unwrap();
        assert!(bridge.attach_rider(501, 77, [0.0, 1.0, 0.0], true));
        bridge.assign_helm_seat(501, 77, 100).unwrap();
        assert_eq!(
            bridge.ship_block_entities[&77].get(&[0, 0, 0]).map(Vec::as_slice),
            Some(b"chest-contents".as_slice())
        );

        // Total preflight failure mutates nothing: ship, rider, lease, and
        // payload all remain owned by the subworld for a clean retry.
        let err = bridge.disassemble_ship_atomic(77, |_, _| false).unwrap_err();
        assert!(err.contains("obstructed") || err.contains("protected"));
        assert!(bridge.airships.contains_key(&77));
        assert!(bridge.passengers.contains_key(&501));
        assert!(bridge.helm_leases.contains_key(&501));
        assert!(bridge.ship_block_entities[&77].contains_key(&[0, 0, 0]));

        let plan = bridge
            .disassemble_ship_atomic(77, |_, _| true)
            .expect("all targets free");
        assert_eq!(plan.ship_id, 77);
        assert_eq!(plan.detached_riders, [501]);
        assert_eq!(plan.writes.len(), 2);
        assert!(plan.writes.iter().any(|write| {
            write.block_id == "aeronautics:andesite_propeller"
                && write.block_entity.as_deref() == Some(b"chest-contents".as_slice())
        }));
        assert!(!bridge.airships.contains_key(&77));
        assert!(!bridge.passengers.contains_key(&501));
        assert!(bridge.helm_leases.is_empty());
        assert!(!bridge.ship_block_entities.contains_key(&77));
    }

    #[test]
    fn late_join_baseline_is_revisioned_full_voxel_sw6d_for_dimension() {
        let mut bridge = AeronauticsRuntimeBridge::new();
        bridge
            .assemble_ship_atomic(88, 7, [0, 64, 0], &audit_cells([0, 64, 0], false), None, 100.0)
            .unwrap();
        let frames = bridge.client_join_baseline(12, 7);
        assert_eq!(frames.len(), 1);
        let frame = &frames[0];
        assert_eq!(&frame[..4], b"SW6D");
        assert_eq!(frame[4], ADB_SW6D_VERSION);
        assert_eq!(frame[5], ADB_SW6D_MODE_BASELINE);
        assert_eq!(u16::from_be_bytes([frame[6], frame[7]]), 1);
        assert_eq!(u64::from_be_bytes(frame[8..16].try_into().unwrap()), 88);
        assert_eq!(u32::from_be_bytes(frame[16..20].try_into().unwrap()), 7);
        assert!(frame.len() > 8 + 93, "baseline includes full voxel payload");
        // Wrong-dimension clients receive no stale baseline from this fleet.
        assert!(bridge.client_join_baseline(13, 8).is_empty());
    }

}
