// Live root integration bridge to the standalone mod-adapter crates.
//
// `include!`d into `main.rs` unconditionally, in the same flat root-scope
// style as `mod_offload_bridge.rs`, so the layers module graph is untouched.
// Every item below that needs an adapter crate is `#[cfg(feature =
// "mod-suite")]`-gated; without the feature only `#[inline(always)]` no-op
// stubs compile, so default builds keep an unchanged dependency graph and
// byte-identical wire behavior.
//
// Surfaces wired under `--features mod-suite`:
//   * `live_mod_suite_boot` — startup `mods/` scan over `MC_SPINE_MODS_DIR`
//     (set in `main`, before the listener binds): every
//     `BUILTIN_SUPERSEDED_PERF_MODS` entry is skipped before any bytecode
//     work, every remaining JAR feeds the generic culinary auto-discovery
//     (Farmer's Delight + addon `*:cooking` / `*:fermenting` / `*:cutting`
//     recipes, item/block tags, `*:add_item` / `*:add_table` loot modifiers,
//     food specs), and the pure-Rust `.class` Mixin scanner promotes mods
//     whose only Tier 3 blockers are fully liftable `@Inject` sites to the
//     bounded Tier 2 shim while unliftable mods stay Tier 3 with the exact
//     offending site named.
//   * `live_mod_appleskin_sync` — per-tick `appleskin:exhaustion_sync` /
//     `appleskin:saturation_sync` `CustomPacketPayload` emit on the play
//     S2C `0x19` id (verified against `proto/src/data_v767.rs`'s
//     `(Play, Clientbound, "custom_payload", 0x19)` entry), delta-suppressed
//     per player at the adapter's `APPLESKIN_DELTA_EPSILON`.
//   * `live_check_progression_barrier` + `live_progression_denies_break` —
//     declarative dimension/biome progression gates (Twilight-Forest-style)
//     evaluated against a player's completed advancements; armed rules are
//     loaded from `MC_SPINE_PROGRESSION` JSON, and `live_progression_grant`
//     covers advancement ids the vanilla tree cannot represent.
//   * `live_mod_sync_on_join` — serves the `mc-spine:mod_sync_v1` (`MSY1`)
//     `CustomPacketPayload` to joining clients when an operator manifest
//     (`MC_SPINE_MOD_SYNC` JSON, `MC_SPINE_MOD_SYNC_MIRRORS` allowlist)
//     validates under `mod_sync`'s structural + trusted-CDN rules.

/// Protocol 767 Play-phase clientbound Custom Payload packet id — the
/// verified `proto/src/data_v767.rs` entry `(Play, Clientbound,
/// "custom_payload", 0x19)`. Kept as a named constant so every emit site
/// reads the contract instead of a bare literal.
#[allow(dead_code)]
const LIVE_MOD_S2C_CUSTOM_PAYLOAD_ID: i64 = 0x19;

/// Feature-independent mirror of
/// `mod_adapter_core::world_extensions::ProgressionVerdict`, so
/// `live_check_progression_barrier` keeps one signature (and one return
/// type) with or without `mod-suite`.
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq)]
enum LiveProgressionVerdict {
    /// No locked rule matched — the player may act freely.
    Allowed,
    /// A rule's advancement predicate is unmet; the area stays sealed.
    Locked {
        rule_id: String,
        missing_advancements: Vec<String>,
        block_break_blocked: bool,
        block_place_blocked: bool,
        effects: Vec<(String, u32, u8)>,
        periodic_damage: Option<(f32, u32)>,
    },
}

// ===========================================================================
// mod-suite implementation
// ===========================================================================

#[cfg(feature = "mod-suite")]
use mod_adapter_core::bytecode_subset_vm::{
    execute_verified_method, parse_class_methods, verify_subset_method, HostCallKind, SubsetVmHost,
    VerifiedMethod, VmError, VmValue, DEFAULT_FUEL,
};
#[cfg(feature = "mod-suite")]
use mod_adapter_core::class_mixin_scanner::{scan_jar_mixins_with_options, JarScanOptions};
#[cfg(feature = "mod-suite")]
use mod_adapter_core::culinary_adapter::{
    CookResult, CookingPotState, CulinaryDiscoveryStats, CulinaryRegistry, CuttingOutcome,
};
#[cfg(feature = "mod-suite")]
use mod_adapter_core::ecosystem_compat::{
    plan_dual_loader_mods_dir, AccessoryError, AccessoryInventory, AccessoryItem, AttributeTotals,
    DualLoaderPlan,
};
#[cfg(feature = "mod-suite")]
use mod_adapter_core::fallback::{
    classify_mod_with_mixins, is_builtin_superseded_mod, DiscoveredMod, ExecutionTier,
    REASON_SUPERSEDED,
};
#[cfg(feature = "mod-suite")]
use mod_adapter_core::mod_sync as lmb_sync;
#[cfg(feature = "mod-suite")]
use mod_adapter_core::mixin_lifter::{lift_mixin_sites, LiftedBinding, MixinLiftVerdict};
#[cfg(feature = "mod-suite")]
use mod_adapter_core::unified_bundle::{DeclarativeModBundle, UnifiedModRuntime};
#[cfg(feature = "mod-suite")]
use mod_adapter_core::modpack::infer_candidate_mod_id;
#[cfg(feature = "mod-suite")]
use mod_adapter_core::profile as lmb_profile;
#[cfg(feature = "mod-suite")]
use mod_adapter_core::version_compat::McVersion;
#[cfg(feature = "mod-suite")]
use mod_adapter_core::world_extensions::{
    AppleSkinDeltaTracker, ProgressionBarrierEngine, ProgressionRule, ProgressionVerdict,
    CHANNEL_APPLESKIN_EXHAUSTION, CHANNEL_APPLESKIN_SATURATION,
};

// The bridge speaks protocol 767 (Minecraft 1.21.1) exclusively — the release
// every adapter target pins. `mc_spine_core`'s shared version table is the
// source of truth; if its protocol ever drifts from 767 this fails to compile
// instead of silently serving a wrong-version manifest or QoL payload.
#[cfg(feature = "mod-suite")]
const _: () = assert!(
    mc_spine_core::version::TargetVersion::V1_21_1.protocol() == 767,
    "live mod bridge: protocol table drifted from 767"
);
#[cfg(feature = "mod-suite")]
const LIVE_MOD_MC_TARGET: McVersion = McVersion::V1_21_1;

/// Mod ids the flagship `create-adapter` handles natively for this feature.
#[cfg(feature = "mod-suite")]
fn live_mod_flagship_ids() -> BTreeSet<String> {
    BTreeSet::from(["create".to_string()])
}

/// Audit record for one scanned mod JAR: what the classifier decided and the
/// plain-language diagnostic the startup report prints.
#[cfg(feature = "mod-suite")]
#[derive(Clone, Debug)]
struct LiveModJarReport {
    file_name: String,
    mod_id: String,
    tier: ExecutionTier,
    diagnostic: String,
    classes_scanned: usize,
    mixin_sites: usize,
    skipped_client_mixins: usize,
}

/// What was automatically onboarded from a JAR. Lifted bindings are installed
/// only for a fully verified Tier 2 classification; Tier 3 is diagnostic.
#[cfg(feature = "mod-suite")]
#[allow(dead_code)]
#[derive(Clone, Debug)]
struct AutoAdaptedModSummary {
    file_name: String,
    mod_id: String,
    tier: ExecutionTier,
    lifted_hooks: Vec<LiftedBinding>,
    cooking_recipes: usize,
    cutting_recipes: usize,
    tag_files: usize,
    tag_values: usize,
}

/// Everything the live mod suite owns at runtime. `active` stays false until
/// `live_mod_suite_boot` (or a test) arms a surface — an unarmed suite is a
/// zero-cost no-op for every emitter below.
#[cfg(feature = "mod-suite")]
struct LiveModSuite {
    /// `true` once boot ran: QoL emitters and the dig gate consult it before
    /// doing any work.
    active: bool,
    /// Per-JAR classification audit, in sorted file order.
    reports: Vec<LiveModJarReport>,
    /// Validated native bundle registry, shared with the live boot surfaces.
    unified: UnifiedModRuntime,
    /// Automatic JAR adaptation receipts, including Tier 3 diagnostics.
    auto_adapted: Vec<AutoAdaptedModSummary>,
    /// Verified Tier 2 event bindings, keyed by the source mod id.
    lifted_hooks: Vec<(String, LiftedBinding)>,
    /// Validated merged Fabric + NeoForge server-side manifest table.
    dual_loader: DualLoaderPlan,
    /// Methods verified against the restricted JVM opcode and host-call sets.
    damage_hooks: Vec<VerifiedMethod>,
    /// Merged Farmer's Delight + addon culinary content across all loaded
    /// JARs (cooking/cutting recipes, tags, loot modifiers, food specs).
    culinary: CulinaryRegistry,
    /// Aggregated discovery counters over `culinary` (for the boot report).
    culinary_stats: CulinaryDiscoveryStats,
    /// Live pots are keyed by dimension and block position.
    cooking_pots: HashMap<(DimId, i32, i32, i32), CookingPotState>,
    /// Only active completion deadlines enter the scheduler. Idle pots have
    /// no entries and therefore cost no work during ticks.
    cooking_deadlines: std::collections::BTreeSet<(u64, DimId, i32, i32, i32)>,
    /// Independent Curios/Trinkets-compatible inventories for each player.
    accessories: HashMap<String, AccessoryInventory>,
    /// Declarative dimension/biome progression gates; empty = no gating.
    barriers: ProgressionBarrierEngine,
    /// `(player, advancement)` grants for ids the vanilla advancement tree
    /// cannot represent — modded progression credited by a Tier 2 shim event
    /// hook or an operator. Merged into every per-player barrier check.
    extra_advancements: BTreeSet<(String, String)>,
    /// Per-eid AppleSkin delta suppressors (keyed like every other per-player
    /// runtime map).
    appleskin: HashMap<i32, AppleSkinDeltaTracker>,
    /// Encoded `MSY1` body for `mc-spine:mod_sync_v1`, validated at arm time.
    mod_sync_body: Option<Vec<u8>>,
    mod_sync_root_hash: Option<[u8; 32]>,
}

#[cfg(feature = "mod-suite")]
impl LiveModSuite {
    fn new() -> Self {
        Self {
            active: false,
            reports: Vec::new(),
            unified: UnifiedModRuntime::default(),
            auto_adapted: Vec::new(),
            lifted_hooks: Vec::new(),
            dual_loader: DualLoaderPlan::default(),
            damage_hooks: Vec::new(),
            culinary: CulinaryRegistry::new(),
            culinary_stats: CulinaryDiscoveryStats::default(),
            cooking_pots: HashMap::new(),
            cooking_deadlines: std::collections::BTreeSet::new(),
            accessories: HashMap::new(),
            barriers: ProgressionBarrierEngine::new(Vec::new()),
            extra_advancements: BTreeSet::new(),
            appleskin: HashMap::new(),
            mod_sync_body: None,
            mod_sync_root_hash: None,
        }
    }

    /// `*.jar` members of `dir`, in sorted basename order (deterministic scan
    /// and report order).
    fn jar_listing(dir: &std::path::Path) -> Vec<(String, std::path::PathBuf)> {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut jars: Vec<(String, std::path::PathBuf)> = rd
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_type().map(|t| t.is_file()).unwrap_or(false))
            .filter_map(|entry| {
                let name = entry.file_name().to_str()?.to_string();
                (name.len() > 4 && name[name.len() - 4..].eq_ignore_ascii_case(".jar"))
                    .then_some((name, entry.path()))
            })
            .collect();
        jars.sort();
        jars
    }

    /// Scan one `mods/` directory: superseded perf mods are skipped before
    /// any bytecode work; every remaining JAR feeds the generic culinary
    /// registry and the `.class` Mixin scanner, which promotes fully
    /// liftable `@Inject` mods Tier 3 → bounded Tier 2 and reports the rest
    /// as Tier 3 with exact diagnostics.
    fn scan_mods_dir(&mut self, dir: &std::path::Path) {
        if !dir.is_dir() {
            eprintln!(
                "[live-mods] MC_SPINE_MODS_DIR {} is not a directory — mod scan skipped",
                dir.display()
            );
            return;
        }
        self.scan_bundles_dir(dir);
        let jars = Self::jar_listing(dir);
        let mut bounded_jars = Vec::new();
        for (file_name, path) in &jars {
            self.scan_jar(file_name, path);
            if let Ok(metadata) = std::fs::metadata(path) {
                if metadata.len() <= lmb_sync::MAX_MOD_JAR_BYTES {
                    if let Ok(bytes) = std::fs::read(path) {
                        bounded_jars.push((file_name.clone(), bytes));
                    }
                }
            }
        }
        let plan = plan_dual_loader_mods_dir(
            bounded_jars
                .iter()
                .map(|(name, bytes)| (name.as_str(), bytes.as_slice())),
        );
        let accepted: BTreeSet<&str> = plan
            .unified_mods
            .iter()
            .map(|entry| entry.jar_name.as_str())
            .collect();
        for (name, bytes) in &bounded_jars {
            if accepted.contains(name.as_str()) {
                self.load_damage_hooks(name, bytes);
            }
        }
        eprintln!(
            "[live-mods] dual-loader: {} unified, {} client-only, {} superseded, {} rejected, {} duplicate ids, {} missing deps, {} verified damage hooks",
            plan.unified_mods.len(),
            plan.skipped_client_only.len(),
            plan.skipped_superseded.len(),
            plan.rejected_jars.len(),
            plan.duplicate_conflicts.len(),
            plan.missing_dependencies.len(),
            self.damage_hooks.len(),
        );
        for rejected in &plan.rejected_jars {
            eprintln!(
                "[live-mods] rejected {}: {}",
                rejected.jar_name, rejected.reason
            );
        }
        for conflict in &plan.duplicate_conflicts {
            eprintln!(
                "[live-mods] duplicate id {}: {:?}",
                conflict.mod_id, conflict.jar_names
            );
        }
        for missing in &plan.missing_dependencies {
            eprintln!(
                "[live-mods] {} requires missing {} ({})",
                missing.mod_id,
                missing.required_mod_id,
                missing.version_range.as_deref().unwrap_or("unspecified"),
            );
        }
        self.dual_loader = plan;
        self.log_boot_report(jars.len());
    }

    /// Load validated declarative bundles from mods/ and mods/adapted/.
    /// Invalid, incompatible and duplicate bundle ids have no runtime effect.
    fn scan_bundles_dir(&mut self, dir: &std::path::Path) {
        const MAX_BUNDLE_BYTES: u64 = 4 * 1024 * 1024;
        let mut paths = Vec::new();
        for root in [dir.to_path_buf(), dir.join("adapted")] {
            let Ok(entries) = std::fs::read_dir(root) else {
                continue;
            };
            let mut root_paths = Vec::new();
            for entry in entries.flatten() {
                let path = entry.path();
                if entry.file_type().is_ok_and(|ty| ty.is_file())
                    && path.file_name().and_then(|n| n.to_str()).is_some_and(|name| name.ends_with(".bundle.json"))
                {
                    root_paths.push(path);
                }
            }
            root_paths.sort();
            paths.extend(root_paths);
        }
        let mut installed = BTreeSet::new();
        for path in paths {
            let result = (|| {
                let metadata = std::fs::metadata(&path).map_err(|e| e.to_string())?;
                if metadata.len() > MAX_BUNDLE_BYTES {
                    return Err("bundle exceeds 4 MiB limit".to_string());
                }
                let json = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
                let bundle = DeclarativeModBundle::from_json(&json).map_err(|e| e.to_string())?;
                if installed.contains(&bundle.mod_id) {
                    return Err(format!("duplicate bundle mod id {}", bundle.mod_id));
                }
                let receipt = self.unified.register_bundle(&bundle, LIVE_MOD_MC_TARGET)
                    .map_err(|e| e.to_string())?;
                installed.insert(bundle.mod_id);
                Ok(receipt)
            })();
            match result {
                Ok(receipt) => eprintln!(
                    "[live-mods] auto-adapted native bundle {}: {} portals, {} progression barriers, {} regions, {} creatures, {} channels",
                    receipt.mod_id, receipt.portals_installed, receipt.progression_barriers_installed,
                    receipt.biome_regions_installed, receipt.creatures_installed,
                    receipt.custom_channels_declared,
                ),
                Err(error) => eprintln!("[live-mods] bundle {} rejected: {error}", path.display()),
            }
        }
        self.barriers = self.unified.progression_engine();
    }

    /// Classify + data-load one JAR file.
    fn scan_jar(&mut self, file_name: &str, path: &std::path::Path) {
        let mod_id = infer_candidate_mod_id(file_name)
            .unwrap_or_else(|| file_name[..file_name.len() - 4].to_lowercase());
        // Superseded perf mods never reach the scanner or the loader: their
        // entire function already runs natively inside mc-spine.
        if is_builtin_superseded_mod(&mod_id) {
            self.reports.push(LiveModJarReport {
                file_name: file_name.to_string(),
                mod_id,
                tier: ExecutionTier::BuiltInSuperseded,
                diagnostic: REASON_SUPERSEDED.to_string(),
                classes_scanned: 0,
                mixin_sites: 0,
                skipped_client_mixins: 0,
            });
            return;
        }
        let meta = match std::fs::metadata(path) {
            Ok(meta) => meta,
            Err(error) => {
                eprintln!("[live-mods] {file_name}: cannot stat JAR: {error}");
                return;
            }
        };
        if meta.len() > lmb_sync::MAX_MOD_JAR_BYTES {
            eprintln!(
                "[live-mods] {file_name}: {} bytes exceeds the {}-byte JAR bound — skipped",
                meta.len(),
                lmb_sync::MAX_MOD_JAR_BYTES
            );
            return;
        }
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) => {
                eprintln!("[live-mods] {file_name}: cannot read JAR: {error}");
                return;
            }
        };
        // Generic culinary auto-discovery: only jars actually carrying
        // `data/<ns>/{recipes,tags,loot_modifiers,food}` contribute, so this
        // is safe to feed every content mod, not just Farmer's Delight.
        let mut discovered = CulinaryDiscoveryStats::default();
        match self.culinary.load_from_jar_bytes(&bytes) {
            Ok(stats) => {
                discovered = stats.clone();
                let agg = &mut self.culinary_stats;
                agg.jar_members += stats.jar_members;
                agg.data_files += stats.data_files;
                agg.cooking_recipes += stats.cooking_recipes;
                agg.cutting_recipes += stats.cutting_recipes;
                agg.skipped_recipes += stats.skipped_recipes;
                agg.tag_files += stats.tag_files;
                agg.tag_values += stats.tag_values;
                agg.loot_modifiers += stats.loot_modifiers;
                agg.food_specs += stats.food_specs;
            }
            Err(error) => {
                eprintln!("[live-mods] {file_name}: culinary scan skipped: {error:?}");
            }
        }
        let (tier, diagnostic, classes_scanned, mixin_sites, skipped_client_mixins) =
            live_mod_classify(&mod_id, &bytes);
        let lifted_hooks = if tier == ExecutionTier::BoundedJvmShim {
            match scan_jar_mixins_with_options(&bytes, JarScanOptions::default()) {
                Ok(scan) => match lift_mixin_sites(&scan.sites) {
                    MixinLiftVerdict::FullyLiftable(hooks) if !hooks.is_empty() => hooks,
                    _ => Vec::new(),
                },
                Err(_) => Vec::new(),
            }
        } else {
            Vec::new()
        };
        self.lifted_hooks.extend(
            lifted_hooks.iter().cloned().map(|hook| (mod_id.clone(), hook))
        );
        self.auto_adapted.push(AutoAdaptedModSummary {
            file_name: file_name.to_string(),
            mod_id: mod_id.clone(),
            tier,
            lifted_hooks,
            cooking_recipes: discovered.cooking_recipes,
            cutting_recipes: discovered.cutting_recipes,
            tag_files: discovered.tag_files,
            tag_values: discovered.tag_values,
        });
        self.reports.push(LiveModJarReport {
            file_name: file_name.to_string(),
            mod_id,
            tier,
            diagnostic,
            classes_scanned,
            mixin_sites,
            skipped_client_mixins,
        });
    }

    /// Install only an explicitly named damage entrypoint after full static
    /// verification. The archive scanner enforces ZIP and decoded-size bounds.
    fn load_damage_hooks(&mut self, jar_name: &str, jar: &[u8]) {
        const DAMAGE_DESCRIPTOR: &str = "(Lmcspine/api/Event;)V";
        let classes = match live_mod_class_members(jar) {
            Ok(classes) => classes,
            Err(reason) => {
                eprintln!("[live-mods] {jar_name}: class scan rejected: {reason}");
                return;
            }
        };
        for (class_name, bytes) in classes {
            if self.damage_hooks.len() >= 128 {
                eprintln!("[live-mods] damage hook count bound reached ({jar_name})");
                return;
            }
            let Ok(parsed) = parse_class_methods(&bytes) else {
                continue;
            };
            if !parsed
                .methods
                .iter()
                .any(|m| m.name == "onDamage" && m.descriptor == DAMAGE_DESCRIPTOR)
            {
                continue;
            }
            match verify_subset_method(&parsed, "onDamage", DAMAGE_DESCRIPTOR) {
                Ok(method) => self.damage_hooks.push(method),
                Err(error) => {
                    eprintln!("[live-mods] {jar_name} {class_name}: damage hook rejected: {error}")
                }
            }
        }
    }

    /// Replace the state of a pot after an inventory/heat/world interaction.
    /// Inserting a sleeping pot adds no deadline to the tick scheduler.
    fn set_cooking_pot(
        &mut self,
        key: (DimId, i32, i32, i32),
        mut pot: CookingPotState,
        tick: u64,
    ) {
        if let Some(previous) = self.cooking_pots.get(&key) {
            if let Some(deadline) = previous.next_completion_tick {
                self.cooking_deadlines
                    .remove(&(deadline, key.0, key.1, key.2, key.3));
            }
        }
        pot.refresh(&self.culinary, tick);
        if let Some(deadline) = pot.next_completion_tick {
            self.cooking_deadlines
                .insert((deadline, key.0, key.1, key.2, key.3));
        }
        self.cooking_pots.insert(key, pot);
    }

    fn tick_cooking_pots(&mut self, tick: u64) -> Vec<((DimId, i32, i32, i32), CookResult)> {
        let mut completed = Vec::new();
        // With no active pots this checks one empty BTreeSet and returns.
        while let Some(&(deadline, dim, x, y, z)) = self.cooking_deadlines.first() {
            if deadline > tick {
                break;
            }
            self.cooking_deadlines.pop_first();
            let key = (dim, x, y, z);
            let Some(pot) = self.cooking_pots.get_mut(&key) else {
                continue;
            };
            if pot.next_completion_tick != Some(deadline) {
                continue;
            }
            if let Some(result) = pot.advance_to(tick, &self.culinary) {
                completed.push((key, result));
            }
            if let Some(next) = pot.next_completion_tick {
                self.cooking_deadlines.insert((next, dim, x, y, z));
            }
        }
        completed
    }

    /// Startup report: one line per tier group, then per-mod detail for
    /// anything needing operator attention (lifted and unliftable alike).
    fn log_boot_report(&self, jars_seen: usize) {
        if self.reports.is_empty() {
            eprintln!("[live-mods] mods dir scanned: {jars_seen} jar(s), none classified");
            return;
        }
        let count = |tier: ExecutionTier| self.reports.iter().filter(|r| r.tier == tier).count();
        eprintln!(
            "[live-mods] {jars_seen} jar(s): {} superseded, {} native, {} data-only, {} bounded-shim, {} tier3",
            count(ExecutionTier::BuiltInSuperseded),
            count(ExecutionTier::NativeFlagship),
            count(ExecutionTier::UniversalNative),
            count(ExecutionTier::BoundedJvmShim),
            count(ExecutionTier::RequiresFallback),
        );
        let stats = &self.culinary_stats;
        if stats.cooking_recipes + stats.cutting_recipes + stats.loot_modifiers + stats.food_specs
            > 0
        {
            eprintln!(
                "[live-mods] culinary: {} cooking, {} cutting, {} loot modifiers, {} food specs, {} tag values",
                stats.cooking_recipes,
                stats.cutting_recipes,
                stats.loot_modifiers,
                stats.food_specs,
                stats.tag_values,
            );
        }
        for report in &self.reports {
            let skipped = if report.skipped_client_mixins > 0 {
                format!(
                    ", {} client-only mixin(s) skipped",
                    report.skipped_client_mixins
                )
            } else {
                String::new()
            };
            match report.tier {
                ExecutionTier::RequiresFallback => eprintln!(
                    "[live-mods] {} ({}) tier3: {} classes, {} site(s){} — {}",
                    report.file_name,
                    report.mod_id,
                    report.classes_scanned,
                    report.mixin_sites,
                    skipped,
                    report.diagnostic
                ),
                ExecutionTier::BoundedJvmShim => eprintln!(
                    "[live-mods] {} ({}) bounded shim: {} classes, {} site(s){} — {}",
                    report.file_name,
                    report.mod_id,
                    report.classes_scanned,
                    report.mixin_sites,
                    skipped,
                    report.diagnostic
                ),
                _ => {}
            }
        }
    }

    /// Arm the `mc-spine:mod_sync_v1` manifest from an operator JSON file:
    /// `{"entries":[{"mod_id","file_name","download_url","sha256_hex",
    /// "size_bytes"}]}`. Every entry must pass `mod_sync`'s structural +
    /// trusted-CDN validation (plus `MC_SPINE_MOD_SYNC_MIRRORS` hosts) before
    /// the encoded `MSY1` body is installed — an invalid manifest arms
    /// nothing.
    fn arm_mod_sync_json(&mut self, path: &std::path::Path) {
        let result = std::fs::read(path)
            .map_err(|error| format!("cannot read: {error}"))
            .and_then(|bytes| live_mod_sync_manifest_from_json(&bytes));
        let manifest = match result {
            Ok(manifest) => manifest,
            Err(error) => {
                eprintln!("[live-mods] mod sync manifest {}: {error}", path.display());
                return;
            }
        };
        let policy = match std::env::var("MC_SPINE_MOD_SYNC_MIRRORS") {
            Ok(spec) => {
                let mirrors: Vec<&str> = spec
                    .split(',')
                    .map(str::trim)
                    .filter(|h| !h.is_empty())
                    .collect();
                match lmb_sync::SyncPolicy::with_operator_mirrors(&mirrors) {
                    Ok(policy) => policy,
                    Err(error) => {
                        eprintln!("[live-mods] mod sync mirrors rejected: {}", error.reason());
                        return;
                    }
                }
            }
            Err(_) => lmb_sync::SyncPolicy::canonical(),
        };
        for entry in &manifest.entries {
            if let Err(error) = lmb_sync::validate_sync_entry(entry, &policy) {
                eprintln!(
                    "[live-mods] mod sync entry '{}' rejected: {}",
                    entry.mod_id,
                    error.reason()
                );
                return;
            }
        }
        match lmb_sync::encode_sync_manifest(&manifest) {
            Ok(body) => {
                eprintln!(
                    "[live-mods] mod sync manifest armed: {} entr{}",
                    manifest.entries.len(),
                    if manifest.entries.len() == 1 {
                        "y"
                    } else {
                        "ies"
                    }
                );
                self.mod_sync_body = Some(body);
                self.mod_sync_root_hash = Some(lmb_sync::manifest_root_hash(&manifest));
            }
            Err(error) => {
                eprintln!(
                    "[live-mods] mod sync manifest encode failed: {}",
                    error.reason()
                );
            }
        }
    }

    /// Load declarative progression rules from `MC_SPINE_PROGRESSION` JSON:
    /// `{"rules":[{"id","dimension","biomes":[..],"requires":[..],
    /// "block_break":bool,"block_place":bool,"effects":[[id,ticks,amp]],
    /// "damage":[hearts,interval_ticks]}]}`.
    fn load_progression_json(&mut self, path: &std::path::Path) {
        let result = std::fs::read(path)
            .map_err(|error| format!("cannot read: {error}"))
            .and_then(|bytes| live_mod_progression_rules_from_json(&bytes));
        match result {
            Ok(rules) => {
                eprintln!(
                    "[live-mods] progression barriers armed: {} rule(s)",
                    rules.len()
                );
                let mut all_rules = self.unified.progression_rules().to_vec();
                all_rules.extend(rules);
                self.barriers = ProgressionBarrierEngine::new(all_rules);
            }
            Err(error) => {
                eprintln!("[live-mods] progression rules {}: {error}", path.display());
            }
        }
    }
}

/// A bounded ZIP central-directory reader for `.class` member bytes. The
/// adapter's public manifest scanner validates the mod archive separately.
/// This reader accepts STORE and DEFLATE, checks size and CRC, and never
/// executes or loads a JVM class.
#[cfg(feature = "mod-suite")]
fn live_mod_class_members(jar: &[u8]) -> Result<Vec<(String, Vec<u8>)>, &'static str> {
    use std::io::Read;

    const MAX_CLASSES: usize = 8192;
    const MAX_CLASS_BYTES: usize = 2 * 1024 * 1024;
    const MAX_TOTAL_CLASS_BYTES: usize = 64 * 1024 * 1024;

    let u16_at = |p: usize| -> Option<u16> {
        Some(u16::from_le_bytes(
            jar.get(p..p.checked_add(2)?)?.try_into().ok()?,
        ))
    };
    let u32_at = |p: usize| -> Option<u32> {
        Some(u32::from_le_bytes(
            jar.get(p..p.checked_add(4)?)?.try_into().ok()?,
        ))
    };
    if jar.len() < 22 {
        return Err("truncated zip footer");
    }
    let eocd = (jar.len().saturating_sub(65_557)..=jar.len() - 22)
        .rev()
        .find(|&p| {
            u32_at(p) == Some(0x0605_4b50)
                && u16_at(p + 20).is_some_and(|len| p + 22 + usize::from(len) == jar.len())
        })
        .ok_or("missing zip central directory")?;
    if u16_at(eocd + 4) != Some(0) || u16_at(eocd + 6) != Some(0) {
        return Err("multi-disk zip unsupported");
    }
    let count = usize::from(u16_at(eocd + 10).ok_or("zip entry count")?);
    if count != usize::from(u16_at(eocd + 8).ok_or("zip disk entry count")?) || count > 65_000 {
        return Err("zip entry count mismatch");
    }
    let directory = usize::try_from(u32_at(eocd + 16).ok_or("zip directory offset")?)
        .map_err(|_| "zip offset overflow")?;
    let directory_len = usize::try_from(u32_at(eocd + 12).ok_or("zip directory size")?)
        .map_err(|_| "zip length overflow")?;
    if directory
        .checked_add(directory_len)
        .is_none_or(|end| end > eocd)
    {
        return Err("zip directory truncated");
    }
    let mut cursor = directory;
    let mut decoded_total = 0usize;
    let mut classes = Vec::new();
    for _ in 0..count {
        if u32_at(cursor) != Some(0x0201_4b50) {
            return Err("invalid zip central member");
        }
        let flags = u16_at(cursor + 8).ok_or("zip flags")?;
        let method = u16_at(cursor + 10).ok_or("zip method")?;
        let crc = u32_at(cursor + 16).ok_or("zip crc")?;
        let compressed = usize::try_from(u32_at(cursor + 20).ok_or("zip compressed size")?)
            .map_err(|_| "zip compressed length overflow")?;
        let expanded = usize::try_from(u32_at(cursor + 24).ok_or("zip expanded size")?)
            .map_err(|_| "zip expanded length overflow")?;
        let name_len = usize::from(u16_at(cursor + 28).ok_or("zip name size")?);
        let extra_len = usize::from(u16_at(cursor + 30).ok_or("zip extra size")?);
        let comment_len = usize::from(u16_at(cursor + 32).ok_or("zip comment size")?);
        let local = usize::try_from(u32_at(cursor + 42).ok_or("zip local offset")?)
            .map_err(|_| "zip local offset overflow")?;
        let name_start = cursor.checked_add(46).ok_or("zip header overflow")?;
        let name_end = name_start
            .checked_add(name_len)
            .ok_or("zip name overflow")?;
        let name = jar.get(name_start..name_end).ok_or("zip name truncated")?;
        cursor = name_end
            .checked_add(extra_len)
            .and_then(|p| p.checked_add(comment_len))
            .ok_or("zip member overflow")?;
        if cursor > directory + directory_len {
            return Err("zip central member truncated");
        }
        if !name.ends_with(b".class") {
            continue;
        }
        if classes.len() >= MAX_CLASSES || expanded > MAX_CLASS_BYTES {
            return Err("class count or size bound exceeded");
        }
        if flags & 1 != 0 || (method != 0 && method != 8) {
            return Err("encrypted or unsupported compressed class");
        }
        let name = std::str::from_utf8(name).map_err(|_| "non-UTF8 class name")?;
        if name.starts_with('/') || name.split('/').any(|segment| segment == "..") {
            return Err("unsafe class member name");
        }
        if u32_at(local) != Some(0x0403_4b50) {
            return Err("invalid local class header");
        }
        let payload_start = local
            .checked_add(30)
            .and_then(|p| p.checked_add(usize::from(u16_at(local + 26)?)))
            .and_then(|p| p.checked_add(usize::from(u16_at(local + 28)?)))
            .ok_or("class payload offset overflow")?;
        let payload_end = payload_start
            .checked_add(compressed)
            .ok_or("class payload overflow")?;
        let payload = jar
            .get(payload_start..payload_end)
            .ok_or("class payload truncated")?;
        let decoded = if method == 0 {
            payload.to_vec()
        } else {
            let mut decoder = flate2::read::DeflateDecoder::new(payload);
            let mut out = Vec::new();
            decoder
                .by_ref()
                .take((MAX_CLASS_BYTES + 1) as u64)
                .read_to_end(&mut out)
                .map_err(|_| "invalid class deflate stream")?;
            out
        };
        decoded_total = decoded_total
            .checked_add(decoded.len())
            .ok_or("class total size overflow")?;
        if decoded.len() != expanded || decoded_total > MAX_TOTAL_CLASS_BYTES {
            return Err("class expanded size mismatch or total bound exceeded");
        }
        let mut checksum = !0u32;
        for &byte in &decoded {
            checksum ^= u32::from(byte);
            for _ in 0..8 {
                checksum = (checksum >> 1) ^ (0xedb8_8320 & 0u32.wrapping_sub(checksum & 1));
            }
        }
        if !checksum != crc {
            return Err("class crc mismatch");
        }
        classes.push((name.to_string(), decoded));
    }
    if cursor != directory + directory_len {
        return Err("zip directory size mismatch");
    }
    Ok(classes)
}

/// The process-wide suite. Constructed empty: until `live_mod_suite_boot`
/// runs (or a test installs a suite) every surface is inert.
#[cfg(feature = "mod-suite")]
fn live_mod_suite() -> &'static Mutex<LiveModSuite> {
    static SUITE: OnceLock<Mutex<LiveModSuite>> = OnceLock::new();
    SUITE.get_or_init(|| Mutex::new(LiveModSuite::new()))
}

/// Snapshot of the automatically registered native mod bundles.
#[cfg(feature = "mod-suite")]
#[allow(dead_code)]
fn live_mod_unified_runtime() -> UnifiedModRuntime {
    live_mod_suite()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .unified
        .clone()
}

/// The only capabilities exposed to verified damage bytecode. Each method
/// receives a fresh host so that failures cannot leak partial mutations.
#[cfg(feature = "mod-suite")]
struct LiveModDamageHost {
    damage: f32,
}

#[cfg(feature = "mod-suite")]
impl SubsetVmHost for LiveModDamageHost {
    fn call(&mut self, kind: HostCallKind, args: &[VmValue]) -> Result<Option<VmValue>, VmError> {
        match (kind, args) {
            (HostCallKind::EventGetDamage, [VmValue::Ref(1)]) => {
                Ok(Some(VmValue::Float(self.damage)))
            }
            (HostCallKind::EventSetDamage, [VmValue::Ref(1), VmValue::Float(value)])
                if value.is_finite() && *value >= 0.0 =>
            {
                self.damage = *value;
                Ok(None)
            }
            _ => Err(VmError::HostError(
                "unsupported damage event host call".into(),
            )),
        }
    }
}

/// Evaluate preverified onDamage methods in deterministic scan order, with
/// an independent fuel budget and atomic damage update per invocation.
#[cfg(feature = "mod-suite")]
#[allow(dead_code)]
fn live_mod_eval_damage_hooks(initial_damage: f32) -> f32 {
    if !initial_damage.is_finite() || initial_damage < 0.0 {
        return initial_damage;
    }
    let suite = live_mod_suite().lock().unwrap_or_else(|e| e.into_inner());
    if !suite.active {
        return initial_damage;
    }
    let mut damage = initial_damage;
    for method in &suite.damage_hooks {
        let mut host = LiveModDamageHost { damage };
        match execute_verified_method(method, &[VmValue::Ref(1)], &mut host, DEFAULT_FUEL) {
            Ok(_) => damage = host.damage,
            Err(error) => eprintln!("[live-mods] damage hook execution rejected: {error}"),
        }
    }
    damage
}

/// Install an updated pot snapshot after an inventory, heat, or block event.
#[cfg(feature = "mod-suite")]
#[allow(dead_code)]
fn live_mod_set_cooking_pot(dim: DimId, x: i32, y: i32, z: i32, pot: CookingPotState, tick: u64) {
    let mut suite = live_mod_suite().lock().unwrap_or_else(|e| e.into_inner());
    if suite.active {
        suite.set_cooking_pot((dim, x, y, z), pot, tick);
    }
}

/// Remove a broken pot and its scheduled wake-up, if any.
#[cfg(feature = "mod-suite")]
#[allow(dead_code)]
fn live_mod_remove_cooking_pot(dim: DimId, x: i32, y: i32, z: i32) {
    let mut suite = live_mod_suite().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(pot) = suite.cooking_pots.remove(&(dim, x, y, z)) {
        if let Some(deadline) = pot.next_completion_tick {
            suite.cooking_deadlines.remove(&(deadline, dim, x, y, z));
        }
    }
}

/// Wake only pots whose deadline is due. Empty or unheated pots perform no
/// per-tick work, irrespective of how many are registered.
#[cfg(feature = "mod-suite")]
#[allow(dead_code)]
fn live_mod_tick_cooking_pots(tick: u64) -> Vec<((DimId, i32, i32, i32), CookResult)> {
    let mut suite = live_mod_suite().lock().unwrap_or_else(|e| e.into_inner());
    if suite.active {
        suite.tick_cooking_pots(tick)
    } else {
        Vec::new()
    }
}

/// Execute cutting recipes discovered from base and unknown-addon JARs.
#[cfg(feature = "mod-suite")]
#[allow(dead_code)]
fn live_mod_cutting_board_use(item: &str, tool: &str, seed: u64) -> Option<CuttingOutcome> {
    let suite = live_mod_suite().lock().unwrap_or_else(|e| e.into_inner());
    suite
        .active
        .then(|| suite.culinary.execute_cutting(item, tool, seed))
        .flatten()
}

/// Equip for a single player without affecting any other player's slots.
#[cfg(feature = "mod-suite")]
#[allow(dead_code)]
fn live_mod_equip_accessory(
    player: &str,
    slot_type: &str,
    index: usize,
    item: AccessoryItem,
) -> Result<(), AccessoryError> {
    let mut suite = live_mod_suite().lock().unwrap_or_else(|e| e.into_inner());
    suite
        .accessories
        .entry(player.to_string())
        .or_insert_with(AccessoryInventory::standard)
        .equip(slot_type, index, item)
}

/// Aggregate the three vanilla attribute-modifier stages for a player.
#[cfg(feature = "mod-suite")]
#[allow(dead_code)]
fn live_mod_accessory_attributes(player: &str) -> BTreeMap<String, AttributeTotals> {
    live_mod_suite()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .accessories
        .get(player)
        .map(AccessoryInventory::aggregate_attribute_modifiers)
        .unwrap_or_default()
}

/// Probe the armed manifest using the NET-3c MSH1 magic and 32-byte root.
#[cfg(feature = "mod-suite")]
#[allow(dead_code)]
fn live_mod_sync_root_hash_probe() -> Option<[u8; 36]> {
    live_mod_suite()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .mod_sync_root_hash
        .as_ref()
        .map(lmb_sync::encode_root_hash_probe)
}

/// A matching client MSH1 probe avoids a redundant MSY1 transfer. A missing
/// or invalid probe requires the full manifest for reconciliation.
#[cfg(feature = "mod-suite")]
#[allow(dead_code)]
fn live_mod_sync_manifest_for_client_probe(client_probe: Option<&[u8]>) -> Option<Vec<u8>> {
    let suite = live_mod_suite().lock().unwrap_or_else(|e| e.into_inner());
    if let (Some(expected), Some(probe)) = (suite.mod_sync_root_hash, client_probe) {
        if let Ok(actual) = lmb_sync::decode_root_hash_probe(probe) {
            if lmb_sync::digest_eq(&actual, &expected) {
                return None;
            }
        }
    }
    suite.mod_sync_body.clone()
}

/// Tier-classify one JAR's bytes: the pure-Rust `.class` scanner extracts
/// every Mixin site, then `classify_mod_with_mixins` runs the shared
/// superseded→version→flagship/profile→shim→fallback chain with the clean-room
/// promotion step. Returns `(tier, diagnostic, classes, sites, skipped_client)`.
#[cfg(feature = "mod-suite")]
fn live_mod_classify(
    mod_id: &str,
    jar_bytes: &[u8],
) -> (ExecutionTier, String, usize, usize, usize) {
    let flagship = live_mod_flagship_ids();
    let profiles: BTreeSet<String> = lmb_profile::builtin_reference_profiles()
        .iter()
        .map(|profile| profile.mod_id.clone())
        .collect();
    let report = match scan_jar_mixins_with_options(jar_bytes, JarScanOptions::default()) {
        Ok(report) => report,
        Err(error) => {
            // A jar the parser cannot read is unverifiable — fail closed as
            // Tier 3 with a non-mixin reason so promotion can never fire.
            let m = DiscoveredMod {
                mod_id: mod_id.to_string(),
                has_java_bytecode: true,
                shimcheck_compatible: false,
                has_unsupported_mixin_or_at: false,
                rejection_reasons: vec!["UNSCANNABLE_JAVA_MOD".to_string()],
                mc_version_range: None,
            };
            let (tier, diagnostic, _) =
                classify_mod_with_mixins(&m, &[], &flagship, &profiles, LIVE_MOD_MC_TARGET);
            return (
                tier,
                format!("{diagnostic}; mixin scan failed closed: {error}"),
                0,
                0,
                0,
            );
        }
    };
    let classes = report.classes_scanned;
    let sites = report.sites.len();
    let skipped_client = report.skipped_client_mixins;
    let m = if sites == 0 {
        if classes == 0 {
            // Pure data pack in jar form — no bytecode at all.
            DiscoveredMod {
                mod_id: mod_id.to_string(),
                has_java_bytecode: false,
                shimcheck_compatible: false,
                has_unsupported_mixin_or_at: false,
                rejection_reasons: Vec::new(),
                mc_version_range: None,
            }
        } else {
            // Bytecode but zero mixin sites: the scanner proves nothing about
            // `net.minecraft.*` API surface, so without a full ShimCheck this
            // stays fail-closed Tier 3 rather than a blind Tier 2 grant.
            DiscoveredMod {
                mod_id: mod_id.to_string(),
                has_java_bytecode: true,
                shimcheck_compatible: false,
                has_unsupported_mixin_or_at: false,
                rejection_reasons: vec!["UNVERIFIED_JAVA_BYTECODE".to_string()],
                mc_version_range: None,
            }
        }
    } else {
        // Mixin sites found: the recorded blockers are mixin-coded — the
        // exact shape `classify_mod_with_mixins` needs before it will try
        // lifting (promotion still requires every site to bind a hook).
        DiscoveredMod {
            mod_id: mod_id.to_string(),
            has_java_bytecode: true,
            shimcheck_compatible: false,
            has_unsupported_mixin_or_at: true,
            rejection_reasons: vec!["UNSUPPORTED_MIXIN_INJECTION".to_string()],
            mc_version_range: None,
        }
    };
    let (tier, diagnostic, _) =
        classify_mod_with_mixins(&m, &report.sites, &flagship, &profiles, LIVE_MOD_MC_TARGET);
    (tier, diagnostic, classes, sites, skipped_client)
}

/// `custom_payload` S2C body: `write_str(channel) + raw payload` — the same
/// frame shape `lod_v1_request`/`lod_v1_register` emit.
#[cfg(feature = "mod-suite")]
fn live_mod_payload_body(channel: &str, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(channel.len() + payload.len() + 8);
    write_str(&mut out, channel);
    out.extend_from_slice(payload);
    out
}

/// Startup entry wired from `main`: scans `MC_SPINE_MODS_DIR` (if set), then
/// arms the optional operator manifest (`MC_SPINE_MOD_SYNC`) and progression
/// rules (`MC_SPINE_PROGRESSION`). Without any of the three the bridge stays
/// completely inert — same as a default build.
#[cfg(feature = "mod-suite")]
fn live_mod_suite_boot() {
    let mods_dir = std::env::var_os("MC_SPINE_MODS_DIR");
    let sync_path = std::env::var_os("MC_SPINE_MOD_SYNC");
    let progression_path = std::env::var_os("MC_SPINE_PROGRESSION");
    if mods_dir.is_none() && sync_path.is_none() && progression_path.is_none() {
        return;
    }
    let mut suite = LiveModSuite::new();
    if let Some(dir) = mods_dir.as_deref() {
        suite.scan_mods_dir(std::path::Path::new(dir));
    }
    if let Some(path) = sync_path.as_deref() {
        suite.arm_mod_sync_json(std::path::Path::new(path));
    }
    if let Some(path) = progression_path.as_deref() {
        suite.load_progression_json(std::path::Path::new(path));
    }
    suite.active = true;
    *live_mod_suite().lock().unwrap_or_else(|e| e.into_inner()) = suite;
}

/// Per-tick AppleSkin QoL sync, wired from `game_tick` next to the vanilla
/// hunger update: emits `appleskin:exhaustion_sync` /
/// `appleskin:saturation_sync` `0x19` payloads whenever a player's values move
/// by at least the adapter's delta epsilon. All suite/client state locks are
/// released before the socket is touched.
#[cfg(feature = "mod-suite")]
fn live_mod_appleskin_sync(client: &Client) {
    let bodies = {
        let Ok(mut suite) = live_mod_suite().lock() else {
            return;
        };
        if !suite.active {
            return;
        }
        let exhaustion = *client.walk_acc.lock().unwrap_or_else(|e| e.into_inner()) as f32;
        let saturation = *client.sat.lock().unwrap_or_else(|e| e.into_inner());
        let tracker = suite
            .appleskin
            .entry(client.eid)
            .or_insert_with(AppleSkinDeltaTracker::new);
        let mut bodies = Vec::with_capacity(2);
        if let Some(payload) = tracker.poll_exhaustion(exhaustion) {
            bodies.push(live_mod_payload_body(
                CHANNEL_APPLESKIN_EXHAUSTION,
                &payload,
            ));
        }
        if let Some(payload) = tracker.poll_saturation(saturation) {
            bodies.push(live_mod_payload_body(
                CHANNEL_APPLESKIN_SATURATION,
                &payload,
            ));
        }
        bodies
    };
    if bodies.is_empty() {
        return;
    }
    if let Ok(mut sock) = client.sock.lock() {
        for body in bodies {
            let _ = send_packet(&mut sock, LIVE_MOD_S2C_CUSTOM_PAYLOAD_ID, body);
        }
    }
}

/// Per-game-tick entry wired into `game_tick` as `run("live_mods", ...)`.
/// Inactive suites return before touching the client list, and the `Arc`s
/// are cloned so no lock is held across per-client socket writes.
#[cfg(feature = "mod-suite")]
fn live_mod_tick() {
    if live_mod_suite()
        .lock()
        .map(|suite| !suite.active)
        .unwrap_or(true)
    {
        return;
    }
    let players = clients().lock().unwrap_or_else(|e| e.into_inner()).clone();
    for client in &players {
        live_mod_appleskin_sync(client);
    }
}

/// Serve the armed `mc-spine:mod_sync_v1` (`MSY1`) manifest to a client that
/// just entered play, wired from the join path. A no-op until an operator
/// manifest validates — vanilla joins send nothing new.
#[cfg(feature = "mod-suite")]
fn live_mod_sync_on_join(s: &mut TcpStream) {
    let body = live_mod_suite()
        .lock()
        .ok()
        .and_then(|suite| suite.mod_sync_body.clone());
    let Some(msy1) = body else {
        return;
    };
    let _ = send_packet(
        s,
        LIVE_MOD_S2C_CUSTOM_PAYLOAD_ID,
        live_mod_payload_body(lmb_sync::MOD_SYNC_CHANNEL, &msy1),
    );
}

/// Evaluate the armed progression gates for a player at `biome` in
/// `dimension` holding `player_advancements`. Creative/spectator players are
/// always allowed, mirroring the engine contract.
#[cfg(feature = "mod-suite")]
fn live_check_progression_barrier(
    dim: &str,
    biome: &str,
    player_advancements: &BTreeSet<String>,
    is_creative_or_spectator: bool,
) -> LiveProgressionVerdict {
    let Ok(suite) = live_mod_suite().lock() else {
        return LiveProgressionVerdict::Allowed;
    };
    match suite.barriers.check_progression(
        dim,
        biome,
        player_advancements,
        is_creative_or_spectator,
    ) {
        ProgressionVerdict::Allowed => LiveProgressionVerdict::Allowed,
        ProgressionVerdict::Locked {
            rule_id,
            missing_advancements,
            block_break_blocked,
            block_place_blocked,
            effects,
            periodic_damage,
        } => LiveProgressionVerdict::Locked {
            rule_id,
            missing_advancements,
            block_break_blocked,
            block_place_blocked,
            effects,
            periodic_damage,
        },
    }
}

/// Advancement ids `player` provably completed under the vanilla tree — the
/// seed set `live_progression_denies_break` merges bridge-side extra grants
/// into before checking the barrier engine.
#[cfg(feature = "mod-suite")]
fn live_player_advancement_set(player: &str) -> BTreeSet<String> {
    let tracker = player_advancements()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    tracker
        .definitions
        .iter()
        .filter(|def| tracker.completed(player, def.id))
        .map(|def| def.id.to_string())
        .collect()
}

/// Record an advancement id outside the vanilla tree for `player` — modded
/// progression the vanilla tracker can never `completed()`, granted by a
/// Tier 2 shim event hook or an operator. Returns `false` on lock failure.
#[cfg(feature = "mod-suite")]
#[allow(dead_code)]
fn live_progression_grant(player: &str, advancement_id: &str) -> bool {
    live_mod_suite()
        .lock()
        .map(|mut suite| {
            suite
                .extra_advancements
                .insert((player.to_string(), advancement_id.to_string()))
        })
        .unwrap_or(false)
}

/// Live dig gate wired into `finish_block_break`: an armed progression rule
/// sealing this dimension/biome denies the break and resyncs the cell so the
/// client sees the block reappear — the same response the adventure-mode
/// recheck produces. Zero cost while no rules are armed.
#[cfg(feature = "mod-suite")]
fn live_progression_denies_break(me: &Client, dim: DimId, x: i32, y: i32, z: i32) -> bool {
    {
        let Ok(suite) = live_mod_suite().lock() else {
            return false;
        };
        if suite.barriers.rules().is_empty() {
            return false;
        }
    }
    let dim_id = dim.dimension_id().to_string();
    // An unknown/ungenerated biome names `""`: rules must opt into matching
    // it, so a world without a generator can never silently unlock a gate.
    let biome = worldgen::with_gen_dim(dim, |g| g.biome_name_at(x, y, z))
        .flatten()
        .unwrap_or_default();
    let mut advancements = live_player_advancement_set(&me.name);
    {
        let Ok(suite) = live_mod_suite().lock() else {
            return false;
        };
        for (player, advancement) in &suite.extra_advancements {
            if player == &me.name {
                advancements.insert(advancement.clone());
            }
        }
    }
    let creative = matches!(*me.gm.lock().unwrap_or_else(|e| e.into_inner()), 1 | 3);
    match live_check_progression_barrier(&dim_id, &biome, &advancements, creative) {
        LiveProgressionVerdict::Locked {
            block_break_blocked: true,
            ..
        } => {
            ac_resync_block(me, dim, (x, y, z));
            true
        }
        _ => false,
    }
}

// ---------- operator-file JSON readers ----------

/// Parse the mod-sync manifest JSON described at `arm_mod_sync_json`.
#[cfg(feature = "mod-suite")]
fn live_mod_sync_manifest_from_json(bytes: &[u8]) -> Result<lmb_sync::SyncManifest, String> {
    let root = jparse(bytes).ok_or_else(|| "invalid JSON".to_string())?;
    let entries =
        json::jarr(&root, "entries").ok_or_else(|| "missing 'entries' array".to_string())?;
    let mut out = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let need = |key: &str| -> Result<String, String> {
            json::jstr(entry, key).ok_or_else(|| format!("entry {index}: missing string '{key}'"))
        };
        let size = entry
            .get("size_bytes")
            .map(J::num)
            .ok_or_else(|| format!("entry {index}: missing number 'size_bytes'"))?;
        out.push(lmb_sync::ModSyncEntry {
            mod_id: need("mod_id")?,
            file_name: need("file_name")?,
            download_url: need("download_url")?,
            sha256_hex: need("sha256_hex")?,
            size_bytes: if size >= 0.0 && size.is_finite() {
                size as u64
            } else {
                return Err(format!("entry {index}: 'size_bytes' out of range"));
            },
        });
    }
    Ok(lmb_sync::SyncManifest { entries: out })
}

/// Parse the progression-rules JSON described at `load_progression_json`.
#[cfg(feature = "mod-suite")]
fn live_mod_progression_rules_from_json(bytes: &[u8]) -> Result<Vec<ProgressionRule>, String> {
    let root = jparse(bytes).ok_or_else(|| "invalid JSON".to_string())?;
    let rules = json::jarr(&root, "rules").ok_or_else(|| "missing 'rules' array".to_string())?;
    let mut out = Vec::with_capacity(rules.len());
    for (index, rule) in rules.iter().enumerate() {
        let id = json::jstr(rule, "id").ok_or_else(|| format!("rule {index}: missing 'id'"))?;
        let dimension = json::jstr(rule, "dimension")
            .ok_or_else(|| format!("rule {index} ({id}): missing 'dimension'"))?;
        let biomes = json::jarr(rule, "biomes")
            .ok_or_else(|| format!("rule {index} ({id}): missing 'biomes' array"))?
            .iter()
            .map(|b| b.str().to_string())
            .collect();
        let requires = json::jarr(rule, "requires")
            .map(|arr| arr.iter().map(|a| a.str().to_string()).collect())
            .unwrap_or_default();
        let effects = json::jarr(rule, "effects")
            .map(|arr| {
                arr.iter()
                    .filter_map(|e| {
                        let parts = e.as_arr()?;
                        if parts.len() != 3 {
                            return None;
                        }
                        Some((
                            parts[0].str().to_string(),
                            parts[1].num() as u32,
                            parts[2].num().clamp(0.0, 255.0) as u8,
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let periodic_damage = json::jarr(rule, "damage").and_then(|parts| {
            if parts.len() != 2 {
                return None;
            }
            let hearts = parts[0].num() as f32;
            let interval = parts[1].num();
            (hearts.is_finite() && interval >= 1.0).then_some((hearts, interval as u32))
        });
        out.push(ProgressionRule {
            id,
            dimension,
            target_biomes: biomes,
            required_advancements: requires,
            block_break_protected: json::jbool(rule, "block_break", true),
            block_place_protected: json::jbool(rule, "block_place", true),
            enforce_effects: effects,
            periodic_damage,
        });
    }
    Ok(out)
}

// ===========================================================================
// Default-build stubs: identical call sites, zero emitted code.
// ===========================================================================

/// No-op without `mod-suite`: no mods dir scan, no classification.
#[cfg(not(feature = "mod-suite"))]
#[inline(always)]
fn live_mod_suite_boot() {}

/// No-op without `mod-suite`: no AppleSkin frames ever leave the server.
#[cfg(not(feature = "mod-suite"))]
#[allow(dead_code)]
#[inline(always)]
fn live_mod_appleskin_sync(_client: &Client) {}

/// No-op without `mod-suite`: no mod-sync manifest is ever armed.
#[cfg(not(feature = "mod-suite"))]
#[inline(always)]
fn live_mod_sync_on_join(_s: &mut TcpStream) {}

/// No-op without `mod-suite`: the tick hook contributes nothing.
#[cfg(not(feature = "mod-suite"))]
#[inline(always)]
fn live_mod_tick() {}

/// `Allowed` without `mod-suite`: no barrier engine exists to consult.
#[cfg(not(feature = "mod-suite"))]
#[allow(dead_code)]
#[inline(always)]
fn live_check_progression_barrier(
    _dim: &str,
    _biome: &str,
    _player_advancements: &BTreeSet<String>,
    _is_creative_or_spectator: bool,
) -> LiveProgressionVerdict {
    LiveProgressionVerdict::Allowed
}

/// `false` without `mod-suite`: digs proceed exactly as before.
#[cfg(not(feature = "mod-suite"))]
#[inline(always)]
fn live_progression_denies_break(_me: &Client, _dim: DimId, _x: i32, _y: i32, _z: i32) -> bool {
    false
}

// ===========================================================================
// Tests
// ===========================================================================

/// Default-build contract: every live hook is present and a pure no-op, so
/// call sites in `main.rs`/`play_protocol.rs` never need `#[cfg]` of their
/// own.
#[cfg(all(test, not(feature = "mod-suite")))]
mod wire_mods_stub_tests {
    use super::*;

    #[test]
    fn wire_mods_stubs_compile_and_noop() {
        live_mod_suite_boot();
        let verdict = live_check_progression_barrier(
            "minecraft:overworld",
            "minecraft:plains",
            &BTreeSet::new(),
            false,
        );
        assert_eq!(verdict, LiveProgressionVerdict::Allowed);
        assert_eq!(LIVE_MOD_S2C_CUSTOM_PAYLOAD_ID, 0x19);
    }

    #[test]
    fn wire_mods_progression_verdict_mirror_shape() {
        let locked = LiveProgressionVerdict::Locked {
            rule_id: "gate:test".to_string(),
            missing_advancements: vec!["minecraft:story/root".to_string()],
            block_break_blocked: true,
            block_place_blocked: false,
            effects: vec![("minecraft:darkness".to_string(), 200, 0)],
            periodic_damage: Some((1.0, 40)),
        };
        let LiveProgressionVerdict::Locked {
            rule_id,
            block_break_blocked,
            periodic_damage,
            ..
        } = locked
        else {
            panic!("locked verdict lost its payload");
        };
        assert_eq!(rule_id, "gate:test");
        assert!(block_break_blocked);
        assert_eq!(periodic_damage, Some((1.0, 40)));
    }
}

/// Suite-enabled tests: real jars, real `.class` bytecode, real wire frames.
/// The class/ZIP fixtures below are hand-assembled JVMS §4 and APPNOTE
/// bytes, the same proven builder shapes as `mixin_jar_scan.rs` — no `javac`,
/// no external JARs, no crates.
#[cfg(all(test, feature = "mod-suite"))]
mod wire_mods_tests {
    use super::*;
    use mod_adapter_core::mixin_lifter::{
        InjectPoint, MixinAnnotationKind, MixinLiftVerdict, StandardEventHook,
    };
    use std::io::Read;

    /// Several tests reprogram the process-global suite; serialize them.
    static WIRE_LOCK: Mutex<()> = Mutex::new(());

    fn fresh_suite() {
        *live_mod_suite().lock().unwrap_or_else(|e| e.into_inner()) = LiveModSuite::new();
    }

    // ---------- JVM .class file builder (same shape as mixin_jar_scan) ----------

    enum CpItem {
        Utf8(Vec<u8>),
        Class(u16),
        Int(i32),
    }

    struct Cp {
        items: Vec<CpItem>,
        utf8: HashMap<Vec<u8>, u16>,
        class: HashMap<Vec<u8>, u16>,
        ints: HashMap<i32, u16>,
    }

    impl Cp {
        fn new() -> Self {
            Self {
                items: Vec::new(),
                utf8: HashMap::new(),
                class: HashMap::new(),
                ints: HashMap::new(),
            }
        }

        fn push(&mut self, item: CpItem) -> u16 {
            let index = (self.items.len() + 1) as u16;
            self.items.push(item);
            index
        }

        fn utf8(&mut self, s: &str) -> u16 {
            if let Some(&i) = self.utf8.get(s.as_bytes()) {
                return i;
            }
            let index = self.push(CpItem::Utf8(s.as_bytes().to_vec()));
            self.utf8.insert(s.as_bytes().to_vec(), index);
            index
        }

        fn utf8_idx(&self, s: &str) -> u16 {
            self.utf8[s.as_bytes()]
        }

        fn class(&mut self, internal_name: &str) -> u16 {
            if let Some(&i) = self.class.get(internal_name.as_bytes()) {
                return i;
            }
            let name_index = self.utf8(internal_name);
            let index = self.push(CpItem::Class(name_index));
            self.class.insert(internal_name.as_bytes().to_vec(), index);
            index
        }

        fn class_idx(&self, internal_name: &str) -> u16 {
            self.class[internal_name.as_bytes()]
        }

        fn int(&mut self, v: i32) -> u16 {
            if let Some(&i) = self.ints.get(&v) {
                return i;
            }
            let index = self.push(CpItem::Int(v));
            self.ints.insert(v, index);
            index
        }

        fn int_idx(&self, v: i32) -> u16 {
            self.ints[&v]
        }

        fn count(&self) -> u16 {
            (self.items.len() + 1) as u16
        }

        fn encode(&self) -> Vec<u8> {
            let mut out = Vec::new();
            for item in &self.items {
                match item {
                    CpItem::Utf8(bytes) => {
                        out.push(1);
                        out.extend((bytes.len() as u16).to_be_bytes());
                        out.extend(bytes);
                    }
                    CpItem::Class(name_index) => {
                        out.push(7);
                        out.extend(name_index.to_be_bytes());
                    }
                    CpItem::Int(v) => {
                        out.push(3);
                        out.extend(v.to_be_bytes());
                    }
                }
            }
            out
        }
    }

    /// One annotation `element_value` for the fixture builder.
    enum Ev {
        S(String),
        Z(bool),
        Cls(String),
        Ann(Box<Ann>),
        Arr(Vec<Ev>),
    }

    /// One `annotation` structure plus the attribute it belongs in.
    struct Ann {
        desc: String,
        elems: Vec<(String, Ev)>,
        invisible: bool,
    }

    fn ann(desc: &str, elems: Vec<(&str, Ev)>, invisible: bool) -> Ann {
        Ann {
            desc: desc.to_string(),
            elems: elems.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
            invisible,
        }
    }

    fn ev_s(v: &str) -> Ev {
        Ev::S(v.to_string())
    }

    fn ev_cls(internal: &str) -> Ev {
        Ev::Cls(internal.to_string())
    }

    fn write_ev(out: &mut Vec<u8>, cp: &mut Cp, ev: &Ev) {
        match ev {
            Ev::S(v) => {
                out.push(b's');
                out.extend(cp.utf8(v).to_be_bytes());
            }
            Ev::Z(b) => {
                out.push(b'Z');
                out.extend(cp.int(i32::from(*b)).to_be_bytes());
            }
            Ev::Cls(name) => {
                out.push(b'c');
                out.extend(cp.class(name).to_be_bytes());
            }
            Ev::Ann(nested) => {
                out.push(b'@');
                write_ann(out, cp, nested);
            }
            Ev::Arr(items) => {
                out.push(b'[');
                out.extend((items.len() as u16).to_be_bytes());
                for item in items {
                    write_ev(out, cp, item);
                }
            }
        }
    }

    fn write_ann(out: &mut Vec<u8>, cp: &mut Cp, ann: &Ann) {
        out.extend(cp.utf8(&ann.desc).to_be_bytes());
        out.extend((ann.elems.len() as u16).to_be_bytes());
        for (name, ev) in &ann.elems {
            out.extend(cp.utf8(name).to_be_bytes());
            write_ev(out, cp, ev);
        }
    }

    /// `Runtime{Visible,Invisible}Annotations` attribute payload for a group
    /// of same-visibility annotations.
    fn anns_payload(cp: &mut Cp, anns: &[&Ann]) -> Vec<u8> {
        let mut payload = (anns.len() as u16).to_be_bytes().to_vec();
        for ann in anns {
            write_ann(&mut payload, cp, ann);
        }
        payload
    }

    struct Meth {
        name: String,
        anns: Vec<Ann>,
    }

    fn meth(name: &str, anns: Vec<Ann>) -> Meth {
        Meth {
            name: name.to_string(),
            anns,
        }
    }

    /// Assemble a complete `.class` file: public class `internal` extending
    /// `java/lang/Object`, one private `int dummy` field carrying a
    /// `ConstantValue` attribute, each method a `()V` stub with a real `Code`
    /// attribute plus its annotation attributes, class-level annotations,
    /// and a `SourceFile` attribute.
    fn build_class(internal: &str, class_anns: Vec<Ann>, methods: Vec<Meth>) -> Vec<u8> {
        let mut cp = Cp::new();
        cp.class(internal);
        cp.class("java/lang/Object");
        for s in [
            "Code",
            "ConstantValue",
            "SourceFile",
            "RuntimeVisibleAnnotations",
            "RuntimeInvisibleAnnotations",
            "dummy",
            "I",
            "()V",
            "Dummy.java",
        ] {
            cp.utf8(s);
        }
        for m in &methods {
            cp.utf8(&m.name);
        }
        cp.int(7);

        let mut method_attrs: Vec<Vec<(u16, Vec<u8>)>> = Vec::new();
        for m in &methods {
            let mut attrs = Vec::new();
            let code = {
                let mut p = Vec::new();
                p.extend(1u16.to_be_bytes());
                p.extend(1u16.to_be_bytes());
                p.extend(1u32.to_be_bytes());
                p.push(0xB1);
                p.extend(0u16.to_be_bytes());
                p.extend(0u16.to_be_bytes());
                p
            };
            attrs.push((cp.utf8("Code"), code));
            let (invisible, visible): (Vec<&Ann>, Vec<&Ann>) =
                m.anns.iter().partition(|a| a.invisible);
            for (name, group) in [
                ("RuntimeInvisibleAnnotations", invisible),
                ("RuntimeVisibleAnnotations", visible),
            ] {
                if !group.is_empty() {
                    attrs.push((cp.utf8(name), anns_payload(&mut cp, &group)));
                }
            }
            method_attrs.push(attrs);
        }
        let (invisible, visible): (Vec<&Ann>, Vec<&Ann>) =
            class_anns.iter().partition(|a| a.invisible);
        let mut class_attrs: Vec<(u16, Vec<u8>)> = Vec::new();
        for (name, group) in [
            ("RuntimeInvisibleAnnotations", invisible),
            ("RuntimeVisibleAnnotations", visible),
        ] {
            if !group.is_empty() {
                class_attrs.push((cp.utf8(name), anns_payload(&mut cp, &group)));
            }
        }
        class_attrs.push((
            cp.utf8("SourceFile"),
            cp.utf8("Dummy.java").to_be_bytes().to_vec(),
        ));

        let mut out = Vec::new();
        out.extend(0xCAFE_BABEu32.to_be_bytes());
        out.extend(0u16.to_be_bytes());
        out.extend(61u16.to_be_bytes());
        out.extend(cp.count().to_be_bytes());
        out.extend(cp.encode());
        out.extend(0x0021u16.to_be_bytes());
        out.extend(cp.class_idx(internal).to_be_bytes());
        out.extend(cp.class_idx("java/lang/Object").to_be_bytes());
        out.extend(0u16.to_be_bytes());

        out.extend(1u16.to_be_bytes());
        out.extend(0x0002u16.to_be_bytes());
        out.extend(cp.utf8_idx("dummy").to_be_bytes());
        out.extend(cp.utf8_idx("I").to_be_bytes());
        out.extend(1u16.to_be_bytes());
        out.extend(cp.utf8_idx("ConstantValue").to_be_bytes());
        out.extend(2u32.to_be_bytes());
        out.extend(cp.int_idx(7).to_be_bytes());

        out.extend((methods.len() as u16).to_be_bytes());
        for (m, attrs) in methods.iter().zip(&method_attrs) {
            out.extend(0x0001u16.to_be_bytes());
            out.extend(cp.utf8_idx(&m.name).to_be_bytes());
            out.extend(cp.utf8_idx("()V").to_be_bytes());
            out.extend((attrs.len() as u16).to_be_bytes());
            for (name_index, payload) in attrs {
                out.extend(name_index.to_be_bytes());
                out.extend((payload.len() as u32).to_be_bytes());
                out.extend(payload);
            }
        }

        out.extend((class_attrs.len() as u16).to_be_bytes());
        for (name_index, payload) in &class_attrs {
            out.extend(name_index.to_be_bytes());
            out.extend((payload.len() as u32).to_be_bytes());
            out.extend(payload);
        }
        out
    }

    // ---------- ZIP/JAR builder (stored members, real central directory) ----------

    fn lmb_crc32(data: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &byte in data {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                let mask = 0u32.wrapping_sub(crc & 1);
                crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
            }
        }
        !crc
    }

    /// Pack `entries` (name → bytes) into a real ZIP archive with stored
    /// (uncompressed) members.
    fn lmb_test_jar(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, data) in entries {
            let name = name.as_bytes();
            let crc = lmb_crc32(data);
            let size = data.len() as u32;
            let offset = out.len() as u32;
            out.extend(0x0403_4B50u32.to_le_bytes());
            out.extend(10u16.to_le_bytes());
            out.extend(0u16.to_le_bytes());
            out.extend(0u16.to_le_bytes());
            out.extend(0u16.to_le_bytes());
            out.extend(0u16.to_le_bytes());
            out.extend(crc.to_le_bytes());
            out.extend(size.to_le_bytes());
            out.extend(size.to_le_bytes());
            out.extend((name.len() as u16).to_le_bytes());
            out.extend(0u16.to_le_bytes());
            out.extend(name);
            out.extend(*data);
            central.extend(0x0201_4B50u32.to_le_bytes());
            central.extend(20u16.to_le_bytes());
            central.extend(10u16.to_le_bytes());
            central.extend(0u16.to_le_bytes());
            central.extend(0u16.to_le_bytes());
            central.extend(0u16.to_le_bytes());
            central.extend(0u16.to_le_bytes());
            central.extend(crc.to_le_bytes());
            central.extend(size.to_le_bytes());
            central.extend(size.to_le_bytes());
            central.extend((name.len() as u16).to_le_bytes());
            central.extend(0u16.to_le_bytes());
            central.extend(0u16.to_le_bytes());
            central.extend(0u16.to_le_bytes());
            central.extend(0u16.to_be_bytes());
            central.extend(0u32.to_le_bytes());
            central.extend(offset.to_le_bytes());
            central.extend(name);
        }
        let cd_offset = out.len() as u32;
        let cd_size = central.len() as u32;
        out.extend(central);
        out.extend(0x0605_4B50u32.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend((entries.len() as u16).to_le_bytes());
        out.extend((entries.len() as u16).to_le_bytes());
        out.extend(cd_size.to_le_bytes());
        out.extend(cd_offset.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out
    }

    // ---------- mixin fixture helpers ----------

    const MIXIN: &str = "Lorg/spongepowered/asm/mixin/Mixin;";
    const INJECT: &str = "Lorg/spongepowered/asm/mixin/injection/Inject;";
    const REDIRECT: &str = "Lorg/spongepowered/asm/mixin/injection/Redirect;";
    const AT: &str = "Lorg/spongepowered/asm/mixin/injection/At;";

    fn at(value: &str) -> Ev {
        Ev::Ann(Box::new(ann(AT, vec![("value", ev_s(value))], true)))
    }

    fn mixin(elems: Vec<(&str, Ev)>) -> Ann {
        ann(MIXIN, elems, true)
    }

    /// A `@Mixin(target)` class with one `@Inject(method, at = HEAD,
    /// cancellable)` method — the liftable-injection fixture shape.
    fn lmb_inject_jar(mod_pkg: &str, target: &str, method: &str) -> Vec<u8> {
        let class = build_class(
            &format!("{mod_pkg}/mixin/TestMixin"),
            vec![mixin(vec![("value", Ev::Arr(vec![ev_cls(target)]))])],
            vec![meth(
                "handler",
                vec![ann(
                    INJECT,
                    vec![
                        ("method", ev_s(method)),
                        ("at", at("HEAD")),
                        ("cancellable", Ev::Z(true)),
                    ],
                    true,
                )],
            )],
        );
        let config = format!(
            r#"{{"required":true,"package":"{pkg}.mixin","mixins":["TestMixin"]}}"#,
            pkg = mod_pkg.replace('/', ".")
        );
        lmb_test_jar(&[
            ("test.mixins.json", config.as_bytes()),
            (
                &format!("{mod_pkg}/mixin/TestMixin.class"),
                class.as_slice(),
            ),
        ])
    }

    /// A `@Mixin` class whose injection uses `@Redirect` — always unliftable.
    fn lmb_redirect_jar(mod_pkg: &str, target: &str, method: &str) -> Vec<u8> {
        let class = build_class(
            &format!("{mod_pkg}/mixin/RedirectMixin"),
            vec![mixin(vec![("value", Ev::Arr(vec![ev_cls(target)]))])],
            vec![meth(
                "handler",
                vec![ann(
                    REDIRECT,
                    vec![("method", ev_s(method)), ("at", at("INVOKE"))],
                    true,
                )],
            )],
        );
        let config = format!(
            r#"{{"required":true,"package":"{pkg}.mixin","mixins":["RedirectMixin"]}}"#,
            pkg = mod_pkg.replace('/', ".")
        );
        lmb_test_jar(&[
            ("test.mixins.json", config.as_bytes()),
            (
                &format!("{mod_pkg}/mixin/RedirectMixin.class"),
                class.as_slice(),
            ),
        ])
    }

    /// A culinary jar fixture: one cooking recipe, one cutting recipe, one
    /// item tag, one loot modifier and one food spec.
    fn lmb_culinary_jar(ns: &str) -> Vec<u8> {
        let cooking = format!(
            r#"{{"type":"{ns}:cooking","ingredients":[{{"item":"minecraft:carrot"}}],
                "result":{{"id":"{ns}:test_soup","count":1}},
                "container":{{"item":"minecraft:bowl"}},"cookingtime":200,"experience":0.35}}"#
        );
        let cutting = format!(
            r#"{{"type":"{ns}:cutting","ingredients":[{{"item":"{ns}:test_soup"}}],
                "tool":{{"item":"{ns}:knife"}},
                "result":[{{"item":"{ns}:soup_slice","count":2}}]}}"#
        );
        let tag = r#"{"values":["minecraft:carrot","minecraft:potato"]}"#;
        let loot_mod = format!(
            r#"{{"type":"{ns}:add_item","item":"{ns}:soup_slice","count":1,
                "conditions":[{{"condition":"minecraft:loot_table_id",
                "loot_table_id":"minecraft:chests/village/village_cartographer"}}]}}"#
        );
        let food = r#"{"nutrition":6,"saturation":0.6}"#;
        lmb_test_jar(&[
            (
                &format!("data/{ns}/recipes/test_soup.json"),
                cooking.as_bytes(),
            ),
            (
                &format!("data/{ns}/recipes/test_soup_slice.json"),
                cutting.as_bytes(),
            ),
            (
                &format!("data/{ns}/tags/item/vegetables.json"),
                tag.as_bytes(),
            ),
            (
                &format!("data/{ns}/loot_modifiers/soup_slice.json"),
                loot_mod.as_bytes(),
            ),
            (&format!("data/{ns}/food/test_soup.json"), food.as_bytes()),
        ])
    }

    fn lmb_temp_dir(tag: &str) -> std::path::PathBuf {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "mc-spine-wire-mods-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A fresh client/server `TcpStream` pair: `send` is the end the server
    /// writes to, `peer` reads exactly those bytes.
    fn lmb_socket_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let send = TcpStream::connect(addr).unwrap();
        let (peer, _) = listener.accept().unwrap();
        (send, peer)
    }

    // ---------- loopback client fixture (same shape as play_protocol's) ----------

    fn lmb_client(name: &str, eid: i32) -> (Arc<Client>, std::net::TcpStream) {
        let (stream, peer) = lmb_socket_pair();
        let me = Arc::new(Client {
            eid,
            uuid: [0x77u8; 16],
            name: name.to_string(),
            player_attack_damage_id: None,
            pos: Mutex::new((0.5, 65.0, 0.5, 0.0, 0.0, true)),
            teleport: Mutex::new(Default::default()),
            hp: Mutex::new(20.0),
            sock: Mutex::new(stream),
            inv: Mutex::new(HashMap::new()),
            cursor: Mutex::new((0, 0, Vec::new())),
            drag: Mutex::new((0, Vec::new())),
            open_chest: Mutex::new(None),
            chest_sync: Mutex::new(None),
            open_furnace: Mutex::new(None),
            craft: Mutex::new(HashMap::new()),
            craft9: Mutex::new(HashMap::new()),
            open_table: Mutex::new(false),
            open_merchant: Mutex::new(false),
            trade_sel: Mutex::new(0),
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
            pearl_cd: Mutex::new(Instant::now() - Duration::from_secs(10)),
            xp: Mutex::new(0),
            skin: Mutex::new(None),
            seen: Mutex::new(HashSet::new()),
            chunks: Mutex::new(HashSet::new()),
            chunk_ack: Mutex::new(ChunkFeedback::new(1)),
            gen_sources: Mutex::new(GenSources::default()),
            gm: Mutex::new(0),
            fall_from: Mutex::new(65.0),
            food: Mutex::new(20),
            air: Mutex::new(server_ticks(AIR_SUPPLY_GT) as i32),
            armor_pts: Mutex::new(0),
            sat: Mutex::new(5.0),
            walk_acc: Mutex::new(0.0),
            lastp: Mutex::new((0.5, 65.0, 0.5)),
            spawn_pt: Mutex::new(None),
            dim: Mutex::new(DimId::Overworld),
            spawn_dim: Mutex::new(DimId::Overworld),
            view: Mutex::new(2),
            in_portal: Mutex::new(false),
            portal_at: Mutex::new(None),
            held: Mutex::new(0),
            effs: Mutex::new(Vec::new()),
            lod_v1_enabled: AtomicBool::new(false),
            underground_revealed: AtomicBool::new(false),
            sleeping: Mutex::new(None),
            inv_state_id: AtomicI32::new(0),
        });
        (me, peer)
    }

    /// Read one framed server packet from the client peer. TCP may coalesce
    /// or split frames, so `stash` buffers bytes across calls: only the
    /// varint-delimited frame is consumed per call.
    fn lmb_read_frame(peer: &mut TcpStream, stash: &mut Vec<u8>) -> Option<(i64, Vec<u8>)> {
        peer.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
        loop {
            {
                let mut sl = stash.as_slice();
                if let Some(len) = read_varint(&mut sl) {
                    let head = stash.len() - sl.len();
                    if sl.len() >= len as usize {
                        let mut frame = &sl[..len as usize];
                        let id = read_varint(&mut frame)?;
                        let body = frame.to_vec();
                        stash.drain(..head + len as usize);
                        return Some((id, body));
                    }
                }
            }
            let mut buf = [0u8; 4096];
            let n = peer.read(&mut buf).ok()?;
            stash.extend_from_slice(&buf[..n]);
        }
    }

    /// Decode a `custom_payload` S2C body into `(channel, payload)`.
    fn lmb_decode_payload(body: &[u8]) -> (String, Vec<u8>) {
        let mut sl = body;
        let len = read_varint(&mut sl).expect("channel varint") as usize;
        let (channel, payload) = sl.split_at(len);
        (
            std::str::from_utf8(channel).unwrap().to_string(),
            payload.to_vec(),
        )
    }

    fn lmb_silent(peer: &mut TcpStream) -> bool {
        peer.set_read_timeout(Some(Duration::from_millis(150))).ok();
        peer.read(&mut [0u8; 8]).is_err()
    }

    // ---------- boot scan ----------

    #[test]
    fn live_mod_bridge_auto_registers_native_bundles_on_boot() {
        let _serial = WIRE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = lmb_temp_dir("bundle-boot");
        let adapted = dir.join("adapted");
        std::fs::create_dir_all(&adapted).unwrap();
        for (path, id, channel) in [
            (dir.join("first.bundle.json"), "first:mod", "first:sync"),
            (adapted.join("second.bundle.json"), "second:mod", "second:sync"),
        ] {
            std::fs::write(path, format!(
                r#"{{"schema":"mc-spine-mod-bundle/1","mod_id":"{id}","mc_version_range":"[1.21.1,1.21.1]","portals":[],"progression_barriers":[],"biome_regions":[],"creatures":[],"custom_channels":["{channel}"]}}"#
            )).unwrap();
        }
        // A parsed but incompatible bundle must not reserve the id of a
        // subsequent valid bundle during the same boot.
        std::fs::write(dir.join("a-incompatible.bundle.json"),
            r#"{"schema":"mc-spine-mod-bundle/1","mod_id":"first:mod","mc_version_range":"[1.20.1,1.20.1]","portals":[],"progression_barriers":[],"biome_regions":[],"creatures":[],"custom_channels":[]}"#
        ).unwrap();
        std::fs::write(dir.join("invalid.bundle.json"), r#"{"schema":"wrong"}"#).unwrap();
        let previous = std::env::var_os("MC_SPINE_MODS_DIR");
        std::env::set_var("MC_SPINE_MODS_DIR", &dir);
        live_mod_suite_boot();
        if let Some(value) = previous {
            std::env::set_var("MC_SPINE_MODS_DIR", value);
        } else {
            std::env::remove_var("MC_SPINE_MODS_DIR");
        }
        let runtime = live_mod_unified_runtime();
        assert_eq!(runtime.installed_mod_ids(), &["first:mod", "second:mod"]);
        assert!(runtime.is_channel_registered("first:sync"));
        assert!(runtime.is_channel_registered("second:sync"));
        fresh_suite();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn live_mod_bridge_auto_adapts_jars_on_boot() {
        let _serial = WIRE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = lmb_temp_dir("jar-boot");
        std::fs::write(dir.join("sodium-0.6.jar"), lmb_test_jar(&[("x.bin", b"x")])).unwrap();
        std::fs::write(dir.join("delightaddon-1.jar"), lmb_culinary_jar("delightaddon")).unwrap();
        std::fs::write(dir.join("safemod-1.jar"), lmb_inject_jar(
            "com/safemod", "net/minecraft/world/entity/LivingEntity", "hurt"
        )).unwrap();
        std::fs::write(dir.join("redirectmod-1.jar"), lmb_redirect_jar(
            "com/redmod", "net/minecraft/world/entity/LivingEntity", "hurt"
        )).unwrap();
        let previous = std::env::var_os("MC_SPINE_MODS_DIR");
        std::env::set_var("MC_SPINE_MODS_DIR", &dir);
        live_mod_suite_boot();
        if let Some(value) = previous {
            std::env::set_var("MC_SPINE_MODS_DIR", value);
        } else {
            std::env::remove_var("MC_SPINE_MODS_DIR");
        }
        let suite = live_mod_suite().lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(suite.auto_adapted.len(), 3);
        assert_eq!(suite.culinary_stats.cooking_recipes, 1);
        assert_eq!(suite.auto_adapted.iter().find(|m| m.mod_id == "delightaddon")
            .unwrap().tag_files, 1);
        let safe = suite.auto_adapted.iter().find(|m| m.mod_id == "safemod").unwrap();
        assert_eq!(safe.tier, ExecutionTier::BoundedJvmShim);
        assert_eq!(safe.lifted_hooks.len(), 1);
        assert_eq!(suite.lifted_hooks.len(), 1);
        assert_eq!(suite.lifted_hooks[0].1.hook, StandardEventHook::EntityDamagePre);
        let fallback = suite.auto_adapted.iter().find(|m| m.mod_id == "redirectmod").unwrap();
        assert_eq!(fallback.tier, ExecutionTier::RequiresFallback);
        assert!(fallback.lifted_hooks.is_empty());
        assert!(suite.reports.iter().any(|r| r.mod_id == "sodium"
            && r.tier == ExecutionTier::BuiltInSuperseded));
        drop(suite);
        fresh_suite();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn wire_mods_boot_scan_supersedes_loads_culinary_and_classifies() {
        let _serial = WIRE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        fresh_suite();
        let dir = lmb_temp_dir("scan");
        std::fs::write(
            dir.join("sodium-0.6.13.jar"),
            lmb_test_jar(&[("x.bin", b"x")]),
        )
        .unwrap();
        std::fs::write(
            dir.join("farmersdelight-1.2.jar"),
            lmb_culinary_jar("farmersdelight"),
        )
        .unwrap();
        std::fs::write(
            dir.join("delightaddon-9.jar"),
            lmb_culinary_jar("delightaddon"),
        )
        .unwrap();
        std::fs::write(
            dir.join("safemod-1.0.jar"),
            lmb_inject_jar(
                "com/safemod",
                "net/minecraft/world/entity/LivingEntity",
                "hurt",
            ),
        )
        .unwrap();
        std::fs::write(
            dir.join("redirectmod-2.0.jar"),
            lmb_redirect_jar(
                "com/redmod",
                "net/minecraft/world/entity/LivingEntity",
                "hurt",
            ),
        )
        .unwrap();

        let mut suite = LiveModSuite::new();
        suite.scan_mods_dir(&dir);

        // Superseded perf mod: classified before any bytecode work.
        let sodium = suite
            .reports
            .iter()
            .find(|r| r.mod_id == "sodium")
            .expect("sodium report");
        assert_eq!(sodium.tier, ExecutionTier::BuiltInSuperseded);
        assert_eq!(sodium.diagnostic, REASON_SUPERSEDED);
        assert_eq!(
            sodium.classes_scanned, 0,
            "superseded jars are never scanned"
        );

        // Culinary jars merged base + addon content into the live registry.
        let stats = &suite.culinary_stats;
        assert_eq!(stats.cooking_recipes, 2, "{stats:?}");
        assert_eq!(stats.cutting_recipes, 2, "{stats:?}");
        assert_eq!(stats.loot_modifiers, 2, "{stats:?}");
        assert_eq!(stats.food_specs, 2, "{stats:?}");
        assert!(stats.tag_values >= 4, "{stats:?}");
        assert!(suite
            .culinary
            .food_spec("farmersdelight:test_soup")
            .is_some());
        assert!(suite.culinary.food_spec("delightaddon:test_soup").is_some());

        // The safe @Inject jar promotes Tier 3 -> bounded Tier 2.
        let safe = suite
            .reports
            .iter()
            .find(|r| r.mod_id == "safemod")
            .expect("safemod report");
        assert_eq!(
            safe.tier,
            ExecutionTier::BoundedJvmShim,
            "{}",
            safe.diagnostic
        );
        assert_eq!(safe.mixin_sites, 1);

        // The @Redirect jar stays Tier 3 with a mixin diagnostic.
        let redirect = suite
            .reports
            .iter()
            .find(|r| r.mod_id == "redirectmod")
            .expect("redirectmod report");
        assert_eq!(redirect.tier, ExecutionTier::RequiresFallback);
        assert!(
            redirect.diagnostic.to_uppercase().contains("MIXIN"),
            "{}",
            redirect.diagnostic
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wire_mods_classify_data_only_and_unverified_bytecode() {
        let _serial = WIRE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // No `.class` members at all -> pure data jar -> Tier 1.
        let data_jar = lmb_test_jar(&[("data/x/recipes/a.json", br#"{"type":"x:y"}"#)]);
        let (tier, _, classes, sites, _) = live_mod_classify("datamod", &data_jar);
        assert_eq!(tier, ExecutionTier::UniversalNative);
        assert_eq!((classes, sites), (0, 0));

        // Bytecode present but no mixin sites -> fail-closed Tier 3.
        let cls = build_class("com/codemod/Thing", vec![], vec![]);
        let code_jar = lmb_test_jar(&[("com/codemod/Thing.class", cls.as_slice())]);
        let (tier, diagnostic, classes, sites, _) = live_mod_classify("codemod", &code_jar);
        assert_eq!(tier, ExecutionTier::RequiresFallback, "{diagnostic}");
        assert_eq!(sites, 0);
        assert!(classes > 0);

        // A truncated jar cannot even be scanned -> Tier 3, scan diagnostic.
        let (tier, diagnostic, _, _, _) = live_mod_classify("badmod", &[0x50, 0x4b, 1, 2]);
        assert_eq!(tier, ExecutionTier::RequiresFallback, "{diagnostic}");
        assert!(diagnostic.contains("UNSCANNABLE"), "{diagnostic}");
    }

    // ---------- appleskin wire ----------

    #[test]
    fn wire_mods_appleskin_emits_767_custom_payload_with_delta_suppression() {
        let _serial = WIRE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        fresh_suite();
        live_mod_suite()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .active = true;
        let (me, mut peer) = lmb_client("wire_appleskin", 977_101);

        // First observation emits both channels once each on the verified
        // play-phase 0x19 id, each a bare 4B BE f32.
        let mut stash = Vec::new();
        live_mod_appleskin_sync(&me);
        let mut channels = Vec::new();
        for _ in 0..2 {
            let (id, body) = lmb_read_frame(&mut peer, &mut stash).expect("appleskin frame");
            assert_eq!(id, LIVE_MOD_S2C_CUSTOM_PAYLOAD_ID);
            let (channel, payload) = lmb_decode_payload(&body);
            assert_eq!(payload.len(), 4, "AppleSkin payload is one BE f32");
            channels.push((channel, payload));
        }
        channels.sort();
        assert_eq!(channels[0].0, CHANNEL_APPLESKIN_EXHAUSTION);
        assert_eq!(channels[1].0, CHANNEL_APPLESKIN_SATURATION);
        let sat_sent = f32::from_be_bytes(channels[1].1.clone().try_into().unwrap());
        assert_eq!(sat_sent, 5.0);

        // Sub-epsilon drift is suppressed; a >=0.01 move emits once.
        *me.sat.lock().unwrap_or_else(|e| e.into_inner()) = 5.005;
        live_mod_appleskin_sync(&me);
        assert!(lmb_silent(&mut peer), "sub-epsilon drift must stay silent");

        *me.sat.lock().unwrap_or_else(|e| e.into_inner()) = 4.5;
        *me.walk_acc.lock().unwrap_or_else(|e| e.into_inner()) = 0.25;
        live_mod_appleskin_sync(&me);
        let mut channels = Vec::new();
        for _ in 0..2 {
            let (id, body) = lmb_read_frame(&mut peer, &mut stash).expect("appleskin frame");
            assert_eq!(id, 0x19);
            channels.push(lmb_decode_payload(&body));
        }
        channels.sort();
        assert_eq!(channels[0].0, CHANNEL_APPLESKIN_EXHAUSTION);
        assert_eq!(
            f32::from_be_bytes(channels[0].1.clone().try_into().unwrap()),
            0.25
        );
        assert_eq!(channels[0].1.len(), 4);
        assert_eq!(channels[1].0, CHANNEL_APPLESKIN_SATURATION);
        assert_eq!(
            f32::from_be_bytes(channels[1].1.clone().try_into().unwrap()),
            4.5
        );

        // An inactive suite emits nothing even with changed values.
        fresh_suite();
        let (me2, mut peer2) = lmb_client("wire_appleskin_off", 977_102);
        *me2.sat.lock().unwrap_or_else(|e| e.into_inner()) = 1.0;
        live_mod_appleskin_sync(&me2);
        assert!(lmb_silent(&mut peer2), "inactive suite must emit nothing");
    }

    // ---------- progression barrier ----------

    fn lmb_gate_rule() -> ProgressionRule {
        ProgressionRule {
            id: "gate:test".to_string(),
            dimension: "minecraft:overworld".to_string(),
            target_biomes: BTreeSet::from([
                "minecraft:plains".to_string(),
                String::new(), // ungenerated worlds resolve to "" — opt-in sealed
            ]),
            required_advancements: vec!["minecraft:story/mine_diamond".to_string()],
            block_break_protected: true,
            block_place_protected: true,
            enforce_effects: vec![("minecraft:darkness".to_string(), 200, 0)],
            periodic_damage: Some((1.0, 40)),
        }
    }

    #[test]
    fn wire_mods_progression_barrier_locks_and_allows() {
        let _serial = WIRE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        fresh_suite();
        live_mod_suite()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .barriers = ProgressionBarrierEngine::new(vec![lmb_gate_rule()]);

        // Missing the gate advancement -> Locked with the full payload.
        let verdict = live_check_progression_barrier(
            "minecraft:overworld",
            "minecraft:plains",
            &BTreeSet::new(),
            false,
        );
        let LiveProgressionVerdict::Locked {
            rule_id,
            missing_advancements,
            block_break_blocked,
            periodic_damage,
            ..
        } = verdict
        else {
            panic!("expected locked verdict, got {verdict:?}");
        };
        assert_eq!(rule_id, "gate:test");
        assert_eq!(missing_advancements, ["minecraft:story/mine_diamond"]);
        assert!(block_break_blocked);
        assert_eq!(periodic_damage, Some((1.0, 40)));

        // Holding it -> Allowed; creative bypasses regardless.
        let held = BTreeSet::from(["minecraft:story/mine_diamond".to_string()]);
        assert_eq!(
            live_check_progression_barrier("minecraft:overworld", "minecraft:plains", &held, false),
            LiveProgressionVerdict::Allowed
        );
        assert_eq!(
            live_check_progression_barrier(
                "minecraft:overworld",
                "minecraft:plains",
                &BTreeSet::new(),
                true
            ),
            LiveProgressionVerdict::Allowed
        );
        // Another dimension/biome never matches this rule.
        assert_eq!(
            live_check_progression_barrier(
                "minecraft:the_nether",
                "minecraft:plains",
                &BTreeSet::new(),
                false
            ),
            LiveProgressionVerdict::Allowed
        );
        assert_eq!(
            live_check_progression_barrier(
                "minecraft:overworld",
                "minecraft:desert",
                &BTreeSet::new(),
                false
            ),
            LiveProgressionVerdict::Allowed
        );
    }

    #[test]
    fn wire_mods_progression_denies_break_and_resyncs() {
        let _serial = WIRE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        fresh_suite();
        {
            let mut suite = live_mod_suite().lock().unwrap_or_else(|e| e.into_inner());
            suite.barriers = ProgressionBarrierEngine::new(vec![lmb_gate_rule()]);
        }
        let (me, mut peer) = lmb_client("wire_gate", 977_103);

        // The gate's `""` biome entry matches the ungenerated test world, so
        // the dig is denied and the cell resynced (0x09 block update).
        assert!(live_progression_denies_break(
            &me,
            DimId::Overworld,
            4,
            64,
            4
        ));
        let mut stash = Vec::new();
        let (id, _) = lmb_read_frame(&mut peer, &mut stash).expect("resync frame");
        assert_eq!(id, 0x09, "denied dig resyncs the client block view");

        // Granting the required advancement (outside the vanilla tree, via
        // the bridge-side grant path) opens the gate.
        assert!(live_progression_grant(
            &me.name,
            "minecraft:story/mine_diamond"
        ));
        assert!(!live_progression_denies_break(
            &me,
            DimId::Overworld,
            4,
            64,
            4
        ));

        // Empty rules short-circuit before any world lookup.
        fresh_suite();
        assert!(!live_progression_denies_break(
            &me,
            DimId::Overworld,
            4,
            64,
            4
        ));
    }

    #[test]
    fn wire_mods_progression_json_parses_rules() {
        let _serial = WIRE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let json = br#"{"rules":[{
            "id":"gate:a","dimension":"twilightforest:twilight_forest",
            "biomes":["twilightforest:naga_courtyard"],
            "requires":["twilightforest:progress/naga"],
            "block_break":true,"block_place":false,
            "effects":[["minecraft:darkness",200,0]],
            "damage":[1.5,40]}]}"#;
        let rules = live_mod_progression_rules_from_json(json).unwrap();
        assert_eq!(rules.len(), 1);
        let rule = &rules[0];
        assert_eq!(rule.id, "gate:a");
        assert_eq!(rule.dimension, "twilightforest:twilight_forest");
        assert!(rule.target_biomes.contains("twilightforest:naga_courtyard"));
        assert_eq!(rule.required_advancements, ["twilightforest:progress/naga"]);
        assert!(rule.block_break_protected);
        assert!(!rule.block_place_protected);
        assert_eq!(
            rule.enforce_effects,
            [("minecraft:darkness".to_string(), 200, 0)]
        );
        assert_eq!(rule.periodic_damage, Some((1.5, 40)));
        assert!(live_mod_progression_rules_from_json(b"not json").is_err());
        assert!(live_mod_progression_rules_from_json(br#"{"rules":[]}"#)
            .unwrap()
            .is_empty());
    }

    // ---------- mod sync manifest ----------

    #[test]
    fn wire_mods_msy1_manifest_served_on_join() {
        let _serial = WIRE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        fresh_suite();
        let dir = lmb_temp_dir("sync");
        let manifest_path = dir.join("manifest.json");
        let sha = "a".repeat(64);
        let json = format!(
            r#"{{"entries":[{{"mod_id":"jei","file_name":"jei.jar",
                "download_url":"https://cdn.modrinth.com/data/jei/jei.jar",
                "sha256_hex":"{sha}","size_bytes":12345}}]}}"#
        );
        std::fs::write(&manifest_path, json).unwrap();
        {
            let mut suite = live_mod_suite().lock().unwrap_or_else(|e| e.into_inner());
            suite.arm_mod_sync_json(&manifest_path);
            assert!(suite.mod_sync_body.is_some(), "valid manifest must arm");
        }

        // The join hook emits one 0x19 frame on `mc-spine:mod_sync_v1`
        // whose body decodes as MSY1 back into the same entry.
        let (mut send, mut peer) = lmb_socket_pair();
        live_mod_sync_on_join(&mut send);
        let mut stash = Vec::new();
        let (id, body) = lmb_read_frame(&mut peer, &mut stash).expect("msy1 frame");
        assert_eq!(id, LIVE_MOD_S2C_CUSTOM_PAYLOAD_ID);
        let (channel, msy1) = lmb_decode_payload(&body);
        assert_eq!(channel, lmb_sync::MOD_SYNC_CHANNEL);
        assert!(msy1.starts_with(&lmb_sync::MSY1_MAGIC));
        let decoded = lmb_sync::decode_sync_manifest(&msy1).unwrap();
        assert_eq!(decoded.entries.len(), 1);
        assert_eq!(decoded.entries[0].mod_id, "jei");
        assert_eq!(decoded.entries[0].file_name, "jei.jar");
        assert_eq!(decoded.entries[0].size_bytes, 12345);
        assert_eq!(decoded.entries[0].sha256_hex, sha);

        // Nothing armed -> nothing sent.
        fresh_suite();
        let (mut send2, mut peer2) = lmb_socket_pair();
        live_mod_sync_on_join(&mut send2);
        assert!(lmb_silent(&mut peer2), "unarmed manifest must emit nothing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wire_mods_msy1_arm_rejects_bad_manifest() {
        let _serial = WIRE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        fresh_suite();
        let dir = lmb_temp_dir("syncbad");
        // http:// (not https) on a trusted host still fails the URL check.
        let bad = dir.join("bad.json");
        std::fs::write(
            &bad,
            br#"{"entries":[{"mod_id":"jei","file_name":"jei.jar",
                "download_url":"http://cdn.modrinth.com/x.jar",
                "sha256_hex":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "size_bytes":10}]}"#,
        )
        .unwrap();
        {
            let mut suite = live_mod_suite().lock().unwrap_or_else(|e| e.into_inner());
            suite.arm_mod_sync_json(&bad);
            assert!(
                suite.mod_sync_body.is_none(),
                "untrusted scheme must not arm"
            );
        }
        // Malformed JSON arms nothing either.
        let worse = dir.join("worse.json");
        std::fs::write(&worse, b"{").unwrap();
        {
            let mut suite = live_mod_suite().lock().unwrap_or_else(|e| e.into_inner());
            suite.arm_mod_sync_json(&worse);
            assert!(suite.mod_sync_body.is_none());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---------- protocol pin ----------

    #[test]
    fn wire_mods_protocol_table_pins_767() {
        assert_eq!(
            mc_spine_core::version::TargetVersion::V1_21_1.protocol(),
            767
        );
        assert_eq!(LIVE_MOD_S2C_CUSTOM_PAYLOAD_ID, 0x19);
    }

    // ---------- mixin lifter sanity through the bridge path ----------

    #[test]
    fn wire_mods_inject_sites_lift_and_redirects_fail_closed() {
        let _serial = WIRE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let jar = lmb_inject_jar(
            "com/safemod",
            "net/minecraft/world/entity/LivingEntity",
            "hurt",
        );
        let report = scan_jar_mixins_with_options(&jar, JarScanOptions::default()).unwrap();
        assert_eq!(report.sites.len(), 1);
        assert_eq!(report.sites[0].annotation, MixinAnnotationKind::Inject);
        assert_eq!(report.sites[0].at, Some(InjectPoint::Head));
        let MixinLiftVerdict::FullyLiftable(bindings) = &report.verdict else {
            panic!("inject site must lift, got {:?}", report.verdict);
        };
        assert_eq!(bindings[0].hook, StandardEventHook::EntityDamagePre);

        let jar = lmb_redirect_jar(
            "com/redmod",
            "net/minecraft/world/entity/LivingEntity",
            "hurt",
        );
        let report = scan_jar_mixins_with_options(&jar, JarScanOptions::default()).unwrap();
        let MixinLiftVerdict::RequiresFallback { unliftable, .. } = &report.verdict else {
            panic!(
                "redirect site must stay unliftable, got {:?}",
                report.verdict
            );
        };
        assert_eq!(unliftable.len(), 1);
    }

    #[test]
    fn w_mod_live_mixed_loader_boot_and_manifest_diagnostics() {
        let _serial = WIRE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        fresh_suite();
        let dir = lmb_temp_dir("dual-loader");
        let fabric = br#"{"schemaVersion":1,"id":"fabricharm","version":"1.0.0",
            "environment":"*","depends":{"mylibrary":">=1.0"}}"#;
        let neoforge = br#"modLoader="javafml"
loaderVersion="[4.0.39]"
license="MIT"
[[mods]]
modId="neoharm"
version="2.0.0"
"#;
        let duplicate = br#"modLoader="javafml"
loaderVersion="[4.0.39]"
license="MIT"
[[mods]]
modId="fabricharm"
version="3.0.0"
"#;
        std::fs::write(
            dir.join("fabricharm-1.jar"),
            lmb_test_jar(&[("fabric.mod.json", fabric)]),
        )
        .unwrap();
        std::fs::write(
            dir.join("neoharm-2.jar"),
            lmb_test_jar(&[("META-INF/neoforge.mods.toml", neoforge)]),
        )
        .unwrap();
        std::fs::write(
            dir.join("z-fabricharm-3.jar"),
            lmb_test_jar(&[("META-INF/neoforge.mods.toml", duplicate)]),
        )
        .unwrap();

        let mut suite = LiveModSuite::new();
        suite.scan_mods_dir(&dir);
        assert_eq!(
            suite.dual_loader.unified_mod_ids(),
            ["fabricharm", "neoharm"]
        );
        assert_eq!(suite.dual_loader.duplicate_conflicts.len(), 1);
        assert_eq!(
            suite.dual_loader.duplicate_conflicts[0].mod_id,
            "fabricharm"
        );
        assert_eq!(suite.dual_loader.missing_dependencies.len(), 1);
        assert_eq!(
            suite.dual_loader.missing_dependencies[0].required_mod_id,
            "mylibrary"
        );
        assert_eq!(suite.reports.len(), 3);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn w_mod_live_cafebabe_damage_hook_executes_without_jvm() {
        use mod_adapter_core::bytecode_subset_vm::testkit::{ClassAssembler, MethodSpec};

        let _serial = WIRE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        fresh_suite();
        let mut asm = ClassAssembler::new("example/ThresholdDamage");
        let get = asm.cp.method_ref("mcspine/api/Event", "getDamage", "()F");
        let set = asm.cp.method_ref("mcspine/api/Event", "setDamage", "(F)V");
        let ten = asm.cp.float(10.0);
        let half = asm.cp.float(0.5);
        let code = vec![
            0x2a,
            0xb6,
            (get >> 8) as u8,
            get as u8,
            0x12,
            ten as u8,
            0x95,
            0x9e,
            0,
            14,
            0x2a,
            0x2a,
            0xb6,
            (get >> 8) as u8,
            get as u8,
            0x12,
            half as u8,
            0x6a,
            0xb6,
            (set >> 8) as u8,
            set as u8,
            0xb1,
        ];
        let class = asm.finish(&[MethodSpec::new(
            "onDamage",
            "(Lmcspine/api/Event;)V",
            4,
            1,
            code,
        )]);
        assert_eq!(&class[..4], &[0xca, 0xfe, 0xba, 0xbe]);
        let dir = lmb_temp_dir("damage-class");
        let manifest = br#"{"schemaVersion":1,"id":"damagehook","version":"1.0.0"}"#;
        std::fs::write(
            dir.join("damagehook-1.jar"),
            lmb_test_jar(&[
                ("fabric.mod.json", manifest),
                ("example/ThresholdDamage.class", &class),
            ]),
        )
        .unwrap();
        let mut suite = LiveModSuite::new();
        suite.scan_mods_dir(&dir);
        assert_eq!(suite.damage_hooks.len(), 1);
        suite.active = true;
        *live_mod_suite().lock().unwrap_or_else(|e| e.into_inner()) = suite;
        assert_eq!(live_mod_eval_damage_hooks(14.0), 7.0);
        assert_eq!(live_mod_eval_damage_hooks(6.0), 6.0);
        assert!(live_mod_eval_damage_hooks(f32::NAN).is_nan());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn w_mod_live_pot_sleep_completion_and_unknown_addon_cutting() {
        let _serial = WIRE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        fresh_suite();
        let dir = lmb_temp_dir("pot-cutting");
        std::fs::write(
            dir.join("farmersdelight.jar"),
            lmb_culinary_jar("farmersdelight"),
        )
        .unwrap();
        let addon_manifest = br#"{"schemaVersion":1,"id":"unknownaddon","version":"1.0.0"}"#;
        let cutting = br#"{"type":"unknownaddon:cutting",
            "ingredients":[{"item":"minecraft:carrot"}],
            "tool":{"item":"unknownaddon:knife"},
            "result":[{"item":"unknownaddon:carrot_slices","count":3}]}"#;
        std::fs::write(
            dir.join("unknownaddon.jar"),
            lmb_test_jar(&[
                ("fabric.mod.json", addon_manifest),
                ("data/unknownaddon/recipes/carrot_slices.json", cutting),
            ]),
        )
        .unwrap();
        let mut suite = LiveModSuite::new();
        suite.scan_mods_dir(&dir);
        assert!(suite.culinary_stats.cutting_recipes >= 2);
        suite.active = true;
        *live_mod_suite().lock().unwrap_or_else(|e| e.into_inner()) = suite;

        let key = (DimId::Overworld, 10, 64, -3);
        let mut pot = CookingPotState::new();
        assert!(pot.insert_input("minecraft:carrot", 1));
        pot.container = Some(("minecraft:bowl".into(), 1));
        live_mod_set_cooking_pot(key.0, key.1, key.2, key.3, pot.clone(), 100);
        {
            let suite = live_mod_suite().lock().unwrap_or_else(|e| e.into_inner());
            assert!(suite.cooking_pots[&key].is_sleeping());
            assert!(suite.cooking_deadlines.is_empty());
        }
        assert!(live_mod_tick_cooking_pots(100_000).is_empty());
        pot.heated = true;
        live_mod_set_cooking_pot(key.0, key.1, key.2, key.3, pot, 100_000);
        assert!(live_mod_tick_cooking_pots(100_199).is_empty());
        let completed = live_mod_tick_cooking_pots(100_200);
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].0, key);
        assert_eq!(completed[0].1.result_item, "farmersdelight:test_soup");
        assert!(live_mod_tick_cooking_pots(100_201).is_empty());
        let cut = live_mod_cutting_board_use("minecraft:carrot", "unknownaddon:knife", 42).unwrap();
        assert_eq!(cut.outputs, [("unknownaddon:carrot_slices".to_string(), 3)]);
        live_mod_remove_cooking_pot(key.0, key.1, key.2, key.3);
        assert!(live_mod_suite()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .cooking_pots
            .is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn w_mod_live_accessory_aggregation_and_msh1_probe() {
        use mod_adapter_core::ecosystem_compat::{AttributeModifier, ModifierOperation};

        let _serial = WIRE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        fresh_suite();
        let attr = "minecraft:generic.armor";
        let ring = AccessoryItem::new("example:ring", ["ring"]).with_modifiers(vec![
            AttributeModifier {
                attribute: attr.into(),
                operation: ModifierOperation::AddValue,
                amount: 2.0,
            },
            AttributeModifier {
                attribute: attr.into(),
                operation: ModifierOperation::AddMultipliedBase,
                amount: 0.1,
            },
            AttributeModifier {
                attribute: attr.into(),
                operation: ModifierOperation::AddMultipliedTotal,
                amount: 0.25,
            },
        ]);
        live_mod_equip_accessory("alice", "ring", 0, ring.clone()).unwrap();
        assert!(matches!(
            live_mod_equip_accessory("alice", "ring", 0, ring.clone()),
            Err(AccessoryError::SlotOccupied { .. })
        ));
        assert!(live_mod_accessory_attributes("bob").is_empty());
        let totals = live_mod_accessory_attributes("alice");
        assert_eq!(totals[attr].add_value, 2.0);
        assert_eq!(totals[attr].add_multiplied_base, 0.1);
        assert_eq!(totals[attr].add_multiplied_total, 0.25);
        assert_eq!(totals[attr].apply(20.0), 30.0);
        live_mod_equip_accessory("bob", "ring", 1, ring).unwrap();
        assert_eq!(live_mod_accessory_attributes("bob")[attr].apply(20.0), 30.0);

        assert!(live_mod_sync_root_hash_probe().is_none());
        let dir = lmb_temp_dir("msh1");
        let file = dir.join("manifest.json");
        let json = format!(
            r#"{{"entries":[{{"mod_id":"jei","file_name":"jei.jar",
            "download_url":"https://cdn.modrinth.com/data/jei/jei.jar",
            "sha256_hex":"{}","size_bytes":12345}}]}}"#,
            "a".repeat(64),
        );
        std::fs::write(&file, json).unwrap();
        live_mod_suite()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .arm_mod_sync_json(&file);
        let probe = live_mod_sync_root_hash_probe().expect("armed MSH1 probe");
        assert_eq!(probe.len(), 36);
        assert_eq!(&probe[..4], b"MSH1");
        assert_eq!(
            lmb_sync::decode_root_hash_probe(&probe).unwrap(),
            probe[4..]
        );
        assert!(live_mod_sync_manifest_for_client_probe(Some(&probe)).is_none());
        let mut mismatch = probe;
        mismatch[10] ^= 1;
        assert!(live_mod_sync_manifest_for_client_probe(Some(&mismatch))
            .unwrap()
            .starts_with(b"MSY1"));
        assert!(live_mod_sync_manifest_for_client_probe(None).is_some());
        let _ = std::fs::remove_dir_all(dir);
    }
}
