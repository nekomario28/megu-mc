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
//   * `live_mod_suite_boot` — startup `mods/` scan over `MEGU_MODS_DIR`
//     (set in `main`, before the listener binds) delegated 1:1 to
//     `mod_adapter_core::auto_adapter::auto_adapt_mods_dir` — the same
//     engine `megu-mod-check --adapt-mods-dir` runs: renamed
//     `BUILTIN_SUPERSEDED_PERF_MODS` entries are skipped before any
//     bytecode read on their manifest-declared ids, loader metadata,
//     data/culinary/dimension discovery, `.class` Mixin lifting, and
//     `megu-mod-bundle/1` synthesis all come from the adapter. Its
//     emitted bundles register into `live_mod_unified_runtime()` next to
//     standalone `*.bundle.json` files, the culinary registry feeds every
//     discovered JAR (Farmer's Delight + addon `*:cooking` /
//     `*:fermenting` / `*:cutting` recipes, item/block tags, `*:add_item`
//     / `*:add_table` loot modifiers, food specs), and
//     `live_mod_dispatch_hook` answers which lifted `LiftedBinding`s a
//     standard event fires immediately after boot.
//   * `live_mod_appleskin_sync` — per-tick `appleskin:exhaustion_sync` /
//     `appleskin:saturation_sync` `CustomPacketPayload` emit on the play
//     S2C `0x19` id (verified against `proto/src/data_v767.rs`'s
//     `(Play, Clientbound, "custom_payload", 0x19)` entry), delta-suppressed
//     per player at the adapter's `APPLESKIN_DELTA_EPSILON`.
//   * `live_check_progression_barrier` + `live_progression_denies_break` —
//     declarative dimension/biome progression gates (Twilight-Forest-style)
//     evaluated against a player's completed advancements; armed rules are
//     loaded from `MEGU_PROGRESSION` JSON, and `live_progression_grant`
//     covers advancement ids the vanilla tree cannot represent.
//   * `live_mod_sync_on_join` — serves the `megu:mod_sync_v1` (`MSY1`)
//     `CustomPacketPayload` to joining clients when an operator manifest
//     (`MEGU_MOD_SYNC` JSON, `MEGU_MOD_SYNC_MIRRORS` allowlist)
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

// Performance governors are opt-in and retain one bounded cache and crowd
// density map for the server. Callers gate only idle AI with the crowd result;
// movement, gravity, combat and damage remain on the regular tick path.
#[cfg(feature = "mod-suite")]
struct LiveServerPerfBridge {
    velocity: megu_core::server_perf_governors::VelocityViewGovernor,
    crowd: megu_core::server_perf_governors::CrowdTickGuard,
    chunks: megu_core::server_perf_governors::CompressedChunkPacketCache,
}

#[cfg(feature = "mod-suite")]
fn live_server_perf_bridge() -> &'static Mutex<LiveServerPerfBridge> {
    static BRIDGE: OnceLock<Mutex<LiveServerPerfBridge>> = OnceLock::new();
    BRIDGE.get_or_init(|| {
        Mutex::new(LiveServerPerfBridge {
            velocity: megu_core::server_perf_governors::VelocityViewGovernor::new(10),
            crowd: megu_core::server_perf_governors::CrowdTickGuard::default(),
            chunks: megu_core::server_perf_governors::CompressedChunkPacketCache::new(1024),
        })
    })
}

/// Select the current tick's outgoing chunks, keeping forward Elytra reach.
#[cfg(feature = "mod-suite")]
#[allow(dead_code, clippy::too_many_arguments)]
fn live_plan_velocity_chunks(
    view_distance: u8,
    player_chunk_x: i32,
    player_chunk_z: i32,
    velocity_x: f32,
    velocity_z: f32,
    load_scale: f32,
    max_chunks_per_tick: usize,
) -> Vec<(i32, i32)> {
    let mut bridge = live_server_perf_bridge()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    bridge.velocity.set_view_distance(view_distance);
    bridge.velocity.plan_chunk_sends(
        player_chunk_x,
        player_chunk_z,
        velocity_x,
        velocity_z,
        load_scale,
        max_chunks_per_tick,
    )
}

/// Only idle AI may be deferred. Physics, damage and combat always run.
#[cfg(feature = "mod-suite")]
#[allow(dead_code)]
fn live_should_tick_crowd_mob_ai(
    entity_id: i64,
    chunk_x: i32,
    chunk_z: i32,
    tick: u64,
    chunk_entity_count: u32,
    is_critical: bool,
) -> bool {
    use megu_core::server_perf_governors::EntityTickClass;

    let mut bridge = live_server_perf_bridge()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    bridge
        .crowd
        .set_chunk_entity_count(chunk_x, chunk_z, chunk_entity_count);
    let class = if is_critical {
        EntityTickClass::Critical
    } else {
        EntityTickClass::CrowdMob
    };
    let decision = bridge.crowd.decide(class, entity_id, chunk_x, chunk_z, tick);
    debug_assert!(decision.run_physics_and_damage());
    decision.run_idle_ai()
}

/// Cache the compressed chunk payload across recipients at the same epoch.
#[cfg(feature = "mod-suite")]
#[allow(dead_code)]
fn live_chunk_packet_cache_get_or_encode<F>(
    dim: i32,
    cx: i32,
    cz: i32,
    dirty_epoch: u64,
    antixray_profile: u8,
    encode_fn: F,
) -> Arc<[u8]>
where
    F: FnOnce() -> Vec<u8>,
{
    use megu_core::server_perf_governors::ChunkPacketCacheKey;

    live_server_perf_bridge()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .chunks
        .get_or_encode(
            ChunkPacketCacheKey {
                dim,
                cx,
                cz,
                dirty_epoch,
                antixray_profile,
            },
            encode_fn,
        )
}

#[cfg(feature = "mod-suite")]
#[allow(dead_code)]
fn live_chunk_packet_cache_invalidate(dim: i32, cx: i32, cz: i32) -> usize {
    live_server_perf_bridge()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .chunks
        .invalidate_chunk(dim, cx, cz)
}

#[cfg(not(feature = "mod-suite"))]
#[allow(dead_code, clippy::too_many_arguments)]
fn live_plan_velocity_chunks(
    _view_distance: u8,
    _player_chunk_x: i32,
    _player_chunk_z: i32,
    _velocity_x: f32,
    _velocity_z: f32,
    _load_scale: f32,
    _max_chunks_per_tick: usize,
) -> Vec<(i32, i32)> {
    Vec::new()
}

#[cfg(not(feature = "mod-suite"))]
#[allow(dead_code)]
fn live_should_tick_crowd_mob_ai(
    _entity_id: i64,
    _chunk_x: i32,
    _chunk_z: i32,
    _tick: u64,
    _chunk_entity_count: u32,
    _is_critical: bool,
) -> bool {
    true
}

#[cfg(not(feature = "mod-suite"))]
#[allow(dead_code)]
fn live_chunk_packet_cache_get_or_encode<F>(
    _dim: i32,
    _cx: i32,
    _cz: i32,
    _dirty_epoch: u64,
    _antixray_profile: u8,
    encode_fn: F,
) -> Arc<[u8]>
where
    F: FnOnce() -> Vec<u8>,
{
    Arc::from(encode_fn())
}

#[cfg(not(feature = "mod-suite"))]
#[allow(dead_code)]
fn live_chunk_packet_cache_invalidate(_dim: i32, _cx: i32, _cz: i32) -> usize {
    0
}

// ===========================================================================
// mod-suite implementation
// ===========================================================================

#[cfg(feature = "mod-suite")]
use mod_adapter_core::auto_adapter::{
    auto_adapt_jar, auto_adapt_mods_dir, AdaptationTier, AutoAdaptOptions, JarAdaptationResult,
    ModsDirAdaptationReport,
};
#[cfg(feature = "mod-suite")]
use mod_adapter_core::bytecode_subset_vm::{
    execute_verified_method, parse_class_methods, verify_subset_method, HostCallKind, SubsetVmHost,
    VerifiedMethod, VmError, VmValue, DEFAULT_FUEL,
};
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
use mod_adapter_core::live_registry_bridge::{
    DynamicBlockStateEntry, RegistryContentIndex, UnifiedModRegistryPlan,
    MOD_REGISTRY_SYNC_CHANNEL,
};
#[cfg(feature = "mod-suite")]
use mod_adapter_core::magic_primitives as lmb_magic;
#[cfg(feature = "mod-suite")]
use mod_adapter_core::mixin_lifter::{LiftedBinding, MixinLiftVerdict, StandardEventHook};
#[cfg(feature = "mod-suite")]
use mod_adapter_core::mod_sync as lmb_sync;
#[cfg(feature = "mod-suite")]
use mod_adapter_core::season_primitives::{
    encode_ssn1, seasonal_foliage_tint, seasonal_grass_tint, BiomeClimateClass,
    BiomeClimateRules, SeasonCalendarConfig, SeasonState, SeasonSyncState, SeasonalTintRules,
    SSN1_FRAME_LEN,
};
#[cfg(feature = "mod-suite")]
use mod_adapter_core::unified_bundle::{DeclarativeModBundle, UnifiedModRuntime};
#[cfg(feature = "mod-suite")]
use mod_adapter_core::vanilla_qol_and_holidays::{
    chest_holiday_variant, evaluate_right_click_harvest, roll_halloween_headgear, CalendarDate,
    ChestVisualVariant, HalloweenHeadgearKind, HolidayConfig, QuarkCompatQolConfig,
};
#[cfg(feature = "mod-suite")]
use mod_adapter_core::version_compat::McVersion;
#[cfg(feature = "mod-suite")]
use mod_adapter_core::world_extensions::{
    AppleSkinDeltaTracker, ProgressionBarrierEngine, ProgressionRule, ProgressionVerdict,
    CHANNEL_APPLESKIN_EXHAUSTION, CHANNEL_APPLESKIN_SATURATION,
};

// The bridge speaks protocol 767 (Minecraft 1.21.1) exclusively — the release
// every adapter target pins. `megu_core`'s shared version table is the
// source of truth; if its protocol ever drifts from 767 this fails to compile
// instead of silently serving a wrong-version manifest or QoL payload.
#[cfg(feature = "mod-suite")]
const _: () = assert!(
    megu_core::version::TargetVersion::V1_21_1.protocol() == 767,
    "live mod bridge: protocol table drifted from 767"
);
#[cfg(feature = "mod-suite")]
const LIVE_MOD_MC_TARGET: McVersion = McVersion::V1_21_1;

/// Mod ids the flagship `create-adapter` handles natively for this feature.
#[cfg(feature = "mod-suite")]
fn live_mod_flagship_ids() -> BTreeSet<String> {
    BTreeSet::from(["create".to_string()])
}

/// `MRS1` id bases for generic `MEGU_MODS_DIR` mod entries — deliberately far
/// above the vanilla block-state table (< `DEFAULT_BASE_STATE_ID` = 26_684),
/// Create's contiguous range from that base, and the `LIVE_CREATE_ITEM_BASE`
/// item ids, so no two registries merged into one frame can alias an id.
#[cfg(feature = "mod-suite")]
const LIVE_MRS_GENERIC_BLOCK_BASE: u32 = 1_000_000;
#[cfg(feature = "mod-suite")]
const LIVE_MRS_GENERIC_ITEM_BASE: u32 = 2_000_000;

#[cfg(all(test, feature = "mod-suite"))]
mod live_server_perf_bridge_tests {
    use super::*;

    #[test]
    fn live_perf_bridge_elytra_cone_retains_forward_distance() {
        let planned = live_plan_velocity_chunks(12, 0, 0, 1.4, 0.0, 0.35, 1024);
        assert!(planned.contains(&(12, 0)));
        assert!(!planned.contains(&(-12, 0)));
        assert!(planned.len() <= 1024);
    }

    #[test]
    fn live_perf_bridge_crowd_ai_strides_without_skipping_physics() {
        use megu_core::server_perf_governors::{EntityTickClass, TickDecision};

        let entity_id = 203;
        let (cx, cz) = (900, 901);
        let ticks: Vec<bool> = (0..15)
            .map(|tick| live_should_tick_crowd_mob_ai(entity_id, cx, cz, tick, 40, false))
            .collect();
        assert_eq!(ticks.iter().filter(|&&t| t).count(), 5);
        assert!(live_should_tick_crowd_mob_ai(entity_id, cx, cz, 0, 40, true));
        assert_eq!(
            live_server_perf_bridge()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .crowd
                .decide(EntityTickClass::CrowdMob, entity_id, cx, cz, 0),
            TickDecision::PhysicsOnly
        );
        assert!(TickDecision::PhysicsOnly.run_physics_and_damage());
    }

    #[test]
    fn live_perf_bridge_chunk_cache_shares_and_invalidates_epochs() {
        let _guard = crate::world_guard();
        let (dim, cx, cz) = (300, 910, 920);
        let first = live_chunk_packet_cache_get_or_encode(dim, cx, cz, 1, 0, || vec![1, 2, 3]);
        let shared = live_chunk_packet_cache_get_or_encode(dim, cx, cz, 1, 0, || {
            panic!("cache hit must not encode again")
        });
        assert!(Arc::ptr_eq(&first, &shared));
        let next_epoch = live_chunk_packet_cache_get_or_encode(dim, cx, cz, 2, 0, || vec![4, 5]);
        assert_eq!(&*next_epoch, &[4, 5]);
        assert_eq!(live_chunk_packet_cache_invalidate(dim, cx, cz), 2);
        let reencoded =
            live_chunk_packet_cache_get_or_encode(dim, cx, cz, 1, 0, || vec![6, 7]);
        assert_eq!(&*reencoded, &[6, 7]);
        assert!(!Arc::ptr_eq(&first, &reencoded));
    }
}

/// Audit record for one scanned mod JAR: the `auto_adapter` verdict and the
/// plain-language diagnostic the startup report prints.
#[cfg(feature = "mod-suite")]
#[derive(Clone, Debug)]
struct LiveModJarReport {
    file_name: String,
    mod_id: String,
    tier: AdaptationTier,
    diagnostic: String,
    classes_scanned: usize,
    mixin_sites: usize,
    skipped_client_mixins: usize,
}

/// What was automatically onboarded from a JAR. Lifted bindings are installed
/// only for a fully verified [`AdaptationTier::Tier2LiftedShim`]
/// classification; Tier 3 is diagnostic and auto-skipped JARs record no
/// summary at all.
#[cfg(feature = "mod-suite")]
#[allow(dead_code)]
#[derive(Clone, Debug)]
struct AutoAdaptedModSummary {
    file_name: String,
    mod_id: String,
    tier: AdaptationTier,
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
    /// Last successfully sent SSN1 frame, scoped to the player's session.
    season_sync: HashMap<i32, ([u8; 16], [u8; SSN1_FRAME_LEN])>,
    /// Encoded `MSY1` body for `megu:mod_sync_v1`, validated at arm time.
    mod_sync_body: Option<Vec<u8>>,
    mod_sync_root_hash: Option<[u8; 32]>,
    /// Deterministic `MRS1` registry plan over every successfully adapted
    /// JAR's declared blockstates/items, built once by `scan_mods_dir`.
    /// `None` when no `mods/` scan ran or the plan rejected the content.
    registry_plan: Option<UnifiedModRegistryPlan>,
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
            season_sync: HashMap::new(),
            mod_sync_body: None,
            mod_sync_root_hash: None,
            registry_plan: None,
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

    /// Scan one `mods/` directory. JAR classification is delegated 1:1 to
    /// [`auto_adapt_mods_dir`] — the single source of truth shared with
    /// `megu-mod-check --adapt-mods-dir`: filename- and
    /// manifest-declared superseded-perf-mod skips, loader metadata,
    /// data/culinary/dimension discovery, `.class` Mixin lifting, and
    /// `megu-mod-bundle/1` synthesis all happen inside the adapter. The
    /// bridge folds each [`JarAdaptationResult`] back into the live suite:
    /// emitted bundles join the unified runtime, culinary stats aggregate
    /// for the boot report, and fully lifted Tier 2 bindings arm
    /// `live_mod_dispatch_hook`.
    fn scan_mods_dir(&mut self, dir: &std::path::Path) {
        if !dir.is_dir() {
            eprintln!(
                "[live-mods] MEGU_MODS_DIR {} is not a directory — mod scan skipped",
                dir.display()
            );
            return;
        }
        self.scan_bundles_dir(dir);
        let jars = Self::jar_listing(dir);
        let options = AutoAdaptOptions {
            target: LIVE_MOD_MC_TARGET,
            flagship_mod_ids: live_mod_flagship_ids(),
            ..AutoAdaptOptions::default()
        };
        // `auto_adapt_mods_dir` is fail-closed over the whole directory: one
        // hostile or corrupt JAR aborts the pass. Boot must not lose every
        // other mod to a single bad entry, so on `Err` the same engine runs
        // per JAR (`auto_adapt_jar`) and failures land as Tier 3 rows —
        // never silently dropped, never promoted.
        let adapted: Vec<(String, Result<JarAdaptationResult, String>)> =
            match auto_adapt_mods_dir(dir, &options) {
                Ok(report) => {
                    for note in &report.audit_notes {
                        eprintln!("[live-mods] auto-adapt: {note}");
                    }
                    report
                        .jars
                        .into_iter()
                        .map(|result| (result.jar.clone(), Ok(result)))
                        .collect()
                }
                Err(error) => {
                    eprintln!(
                        "[live-mods] auto-adaptation of {} failed ({error}); retrying per JAR",
                        dir.display()
                    );
                    jars.iter()
                        .map(|(file_name, path)| {
                            (file_name.clone(), auto_adapt_jar(path, &options))
                        })
                        .collect()
                }
            };
        // JARs the adapter culinary-scanned get their content merged into
        // the live registry below — the report carries stats only.
        let culinary_live: BTreeSet<&str> = adapted
            .iter()
            .filter_map(|(name, result)| {
                result
                    .as_ref()
                    .ok()
                    .filter(|r| r.culinary.is_some())
                    .map(|_| name.as_str())
            })
            .collect();
        for (file_name, result) in &adapted {
            match result {
                Ok(result) => self.adopt_jar_result(file_name, result),
                Err(error) => {
                    eprintln!("[live-mods] {file_name}: {error}");
                    let mod_id = live_mod_file_stem(file_name);
                    self.auto_adapted.push(AutoAdaptedModSummary {
                        file_name: file_name.clone(),
                        mod_id: mod_id.clone(),
                        tier: AdaptationTier::Tier3FallbackRequired,
                        lifted_hooks: Vec::new(),
                        cooking_recipes: 0,
                        cutting_recipes: 0,
                        tag_files: 0,
                        tag_values: 0,
                    });
                    self.reports.push(LiveModJarReport {
                        file_name: file_name.clone(),
                        mod_id,
                        tier: AdaptationTier::Tier3FallbackRequired,
                        diagnostic: error.clone(),
                        classes_scanned: 0,
                        mixin_sites: 0,
                        skipped_client_mixins: 0,
                    });
                }
            }
        }
        // MRS1 (`megu:mod_registry_sync_v1`): fold every successfully adapted
        // JAR's declared blockstates/items into one deterministic registry
        // plan so joining clients learn the dynamic ids up front. A plan
        // rejection (conflicting or oversized content) drops only the generic
        // entries — Create's own ids are merged per-join instead.
        let registry_jars: Vec<JarAdaptationResult> = adapted
            .iter()
            .filter_map(|(_, result)| result.as_ref().ok().cloned())
            .collect();
        if !registry_jars.is_empty() {
            let report = ModsDirAdaptationReport {
                dir: dir.display().to_string(),
                jars: registry_jars,
                non_jar_entries: 0,
                audit_notes: Vec::new(),
            };
            match UnifiedModRegistryPlan::from_auto_adapt(
                LIVE_MRS_GENERIC_BLOCK_BASE,
                LIVE_MRS_GENERIC_ITEM_BASE,
                &report,
            ) {
                Ok(plan) => self.registry_plan = Some(plan),
                Err(error) => {
                    eprintln!("[live-mods] MRS1 registry plan rejected: {error:?}")
                }
            }
        }
        let mut bounded_jars = Vec::new();
        for (file_name, path) in &jars {
            if let Ok(metadata) = std::fs::metadata(path) {
                if metadata.len() <= lmb_sync::MAX_MOD_JAR_BYTES {
                    if let Ok(bytes) = std::fs::read(path) {
                        if culinary_live.contains(file_name.as_str()) {
                            if let Err(error) = self.culinary.load_from_jar_bytes(&bytes) {
                                eprintln!(
                                    "[live-mods] {file_name}: culinary scan skipped: {error:?}"
                                );
                            }
                        }
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
                    && path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|name| name.ends_with(".bundle.json"))
                {
                    root_paths.push(path);
                }
            }
            root_paths.sort();
            paths.extend(root_paths);
        }
        for path in paths {
            let parsed = (|| -> Result<DeclarativeModBundle, String> {
                let metadata = std::fs::metadata(&path).map_err(|e| e.to_string())?;
                if metadata.len() > MAX_BUNDLE_BYTES {
                    return Err("bundle exceeds 4 MiB limit".to_string());
                }
                let json = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
                DeclarativeModBundle::from_json(&json).map_err(|e| e.to_string())
            })();
            match parsed {
                Ok(bundle) => self.install_bundle(&bundle, &path.display().to_string()),
                Err(error) => {
                    eprintln!("[live-mods] bundle {} rejected: {error}", path.display())
                }
            }
        }
        self.barriers = self.unified.progression_engine();
    }

    /// Install one validated `megu-mod-bundle/1` document into the
    /// unified runtime — the single registration path shared by standalone
    /// `*.bundle.json` files and bundles the auto-adapter synthesized for
    /// native JARs. First mod id wins (an operator's standalone bundle
    /// beats a synthesized minimal one), barriers refresh on success.
    fn install_bundle(&mut self, bundle: &DeclarativeModBundle, origin: &str) {
        if self
            .unified
            .installed_mod_ids()
            .iter()
            .any(|id| id == &bundle.mod_id)
        {
            eprintln!(
                "[live-mods] bundle {origin}: duplicate mod id {} — skipped",
                bundle.mod_id
            );
            return;
        }
        match self.unified.register_bundle(bundle, LIVE_MOD_MC_TARGET) {
            Ok(receipt) => {
                eprintln!(
                    "[live-mods] native bundle {} ({origin}): {} portals, {} progression barriers, {} regions, {} creatures, {} channels",
                    receipt.mod_id, receipt.portals_installed, receipt.progression_barriers_installed,
                    receipt.biome_regions_installed, receipt.creatures_installed,
                    receipt.custom_channels_declared,
                );
                self.barriers = self.unified.progression_engine();
            }
            Err(error) => eprintln!("[live-mods] bundle {origin} rejected: {error}"),
        }
    }

    /// Fold one [`JarAdaptationResult`] into the live suite: install the
    /// emitted bundle, aggregate the culinary stats, arm the lifted hook
    /// bindings, and record the audit/summary rows the boot report reads.
    /// Auto-skipped JARs report their skip but onboard nothing — exactly
    /// what a superseded perf mod must do.
    fn adopt_jar_result(&mut self, file_name: &str, result: &JarAdaptationResult) {
        if let Some(bundle) = &result.bundle {
            self.install_bundle(bundle, file_name);
        }
        if let Some(stats) = &result.culinary {
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
        let mod_id = result
            .mod_id()
            .map(str::to_string)
            .unwrap_or_else(|| live_mod_file_stem(file_name));
        let (classes_scanned, mixin_sites, skipped_client_mixins) = result
            .mixin_scan
            .as_ref()
            .map(|scan| {
                (
                    scan.classes_scanned,
                    scan.sites.len(),
                    scan.skipped_client_mixins,
                )
            })
            .unwrap_or_default();
        let lifted_hooks = if result.tier == AdaptationTier::Tier2LiftedShim {
            result
                .mixin_scan
                .as_ref()
                .map(|scan| live_lifted_bindings(&scan.verdict).to_vec())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        self.lifted_hooks.extend(
            lifted_hooks
                .iter()
                .cloned()
                .map(|hook| (mod_id.clone(), hook)),
        );
        if result.tier != AdaptationTier::AutoSkipped {
            self.auto_adapted.push(AutoAdaptedModSummary {
                file_name: file_name.to_string(),
                mod_id: mod_id.clone(),
                tier: result.tier,
                lifted_hooks,
                cooking_recipes: result
                    .culinary
                    .as_ref()
                    .map(|s| s.cooking_recipes)
                    .unwrap_or_default(),
                cutting_recipes: result
                    .culinary
                    .as_ref()
                    .map(|s| s.cutting_recipes)
                    .unwrap_or_default(),
                tag_files: result
                    .culinary
                    .as_ref()
                    .map(|s| s.tag_files)
                    .unwrap_or_default(),
                tag_values: result
                    .culinary
                    .as_ref()
                    .map(|s| s.tag_values)
                    .unwrap_or_default(),
            });
        }
        self.reports.push(LiveModJarReport {
            file_name: file_name.to_string(),
            mod_id,
            tier: result.tier,
            diagnostic: if result.blockers.is_empty() {
                result.audit_notes.join("; ")
            } else {
                result.blockers.join("; ")
            },
            classes_scanned,
            mixin_sites,
            skipped_client_mixins,
        });
    }

    /// Install only an explicitly named damage entrypoint after full static
    /// verification. The archive scanner enforces ZIP and decoded-size bounds.
    fn load_damage_hooks(&mut self, jar_name: &str, jar: &[u8]) {
        const DAMAGE_DESCRIPTOR: &str = "(Lmegu/api/Event;)V";
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

    /// Startup report: one line per adaptation tier, then per-mod detail
    /// for anything needing operator attention (lifted and unliftable
    /// alike).
    fn log_boot_report(&self, jars_seen: usize) {
        if self.reports.is_empty() {
            eprintln!("[live-mods] mods dir scanned: {jars_seen} jar(s), none classified");
            return;
        }
        let count = |tier: AdaptationTier| self.reports.iter().filter(|r| r.tier == tier).count();
        eprintln!(
            "[live-mods] {jars_seen} jar(s): {} auto-skipped, {} native-bundle, {} tier2-shim, {} tier3",
            count(AdaptationTier::AutoSkipped),
            count(AdaptationTier::NativeBundle),
            count(AdaptationTier::Tier2LiftedShim),
            count(AdaptationTier::Tier3FallbackRequired),
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
                AdaptationTier::Tier3FallbackRequired => eprintln!(
                    "[live-mods] {} ({}) tier3: {} classes, {} site(s){} — {}",
                    report.file_name,
                    report.mod_id,
                    report.classes_scanned,
                    report.mixin_sites,
                    skipped,
                    report.diagnostic
                ),
                AdaptationTier::Tier2LiftedShim => eprintln!(
                    "[live-mods] {} ({}) tier2 shim: {} classes, {} site(s){} — {}",
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

    /// Arm the `megu:mod_sync_v1` manifest from an operator JSON file:
    /// `{"entries":[{"mod_id","file_name","download_url","sha256_hex",
    /// "size_bytes"}]}`. Every entry must pass `mod_sync`'s structural +
    /// trusted-CDN validation (plus `MEGU_MOD_SYNC_MIRRORS` hosts) before
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
        let policy = match std::env::var("MEGU_MOD_SYNC_MIRRORS") {
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

    /// Load declarative progression rules from `MEGU_PROGRESSION` JSON:
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

/// The dispatch/query bridge over the lifted [`LiftedBinding`]s recorded
/// at boot: every `(mod_id, binding)` pair subscribed to `hook`, in boot
/// scan order. Callers fire a standard event by dispatching the returned
/// bindings; an inactive suite (or a boot that never ran) answers empty —
/// no manual step is needed between boot and the first query.
#[cfg(feature = "mod-suite")]
#[allow(dead_code)]
fn live_mod_dispatch_hook(hook: StandardEventHook) -> Vec<(String, LiftedBinding)> {
    let suite = live_mod_suite().lock().unwrap_or_else(|e| e.into_inner());
    if !suite.active {
        return Vec::new();
    }
    suite
        .lifted_hooks
        .iter()
        .filter(|(_, binding)| binding.hook == hook)
        .cloned()
        .collect()
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

/// The lifted bindings a [`MixinLiftVerdict`] carries — every binding when
/// fully liftable, the diagnostic `lifted` prefix otherwise. Mirrors the
/// adapter's internal selector; `auto_adapter` only promotes to Tier 2
/// when this slice covers every site.
#[cfg(feature = "mod-suite")]
fn live_lifted_bindings(verdict: &MixinLiftVerdict) -> &[LiftedBinding] {
    match verdict {
        MixinLiftVerdict::FullyLiftable(bindings) => bindings,
        MixinLiftVerdict::RequiresFallback { lifted, .. } => lifted,
    }
}

/// Filename-stem fallback for a JAR the adapter reported without a mod id:
/// the bare name lowercased, the same shape the old inferred-id fallback
/// produced.
#[cfg(feature = "mod-suite")]
fn live_mod_file_stem(file_name: &str) -> String {
    file_name
        .rsplit_once('.')
        .map(|(stem, _)| stem)
        .unwrap_or(file_name)
        .to_lowercase()
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

/// The season payload channel requested by the server bridge's wire contract.
#[cfg(feature = "mod-suite")]
const LIVE_MOD_SEASON_CHANNEL: &str = "megu:season";

/// Assemble the deterministic SSN1 season snapshot for the selected biome
/// temperature. Tints use the representative plains biome because the bridge
/// currently receives only the temperature, rather than a biome identifier.
#[cfg(feature = "mod-suite")]
#[allow(dead_code)]
fn live_mod_season_sync_frame(world_ticks: u64, biome_temp: f32) -> Option<(String, Vec<u8>)> {
    if !biome_temp.is_finite() {
        return None;
    }
    let state = SeasonState::from_world_ticks(world_ticks, &SeasonCalendarConfig::default());
    let climate = BiomeClimateRules::default().classify_biome("minecraft:plains", biome_temp);
    let tints = SeasonalTintRules::default();
    let foliage = seasonal_foliage_tint(&state, climate, "minecraft:plains", &tints);
    let grass = seasonal_grass_tint(&state, climate, &tints);
    let tropical = matches!(climate, BiomeClimateClass::Tropical);
    let wire = SeasonSyncState::from_state(&state, foliage, grass, tropical, false, !tropical);
    Some((LIVE_MOD_SEASON_CHANNEL.to_string(), encode_ssn1(&wire)))
}

/// Send a new SSN1 snapshot as the protocol-767 S2C custom payload (0x19).
/// Per-eid caches additionally pin the UUID to prevent a reused eid from
/// inheriting the previous session's frame.
#[cfg(feature = "mod-suite")]
#[allow(dead_code)]
fn live_mod_emit_season_sync(cid: u64, world_ticks: u64, biome_temp: f32) -> bool {
    let Ok(eid) = i32::try_from(cid) else {
        return false;
    };
    let Some((channel, payload)) = live_mod_season_sync_frame(world_ticks, biome_temp) else {
        return false;
    };
    let Ok(frame): Result<[u8; SSN1_FRAME_LEN], _> = payload.as_slice().try_into() else {
        return false;
    };
    let client = {
        let players = clients().lock().unwrap_or_else(|e| e.into_inner());
        players.iter().find(|player| player.eid == eid).cloned()
    };
    let Some(client) = client else {
        return false;
    };
    {
        let suite = live_mod_suite().lock().unwrap_or_else(|e| e.into_inner());
        if !suite.active || suite.season_sync.get(&eid) == Some(&(client.uuid, frame)) {
            return false;
        }
    }
    let sent = {
        let mut socket = client.sock.lock().unwrap_or_else(|e| e.into_inner());
        send_packet(
            &mut socket,
            LIVE_MOD_S2C_CUSTOM_PAYLOAD_ID,
            live_mod_payload_body(&channel, &payload),
        )
        .is_ok()
    };
    if sent {
        live_mod_suite()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .season_sync
            .insert(eid, (client.uuid, frame));
    }
    sent
}

#[cfg(feature = "mod-suite")]
#[allow(dead_code)]
fn live_mod_roll_halloween_headgear(
    entity_id: &str,
    month: u8,
    day: u8,
    roll: f32,
) -> Option<(&'static str, f32)> {
    let date = CalendarDate::new(month, day)?;
    let headgear = roll_halloween_headgear(entity_id, date, &HolidayConfig::default(), roll)?;
    let id = match headgear.kind {
        HalloweenHeadgearKind::JackOLantern => "minecraft:jack_o_lantern",
        HalloweenHeadgearKind::CarvedPumpkin => "minecraft:carved_pumpkin",
    };
    Some((id, headgear.head_drop_chance))
}

#[cfg(feature = "mod-suite")]
#[allow(dead_code)]
fn live_mod_chest_holiday_variant(block_id: &str, month: u8, day: u8) -> &'static str {
    let Some(date) = CalendarDate::new(month, day) else {
        return "standard";
    };
    match chest_holiday_variant(block_id, date, &HolidayConfig::default()) {
        ChestVisualVariant::Standard => "standard",
        ChestVisualVariant::ChristmasPresent => "christmas_present",
    }
}

#[cfg(feature = "mod-suite")]
#[allow(dead_code, clippy::too_many_arguments)]
fn live_mod_try_qol_right_click_harvest(
    enabled: bool,
    block_id: &str,
    age: u8,
    max_age: u8,
    drop_item: &str,
    seed_item: &str,
    raw_drops: u32,
    seed_drops: u32,
) -> Option<(u8, u32, u32)> {
    let config = QuarkCompatQolConfig {
        right_click_harvest: enabled,
        ..Default::default()
    };
    let plan = evaluate_right_click_harvest(block_id, age, max_age, &config)?;
    // Replanting consumes an actual seed from the loot. If seeds are also
    // represented by the main drop, deduct from that bucket instead.
    if seed_drops >= plan.seeds_deducted {
        Some((plan.reset_age, raw_drops, seed_drops - plan.seeds_deducted))
    } else if drop_item == seed_item && raw_drops >= plan.seeds_deducted {
        Some((plan.reset_age, raw_drops - plan.seeds_deducted, seed_drops))
    } else {
        None
    }
}

/// Uncompressed protocol-767 Play packet frame: VarInt length, packet id
/// 0x19, channel identifier, and opaque custom payload. When writing to a
/// live socket, re-enter `send_packet` so compression/encryption are honored.
#[cfg(feature = "mod-suite")]
#[allow(dead_code)]
fn live_mod_custom_payload_frame(channel: &str, payload: &[u8]) -> Vec<u8> {
    let body = live_mod_payload_body(channel, payload);
    let mut packet = Vec::with_capacity(body.len() + 5);
    write_varint(&mut packet, LIVE_MOD_S2C_CUSTOM_PAYLOAD_ID);
    packet.extend_from_slice(&body);
    let mut frame = Vec::with_capacity(packet.len() + 5);
    write_varint(&mut frame, packet.len() as i64);
    frame.extend_from_slice(&packet);
    frame
}

/// Each connected UUID owns one caster and status table. The spell definition
/// remains available until its delayed completion so its declared cooldown
/// can be armed exactly once. `last_sync` suppresses identical MAG1 snapshots.
#[cfg(feature = "mod-suite")]
#[derive(Clone, Debug)]
struct LiveMagicPlayer {
    caster: lmb_magic::SpellCaster,
    effects: lmb_magic::ActiveStatusEffects,
    pending_spell: Option<lmb_magic::SpellDefinition>,
    last_sync: Option<lmb_magic::MagicSyncState>,
}

#[cfg(feature = "mod-suite")]
impl LiveMagicPlayer {
    fn new(attributes: lmb_magic::MagicAttributes) -> Self {
        Self {
            caster: lmb_magic::SpellCaster::new(lmb_magic::ManaPool::new(attributes), 0),
            effects: lmb_magic::ActiveStatusEffects::default(),
            pending_spell: None,
            last_sync: None,
        }
    }

    fn snapshot(&self) -> lmb_magic::MagicSyncState {
        lmb_magic::MagicSyncState {
            mana: self.caster.mana.current_mana,
            max_mana: self.caster.mana.attributes.max_mana,
            cast: self
                .caster
                .active
                .as_ref()
                .map(|active| lmb_magic::WireCastProgress {
                    spell_id: active.spell_id.clone(),
                    state: active.state.clone(),
                }),
            cooldowns: self.caster.cooldowns.entries().clone(),
            statuses: self
                .effects
                .entries()
                .into_iter()
                .map(|status| lmb_magic::WireStatus {
                    id: status.def_id,
                    amplifier: status.amplifier,
                    remaining_ticks: status.remaining_ticks,
                    absorption_remaining: status.absorption_remaining,
                })
                .collect(),
        }
    }
}

/// The table is bounded even when callers attempt to initialize disconnected
/// UUIDs. `live_mod_tick` prunes disconnected entries on each game tick.
#[cfg(feature = "mod-suite")]
#[derive(Default)]
struct LiveMagicState {
    players: HashMap<[u8; 16], LiveMagicPlayer>,
    registry: lmb_magic::CustomStatusRegistry,
}

#[cfg(feature = "mod-suite")]
const LIVE_MAGIC_MAX_PLAYERS: usize = 1024;

#[cfg(feature = "mod-suite")]
fn live_magic_state() -> &'static Mutex<LiveMagicState> {
    static MAGIC: OnceLock<Mutex<LiveMagicState>> = OnceLock::new();
    MAGIC.get_or_init(|| Mutex::new(LiveMagicState::default()))
}

#[cfg(feature = "mod-suite")]
impl LiveMagicState {
    fn player_mut(&mut self, uuid: [u8; 16]) -> Result<&mut LiveMagicPlayer, LiveMagicError> {
        if !self.players.contains_key(&uuid) && self.players.len() >= LIVE_MAGIC_MAX_PLAYERS {
            return Err(LiveMagicError::PlayerLimit);
        }
        Ok(self
            .players
            .entry(uuid)
            .or_insert_with(|| LiveMagicPlayer::new(lmb_magic::MagicAttributes::default())))
    }
}

#[cfg(feature = "mod-suite")]
#[allow(dead_code)]
#[derive(Debug, PartialEq)]
enum LiveMagicError {
    PlayerLimit,
    CooldownLimit,
    Silenced,
    InvalidSpell(lmb_magic::SpellValidationError),
    Cast(lmb_magic::CastError),
    Execute(lmb_magic::SpellExecutionError),
    UnknownStatus,
    ApplyStatus(lmb_magic::ApplyStatusError),
}

/// Events and effect plans are returned to the server host for application.
/// `frame` is a complete uncompressed protocol-767 0x19 packet, if changed.
#[cfg(feature = "mod-suite")]
#[allow(dead_code)]
#[derive(Debug, Default)]
struct LiveMagicTick {
    cast_events: Vec<lmb_magic::CastEvent>,
    spell_plans: Vec<lmb_magic::SpellExecutionResult>,
    execution_errors: Vec<lmb_magic::SpellExecutionError>,
    statuses: lmb_magic::StatusTickReport,
    crowd_control: lmb_magic::CcFlags,
    mana_delta: f32,
    cooldowns_changed: bool,
    frame: Option<Vec<u8>>,
}

#[cfg(feature = "mod-suite")]
#[allow(dead_code)]
#[derive(Debug, Default)]
struct LiveMagicCast {
    events: Vec<lmb_magic::CastEvent>,
    spell_plans: Vec<lmb_magic::SpellExecutionResult>,
}

/// Configure a player without wiping casts, cooldowns, or statuses on repeat
/// calls. Current mana is clamped to the new maximum.
#[cfg(feature = "mod-suite")]
#[allow(dead_code)]
fn live_mod_magic_set_attributes(
    uuid: [u8; 16],
    attributes: lmb_magic::MagicAttributes,
) -> Result<(), LiveMagicError> {
    let mut magic = live_magic_state().lock().unwrap_or_else(|e| e.into_inner());
    let player = magic.player_mut(uuid)?;
    let current = player.caster.mana.current_mana;
    player.caster.mana = lmb_magic::ManaPool::with_mana(attributes, current);
    Ok(())
}

#[cfg(feature = "mod-suite")]
#[allow(dead_code)]
fn live_mod_magic_register_status(
    definition: lmb_magic::StatusEffectDefinition,
) -> Result<(), lmb_magic::StatusDefError> {
    live_magic_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .registry
        .register(definition)
}

#[cfg(feature = "mod-suite")]
#[allow(dead_code)]
fn live_mod_magic_apply_status(
    uuid: [u8; 16],
    id: &str,
    amplifier: u8,
    duration_ticks: u32,
) -> Result<(), LiveMagicError> {
    let mut magic = live_magic_state().lock().unwrap_or_else(|e| e.into_inner());
    let def = magic
        .registry
        .get(id)
        .ok_or(LiveMagicError::UnknownStatus)?
        .clone();
    magic
        .player_mut(uuid)?
        .effects
        .apply(&def, amplifier, duration_ticks)
        .map_err(LiveMagicError::ApplyStatus)
}

/// Consume active absorption pools before the host applies remaining damage.
#[cfg(feature = "mod-suite")]
#[allow(dead_code)]
fn live_mod_magic_absorb_damage(uuid: [u8; 16], damage: f32) -> f32 {
    let mut magic = live_magic_state().lock().unwrap_or_else(|e| e.into_inner());
    magic
        .players
        .get_mut(&uuid)
        .map_or(damage, |player| player.effects.absorb_damage(damage))
}

/// Refuse invalid, silenced or unaffordable casts atomically. Instants return
/// their resolved effect plans now; timed/channeled casts resolve on ticks.
#[cfg(feature = "mod-suite")]
#[allow(dead_code)]
fn live_mod_magic_cast(
    uuid: [u8; 16],
    spell: &lmb_magic::SpellDefinition,
) -> Result<LiveMagicCast, LiveMagicError> {
    spell.validate().map_err(LiveMagicError::InvalidSpell)?;
    let mut magic = live_magic_state().lock().unwrap_or_else(|e| e.into_inner());
    let silenced = magic
        .players
        .get(&uuid)
        .is_some_and(|player| player.effects.crowd_control(&magic.registry).silenced);
    if silenced {
        return Err(LiveMagicError::Silenced);
    }
    let player = magic.player_mut(uuid)?;
    if !player.caster.cooldowns.entries().contains_key(&spell.id)
        && player.caster.cooldowns.entries().len() >= lmb_magic::MAX_WIRE_COOLDOWNS
    {
        return Err(LiveMagicError::CooldownLimit);
    }
    let mut caster = player.caster.clone();
    let events = caster.begin_cast(spell).map_err(LiveMagicError::Cast)?;
    let completed = events
        .iter()
        .any(|event| matches!(event, lmb_magic::CastEvent::Completed { .. }));
    let spell_plans = if completed {
        vec![spell
            .execute(&caster.mana.attributes, &lmb_magic::CastContext::default())
            .map_err(LiveMagicError::Execute)?]
    } else {
        Vec::new()
    };
    player.caster = caster;
    player.pending_spell = (!completed).then(|| spell.clone());
    Ok(LiveMagicCast {
        events,
        spell_plans,
    })
}

/// Advance one stored player exactly once, returning delta events and an
/// optional MAG1 state snapshot wrapped in a protocol-767 custom packet.
#[cfg(feature = "mod-suite")]
#[allow(dead_code)]
fn live_mod_magic_tick_player(uuid: [u8; 16]) -> Option<LiveMagicTick> {
    let mut magic = live_magic_state().lock().unwrap_or_else(|e| e.into_inner());
    let LiveMagicState { players, registry } = &mut *magic;
    let player = players.get_mut(&uuid)?;
    let before_mana = player.caster.mana.current_mana;
    let before_cooldowns = player.caster.cooldowns.clone();
    let cast_events = player.caster.tick();
    let mut spell_plans = Vec::new();
    let mut execution_errors = Vec::new();
    for event in &cast_events {
        match event {
            lmb_magic::CastEvent::Completed { spell_id } => {
                if let Some(spell) = player.pending_spell.take() {
                    if spell.id == *spell_id {
                        player.caster.complete_cooldown(&spell);
                        match spell.execute(
                            &player.caster.mana.attributes,
                            &lmb_magic::CastContext::default(),
                        ) {
                            Ok(plan) => spell_plans.push(plan),
                            Err(error) => execution_errors.push(error),
                        }
                    }
                }
            }
            lmb_magic::CastEvent::Interrupted { .. } => {
                player.pending_spell = None;
            }
            lmb_magic::CastEvent::Pulse { .. } => {}
        }
    }
    let statuses = player.effects.tick(registry);
    let crowd_control = player.effects.crowd_control(registry);
    let snapshot = player.snapshot();
    let frame = if player.last_sync.as_ref() == Some(&snapshot) {
        None
    } else {
        lmb_magic::encode_magic_sync(&snapshot)
            .ok()
            .map(|bytes| live_mod_custom_payload_frame(lmb_magic::MAGIC_CHANNEL, &bytes))
    };
    if frame.is_some() {
        player.last_sync = Some(snapshot);
    }
    Some(LiveMagicTick {
        cast_events,
        spell_plans,
        execution_errors,
        statuses,
        crowd_control,
        mana_delta: player.caster.mana.current_mana - before_mana,
        cooldowns_changed: player.caster.cooldowns != before_cooldowns,
        frame,
    })
}

/// Disconnect cleanup is safe to repeat, including for unknown UUIDs.
#[cfg(feature = "mod-suite")]
#[allow(dead_code)]
fn live_mod_magic_remove_player(uuid: [u8; 16]) -> bool {
    live_magic_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .players
        .remove(&uuid)
        .is_some()
}

/// Feed a pre-framed uncompressed payload through the existing socket layer,
/// which owns compression and encryption negotiation.
#[cfg(feature = "mod-suite")]
fn live_mod_magic_send_frame(client: &Client, frame: &[u8]) {
    let mut bytes = frame;
    let Some(len) = read_varint(&mut bytes) else {
        return;
    };
    if len < 1 || len as usize != bytes.len() {
        return;
    }
    if read_varint(&mut bytes) != Some(LIVE_MOD_S2C_CUSTOM_PAYLOAD_ID) {
        return;
    }
    let mut socket = client.sock.lock().unwrap_or_else(|e| e.into_inner());
    let _ = send_packet(&mut socket, LIVE_MOD_S2C_CUSTOM_PAYLOAD_ID, bytes.to_vec());
}

/// Startup entry wired from `main`: scans `MEGU_MODS_DIR` (if set), then
/// arms the optional operator manifest (`MEGU_MOD_SYNC`) and progression
/// rules (`MEGU_PROGRESSION`). Without any of the three the bridge stays
/// completely inert — same as a default build.
#[cfg(feature = "mod-suite")]
fn live_mod_suite_boot() {
    let mods_dir = std::env::var_os("MEGU_MODS_DIR");
    let sync_path = std::env::var_os("MEGU_MOD_SYNC");
    let progression_path = std::env::var_os("MEGU_PROGRESSION");
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
/// The `Arc`s are cloned before any socket write. Magic players are advanced
/// independently of the optional AppleSkin suite and expired on disconnect.
#[cfg(feature = "mod-suite")]
fn live_mod_tick() {
    let players = clients().lock().unwrap_or_else(|e| e.into_inner()).clone();
    let connected: HashSet<[u8; 16]> = players.iter().map(|client| client.uuid).collect();
    {
        let connected_sessions: HashSet<(i32, [u8; 16])> =
            players.iter().map(|client| (client.eid, client.uuid)).collect();
        let mut suite = live_mod_suite().lock().unwrap_or_else(|e| e.into_inner());
        suite
            .season_sync
            .retain(|eid, (uuid, _)| connected_sessions.contains(&(*eid, *uuid)));
    }
    {
        let mut magic = live_magic_state().lock().unwrap_or_else(|e| e.into_inner());
        magic.players.retain(|uuid, _| connected.contains(uuid));
    }
    for client in &players {
        let initialized = {
            let mut magic = live_magic_state().lock().unwrap_or_else(|e| e.into_inner());
            magic.player_mut(client.uuid).is_ok()
        };
        if initialized {
            if let Some(tick) = live_mod_magic_tick_player(client.uuid) {
                if let Some(frame) = tick.frame {
                    live_mod_magic_send_frame(client, &frame);
                }
            }
        }
        live_mod_appleskin_sync(client);
    }
}

/// Serve the armed `megu:mod_sync_v1` (`MSY1`) manifest to a client that
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

/// Encode the live dynamic mod registry as one `MRS1`
/// (`megu:mod_registry_sync_v1`) frame for a joining client: Create's armed
/// block-state/item ids (states at `DEFAULT_BASE_STATE_ID`+, items at
/// `LIVE_CREATE_ITEM_BASE`+) plus the generic `MEGU_MODS_DIR` registry plan
/// built at boot. `None` when nothing modded is registered, so a vanilla or
/// unarmed server emits zero extra packets.
#[cfg(feature = "mod-suite")]
fn live_mod_registry_sync_frame() -> Option<Vec<u8>> {
    let mut block_states: Vec<DynamicBlockStateEntry> = Vec::new();
    let mut items: BTreeMap<u32, String> = BTreeMap::new();

    if live_create_enabled() {
        if let Some(registry) = breg().and_then(|reg| reg.create_registry()) {
            for entry in registry.states() {
                let mut properties = entry.properties.clone();
                properties.sort_by(|a, b| a.0.cmp(&b.0));
                block_states.push(DynamicBlockStateEntry {
                    state_id: entry.state_id,
                    block_id: entry.block_id.clone(),
                    properties,
                });
            }
        }
        for (index, (name, _)) in live_create_item_table().iter().enumerate() {
            let id = u32::try_from(LIVE_CREATE_ITEM_BASE + index as i64).ok()?;
            items.insert(id, name.clone());
        }
    }

    {
        let suite = live_mod_suite().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(plan) = suite.registry_plan.as_ref() {
            block_states.extend(plan.block_states.iter().cloned());
            items.extend(plan.items.iter().map(|(id, name)| (*id, name.clone())));
        }
    }

    if block_states.is_empty() && items.is_empty() {
        return None;
    }
    block_states.sort_by_key(|entry| entry.state_id);
    let base_block_state_id = block_states.first().map_or(0, |entry| entry.state_id);
    let base_item_id = items.keys().next().copied().unwrap_or(0);
    Some(
        UnifiedModRegistryPlan {
            base_block_state_id,
            base_item_id,
            block_states,
            items,
            content: RegistryContentIndex::default(),
        }
        .encode_registry_sync_frame(),
    )
}

/// Serve the `megu:mod_registry_sync_v1` (`MRS1`) dynamic registry frame to
/// a client that just entered play, wired next to the Create kinetic
/// snapshot. A no-op on vanilla/unarmed servers — `None` frame, no packet.
#[cfg(feature = "mod-suite")]
fn live_mod_registry_sync_on_join(s: &mut TcpStream) {
    let Some(frame) = live_mod_registry_sync_frame() else {
        return;
    };
    let _ = send_packet(
        s,
        LIVE_MOD_S2C_CUSTOM_PAYLOAD_ID,
        live_mod_payload_body(MOD_REGISTRY_SYNC_CHANNEL, &frame),
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
// Live Create runtime (`mod-suite` only): binds `create-adapter`'s
// `CreateFactory` to the real world — block placement/break hooks, wrench
// rotation, a per-tick kinetic step, the Play S2C `0x19` broadcast, the
// late-join snapshot, `create:*` item resolution, and the
// `create_factory.bin` world sidecar.
//
// Wire contract (mirrors `create-adapter/src/neoforge_wire.rs`):
//   packet id : Play S2C custom_payload `0x19`
//   channel   : `"create:kinetic_sync_v1"` (CHANNEL_CREATE_KINETIC_SYNC)
//   body      : write_str(channel) + VarInt count + per-record 29 bytes
//               [ i32be dim | i32be x | i32be y | i32be z
//               | f32be speed_rpm | f32be stress_capacity | f32be stress_load
//               | u8 overstressed (0|1) ]
// ===========================================================================

#[cfg(feature = "mod-suite")]
use crate::block_registry::{
    live_create_enabled, live_create_item_table, LIVE_CREATE_ITEM_BASE,
};
#[cfg(all(test, feature = "mod-suite"))]
use crate::block_registry::LIVE_CREATE_TEST_GATE;

/// The 1.21.1 `water`/`flowing_water` state-id window the fluid scheduler
/// already relies on (`register_overworld_scheduled` uses the same range).
/// Used when `breg()` is unavailable so the wheel check stays zero-cost.
#[cfg(feature = "mod-suite")]
const LIVE_CREATE_WATER_STATES: std::ops::RangeInclusive<u32> = 80..=95;

/// `/give` resolution: `"create:shaft"` -> a stable `1_000_000+` item id.
/// Returns `None` for every non-Create name so the vanilla tables below it
/// still answer first.
#[cfg(feature = "mod-suite")]
fn live_create_resolve_item(name: &str) -> Option<i64> {
    if !live_create_enabled() || !name.starts_with("create:") {
        return None;
    }
    live_create_item_table()
        .iter()
        .position(|(n, _)| n == name)
        .map(|i| LIVE_CREATE_ITEM_BASE + i as i64)
}

/// Reverse of [`live_create_resolve_item`]; `None` for vanilla item ids.
#[cfg(feature = "mod-suite")]
fn live_create_item_name(item: i64) -> Option<&'static str> {
    if !live_create_enabled() {
        return None;
    }
    let idx = usize::try_from(item.checked_sub(LIVE_CREATE_ITEM_BASE)?).ok()?;
    live_create_item_table().get(idx).map(|(n, _)| n.as_str())
}

/// The block id a `create:*` item places, or `None` for the wrench.
#[cfg(feature = "mod-suite")]
fn live_create_item_block_name(item: i64) -> Option<&'static str> {
    let idx = usize::try_from(item.checked_sub(LIVE_CREATE_ITEM_BASE)?).ok()?;
    live_create_item_table().get(idx)?.1.as_deref()
}

/// Water check used by the wheel rule. Prefers the live registry name so
/// waterlogged/fluid renames keep working, with the hardcoded 1.21.1 window
/// (see [`LIVE_CREATE_WATER_STATES`]) as the no-assets fallback.
#[cfg(feature = "mod-suite")]
fn live_create_is_water(st: u32) -> bool {
    if LIVE_CREATE_WATER_STATES.contains(&st) {
        return true;
    }
    breg().is_some_and(|reg| {
        matches!(reg.name_of(st), "minecraft:water" | "minecraft:flowing_water")
    })
}

/// `create:pressing` — a powered `create:mechanical_press` two blocks above
/// a depot or basin. `(input_item, output_item)` names resolved once at
/// arm-time through the real item tables.
#[cfg(feature = "mod-suite")]
const LIVE_CREATE_PRESSING: &[(&str, &str)] = &[
    ("minecraft:iron_ingot", "create:iron_sheet"),
    ("minecraft:gold_ingot", "create:gold_sheet"),
    ("minecraft:copper_ingot", "create:copper_sheet"),
    ("create:brass_ingot", "create:brass_sheet"),
];

/// `create:mixing` — a powered `create:mechanical_mixer` two blocks above a
/// basin. `(&input_items, output_item)`.
#[cfg(feature = "mod-suite")]
const LIVE_CREATE_MIXING: &[(&[&str], &str)] = &[
    (
        &["minecraft:copper_ingot", "create:zinc_ingot"],
        "create:brass_ingot",
    ),
    (
        &["minecraft:andesite", "minecraft:iron_nugget"],
        "create:andesite_alloy",
    ),
];

/// Processing duration in ticks at `REFERENCE_RPM` — progress accrues in
/// rpm·tick units so a faster shaft finishes proportionally sooner.
#[cfg(feature = "mod-suite")]
const LIVE_CREATE_PROCESS_TICKS: u32 = 40;

/// One resolved processing recipe: an input multiset -> one output item.
#[cfg(feature = "mod-suite")]
struct LiveCreateRecipe {
    inputs: Vec<i64>,
    output: i64,
    base_ticks: u32,
}

/// Items resting on a depot or inside a basin. `progress` accrues
/// rpm·ticks while a powered press/mixer two blocks above can match a
/// recipe against `items`; `active` pins the accruing recipe (`0..` =
/// `pressing`, `0x100..` = `mixing`) so a changed match restarts cleanly.
#[cfg(feature = "mod-suite")]
#[derive(Default)]
struct LiveCreateHeld {
    items: Vec<i64>,
    progress: f64,
    active: Option<u16>,
}

/// Live `CreateFactory` bound to the server world. `placed` mirrors every
/// Create block currently in the world (the authoritative set for
/// persistence); `dormant` marks the subset — water wheels with no adjacent
/// water — that are placed but intentionally NOT registered in the kinetic
/// graph so they contribute no RPM until flow arrives. `held` is the
/// depot/basin item slot state persisted beside `placed`; `belt_machines`
/// maps belt blocks onto their lazily-registered `SleepingTransport`
/// machine ids.
#[cfg(feature = "mod-suite")]
struct LiveCreateRuntime {
    factory: create_adapter::factory::CreateFactory,
    placed: std::collections::BTreeMap<create_adapter::factory::DimBlockPos, u32>,
    dormant: std::collections::BTreeSet<create_adapter::factory::DimBlockPos>,
    held: std::collections::BTreeMap<create_adapter::factory::DimBlockPos, LiveCreateHeld>,
    belt_machines: std::collections::BTreeMap<create_adapter::factory::DimBlockPos, u32>,
    next_machine_id: u32,
    pressing: Vec<LiveCreateRecipe>,
    mixing: Vec<LiveCreateRecipe>,
    /// Number of non-empty kinetic batches drained since boot (test hook).
    kinetic_batches_emitted: u64,
}

#[cfg(feature = "mod-suite")]
type FactoryPos = create_adapter::factory::DimBlockPos;

#[cfg(feature = "mod-suite")]
fn live_create_state() -> &'static Mutex<Option<LiveCreateRuntime>> {
    static RT: OnceLock<Mutex<Option<LiveCreateRuntime>>> = OnceLock::new();
    RT.get_or_init(|| Mutex::new(None))
}

#[cfg(feature = "mod-suite")]
fn live_create_runtime_init() -> Option<LiveCreateRuntime> {
    if !live_create_enabled() {
        return None;
    }
    let registry = create_adapter::blockstates::core_create_registry(
        create_adapter::blockstates::DEFAULT_BASE_STATE_ID,
    );
    Some(LiveCreateRuntime {
        factory: create_adapter::factory::CreateFactory::new(registry),
        placed: std::collections::BTreeMap::new(),
        dormant: std::collections::BTreeSet::new(),
        held: std::collections::BTreeMap::new(),
        belt_machines: std::collections::BTreeMap::new(),
        next_machine_id: 1,
        pressing: LIVE_CREATE_PRESSING
            .iter()
            .filter_map(|&(input, output)| {
                Some(LiveCreateRecipe {
                    inputs: vec![name_to_item(input)?],
                    output: name_to_item(output)?,
                    base_ticks: LIVE_CREATE_PROCESS_TICKS,
                })
            })
            .collect(),
        mixing: LIVE_CREATE_MIXING
            .iter()
            .filter_map(|&(inputs, output)| {
                Some(LiveCreateRecipe {
                    inputs: inputs
                        .iter()
                        .map(|&n| name_to_item(n))
                        .collect::<Option<_>>()?,
                    output: name_to_item(output)?,
                    base_ticks: LIVE_CREATE_PROCESS_TICKS,
                })
            })
            .collect(),
        kinetic_batches_emitted: 0,
    })
}

/// Borrow the armed runtime, lazily arming it on first use. `None` when
/// Create is disabled — every caller treats that as a zero-cost no-op, so
/// default builds and unconfigured mod-suite builds stay byte-identical.
#[cfg(feature = "mod-suite")]
fn live_create_with<R>(f: impl FnOnce(&mut LiveCreateRuntime) -> R) -> Option<R> {
    if !live_create_enabled() {
        return None;
    }
    let mut guard = live_create_state().lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        *guard = live_create_runtime_init();
    }
    guard.as_mut().map(f)
}

#[cfg(feature = "mod-suite")]
impl LiveCreateRuntime {
    fn is_create_state(&self, st: u32) -> bool {
        self.factory.registry().state(st).is_some()
    }

    fn is_water_wheel(&self, st: u32) -> bool {
        self.factory
            .registry()
            .block_def_for_state(st)
            .is_some_and(|def| def.id.ends_with("water_wheel"))
    }

    /// Any of the six orthogonally adjacent cells holds water.
    fn adjacent_water(&self, pos: FactoryPos) -> bool {
        let dim = DimId::from_raw(pos.dim as u32);
        [
            (1i32, 0i32, 0i32),
            (-1, 0, 0),
            (0, 1, 0),
            (0, -1, 0),
            (0, 0, 1),
            (0, 0, -1),
        ]
        .iter()
        .any(|&(dx, dy, dz)| {
            live_create_is_water(block_at_dim(dim, pos.x + dx, pos.y + dy, pos.z + dz))
        })
    }

    /// Bring the kinetic graph in line with `st` at `pos`. A water wheel
    /// beside no water stays in `placed`/`dormant` (visible block, inert
    /// node); everything else registers directly.
    fn sync_cell(&mut self, pos: FactoryPos, st: u32) {
        if !self.is_create_state(st) {
            self.placed.remove(&pos);
            self.dormant.remove(&pos);
            self.held.remove(&pos);
            self.belt_machines.remove(&pos);
            let _ = self.factory.remove_block(pos);
            return;
        }
        // The block keeps its cell: drop slot/machine state the new state
        // can no longer own (e.g. a depot swapped for a shaft).
        match self
            .factory
            .registry()
            .block_def_for_state(st)
            .map(|def| def.id.as_str())
        {
            Some("create:depot" | "create:basin") => {
                self.belt_machines.remove(&pos);
            }
            Some("create:mechanical_belt") => {
                self.held.remove(&pos);
            }
            _ => {
                self.held.remove(&pos);
                self.belt_machines.remove(&pos);
            }
        }
        let active = !self.is_water_wheel(st) || self.adjacent_water(pos);
        if active
            && self.placed.get(&pos) == Some(&st)
            && !self.dormant.contains(&pos)
            && self.factory.kinetic_solution(pos).is_some()
        {
            // Already registered with this exact state — the wrench path
            // pre-committed the factory node; skip the redundant re-solve.
            return;
        }
        if active {
            self.dormant.remove(&pos);
            self.placed.insert(pos, st);
            let _ = self.factory.place_block(pos, st);
        } else {
            let _ = self.factory.remove_block(pos);
            self.placed.insert(pos, st);
            self.dormant.insert(pos);
        }
    }

    /// A water cell appeared/vanished at `pos`: re-evaluate every tracked
    /// water wheel in the adjacent cells so a wheel activates when flow
    /// arrives and idles when it leaves.
    fn reeval_wheel_neighbors(&mut self, pos: FactoryPos) {
        for (dx, dy, dz) in [
            (1i32, 0i32, 0i32),
            (-1, 0, 0),
            (0, 1, 0),
            (0, -1, 0),
            (0, 0, 1),
            (0, 0, -1),
        ] {
            let npos = pos.offset(dx, dy, dz);
            let Some(&st) = self.placed.get(&npos) else {
                continue;
            };
            if self.is_water_wheel(st) {
                self.sync_cell(npos, st);
            }
        }
    }

    /// Rebuild the whole runtime from saved entries (`load_entries` is only
    /// reached after the world overlay is applied, so `adjacent_water`
    /// already sees the restored fluids). Each entry carries the items
    /// resting on the cell at save time; they are re-seated only when the
    /// restored block is still a depot/basin, otherwise dropped — in-flight
    /// progress is intentionally not saved.
    fn load_entries(&mut self, entries: Vec<(FactoryPos, u32, Vec<i64>)>) {
        self.factory = create_adapter::factory::CreateFactory::new(
            create_adapter::blockstates::core_create_registry(
                create_adapter::blockstates::DEFAULT_BASE_STATE_ID,
            ),
        );
        self.placed.clear();
        self.dormant.clear();
        self.held.clear();
        self.belt_machines.clear();
        self.next_machine_id = 1;
        for (pos, st, items) in entries {
            self.sync_cell(pos, st);
            if items.is_empty() {
                continue;
            }
            let cap = match self
                .factory
                .registry()
                .block_def_for_state(st)
                .map(|def| def.id.as_str())
            {
                Some("create:depot") => 1,
                Some("create:basin") => 9,
                _ => continue,
            };
            let mut held = LiveCreateHeld::default();
            held.items = items.into_iter().take(cap).collect();
            self.held.insert(pos, held);
        }
    }

    /// Lazily register the `SleepingTransport` machine behind a belt block
    /// and return its region-local id. The traversal delay scales
    /// inversely with the belt's solved RPM (floor 8 rpm -> 8 ticks).
    fn belt_machine(&mut self, pos: FactoryPos) -> u32 {
        if let Some(&id) = self.belt_machines.get(&pos) {
            return id;
        }
        let id = self.next_machine_id;
        self.next_machine_id = self.next_machine_id.saturating_add(1).max(1);
        let rpm = self
            .factory
            .kinetic_solution(pos)
            .map(|sol| sol.speed.abs())
            .unwrap_or(0.0);
        let delay = (64.0 / rpm.max(8.0)).round().clamp(1.0, 64.0) as u64;
        let _ = self.factory.add_belt_segment(pos, id, 1, delay);
        self.belt_machines.insert(pos, id);
        id
    }

    /// Active processing pass, run once per `live_create_tick` before the
    /// factory steps: every depot/basin holding items looks for a powered
    /// `create:mechanical_press` (depot or basin) or `create:mechanical_mixer`
    /// (basin only) two blocks above and accrues `rpm` progress. Reaching
    /// `base_ticks * REFERENCE_RPM` consumes the recipe inputs and leaves
    /// the product in the same slot — deterministic and proportional to
    /// the solved speed.
    fn tick_processing(&mut self) {
        if self.held.is_empty() {
            return;
        }
        let positions: Vec<FactoryPos> = self.held.keys().copied().collect();
        for pos in positions {
            let Some(&st) = self.placed.get(&pos) else {
                self.held.remove(&pos);
                continue;
            };
            let basin = self
                .factory
                .registry()
                .block_def_for_state(st)
                .is_some_and(|def| def.id == "create:basin");
            let depot = !basin
                && self
                    .factory
                    .registry()
                    .block_def_for_state(st)
                    .is_some_and(|def| def.id == "create:depot");
            if !basin && !depot {
                self.held.remove(&pos);
                continue;
            }
            let proc_pos = pos.offset(0, 2, 0);
            let Some(&pst) = self.placed.get(&proc_pos) else {
                continue;
            };
            let Some(pdef) = self.factory.registry().block_def_for_state(pst) else {
                continue;
            };
            let mixer = basin && pdef.id == "create:mechanical_mixer";
            if pdef.id != "create:mechanical_press" && !mixer {
                continue;
            }
            let Some(sol) = self.factory.kinetic_solution(proc_pos) else {
                continue;
            };
            if !matches!(
                sol.status,
                mod_offload::kinetic_graph::NetworkStatus::Running
            ) || sol.speed.abs() < 1e-6
            {
                continue;
            }
            let rpm = sol.speed.abs();
            let recipes = if mixer { &self.mixing } else { &self.pressing };
            let Some(slot) = self.held.get_mut(&pos) else {
                continue;
            };
            let Some((idx, recipe)) = recipes
                .iter()
                .enumerate()
                .find(|(_, r)| r.inputs.iter().all(|need| slot.items.contains(need)))
            else {
                slot.progress = 0.0;
                slot.active = None;
                continue;
            };
            let key = idx as u16 + if mixer { 0x100 } else { 0 };
            if slot.active != Some(key) {
                slot.active = Some(key);
                slot.progress = 0.0;
            }
            slot.progress += rpm;
            let need = f64::from(recipe.base_ticks)
                * f64::from(mod_offload::recipe_processor::REFERENCE_RPM);
            if slot.progress < need {
                continue;
            }
            for input in &recipe.inputs {
                if let Some(at) = slot.items.iter().position(|item| item == input) {
                    slot.items.remove(at);
                }
            }
            slot.items.push(recipe.output);
            slot.progress = 0.0;
            slot.active = None;
        }
    }

    /// Full kinetic snapshot used by the join packet.
    fn snapshot_tags(&self) -> Vec<create_adapter::factory::KineticUpdateTag> {
        let mut tags = Vec::with_capacity(self.placed.len());
        for &pos in self.placed.keys() {
            if self.dormant.contains(&pos) {
                continue;
            }
            if let Some(sol) = self.factory.kinetic_solution(pos) {
                tags.push(create_adapter::factory::KineticUpdateTag {
                    pos,
                    speed_rpm: sol.speed,
                    stress_capacity: sol.capacity,
                    stress_load: sol.load,
                    overstressed: matches!(
                        sol.status,
                        mod_offload::kinetic_graph::NetworkStatus::Overstressed
                    ),
                });
            }
        }
        tags
    }
}

/// World-write hook wired beside `live_block_changed` in every block
/// mutation path (`set_block_if_inner_dim`, `set_block_if_dim_edit`,
/// `set_blocks_bulk_dim`). Vanilla-only writes pay two compares and exit;
/// a water<->non-water edge additionally probes the runtime so flow
/// arriving next to a wheel activates it.
#[cfg(feature = "mod-suite")]
fn live_create_on_block_set(dim: DimId, x: i32, y: i32, z: i32, old: u32, st: u32) {
    if !live_create_enabled() {
        return;
    }
    let base = create_adapter::blockstates::DEFAULT_BASE_STATE_ID;
    let was_mod = old >= base;
    let is_mod = st >= base;
    let water_edge =
        !was_mod && !is_mod && live_create_is_water(old) != live_create_is_water(st);
    if !was_mod && !is_mod && !water_edge {
        return;
    }
    let pos = FactoryPos::new(dim.raw() as i32, x, y, z);
    live_create_with(|rt| {
        if was_mod || is_mod {
            rt.sync_cell(pos, st);
        }
        if water_edge {
            rt.reeval_wheel_neighbors(pos);
        }
    });
}

/// `BlockItem.place` for `create:*` block items, invoked from
/// `try_live_block_placement` before the vanilla item table is consulted.
/// Reproduces the vanilla placement contract: clicked-replaceable absorbs,
/// face offsets otherwise; `axis` follows the clicked face, `facing`
/// follows the look direction (or the clicked face for all-direction
/// blocks), `powered=false`, `waterlogged` follows the replaced fluid.
/// The write goes through `set_block_if_inner_dim`, so the kinetic-graph
/// hook, save guard, and client resync all see one ordinary block write.
#[cfg(feature = "mod-suite")]
fn live_create_block_place(
    c: &Client,
    (dim, x, y, z): (DimId, i32, i32, i32),
    face: usize,
    held: i16,
    item: i64,
) -> bool {
    let Some(reg) = breg() else {
        return false;
    };
    let Some(block_id) = live_create_item_block_name(item) else {
        return false;
    };
    let clicked_st = block_at_dim(dim, x, y, z);
    if reg.is_air(clicked_st) {
        return false;
    }
    let fdir = RsDir::ALL[face.min(5)];
    let (dx, dy, dz) = fdir.offset();
    let (tx, ty, tz) = if reg.is_replaceable(clicked_st) {
        (x, y, z)
    } else {
        (x + dx, y + dy, z + dz)
    };
    let target_old = block_at_dim(dim, tx, ty, tz);
    if !reg.is_replaceable(target_old) {
        return true;
    }
    if matches!(*c.gm.lock().unwrap_or_else(|e| e.into_inner()), 2 | 3) {
        return true;
    }
    let yaw = c.pos.lock().unwrap_or_else(|e| e.into_inner()).3;
    let look = placement_look(yaw);
    let Some(st) = live_create_with(|rt| {
        let registry = rt.factory.registry();
        let def = registry.block_def(block_id)?;
        let mut st = registry.default_state(block_id)?;
        if def.properties.iter().any(|p| p.name == "axis") {
            let axis = match fdir.axis() {
                RsAxis::X => "x",
                RsAxis::Y => "y",
                RsAxis::Z => "z",
            };
            st = registry.with_property(st, "axis", axis).unwrap_or(st);
        }
        if let Some(prop) = def.properties.iter().find(|p| p.name == "facing") {
            let wanted = fdir.opposite().property();
            let facing = if prop.values.iter().any(|v| v == wanted) {
                wanted
            } else {
                let horiz = look.opposite().property();
                if prop.values.iter().any(|v| v == horiz) {
                    horiz
                } else {
                    prop.values[0].as_str()
                }
            };
            st = registry.with_property(st, "facing", facing).unwrap_or(st);
        }
        if def.properties.iter().any(|p| p.name == "powered") {
            st = registry.with_property(st, "powered", "false").unwrap_or(st);
        }
        if def.properties.iter().any(|p| p.name == "waterlogged") {
            let wet = reg.fluid_name(target_old) == "minecraft:water";
            st = registry
                .with_property(st, "waterlogged", if wet { "true" } else { "false" })
                .unwrap_or(st);
        }
        Some(st)
    })
    .flatten() else {
        return false;
    };
    with_save_guard(|| {
        if set_block_if_inner_dim(dim, tx, ty, tz, st, Some(target_old)) {
            note_block_update_dim(dim, tx, ty, tz, block_at_dim(dim, tx, ty, tz));
            ac_consume_placed_item(c, held);
        }
    });
    true
}

/// `create:wrench` right-click on a tracked Create block: cycle `axis`
/// first, then `facing` (press/pump/funnel/bearing family), re-solve the
/// network through `CreateFactory::wrench_rotate`, and write the rotated
/// state back so clients see the new blockstate. Rotation while sneaking
/// follows the same contract.
#[cfg(feature = "mod-suite")]
fn live_create_wrench_use(
    me: &Client,
    dim: DimId,
    x: i32,
    y: i32,
    z: i32,
    clicked: u32,
    held_slot: i16,
) -> bool {
    if !live_create_enabled() {
        return false;
    }
    let item = me
        .inv
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(36 + held_slot))
        .map(|s| s.0)
        .unwrap_or(0);
    if live_create_item_name(item) != Some("create:wrench") {
        return false;
    }
    let pos = FactoryPos::new(dim.raw() as i32, x, y, z);
    let Some(next) = live_create_wrench_rotate(pos, clicked) else {
        return false;
    };
    if set_block_if_dim(dim, x, y, z, next, Some(clicked)) {
        note_block_update_dim(dim, x, y, z, next);
    }
    true
}

/// The runtime half of the wrench interaction — separated from `Client` so
/// tests exercise the exact factory + world-write contract.
#[cfg(feature = "mod-suite")]
fn live_create_wrench_rotate(pos: FactoryPos, clicked: u32) -> Option<u32> {
    live_create_with(|rt| {
        if rt.factory.kinetic_solution(pos).is_none() && !rt.placed.contains_key(&pos) {
            return None;
        }
        if let Some((next, _diffs)) = rt.factory.wrench_rotate(pos) {
            rt.placed.insert(pos, next);
            return Some(next);
        }
        let st = *rt.placed.get(&pos).unwrap_or(&clicked);
        let next = rt
            .factory
            .registry()
            .cycle_property(st, "facing")
            .or_else(|| rt.factory.registry().cycle_property(st, "axis"))?;
        if rt.dormant.contains(&pos) {
            rt.placed.insert(pos, next);
        } else {
            let _ = rt.factory.place_block(pos, next);
            rt.placed.insert(pos, next);
        }
        Some(next)
    })
    .flatten()
}

/// Right-click on a tracked `create:depot`, `create:mechanical_belt` or
/// `create:basin` without the wrench. A held item inserts one unit into the
/// machine's item slot (depot 1, basin 9, belt input 1) and the caller
/// consumes it like a placement; an empty hand — or any sneak-click —
/// pulls the stored or processed item back through the vanilla
/// `Inventory.add` pickup path. Returns `false` when the click is not
/// handled so the normal placement/interaction flow still runs.
#[cfg(feature = "mod-suite")]
fn live_create_interact_block(
    me: &Client,
    dim: DimId,
    x: i32,
    y: i32,
    z: i32,
    clicked: u32,
    held_slot: i16,
) -> bool {
    if !live_create_enabled() || *me.gm.lock().unwrap_or_else(|e| e.into_inner()) == 3 {
        return false;
    }
    let held_item = me
        .inv
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(36 + held_slot))
        .map(|s| s.0)
        .unwrap_or(0);
    if live_create_item_name(held_item) == Some("create:wrench") {
        return false;
    }
    let sneaking = *me.sneaking.lock().unwrap_or_else(|e| e.into_inner());
    let pos = FactoryPos::new(dim.raw() as i32, x, y, z);
    enum LiveCreateInteract {
        Inserted,
        Gave(Vec<(i16, Stack)>),
    }
    let Some(outcome) = live_create_with(|rt| {
        if rt.placed.get(&pos).copied() != Some(clicked) {
            return None;
        }
        let id = rt.factory.registry().block_def_for_state(clicked)?.id.as_str();
        let belt = id == "create:mechanical_belt";
        let depot = id == "create:depot";
        let basin = id == "create:basin";
        if !belt && !depot && !basin {
            return None;
        }
        if held_item != 0 && !sneaking {
            if belt {
                let machine = rt.belt_machine(pos);
                let item = u32::try_from(held_item).ok()?;
                rt.factory.insert_belt_item(pos, machine, item).ok()?;
            } else {
                let cap = if depot { 1 } else { 9 };
                if rt.held.get(&pos).is_some_and(|s| s.items.len() >= cap) {
                    return None;
                }
                let slot = rt.held.entry(pos).or_default();
                slot.items.push(held_item);
                slot.progress = 0.0;
                slot.active = None;
            }
            return Some(LiveCreateInteract::Inserted);
        }
        // Retrieval: extract first, then `Inventory.add`; a full inventory
        // re-seats the item so a failed grab never deletes it.
        let item: i64 = if belt {
            let machine = *rt.belt_machines.get(&pos)?;
            rt.factory
                .scheduler_mut()
                .region_mut(pos.region_key())
                .extract_output(machine)
                .ok()?
                .map(i64::from)?
        } else {
            rt.held.get_mut(&pos)?.items.pop()?
        };
        let mut inv = me.inv.lock().unwrap_or_else(|e| e.into_inner());
        let selected = *me.held.lock().unwrap_or_else(|e| e.into_inner());
        let creative = *me.gm.lock().unwrap_or_else(|e| e.into_inner()) == 1;
        let before = inv.clone();
        let mut source = (item, 1u8, Vec::new());
        let accepted = pickup_into_inventory(&mut inv, selected, creative, &mut source);
        if !accepted || source.1 != 0 {
            if belt {
                if let (Some(&machine), Ok(item)) =
                    (rt.belt_machines.get(&pos), u32::try_from(item))
                {
                    let _ = rt.factory.insert_belt_item(pos, machine, item);
                }
            } else if let Some(slot) = rt.held.get_mut(&pos) {
                slot.items.push(item);
            }
            return None;
        }
        if !belt && rt.held.get(&pos).is_some_and(|h| h.items.is_empty()) {
            rt.held.remove(&pos);
        }
        let changed: Vec<(i16, Stack)> = (5..=45)
            .filter_map(|slot| {
                inv.get(&slot)
                    .filter(|st| before.get(&slot) != Some(st))
                    .map(|st| (slot, st.clone()))
            })
            .collect();
        Some(LiveCreateInteract::Gave(changed))
    })
    .flatten() else {
        return false;
    };
    match outcome {
        LiveCreateInteract::Inserted => {
            ac_consume_placed_item(me, held_slot);
        }
        LiveCreateInteract::Gave(changed) => {
            for (slot, st) in changed {
                let mut p = vec![0u8];
                write_varint(&mut p, i64::from(inv_state_next(me)));
                p.extend_from_slice(&slot.to_be_bytes());
                p.extend_from_slice(&slot_wire(&st));
                let _ = send_packet(
                    &mut me.sock.lock().unwrap_or_else(|e| e.into_inner()),
                    0x15,
                    p,
                );
            }
        }
    }
    true
}

/// The tick-half without the socket write — step the factory once, drain
/// the coalesced `KineticUpdateTag` batch (only changed nodes; the
/// fingerprint gate suppresses idle churn), and encode it for the wire.
/// Factored out so tests observe the exact bytes clients receive.
#[cfg(feature = "mod-suite")]
fn live_create_tick_body() -> Option<Vec<u8>> {
    if !live_create_enabled() {
        return None;
    }
    let tick = TICK_IDX.load(Ordering::Relaxed);
    live_create_with(|rt| {
        rt.tick_processing();
        let _ = rt.factory.step_tick(tick, 1_024, None);
        let mut drained = Vec::new();
        rt.factory.drain_update_batch_into(&mut drained);
        if drained.is_empty() {
            return None;
        }
        let tags: Vec<create_adapter::factory::KineticUpdateTag> =
            drained.into_iter().map(|(_, tag)| tag).collect();
        rt.kinetic_batches_emitted += 1;
        Some(live_mod_payload_body(
            create_adapter::neoforge_wire::CHANNEL_CREATE_KINETIC_SYNC,
            &create_adapter::neoforge_wire::encode_kinetic_update_batch(&tags),
        ))
    })
    .flatten()
}

/// Per-game-tick stage (`run("live_create", live_create_tick)`): broadcast
/// the drained kinetic batch to every connected client over Play S2C
/// `0x19` on channel `create:kinetic_sync_v1`. The factory lock is released
/// before any socket is touched.
#[cfg(feature = "mod-suite")]
fn live_create_tick() {
    let Some(body) = live_create_tick_body() else {
        return;
    };
    for client in client_snapshot() {
        let mut socket = client.sock.lock().unwrap_or_else(|e| e.into_inner());
        let _ = send_packet(&mut socket, LIVE_MOD_S2C_CUSTOM_PAYLOAD_ID, body.clone());
    }
}

/// Full kinetic snapshot packet for a joining client, sent right after
/// `live_mod_sync_on_join` so late joiners see spinning wheels/shafts
/// without waiting for the next delta.
#[cfg(feature = "mod-suite")]
fn live_create_sync_on_join(s: &mut TcpStream) {
    let Some(body) = live_create_with(|rt| {
        let tags = rt.snapshot_tags();
        if tags.is_empty() {
            return None;
        }
        Some(live_mod_payload_body(
            create_adapter::neoforge_wire::CHANNEL_CREATE_KINETIC_SYNC,
            &create_adapter::neoforge_wire::encode_kinetic_update_batch(&tags),
        ))
    })
    .flatten() else {
        return;
    };
    let _ = send_packet(s, LIVE_MOD_S2C_CUSTOM_PAYLOAD_ID, body);
}

// ---- create_factory.bin sidecar ------------------------------------------
//
// Format (`MCF2`): magic(4) + VarInt entry_count + per entry
// [ i32be dim | i32be x | i32be y | i32be z | VarStr canonical
// | VarInt held_count | held_count x VarStr item_name ], then a
// big-endian u32 FNV-1a checksum over every preceding byte. `canonical` is
// `BlockRegistry::encode_canonical(state_id)` — e.g.
// `create:shaft[axis=x,waterlogged=false]` — so the sidecar survives table
// reordering; on load each entry is re-decoded through
// `decode_canonical`. `item_name` is the `item_to_name` form of an item
// resting on a depot/basin (processing progress is not saved). `MCF1`
// files — the same layout without the item tail — still decode with empty
// item lists. Any malformed tail fails the load closed.

#[cfg(feature = "mod-suite")]
const LIVE_CREATE_SIDECAR_MAGIC: &[u8; 4] = b"MCF2";
#[cfg(feature = "mod-suite")]
const LIVE_CREATE_SIDECAR_MAGIC_V1: &[u8; 4] = b"MCF1";
#[cfg(feature = "mod-suite")]
const LIVE_CREATE_SIDECAR_FILE: &str = "create_factory.bin";

#[cfg(feature = "mod-suite")]
fn live_create_fnv32(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for &b in bytes {
        hash ^= u32::from(b);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// Bytes for `create_factory.bin`, or `None` while Create is unarmed —
/// callers must not write the file in that case.
#[cfg(feature = "mod-suite")]
fn live_create_serialize() -> Option<Vec<u8>> {
    if !live_create_enabled() {
        return None;
    }
    live_create_with(|rt| {
        let registry = create_adapter::blockstates::core_create_registry(
            create_adapter::blockstates::DEFAULT_BASE_STATE_ID,
        );
        let mut out = Vec::new();
        out.extend_from_slice(LIVE_CREATE_SIDECAR_MAGIC);
        write_varint(&mut out, rt.placed.len() as i64);
        for (pos, st) in &rt.placed {
            out.extend_from_slice(&pos.dim.to_be_bytes());
            out.extend_from_slice(&pos.x.to_be_bytes());
            out.extend_from_slice(&pos.y.to_be_bytes());
            out.extend_from_slice(&pos.z.to_be_bytes());
            let canon = registry.encode_canonical(*st).unwrap_or_default();
            write_str(&mut out, canon);
            let names: Vec<&'static str> = rt
                .held
                .get(pos)
                .map(|held| {
                    held.items
                        .iter()
                        .filter_map(|item| item_to_name(*item))
                        .collect()
                })
                .unwrap_or_default();
            write_varint(&mut out, names.len() as i64);
            for name in names {
                write_str(&mut out, name);
            }
        }
        let sum = live_create_fnv32(&out);
        out.extend_from_slice(&sum.to_be_bytes());
        out
    })
}

/// Decode + validate the sidecar into `(pos, state_id, held_items)`
/// entries. `MCF1` entries carry no item tail and decode with an empty
/// list. Every malformed tail — bad magic, bad checksum, truncation,
/// unknown canonical or item name, impossible count — is `Err` so
/// `load_world_in` fails closed.
#[cfg(feature = "mod-suite")]
fn live_create_decode_sidecar(
    raw: &[u8],
) -> Result<Vec<(FactoryPos, u32, Vec<i64>)>, &'static str> {
    const ERR: &str = "corrupt create factory save";
    let v2 = raw.len() >= 4 && raw[..4] == *LIVE_CREATE_SIDECAR_MAGIC;
    if raw.len() < 9 || !(v2 || raw[..4] == *LIVE_CREATE_SIDECAR_MAGIC_V1) {
        return Err(ERR);
    }
    let (body, check) = raw.split_at(raw.len() - 4);
    let want = u32::from_be_bytes(check.try_into().map_err(|_| ERR)?);
    if live_create_fnv32(body) != want {
        return Err(ERR);
    }
    let mut rest = &body[4..];
    let count = read_varint(&mut rest).ok_or(ERR)?;
    if !(0..=1_000_000).contains(&count) {
        return Err(ERR);
    }
    let registry = create_adapter::blockstates::core_create_registry(
        create_adapter::blockstates::DEFAULT_BASE_STATE_ID,
    );
    let mut entries = Vec::with_capacity(count as usize);
    for _ in 0..count {
        if rest.len() < 16 {
            return Err(ERR);
        }
        let dim = i32::from_be_bytes(rest[0..4].try_into().map_err(|_| ERR)?);
        let x = i32::from_be_bytes(rest[4..8].try_into().map_err(|_| ERR)?);
        let y = i32::from_be_bytes(rest[8..12].try_into().map_err(|_| ERR)?);
        let z = i32::from_be_bytes(rest[12..16].try_into().map_err(|_| ERR)?);
        rest = &rest[16..];
        let strlen = read_varint(&mut rest).ok_or(ERR)?;
        if !(0..=256).contains(&strlen) || rest.len() < strlen as usize {
            return Err(ERR);
        }
        let canon = std::str::from_utf8(&rest[..strlen as usize]).map_err(|_| ERR)?;
        rest = &rest[strlen as usize..];
        let st = registry.decode_canonical(canon).ok_or(ERR)?;
        let mut items = Vec::new();
        if v2 {
            let item_count = read_varint(&mut rest).ok_or(ERR)?;
            if !(0..=64).contains(&item_count) {
                return Err(ERR);
            }
            for _ in 0..item_count {
                let slen = read_varint(&mut rest).ok_or(ERR)?;
                if !(0..=256).contains(&slen) || rest.len() < slen as usize {
                    return Err(ERR);
                }
                let name =
                    std::str::from_utf8(&rest[..slen as usize]).map_err(|_| ERR)?;
                rest = &rest[slen as usize..];
                items.push(name_to_item(name).ok_or(ERR)?);
            }
        }
        entries.push((FactoryPos::new(dim, x, y, z), st, items));
    }
    if !rest.is_empty() {
        return Err(ERR);
    }
    Ok(entries)
}

/// Called by `load_world_in` after the dim overlays are applied. A missing
/// file means an empty factory; a corrupt file fails the whole load
/// (fail-closed, same contract as the protected world save).
#[cfg(feature = "mod-suite")]
fn live_create_load_sidecar(dir: &std::path::Path) -> Result<(), &'static str> {
    if !live_create_enabled() {
        return Ok(());
    }
    let path = dir.join(LIVE_CREATE_SIDECAR_FILE);
    let raw = match std::fs::read(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err("failed to read create factory save"),
    };
    let entries = live_create_decode_sidecar(&raw)?;
    live_create_with(|rt| rt.load_entries(entries));
    Ok(())
}

#[cfg(not(feature = "mod-suite"))]
#[allow(dead_code)]
#[inline(always)]
fn live_create_on_block_set(_dim: DimId, _x: i32, _y: i32, _z: i32, _old: u32, _st: u32) {}

#[cfg(not(feature = "mod-suite"))]
#[allow(dead_code)]
#[inline(always)]
fn live_create_wrench_use(
    _me: &Client,
    _dim: DimId,
    _x: i32,
    _y: i32,
    _z: i32,
    _clicked: u32,
    _held_slot: i16,
) -> bool {
    false
}

#[cfg(not(feature = "mod-suite"))]
#[allow(dead_code)]
#[inline(always)]
fn live_create_interact_block(
    _me: &Client,
    _dim: DimId,
    _x: i32,
    _y: i32,
    _z: i32,
    _clicked: u32,
    _held_slot: i16,
) -> bool {
    false
}

#[cfg(not(feature = "mod-suite"))]
#[inline(always)]
fn live_create_tick() {}

#[cfg(not(feature = "mod-suite"))]
#[inline(always)]
fn live_create_sync_on_join(_s: &mut TcpStream) {}

#[cfg(not(feature = "mod-suite"))]
#[inline(always)]
fn live_create_serialize() -> Option<Vec<u8>> {
    None
}

#[cfg(not(feature = "mod-suite"))]
#[inline(always)]
fn live_create_load_sidecar(_dir: &std::path::Path) -> Result<(), &'static str> {
    Ok(())
}

// ===========================================================================
// Default-build stubs: identical call sites, zero emitted code.
// ===========================================================================

#[cfg(not(feature = "mod-suite"))]
#[allow(dead_code)]
#[inline(always)]
fn live_mod_season_sync_frame(_world_ticks: u64, _biome_temp: f32) -> Option<(String, Vec<u8>)> {
    None
}

#[cfg(not(feature = "mod-suite"))]
#[allow(dead_code)]
#[inline(always)]
fn live_mod_emit_season_sync(_cid: u64, _world_ticks: u64, _biome_temp: f32) -> bool {
    false
}

#[cfg(not(feature = "mod-suite"))]
#[allow(dead_code)]
#[inline(always)]
fn live_mod_roll_halloween_headgear(
    _entity_id: &str,
    _month: u8,
    _day: u8,
    _roll: f32,
) -> Option<(&'static str, f32)> {
    None
}

#[cfg(not(feature = "mod-suite"))]
#[allow(dead_code)]
#[inline(always)]
fn live_mod_chest_holiday_variant(_block_id: &str, _month: u8, _day: u8) -> &'static str {
    "standard"
}

#[cfg(not(feature = "mod-suite"))]
#[allow(dead_code, clippy::too_many_arguments)]
#[inline(always)]
fn live_mod_try_qol_right_click_harvest(
    _enabled: bool,
    _block_id: &str,
    _age: u8,
    _max_age: u8,
    _drop_item: &str,
    _seed_item: &str,
    _raw_drops: u32,
    _seed_drops: u32,
) -> Option<(u8, u32, u32)> {
    None
}

/// Default builds do not emit mod custom payloads.
#[cfg(not(feature = "mod-suite"))]
#[allow(dead_code)]
#[inline(always)]
fn live_mod_custom_payload_frame(_channel: &str, _payload: &[u8]) -> Vec<u8> {
    Vec::new()
}

/// Magic adapters are unavailable in default builds.
#[cfg(not(feature = "mod-suite"))]
#[allow(dead_code)]
#[inline(always)]
fn live_mod_magic_set_attributes<T>(_uuid: [u8; 16], _attributes: T) {}

#[cfg(not(feature = "mod-suite"))]
#[allow(dead_code)]
#[inline(always)]
fn live_mod_magic_register_status<T>(_definition: T) {}

#[cfg(not(feature = "mod-suite"))]
#[allow(dead_code)]
#[inline(always)]
fn live_mod_magic_apply_status(_uuid: [u8; 16], _id: &str, _amplifier: u8, _duration_ticks: u32) {}

#[cfg(not(feature = "mod-suite"))]
#[allow(dead_code)]
#[inline(always)]
fn live_mod_magic_absorb_damage(_uuid: [u8; 16], damage: f32) -> f32 {
    damage
}

#[cfg(not(feature = "mod-suite"))]
#[allow(dead_code)]
#[inline(always)]
fn live_mod_magic_cast<T>(_uuid: [u8; 16], _spell: &T) {}

#[cfg(not(feature = "mod-suite"))]
#[allow(dead_code)]
#[inline(always)]
fn live_mod_magic_tick_player(_uuid: [u8; 16]) -> Option<()> {
    None
}

#[cfg(not(feature = "mod-suite"))]
#[allow(dead_code)]
#[inline(always)]
fn live_mod_magic_remove_player(_uuid: [u8; 16]) -> bool {
    false
}

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

/// No-op without `mod-suite`: no dynamic registry frame is ever emitted.
#[cfg(not(feature = "mod-suite"))]
#[inline(always)]
fn live_mod_registry_sync_on_join(_s: &mut TcpStream) {}

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

// ---------------------------------------------------------------------------
// Lane WIRE-MINECUT-ORBITAL-SYNC (`create-offload`): production entry points
// through the process-wide `mod_offload_bridge()` — the same bridge
// `mod_offload_tick` steps every game tick — for the two live offload
// surfaces that need no further packet plumbing yet:
//   * `live_ingest_external_graph_deltas` — an external/JVM graph source
//     (NeoForge Create bridge, datapack scheduler, remote mod runtime)
//     pushes incremental dirty-node deltas straight into the offload
//     kernels via `mod_offload::sync_bridge::OffloadSyncBridge`.
//   * `live_register_orbital_*` / `live_take_orbital_transitions` —
//     aeronautics sub-worlds register for per-tick orbital stepping; fired
//     `ShipDimensionTransition`s queue for the wire layer to drain.
// ---------------------------------------------------------------------------

/// Ingest one batch of external dirty-node deltas (kinetic, fluid,
/// transport, ME) into the live offload bridge. State deltas coalesce
/// last-wins and apply in canonical order inside
/// [`create_adapter::offload_bridge::OffloadBridge::ingest_external_deltas`].
#[cfg(feature = "create-offload")]
#[allow(dead_code)]
fn live_ingest_external_graph_deltas(
    deltas: &[create_adapter::offload_bridge::ExternalGraphDelta],
) -> Result<
    create_adapter::offload_bridge::BridgeSyncReport,
    create_adapter::offload_bridge::BridgeError,
> {
    mod_offload_bridge()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .offload
        .ingest_external_deltas(deltas)
}

/// Register a dimension the live orbital router can route 6-DOF ships
/// through (`"minecraft:overworld"`, a discovered `ad_astra:` dim, ...).
#[cfg(feature = "create-offload")]
#[allow(dead_code)]
fn live_register_orbital_dimension(
    dim: u32,
    descriptor: create_adapter::orbital::OrbitalDimension,
) {
    mod_offload_bridge()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .offload
        .register_orbital_dimension(dim, descriptor);
}

/// Register an orbital transition rule on the live bridge — e.g. an
/// `AscendCeiling` rule taking a ship at `Y >= 560` from the Overworld
/// into space.
#[cfg(feature = "create-offload")]
#[allow(dead_code)]
fn live_register_orbital_rule(rule: create_adapter::orbital::OrbitalTransitionRule) {
    mod_offload_bridge()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .offload
        .register_orbital_rule(rule);
}

/// Register a 6-DOF ship for per-tick orbital stepping by the live
/// bridge's router (invoked from `mod_offload_tick` -> `step_tick`).
#[cfg(feature = "create-offload")]
#[allow(dead_code)]
fn live_register_orbital_ship(
    ship: create_adapter::aeronautics::RigidBodySubWorld,
) -> Result<(), create_adapter::offload_bridge::OffloadError> {
    mod_offload_bridge()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .offload
        .register_orbital_ship(ship)
}

/// Attach rider `entity_id` to `ship_id` at a local offset from the ship's
/// center of mass so the rider rides through orbital transitions.
#[cfg(feature = "create-offload")]
#[allow(dead_code)]
fn live_attach_orbital_passenger(entity_id: i32, ship_id: u64, local_offset: [f64; 3]) -> bool {
    mod_offload_bridge()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .offload
        .attach_orbital_passenger(entity_id, ship_id, local_offset)
}

/// Drain the queued orbital dimension transitions — ship re-parenting plus
/// per-rider `TeleportIntent`s the wire layer executes.
#[cfg(feature = "create-offload")]
#[allow(dead_code)]
fn live_take_orbital_transitions()
-> Vec<create_adapter::orbital::ShipDimensionTransition> {
    mod_offload_bridge()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .offload
        .take_orbital_transitions()
}

#[cfg(feature = "mod-suite")]
#[allow(dead_code)]
fn live_format_romaji_chat(raw: &str, mode: megu_core::romaji_ime::RomajiChatMode) -> String {
    let cfg = megu_core::romaji_ime::RomajiChatConfig {
        mode,
        ..megu_core::romaji_ime::RomajiChatConfig::default()
    };
    megu_core::romaji_ime::format_romaji_chat(raw, &cfg)
}

#[cfg(feature = "mod-suite")]
#[allow(dead_code)]
fn live_tier25_batch_coalescer() -> &'static Mutex<megu_core::tier25_guard_batch::TickBatchCoalescer>
{
    static COALESCER: OnceLock<Mutex<megu_core::tier25_guard_batch::TickBatchCoalescer>> =
        OnceLock::new();
    COALESCER.get_or_init(|| Mutex::new(megu_core::tier25_guard_batch::TickBatchCoalescer::new()))
}

#[cfg(feature = "mod-suite")]
#[allow(dead_code)]
fn live_tier25_submit_guarded(
    loader: megu_core::tier25_guard_batch::LoaderLane,
    guard: &megu_core::tier25_guard_batch::GuardPredicate,
    ctx: &megu_core::tier25_guard_batch::GuardEvalContext,
    record: megu_core::tier25_guard_batch::BatchEventRecord,
) -> Result<bool, megu_core::tier25_guard_batch::Tier25Error> {
    live_tier25_batch_coalescer()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .submit_guarded(loader, guard, ctx, record)
}

#[cfg(feature = "mod-suite")]
#[allow(dead_code)]
fn live_tier25_flush_tick(
    tick: u64,
    loader: megu_core::tier25_guard_batch::LoaderLane,
) -> Option<Vec<u8>> {
    live_tier25_batch_coalescer()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .flush_tick(tick, loader)
}

#[cfg(feature = "mod-suite")]
struct ServerMathOptRuntime {
    plan: megu_core::hw_adaptive_engine::UnifiedOptimizationPlan,
    roofline: megu_core::math_opt_engine::RooflineModel,
    mspt_p95: megu_core::math_opt_engine::P2QuantileEstimator,
    mspt_p99: megu_core::math_opt_engine::P2QuantileEstimator,
    pid_budget: megu_core::math_opt_engine::PidBudgetController,
    budget_scale: f64,
    ticks_observed: u64,
}

#[cfg(feature = "mod-suite")]
fn live_server_opt_runtime() -> &'static Mutex<ServerMathOptRuntime> {
    static OPT: OnceLock<Mutex<ServerMathOptRuntime>> = OnceLock::new();
    OPT.get_or_init(|| {
        let profile = megu_core::hw_adaptive_engine::detect_universal_hardware_profile();
        let plan = megu_core::hw_adaptive_engine::build_unified_optimization_plan(&profile, None);
        let roofline = plan.roofline_model();
        Mutex::new(ServerMathOptRuntime {
            plan,
            roofline,
            mspt_p95: megu_core::math_opt_engine::P2QuantileEstimator::new(0.95),
            mspt_p99: megu_core::math_opt_engine::P2QuantileEstimator::new(0.99),
            pid_budget: megu_core::math_opt_engine::PidBudgetController::new(50.0),
            budget_scale: 1.0,
            ticks_observed: 0,
        })
    })
}

#[cfg(feature = "mod-suite")]
fn live_observe_server_tick_mspt(mspt: f64) {
    if let Ok(mut rt) = live_server_opt_runtime().try_lock() {
        rt.mspt_p95.observe(mspt);
        rt.mspt_p99.observe(mspt);
        let p95 = rt.mspt_p95.estimate().unwrap_or(mspt);
        rt.budget_scale = rt.pid_budget.update(p95);
        rt.ticks_observed = rt.ticks_observed.saturating_add(1);
    }
}

#[cfg(not(feature = "mod-suite"))]
#[inline]
fn live_observe_server_tick_mspt(_mspt: f64) {}

#[cfg(feature = "mod-suite")]
#[allow(dead_code)]
fn live_server_opt_summary() -> (f64, f64, f64, f64, usize) {
    let rt = live_server_opt_runtime()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    (
        rt.mspt_p95.estimate().unwrap_or(0.0),
        rt.mspt_p99.estimate().unwrap_or(0.0),
        rt.budget_scale,
        rt.roofline.ridge_intensity,
        rt.plan.compute.workers.background_threads as usize,
    )
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
        assert!(live_mod_custom_payload_frame("megu:magic_v1", b"MAG1").is_empty());
        assert_eq!(live_mod_magic_tick_player([1; 16]), None);
        assert!(!live_mod_magic_remove_player([1; 16]));
        assert_eq!(live_mod_magic_absorb_damage([1; 16], 3.0), 3.0);
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

#[cfg(all(test, feature = "mod-suite"))]
mod live_mod_bridge_magic_tests {
    use super::*;

    static MAGIC_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn reset_magic() {
        *live_magic_state().lock().unwrap_or_else(|e| e.into_inner()) = LiveMagicState::default();
    }

    fn snapshot(uuid: [u8; 16]) -> lmb_magic::MagicSyncState {
        live_magic_state()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .players
            .get(&uuid)
            .expect("registered player")
            .snapshot()
    }

    fn spell(id: &str, cast: lmb_magic::SpellCastState) -> lmb_magic::SpellDefinition {
        lmb_magic::SpellDefinition {
            id: id.into(),
            mana_cost: 10.0,
            base_cooldown_ticks: 4,
            cast,
            interrupt_on_damage: false,
            interrupt_on_move: false,
            steps: vec![lmb_magic::SpellNode {
                form: lmb_magic::SpellForm::SelfCast,
                effects: vec![lmb_magic::SpellEffect::Heal { base_amount: 5.0 }],
            }],
        }
    }

    fn decode_custom_frame(frame: &[u8]) -> (String, &[u8]) {
        let mut rest = frame;
        let declared_len = read_varint(&mut rest).expect("frame length");
        assert_eq!(declared_len as usize, rest.len());
        assert_eq!(read_varint(&mut rest), Some(0x19));
        let channel_len = read_varint(&mut rest).expect("channel length") as usize;
        let channel = std::str::from_utf8(&rest[..channel_len])
            .expect("utf-8 channel")
            .to_owned();
        (channel, &rest[channel_len..])
    }

    #[test]
    fn live_mod_bridge_mana_regen_and_sync_deltas() {
        let _guard = MAGIC_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_magic();
        let uuid = [1; 16];
        live_mod_magic_set_attributes(
            uuid,
            lmb_magic::MagicAttributes {
                max_mana: 50.0,
                regen_per_tick: 3.0,
                ..lmb_magic::MagicAttributes::default()
            },
        )
        .unwrap();
        {
            let mut state = live_magic_state().lock().unwrap_or_else(|e| e.into_inner());
            state
                .players
                .get_mut(&uuid)
                .unwrap()
                .caster
                .mana
                .set_current(43.0);
        }
        let first = live_mod_magic_tick_player(uuid).unwrap();
        assert_eq!(first.mana_delta, 3.0);
        let (channel, body) = decode_custom_frame(first.frame.as_ref().unwrap());
        assert_eq!(channel, lmb_magic::MAGIC_CHANNEL);
        assert_eq!(lmb_magic::decode_magic_sync(body).unwrap().mana, 46.0);
        assert_eq!(live_mod_magic_tick_player(uuid).unwrap().mana_delta, 3.0);
        assert_eq!(live_mod_magic_tick_player(uuid).unwrap().mana_delta, 1.0);
        let stable = live_mod_magic_tick_player(uuid).unwrap();
        assert_eq!(stable.mana_delta, 0.0);
        assert!(stable.frame.is_none());
        reset_magic();
    }

    #[test]
    fn live_mod_bridge_spell_cast_cooldown_and_silence() {
        let _guard = MAGIC_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_magic();
        let uuid = [2; 16];
        let mut expensive = spell("test:expensive", lmb_magic::SpellCastState::Instant);
        expensive.mana_cost = 101.0;
        assert_eq!(
            live_mod_magic_cast(uuid, &expensive).unwrap_err(),
            LiveMagicError::Cast(lmb_magic::CastError::InsufficientMana {
                needed: 101.0,
                available: 100.0,
            })
        );
        assert_eq!(snapshot(uuid).mana, 100.0);
        let timed = spell("test:cast", lmb_magic::SpellCastState::cast_time(2));
        assert!(live_mod_magic_cast(uuid, &timed).unwrap().events.is_empty());
        assert!(live_mod_magic_tick_player(uuid)
            .unwrap()
            .cast_events
            .is_empty());
        let completed = live_mod_magic_tick_player(uuid).unwrap();
        assert_eq!(
            completed.cast_events,
            vec![lmb_magic::CastEvent::Completed {
                spell_id: timed.id.clone()
            }]
        );
        assert_eq!(completed.spell_plans.len(), 1);
        assert!(completed.execution_errors.is_empty());
        assert!(completed.cooldowns_changed);
        assert_eq!(
            live_mod_magic_cast(uuid, &timed).unwrap_err(),
            LiveMagicError::Cast(lmb_magic::CastError::OnCooldown { remaining_ticks: 4 })
        );
        for _ in 0..4 {
            live_mod_magic_tick_player(uuid);
        }
        live_mod_magic_register_status(lmb_magic::StatusEffectDefinition {
            id: "test:silence".into(),
            silenced: true,
            ..lmb_magic::StatusEffectDefinition::default()
        })
        .unwrap();
        live_mod_magic_apply_status(uuid, "test:silence", 0, 2).unwrap();
        assert_eq!(
            live_mod_magic_cast(uuid, &timed).unwrap_err(),
            LiveMagicError::Silenced
        );
        live_mod_magic_tick_player(uuid);
        assert_eq!(
            live_mod_magic_cast(uuid, &timed).unwrap_err(),
            LiveMagicError::Silenced
        );
        assert_eq!(
            live_mod_magic_tick_player(uuid).unwrap().statuses.expired,
            vec!["test:silence"]
        );
        assert!(live_mod_magic_cast(uuid, &timed).is_ok());
        reset_magic();
    }

    #[test]
    fn live_mod_bridge_channel_pulse_and_out_of_mana() {
        let _guard = MAGIC_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_magic();
        let uuid = [3; 16];
        live_mod_magic_set_attributes(
            uuid,
            lmb_magic::MagicAttributes {
                max_mana: 7.0,
                ..lmb_magic::MagicAttributes::default()
            },
        )
        .unwrap();
        let mut spell = spell(
            "test:channel",
            lmb_magic::SpellCastState::channeled(2, 4, 3.0),
        );
        spell.mana_cost = 2.0;
        live_mod_magic_cast(uuid, &spell).unwrap();
        live_mod_magic_tick_player(uuid);
        let pulse = live_mod_magic_tick_player(uuid).unwrap();
        assert_eq!(
            pulse.cast_events,
            vec![lmb_magic::CastEvent::Pulse {
                spell_id: spell.id.clone()
            }]
        );
        assert_eq!(pulse.mana_delta, -3.0);
        live_mod_magic_tick_player(uuid);
        let stopped = live_mod_magic_tick_player(uuid).unwrap();
        assert_eq!(
            stopped.cast_events,
            vec![lmb_magic::CastEvent::Interrupted {
                spell_id: spell.id,
                reason: lmb_magic::InterruptReason::OutOfMana,
            }]
        );
        assert!(stopped.spell_plans.is_empty());
        assert_eq!(snapshot(uuid).mana, 2.0);

        let sufficient_uuid = [7; 16];
        let mut success = self::spell(
            "test:successful_channel",
            lmb_magic::SpellCastState::channeled(2, 4, 3.0),
        );
        success.mana_cost = 2.0;
        live_mod_magic_cast(sufficient_uuid, &success).unwrap();
        for _ in 0..3 {
            live_mod_magic_tick_player(sufficient_uuid);
        }
        let finished = live_mod_magic_tick_player(sufficient_uuid).unwrap();
        assert_eq!(
            finished.cast_events,
            vec![
                lmb_magic::CastEvent::Pulse {
                    spell_id: success.id.clone()
                },
                lmb_magic::CastEvent::Completed {
                    spell_id: success.id.clone()
                },
            ]
        );
        assert_eq!(finished.spell_plans.len(), 1);
        assert_eq!(snapshot(sufficient_uuid).cooldowns[&success.id], 4);
        reset_magic();
    }

    #[test]
    fn live_mod_bridge_status_dot_hot_shield_and_cc() {
        let _guard = MAGIC_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_magic();
        let uuid = [4; 16];
        for status in [
            lmb_magic::StatusEffectDefinition {
                id: "test:burn".into(),
                max_amplifier: 1,
                pulse_interval_ticks: 2,
                damage_per_pulse: 2.0,
                ..lmb_magic::StatusEffectDefinition::default()
            },
            lmb_magic::StatusEffectDefinition {
                id: "test:mend".into(),
                pulse_interval_ticks: 1,
                heal_per_pulse: 1.5,
                ..lmb_magic::StatusEffectDefinition::default()
            },
            lmb_magic::StatusEffectDefinition {
                id: "test:ward".into(),
                absorption_per_level: 4.0,
                rooted: true,
                ..lmb_magic::StatusEffectDefinition::default()
            },
        ] {
            live_mod_magic_register_status(status).unwrap();
        }
        live_mod_magic_apply_status(uuid, "test:burn", 1, 4).unwrap();
        live_mod_magic_apply_status(uuid, "test:mend", 0, 4).unwrap();
        live_mod_magic_apply_status(uuid, "test:ward", 0, 4).unwrap();
        let first = live_mod_magic_tick_player(uuid).unwrap();
        assert_eq!(first.statuses.damage, 0.0);
        assert_eq!(first.statuses.healing, 1.5);
        assert_eq!(snapshot(uuid).statuses.len(), 3);
        assert!(first.crowd_control.rooted);
        assert_eq!(live_mod_magic_absorb_damage(uuid, 3.0), 0.0);
        assert_eq!(
            live_magic_state()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .players
                .get(&uuid)
                .unwrap()
                .effects
                .total_absorption(),
            1.0
        );
        let second = live_mod_magic_tick_player(uuid).unwrap();
        assert_eq!(second.statuses.damage, 4.0);
        assert_eq!(second.statuses.healing, 1.5);
        let decoded =
            lmb_magic::decode_magic_sync(decode_custom_frame(second.frame.as_ref().unwrap()).1)
                .unwrap();
        assert_eq!(
            decoded
                .statuses
                .iter()
                .find(|s| s.id == "test:ward")
                .unwrap()
                .absorption_remaining,
            1.0
        );
        live_mod_magic_tick_player(uuid);
        let expired = live_mod_magic_tick_player(uuid).unwrap();
        assert_eq!(expired.statuses.expired.len(), 3);
        assert!(!expired.crowd_control.rooted);
        assert!(snapshot(uuid).statuses.is_empty());
        reset_magic();
    }

    #[test]
    fn live_mod_bridge_mag1_and_motion_vfx_anim_wire_frames() {
        let _guard = MAGIC_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_magic();
        let uuid = [5; 16];
        let instant = spell("test:instant", lmb_magic::SpellCastState::Instant);
        assert_eq!(
            live_mod_magic_cast(uuid, &instant)
                .unwrap()
                .spell_plans
                .len(),
            1
        );
        let frame = live_mod_magic_tick_player(uuid).unwrap().frame.unwrap();
        let (channel, payload) = decode_custom_frame(&frame);
        assert_eq!(channel, "megu:magic_v1");
        assert!(payload.starts_with(b"MAG1"));
        let decoded = lmb_magic::decode_magic_sync(payload).expect("MAG1 decode");
        assert_eq!(decoded, snapshot(uuid));
        assert_eq!(lmb_magic::encode_magic_sync(&decoded).unwrap(), payload);
        assert_eq!(decoded.mana, 90.0);
        assert_eq!(decoded.cooldowns["test:instant"], 3);
        for channel in ["megu:vfx_v1", "megu:anim_v1", "megu:motion_v1"] {
            let frame = live_mod_custom_payload_frame(channel, b"\0\x01\xff");
            let (actual_channel, body) = decode_custom_frame(&frame);
            assert_eq!(actual_channel, channel);
            assert_eq!(body, b"\0\x01\xff");
        }
        reset_magic();
    }

    #[test]
    fn live_mod_bridge_disconnect_cleanup_and_player_limit() {
        let _guard = MAGIC_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_magic();
        let uuid = [6; 16];
        live_mod_magic_set_attributes(uuid, lmb_magic::MagicAttributes::default()).unwrap();
        assert!(live_mod_magic_remove_player(uuid));
        assert!(!live_mod_magic_remove_player(uuid));
        assert!(live_mod_magic_tick_player(uuid).is_none());
        for value in 0..LIVE_MAGIC_MAX_PLAYERS {
            live_mod_magic_set_attributes(
                (value as u128).to_be_bytes(),
                lmb_magic::MagicAttributes::default(),
            )
            .unwrap();
        }
        assert_eq!(
            live_mod_magic_cast(
                [255; 16],
                &spell("test:cap", lmb_magic::SpellCastState::Instant)
            )
            .unwrap_err(),
            LiveMagicError::PlayerLimit
        );
        assert!(live_mod_magic_remove_player([0; 16]));
        assert!(live_mod_magic_cast(
            [255; 16],
            &spell("test:cap", lmb_magic::SpellCastState::Instant)
        )
        .is_ok());
        reset_magic();
    }
}

/// Suite-enabled tests: real jars, real `.class` bytecode, real wire frames.
/// The class/ZIP fixtures below are hand-assembled JVMS §4 and APPNOTE
/// bytes, the same proven builder shapes as `mixin_jar_scan.rs` — no `javac`,
/// no external JARs, no crates.
#[cfg(all(test, feature = "mod-suite"))]
mod wire_mods_tests {
    use super::*;
    use mod_adapter_core::auto_adapter::auto_adapt_jar_bytes;
    use mod_adapter_core::class_mixin_scanner::{scan_jar_mixins_with_options, JarScanOptions};
    use mod_adapter_core::fallback::REASON_SUPERSEDED;
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

    /// A minimal `fabric.mod.json` giving a fixture jar a manifest-declared
    /// mod id — without loader metadata the adapter (correctly) records a
    /// no-manifest blocker and the jar can never leave Tier 3.
    fn lmb_fabric_manifest(mod_id: &str) -> Vec<u8> {
        format!(r#"{{"schemaVersion":1,"id":"{mod_id}","version":"1.0.0","environment":"*"}}"#)
            .into_bytes()
    }

    /// A `@Mixin(target)` class with one `@Inject(method, at = HEAD,
    /// cancellable)` method — the liftable-injection fixture shape. The
    /// manifest declares the package's last segment as the mod id.
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
        let manifest = lmb_fabric_manifest(mod_pkg.rsplit('/').next().unwrap_or(mod_pkg));
        lmb_test_jar(&[
            ("fabric.mod.json", manifest.as_slice()),
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
        let manifest = lmb_fabric_manifest(mod_pkg.rsplit('/').next().unwrap_or(mod_pkg));
        lmb_test_jar(&[
            ("fabric.mod.json", manifest.as_slice()),
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
            "megu-wire-mods-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A fresh client/server `TcpStream` pair: `send` is the end the server
    /// writes to, `peer` reads exactly those bytes.
    pub(crate) fn lmb_socket_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let send = TcpStream::connect(addr).unwrap();
        let (peer, _) = listener.accept().unwrap();
        (send, peer)
    }

    // ---------- loopback client fixture (same shape as play_protocol's) ----------

    pub(crate) fn lmb_client(name: &str, eid: i32) -> (Arc<Client>, std::net::TcpStream) {
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
            use_on_resend: Mutex::new(None),
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
        let _guard = crate::world_guard();
        let _serial = WIRE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = lmb_temp_dir("bundle-boot");
        let adapted = dir.join("adapted");
        std::fs::create_dir_all(&adapted).unwrap();
        for (path, id, channel) in [
            (dir.join("first.bundle.json"), "first:mod", "first:sync"),
            (
                adapted.join("second.bundle.json"),
                "second:mod",
                "second:sync",
            ),
        ] {
            std::fs::write(path, format!(
                r#"{{"schema":"megu-mod-bundle/1","mod_id":"{id}","mc_version_range":"[1.21.1,1.21.1]","portals":[],"progression_barriers":[],"biome_regions":[],"creatures":[],"custom_channels":["{channel}"]}}"#
            )).unwrap();
        }
        // A parsed but incompatible bundle must not reserve the id of a
        // subsequent valid bundle during the same boot.
        std::fs::write(dir.join("a-incompatible.bundle.json"),
            r#"{"schema":"megu-mod-bundle/1","mod_id":"first:mod","mc_version_range":"[1.20.1,1.20.1]","portals":[],"progression_barriers":[],"biome_regions":[],"creatures":[],"custom_channels":[]}"#
        ).unwrap();
        std::fs::write(dir.join("invalid.bundle.json"), r#"{"schema":"wrong"}"#).unwrap();
        let previous = std::env::var_os("MEGU_MODS_DIR");
        std::env::set_var("MEGU_MODS_DIR", &dir);
        live_mod_suite_boot();
        if let Some(value) = previous {
            std::env::set_var("MEGU_MODS_DIR", value);
        } else {
            std::env::remove_var("MEGU_MODS_DIR");
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
        let _guard = crate::world_guard();
        let _serial = WIRE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = lmb_temp_dir("jar-boot");
        std::fs::write(dir.join("sodium-0.6.jar"), lmb_test_jar(&[("x.bin", b"x")])).unwrap();
        std::fs::write(
            dir.join("delightaddon-1.jar"),
            lmb_culinary_jar("delightaddon"),
        )
        .unwrap();
        std::fs::write(
            dir.join("safemod-1.jar"),
            lmb_inject_jar(
                "com/safemod",
                "net/minecraft/world/entity/LivingEntity",
                "hurt",
            ),
        )
        .unwrap();
        std::fs::write(
            dir.join("redirectmod-1.jar"),
            lmb_redirect_jar(
                "com/redirectmod",
                "net/minecraft/world/entity/LivingEntity",
                "hurt",
            ),
        )
        .unwrap();
        let previous = std::env::var_os("MEGU_MODS_DIR");
        std::env::set_var("MEGU_MODS_DIR", &dir);
        live_mod_suite_boot();
        if let Some(value) = previous {
            std::env::set_var("MEGU_MODS_DIR", value);
        } else {
            std::env::remove_var("MEGU_MODS_DIR");
        }
        let suite = live_mod_suite().lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(suite.auto_adapted.len(), 3);
        assert_eq!(suite.culinary_stats.cooking_recipes, 1);
        assert_eq!(
            suite
                .auto_adapted
                .iter()
                .find(|m| m.mod_id == "delightaddon")
                .unwrap()
                .tag_files,
            1
        );
        let safe = suite
            .auto_adapted
            .iter()
            .find(|m| m.mod_id == "safemod")
            .unwrap();
        assert_eq!(safe.tier, AdaptationTier::Tier2LiftedShim);
        assert_eq!(safe.lifted_hooks.len(), 1);
        assert_eq!(suite.lifted_hooks.len(), 1);
        assert_eq!(
            suite.lifted_hooks[0].1.hook,
            StandardEventHook::EntityDamagePre
        );
        let fallback = suite
            .auto_adapted
            .iter()
            .find(|m| m.mod_id == "redirectmod")
            .unwrap();
        assert_eq!(fallback.tier, AdaptationTier::Tier3FallbackRequired);
        assert!(fallback.lifted_hooks.is_empty());
        assert!(suite
            .reports
            .iter()
            .any(|r| r.mod_id == "sodium" && r.tier == AdaptationTier::AutoSkipped));
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
                "com/redirectmod",
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
        assert_eq!(sodium.tier, AdaptationTier::AutoSkipped);
        assert!(
            sodium.diagnostic.contains(REASON_SUPERSEDED),
            "{}",
            sodium.diagnostic
        );
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

        // The safe @Inject jar promotes to the lifted Tier 2 shim.
        let safe = suite
            .reports
            .iter()
            .find(|r| r.mod_id == "safemod")
            .expect("safemod report");
        assert_eq!(
            safe.tier,
            AdaptationTier::Tier2LiftedShim,
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
        assert_eq!(redirect.tier, AdaptationTier::Tier3FallbackRequired);
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
        let options = AutoAdaptOptions {
            target: LIVE_MOD_MC_TARGET,
            ..AutoAdaptOptions::default()
        };
        // No `.class` members at all -> pure data jar -> native bundle.
        let data_jar = lmb_test_jar(&[("data/x/recipes/a.json", br#"{"type":"x:y"}"#)]);
        let result = auto_adapt_jar_bytes(&data_jar, "datamod.jar", &options).unwrap();
        assert_eq!(result.tier, AdaptationTier::NativeBundle);
        assert_eq!(result.data.java_class_members, 0);
        assert_eq!(
            result.bundle.as_ref().map(|b| b.mod_id.as_str()),
            Some("datamod"),
            "the synthesized bundle carries the filename-inferred mod id"
        );

        // Bytecode present but no mixin sites -> fail-closed Tier 3. The
        // manifest keeps the loader-metadata blocker out so the exact
        // bytecode blocker is what lands.
        let cls = build_class("com/codemod/Thing", vec![], vec![]);
        let manifest = lmb_fabric_manifest("codemod");
        let code_jar = lmb_test_jar(&[
            ("fabric.mod.json", manifest.as_slice()),
            ("com/codemod/Thing.class", cls.as_slice()),
        ]);
        let result = auto_adapt_jar_bytes(&code_jar, "codemod.jar", &options).unwrap();
        assert_eq!(result.tier, AdaptationTier::Tier3FallbackRequired);
        assert!(result
            .mixin_scan
            .as_ref()
            .is_some_and(|scan| scan.sites.is_empty()));
        assert!(
            result
                .blockers
                .iter()
                .any(|b| b.contains("no liftable mixin sites")),
            "{:?}",
            result.blockers
        );

        // A truncated jar fails closed: the adaptation pass reports Err
        // rather than guessing at a mod it cannot read.
        assert!(auto_adapt_jar_bytes(&[0x50, 0x4b, 1, 2], "badmod.jar", &options).is_err());
    }

    // ---------- auto-adapter unified path ----------

    #[test]
    fn live_mod_bridge_renamed_superseded_mod_auto_skips() {
        let _serial = WIRE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        fresh_suite();
        let dir = lmb_temp_dir("renamed-superseded");
        // The filename infers "harmless-tweaks" — only the manifest-declared
        // id reveals the superseded perf mod; `auto_adapt_mods_dir` still
        // skips it before a single `.class` byte is read.
        let manifest = br#"{"schemaVersion":1,"id":"lithium","version":"9.9.9","environment":"*"}"#;
        std::fs::write(
            dir.join("harmless-tweaks-1.0.jar"),
            lmb_test_jar(&[("fabric.mod.json", manifest)]),
        )
        .unwrap();
        let mut suite = LiveModSuite::new();
        suite.scan_mods_dir(&dir);
        let report = suite
            .reports
            .iter()
            .find(|r| r.mod_id == "lithium")
            .expect("lithium report");
        assert_eq!(report.tier, AdaptationTier::AutoSkipped);
        assert_eq!(
            report.classes_scanned, 0,
            "superseded jars are never scanned"
        );
        assert!(
            report.diagnostic.contains(REASON_SUPERSEDED),
            "{}",
            report.diagnostic
        );
        // A skipped jar reports its skip but onboards nothing.
        assert!(suite.auto_adapted.iter().all(|m| m.mod_id != "lithium"));
        assert!(suite.lifted_hooks.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn live_mod_bridge_synthesized_and_standalone_bundles_live_after_boot() {
        let _serial = WIRE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        fresh_suite();
        let dir = lmb_temp_dir("synth-register");
        // A standalone bundle carrying a progression gate + channel, next
        // to a native data jar the adapter synthesizes a bundle for.
        std::fs::write(
            dir.join("farmersdelight-1.0.jar"),
            lmb_culinary_jar("farmersdelight"),
        )
        .unwrap();
        std::fs::write(
            dir.join("gated.bundle.json"),
            r#"{"schema":"megu-mod-bundle/1","mod_id":"gated_pack","mc_version_range":"[1.21,)","portals":[],"progression_barriers":[{"id":"gated_pack:seal","dimension":"minecraft:overworld","target_biomes":["minecraft:plains"],"required_advancements":["minecraft:story/mine_diamond"],"block_break_protected":true,"block_place_protected":false,"enforce_effects":[],"periodic_damage":null}],"biome_regions":[],"creatures":[],"custom_channels":["gated_pack:sync"]}"#,
        )
        .unwrap();
        let mut suite = LiveModSuite::new();
        suite.scan_mods_dir(&dir);

        // Standalone files register first; the adapter's synthesized
        // `megu-mod-bundle/1` for the native jar joins the same
        // runtime — one registry, zero manual steps.
        assert_eq!(
            suite.unified.installed_mod_ids(),
            &["gated_pack", "farmersdelight"]
        );
        assert!(suite.unified.is_channel_registered("gated_pack:sync"));
        // The standalone bundle's rules already gate in the barrier engine
        // the boot scan refreshed.
        assert!(matches!(
            suite.barriers.check_progression(
                "minecraft:overworld",
                "minecraft:plains",
                &BTreeSet::new(),
                false,
            ),
            ProgressionVerdict::Locked { .. }
        ));
        // And the native jar's discovered culinary content is live too.
        assert!(suite
            .culinary
            .food_spec("farmersdelight:test_soup")
            .is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn live_mod_bridge_dispatch_hook_answers_lifted_bindings_after_boot() {
        let _guard = crate::world_guard();
        let _serial = WIRE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        fresh_suite();
        let dir = lmb_temp_dir("dispatch");
        std::fs::write(
            dir.join("safemod-1.0.jar"),
            lmb_inject_jar(
                "com/safemod",
                "net/minecraft/world/entity/LivingEntity",
                "hurt",
            ),
        )
        .unwrap();
        let previous = std::env::var_os("MEGU_MODS_DIR");
        std::env::set_var("MEGU_MODS_DIR", &dir);
        live_mod_suite_boot();
        if let Some(value) = previous {
            std::env::set_var("MEGU_MODS_DIR", value);
        } else {
            std::env::remove_var("MEGU_MODS_DIR");
        }

        // The lifted JAR's binding fires EntityDamagePre — live-queryable
        // the instant boot returns.
        let fired = live_mod_dispatch_hook(StandardEventHook::EntityDamagePre);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].0, "safemod");
        assert_eq!(fired[0].1.site.mixin_class, "com/safemod/mixin/TestMixin");
        assert!(fired[0].1.cancellable);
        assert!(live_mod_dispatch_hook(StandardEventHook::BlockBreakPre).is_empty());

        // An inactive suite dispatches nothing.
        fresh_suite();
        assert!(live_mod_dispatch_hook(StandardEventHook::EntityDamagePre).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
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

    #[test]
    fn live_mod_bridge_season_sync_is_ssn1_and_suppressed_per_player() {
        let _guard = crate::world_guard();
        use mod_adapter_core::season_primitives::{decode_ssn1, SSN1_FRAME_LEN, SSN1_MAGIC};

        let _serial = WIRE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        fresh_suite();
        live_mod_suite().lock().unwrap_or_else(|e| e.into_inner()).active = true;
        let (player_a, mut peer_a) = lmb_client("wire_season_a", 977_201);
        let (player_b, mut peer_b) = lmb_client("wire_season_b", 977_202);
        clients()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend([Arc::clone(&player_a), Arc::clone(&player_b)]);

        let (channel, payload) = live_mod_season_sync_frame(0, 0.8).unwrap();
        assert_eq!(channel, "megu:season");
        assert_eq!(payload.len(), SSN1_FRAME_LEN);
        assert_eq!(&payload[..4], SSN1_MAGIC);
        assert!(decode_ssn1(&payload).is_some());
        assert!(live_mod_emit_season_sync(977_201, 0, 0.8));
        let mut stash_a = Vec::new();
        let (id, body) = lmb_read_frame(&mut peer_a, &mut stash_a).unwrap();
        assert_eq!(id, LIVE_MOD_S2C_CUSTOM_PAYLOAD_ID);
        assert_eq!(lmb_decode_payload(&body), (channel, payload.clone()));
        assert!(!live_mod_emit_season_sync(977_201, 0, 0.8));
        assert!(lmb_silent(&mut peer_a));

        assert!(live_mod_emit_season_sync(977_202, 0, 0.8));
        let (id, body) = lmb_read_frame(&mut peer_b, &mut Vec::new()).unwrap();
        assert_eq!(id, LIVE_MOD_S2C_CUSTOM_PAYLOAD_ID);
        assert_eq!(lmb_decode_payload(&body), ("megu:season".into(), payload.clone()));
        assert!(live_mod_emit_season_sync(977_201, 24_000 * 8, 0.8));
        let (_, body) = lmb_read_frame(&mut peer_a, &mut stash_a).unwrap();
        let (_, changed) = lmb_decode_payload(&body);
        assert!(decode_ssn1(&changed).is_some());
        assert_ne!(changed, payload);

        assert!(!live_mod_emit_season_sync(u64::MAX, 0, 0.8));
        assert!(live_mod_season_sync_frame(0, f32::NAN).is_none());
        clients()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|client| client.eid != 977_201 && client.eid != 977_202);
        fresh_suite();
    }

    #[test]
    fn live_mod_bridge_holiday_and_harvest_adapters() {
        assert_eq!(
            live_mod_roll_halloween_headgear("minecraft:zombie", 10, 31, 0.01),
            Some(("minecraft:jack_o_lantern", 0.0))
        );
        assert_eq!(
            live_mod_roll_halloween_headgear("minecraft:skeleton", 10, 31, 0.1),
            Some(("minecraft:carved_pumpkin", 0.0))
        );
        assert_eq!(live_mod_roll_halloween_headgear("minecraft:creeper", 10, 31, 0.01), None);
        assert_eq!(live_mod_roll_halloween_headgear("minecraft:zombie", 10, 30, 0.01), None);
        assert_eq!(live_mod_roll_halloween_headgear("minecraft:zombie", 13, 31, 0.01), None);
        assert_eq!(live_mod_chest_holiday_variant("minecraft:chest", 12, 25), "christmas_present");
        assert_eq!(live_mod_chest_holiday_variant("minecraft:trapped_chest", 12, 24), "christmas_present");
        assert_eq!(live_mod_chest_holiday_variant("minecraft:ender_chest", 12, 25), "standard");
        assert_eq!(live_mod_chest_holiday_variant("minecraft:chest", 12, 27), "standard");
        assert_eq!(
            live_mod_try_qol_right_click_harvest(
                true, "minecraft:wheat", 7, 7, "minecraft:wheat", "minecraft:wheat_seeds", 3, 2
            ),
            Some((0, 3, 1))
        );
        assert_eq!(
            live_mod_try_qol_right_click_harvest(
                true, "minecraft:carrots", 7, 7, "minecraft:carrot", "minecraft:carrot", 3, 0
            ),
            Some((0, 2, 0))
        );
        assert!(live_mod_try_qol_right_click_harvest(
            true, "minecraft:wheat", 7, 7, "minecraft:wheat", "minecraft:wheat_seeds", 3, 0
        ).is_none());
        assert!(live_mod_try_qol_right_click_harvest(
            false, "minecraft:wheat", 7, 7, "minecraft:wheat", "minecraft:wheat_seeds", 3, 2
        ).is_none());
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

        // The join hook emits one 0x19 frame on `megu:mod_sync_v1`
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
            megu_core::version::TargetVersion::V1_21_1.protocol(),
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
            "com/redirectmod",
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
        let get = asm.cp.method_ref("megu/api/Event", "getDamage", "()F");
        let set = asm.cp.method_ref("megu/api/Event", "setDamage", "(F)V");
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
            "(Lmegu/api/Event;)V",
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
    #[cfg(feature = "create-offload")]
    #[test]
    fn mega_w3_live_mod_bridge_culinary_season_magic_and_harvest_execute() {
        use mod_adapter_core::dimension_mod as dim_mod;

        let _serial = WIRE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        fresh_suite();
        *live_magic_state().lock().unwrap_or_else(|e| e.into_inner()) =
            LiveMagicState::default();
        *dimension_mod_bridge().lock().unwrap_or_else(|e| e.into_inner()) =
            DimensionModRuntimeBridge::new();

        // ---- culinary: a real JAR scan feeds the live dispatchers ----
        let dir = lmb_temp_dir("mega-w3");
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
        suite.active = true;
        *live_mod_suite().lock().unwrap_or_else(|e| e.into_inner()) = suite;

        // Heated pot with carrot + bowl completes the scanned recipe on its deadline.
        let key = (DimId::Overworld, 30, 64, 30);
        let mut pot = CookingPotState::new();
        assert!(pot.insert_input("minecraft:carrot", 1));
        pot.container = Some(("minecraft:bowl".into(), 1));
        pot.heated = true;
        live_mod_set_cooking_pot(key.0, key.1, key.2, key.3, pot, 500_000);
        assert!(live_mod_tick_cooking_pots(500_199).is_empty());
        let completed = live_mod_tick_cooking_pots(500_200);
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].0, key);
        assert_eq!(completed[0].1.result_item, "farmersdelight:test_soup");
        assert_eq!(completed[0].1.completed_tick, 500_200);
        // The unknown addon's cutting recipe resolves through the same registry.
        let cut = live_mod_cutting_board_use("minecraft:carrot", "unknownaddon:knife", 7)
            .expect("addon cutting recipe");
        assert_eq!(cut.outputs, [("unknownaddon:carrot_slices".to_string(), 3)]);
        live_mod_remove_cooking_pot(key.0, key.1, key.2, key.3);

        // ---- SSN1 season sync on a real socket, deduplicated per player ----
        let (season_player, mut season_peer) = lmb_client("mega_w3_season", 977_300);
        clients()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Arc::clone(&season_player));
        assert!(live_mod_emit_season_sync(977_300, 0, 0.8));
        let mut stash = Vec::new();
        let (id, body) = lmb_read_frame(&mut season_peer, &mut stash).unwrap();
        assert_eq!(id, LIVE_MOD_S2C_CUSTOM_PAYLOAD_ID);
        let (channel, payload) = lmb_decode_payload(&body);
        assert_eq!(channel, "megu:season");
        assert!(payload.starts_with(b"SSN1"));
        assert!(!live_mod_emit_season_sync(977_300, 0, 0.8));
        assert!(lmb_silent(&mut season_peer));
        clients()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|c| c.eid != 977_300);

        // ---- right-click harvest: seed deducted from the loot, crop replanted ----
        assert_eq!(
            live_mod_try_qol_right_click_harvest(
                true, "minecraft:wheat", 7, 7, "minecraft:wheat", "minecraft:wheat_seeds", 3, 2
            ),
            Some((0, 3, 1))
        );
        assert!(live_mod_try_qol_right_click_harvest(
            true, "minecraft:wheat", 7, 7, "minecraft:wheat", "minecraft:wheat_seeds", 3, 0
        )
        .is_none());

        // ---- accessories: per-player slots + attribute aggregation ----
        let attr = "minecraft:generic.movement_speed";
        let ring = AccessoryItem::new("mega:ring", ["ring"]).with_modifiers(vec![
            mod_adapter_core::ecosystem_compat::AttributeModifier {
                attribute: attr.into(),
                operation: mod_adapter_core::ecosystem_compat::ModifierOperation::AddValue,
                amount: 0.05,
            },
        ]);
        live_mod_equip_accessory("mega_w3", "ring", 0, ring).unwrap();
        assert!(matches!(
            live_mod_equip_accessory(
                "mega_w3",
                "ring",
                0,
                AccessoryItem::new("mega:ring2", ["ring"])
            ),
            Err(AccessoryError::SlotOccupied { .. })
        ));
        assert_eq!(
            live_mod_accessory_attributes("mega_w3")[attr].apply(1.0),
            1.05
        );
        assert!(live_mod_accessory_attributes("mega_w3_stranger").is_empty());

        // ---- MAG1: mana shield absorbs, cast lands, sync frame encodes ----
        let uuid = [0x77; 16];
        live_mod_magic_set_attributes(uuid, lmb_magic::MagicAttributes::default()).unwrap();
        live_mod_magic_register_status(lmb_magic::StatusEffectDefinition {
            id: "mega:ward".into(),
            absorption_per_level: 4.0,
            ..lmb_magic::StatusEffectDefinition::default()
        })
        .unwrap();
        live_mod_magic_apply_status(uuid, "mega:ward", 0, 40).unwrap();
        assert_eq!(live_mod_magic_absorb_damage(uuid, 3.0), 0.0);
        assert_eq!(live_mod_magic_absorb_damage(uuid, 5.0), 4.0);
        let instant = lmb_magic::SpellDefinition {
            id: "mega:zap".into(),
            mana_cost: 10.0,
            base_cooldown_ticks: 4,
            cast: lmb_magic::SpellCastState::Instant,
            interrupt_on_damage: false,
            interrupt_on_move: false,
            steps: vec![lmb_magic::SpellNode {
                form: lmb_magic::SpellForm::SelfCast,
                effects: vec![lmb_magic::SpellEffect::Heal { base_amount: 5.0 }],
            }],
        };
        assert!(live_mod_magic_cast(uuid, &instant).is_ok());
        assert!(matches!(
            live_mod_magic_cast(uuid, &instant),
            Err(LiveMagicError::Cast(lmb_magic::CastError::OnCooldown { .. }))
        ));
        let tick = live_mod_magic_tick_player(uuid).unwrap();
        let frame = tick.frame.expect("MAG1 sync frame");
        assert!(frame.windows(4).any(|w| w == b"MAG1"));

        // ---- dimension portal: transition -> intent -> anti-cheat grace ----
        {
            let mut b = dimension_mod_bridge().lock().unwrap_or_else(|e| e.into_inner());
            b.register_portal_rule(dim_mod::PortalRule::new(
                "megaw3:nether_portal",
                dim_mod::PortalTrigger::VerticalFrame {
                    frame_block: "minecraft:obsidian".into(),
                    min_width: 2,
                    min_height: 3,
                    max_width: 4,
                    max_height: 5,
                    igniter_item: "minecraft:flint_and_steel".into(),
                    portal_block: "minecraft:nether_portal".into(),
                },
                "minecraft:the_nether",
            ));
        }
        let transition = dim_mod::PortalTransition {
            rule_name: "megaw3:nether_portal".into(),
            target_dim: "minecraft:the_nether".into(),
            anchor: [0, 60, 0],
            cell_count: 6,
        };
        let (traveller, _tp) = lmb_client("mega_w3_travel", 977_301);
        let (control, _cp) = lmb_client("mega_w3_control", 977_302);
        for c in [&traveller, &control] {
            ac_test_set_player_preset(&c.name, AcPreset::Standard);
            let v = ac_check_move(c, (0.5, 80.0, 0.5), (0.6, 80.0, 0.5), false);
            assert!(matches!(v, AcMoveVerdict::Accept { .. }));
        }
        let source_pos = [4000.5, 70.0, -2000.5];
        let intent = {
            let b = dimension_mod_bridge().lock().unwrap_or_else(|e| e.into_inner());
            b.try_mod_portal_teleport(
                traveller.eid,
                DimId::Overworld,
                DimId::Nether,
                source_pos,
                &transition,
            )
            .expect("nether transition must produce a teleport intent")
        };
        assert_eq!(intent.target_dim, "minecraft:the_nether");
        assert_eq!(intent.expect_teleport_grace_ticks, 20);
        // Nether coordinate scale 8.0 maps x/z by source_scale / target_scale.
        assert!((intent.target_pos[0] - 500.0625).abs() < 1e-9);
        assert!((intent.target_pos[1] - 70.0).abs() < 1e-9);
        assert!((intent.target_pos[2] - (-250.0625)).abs() < 1e-9);
        let to = (intent.target_pos[0], intent.target_pos[1], intent.target_pos[2]);
        let v = ac_check_move(&traveller, (4000.5, 70.0, -2000.5), to, false);
        assert!(
            matches!(v, AcMoveVerdict::Accept { .. }),
            "portal grace must absorb the dimensional jump"
        );
        let v = ac_check_move(&control, (0.6, 80.0, 0.5), (4000.6, 80.0, -2000.5), false);
        assert!(
            matches!(v, AcMoveVerdict::Setback),
            "a non-portal jump of the same distance must still flag"
        );

        // Leave every global clean for the next test.
        *dimension_mod_bridge().lock().unwrap_or_else(|e| e.into_inner()) =
            DimensionModRuntimeBridge::new();
        *live_magic_state().lock().unwrap_or_else(|e| e.into_inner()) =
            LiveMagicState::default();
        fresh_suite();
        let _ = std::fs::remove_dir_all(dir);
    }

    // ---------- MRS1 dynamic registry sync ----------

    /// Vanilla / unarmed: `live_mod_registry_sync_frame` must stay `None`
    /// and the join hook must put zero bytes on the wire.
    #[test]
    fn wire_mods_mrs1_vanilla_emits_nothing() {
        let _serial = WIRE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        fresh_suite();
        LIVE_CREATE_TEST_GATE.store(2, Ordering::Relaxed);
        assert!(
            live_mod_registry_sync_frame().is_none(),
            "unarmed server must not emit an MRS1 frame"
        );
        let (mut send, mut peer) = lmb_socket_pair();
        live_mod_registry_sync_on_join(&mut send);
        assert!(lmb_silent(&mut peer), "vanilla join must emit no MRS1 packet");
        LIVE_CREATE_TEST_GATE.store(1, Ordering::Relaxed);
    }

    /// Create armed: the frame decodes under the client contract — `MRS1`
    /// magic, IEEE CRC32 and monotonic ids are all enforced by
    /// `decode_registry_sync_frame` — and carries the `create:*` item table.
    /// When `breg()` is loaded with the mod extension, block states join too.
    #[test]
    fn wire_mods_mrs1_create_armed_frame_decodes() {
        let _serial = WIRE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        fresh_suite();
        LIVE_CREATE_TEST_GATE.store(1, Ordering::Relaxed);
        let frame = live_mod_registry_sync_frame().expect("armed Create must emit MRS1");
        assert!(frame.starts_with(b"MRS1"));
        let synced = mod_adapter_core::live_registry_bridge::decode_registry_sync_frame(&frame)
            .expect("server MRS1 frame must satisfy the client decoder");
        for name in ["create:shaft", "create:wrench", "create:brass_ingot"] {
            assert!(
                synced.items.values().any(|n| n == name),
                "missing create item {name}"
            );
        }
        if breg()
            .and_then(|reg| reg.create_registry())
            .is_some()
        {
            assert!(
                synced
                    .block_states
                    .iter()
                    .any(|e| e.block_id == "create:shaft"),
                "armed Create registry must sync its block states"
            );
            assert!(synced.block_states.iter().all(|e| {
                e.state_id >= create_adapter::blockstates::DEFAULT_BASE_STATE_ID
            }));
        }
        // The join path emits exactly one 0x19 custom_payload on the MRS1
        // channel carrying the identical frame bytes.
        let (mut send, mut peer) = lmb_socket_pair();
        live_mod_registry_sync_on_join(&mut send);
        let mut stash = Vec::new();
        let (id, body) = lmb_read_frame(&mut peer, &mut stash).expect("mrs1 frame");
        assert_eq!(id, LIVE_MOD_S2C_CUSTOM_PAYLOAD_ID);
        let (channel, payload) = lmb_decode_payload(&body);
        assert_eq!(channel, MOD_REGISTRY_SYNC_CHANNEL);
        assert_eq!(payload, frame);
    }

    /// `MEGU_MODS_DIR` content: a scanned JAR's declared blockstates/items
    /// land in the suite's registry plan and ride the same MRS1 frame at the
    /// dedicated generic id bases — disjoint from Create's ranges.
    #[test]
    fn wire_mods_mrs1_includes_mods_dir_registry_plan() {
        let _serial = WIRE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        fresh_suite();
        LIVE_CREATE_TEST_GATE.store(2, Ordering::Relaxed);
        let dir = lmb_temp_dir("mrs1");
        let blockstates = br#"{"variants":{"axis=x,waterlogged=false":{"model":"testmod:block/rock"},"axis=y,waterlogged=false":{"model":"testmod:block/rock"}}}"#;
        let item_model = br#"{"parent":"minecraft:item/generated"}"#;
        let manifest = lmb_fabric_manifest("testmod");
        let jar = lmb_test_jar(&[
            ("fabric.mod.json", manifest.as_slice()),
            ("assets/testmod/blockstates/rock.json", blockstates),
            ("assets/testmod/models/item/rock.json", item_model),
            ("assets/testmod/items/geode.json", item_model),
        ]);
        std::fs::write(dir.join("testmod.jar"), &jar).unwrap();
        let mut suite = LiveModSuite::new();
        suite.scan_mods_dir(&dir);
        assert!(
            suite.registry_plan.is_some(),
            "adapted JAR must produce an MRS1 registry plan"
        );
        *live_mod_suite().lock().unwrap_or_else(|e| e.into_inner()) = suite;

        let frame = live_mod_registry_sync_frame().expect("generic mods must emit MRS1");
        let synced = mod_adapter_core::live_registry_bridge::decode_registry_sync_frame(&frame)
            .expect("merged frame must satisfy the client decoder");
        let rock = synced
            .block_states
            .iter()
            .find(|e| e.block_id == "testmod:rock")
            .expect("testmod:rock block state missing");
        assert!(rock.state_id >= LIVE_MRS_GENERIC_BLOCK_BASE);
        assert!(rock.properties.iter().any(|(k, v)| k == "axis" && v == "x"));
        assert!(
            synced.items.values().any(|n| n == "testmod:geode"),
            "testmod:geode item missing"
        );
        assert!(
            synced.items.keys().all(|id| *id >= LIVE_MRS_GENERIC_ITEM_BASE),
            "generic item ids must stay in their dedicated range"
        );

        let _ = std::fs::remove_dir_all(&dir);
        fresh_suite();
        LIVE_CREATE_TEST_GATE.store(1, Ordering::Relaxed);
    }

}

// ===========================================================================
// Live Create runtime tests (`mod-suite`).
// ===========================================================================

#[cfg(all(test, feature = "mod-suite"))]
mod live_create_tests {
    use super::*;

    /// Force-arm the runtime for the test and reset it to a clean slate.
    /// `world_guard` serializes every test that writes world cells.
    fn arm() {
        LIVE_CREATE_TEST_GATE.store(1, Ordering::Relaxed);
        *live_create_state().lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    fn registry() -> create_adapter::blockstates::CreateBlockRegistry {
        create_adapter::blockstates::core_create_registry(
            create_adapter::blockstates::DEFAULT_BASE_STATE_ID,
        )
    }

    fn state_of(id: &str, props: &[(&str, &str)]) -> u32 {
        let reg = registry();
        let mut st = reg.default_state(id).expect(id);
        for (k, v) in props {
            st = reg.with_property(st, k, v).expect(k);
        }
        st
    }

    /// Write `st` directly into the world overlay (no hooks), then fire the
    /// real write hook so the test exercises the same path as `set_block`.
    fn put(dim: DimId, x: i32, y: i32, z: i32, st: u32) {
        let old = world()
            .read(&(dim, x >> 4, z >> 4))
            .get(&(dim, x >> 4, z >> 4))
            .and_then(|m| m.get(&local_idx(x, y, z)).copied())
            .unwrap_or(0);
        world()
            .write(&(dim, x >> 4, z >> 4))
            .entry((dim, x >> 4, z >> 4))
            .or_default()
            .insert(local_idx(x, y, z), st);
        live_create_on_block_set(dim, x, y, z, old, st);
    }

    fn clear(x: i32, y: i32, z: i32) {
        put(DimId::Overworld, x, y, z, 0);
    }

    /// A water state id inside the pinned 1.21.1 `water`/`flowing_water`
    /// window the fluid scheduler uses — resolved without `breg()`.
    const TEST_WATER: u32 = 90;

    // Unique far-away coordinates per test so parallel world writes never
    // meet a foreign cell. W1 sits mid-region: negative offsets stay inside
    // the same kinetic region, keeping every link intra-region.
    const W1: (i32, i32, i32) = (9_000_008, 80, 0);
    const W2: (i32, i32, i32) = (9_000_000, 80, 100);
    const W3: (i32, i32, i32) = (9_000_000, 80, 200);
    const W4: (i32, i32, i32) = (9_000_000, 80, 300);
    const W5: (i32, i32, i32) = (9_000_000, 80, 400);
    const W6: (i32, i32, i32) = (9_000_008, 80, 500);
    const W7: (i32, i32, i32) = (9_000_000, 80, 600);

    fn pos_of(c: (i32, i32, i32)) -> FactoryPos {
        FactoryPos::new(DimId::Overworld.raw() as i32, c.0, c.1, c.2)
    }

    #[test]
    fn create_item_resolution_and_names() {
        let _g = world_guard();
        arm();
        let wrench = live_create_resolve_item("create:wrench").expect("wrench item");
        assert_eq!(live_create_item_name(wrench), Some("create:wrench"));
        assert_eq!(live_create_item_block_name(wrench), None);
        let shaft = live_create_resolve_item("create:shaft").expect("shaft item");
        assert_eq!(live_create_item_name(shaft), Some("create:shaft"));
        assert_eq!(live_create_item_block_name(shaft), Some("create:shaft"));
        // The belt alias resolves to the mechanical_belt block.
        let belt = live_create_resolve_item("create:belt").expect("belt item");
        assert_eq!(live_create_item_block_name(belt), Some("create:mechanical_belt"));
        // Processed material items resolve through the same tables but
        // never place a block.
        for item in [
            "create:iron_sheet",
            "create:gold_sheet",
            "create:copper_sheet",
            "create:brass_sheet",
            "create:andesite_alloy",
            "create:brass_ingot",
            "create:zinc_ingot",
        ] {
            let id = live_create_resolve_item(item).unwrap_or_else(|| {
                panic!("missing item for {item}")
            });
            assert_eq!(live_create_item_name(id), Some(item));
            assert_eq!(live_create_item_block_name(id), None);
            assert_eq!(name_to_item(item), Some(id));
            assert_eq!(item_to_name(id), Some(item));
        }
        for block in [
            "create:water_wheel",
            "create:cogwheel",
            "create:large_cogwheel",
            "create:gearbox",
            "create:clutch",
            "create:gearshift",
            "create:mechanical_press",
            "create:depot",
            "create:basin",
        ] {
            assert!(
                live_create_resolve_item(block).is_some(),
                "missing item for {block}"
            );
        }
        // /give entry points resolve through the real tables.
        assert_eq!(name_to_item("create:wrench"), Some(wrench));
        assert_eq!(name_to_item("create:shaft"), Some(shaft));
        assert_eq!(item_to_name(wrench), Some("create:wrench"));
        assert_eq!(item_to_name(shaft), Some("create:shaft"));
        // Vanilla items are untouched.
        assert_eq!(name_to_item("minecraft:stone"), Some(1));
        assert_eq!(live_create_resolve_item("minecraft:stone"), None);
        assert_eq!(live_create_resolve_item("diamond"), None);
    }

    #[test]
    fn create_kinetic_lifecycle_rpm_wire_wrench_break() {
        let _g = world_guard();
        arm();
        let (wx, wy, wz) = W1;
        let wheel_pos = pos_of(W1);
        // A real Create train: water -> water_wheel(x) -> shaft(x) ->
        // gearbox(x) -> shaft(y) + mechanical_press(facing=east, axis X).
        // The gearbox turns one shaft vertical; the press links axially in
        // the direction its facing resolves to (east -> X axis).
        let water = (wx + 1, wy, wz);
        let shaft = (wx - 1, wy, wz);
        let gearbox = (wx - 2, wy, wz);
        let shaft_y = (wx - 2, wy - 1, wz);
        let press = (wx - 3, wy, wz);
        for c in [water, shaft, gearbox, shaft_y, press] {
            clear(c.0, c.1, c.2);
        }
        clear(wx, wy, wz);

        // Water, then the wheel: adjacency is checked on the wheel write.
        put(DimId::Overworld, water.0, water.1, water.2, TEST_WATER);
        let wheel = state_of("create:water_wheel", &[("axis", "x")]);
        put(DimId::Overworld, wx, wy, wz, wheel);
        let shaft_st = state_of("create:shaft", &[("axis", "x")]);
        put(DimId::Overworld, shaft.0, shaft.1, shaft.2, shaft_st);
        let gearbox_st = state_of("create:gearbox", &[("axis", "x")]);
        put(DimId::Overworld, gearbox.0, gearbox.1, gearbox.2, gearbox_st);
        let shaft_y_st = state_of("create:shaft", &[("axis", "y")]);
        put(DimId::Overworld, shaft_y.0, shaft_y.1, shaft_y.2, shaft_y_st);
        let press_st = state_of("create:mechanical_press", &[("facing", "east")]);
        put(DimId::Overworld, press.0, press.1, press.2, press_st);

        // Generator -> shaft -> gearbox -> press: non-zero RPM propagates.
        live_create_with(|rt| {
            assert!(rt.placed.contains_key(&wheel_pos));
            assert!(!rt.dormant.contains(&wheel_pos));
            for c in [shaft, gearbox, shaft_y] {
                let sol = rt
                    .factory
                    .kinetic_solution(pos_of(c))
                    .expect("node in graph");
                assert!(
                    sol.speed.abs() > 1e-6,
                    "{c:?} must carry non-zero RPM, got {}",
                    sol.speed
                );
            }
            let press_sol = rt
                .factory
                .kinetic_solution(pos_of(press))
                .expect("press in graph");
            assert!(
                matches!(
                    press_sol.status,
                    mod_offload::kinetic_graph::NetworkStatus::Running
                ),
                "press must be powered, got {:?}",
                press_sol.status
            );
        });

        // Tick drains the accumulated UpdateTags into one S2C batch whose
        // exact bytes are the play-phase 0x19 custom payload body.
        let body = live_create_tick_body().expect("kinetic batch emitted");
        // body = write_str(channel) + payload
        let mut rest: &[u8] = &body;
        let chan_len = read_varint(&mut rest).expect("channel varint") as usize;
        let channel = std::str::from_utf8(&rest[..chan_len]).unwrap();
        assert_eq!(
            channel,
            create_adapter::neoforge_wire::CHANNEL_CREATE_KINETIC_SYNC
        );
        let tags =
            create_adapter::neoforge_wire::decode_kinetic_update_batch(&rest[chan_len..])
                .expect("batch decodes");
        assert!(tags
            .iter()
            .any(|t| t.pos == pos_of(shaft) && t.speed_rpm.abs() > 1e-6));

        // The snapshot the join packet carries names the same nodes.
        live_create_with(|rt| {
            let snap = rt.snapshot_tags();
            assert!(snap.iter().any(|t| t.pos == pos_of(shaft)));
        });

        // Wrench on the shaft cycles axis x -> y and re-solves the graph.
        let rotated = live_create_wrench_rotate(pos_of(shaft), shaft_st)
            .expect("wrench rotates shaft");
        assert_ne!(rotated, shaft_st);
        assert_eq!(
            registry().encode_canonical(rotated).unwrap(),
            "create:shaft[axis=y,waterlogged=false]"
        );
        live_create_with(|rt| {
            assert_eq!(rt.placed.get(&pos_of(shaft)), Some(&rotated));
            let sol = rt
                .factory
                .kinetic_solution(pos_of(shaft))
                .expect("shaft still tracked");
            // Axis Y disconnects it from the x-axis wheel: network re-solved.
            assert!(
                sol.speed.abs() <= 1e-6,
                "disconnected shaft must stop, got {}",
                sol.speed
            );
        });

        // Breaking the shaft removes the graph node and the tracking entry.
        put(DimId::Overworld, shaft.0, shaft.1, shaft.2, 0);
        live_create_with(|rt| {
            assert!(rt.factory.kinetic_solution(pos_of(shaft)).is_none());
            assert!(!rt.placed.contains_key(&pos_of(shaft)));
        });

        for c in [water, shaft, gearbox, shaft_y, press] {
            clear(c.0, c.1, c.2);
        }
        clear(wx, wy, wz);
    }

    #[test]
    fn create_water_wheel_dormant_until_flow_arrives() {
        let _g = world_guard();
        arm();
        let (wx, wy, wz) = W2;
        clear(wx, wy, wz);
        clear(wx + 1, wy, wz);
        let wheel = state_of("create:water_wheel", &[("axis", "x")]);
        put(DimId::Overworld, wx, wy, wz, wheel);
        // No adjacent water: tracked as placed but dormant — no graph node.
        live_create_with(|rt| {
            assert!(rt.dormant.contains(&pos_of(W2)));
            assert!(rt.factory.kinetic_solution(pos_of(W2)).is_none());
        });
        // Water arriving beside it flips the wheel live through the same
        // water-edge hook a real fluid write fires.
        put(DimId::Overworld, wx + 1, wy, wz, TEST_WATER);
        live_create_with(|rt| {
            assert!(!rt.dormant.contains(&pos_of(W2)));
            let sol = rt
                .factory
                .kinetic_solution(pos_of(W2))
                .expect("wheel joined graph");
            assert!(sol.speed.abs() > 1e-6, "wheel must spin, got {}", sol.speed);
        });
        // Removing the water idles the wheel again.
        put(DimId::Overworld, wx + 1, wy, wz, 0);
        live_create_with(|rt| {
            assert!(rt.dormant.contains(&pos_of(W2)));
            assert!(rt.factory.kinetic_solution(pos_of(W2)).is_none());
        });
        clear(wx, wy, wz);
    }

    #[test]
    fn create_sidecar_roundtrip_and_fail_closed() {
        let _g = world_guard();
        arm();
        let (sx, sy, sz) = W3;
        clear(sx, sy, sz);
        clear(sx + 1, sy, sz);
        put(DimId::Overworld, sx + 1, sy, sz, TEST_WATER);
        let wheel = state_of("create:water_wheel", &[("axis", "x")]);
        put(DimId::Overworld, sx, sy, sz, wheel);

        let bytes = live_create_serialize().expect("armed runtime serializes");
        let entries = live_create_decode_sidecar(&bytes).expect("sidecar decodes");
        assert_eq!(entries.len(), 1);
        assert_eq!((entries[0].0, entries[0].1), (pos_of(W3), wheel));
        assert!(entries[0].2.is_empty());

        // Rebuild into a fresh runtime and confirm the graph re-solves.
        live_create_with(|rt| rt.load_entries(entries.clone()));
        live_create_with(|rt| {
            let sol = rt
                .factory
                .kinetic_solution(pos_of(W3))
                .expect("wheel restored");
            assert!(sol.speed.abs() > 1e-6);
            assert_eq!(rt.placed.get(&pos_of(W3)), Some(&wheel));
        });

        // Fail-closed: corrupt magic, checksum, truncation, trailing bytes.
        let mut bad = bytes.clone();
        bad[0] = b'X';
        assert!(live_create_decode_sidecar(&bad).is_err());
        let mut bad = bytes.clone();
        let n = bad.len();
        bad[n - 1] ^= 0xff;
        assert!(live_create_decode_sidecar(&bad).is_err());
        assert!(live_create_decode_sidecar(&bytes[..bytes.len() - 2]).is_err());
        let mut bad = bytes.clone();
        bad.push(0);
        assert!(live_create_decode_sidecar(&bad).is_err());
        // A state id not in the registry must not decode.
        let mut bogus = Vec::new();
        bogus.extend_from_slice(LIVE_CREATE_SIDECAR_MAGIC);
        write_varint(&mut bogus, 1);
        bogus.extend_from_slice(&0i32.to_be_bytes());
        bogus.extend_from_slice(&0i32.to_be_bytes());
        bogus.extend_from_slice(&0i32.to_be_bytes());
        bogus.extend_from_slice(&0i32.to_be_bytes());
        write_str(&mut bogus, "create:not_a_block[axis=x]");
        let sum = live_create_fnv32(&bogus);
        bogus.extend_from_slice(&sum.to_be_bytes());
        assert!(live_create_decode_sidecar(&bogus).is_err());

        clear(sx, sy, sz);
        clear(sx + 1, sy, sz);
    }

    /// Drive `live_create_tick_body` for up to `max` game ticks, returning
    /// early once `done` observes its condition inside the runtime.
    fn step_until(max: u64, done: impl Fn(&mut LiveCreateRuntime) -> bool) -> bool {
        let base = TICK_IDX.load(Ordering::Relaxed);
        for i in 1..=max {
            TICK_IDX.store(base + i, Ordering::Relaxed);
            let _ = live_create_tick_body();
            if live_create_with(&done).unwrap_or(false) {
                return true;
            }
        }
        false
    }

    #[test]
    fn create_depot_right_click_press_and_retrieve() {
        let _g = world_guard();
        arm();
        let (wx, wy, wz) = W6;
        // water -> water_wheel(x) -> shaft(x) -> press(facing=east -> axis
        // X); the depot sits two blocks below the press head.
        let water = (wx + 1, wy, wz);
        let shaft = (wx - 1, wy, wz);
        let press = (wx - 2, wy, wz);
        let depot = (wx - 2, wy - 2, wz);
        for c in [water, shaft, press, depot] {
            clear(c.0, c.1, c.2);
        }
        clear(wx, wy, wz);
        put(DimId::Overworld, water.0, water.1, water.2, TEST_WATER);
        put(DimId::Overworld, wx, wy, wz, state_of("create:water_wheel", &[("axis", "x")]));
        put(DimId::Overworld, shaft.0, shaft.1, shaft.2, state_of("create:shaft", &[("axis", "x")]));
        put(
            DimId::Overworld,
            press.0,
            press.1,
            press.2,
            state_of("create:mechanical_press", &[("facing", "east")]),
        );
        let depot_st = state_of("create:depot", &[]);
        put(DimId::Overworld, depot.0, depot.1, depot.2, depot_st);
        let depot_pos = pos_of(depot);
        live_create_with(|rt| {
            let sol = rt
                .factory
                .kinetic_solution(pos_of(press))
                .expect("press in graph");
            assert!(
                matches!(
                    sol.status,
                    mod_offload::kinetic_graph::NetworkStatus::Running
                ),
                "press must be powered, got {:?}",
                sol.status
            );
        });

        let (me, _peer) = wire_mods_tests::lmb_client("create_p2b_depot", 977_410);
        ac_test_set_player_preset(&me.name, AcPreset::Standard);
        let iron = name_to_item("minecraft:iron_ingot").expect("iron item");
        me.inv
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(36, (iron, 4, Vec::new()));
        *me.held.lock().unwrap_or_else(|e| e.into_inner()) = 0;

        // Right-click inserts one ingot onto the depot and consumes it.
        assert!(live_create_interact_block(
            &me,
            DimId::Overworld,
            depot.0,
            depot.1,
            depot.2,
            depot_st,
            0
        ));
        assert_eq!(
            me.inv
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&36)
                .map(|s| s.1),
            Some(3),
            "insertion consumes one held item"
        );
        live_create_with(|rt| {
            assert_eq!(
                rt.held.get(&depot_pos).map(|h| h.items.as_slice()),
                Some(&[iron][..])
            );
        });
        // The depot's single slot is occupied: a second insert is refused.
        assert!(!live_create_interact_block(
            &me,
            DimId::Overworld,
            depot.0,
            depot.1,
            depot.2,
            depot_st,
            0
        ));

        // A powered press two blocks above converts the ingot at a rate
        // proportional to RPM; the sheet stays on the depot.
        let sheet = name_to_item("create:iron_sheet").expect("sheet item");
        assert!(
            step_until(600, |rt| rt
                .held
                .get(&depot_pos)
                .is_some_and(|h| h.items.as_slice() == [sheet])),
            "press never produced create:iron_sheet"
        );

        // Persistence: the processed item rides the sidecar entry.
        let bytes = live_create_serialize().expect("serialize");
        let entries = live_create_decode_sidecar(&bytes).expect("decode");
        let entry = entries.iter().find(|(p, _, _)| *p == depot_pos).unwrap();
        assert_eq!(entry.2, vec![sheet]);
        live_create_with(|rt| rt.load_entries(entries));
        live_create_with(|rt| {
            assert_eq!(
                rt.held.get(&depot_pos).map(|h| h.items.as_slice()),
                Some(&[sheet][..]),
                "held item must survive a save/load roundtrip"
            );
        });

        // Empty-handed right-click returns the sheet to the inventory.
        me.inv
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&36);
        assert!(live_create_interact_block(
            &me,
            DimId::Overworld,
            depot.0,
            depot.1,
            depot.2,
            depot_st,
            0
        ));
        assert_eq!(
            me.inv
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&36)
                .map(|s| (s.0, s.1)),
            Some((sheet, 1))
        );
        live_create_with(|rt| assert!(rt.held.get(&depot_pos).is_none_or(|h| h.items.is_empty())));
        // Nothing left to take: empty hand on an empty depot is unhandled.
        me.inv
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&36);
        assert!(!live_create_interact_block(
            &me,
            DimId::Overworld,
            depot.0,
            depot.1,
            depot.2,
            depot_st,
            0
        ));

        for c in [water, shaft, press, depot] {
            clear(c.0, c.1, c.2);
        }
        clear(wx, wy, wz);
    }

    #[test]
    fn create_belt_insert_transport_and_retrieve() {
        let _g = world_guard();
        arm();
        let (bx, by, bz) = W7;
        clear(bx, by, bz);
        let belt_st = state_of("create:mechanical_belt", &[("facing", "north")]);
        put(DimId::Overworld, bx, by, bz, belt_st);
        let belt_pos = pos_of(W7);

        let (me, _peer) = wire_mods_tests::lmb_client("create_p2b_belt", 977_411);
        ac_test_set_player_preset(&me.name, AcPreset::Standard);
        let iron = name_to_item("minecraft:iron_ingot").expect("iron item");
        me.inv
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(36, (iron, 2, Vec::new()));

        // Insert one ingot onto the belt's transport machine.
        assert!(live_create_interact_block(&me, DimId::Overworld, bx, by, bz, belt_st, 0));
        assert_eq!(
            me.inv
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&36)
                .map(|s| s.1),
            Some(1)
        );
        live_create_with(|rt| assert!(rt.belt_machines.contains_key(&belt_pos)));

        // After the traversal delay the item reaches the machine output.
        let machine = live_create_with(|rt| rt.belt_machines[&belt_pos]).unwrap();
        assert!(step_until(200, |rt| rt
            .factory
            .scheduler()
            .region(belt_pos.region_key())
            .is_some_and(|r| r.transport().machine(machine).is_some_and(
                |m| m.output_items == 1
            ))));

        // Empty-handed click collects it back into the inventory.
        me.inv
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&36);
        assert!(live_create_interact_block(&me, DimId::Overworld, bx, by, bz, belt_st, 0));
        assert_eq!(
            me.inv
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&36)
                .map(|s| (s.0, s.1)),
            Some((iron, 1))
        );

        clear(bx, by, bz);
    }

    #[test]
    fn create_join_snapshot_and_tick_frame_shape() {
        let _g = world_guard();
        arm();
        let (wx, wy, wz) = W4;
        clear(wx, wy, wz);
        clear(wx + 1, wy, wz);
        clear(wx + 2, wy, wz);
        put(DimId::Overworld, wx + 1, wy, wz, TEST_WATER);
        let wheel = state_of("create:water_wheel", &[("axis", "x")]);
        put(DimId::Overworld, wx, wy, wz, wheel);
        let shaft = state_of("create:shaft", &[("axis", "x")]);
        put(DimId::Overworld, wx + 2, wy, wz, shaft);

        // The join packet: a real socket pair carries the exact S2C frame —
        // packet id 0x19, channel create:kinetic_sync_v1, batch payload.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let mut send = std::net::TcpStream::connect(addr).unwrap();
        live_create_sync_on_join(&mut send);
        drop(send);
        let (mut recv, _) = listener.accept().unwrap();
        use std::io::Read;
        let mut buf = Vec::new();
        recv.read_to_end(&mut buf).unwrap();
        assert!(!buf.is_empty(), "join snapshot emitted");
        let mut frame: &[u8] = &buf;
        // Outer packet length varint, then packet id 0x19.
        let plen = read_varint(&mut frame).expect("frame len") as usize;
        assert_eq!(frame.len(), plen);
        let id = read_varint(&mut frame).expect("packet id");
        assert_eq!(id, LIVE_MOD_S2C_CUSTOM_PAYLOAD_ID);
        let chan_len = read_varint(&mut frame).expect("channel len") as usize;
        let channel = std::str::from_utf8(&frame[..chan_len]).unwrap();
        assert_eq!(
            channel,
            create_adapter::neoforge_wire::CHANNEL_CREATE_KINETIC_SYNC
        );
        let tags =
            create_adapter::neoforge_wire::decode_kinetic_update_batch(&frame[chan_len..])
                .expect("snapshot decodes");
        assert!(tags.iter().any(|t| t.pos == pos_of(W4)));

        for c in [(wx, wy, wz), (wx + 1, wy, wz), (wx + 2, wy, wz)] {
            clear(c.0, c.1, c.2);
        }
    }

    #[test]
    fn create_disabled_gate_is_inert() {
        let _g = world_guard();
        LIVE_CREATE_TEST_GATE.store(2, Ordering::Relaxed);
        assert!(!live_create_enabled());
        assert!(live_create_resolve_item("create:shaft").is_none());
        assert_eq!(live_create_item_name(LIVE_CREATE_ITEM_BASE), None);
        assert!(live_create_serialize().is_none());
        // on_block_set with a create state id is a no-op while disabled.
        live_create_on_block_set(DimId::Overworld, W5.0, W5.1, W5.2, 0, 26_684);
        live_create_with(|_rt| panic!("runtime must not arm while disabled"));
        // Restoring the gate for later tests.
        LIVE_CREATE_TEST_GATE.store(1, Ordering::Relaxed);
    }

    /// `breg()`-dependent coverage: only meaningful when the vanilla assets
    /// are checked out (the `megu_vanilla_data` cfg). With the test gate on,
    /// the extension ran at `BReg::load` whenever it initialized — assert
    /// the Create block table answers through the vanilla accessors.
    #[test]
    #[cfg(megu_vanilla_data)]
    fn create_breg_extension_resolves_states() {
        let _g = world_guard();
        arm();
        let Some(reg) = breg() else {
            return;
        };
        let Some(_create) = reg.create_registry() else {
            // BReg loaded before the test gate armed the env flag; the
            // extension is env-gated at load time — nothing to assert here.
            return;
        };
        let shaft = reg.state_of("create:shaft", None);
        assert!(shaft >= create_adapter::blockstates::DEFAULT_BASE_STATE_ID);
        assert_eq!(reg.name_of(shaft), "create:shaft");
        let rotated = reg.with_prop(shaft, "axis", "y");
        assert_eq!(
            reg.get_prop(rotated, "axis").as_deref(),
            Some("y")
        );
        assert_eq!(
            reg.canonical_state(rotated),
            "create:shaft[axis=y,waterlogged=false]"
        );
        assert!(reg.is_solid_render(shaft));
        assert!(!reg.is_air(shaft));
        assert!(!reg.is_replaceable(shaft));
        // Vanilla states are untouched by the extension.
        assert_eq!(reg.name_of(0), "minecraft:air");
    }
}
